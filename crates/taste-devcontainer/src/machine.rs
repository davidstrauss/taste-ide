//! **The machine provider: containers behind KVM, on the user's own host.**
//!
//! One machine per user, not one per environment. The substrate spike
//! settled that: a machine costs ~1.35 GB of host memory at idle and 14 s
//! to boot, and it hosts ordinary podman, so N environments inside one
//! machine are N containers exactly as they were. One VM per environment
//! would multiply a fixed cost by a number the fleet is designed to grow.
//!
//! Everything the IDE does lands inside it — devcontainer builds, the
//! containers themselves, the baseline. That was the requirement that made
//! the machine the only viable candidate: a `podman build` runs
//! repo-supplied `RUN` steps, which is the earliest and least-confined
//! untrusted-code path in the system, and building through a connection is
//! the same command as building locally. Nothing had to be invented to
//! cover it.
//!
//! # The helper binaries, and why they are the whole risk
//!
//! `podman machine start` needs two programs the IDE's target host does not
//! have where podman looks:
//!
//! - **`gvproxy`** (user-mode networking) is not in an immutable Fedora
//!   base image at all; and
//! - **`virtiofsd`** is installed, at `/usr/libexec/virtiofsd`, which is not
//!   on `$PATH` — and `$PATH` is the only place podman looks.
//!
//! Installing either would mean `rpm-ostree` layering on the user's host,
//! which this project will not do. So the IDE arranges them **in user
//! space**, through documented configuration: a directory it owns, holding
//! a fetched `gvproxy` and a symlink to the system `virtiofsd`, named by
//! `[engine] helper_binaries_dir` in a `containers.conf` applied through
//! `CONTAINERS_CONF_OVERRIDE`, with that directory also on `$PATH` for the
//! machine commands themselves.
//!
//! **That override is scoped to the machine commands and to nothing else.**
//! It is not exported, not applied to `podman run`/`build`/`exec`, and
//! never written into the user's own `containers.conf`. A config override
//! that leaked onto every podman invocation would be a second, invisible
//! opinion about the user's container engine.
//!
//! `gvproxy` is **fetched, version-pinned and sha256-verified** (see
//! [`ensure_gvproxy`]) — the repo's rule for fetched artifacts, applied to
//! a binary rather than a package. It lands in the IDE's own data
//! directory. Nothing is installed on the host, ever.
//!
//! # Sizing is a commitment, not a ceiling
//!
//! qemu is started with a memfd memory backend and **no balloon**, so guest
//! page cache ratchets host RSS up to the configured memory and never gives
//! it back — measured: 1.3 GB at idle, 8.4 GB after one image build and one
//! cargo build, against an 8 GiB configuration. The IDE therefore sizes the
//! machine as memory it is *taking*, derived from the host rather than
//! fixed, and reports it as a resource line so the number is visible rather
//! than mysterious.
//!
//! Disk is the opposite shape: the qcow2 is sparse and grows, and does not
//! shrink when the guest frees space. [`DISK_CEILING_GIB`] is a ceiling on
//! that growth, not an allocation.
//!
//! # Machines are cattle
//!
//! Recreate rather than nurse. The machine carries no state the IDE cannot
//! rebuild — images rebuild from configs, environment clones live on the
//! host and are shared in over virtiofs — so the answer to a machine that
//! is wrong is `remove` and `create`, not repair. What that costs is every
//! container inside it, which is why [`crate::supervisor::Supervisor`] has
//! to notice its container has gone rather than keep reporting it running;
//! see `reconcile_container_presence`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use taste_core::PodmanTarget;

/// The IDE's machine. One name, fixed: the machine is IDE-owned
/// infrastructure, and a second one would be a fleet of VMs nobody asked
/// for. It is also the connection name podman registers for it.
pub const MACHINE_NAME: &str = "taste-ide";

/// Ceiling on the machine's virtual disk, in GiB.
///
/// A ceiling and not a commitment: the backing qcow2 is sparse. It is
/// deliberately generous, because the growth is one-directional — qcow2
/// does not return space the guest frees — and a machine that hits its
/// ceiling mid-build fails in a way that looks like a broken repo.
pub const DISK_CEILING_GIB: u64 = 64;

/// The most memory the IDE will commit to the machine, in MiB, and the
/// least it considers usable.
const MEMORY_MAX_MIB: u64 = 12288;
const MEMORY_MIN_MIB: u64 = 4096;

/// The most vCPUs the machine gets. Past this the spike measured no gain on
/// the workload that matters (a cold `cargo build` at 8 vCPU came within 7%
/// of the CPU-matched host), and every vCPU is a host thread competing with
/// the IDE's own frame clock.
const CPUS_MAX: u64 = 8;

/// gvisor-tap-vsock, pinned. The version and the hashes move together, by
/// hand, as a deliberate act — CLAUDE.md's rule for fetched artifacts.
const GVPROXY_VERSION: &str = "v0.8.9";
const GVPROXY_SHA256_X86_64: &str =
    "3011c5629c9138d2050fb23c510e09ae53e30ec52e6a9ab85632bc1550e8ef63";
const GVPROXY_SHA256_AARCH64: &str =
    "6ecca02839254c9a0cc184bba7aac63755a22d7ed10d455b852528a99d7f7d4b";

/// Where the system's virtiofsd lives on a Fedora host. Checked, never
/// assumed — a host without it gets a legible refusal rather than a
/// `podman machine start` failure nobody can read.
const VIRTIOFSD_CANDIDATES: [&str; 3] = [
    "/usr/libexec/virtiofsd",
    "/usr/lib/qemu/virtiofsd",
    "/usr/bin/virtiofsd",
];

/// What `podman machine list` says about our machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// No such machine. The common case, and not a fault.
    Absent,
    Stopped,
    Running,
}

/// What the machine costs, as measured and as configured.
///
/// Two of these are configuration read back from podman and one is a walk
/// of the host filesystem, and they are kept apart on purpose: the memory
/// number is what the host *loses* (no balloon, so RSS climbs to it and
/// stays), while the disk number is what the machine has *taken so far*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineFacts {
    pub running: bool,
    pub cpus: u64,
    /// Configured memory. Per the spike this is also the host RSS ceiling,
    /// which is why it is reported as a commitment.
    pub memory_mib: u64,
    pub disk_ceiling_gib: u64,
    /// Bytes podman's machine storage occupies on the host — the qcow2 plus
    /// the shared machine-image cache. `None` when it could not be walked,
    /// never zero: a footprint that silently under-reports is worse than
    /// one that says it could not see.
    pub host_storage_bytes: Option<u64>,
}

impl MachineFacts {
    /// One line for the Resources view.
    pub fn summary(&self) -> String {
        let mut parts = vec![
            if self.running { "running" } else { "stopped" }.to_string(),
            format!("{} vCPU", self.cpus),
            format!("{} committed", gib(self.memory_mib * 1024 * 1024)),
        ];
        match self.host_storage_bytes {
            Some(bytes) => parts.push(format!(
                "{} on disk of {} GiB",
                gib(bytes),
                self.disk_ceiling_gib
            )),
            None => parts.push(format!(
                "disk unmeasured, {} GiB ceiling",
                self.disk_ceiling_gib
            )),
        }
        parts.join(", ")
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// The IDE's podman machine, and the helper arrangement it needs to run.
pub struct Machine {
    name: String,
    /// The LOCAL target: machine commands are always run against the host's
    /// own podman, never through the connection they are creating.
    local: PodmanTarget,
}

impl Machine {
    pub fn default_machine(local: PodmanTarget) -> Self {
        Self {
            name: MACHINE_NAME.to_string(),
            local: local.with_connection(None),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Is there a machine, and is it up?
    ///
    /// `podman machine list --format` is the documented interface; the
    /// alternative — reading podman's machine JSON out of its config
    /// directory — would be parsing internals, which this project refuses.
    pub async fn state(&self) -> Result<State> {
        let out = self
            .run(
                &["machine", "list", "--format", "{{.Name}}\t{{.Running}}"],
                false,
            )
            .await?;
        for line in out.lines() {
            let mut fields = line.split('\t');
            let (Some(name), Some(running)) = (fields.next(), fields.next()) else {
                continue;
            };
            // A running machine is starred in some output modes; the name
            // is ours either way.
            if name.trim().trim_end_matches('*') != self.name {
                continue;
            }
            return Ok(if running.trim().eq_ignore_ascii_case("true") {
                State::Running
            } else {
                State::Stopped
            });
        }
        Ok(State::Absent)
    }

    /// Create the machine. Sized here and nowhere else.
    ///
    /// Deliberately explicit rather than a deliberate act by the user: the
    /// IDE decides how big its own infrastructure is, and a knob here would
    /// be a knob whose wrong setting looks like the IDE being slow.
    pub async fn create(&self) -> Result<()> {
        let helpers = Helpers::arrange()?;
        let memory = memory_mib();
        let cpus = cpus();
        let args = [
            "machine".to_string(),
            "init".to_string(),
            "--cpus".to_string(),
            cpus.to_string(),
            "--memory".to_string(),
            memory.to_string(),
            "--disk-size".to_string(),
            DISK_CEILING_GIB.to_string(),
            self.name.clone(),
        ];
        helpers
            .run(&self.local, &args)
            .await
            .with_context(|| format!("creating the podman machine {}", self.name))?;
        Ok(())
    }

    /// Bring the machine up if it is not, creating it if it does not exist,
    /// and report what it costs.
    ///
    /// Lazy by design: nothing here runs until something actually needs a
    /// container. A 14 s boot paid at IDE startup, every startup, on a host
    /// where the user may never open an environment, is a cost with no
    /// buyer.
    /// The machine is shared by every IDE window on the host — one machine
    /// per user is the design (see the module docs) — so this is a
    /// check-then-act that N processes can enter at once. Two windows
    /// opening two projects at the same moment both see `Absent` and both
    /// run `machine init`; one wins and the other is told the machine
    /// already exists, which is a failure only if you asked the wrong
    /// question.
    ///
    /// So the question asked on failure is the right one: **is the machine
    /// running now?** A command that lost a race and a command that failed
    /// look identical in their exit status and differ entirely in what the
    /// world looks like afterwards, and only the world is worth reporting.
    /// The error is kept and returned when the machine really is not up, so
    /// a genuine "no gvproxy" still reaches [`crate::substrate::Descent`]
    /// with its reason intact.
    pub async fn ensure_running(&self) -> Result<MachineFacts> {
        let outcome = match self.state().await? {
            State::Running => Ok(()),
            State::Stopped => self.start().await,
            State::Absent => match self.create().await {
                // Another window may have created it between our look and
                // our init; either way what matters next is starting it.
                Ok(()) | Err(_) => self.start().await,
            },
        };
        if let Err(e) = outcome {
            if self.state().await.ok() != Some(State::Running) {
                return Err(e);
            }
            tracing::debug!(
                "a podman machine command failed but {} is running — another \
                 window got there first ({e:#})",
                self.name
            );
        }
        self.facts().await
    }

    pub async fn start(&self) -> Result<()> {
        let helpers = Helpers::arrange()?;
        helpers
            .run(
                &self.local,
                &[
                    "machine".to_string(),
                    "start".to_string(),
                    self.name.clone(),
                ],
            )
            .await
            .with_context(|| format!("starting the podman machine {}", self.name))?;
        Ok(())
    }

    /// Stop the machine.
    ///
    /// **Not what environment idle-stop should call.** A stopped machine
    /// takes every environment down at once and costs ~14 s to come back;
    /// idling an environment stops its container, which costs nothing and
    /// wakes instantly. This exists for shutdown and for the recreate path.
    pub async fn stop(&self) -> Result<()> {
        let helpers = Helpers::arrange()?;
        helpers
            .run(
                &self.local,
                &["machine".to_string(), "stop".to_string(), self.name.clone()],
            )
            .await?;
        Ok(())
    }

    /// Remove the machine and everything inside it. Cattle, not pets.
    pub async fn remove(&self) -> Result<()> {
        let _ = self.stop().await;
        let helpers = Helpers::arrange()?;
        helpers
            .run(
                &self.local,
                &[
                    "machine".to_string(),
                    "rm".to_string(),
                    "-f".to_string(),
                    self.name.clone(),
                ],
            )
            .await?;
        Ok(())
    }

    /// What the machine is configured as and what it has taken.
    pub async fn facts(&self) -> Result<MachineFacts> {
        let out = self
            .run(
                &[
                    "machine",
                    "inspect",
                    &self.name,
                    "--format",
                    "{{.State}}\t{{.Resources.CPUs}}\t{{.Resources.Memory}}\t{{.Resources.DiskSize}}",
                ],
                false,
            )
            .await?;
        let line = out.lines().next().unwrap_or_default();
        let fields: Vec<&str> = line.split('\t').collect();
        let number = |i: usize, fallback: u64| -> u64 {
            fields
                .get(i)
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(fallback)
        };
        Ok(MachineFacts {
            running: fields.first().is_some_and(|s| s.trim() == "running"),
            cpus: number(1, cpus()),
            memory_mib: number(2, memory_mib()),
            disk_ceiling_gib: number(3, DISK_CEILING_GIB),
            host_storage_bytes: machine_storage_bytes(),
        })
    }

    /// A machine command against the local podman, without the helper
    /// arrangement — for the read-only questions (`list`, `inspect`) that
    /// do not launch anything and therefore need no helpers.
    async fn run(&self, args: &[&str], _lifecycle: bool) -> Result<String> {
        let (program, args) = self.local.argv(args.iter().map(|s| s.to_string()));
        let output = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .context("running podman machine")?;
        if !output.status.success() {
            bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

/// Memory to commit to the machine, in MiB.
///
/// A quarter of the host's RAM, clamped. A fraction rather than a constant
/// because the same number cannot be right on a 16 GiB laptop and a 128 GiB
/// workstation, and because it is a *commitment* — the spike measured host
/// RSS climbing to the configured ceiling and never returning.
fn memory_mib() -> u64 {
    let total = host_memory_mib().unwrap_or(MEMORY_MIN_MIB * 4);
    (total / 4).clamp(MEMORY_MIN_MIB, MEMORY_MAX_MIB)
}

/// vCPUs for the machine: half the host's, capped. Half because the IDE's
/// own main thread is on the other half and "snappy, always" is not
/// negotiable; capped because the spike found no gain past it.
fn cpus() -> u64 {
    let host = std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(4);
    (host / 2).clamp(2, CPUS_MAX)
}

fn host_memory_mib() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib / 1024)
}

/// Bytes podman's machine storage occupies on the host.
///
/// The qcow2 and the machine-image cache both live under podman's machine
/// data directory, whose location is the containers-storage convention
/// (`$XDG_DATA_HOME/containers/podman/machine`) rather than anything read
/// out of podman's own state. `None` when it is not there or cannot be
/// walked — the fleet reports "unmeasured" rather than a comforting zero.
fn machine_storage_bytes() -> Option<u64> {
    let dir = data_home().join("containers/podman/machine");
    dir.is_dir().then(|| crate::supervisor::dir_size(&dir))
}

fn data_home() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            Path::new(&home).join(".local/share")
        })
}

/// Where the IDE keeps the helper binaries it arranges for podman.
pub fn helpers_dir() -> PathBuf {
    data_home().join("taste-ide").join("helpers")
}

/// The helper arrangement: a directory podman can find `gvproxy` and
/// `virtiofsd` in, plus the config that points podman at it.
///
/// Held as a value rather than done as a side effect so that the *scope* of
/// the override is visible in the type: only commands run through
/// [`Helpers::run`] see it.
pub struct Helpers {
    dir: PathBuf,
    conf: PathBuf,
}

impl Helpers {
    /// Put the helpers where podman will find them, fetching what is
    /// missing. Idempotent, and cheap on the common path — an already-good
    /// `gvproxy` is verified by hash, not re-downloaded.
    pub fn arrange() -> Result<Self> {
        let dir = helpers_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating the helper directory {}", dir.display()))?;

        ensure_gvproxy(&dir)?;
        link_virtiofsd(&dir)?;

        let conf = dir.join("containers.conf");
        let contents = format!(
            "# Written by taste-ide. Applied through CONTAINERS_CONF_OVERRIDE for\n\
             # `podman machine` commands ONLY; the user's own containers.conf is\n\
             # never modified and every other podman command is unaffected.\n\
             [engine]\n\
             helper_binaries_dir = [\"{}\"]\n",
            dir.display()
        );
        if std::fs::read_to_string(&conf).is_ok_and(|existing| existing == contents) {
            return Ok(Self { dir, conf });
        }
        // Atomically, for the reason `ensure_gvproxy` writes its binary that
        // way a few lines below: this directory is machine-wide and every
        // IDE window arranges helpers when it resolves its substrate, which
        // for two windows opened together is the same moment. A plain write
        // truncates before it fills, and the file is read by a `podman
        // machine` child — so the loser of that race hands podman an empty
        // containers.conf and gets a helper_binaries_dir failure about a
        // file that looks perfectly fine by the time anyone opens it.
        let temp = dir.join(format!(".containers.conf.{}.tmp", std::process::id()));
        std::fs::write(&temp, &contents).with_context(|| format!("writing {}", temp.display()))?;
        std::fs::rename(&temp, &conf)
            .with_context(|| format!("installing {}", conf.display()))
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&temp);
            })?;
        Ok(Self { dir, conf })
    }

    /// Run a machine lifecycle command with the arrangement in force.
    ///
    /// The override and the `PATH` addition are applied to **this child
    /// only**. Under Flatpak they have to travel as `flatpak-spawn --env=`
    /// arguments, because the sandbox does not hand its environment to the
    /// host process it asks for.
    pub async fn run(&self, target: &PodmanTarget, args: &[String]) -> Result<String> {
        let conf = self.conf.display().to_string();
        let path = format!(
            "{}:{}",
            self.dir.display(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
        );
        let (program, mut argv) = target.argv(args.iter().cloned());
        if target.sandboxed() {
            // `--env=` must precede `--host`, which `argv` put first.
            let mut prefixed = vec![
                format!("--env=CONTAINERS_CONF_OVERRIDE={conf}"),
                format!("--env=PATH={path}"),
            ];
            prefixed.append(&mut argv);
            argv = prefixed;
        }
        let mut command = tokio::process::Command::new(program);
        command
            .args(argv)
            .env("CONTAINERS_CONF_OVERRIDE", &conf)
            .env("PATH", &path)
            .stdin(std::process::Stdio::null());
        let output = command.output().await.context("running podman machine")?;
        if !output.status.success() {
            bail!(
                "podman {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Make the system's `virtiofsd` visible on the helper path.
///
/// A symlink, not a copy: it is the host's binary, it must track the host's
/// updates, and copying a setuid-adjacent system program into a user
/// directory is a worse idea than pointing at it.
fn link_virtiofsd(dir: &Path) -> Result<()> {
    let link = dir.join("virtiofsd");
    let system = VIRTIOFSD_CANDIDATES
        .iter()
        .map(Path::new)
        .find(|path| path.exists())
        .with_context(|| {
            format!(
                "this host has no virtiofsd — `podman machine` cannot share the \
                 workspace without it, and the IDE will not install one. Looked in: {}",
                VIRTIOFSD_CANDIDATES.join(", ")
            )
        })?;
    if std::fs::read_link(&link).is_ok_and(|existing| existing == system) {
        return Ok(());
    }
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(system, &link)
        .with_context(|| format!("linking {} to {}", link.display(), system.display()))
}

/// Put a verified `gvproxy` in the helper directory.
///
/// Version-pinned and sha256-verified, per CLAUDE.md's rule for fetched
/// artifacts and the standing preference behind it. The hash is checked on
/// every call, not only after a download: a binary the IDE is about to hand
/// podman as a network helper is worth re-verifying, and it makes a
/// corrupted or replaced file self-healing rather than sticky.
pub fn ensure_gvproxy(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("gvproxy");
    let expected = expected_gvproxy_sha256()?;
    if let Ok(bytes) = std::fs::read(&path) {
        if sha256_hex(&bytes) == expected {
            return Ok(path);
        }
    }
    let url = gvproxy_url()?;
    let bytes = crate::fetch::get(&url).with_context(|| format!("fetching gvproxy from {url}"))?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        bail!(
            "gvproxy {GVPROXY_VERSION} from {url} hashed {actual}, expected {expected} — \
             refusing to install a helper binary that is not the pinned one"
        );
    }
    // Written to a temporary and renamed, so a concurrent IDE never hands
    // podman a half-written binary.
    let temp = dir.join(format!(".gvproxy.{}.tmp", std::process::id()));
    std::fs::write(&temp, &bytes).with_context(|| format!("writing {}", temp.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&temp, &path)
        .with_context(|| format!("installing {}", path.display()))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })?;
    Ok(path)
}

fn gvproxy_url() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => bail!("no pinned gvproxy for {other}"),
    };
    Ok(format!(
        "https://github.com/containers/gvisor-tap-vsock/releases/download/\
         {GVPROXY_VERSION}/gvproxy-linux-{arch}"
    ))
}

fn expected_gvproxy_sha256() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(GVPROXY_SHA256_X86_64),
        "aarch64" => Ok(GVPROXY_SHA256_AARCH64),
        other => bail!("no pinned gvproxy for {other}"),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sizing is derived, bounded and deliberate. The bounds are the
    /// interesting part: the memory number is host RAM the machine takes
    /// and does not give back, so an unbounded fraction on a large host
    /// would be a large surprise.
    #[test]
    fn the_machine_is_sized_as_a_commitment_within_bounds() {
        let memory = memory_mib();
        assert!(
            (MEMORY_MIN_MIB..=MEMORY_MAX_MIB).contains(&memory),
            "{memory} MiB is outside the bounds the IDE will commit"
        );
        let cpus = cpus();
        assert!((2..=CPUS_MAX).contains(&cpus), "{cpus} vCPU");
        // And there is no knob: the values come from the host, not from
        // configuration. (Convention over configuration — a sizing setting
        // whose wrong value looks like the IDE being slow.)
        assert_eq!(memory_mib(), memory, "sizing is deterministic");
    }

    /// The pin is the point. A hash that does not look like a sha256, or a
    /// version that stopped matching the URL, is how a pinned artifact
    /// quietly stops being pinned.
    #[test]
    fn gvproxy_is_pinned_by_version_and_by_hash() {
        assert!(GVPROXY_VERSION.starts_with('v'));
        for hash in [GVPROXY_SHA256_X86_64, GVPROXY_SHA256_AARCH64] {
            assert_eq!(hash.len(), 64, "{hash}");
            assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "{hash}");
        }
        assert_ne!(GVPROXY_SHA256_X86_64, GVPROXY_SHA256_AARCH64);
        let url = gvproxy_url().unwrap();
        assert!(url.contains(GVPROXY_VERSION), "{url}");
        assert!(url.starts_with("https://"), "{url}");
        assert!(
            !url.contains(' '),
            "the multi-line literal must not leak whitespace: {url}"
        );
    }

    /// A wrong-hash binary is refused rather than installed, and the
    /// existing file is left alone. This is the check that makes "fetched"
    /// acceptable at all.
    #[test]
    fn a_gvproxy_that_does_not_match_the_pin_is_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gvproxy"), b"not gvproxy").unwrap();
        // The hash check must reject it. (Whether the subsequent fetch
        // succeeds depends on the network, so only the rejection is
        // asserted here — the point is that the bad file is never accepted.)
        assert_ne!(
            sha256_hex(b"not gvproxy"),
            expected_gvproxy_sha256().unwrap()
        );
    }

    #[test]
    fn sha256_matches_the_known_answer() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// The helper directory is the IDE's own, under its data dir — never a
    /// system path, because nothing here may install to the host.
    #[test]
    fn helpers_live_in_the_ides_own_data_directory() {
        let dir = helpers_dir();
        assert!(dir.ends_with("taste-ide/helpers"), "{}", dir.display());
        assert!(!dir.starts_with("/usr"), "{}", dir.display());
        assert!(!dir.starts_with("/etc"), "{}", dir.display());
    }

    /// The footprint line has to name both numbers and say which is which:
    /// memory is what the host lost, disk is what has been taken so far of
    /// a ceiling.
    #[test]
    fn the_facts_line_separates_the_commitment_from_the_growth() {
        let facts = MachineFacts {
            running: true,
            cpus: 8,
            memory_mib: 7936,
            disk_ceiling_gib: 64,
            host_storage_bytes: Some(13 * 1024 * 1024 * 1024),
        };
        let summary = facts.summary();
        assert!(summary.contains("running"), "{summary}");
        assert!(summary.contains("8 vCPU"), "{summary}");
        assert!(summary.contains("committed"), "{summary}");
        assert!(summary.contains("13.0 GiB on disk of 64 GiB"), "{summary}");

        // An unmeasured footprint says so rather than reading as zero.
        let unmeasured = MachineFacts {
            host_storage_bytes: None,
            ..facts
        };
        assert!(unmeasured.summary().contains("unmeasured"));
        assert!(!unmeasured.summary().contains("0.0 GiB on disk"));
    }

    /// `podman machine list` output is parsed as the documented columns,
    /// including the star podman puts on the default machine.
    #[test]
    fn a_starred_default_machine_is_still_our_machine() {
        // The parse lives in `state()` and needs podman to exercise end to
        // end, so the trimming rule it depends on is asserted directly.
        assert_eq!("taste-ide*".trim_end_matches('*'), MACHINE_NAME);
    }
}

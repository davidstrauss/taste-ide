//! The pool's cloud hosts: VMs of the workspace's pool made in the
//! project's Google Cloud instead of under libvirt (ENVIRONMENTS → "VM
//! provisioners", "Phase 3 — cloud provisioners"; David, 2026-10-03: "I
//! want you to now complete the work to provision environments on cloud
//! VM(s)").
//!
//! **Nothing below the substrate learns a new word.** A cloud host boots
//! the same Fedora CoreOS release as the local pool, from the same
//! Ignition ([`crate::provision::ignition`]) with the same keys, so it
//! holds environments exactly as a local VM does; and it is reached the
//! same way — ssh on a loopback port, which here is the far end of an
//! IAP tunnel (`taste_gcp::gcloud::Gcloud::tunnel`) the IDE keeps open
//! while it uses the host. The [`Vm`] it hands the pool is therefore an
//! ordinary one, `127.0.0.1:<port>`, with [`CloudPlace`] saying where it
//! really is; the keys, `known_hosts`, the podman connection, the git
//! URLs, and the files service work unchanged.
//!
//! **What is kept, and where.** Each host is recorded in
//! `cloud-hosts.json` in the workspace's IDE state: its instance, zone,
//! machine, and the loopback port its tunnel binds — kept, so the port a
//! `known_hosts` line and a podman connection name stays the port for
//! the host's life. The project is the one the title bar's cloud is set
//! up with (`taste_gcp::project`), and the IDE acts there as the project's
//! keyless service account, never as a machine-wide login.
//!
//! **Cost.** A host bills while it runs and keeps only its disk while
//! stopped. It stops by itself: the guest's idle timer powers it off once
//! no ssh session has been open for five minutes, which a GCE instance
//! takes as a stop, and the registry stops one that holds no running
//! container (`EnvironmentRegistry::stop_idle_cloud_hosts`). It starts
//! again when an environment in it is started.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use taste_core::PodmanTarget;

use crate::keys::Keys;
use crate::provision::{DomainState, GuestSpec, Vm, VmFacts};

/// Where a cloud host is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudPlace {
    pub project: String,
    pub zone: String,
    pub machine: String,
}

/// One host, as the workspace's state records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HostRecord {
    name: String,
    project: String,
    zone: String,
    machine: String,
    ssh_port: u16,
    cpus: u32,
    memory_mib: u64,
    release: String,
}

const FILE: &str = "cloud-hosts.json";

/// How long a host may take from "start" to podman answering: a GCE
/// instance boots in a minute or so, and a first boot also runs Ignition.
const READY_TIMEOUT: Duration = Duration::from_secs(8 * 60);

/// What the user is told when a cloud start is asked for and the project
/// is not set up for one.
pub const NOT_SET_UP: &str = "this project has no Google Cloud set up for environments: set \
     the project, sign in, and run Set Up Project in the title bar's cloud, then try again";

/// The tunnels this process holds open, by host.
fn tunnels() -> &'static Mutex<HashMap<String, std::process::Child>> {
    static TUNNELS: OnceLock<Mutex<HashMap<String, std::process::Child>>> = OnceLock::new();
    TUNNELS.get_or_init(Default::default)
}

/// A spawn request for [`spawn_for_life`], and where its child goes.
type Spawn = (
    std::process::Command,
    std::sync::mpsc::Sender<std::io::Result<std::process::Child>>,
);

/// Spawn `command` from a thread that lives as long as the process. A
/// tunnel dies with its parent (`Gcloud::tunnel`), and Linux counts the
/// thread that spawned it as that parent, so a tunnel spawned from one of
/// the runtime's pool threads would die when the pool retired the thread —
/// and one that outlived the IDE held a gcloud open with nobody to close
/// it, which is how they used to accumulate.
fn spawn_for_life(command: std::process::Command) -> std::io::Result<std::process::Child> {
    static SPAWNER: OnceLock<std::sync::mpsc::Sender<Spawn>> = OnceLock::new();
    let spawner = SPAWNER.get_or_init(|| {
        let (asks, asked) = std::sync::mpsc::channel::<Spawn>();
        let _ = std::thread::Builder::new()
            .name("taste-tunnels".into())
            .spawn(move || {
                for (mut command, reply) in asked {
                    let _ = reply.send(command.spawn());
                }
            });
        asks
    });
    let gone = || std::io::Error::other("the tunnels' spawning thread is gone");
    let (reply, answer) = std::sync::mpsc::channel();
    spawner.send((command, reply)).map_err(|_| gone())?;
    answer.recv().map_err(|_| gone())?
}

/// Each workspace's host records, read from its state once and written
/// through: the fleet asks which environments are in the cloud on the GTK
/// thread, and that must not be a file read.
fn record_cache() -> &'static Mutex<HashMap<PathBuf, Vec<HostRecord>>> {
    static RECORDS: OnceLock<Mutex<HashMap<PathBuf, Vec<HostRecord>>>> = OnceLock::new();
    RECORDS.get_or_init(Default::default)
}

/// The last state seen for each host, so listing the pool asks nobody.
fn known_states() -> &'static Mutex<HashMap<String, DomainState>> {
    static KNOWN: OnceLock<Mutex<HashMap<String, DomainState>>> = OnceLock::new();
    KNOWN.get_or_init(Default::default)
}

fn remember(name: &str, state: DomainState) {
    known_states()
        .lock()
        .unwrap()
        .insert(name.to_string(), state);
}

/// GCE's instance status as the pool's states: only `RUNNING` is running,
/// and a stopped instance is shut off.
fn state_of(status: Option<&str>) -> DomainState {
    match status {
        Some("RUNNING") => DomainState::Running,
        Some("TERMINATED") | Some("STOPPED") | None => DomainState::ShutOff,
        Some(other) => DomainState::Other(other.to_ascii_lowercase()),
    }
}

/// A workspace's cloud hosts.
#[derive(Clone)]
pub struct CloudSession {
    workspace_root: PathBuf,
    sandboxed: bool,
    /// The VM log, as the local provisioner keeps one
    /// (`LibvirtSession::with_sink`).
    sink: Option<crate::provision::StepSink>,
}

impl std::fmt::Debug for CloudSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudSession")
            .field("workspace_root", &self.workspace_root)
            .finish()
    }
}

impl CloudSession {
    pub fn new(workspace_root: &Path) -> Self {
        Self {
            workspace_root: workspace_root.to_path_buf(),
            sandboxed: taste_core::podman::sandboxed(),
            sink: None,
        }
    }

    /// The same session, telling each step to `sink` as well as to the
    /// IDE log.
    pub fn with_sink(mut self, sink: crate::provision::StepSink) -> Self {
        self.sink = Some(sink);
        self
    }

    /// One step, for the VM's log and the IDE's.
    pub fn say(&self, host: &str, line: &str) {
        tracing::info!("{host}: {line}");
        if let Some(sink) = &self.sink {
            sink(host, line.to_string());
        }
    }

    fn state_dir(&self) -> PathBuf {
        taste_core::state::workspace_state_dir(&self.workspace_root)
    }

    /// The project hosts are made in, when the title bar's cloud names
    /// one and the IDE's own gcloud is there to act in it.
    pub fn project(&self) -> Option<String> {
        let project = taste_gcp::project::load(&self.state_dir())
            .ok()
            .flatten()?
            .project;
        let binary =
            taste_gcp::gcloud::binary(&taste_gcp::gcloud::sdk_root(), &taste_gcp::gcloud::SDK);
        binary.exists().then_some(project)
    }

    /// Whether a cloud host can be made or reached at all, and why not.
    pub fn available(&self) -> Result<String> {
        self.project().context(NOT_SET_UP)
    }

    fn gcloud(&self, project: &str) -> taste_gcp::gcloud::Gcloud {
        taste_gcp::project::gcloud(
            &self.state_dir(),
            taste_gcp::gcloud::binary(&taste_gcp::gcloud::sdk_root(), &taste_gcp::gcloud::SDK),
            project,
        )
    }

    fn gcp(&self, project: &str) -> taste_gcp::rest::Gcp {
        taste_gcp::rest::Gcp::new(
            std::sync::Arc::new(taste_gcp::rest::TokenSource::gcloud(self.gcloud(project))),
            taste_gcp::rest::Endpoints::default(),
        )
    }

    fn records(&self) -> Vec<HostRecord> {
        let dir = self.state_dir();
        let mut cache = record_cache().lock().unwrap();
        cache
            .entry(dir.clone())
            .or_insert_with(|| {
                std::fs::read(dir.join(FILE))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                    .unwrap_or_default()
            })
            .clone()
    }

    fn store(&self, records: &[HostRecord]) -> Result<()> {
        let dir = self.state_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(FILE);
        let part = path.with_extension("json.part");
        std::fs::write(&part, serde_json::to_vec_pretty(records)?)?;
        std::fs::rename(&part, &path).with_context(|| format!("writing {}", path.display()))?;
        record_cache().lock().unwrap().insert(dir, records.to_vec());
        Ok(())
    }

    fn record(&self, name: &str) -> Result<HostRecord> {
        self.records()
            .into_iter()
            .find(|r| r.name == name)
            .with_context(|| format!("{name} is not one of this workspace's cloud hosts"))
    }

    fn vm_of(&self, record: &HostRecord) -> Vm {
        Vm {
            domain: record.name.clone(),
            ssh_port: record.ssh_port,
            workspace_root: self.workspace_root.clone(),
            state: known_states()
                .lock()
                .unwrap()
                .get(&record.name)
                .cloned()
                .unwrap_or(DomainState::ShutOff),
            cloud: Some(CloudPlace {
                project: record.project.clone(),
                zone: record.zone.clone(),
                machine: record.machine.clone(),
            }),
        }
    }

    fn location(vm: &Vm) -> Result<taste_gcp::resources::Location> {
        let place = vm
            .cloud
            .as_ref()
            .with_context(|| format!("{} is not a cloud host", vm.domain))?;
        taste_gcp::resources::Location::new(&place.project, &place.zone)
    }

    /// The workspace's hosts, as last seen — no request is made, so the
    /// pool can be listed as often as it likes.
    pub fn list(&self) -> Vec<Vm> {
        self.records().iter().map(|r| self.vm_of(r)).collect()
    }

    /// What a host gives, from its record.
    pub fn facts(&self, vm: &Vm) -> Result<VmFacts> {
        let record = self.record(&vm.domain)?;
        Ok(VmFacts {
            running: vm.state == DomainState::Running,
            cpus: u64::from(record.cpus),
            memory_mib: record.memory_mib,
            disk_ceiling_gib: taste_gcp::hosts::HOST_DISK_GIB,
            host_storage_bytes: None,
        })
    }

    /// The release a host was made from: the pool's, at the time.
    pub fn release_of(&self, vm: &Vm) -> Option<String> {
        self.record(&vm.domain).ok().map(|r| r.release)
    }

    /// One more host: the pool's guest, made in the project's cloud in the
    /// first zone of its region that has a host machine, recorded, and
    /// left booting — [`Self::ensure_running`] waits for it.
    pub async fn create(&self, report: &(dyn Fn(&str) + Sync)) -> Result<Vm> {
        let project = self.available()?;
        let gcp = self.gcp(&project);
        let ws = taste_gcp::project::workspace(&self.state_dir())?;
        let keys = Keys::for_workspace(&self.workspace_root);
        keys.ensure().await?;
        let name = ws.name(&format!("env-{}", short_suffix()))?;
        let release = crate::guest::image()?.release;
        let user_data = crate::provision::ignition(&GuestSpec {
            ssh_public_key: keys.identity_public()?,
            hostname: name.clone(),
            deny_private_networks: true,
            host_key: Some(keys.host_key()?),
            metadata_server: true,
        })?;
        let zones =
            taste_gcp::hosts::zones(&gcp, &project, taste_gcp::model::DEFAULT_REGION).await?;
        report(&format!("making cloud host {name} in {project}"));
        let image = taste_gcp::guest::fcos_image(&release);
        let (loc, machine) = taste_gcp::hosts::create(
            &gcp,
            &ws,
            &project,
            &zones,
            &taste_gcp::hosts::HostRequest {
                name: &name,
                image: &image,
                user_data: &user_data,
            },
            report,
        )
        .await?;
        let (cpus, memory_mib) = taste_gcp::hosts::machine_facts(&gcp, &loc, &machine).await?;
        let ssh_port = crate::provision::free_loopback_port()?;
        let record = HostRecord {
            name: name.clone(),
            project: project.clone(),
            zone: loc.zone.clone(),
            machine: machine.clone(),
            ssh_port,
            cpus,
            memory_mib,
            release,
        };
        let mut records = self.records();
        records.push(record.clone());
        self.store(&records)?;
        keys.record_host(ssh_port)?;
        remember(&name, DomainState::Running);
        report(&format!(
            "{name} made on {machine} in {}: {cpus} vCPU, {memory_mib} MiB, reached through \
             IAP on 127.0.0.1:{ssh_port}",
            loc.zone
        ));
        Ok(self.vm_of(&record))
    }

    /// The host's state, asked of GCP.
    pub async fn state(&self, vm: &Vm) -> Result<DomainState> {
        let place = vm
            .cloud
            .as_ref()
            .with_context(|| format!("{} is not a cloud host", vm.domain))?;
        let gcp = self.gcp(&place.project);
        let status = taste_gcp::hosts::status(&gcp, &Self::location(vm)?, &vm.domain).await?;
        let state = state_of(status.as_deref());
        remember(&vm.domain, state.clone());
        Ok(state)
    }

    /// Bring a host up — started if it was stopped, its tunnel open, ssh
    /// and podman answering through it — and report what it gives.
    pub async fn ensure_running(&self, vm: &Vm, report: &(dyn Fn(&str) + Sync)) -> Result<VmFacts> {
        let place = vm
            .cloud
            .clone()
            .with_context(|| format!("{} is not a cloud host", vm.domain))?;
        let gcp = self.gcp(&place.project);
        let loc = Self::location(vm)?;
        match taste_gcp::hosts::status(&gcp, &loc, &vm.domain).await? {
            None => bail!(
                "cloud host {} is gone from {}; its environments are placed anew",
                vm.domain,
                place.project
            ),
            Some(status) if status == "RUNNING" => {}
            Some(status) if status == "STOPPING" || status == "SUSPENDING" => {
                // A stop under way finishes before a start is accepted.
                report(&format!(
                    "{} is stopping; waiting to start it again",
                    vm.domain
                ));
                let deadline = Instant::now() + Duration::from_secs(180);
                while taste_gcp::hosts::status(&gcp, &loc, &vm.domain)
                    .await?
                    .is_some_and(|s| s == "STOPPING" || s == "SUSPENDING")
                {
                    if Instant::now() > deadline {
                        bail!("{} did not finish stopping", vm.domain);
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                report(&format!("starting cloud host {}", vm.domain));
                taste_gcp::hosts::start(&gcp, &loc, &vm.domain).await?;
            }
            Some(_) => {
                report(&format!("starting cloud host {}", vm.domain));
                taste_gcp::hosts::start(&gcp, &loc, &vm.domain).await?;
            }
        }
        remember(&vm.domain, DomainState::Running);
        self.wait_ready(vm, &place, report).await?;
        let mut facts = self.facts(vm)?;
        facts.running = true;
        Ok(facts)
    }

    /// The tunnel to a host's sshd, opened if it is not: one `gcloud
    /// compute start-iap-tunnel` per host for as long as this process
    /// lives, restarted when it has exited.
    fn ensure_tunnel(&self, vm: &Vm, place: &CloudPlace) -> Result<()> {
        let mut tunnels = tunnels().lock().unwrap();
        if let Some(child) = tunnels.get_mut(&vm.domain) {
            if matches!(child.try_wait(), Ok(None)) {
                return Ok(());
            }
        }
        let log = std::fs::File::create(
            self.state_dir()
                .join(format!("cloud-tunnel-{}.log", vm.domain)),
        )
        .context("opening the tunnel's log")?;
        let mut command =
            self.gcloud(&place.project)
                .tunnel(&vm.domain, &place.zone, 22, vm.ssh_port);
        command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(log));
        let child =
            spawn_for_life(command).context("starting gcloud's IAP tunnel to the cloud host")?;
        tunnels.insert(vm.domain.clone(), child);
        Ok(())
    }

    /// Close a host's tunnel, if one is open.
    fn close_tunnel(&self, vm: &Vm) {
        if let Some(mut child) = tunnels().lock().unwrap().remove(&vm.domain) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Wait for ssh through the tunnel, register the podman connection,
    /// and wait for podman in the guest — the cloud half of
    /// `LibvirtSession::wait_ready`. The tunnel accepts a connection before
    /// the host does, so the test is an ssh round trip, not a connect.
    async fn wait_ready(
        &self,
        vm: &Vm,
        place: &CloudPlace,
        report: &(dyn Fn(&str) + Sync),
    ) -> Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        let keys = Keys::for_workspace(&self.workspace_root);
        report(&format!(
            "waiting for {}'s sshd through IAP on 127.0.0.1:{}",
            vm.domain, vm.ssh_port
        ));
        loop {
            self.ensure_tunnel(vm, place)?;
            let (program, args) = keys.ssh_argv(vm.ssh_port, ["true"]);
            let reached = tokio::time::timeout(
                Duration::from_secs(30),
                tokio::process::Command::new(program)
                    .args(args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status(),
            )
            .await;
            if matches!(reached, Ok(Ok(status)) if status.success()) {
                break;
            }
            if Instant::now() > deadline {
                bail!(
                    "{} did not answer ssh through IAP within {} minutes (see {})",
                    vm.domain,
                    READY_TIMEOUT.as_secs() / 60,
                    self.state_dir()
                        .join(format!("cloud-tunnel-{}.log", vm.domain))
                        .display()
                );
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        report("sshd answers; registering the podman connection");
        crate::provision::register_connection(vm, self.sandboxed).await?;
        let target = PodmanTarget::connection(&vm.domain, self.sandboxed);
        loop {
            match crate::substrate::probe(&target).await {
                Ok(()) => {
                    report(&format!(
                        "podman in {} answers; the host is ready",
                        vm.domain
                    ));
                    return Ok(());
                }
                Err(e) if Instant::now() > deadline => bail!(
                    "podman in {} did not answer within {} minutes ({e})",
                    vm.domain,
                    READY_TIMEOUT.as_secs() / 60
                ),
                Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
    }

    /// Stop a host: its tunnel closed, the instance stopped. Its disk —
    /// checkouts, images, volumes — stays for the next start.
    pub async fn stop(&self, vm: &Vm) -> Result<()> {
        self.close_tunnel(vm);
        let place = vm
            .cloud
            .as_ref()
            .with_context(|| format!("{} is not a cloud host", vm.domain))?;
        taste_gcp::hosts::stop(&self.gcp(&place.project), &Self::location(vm)?, &vm.domain).await?;
        remember(&vm.domain, DomainState::ShutOff);
        Ok(())
    }

    /// Delete a host and its disk, and forget it.
    pub async fn destroy(&self, vm: &Vm) -> Result<()> {
        self.close_tunnel(vm);
        let place = vm
            .cloud
            .as_ref()
            .with_context(|| format!("{} is not a cloud host", vm.domain))?;
        taste_gcp::hosts::delete(&self.gcp(&place.project), &Self::location(vm)?, &vm.domain)
            .await?;
        let records: Vec<HostRecord> = self
            .records()
            .into_iter()
            .filter(|r| r.name != vm.domain)
            .collect();
        self.store(&records)?;
        known_states().lock().unwrap().remove(&vm.domain);
        Ok(())
    }
}

/// Six lowercase characters for a host's name, as the local pool's domain
/// names have.
fn short_suffix() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 6];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok();
    if !filled {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        bytes = nanos.to_le_bytes().repeat(2)[..6]
            .try_into()
            .unwrap_or([7; 6]);
    }
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_running_is_running_and_a_stopped_host_is_shut_off() {
        assert_eq!(state_of(Some("RUNNING")), DomainState::Running);
        assert_eq!(state_of(Some("TERMINATED")), DomainState::ShutOff);
        assert_eq!(state_of(None), DomainState::ShutOff);
        assert_eq!(
            state_of(Some("STAGING")),
            DomainState::Other("staging".into())
        );
    }

    #[test]
    fn a_host_name_suffix_is_six_name_characters() {
        let suffix = short_suffix();
        assert_eq!(suffix.len(), 6);
        assert!(suffix
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
    }

    #[test]
    fn a_hosts_record_lists_as_a_loopback_vm_that_says_where_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let session = CloudSession::new(dir.path());
        let record = HostRecord {
            name: "taste-0a1b2c3d-env-abc123".into(),
            project: "p".into(),
            zone: "us-central1-b".into(),
            machine: "n4-standard-8".into(),
            ssh_port: 42022,
            cpus: 8,
            memory_mib: 32768,
            release: "44.20260829.3.1".into(),
        };
        let vm = session.vm_of(&record);
        assert!(vm.is_cloud());
        assert_eq!(vm.ssh_port, 42022);
        assert_eq!(
            vm.podman_uri(),
            "ssh://core@127.0.0.1:42022/run/user/1000/podman/podman.sock"
        );
        assert_eq!(vm.cloud.as_ref().unwrap().zone, "us-central1-b");
    }

    #[test]
    fn a_cloud_guest_keeps_its_lease_and_clock_but_not_the_metadata_http() {
        let text = crate::provision::ignition(&GuestSpec {
            ssh_public_key: "ssh-ed25519 AAAA test".into(),
            hostname: "taste-x".into(),
            deny_private_networks: true,
            host_key: None,
            metadata_server: true,
        })
        .unwrap();
        let config: serde_json::Value = serde_json::from_str(&text).unwrap();
        let rules = config["storage"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == "/etc/sysconfig/nftables.conf")
            .unwrap()["contents"]["source"]
            .as_str()
            .unwrap()
            .to_string();
        let decoded = urlencoding_decode(rules.trim_start_matches("data:,"));
        assert!(
            decoded.contains("ip daddr 169.254.169.254 udp dport { 67, 123 } accept"),
            "{decoded}"
        );
        // The accept comes before the private networks' reject.
        let accept = decoded.find("169.254.169.254").unwrap();
        let reject = decoded.find("reject").unwrap();
        assert!(accept < reject);
        assert!(!decoded.contains("tcp dport 80 accept"));
    }

    fn urlencoding_decode(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let Ok(byte) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}

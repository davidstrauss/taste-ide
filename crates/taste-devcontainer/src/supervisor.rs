//! One environment's devcontainer lifecycle state machine.
//!
//! ```text
//! NoConfig → ConfigDetected → Building → Starting → Running
//!                  ↑_____________________________↓
//!                      pending changes (config drift)
//! ```
//!
//! A `Supervisor` supervises exactly **one environment** — never "the"
//! devcontainer. Its identity (which environment, whose workspace, which
//! checkout) is injected by the [`crate::EnvironmentRegistry`] rather than
//! derived from a single root, and every state mutex, drift flag, log ring
//! and watcher in here is per-environment by construction. That is why
//! there is no environment id threaded through the methods: an instance
//! *is* the environment.
//!
//! The supervisor never restarts the IDE and never touches agent sessions:
//! a reload tears down and recreates only its container, then re-points
//! that environment's [`ExecContext`] so *new* terminals and commands land
//! inside it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use taste_core::environment::{
    self, Checkout, DiskBudgetScope, EnvironmentId, LABEL_AUTHORITY, LABEL_CONFIG_HASH, LABEL_ENV,
    LABEL_PORTS, LABEL_WORKSPACE,
};
use taste_core::event::DevcontainerStateEvent;
use taste_core::{ConfigAuthority, Event, EventBus, ExecContext};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::channel::{ChannelPaths, ChannelServices, EnvChannel};
use crate::hash::build_hash;
use crate::{config::lifecycle_commands, config_hash, DevcontainerConfig};

const LOG_RING_CAPACITY: usize = 2000;

/// One podman resource associated with this workspace's devcontainer, for
/// the environment view (and the read-only MCP mirror).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceInfo {
    pub kind: ResourceKind,
    pub name: String,
    pub id: String,
    pub status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Container,
    Image,
    Volume,
    /// The substrate itself — the machine or remote host the other three
    /// live on. Present only when that is not the user's own host, because
    /// a row saying "your computer" explains nothing; a row saying what a
    /// VM has taken explains a number nothing else accounts for.
    Substrate,
}

/// One environment's footprint on disk, as measured.
///
/// Two numbers and two counts, because "how big is this environment" has an
/// honest answer only for the parts that could actually be walked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskUsage {
    /// The environment's checkout — its clone, build artifacts included.
    pub checkout_bytes: u64,
    /// The volumes that could be measured, summed.
    pub volume_bytes: u64,
    pub volumes_measured: usize,
    /// Volumes that exist but whose contents this process cannot read.
    pub volumes_unmeasured: usize,
}

/// What an environment's own volumes came to, counted both ways: the
/// display wants the apparent size the rest of the footprint is in, and the
/// budget wants what the disk actually gave up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct VolumeUsage {
    apparent_bytes: u64,
    on_disk_bytes: u64,
    measured: usize,
    unmeasured: usize,
}

impl DiskUsage {
    pub fn total_bytes(&self) -> u64 {
        self.checkout_bytes + self.volume_bytes
    }

    /// Whether some of the footprint is missing from the total.
    pub fn partial(&self) -> bool {
        self.volumes_unmeasured > 0
    }
}

/// One environment's footprint against the workspace's disk budget, as it
/// was last measured.
///
/// Cached on the supervisor and read — never computed — by the gates in
/// `taste-mcp`. A tool call that had to walk a checkout to answer would be
/// a `du` over `target/` on the request path, which is the same objection
/// that keeps the running cap off the fleet snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSample {
    /// Bytes actually consumed, summed over whatever
    /// [`taste_core::environment::DISK_BUDGET_SCOPE`] said to sum when this
    /// was taken. The number the budget weighs.
    pub budget_bytes: u64,
    /// What the environment costs the filesystem in total — clone, build
    /// artifacts, and volumes alike.
    ///
    /// `Some` only when something actually walked the artifacts: the whole
    /// scope's own measurement, or the user's explicit Refresh in the
    /// environments view. Under the clone scope the cadence deliberately
    /// prunes at `target/` and never learns this, and a guess in its place
    /// would be worse than the honest absence.
    pub whole_bytes: Option<u64>,
    /// Volumes that exist and could not be read. Non-zero means
    /// [`Self::whole_bytes`] is a floor rather than a total.
    pub unmeasured_volumes: usize,
    /// When the walk finished, so a report can say how old its number is.
    pub at: std::time::Instant,
}

/// What a walk of a checkout found: the same files counted two ways and
/// split two ways, because "how big is this" and "what does this cost the
/// disk" are different questions with different right answers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CheckoutWalk {
    /// Every file's *length*, summed — what `du --apparent-size` reports,
    /// and what a person means by "how big is this checkout".
    pub apparent_bytes: u64,
    /// Every file's *allocated blocks*, summed (`st_blocks` × 512) — what
    /// plain `du` reports, and the honest number for a budget, which is a
    /// claim about a disk filling up rather than about how long the files
    /// are. The two differ in both directions: a sparse file occupies less
    /// than its length, and a thousand one-byte files occupy a block each.
    /// This runs on fuse-overlayfs, which passes the underlying
    /// filesystem's `st_blocks` through, so what comes back is the lower
    /// filesystem's answer — the one worth having.
    pub on_disk_bytes: u64,
    /// Of those allocated bytes, the ones git does not ignore: the clone
    /// itself, without the build output. The project's own `.gitignore` is
    /// the only statement of which files are a cache that does not have to
    /// be maintained separately, and on this repository the two numbers
    /// differ by three hundred times.
    pub clone_on_disk_bytes: u64,
    /// Whether ignored directories were skipped rather than counted, which
    /// makes the two totals above partial by exactly that much.
    pub pruned_ignored: bool,
}

/// Walk a checkout, following no symlinks (a link out of a clone is not the
/// clone's disk) and giving up quietly on what cannot be read.
///
/// `prune_ignored` is what makes the clone scope cheap: an ignored
/// directory is not descended into at all, so a 111 GiB `target/` costs one
/// `is_path_ignored` call rather than a walk of every object in it. It is
/// also why libgit2 is asked about directories on the way down and about
/// files only inside directories that survived — within the clone proper
/// that is a few thousand questions, and inside the build output it would
/// be millions.
pub(crate) fn walk_checkout(root: &Path, prune_ignored: bool) -> CheckoutWalk {
    use std::os::unix::fs::MetadataExt;

    // The clone's OWN repository, never an ancestor's: `discover` walks up,
    // and a stand-in workspace nested under some other checkout would
    // otherwise be measured against rules that are not its.
    let here = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let git = taste_git::GitWorkspace::discover(root).filter(|git| {
        std::fs::canonicalize(git.workdir()).unwrap_or_else(|_| git.workdir().to_path_buf()) == here
    });
    let ignored = |path: &Path| git.as_ref().is_some_and(|git| git.ignores(path));

    let mut walk = CheckoutWalk {
        pruned_ignored: prune_ignored,
        ..CheckoutWalk::default()
    };
    // Each directory carries whether anything above it was ignored: once a
    // parent is out, everything under it is out, and nothing below has to
    // be asked again.
    let mut stack = vec![(root.to_path_buf(), false)];
    while let Some((current, under_ignored)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            let out = under_ignored || ignored(&path);
            if kind.is_dir() {
                if out && prune_ignored {
                    continue;
                }
                stack.push((path, out));
            } else if let Ok(meta) = entry.metadata() {
                walk.apparent_bytes += meta.len();
                let on_disk = meta.blocks() * 512;
                walk.on_disk_bytes += on_disk;
                if !out {
                    walk.clone_on_disk_bytes += on_disk;
                }
            }
        }
    }
    walk
}

/// Apparent bytes under `dir`, ignoring nothing — the footprint the
/// environments view has always shown, and the right answer for a
/// directory that is not a checkout at all (a podman machine's image).
pub(crate) fn dir_size(dir: &Path) -> u64 {
    walk_checkout(dir, false).apparent_bytes
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorState {
    NoConfig,
    ConfigDetected,
    Building,
    Starting,
    Running { container_id: String },
    Failed { message: String },
    Stopped,
}

/// One environment's situation, said for an agent: see
/// [`Supervisor::situation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Situation {
    /// `container` when the project's own config is running, `safe`
    /// otherwise.
    pub mode: &'static str,
    /// Whose config is in force: `project`, `baseline`, or `none` when
    /// nothing is running at all.
    pub authority: &'static str,
    /// What the agent may write, as a phrase.
    pub writable: String,
    /// The last thing that went wrong, if anything: a failed build, a
    /// passed-over config, or a failed lifecycle command.
    pub failure: Option<String>,
    /// What to do next, in one or two sentences naming the tool.
    pub next: String,
}

impl SupervisorState {
    /// Whether this environment is spending the machine right now: a
    /// container up, or the build and the start that produce one.
    ///
    /// This is what the orchestration cap counts
    /// ([`taste_core::environment::MAX_ORCHESTRATED_ENVIRONMENTS`]), and each
    /// of the three variants is here for its own reason. `Running` is the
    /// obvious one. `Starting` is a container that already exists. `Building`
    /// is the most expensive state in the whole list — a cold image build is
    /// the heaviest thing this IDE ever asks of a laptop — and leaving
    /// either of the last two out would let six starts in a row pass a cap
    /// that none of them had come up to spend yet.
    ///
    /// Everything else costs a clone on disk and nothing more: `Stopped`,
    /// `Failed`, and the two states of an environment that was never built.
    pub fn holds_a_container(&self) -> bool {
        matches!(self, Self::Running { .. } | Self::Starting | Self::Building)
    }

    fn to_event(&self) -> DevcontainerStateEvent {
        match self {
            SupervisorState::NoConfig => DevcontainerStateEvent::NoConfig,
            SupervisorState::ConfigDetected => DevcontainerStateEvent::ConfigDetected,
            SupervisorState::Building => DevcontainerStateEvent::Building,
            SupervisorState::Starting => DevcontainerStateEvent::Starting,
            SupervisorState::Running { container_id } => DevcontainerStateEvent::Running {
                container_id: container_id.clone(),
            },
            SupervisorState::Failed { message } => DevcontainerStateEvent::Failed {
                message: message.clone(),
            },
            SupervisorState::Stopped => DevcontainerStateEvent::Stopped,
        }
    }
}

/// Whether a review state and a container state together mean "stop it".
///
/// Pure, and separate from [`Supervisor::apply_review_state`], because this
/// is the whole of the decision and the rest is podman. Two properties it
/// exists to pin down:
///
/// - **Only work-in-progress keeps a container.** Flagged, merged and
///   rejected all mean nobody is talking to that world
///   ([`taste_core::ReviewState::should_be_stopped`]).
/// - **Stopping something already down is not idempotence, it is noise.**
///   A supervisor with no container — never built, already stopped, failed
///   — is left exactly as it is, so the fleet does not log a stop per
///   refresh. That is [`SupervisorState::holds_a_container`], the same
///   predicate the orchestration cap counts with: "there is something to
///   stop" and "this one is spending the machine" are one question asked
///   from two sides, and two spellings of it would eventually disagree.
pub fn stop_wanted(review: taste_core::ReviewState, state: &SupervisorState) -> bool {
    review.should_be_stopped() && state.holds_a_container()
}

/// Which environment a [`Supervisor`] is, injected at construction.
///
/// Three facts, deliberately separate: the environment's slug, the
/// workspace every podman name is keyed by, and *this* environment's
/// checkout. For the primary environment the last two are the same path;
/// for every other environment the checkout is a clone under
/// `$XDG_STATE_HOME`. Nothing in the supervisor may re-derive one from
/// another — that conflation is exactly what made the single-environment
/// scheme unable to grow a second one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentIdentity {
    pub id: EnvironmentId,
    pub workspace_root: PathBuf,
    /// Where the working copy is — the tree the container mounts and the
    /// agent edits. See [`Checkout`] for the two worlds it can be in.
    pub checkout: Checkout,
    /// The repository on this host that holds the environment's refs: the
    /// same directory as a local checkout, and the host-side half of a
    /// remote one. Review, publish, and every other read of history goes
    /// here, never to the checkout.
    pub peer: PathBuf,
}

impl EnvironmentIdentity {
    /// The primary environment: the main checkout itself.
    pub fn primary(workspace_root: impl Into<PathBuf>) -> Self {
        let workspace_root = workspace_root.into();
        Self {
            id: EnvironmentId::primary(),
            checkout: Checkout::Local(workspace_root.clone()),
            peer: workspace_root.clone(),
            workspace_root,
        }
    }

    /// A non-primary environment, rooted at its clone on this host.
    pub fn cloned(workspace_root: impl Into<PathBuf>, id: EnvironmentId) -> Self {
        let workspace_root = workspace_root.into();
        Self::local_at(
            workspace_root.clone(),
            id.clone(),
            environment::env_repo_root(&workspace_root, &id),
        )
    }

    /// An environment whose checkout and peer are one directory on this
    /// host.
    pub fn local_at(workspace_root: impl Into<PathBuf>, id: EnvironmentId, root: PathBuf) -> Self {
        Self {
            id,
            workspace_root: workspace_root.into(),
            checkout: Checkout::Local(root.clone()),
            peer: root,
        }
    }

    /// Whether this is the main checkout — by identity or by directory.
    fn is_main_checkout(&self) -> bool {
        self.id.is_primary() || self.checkout.path() == self.workspace_root
    }
}

/// Whether this environment's container can host the chat's agent process
/// itself, rather than merely running the commands it brokers.
///
/// Relocation is not a promise the IDE can make on a repo's behalf: the
/// container is built from the repo's own config, and an agent needs two
/// things in there that a devcontainer is not obliged to have. So this is
/// answered by asking the container, once per container, and a `No` is
/// reported rather than worked around — the chat keeps the outside-confined
/// topology, which works everywhere and is where safe mode lives anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentHosting {
    /// Not asked yet: no container, or one adopted from a previous IDE run
    /// whose probe has not come back.
    Unknown,
    /// The container has `node` and a writable agent home.
    Yes,
    /// It does not, and here is what to tell the user.
    No { reason: String },
}

/// Which configuration the supervisor resolved, and why.
///
/// The ladder has three rungs and this names the first two. The third — no
/// podman at all — is not a config choice but the absence of one: the
/// baseline build fails, the environment lands in `Failed` with no exec
/// target, and the agent keeps the outside-confined topology that works
/// everywhere. That rung is reached by falling off this enum, not by a
/// variant of it.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub config: DevcontainerConfig,
    pub authority: ConfigAuthority,
    /// Why the project's config was passed over, when it was. Shown in the
    /// log so "why am I in safe mode" is answerable without guessing.
    pub reason: Option<String>,
}

pub struct Supervisor {
    env: EnvironmentIdentity,
    events: EventBus,
    exec: ExecContext,
    state: Mutex<SupervisorState>,
    /// Whose config the container currently running (or last started) was
    /// built from. `Project` until something says otherwise, so an
    /// environment that has never run reports the working mode's authority
    /// rather than claiming a baseline it does not have.
    authority: Mutex<ConfigAuthority>,
    /// Whether the running container can host a relocated agent. Reset
    /// whenever the container changes, because it is a fact about *that*
    /// container and not about the environment.
    hosting: Mutex<AgentHosting>,
    /// The environment's own agent asked for the reload in flight
    /// (`devcontainer_reload`, approved): when it finishes, the outcome is
    /// published as `Event::ReloadReport` for that agent's chat to hand it.
    agent_reload: AtomicBool,
    /// This environment's live channel to its container, if it has one. The
    /// helper on the far end binds the sockets a relocated agent dials, so
    /// this is what makes relocation reachable at all — see
    /// [`crate::channel`]. Keyed to the container's life: dropped whenever
    /// the container goes, because the helper goes with it.
    channel: tokio::sync::Mutex<Option<Arc<EnvChannel>>>,
    /// What the IDE serves down that channel. Injected, because the MCP
    /// server and the auth proxy live in crates this one does not (and must
    /// not) depend on. `None` in the unit tests and before the window wires
    /// it up, which is honestly reported as "cannot host an agent" rather
    /// than guessed at.
    channel_services: Mutex<Option<Arc<dyn ChannelServices>>>,
    /// Hash of the config the running container was created from.
    running_hash: Mutex<Option<String>>,
    /// The forwarded ports of the config this environment last resolved —
    /// what the file tree's Ports section lists. Recorded by
    /// `resolve_config`, so it is never a filesystem read at render time.
    declared_ports: Mutex<Vec<crate::config::PortSpec>>,
    /// Which localhost port each forwarded port is actually published on
    /// (container port → host port), filled at start and on adoption
    /// (`LABEL_PORTS`). Empty means each on its own number.
    published_ports: Mutex<std::collections::HashMap<u16, u16>>,
    /// Why the project's config was passed over at the last resolution,
    /// when a config exists and was: what the row, a toast, and
    /// `devcontainer_status` say, because a checkout whose devcontainer.json
    /// is refused otherwise looks exactly like one with none — the
    /// baseline runs either way and nothing drifts — and the user who just
    /// wrote the file waits for a prompt that never comes (David,
    /// 2026-09-16: "the IDE didn't reload into it or even ask me").
    passed_over: Mutex<Option<String>>,
    /// The project config whose image would not build or pull, by the
    /// hash of the whole setup, with podman's word for why. While the
    /// files on disk are the ones that failed, resolution passes them over
    /// for the baseline — so the repair loop has a shell and a writable
    /// `.devcontainer/` instead of the rung below both, where the agent
    /// met a read-only stand-in and "File does not exist" for the config
    /// it had just written (David, 2026-09-16: "The agent is getting
    /// stymied again"). Any edit to the setup is a new attempt: the
    /// failure is forgotten and the strip offers the rebuild ("Replace
    /// this banner with the one suggesting rebuild as soon as new changes
    /// occur").
    build_failed: Mutex<Option<(String, String)>>,
    /// The lifecycle command that failed on the last start, when one did,
    /// with its exit. The container stays up and the environment runs —
    /// the command is the project's, the environment it ran in is real,
    /// and a failed `composer install` is fixed FROM that environment, not
    /// from outside it. Failing the environment instead left a usable
    /// container up with no exec target, and the agent that could have
    /// fixed the command outside any container, blind (David, 2026-09-16:
    /// "So friggin tired of these read/write errors").
    hook_failure: Mutex<Option<String>>,
    pending: AtomicBool,
    logs: Mutex<VecDeque<String>>,
    /// What the container itself wrote (`podman logs`), ring-buffered like
    /// the build log, and followed for as long as the container runs.
    container_logs: Arc<Mutex<VecDeque<String>>>,
    /// The `podman logs --follow` task, alive exactly while the state is
    /// `Running`. Aborting it drops the child, which is killed with it.
    log_follower: Mutex<Option<tokio::task::AbortHandle>>,
    /// Where to ask for this environment's `.devcontainer/` watch to be
    /// re-armed: the FLEET's one watcher, not this environment's.
    ///
    /// A watcher per supervisor was one `inotify_init` per environment on a
    /// per-uid budget of 128 that the user's whole desktop session spends
    /// from too (`crate::configwatch`). Set by `ConfigWatch::add`, so the
    /// handle and the registration can never disagree; `None` in a
    /// supervisor nobody has asked to watch, which is the honest state for
    /// one built straight from a constructor in a test.
    config_watch: Mutex<Option<std::sync::Weak<crate::configwatch::ConfigWatch>>>,
    /// Serializes reload/stop/nuke: concurrent lifecycle operations (banner
    /// click + agent MCP reload) would interleave podman commands.
    lifecycle: tokio::sync::Mutex<()>,
    /// Which podman service this environment's container lives on: the
    /// host's, a machine's, or a remote one's. Every podman invocation in
    /// here goes through it, which is what makes "the environment moved
    /// into a VM" a change of one field rather than of forty call sites.
    ///
    /// Swappable for the same reason the exec target is: the registry
    /// resolves the real substrate on the runtime after the window is
    /// already up, and a supervisor that had captured the startup default
    /// would go on talking to the host forever.
    substrate: Mutex<Arc<crate::substrate::Substrate>>,
    /// True when the IDE itself runs inside a container (self-hosting
    /// bootstrap): the environment is already up, and lifecycle operations
    /// on it must happen from the host IDE instead. No container runtime is
    /// forwarded in — that would put host container creation (arbitrary
    /// mounts, i.e. host root) within reach of the agent and of the repo's
    /// own build.
    inside: bool,
    /// What this environment last measured as, against the workspace's disk
    /// budget. Written by whatever walked the tree — the registry's
    /// background cadence, or the environments view's Refresh — and read by
    /// the gates, which must never walk anything themselves. `None` until
    /// the first walk lands, which is a state the readers have to handle
    /// rather than round down to zero.
    disk: Mutex<Option<DiskSample>>,
}

fn exists_containerenv() -> bool {
    std::path::Path::new("/run/.containerenv").exists()
        || std::path::Path::new("/.dockerenv").exists()
}

/// The generation of the IDE's user-namespace mapping a container was
/// started with, as a label — hashed with the mounts; see `ide_mounts`.
const LABEL_USERNS_GENERATION: &str = "taste.userns-generation";

/// The podman flags the environment's checkout is bound with.
///
/// `Z` in both modes (a private SELinux label, so one container's relabel
/// does not hand the directory to another), plus `ro` under the baseline.
///
/// **This is the physical half of the safe-mode write wall, and it is not
/// the enforcing half.** `taste_core::policy::write_allowed` remains the
/// single source of truth for writes that go THROUGH the IDE, exactly as
/// before; the read-only bind is what stops the *other* route — a shell in
/// the baseline container, which safe mode now has — from editing project
/// source that the mediated path would have refused. The two agree because
/// the mount is strictly the more restrictive of the pair: nothing is
/// writable in here that `write_allowed` would have permitted. The one
/// thing `write_allowed` permits in safe mode that a read-only bind would
/// refuse — the devcontainer config itself — is bound writable over it
/// (`ide_mounts`), because the pinned adapter writes files natively rather
/// than through the IDE, and "EROFS: read-only file system, mkdir
/// .devcontainer" was the agent's whole experience of a project with no
/// config (2026-09-16).
fn workspace_bind_flags(authority: ConfigAuthority) -> &'static str {
    match authority {
        ConfigAuthority::Project => "Z",
        ConfigAuthority::Baseline => "ro,Z",
    }
}

impl Supervisor {
    pub fn new(
        env: EnvironmentIdentity,
        events: EventBus,
        exec: ExecContext,
        substrate: Arc<crate::substrate::Substrate>,
    ) -> Arc<Self> {
        Self::with_inside(env, events, exec, substrate, exists_containerenv())
    }

    /// Test seam: the test suite itself runs in a container, which must not
    /// flip every unit test into self-hosting semantics.
    #[doc(hidden)]
    pub fn new_outside_container_for_tests(
        env: EnvironmentIdentity,
        events: EventBus,
        exec: ExecContext,
        substrate: Arc<crate::substrate::Substrate>,
    ) -> Arc<Self> {
        Self::with_inside(env, events, exec, substrate, false)
    }

    fn with_inside(
        env: EnvironmentIdentity,
        events: EventBus,
        exec: ExecContext,
        substrate: Arc<crate::substrate::Substrate>,
        inside: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            env,
            events,
            exec,
            state: Mutex::new(SupervisorState::NoConfig),
            authority: Mutex::new(ConfigAuthority::Project),
            hosting: Mutex::new(AgentHosting::Unknown),
            agent_reload: AtomicBool::new(false),
            channel: tokio::sync::Mutex::new(None),
            channel_services: Mutex::new(None),
            running_hash: Mutex::new(None),
            declared_ports: Mutex::new(Vec::new()),
            published_ports: Mutex::new(std::collections::HashMap::new()),
            passed_over: Mutex::new(None),
            build_failed: Mutex::new(None),
            hook_failure: Mutex::new(None),
            pending: AtomicBool::new(false),
            logs: Mutex::new(VecDeque::new()),
            container_logs: Arc::new(Mutex::new(VecDeque::new())),
            log_follower: Mutex::new(None),
            config_watch: Mutex::new(None),
            lifecycle: tokio::sync::Mutex::new(()),
            substrate: Mutex::new(substrate),
            inside,
            disk: Mutex::new(None),
        })
    }

    /// Which environment this supervisor is.
    pub fn id(&self) -> &EnvironmentId {
        &self.env.id
    }

    /// This environment's working copy — the main checkout for the primary,
    /// its clone otherwise, on this host or in a VM. Container arguments
    /// and the agent's cwd compose from its path; nothing on this host
    /// opens it without asking [`Checkout::local_path`] first.
    pub fn checkout(&self) -> &Checkout {
        &self.env.checkout
    }

    /// The repository on this host holding this environment's refs. What
    /// review, publish, and history read. The same directory as the
    /// checkout when that is local.
    pub fn peer(&self) -> &Path {
        &self.env.peer
    }

    /// Where `.devcontainer/` is read from on this host: the checkout
    /// itself when it is here, and a host-side mirror of the config
    /// directory when the checkout is in a VM. The supervisor's every
    /// config read goes through this one path, so discovery, hashing and
    /// staging never learn where the files really are.
    pub fn config_root(&self) -> PathBuf {
        match &self.env.checkout {
            Checkout::Local(path) => path.clone(),
            Checkout::Remote { .. } => {
                environment::env_dir(&self.env.workspace_root, &self.env.id).join("config")
            }
        }
    }

    /// Make a directory inside the checkout, wherever the checkout is. On
    /// this host that is `create_dir_all`; in a VM it is the files
    /// service's job, and until that service exists the refusal says so
    /// rather than creating a directory on the wrong machine.
    fn make_dir_in_checkout(&self, dir: &Path) -> std::io::Result<()> {
        match &self.env.checkout {
            Checkout::Local(_) => std::fs::create_dir_all(dir),
            Checkout::Remote { vm, .. } => Err(std::io::Error::other(format!(
                "{} is in VM {vm}; directories there are made by the files service",
                dir.display()
            ))),
        }
    }

    /// Walk the checkout for its footprint — on this host. A checkout in a
    /// VM is not walked from here, and comes back as an empty walk, which
    /// the fleet reports as unmeasured rather than as zero.
    async fn walk(&self, prune_ignored: bool) -> CheckoutWalk {
        let Some(checkout) = self.env.checkout.local_path().map(Path::to_path_buf) else {
            return CheckoutWalk::default();
        };
        tokio::task::spawn_blocking(move || walk_checkout(&checkout, prune_ignored))
            .await
            .unwrap_or_default()
    }

    /// Snapshot the working copy onto this environment's snapshot ref
    /// (`taste_git::snapshot`), wherever the working copy is. `None` when
    /// the checkout is not a repository. Blocking — the object database is
    /// written — so it is called off the GTK thread.
    ///
    /// Here rather than in the chat pane because the pane knows when to
    /// snapshot and the supervisor knows where the files are; a checkout in
    /// a VM is snapshotted over there, and the pane must not have to know.
    pub fn snapshot_blocking(&self) -> Result<Option<taste_git::Snapshot>> {
        let name = taste_git::snapshot_ref(self.env.id.as_str());
        match &self.env.checkout {
            Checkout::Local(root) => {
                let Some(git) = taste_git::GitWorkspace::discover(root) else {
                    return Ok(None);
                };
                git.snapshot_worktree(&name).map(Some)
            }
            Checkout::Remote { vm, .. } => bail!(
                "{}'s working copy is in VM {vm}; snapshotting it there lands with the files \
                 service",
                self.env.id
            ),
        }
    }

    /// The workspace this environment belongs to.
    pub fn workspace_root(&self) -> &Path {
        &self.env.workspace_root
    }

    /// This environment's execution target. One per environment; the
    /// workspace holds a handle to the primary's for the call sites that
    /// predate environments.
    pub fn exec(&self) -> &ExecContext {
        &self.exec
    }

    pub fn state(&self) -> SupervisorState {
        self.state.lock().unwrap().clone()
    }

    /// Test seam: say what this environment is doing, with no podman
    /// involved.
    ///
    /// The states the gates read — [`SupervisorState::holds_a_container`]
    /// above all, which is what the orchestration cap counts — are otherwise
    /// reachable only by building a container for real, which a unit test
    /// cannot do and should not want to. Nothing outside a test calls this:
    /// the state is the supervisor's own account of what it has done, and
    /// anything else setting it would be a second account.
    #[doc(hidden)]
    pub fn set_state_for_tests(&self, state: SupervisorState) {
        self.set_state(state);
    }

    /// Whose config the running container was built from.
    ///
    /// This is the environment fact that distinguishes the two modes now
    /// that both are containers, and it is what the fleet row, the strip and
    /// the traffic light read to say "safe mode (baseline)" honestly — a
    /// container that is green-healthy inside and still wants the user's
    /// attention, because the project's own config is missing or broken.
    pub fn config_authority(&self) -> ConfigAuthority {
        *self.authority.lock().unwrap()
    }

    /// Resolve which configuration this environment should run — the top two
    /// rungs of the ladder.
    ///
    /// The project's own config wins whenever it is present, parseable,
    /// valid and confined. Anything else — absent, malformed, incomplete, or
    /// refused by the security validator — falls to the IDE's baseline,
    /// which is what makes every environment usable rather than only the
    /// ones whose repo already got its devcontainer right.
    ///
    /// The security validator runs *here*, before the baseline is chosen, so
    /// a repo config that reaches outside the workspace is refused exactly
    /// as it was before: it does not get to replace the baseline, and the
    /// reason lands in the log where the repair loop can read it.
    fn resolve_config(&self) -> Result<ResolvedConfig> {
        let resolved = self.resolve_config_uncached();
        if let Ok(resolved) = &resolved {
            *self.declared_ports.lock().unwrap() = resolved.config.ports();
            self.note_passed_over(resolved.reason.clone());
        }
        resolved
    }

    /// Remember why the project's config was passed over, and say so once
    /// per distinct reason: in the log, and as a toast, since the row's
    /// state does not otherwise move — the baseline was running and the
    /// baseline still is.
    fn note_passed_over(&self, reason: Option<String>) {
        let changed = {
            let mut slot = self.passed_over.lock().unwrap();
            let changed = *slot != reason;
            *slot = reason.clone();
            changed
        };
        if !changed {
            return;
        }
        match reason {
            Some(reason) => {
                self.log(format!(
                    "the project's configuration was passed over: {reason}"
                ));
                self.events.publish(Event::Toast(format!(
                    "{}: devcontainer.json was passed over, so safe mode stays — {reason}",
                    self.env.id
                )));
            }
            None => self.log("the project's configuration is in force".to_string()),
        }
    }

    /// Why the project's config is being passed over right now, if a
    /// config exists and is — `None` for a config in force or no config.
    pub fn config_passed_over(&self) -> Option<String> {
        self.passed_over.lock().unwrap().clone()
    }

    /// This environment's situation as an agent needs to hear it: the
    /// facts that decide its next call, in words designed for the smallest
    /// model that will read them (CLAUDE.md → House rules). The MCP
    /// `environment` tool carries it, and the chat puts it ahead of a
    /// prompt whenever the environment has changed under the agent, so the
    /// two never disagree about what is writable or what to do.
    pub fn situation(&self) -> Situation {
        let state = self.state();
        let exec = self.exec();
        let container = exec.is_container();
        let (mode, authority) = if container {
            ("container", "project")
        } else if exec.has_exec_target() {
            ("safe", "baseline")
        } else {
            ("safe", "none")
        };
        let writable = if container {
            "the whole checkout".to_string()
        } else {
            "only .devcontainer/ and the workspace dotfiles (.editorconfig, .gitignore, \
             .gitattributes); the rest of the checkout is read-only until the project's \
             environment builds"
                .to_string()
        };
        let passed_over = self.config_passed_over();
        let failure = match &state {
            SupervisorState::Failed { message } => Some(format!("the build failed: {message}")),
            _ => passed_over
                .as_ref()
                .map(|reason| {
                    format!("the project's devcontainer config was passed over: {reason}")
                })
                .or_else(|| {
                    self.hook_failure()
                        .map(|message| format!("a lifecycle command failed: {message}"))
                }),
        };
        // The CONFIG decides, not the folder: the IDE makes `.devcontainer/`
        // itself as the bind source the agent writes into, so an empty one
        // is the ordinary state of a project with no config yet.
        let has_config = !matches!(state, SupervisorState::NoConfig)
            && !matches!(DevcontainerConfig::discover(&self.config_root()), Ok(None));
        let next = match &state {
            SupervisorState::Building | SupervisorState::Starting => {
                "The environment is coming up. Wait a few seconds and call environment again."
                    .to_string()
            }
            _ if container && failure.is_some() => {
                "The container is up and the checkout is writable. Fix the failed command \
                 under .devcontainer/, then call devcontainer_reload."
                    .to_string()
            }
            _ if container => {
                "The checkout is writable and ide_exec runs in the container. Work normally."
                    .to_string()
            }
            _ if !has_config && self.uncommitted_main_config().is_some() => {
                "The user's checkout has a devcontainer config that is not committed, so \
                 this clone has none. Ask the user to commit .devcontainer/ in their \
                 checkout; then update_from_main, rebase onto their branch, and call \
                 devcontainer_reload."
                    .to_string()
            }
            _ if !exec.has_exec_target() => {
                "No container is running, so ide_exec has nowhere to run. Call environment \
                 with include [\"log\"], fix .devcontainer/ if the log names a cause, then \
                 call devcontainer_reload."
                    .to_string()
            }
            _ if !has_config => "This checkout has no devcontainer config. Write \
                 .devcontainer/devcontainer.json (and its Containerfile, if it builds one), \
                 then call devcontainer_reload. ide_conventions names the exact paths."
                .to_string(),
            _ => "Read the failure above, fix it under .devcontainer/ (that directory is \
                 writable), then call devcontainer_reload. Call environment with include \
                 [\"log\"] for the build output."
                .to_string(),
        };
        Situation {
            mode,
            authority,
            writable,
            failure,
            next,
        }
    }

    /// Whether the config is passed over because its image would not build
    /// or pull — as against being refused or unreadable. The banner's two
    /// sentences, and the two prompts an agent gets.
    pub fn build_failed(&self) -> bool {
        self.build_failed.lock().unwrap().is_some()
    }

    /// The lifecycle command that failed on the last start, if one did:
    /// the container is up regardless, and this is what the row and
    /// `devcontainer_status` say about it.
    pub fn hook_failure(&self) -> Option<String> {
        self.hook_failure.lock().unwrap().clone()
    }

    /// The forwarded ports of the config this environment last resolved,
    /// with their attributes. Empty until the first resolution, and empty
    /// for a baseline (the IDE's own config forwards nothing).
    pub fn ports(&self) -> Vec<crate::config::PortSpec> {
        let published = self.published_ports.lock().unwrap();
        self.declared_ports
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .map(|mut spec| {
                if let Some(host) = published.get(&spec.port) {
                    spec.host = *host;
                }
                spec
            })
            .collect()
    }

    /// The localhost port each of the config's forwarded ports will be
    /// published on: its own number when that is free, a free one when it
    /// is not. Two environments of one project forward the same numbers,
    /// and the second `podman run` failed with "Couldn't listen on
    /// requested ports" (David, 2026-09-16: "Got this error"); a server of
    /// the user's own on that port did the same. Remembered on the
    /// supervisor for `ports()`, and on the container (`LABEL_PORTS`) for
    /// adoption. Said in the log when a port moves, because the address
    /// the config names is then not the address that answers.
    ///
    /// A moved port lands on the nearest free number above its own, never
    /// on one the kernel picks: 8000 taken reads as 8001, which a person
    /// recognises as the same service, where 34603 reads as nothing
    /// (David, 2026-09-16: "choose the closest non-privileged port number
    /// greater than the collision. It will help people recognize the
    /// services"). Numbers this config forwards itself, and numbers this
    /// pass has already handed out, are skipped, so one config's 8000 and
    /// 8001 cannot land on each other.
    fn publish_ports(&self, config: &DevcontainerConfig) -> Vec<(u16, u16)> {
        let specs = config.ports();
        let declared: std::collections::HashSet<u16> = specs.iter().map(|s| s.port).collect();
        let mut assigned: std::collections::HashSet<u16> = std::collections::HashSet::new();
        let pairs: Vec<(u16, u16)> = specs
            .into_iter()
            .map(|spec| {
                let host = if port_is_free(spec.port) {
                    spec.port
                } else {
                    next_free_port_above(spec.port, |candidate| {
                        !declared.contains(&candidate) && !assigned.contains(&candidate)
                    })
                    .unwrap_or(spec.port)
                };
                assigned.insert(host);
                if host != spec.port {
                    self.log(format!(
                        "port {} is in use on this machine (another environment's \
                         container, or a server of the user's), so it is published on \
                         localhost:{host} instead",
                        spec.port
                    ));
                }
                (spec.port, host)
            })
            .collect();
        *self.published_ports.lock().unwrap() = pairs.iter().copied().collect();
        pairs
    }

    fn resolve_config_uncached(&self) -> Result<ResolvedConfig> {
        let baseline = |reason: Option<String>| -> Result<ResolvedConfig> {
            Ok(ResolvedConfig {
                config: crate::baseline::ensure_baseline_config()?,
                authority: ConfigAuthority::Baseline,
                reason,
            })
        };

        let config = match DevcontainerConfig::discover(&self.config_root()) {
            Ok(Some(config)) => config,
            // No config at all is the commonest reason to be here, and it is
            // not an error any more: a repo with no devcontainer gets the
            // baseline immediately.
            // Having no devcontainer is not a fault to report — except in
            // a clone whose main checkout HAS one: a clone is made from a
            // commit, so a config the user wrote and never committed is
            // exactly what it lacks, and "safe mode" with no reason left
            // that to be guessed (David, 2026-09-16: "I rebuilt the
            // container, and it seemed to launch okay. Why is this env in
            // safe mode?").
            Ok(None) => return baseline(self.uncommitted_main_config()),
            Err(e) => {
                return baseline(Some(format!("the project config could not be read: {e:#}")))
            }
        };
        if let Err(e) = config.validate() {
            return baseline(Some(format!("the project config is not usable: {e:#}")));
        }
        if let Err(e) = crate::security::validate_security(&config, self.env.checkout.path()) {
            return baseline(Some(format!("the project config was refused: {e:#}")));
        }
        // A setup that would not build or pull last time, unchanged since:
        // the baseline stands in until any of its files move.
        let failed = self.build_failed.lock().unwrap().clone();
        if let Some((hash, reason)) = failed {
            if self.setup_hash(&config).as_deref() == Some(hash.as_str()) {
                return baseline(Some(format!("it could not be built or started: {reason}")));
            }
            self.build_failed.lock().unwrap().take();
        }
        Ok(ResolvedConfig {
            config,
            authority: ConfigAuthority::Project,
            reason: None,
        })
    }

    /// The reason a clone has no config while the user's checkout has one
    /// on disk: the config is not committed, and clones carry commits.
    /// `None` for the primary, and for a clone whose main checkout has no
    /// config either.
    fn uncommitted_main_config(&self) -> Option<String> {
        if self.env.is_main_checkout() {
            return None;
        }
        let main_has_config = !matches!(
            DevcontainerConfig::discover(&self.env.workspace_root),
            Ok(None)
        );
        main_has_config.then(|| {
            "the user's checkout has a devcontainer config that is not committed; this \
             environment is a clone of a commit, so it has none"
                .to_string()
        })
    }

    /// The project's image failed to build or pull: remember which, so
    /// resolution passes the config over until it changes.
    fn remember_build_failure(&self, config: &DevcontainerConfig, error: &anyhow::Error) {
        let hash = self.setup_hash(config).unwrap_or_default();
        let reason = error.to_string();
        *self.build_failed.lock().unwrap() = Some((hash, reason));
    }

    /// The whole setup as one hash — every file the config reads, plus
    /// the IDE's own mounts — so that any edit reads as a change.
    fn setup_hash(&self, config: &DevcontainerConfig) -> Option<String> {
        config_hash(config, &self.ide_mounts(config, ConfigAuthority::Project)).ok()
    }

    pub fn pending_changes(&self) -> bool {
        self.pending.load(Ordering::SeqCst)
    }

    /// Whose config a reload would build from right now, and why the
    /// project's was passed over when it was — the answer `reload` logs,
    /// available before the reload so the tool that starts one can say it
    /// to the agent. A baseline the IDE itself cannot write is reported as
    /// a baseline with that as the reason; it is not this reader's job to
    /// fail.
    pub fn resolve_authority(&self) -> (ConfigAuthority, Option<String>) {
        match self.resolve_config_uncached() {
            Ok(resolved) => (resolved.authority, resolved.reason),
            Err(e) => (ConfigAuthority::Baseline, Some(format!("{e:#}"))),
        }
    }

    /// Test seam: force the pending-changes flag, so the confirmation gate
    /// keyed on it can be exercised without a running container to drift
    /// against.
    #[doc(hidden)]
    pub fn set_pending_for_tests(&self, pending: bool) {
        self.set_pending(pending);
    }

    /// Hash of the config the running container was built from, if running.
    pub fn running_hash(&self) -> Option<String> {
        self.running_hash.lock().unwrap().clone()
    }

    /// Whether this environment's container can host a relocated agent.
    ///
    /// [`AgentHosting::Unknown`] until [`Self::probe_agent_hosting`] has
    /// answered, and a chat reading `Unknown` must keep the
    /// outside-confined topology: guessing yes and being wrong is an agent
    /// that will not start.
    pub fn agent_hosting(&self) -> AgentHosting {
        self.hosting.lock().unwrap().clone()
    }

    /// Ask the container whether it can host an agent, and remember the
    /// answer for as long as that container lives.
    ///
    /// Three questions, and a perfectly good devcontainer may answer no to
    /// any of them:
    ///
    /// - **`node`.** Every ACP adapter here is a node program, and so is
    ///   the MCP stdio bridge that gives it the IDE's tools. A container
    ///   without node cannot run either. (This is why "a devcontainer that
    ///   wants an in-container agent carries node" is a convention in
    ///   ENVIRONMENTS.md rather than something the IDE installs — the IDE
    ///   does not modify the repo's image.)
    /// - **A writable agent home.** The per-environment home volume mounts
    ///   at [`taste_core::policy::AGENT_HOME_IN_DEVCONTAINER`], and podman
    ///   creates a fresh named volume owned by the container's root when
    ///   the image has nothing at that path. The agent's history lives in
    ///   there, so an unwritable home is a chat that silently forgets. One
    ///   `chown` as container-root fixes it — under rootless podman that is
    ///   the user's own uid seen through the userns, so it grants nothing
    ///   on the host — and if even that fails, the answer is no.
    /// - **The IDE's sockets, actually reachable.** Mounting them is not
    ///   the same as reaching them, and the difference is not theoretical:
    ///   the endpoints are inside the container now (see [`crate::channel`]
    ///   for why the direction had to invert), but "the helper bound them"
    ///   is not "a client gets an answer through them". So the container is
    ///   asked to dial each one and get a real reply out of the IDE — an
    ///   MCP response, an HTTP response from the proxy. Anything less would
    ///   be a probe of a proxy for the mechanism rather than of the
    ///   mechanism, and the failure it would let through is the worst kind:
    ///   a relocated agent that comes up fine with no IDE tools and no way
    ///   to pay for a turn.
    ///
    /// Deliberately not fatal to anything: a `No` costs relocation and
    /// nothing else.
    pub async fn probe_agent_hosting(&self) -> AgentHosting {
        let SupervisorState::Running { container_id } = self.state() else {
            return AgentHosting::Unknown;
        };
        let hosting = self.probe_container().await;
        // Republish Running so a chat that already connected — because the
        // answer was still Unknown when it did — gets the one event it
        // relocates on. Same channel, no second mechanism, and idempotent
        // for every other subscriber.
        if hosting != AgentHosting::Unknown
            && matches!(self.state(), SupervisorState::Running { .. })
        {
            self.set_state(SupervisorState::Running { container_id });
        }
        hosting
    }

    /// [`Self::probe_agent_hosting`] without the state check or the
    /// republish, for `start` — which knows the container is up because it
    /// just started it, and has not announced it yet. Probing there is what
    /// keeps the common case to one spawn: by the time a chat sees
    /// `Running`, the answer is already in.
    async fn probe_container(&self) -> AgentHosting {
        if self.inside {
            return AgentHosting::Unknown;
        }
        let name = self.container_name();
        let sh = |script: String| {
            vec![
                "exec".into(),
                name.clone(),
                "sh".into(),
                "-c".into(),
                script,
            ]
        };
        let home = taste_core::policy::AGENT_HOME_IN_DEVCONTAINER;
        let writable = format!("mkdir -p {home} 2>/dev/null; test -w {home}");

        let hosting = if self
            .run_captured(sh("command -v node".into()))
            .await
            .is_err()
        {
            AgentHosting::No {
                reason: format!(
                    "{name} has no node: an ACP adapter and the IDE's MCP bridge are both \
                     node programs, so this environment's agent runs outside the container"
                ),
            }
        } else {
            if self.run_captured(sh(writable.clone())).await.is_err() {
                // Best-effort repair before the verdict. What this catches
                // is podman handing a brand-new named volume to root, not
                // anything the repo did — so chown it to whoever `podman
                // exec` actually runs as, asked rather than assumed.
                if let Ok(owner) = self.run_captured(sh("id -u; id -g".into())).await {
                    let owner: Vec<&str> = owner.split_whitespace().collect();
                    if let [uid, gid] = owner[..] {
                        let _ = self
                            .run_captured(vec![
                                "exec".into(),
                                "--user".into(),
                                "root".into(),
                                name.clone(),
                                "sh".into(),
                                "-c".into(),
                                format!("mkdir -p {home} && chown -R {uid}:{gid} {home}"),
                            ])
                            .await;
                    }
                }
            }
            match self.run_captured(sh(writable)).await {
                Err(e) => AgentHosting::No {
                    reason: format!(
                        "{name} cannot write the agent home at {home} ({e}); this \
                         environment's agent runs outside the container"
                    ),
                },
                Ok(_) => match self.probe_channel(&name).await {
                    Ok(()) => AgentHosting::Yes,
                    Err(e) => AgentHosting::No {
                        reason: format!(
                            "{name} cannot reach the IDE through its environment \
                             channel ({e}); this environment's agent runs outside \
                             the container, where it can."
                        ),
                    },
                },
            }
        };
        if let AgentHosting::No { reason } = &hosting {
            self.log(reason.clone());
        }
        // Under the project's config the checkout is bound read-write, and
        // the container's user must be able to write it, or every agent
        // edit fails as "read-only" against a read-write mount. Said
        // plainly when it is not so — the cause is a uid the mapping did
        // not cover, and the rebuild is the fix.
        if self.config_authority() == ConfigAuthority::Project {
            let root = self.env.checkout.path().display().to_string();
            if self
                .run_captured(sh(format!("test -w '{root}'")))
                .await
                .is_err()
            {
                let who = self
                    .run_captured(sh("id -u".into()))
                    .await
                    .unwrap_or_else(|_| "?".into());
                let line = format!(
                    "the checkout at {root} is not writable by the container's user (uid \
                     {who}): the host's files belong to another uid inside. Rebuild to map \
                     your uid onto the container's user."
                );
                self.log(line.clone());
                self.events
                    .publish(Event::Toast(format!("{}: {line}", self.env.id)));
            }
        }
        *self.hosting.lock().unwrap() = hosting.clone();
        hosting
    }

    /// What the IDE serves down this environment's channel.
    ///
    /// Set once, by the window, for every supervisor the registry owns. Not
    /// a constructor argument because the MCP server is built *from* the
    /// registry — the cycle is real, and a setter is the honest way to
    /// close it.
    pub fn set_channel_services(&self, services: Arc<dyn ChannelServices>) {
        *self.channel_services.lock().unwrap() = Some(services);
    }

    /// This environment's live channel, started if it is not up (or not up
    /// any more — a helper dies with its container, and with a `podman
    /// restart` that leaves the container's name pointing somewhere new).
    ///
    /// One helper per environment, shared by everything in the container:
    /// the exec costs ~190 ms, which is a price to pay once per container
    /// and never per connection.
    pub async fn ensure_channel(&self) -> Result<Arc<EnvChannel>> {
        let SupervisorState::Running { .. } = self.state() else {
            bail!("environment {} has no container running", self.env.id);
        };
        self.open_channel().await
    }

    /// [`Self::ensure_channel`] without the state check, for the one caller
    /// that has not announced the container yet.
    ///
    /// The check in `ensure_channel` asks the *published* state, and
    /// [`Self::start`] deliberately probes before it publishes — so routing
    /// the start-time probe through the public door made the supervisor
    /// refuse its own container for not being announced. That refusal was
    /// not a probe failing: it was latched as `AgentHosting::No` for the
    /// life of the container, and the reason it carried — "cannot reach the
    /// IDE through its environment channel (environment X has no container
    /// running)" — was then shown in the chat, about a container that had
    /// just come up. A private door with no state check keeps "is the
    /// container announced" from being asked where the answer is knowably
    /// stale.
    async fn open_channel(&self) -> Result<Arc<EnvChannel>> {
        let services = self
            .channel_services
            .lock()
            .unwrap()
            .clone()
            .context("the IDE has not wired its services to environment channels")?;
        let mut slot = self.channel.lock().await;
        if let Some(channel) = slot.as_ref() {
            if channel.alive() {
                return Ok(channel.clone());
            }
        }
        let channel = EnvChannel::start(
            self.env.id.clone(),
            &self.container_name(),
            &self.substrate(),
            services,
        )
        .await?;
        *slot = Some(channel.clone());
        Ok(channel)
    }

    /// The in-container endpoints a relocated spawn points at, if this
    /// environment's channel is up. `None` is a chat that stays
    /// outside-confined — an agent told to dial a socket nothing is serving
    /// would come up with no tools.
    pub fn channel_paths(&self) -> Option<ChannelPaths> {
        let channel = self.channel.try_lock().ok()?;
        let channel = channel.as_ref()?;
        channel.alive().then(|| channel.paths().clone())
    }

    /// Can something in this container get a real answer out of the IDE
    /// through the channel? The question itself is
    /// [`crate::channel::REACH_PROBE`]; this opens the channel and asks it.
    ///
    /// Only services the IDE actually offers are probed: with
    /// `TASTE_AUTH_PROXY=0` there is no proxy to answer, and failing an
    /// environment for a door the IDE never opened would be a lie.
    async fn probe_channel(&self, name: &str) -> Result<()> {
        // `open_channel`, not `ensure_channel`: this runs from `start`,
        // before `Running` is published, and the state check would refuse
        // the very container being probed.
        let channel = self.open_channel().await?;
        let serves_auth = self
            .channel_services
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|services| services.serves(crate::channel::Service::Auth));
        let mut args = vec![
            "exec".into(),
            name.to_string(),
            "node".into(),
            "-e".into(),
            crate::channel::REACH_PROBE.into(),
            channel.paths().mcp.display().to_string(),
        ];
        if serves_auth {
            args.push(channel.paths().auth.display().to_string());
        }
        self.run_captured(args).await.map(|_| ())
    }

    fn forget_agent_hosting(&self) {
        *self.hosting.lock().unwrap() = AgentHosting::Unknown;
        // The helper lives in the container; a container that is gone (or
        // about to be) is not one to keep an address for. Dropping it kills
        // the exec and every connection riding it, which is what a relocated
        // agent's death already looks like from the IDE side.
        if let Ok(mut channel) = self.channel.try_lock() {
            *channel = None;
        }
    }

    /// Last `n` lines of build/startup output (for the MCP `devcontainer_logs`
    /// tool and the supervisor console tab's backfill).
    pub fn logs_tail(&self, n: usize) -> Vec<String> {
        let logs = self.logs.lock().unwrap();
        logs.iter().rev().take(n).rev().cloned().collect()
    }

    fn set_state(&self, state: SupervisorState) {
        *self.state.lock().unwrap() = state.clone();
        self.events.publish(Event::DevcontainerState {
            env: self.env.id.clone(),
            state: state.to_event(),
        });
        self.sync_log_follower(&state);
    }

    /// Last `n` lines the container itself wrote — its main process's
    /// stdout and stderr, as `podman logs` keeps them.
    pub fn container_logs_tail(&self, n: usize) -> Vec<String> {
        let logs = self.container_logs.lock().unwrap();
        logs.iter().rev().take(n).rev().cloned().collect()
    }

    /// Keep one `podman logs --follow` alive while the container runs, and
    /// none otherwise. The devcontainer spec has no notion of a log to
    /// discover; a container's main process's output is the one stream it
    /// formally has, so that is what is followed — for a systemd image it
    /// is the journal's console, for `sleep infinity` nothing at all, and
    /// either is the truth about the container.
    fn sync_log_follower(&self, state: &SupervisorState) {
        let running = matches!(state, SupervisorState::Running { .. });
        let mut slot = self.log_follower.lock().unwrap();
        if !running {
            if let Some(follower) = slot.take() {
                follower.abort();
            }
            return;
        }
        if slot
            .as_ref()
            .is_some_and(|follower| !follower.is_finished())
        {
            return;
        }
        // Only where there is a runtime to follow on: a state set from a
        // test's thread has nobody to read the stream for it.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let args: Vec<String> = vec![
            "logs".into(),
            "--follow".into(),
            "--tail".into(),
            "500".into(),
            self.container_name(),
        ];
        let mut command = self.podman(&args);
        let ring = self.container_logs.clone();
        let events = self.events.clone();
        let env = self.env.id.clone();
        let task = runtime.spawn(async move {
            let mut child = match command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
            {
                Ok(child) => child,
                Err(e) => {
                    tracing::debug!("{env}: podman logs --follow did not start: {e}");
                    return;
                }
            };
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let push = move |line: String| {
                {
                    let mut ring = ring.lock().unwrap();
                    if ring.len() >= LOG_RING_CAPACITY {
                        ring.pop_front();
                    }
                    ring.push_back(line.clone());
                }
                events.publish(Event::ContainerOutput {
                    env: env.clone(),
                    line,
                });
            };
            let push = Arc::new(push);
            let read = |stream: Option<tokio::process::ChildStdout>,
                        push: Arc<dyn Fn(String) + Send + Sync>| async move {
                let Some(stream) = stream else { return };
                let mut lines = tokio::io::BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    push(line);
                }
            };
            let read_err = |stream: Option<tokio::process::ChildStderr>,
                            push: Arc<dyn Fn(String) + Send + Sync>| async move {
                let Some(stream) = stream else { return };
                let mut lines = tokio::io::BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    push(line);
                }
            };
            let out: Arc<dyn Fn(String) + Send + Sync> = push.clone();
            let err: Arc<dyn Fn(String) + Send + Sync> = push;
            tokio::join!(read(stdout, out), read_err(stderr, err));
            let _ = child.wait().await;
        });
        *slot = Some(task.abort_handle());
    }

    fn set_pending(&self, pending: bool) {
        if self.pending.swap(pending, Ordering::SeqCst) != pending {
            self.events.publish(Event::DevcontainerPendingChanges {
                env: self.env.id.clone(),
                pending,
            });
        }
    }

    /// The log ring is per-supervisor, so it is per-environment for free —
    /// no de-interleaving, no shared buffer. The event carries the id so a
    /// subscriber showing one environment's build can drop the rest.
    fn log(&self, line: impl Into<String>) {
        let line = line.into();
        let mut logs = self.logs.lock().unwrap();
        if logs.len() >= LOG_RING_CAPACITY {
            logs.pop_front();
        }
        logs.push_back(line.clone());
        drop(logs);
        self.events.publish(Event::DevcontainerLog {
            env: self.env.id.clone(),
            line,
        });
    }

    /// Re-evaluate config presence and drift. Called at startup and by the
    /// file watcher on every relevant filesystem event.
    pub fn recheck(&self) -> Result<()> {
        // Self-hosting FALLBACK (no reachable runtime): we ARE the
        // devcontainer, running by definition; drift is managed from the
        // host IDE. With a forwarded podman socket this branch is skipped
        // and the devcontainer is supervised as a real sibling.
        if self.inside {
            if self.state()
                != (SupervisorState::Running {
                    container_id: "self".into(),
                })
            {
                self.set_state(SupervisorState::Running {
                    container_id: "self".into(),
                });
            }
            self.set_pending(false);
            return Ok(());
        }
        // Watch `.devcontainer/` whenever it exists, before anything is
        // parsed. Arming this on a *successful* parse was a real hole: a
        // malformed devcontainer.json made discovery fail, recheck returned
        // early, and the watcher was never armed — so the agent's edits
        // fixing that very file raised no event, and the environment sat
        // broken until something else happened to trigger a recheck. The
        // file the repair loop edits is the one it cannot afford to stop
        // watching.
        self.watch_devcontainer_dir();
        // "Is there a project config at all" — the question that separates
        // NoConfig from ConfigDetected. Whether it is *usable* is
        // `resolve_config`'s business, not this one's.
        let project = DevcontainerConfig::discover(&self.config_root())
            .ok()
            .flatten();
        let current = self.state();
        match &current {
            SupervisorState::Running { .. } => {
                // Drift is one question now: does the container that is
                // running match what the ladder would resolve today? That
                // covers the cases the old three-armed version handled and
                // the two the baseline introduces —
                //
                //   * a baseline container running beside a repo that has no
                //     config is the correct steady state, not drift (the old
                //     code flagged "config deleted" unconditionally); and
                //   * a project config that has just become healthy while
                //     the baseline runs IS drift, which is precisely how the
                //     repair loop finishes: the banner lights up, and
                //     `devcontainer_reload` asks the user to apply it.
                let resolved = self.resolve_config()?;
                let hash = config_hash(
                    &resolved.config,
                    &self.ide_mounts(&resolved.config, resolved.authority),
                )?;
                let drift = self.running_hash().as_deref() != Some(hash.as_str())
                    // A change of authority is drift even if the hashes
                    // somehow agreed: it is a change of mode.
                    || resolved.authority != self.config_authority();
                self.set_pending(drift);
            }
            SupervisorState::NoConfig => {
                // A previous IDE instance may have left a container running
                // — of either authority. Adopt it rather than sitting in
                // safe mode next to a healthy environment.
                if let Some(container_id) = self.adopt_running_container() {
                    self.set_state(SupervisorState::Running { container_id });
                } else if project.is_some() {
                    self.set_state(SupervisorState::ConfigDetected);
                    self.set_pending(false);
                } else {
                    // Stay in NoConfig — which is no longer a dead end. It
                    // is the state a workspace with no devcontainer starts
                    // in, and `reload` will bring the baseline up from here.
                    self.set_pending(false);
                }
            }
            // A config edit after a failure (or stop) is the fix loop in
            // action: return to ConfigDetected so Start reappears and MCP
            // reports progress instead of a stale failure.
            SupervisorState::Failed { .. } | SupervisorState::Stopped if project.is_some() => {
                self.set_state(SupervisorState::ConfigDetected);
                self.set_pending(false);
            }
            _ => {}
        }
        Ok(())
    }

    /// Idempotently watch `.devcontainer/` once it exists. notify tolerates
    /// re-watching the same path; errors are non-fatal.
    fn watch_devcontainer_dir(&self) {
        let watch = self.config_watch.lock().unwrap().clone();
        // Nobody is watching this environment, so there is nothing to
        // re-arm — the root watch is not on either. A supervisor reaches
        // that state exactly one way: built straight from a constructor,
        // which only the tests do.
        // inotify watches host directories. A checkout in a VM has its
        // config directory watched from over there, by the files service,
        // which re-arms the mirror this supervisor reads.
        if let (Some(watch), Some(root)) = (
            watch.as_ref().and_then(std::sync::Weak::upgrade),
            self.env.checkout.local_path(),
        ) {
            watch.arm_devcontainer_dir(root);
        }
    }

    /// Told where the fleet's config watcher is, by that watcher, as it
    /// takes this environment on ([`crate::configwatch::ConfigWatch::add`]).
    ///
    /// There is no `start_watching` here any more: an environment does not
    /// own an inotify instance, because instances are per-uid and scarce
    /// while descriptors are not.
    pub fn set_config_watch(&self, watch: std::sync::Weak<crate::configwatch::ConfigWatch>) {
        *self.config_watch.lock().unwrap() = Some(watch);
    }

    /// At startup: if this environment's container is already running,
    /// point execution into it and report honest drift from the config
    /// hash it was created with (stored as a container label).
    ///
    /// Reconciliation is by **label**, not by name lookup. Names are ours
    /// to compute and they will keep changing; the labels are the container's
    /// own claim about which workspace and environment it belongs to, so a
    /// container built by a build whose naming we no longer produce is still
    /// recognised — and a container that merely happens to sit at a name we
    /// would have chosen is not adopted.
    /// The authority comes off the container's own label rather than from
    /// the config now on disk, because those two legitimately disagree in
    /// the case that matters most: a baseline container still running beside
    /// a project config the agent has since repaired. Reading the config
    /// would adopt that container as though it were the project's, quietly
    /// unlocking the workspace to writes the read-only mount still refuses.
    fn adopt_running_container(&self) -> Option<String> {
        // Through the substrate, like everything else. This call site used
        // to build its own `podman`/`flatpak-spawn` command and re-detect
        // the sandbox for itself, which meant adoption was the one podman
        // invocation that would have kept looking at the host after the
        // rest of the IDE moved into a VM — and it would have adopted
        // nothing, silently, forever.
        let output = self
            .substrate()
            .std_command(&[])
            .args([
                "ps",
                "--filter",
                &format!("label={LABEL_WORKSPACE}={}", self.workspace_key()),
                "--filter",
                &format!("label={LABEL_ENV}={}", self.env.id),
                "--format",
                &format!(
                    r#"{{{{.Names}}}}|{{{{index .Labels "{LABEL_CONFIG_HASH}"}}}}|{{{{index .Labels "{LABEL_AUTHORITY}"}}}}|{{{{index .Labels "{LABEL_PORTS}"}}}}"#
                ),
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let line = text.lines().find(|l| !l.trim().is_empty())?;
        let mut fields = line.trim().split('|');
        let name = fields.next()?.to_string();
        let started_hash = fields.next()?.to_string();
        // Absent or `<no value>` reads as Project — the meaning every
        // container that ran before this label existed already had.
        let authority =
            ConfigAuthority::from_label(crate::reconcile::label(fields.next().unwrap_or_default()));
        // Where its ports actually landed, so the rows dial the right
        // address; a container from before the label reads as "each on
        // its own number", which is what it did.
        *self.published_ports.lock().unwrap() =
            parse_ports_label(crate::reconcile::label(fields.next().unwrap_or_default()))
                .into_iter()
                .collect();

        // Resolve against the same ladder a reload would take, so the drift
        // comparison is like-for-like.
        let resolved = self.resolve_config().ok()?;
        // The adopted container's own workspace folder is not recoverable
        // from a label, so the resolved config's is used. When the two rungs
        // disagree the drift flag below is already set, and the rebuild the
        // user is being asked for is what settles it.
        let workdir = resolved.config.workspace_folder().to_string();
        self.exec.set_container(name.clone(), workdir, authority);
        *self.authority.lock().unwrap() = authority;
        *self.running_hash.lock().unwrap() = Some(started_hash.clone());
        let drift = config_hash(
            &resolved.config,
            &self.ide_mounts(&resolved.config, resolved.authority),
        )
        .map(|hash| hash != started_hash)
        .unwrap_or(true)
            || authority != resolved.authority;
        self.set_pending(drift);
        self.log(format!(
            "adopted running container {name} ({} config)",
            authority.label()
        ));
        Some(name)
    }

    /// Does the container this environment believes it is running still
    /// exist on the substrate?
    ///
    /// **This is what a recreated machine looks like from up here.** A
    /// machine is cattle — the answer to one that is wrong is `rm` and
    /// `init`, not repair — and every container inside it dies with it. The
    /// supervisor's own state is host-side and survives, so without this it
    /// would go on reporting `Running` against a container id that names
    /// nothing, `ide_exec` would fail with podman's "no such container"
    /// instead of the IDE's "this environment is down", and a chat would
    /// keep trying to relocate into it.
    ///
    /// The same check covers the mundane cases it was always worth having:
    /// a container the user removed by hand, and a podman that was
    /// restarted underneath us.
    ///
    /// Returns whether the environment is still up. Only ever demotes —
    /// finding a container is not grounds to claim a state the lifecycle
    /// did not produce.
    pub async fn reconcile_container_presence(&self) -> bool {
        if self.inside {
            return true; // we are the container; it exists by construction
        }
        if !matches!(self.state(), SupervisorState::Running { .. }) {
            return false;
        }
        let name = self.container_name();
        let present = self
            .run_captured(vec![
                "ps".into(),
                "--filter".into(),
                format!("name=^{name}$"),
                "--format".into(),
                "{{.Names}}".into(),
            ])
            .await
            .map(|out| out.lines().any(|line| line.trim() == name))
            // A podman that cannot be asked is not a container that is
            // gone. Tearing an environment down because the substrate was
            // briefly unreachable would be worse than the stale state.
            .unwrap_or(true);
        if present {
            return true;
        }
        self.log(format!(
            "{name} is gone from {} — the environment is down; \
             reload to bring it back",
            self.substrate().provider().describe()
        ));
        *self.running_hash.lock().unwrap() = None;
        self.exec.set_host();
        self.forget_agent_hosting();
        self.set_state(SupervisorState::Stopped);
        self.set_pending(false);
        false
    }

    fn workspace_key(&self) -> String {
        environment::workspace_key(&self.env.workspace_root)
    }

    /// This environment's container name. Derived in one place for the
    /// whole IDE — see [`taste_core::environment`].
    pub fn container_name(&self) -> String {
        environment::env_container_name(&self.env.workspace_root, &self.env.id)
    }

    /// The labels every container and image of this environment carries.
    /// They are what reconciliation and cleanup enumerate by.
    fn resource_labels(&self) -> Vec<String> {
        vec![
            "--label".into(),
            format!("{LABEL_WORKSPACE}={}", self.workspace_key()),
            "--label".into(),
            format!("{LABEL_ENV}={}", self.env.id),
        ]
    }

    /// A repo-declared volume name, namespaced to this environment.
    fn namespaced_volume(&self, declared: &str) -> String {
        environment::env_config_volume(&self.env.workspace_root, &self.env.id, declared)
    }

    /// A repo-declared mount spec with its named volume namespaced.
    fn namespaced_mount(&self, mount: &str) -> String {
        crate::config::rewrite_volume_source(mount, |declared| self.namespaced_volume(declared))
    }

    /// The mounts the IDE adds on its own account, regardless of what the
    /// repo asked for. Separate from `start` so the same list can be
    /// HASHED — see `config_hash`. Change what is mounted and every running
    /// container goes stale by itself, which is the only version of this
    /// that survives someone forgetting.
    fn ide_mounts(&self, config: &DevcontainerConfig, authority: ConfigAuthority) -> Vec<String> {
        let workdir = config.workspace_folder().to_string();
        let mut mounts: Vec<String> = Vec::new();

        // The workspace a SECOND time, at its host path. That is what makes
        // every path an agent exchanges with the IDE mean the same thing on
        // both sides — no translation layer — and it keeps the agent
        // conversation history findable, since the adapter keys history by
        // working directory.
        //
        // Read-only under the baseline, like the first bind: an agent in
        // safe mode reads the repo natively and writes nothing but its
        // config, through the IDE. Both binds must carry the same flags or
        // the second is a way around the first.
        let host_path = self.env.checkout.path().display().to_string();
        if host_path != workdir {
            mounts.push("-v".into());
            mounts.push(format!(
                "{host_path}:{host_path}:{}",
                workspace_bind_flags(authority)
            ));
        }

        // The devcontainer config, writable over the read-only checkout
        // under the baseline — at both container paths, for the same
        // reason both checkout binds carry the same flags. Safe mode's
        // whole point is that the agent AUTHORS `.devcontainer/` and the
        // user applies it (`devcontainer_reload`), and `write_allowed`
        // has always said yes to this directory; but the pinned adapter's
        // Write and Edit are Claude Code's own, not the IDE's fs methods,
        // so the yes reached a mount that said no. The directory is made
        // on the host before the container starts (`start`), since a bind
        // needs a source; it is empty until the agent writes, and an empty
        // `.devcontainer/` is not a config (`DevcontainerConfig::discover`
        // reads files, not directories), so nothing else changes.
        if authority == ConfigAuthority::Baseline {
            let source = self
                .env
                .checkout
                .path()
                .join(".devcontainer")
                .display()
                .to_string();
            mounts.push("-v".into());
            mounts.push(format!("{source}:{workdir}/.devcontainer:Z"));
            if host_path != workdir {
                mounts.push("-v".into());
                mounts.push(format!("{source}:{host_path}/.devcontainer:Z"));
            }
        }

        // The agent own home. A volume so credentials and history outlive a
        // rebuild; not /home/dev, which is the USER home in here. Per
        // ENVIRONMENT, not per machine: the old single global volume would
        // have put every environment's agent in one home directory.
        mounts.push("-v".into());
        mounts.push(format!(
            "{}:{}",
            environment::env_home_volume(&self.env.workspace_root, &self.env.id),
            taste_core::policy::AGENT_HOME_IN_DEVCONTAINER
        ));

        // **No IDE socket is mounted here, deliberately.** The MCP socket
        // and the auth proxy's socket used to ride in at their host paths,
        // and on an SELinux-enforcing host that was theatre: the mount
        // succeeded, the file was readable, and `connect(2)` returned EACCES
        // because a `container_t` process may not `connectto` a socket the
        // unconfined IDE bound. The direction is inverted now — the IDE
        // execs a helper that binds those endpoints *inside* the container
        // (see `crate::channel`) — so the repo's own container is handed one
        // host path and one only: its checkout.
        //
        // Removing these two mounts changes the config hash, so every
        // running container is stale by itself the first time this ships,
        // which is exactly what should happen: their contents changed.

        // A generation mark for the user-namespace mapping (`userns_flag_for`),
        // hashed like the mounts so that a container started before the
        // mapping existed reads as stale and is offered its rebuild — an
        // adopted one kept running unmapped, with the checkout owned by
        // root inside and the agent unable to write a byte of it (David,
        // 2026-09-16: "Still an issue"). Bump it whenever what the IDE does
        // to a container's identity changes.
        mounts.push("--label".into());
        mounts.push(format!("{LABEL_USERNS_GENERATION}=1"));

        mounts
    }

    /// The image tag for a config — keyed by the BUILD hash, so every
    /// environment of this workspace whose config hashes the same shares
    /// one image instead of each holding its own copy.
    fn image_tag(&self, config: &DevcontainerConfig) -> Result<String> {
        Ok(environment::env_image_tag(&build_hash(config)?))
    }

    /// The image tag of the config currently on disk, if it builds one.
    /// `None` means "nothing for us to remove": no config, or a config that
    /// pulls a registry image rather than building.
    fn current_image_tag(&self) -> Option<String> {
        let config = DevcontainerConfig::discover(&self.config_root())
            .ok()
            .flatten()?;
        config.dockerfile_path()?;
        self.image_tag(&config).ok()
    }

    /// Which podman service this environment's containers live on.
    pub fn substrate(&self) -> Arc<crate::substrate::Substrate> {
        self.substrate.lock().unwrap().clone()
    }

    /// Point this environment at a substrate. The registry's job, and it
    /// does every environment at once — see
    /// [`crate::EnvironmentRegistry::set_substrate`].
    pub fn set_substrate(&self, substrate: Arc<crate::substrate::Substrate>) {
        *self.substrate.lock().unwrap() = substrate;
    }

    /// Podman never runs *in* the IDE's sandbox and — since the substrate
    /// work — not necessarily on the IDE's host either. Both facts live on
    /// the substrate; this is the only place either is consulted.
    fn podman(&self, args: &[String]) -> tokio::process::Command {
        crate::reconcile::podman(&self.substrate(), args)
    }

    /// Run a podman command, streaming its output into the log ring —
    /// and saying something when the command goes quiet, because a build
    /// that has stopped printing has not stopped working. After a RUN's
    /// last line podman commits the layer, and under rootless
    /// fuse-overlayfs a multi-gigabyte layer is minutes of silence before
    /// the layer id prints; read cold, that is a hang (David, 2026-09-06:
    /// "Improve the build output so this isn't surprising"). So the loop
    /// keeps a clock on the current step, and every [`QUIET_AFTER`] of
    /// nothing it logs what is going on and for how long.
    async fn run_logged(&self, args: Vec<String>) -> Result<()> {
        self.log(format!("$ podman {}", args.join(" ")));
        let mut child = self
            .podman(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .context("spawning podman")?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let mut out_lines = BufReader::new(stdout).lines();
        let mut err_lines = BufReader::new(stderr).lines();
        let mut progress = BuildProgress::default();
        // Branch guards keep a closed stream from spinning the select loop.
        let (mut out_done, mut err_done) = (false, false);
        while !(out_done && err_done) {
            tokio::select! {
                line = out_lines.next_line(), if !out_done => match line? {
                    Some(l) => {
                        if let Some(note) = progress.saw(&l) {
                            self.log(note);
                        }
                        self.log(l);
                    }
                    None => out_done = true,
                },
                line = err_lines.next_line(), if !err_done => match line? {
                    Some(l) => {
                        if let Some(note) = progress.saw(&l) {
                            self.log(note);
                        }
                        self.log(l);
                    }
                    None => err_done = true,
                },
                _ = tokio::time::sleep(QUIET_AFTER) => {
                    if let Some(note) = progress.quiet() {
                        self.log(note);
                    }
                }
            }
        }
        let status = child.wait().await?;
        if !status.success() {
            bail!(
                "podman {} failed: {status}",
                args.first().cloned().unwrap_or_default()
            );
        }
        Ok(())
    }

    /// Run a podman command and capture stdout (for ids and inspection).
    async fn run_captured(&self, args: Vec<String>) -> Result<String> {
        let output = self
            .podman(&args)
            .output()
            .await
            .context("running podman")?;
        if !output.status.success() {
            // The whole command and everything podman said go to the log;
            // the error is podman's own last line, which is what a banner,
            // a toast, or a prompt has room for (the full `podman run …`
            // with every mount in it was what an agent got handed as "the
            // failure", 2026-09-16).
            let err = String::from_utf8_lossy(&output.stderr);
            self.log(format!("$ podman {}", args.join(" ")));
            for line in err.lines().filter(|l| !l.trim().is_empty()) {
                self.log(line.to_string());
            }
            let last = err
                .lines()
                .rev()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("no output");
            let verb = args.first().map(String::as_str).unwrap_or("command");
            bail!("podman {verb} failed: {last}");
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Full (re)build-and-start cycle. Idempotent: tears down any previous
    /// container for this workspace first. Editor buffers, git state, and
    /// agent sessions are structurally out of reach of this function — the
    /// design's "never interrupt the AI session" guarantee.
    /// The reload about to run was asked for by this environment's agent:
    /// its outcome is reported to that agent when it is in
    /// (`Event::ReloadReport`).
    pub fn note_agent_reload(&self) {
        self.agent_reload.store(true, Ordering::SeqCst);
    }

    pub async fn reload(&self) -> Result<()> {
        let result = self.reload_reporting().await;
        if self.agent_reload.swap(false, Ordering::SeqCst) {
            self.events.publish(Event::ReloadReport {
                env: self.env.id.clone(),
                ok: result.is_ok(),
                message: result
                    .as_ref()
                    .err()
                    .map(|e| format!("{e:#}"))
                    .unwrap_or_default(),
            });
        }
        result
    }

    async fn reload_reporting(&self) -> Result<()> {
        if self.inside {
            bail!(
                "the IDE is running inside this devcontainer; rebuild it from \
                 the host-side IDE (a container cannot rebuild itself, and no \
                 container runtime is forwarded in here — that would hand the \
                 agent and the repo's own build the host)"
            );
        }
        // One lifecycle operation at a time: a second reload (agent via MCP,
        // second button press) waits instead of interleaving podman calls.
        let _lifecycle = self.lifecycle.lock().await;
        let failed_before = self.build_failed.lock().unwrap().clone();
        let result = self.reload_locked_with(false).await;
        let failed_after = self.build_failed.lock().unwrap().clone();
        if result.is_err() && failed_after.is_some() && failed_after != failed_before {
            // The project's image would not build or pull. Nothing running
            // is the one outcome that helps nobody — the agent that could
            // repair the config lands outside any container, on a stand-in
            // it cannot write — so the baseline stands in, and the banner
            // and `devcontainer_status` carry podman's reason.
            self.log(
                "the project's environment could not be built or started; the baseline \
                 stands in so the configuration can be repaired"
                    .to_string(),
            );
            return self.reload_locked_with(false).await;
        }
        result
    }

    /// Bring the baseline up without building the project's own config —
    /// safe mode as a container, which is what safe mode is meant to be.
    ///
    /// This is what runs when the user's checkout has no container: at
    /// launch with a devcontainer.json that has not been built, after a
    /// failure and a relaunch, after a stop and a config edit. Without it
    /// the environment sat in "configured, not started" with nothing
    /// running, and its agent landed on the rung below both — outside any
    /// container, on a stand-in workspace where the config it had just
    /// written read as "File does not exist" (David, 2026-09-16: "I really
    /// need you to actually fix these reads"; "Safe mode was supposed to
    /// be its own containerized environment"). The project's own build
    /// stays the user's: its Rebuild, or the agent's devcontainer_reload
    /// with the user's yes. A baseline already running is left alone.
    pub async fn reload_baseline(&self) -> Result<()> {
        if self.inside {
            bail!("the IDE is running inside this devcontainer; nothing to bring up from here");
        }
        let _lifecycle = self.lifecycle.lock().await;
        if matches!(self.state(), SupervisorState::Running { .. })
            && self.config_authority() == ConfigAuthority::Baseline
        {
            return Ok(());
        }
        self.reload_locked_with(true).await
    }

    /// One reload, under the lifecycle lock: resolve, tear down, build or
    /// pull, start. A project image that fails to build or pull is
    /// remembered (`remember_build_failure`) and the error returned; the
    /// caller decides whether the baseline follows. With `baseline_only`
    /// a project config that resolved is set aside for the baseline — not
    /// passed over, just not built yet — so the banner offers its Rebuild.
    async fn reload_locked_with(&self, baseline_only: bool) -> Result<()> {
        // Pick the config: the project's when it is present and confined,
        // the IDE's baseline otherwise. Every early error must land in a
        // *state* — the banner and MCP read states, not Results — and the
        // only error left that can reach here is a broken IDE install (the
        // bundled baseline failing to write or parse), which is a genuine
        // failure rather than a reason to sit in safe mode.
        let resolved = match self.resolve_config() {
            Ok(resolved) => resolved,
            Err(e) => {
                self.log(format!("no usable configuration: {e:#}"));
                self.set_state(SupervisorState::Failed {
                    message: e.to_string(),
                });
                return Err(e);
            }
        };
        // A new start is a new chance for the lifecycle commands.
        self.hook_failure.lock().unwrap().take();
        let resolved = if baseline_only && resolved.authority == ConfigAuthority::Project {
            ResolvedConfig {
                config: crate::baseline::ensure_baseline_config()?,
                authority: ConfigAuthority::Baseline,
                reason: Some(
                    "the project's configuration has not been built yet; Rebuild builds it"
                        .to_string(),
                ),
            }
        } else {
            resolved
        };
        let ResolvedConfig {
            config,
            authority,
            reason,
        } = resolved;
        if authority == ConfigAuthority::Baseline {
            // Say why, once, at the top of the build log. "Why am I in safe
            // mode" is the first question the repair loop asks.
            match &reason {
                Some(reason) => self.log(format!("baseline environment: {reason}")),
                None => self.log(
                    "baseline environment: this checkout has no devcontainer configuration"
                        .to_string(),
                ),
            }
        }
        let hash = config_hash(&config, &self.ide_mounts(&config, authority))?;
        let name = self.container_name();

        // Tear down any previous instance (ignore "no such container").
        self.exec.set_host();
        self.forget_agent_hosting();
        let _ = self
            .run_captured(vec![
                "rm".into(),
                "-f".into(),
                "-t".into(),
                "2".into(),
                name.clone(),
            ])
            .await;

        // Build or pull the image.
        self.set_state(SupervisorState::Building);
        let image = if let Some(dockerfile) = config.dockerfile_path() {
            let tag = self.image_tag(&config)?;
            // Build from a STAGED copy, never from the live directory.
            // Validation alone cannot hold here: the config scope is the
            // one thing an agent may write in either mode, so a directory
            // checked at parse can be a symlink by the time podman reads
            // it. Staging closes that window by construction — walk it
            // once, refuse symlinks on the way, build from bytes already
            // ours.
            let staged = stage_build_context(&config.build_context(), &name)?;
            let staged_dockerfile = dockerfile
                .file_name()
                .map(|f| staged.join(f))
                .unwrap_or_else(|| staged.join("Containerfile"));
            // The argument list is `crate::image::build_args`, shared with
            // the keeper's build of the baseline in a VM: the decisions in
            // it — what a build is denied, the memory ceiling, the label —
            // live in one place and this is not it.
            let args = crate::image::build_args(
                &config,
                &tag,
                &staged_dockerfile,
                &staged,
                &self.workspace_key(),
            );
            self.run_logged(args).await.inspect_err(|e| {
                if authority == ConfigAuthority::Project {
                    self.remember_build_failure(&config, e);
                }
                self.set_state(SupervisorState::Failed {
                    message: e.to_string(),
                })
            })?;
            tag
        } else {
            let image = config.image.clone().unwrap();
            self.run_logged(vec!["pull".into(), image.clone()])
                .await
                .inspect_err(|e| {
                    if authority == ConfigAuthority::Project {
                        self.remember_build_failure(&config, e);
                    }
                    self.set_state(SupervisorState::Failed {
                        message: e.to_string(),
                    })
                })?;
            image
        };

        // Start the container.
        self.set_state(SupervisorState::Starting);
        let workdir = config.workspace_folder().to_string();
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            name.clone(),
            "--label".into(),
            format!("{LABEL_CONFIG_HASH}={hash}"),
            "--label".into(),
            format!("{LABEL_AUTHORITY}={}", authority.label()),
            "--workdir".into(),
            workdir.clone(),
        ];
        args.extend(self.resource_labels());
        // `${localWorkspaceFolder}` means THIS environment's checkout — the
        // clone, for a non-primary environment. The whole config is
        // evaluated against this environment's checkout, which is what
        // makes one config serve N environments without a single
        // conditional — and a checkout in a VM is bound at ITS path there,
        // since the podman doing the binding is the VM's.
        let local_workspace_folder = self.env.checkout.path().display().to_string();
        match &config.workspace_mount {
            Some(mount) => {
                args.push("--mount".into());
                args.push(self.namespaced_mount(
                    &mount.replace("${localWorkspaceFolder}", &local_workspace_folder),
                ));
            }
            None => {
                args.push("-v".into());
                args.push(format!(
                    "{local_workspace_folder}:{workdir}:{}",
                    workspace_bind_flags(authority)
                ));
            }
        }
        for mount in &config.mounts {
            if let Some(m) = mount.as_str() {
                args.push("--mount".into());
                args.push(self.namespaced_mount(
                    &m.replace("${localWorkspaceFolder}", &local_workspace_folder),
                ));
            }
        }

        // The bind source for the config the agent may write (`ide_mounts`):
        // a bind needs one, and a project with no config has none yet.
        if authority == ConfigAuthority::Baseline {
            let config_dir = self.env.checkout.path().join(".devcontainer");
            // On this host. A checkout in a VM has the directory made over
            // there, by the files service, before the run.
            if let Err(e) = self.make_dir_in_checkout(&config_dir) {
                tracing::warn!(
                    "could not make {} for the agent to write its config into: {e}",
                    config_dir.display()
                );
            }
        }
        args.extend(self.ide_mounts(&config, authority));
        for (k, v) in &config.container_env {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        // forwardPorts: published on localhost only — services in the
        // container become reachable from the host without exposing them
        // to the network.
        let published = self.publish_ports(&config);
        for (port, host) in &published {
            args.push("-p".into());
            args.push(format!("127.0.0.1:{host}:{port}"));
        }
        if !published.is_empty() {
            args.push("--label".into());
            args.push(format!("{LABEL_PORTS}={}", ports_label(&published)));
        }
        // The user namespace, when the config does not choose one. Rootless
        // podman maps the host user to container ROOT by default, so a
        // bind-mounted checkout shows as root's inside, and an image whose
        // user is not root — this one's `agent`, uid 1000 — cannot write a
        // byte of it: the agent reported ".devcontainer is mounted
        // read-only" of a read-write mount (David, 2026-09-16: "Shouldn't
        // this be writable?"). `keep-id` with that user's uid maps the host
        // user onto it instead, which is what the baseline's own template
        // has always passed. The uid is the image's word, asked of the
        // image; a root user needs no mapping, and a config with its own
        // `--userns` is left alone.
        if let Some(flag) = self.userns_flag_for(&config, &image).await {
            self.log(format!(
                "{flag}: mapping your uid onto the container's user, so the checkout is \
                 writable inside"
            ));
            args.push(flag);
        }
        for arg in &config.run_args {
            if crate::security::STRIPPED_FLAGS.contains(&arg.as_str()) {
                self.log(format!(
                    "runArgs {arg} ignored — Docker needs it for systemd, rootless podman does not"
                ));
                continue;
            }
            args.push(arg.clone());
        }
        args.push(image);
        if config.override_command != Some(false) {
            args.push("sleep".into());
            args.push("infinity".into());
        }
        // The bind sources the config reads from the workspace, made
        // before the run: podman refuses a missing one ("statfs …: no
        // such file or directory"), and a project that binds its vendor
        // folder has none until its first install (2026-09-16).
        if authority == ConfigAuthority::Project {
            for source in crate::security::bind_sources(&config, self.env.checkout.path()) {
                if self.env.checkout.is_local() && source.exists() {
                    continue;
                }
                match self.make_dir_in_checkout(&source) {
                    Ok(()) => self.log(format!(
                        "created {} for the config's bind mount",
                        source.display()
                    )),
                    Err(e) => self.log(format!(
                        "could not create {} for the config's bind mount: {e}",
                        source.display()
                    )),
                }
            }
        }
        let container_id = self.run_captured(args).await.inspect_err(|e| {
            // A run that fails is the config's fault as much as a build
            // that does: remembered, so the baseline follows.
            if authority == ConfigAuthority::Project {
                self.remember_build_failure(&config, e);
            }
            self.set_state(SupervisorState::Failed {
                message: e.to_string(),
            })
        })?;
        self.log(format!("container started: {container_id}"));

        // Git identity: inherited from the host BEFORE the lifecycle hooks
        // (which may themselves commit). A container that already has one —
        // baked into the image or persisted in a home volume — keeps it.
        self.inherit_git_identity(&name, config.effective_user())
            .await;

        // Lifecycle hooks, in spec order.
        for hook in [
            &config.on_create_command,
            &config.post_create_command,
            &config.post_start_command,
        ]
        .into_iter()
        .flatten()
        {
            for argv in lifecycle_commands(hook) {
                let mut exec_args: Vec<String> =
                    vec!["exec".into(), "--workdir".into(), workdir.clone()];
                if let Some(user) = config.effective_user() {
                    exec_args.push("--user".into());
                    exec_args.push(user.to_string());
                }
                exec_args.push(name.clone());
                exec_args.extend(argv);
                let shown = argv_for_log(&exec_args);
                if let Err(e) = self.run_logged(exec_args).await {
                    // The command is the project's; the container it ran in
                    // is up and real. Keep it, say so, and stop running the
                    // commands after it — they assumed this one.
                    let reason = format!("{shown}: {e}");
                    self.log(format!(
                        "lifecycle command failed: {reason} — the container stays up; fix the \
                         command or the image, then Rebuild"
                    ));
                    *self.hook_failure.lock().unwrap() = Some(reason.clone());
                    // In the runtime log too: the container is up, and a
                    // reader of what it writes should see what did not
                    // run in it (David, 2026-09-16: "shouldn't that be in
                    // my env runtime log?"). The command's own output is
                    // in the build log, where lifecycle output goes.
                    self.events.publish(Event::ContainerOutput {
                        env: self.env.id.clone(),
                        line: format!(
                            "[taste-ide] lifecycle command failed: {} — its output is in the \
                             Environment Build log",
                            first_line(&reason)
                        ),
                    });
                    // With the repair one button away (the window routes
                    // the action to the same Prompt Agent path the banner
                    // uses), and up long enough to reach it.
                    self.events.publish(Event::ToastAction {
                        message: format!(
                            "{}: a lifecycle command failed — {} (full log under Logs → \
                             Environment Build)",
                            self.env.id,
                            first_line(&reason)
                        ),
                        label: "Prompt Agent".into(),
                        action: format!("prompt-repair:{}", self.env.id),
                        timeout_seconds: taste_core::event::ACTION_TOAST_SECS,
                    });
                    break;
                }
            }
            if self.hook_failure.lock().unwrap().is_some() {
                break;
            }
        }

        // Success: record the hash, clear drift, re-point execution.
        *self.running_hash.lock().unwrap() = Some(hash);
        *self.authority.lock().unwrap() = authority;
        self.set_pending(false);
        self.exec.set_container(name, workdir, authority);
        // Answered before anyone can ask. A chat reacting to the Running
        // event decides its topology from this, and `Unknown` would cost it
        // a spawn outside followed by a respawn inside.
        self.probe_container().await;
        self.set_state(SupervisorState::Running { container_id });
        Ok(())
    }

    /// The `--userns` flag the run wants, if the config chose none and the
    /// container's user is not root: `keep-id` onto that user's uid and
    /// gid, read off the image (`id -u; id -g` as the user the container
    /// runs as — `remoteUser`/`containerUser` when the config names one,
    /// the image's default otherwise). `None` when the config has its own
    /// `--userns`, when the user is root, or when the image will not say.
    async fn userns_flag_for(&self, config: &DevcontainerConfig, image: &str) -> Option<String> {
        if config
            .run_args
            .iter()
            .any(|arg| arg.starts_with("--userns"))
        {
            return None;
        }
        let mut probe: Vec<String> = vec!["run".into(), "--rm".into()];
        if let Some(user) = config.effective_user() {
            probe.push("--user".into());
            probe.push(user.to_string());
        }
        probe.extend([
            "--entrypoint".into(),
            "sh".into(),
            image.to_string(),
            "-c".into(),
            "id -u; id -g".into(),
        ]);
        let out = match self.run_captured(probe).await {
            Ok(out) => out,
            Err(e) => {
                self.log(format!(
                    "could not ask the image which user it runs as ({e}); the host user maps \
                     to root inside"
                ));
                return None;
            }
        };
        let mut ids = out
            .lines()
            .map(str::trim)
            .filter_map(|l| l.parse::<u32>().ok());
        let (uid, gid) = (ids.next()?, ids.next()?);
        keep_id_flag(&config.run_args, uid, gid)
    }

    /// Copy the host's `user.name`/`user.email` into the container's
    /// global git config, unless the container already has an identity.
    /// A fresh container otherwise refuses every commit — terminals,
    /// hooks, agents — with "Author identity unknown", an error whose
    /// answer the IDE already knows. Never fatal: a container without
    /// git installed must still start.
    async fn inherit_git_identity(&self, name: &str, user: Option<&str>) {
        let Some(identity) = taste_git::host_identity() else {
            return; // nothing to inherit — the host is equally anonymous
        };
        let exec = |tail: &[&str]| {
            let mut args: Vec<String> = vec!["exec".into()];
            if let Some(user) = user {
                args.push("--user".into());
                args.push(user.to_string());
            }
            args.push(name.to_string());
            args.extend(tail.iter().map(|s| s.to_string()));
            args
        };
        // An existing identity wins: `--get` exits non-zero when unset.
        let existing = self
            .run_captured(exec(&["git", "config", "--global", "--get", "user.email"]))
            .await;
        if existing.map(|out| !out.trim().is_empty()).unwrap_or(false) {
            return;
        }
        for (key, value) in [
            ("user.name", &identity.name),
            ("user.email", &identity.email),
        ] {
            if let Err(e) = self
                .run_captured(exec(&["git", "config", "--global", key, value]))
                .await
            {
                self.log(format!("git identity not inherited ({key}): {e}"));
                return;
            }
        }
        self.log(format!(
            "git identity inherited from host: {} <{}>",
            identity.name, identity.email
        ));
    }

    /// Stop and remove the container; execution falls back to the host.
    pub async fn stop(&self) -> Result<()> {
        if self.inside {
            bail!("cannot stop the container the IDE itself runs in");
        }
        let _lifecycle = self.lifecycle.lock().await;
        let name = self.container_name();
        self.log(format!("stopping {name}"));
        let _ = self
            .run_captured(vec![
                "rm".into(),
                "-f".into(),
                "-t".into(),
                "2".into(),
                name,
            ])
            .await;
        *self.running_hash.lock().unwrap() = None;
        self.exec.set_host();
        // Whatever that container could host, it can host nothing now.
        self.forget_agent_hosting();
        self.set_state(SupervisorState::Stopped);
        self.set_pending(false);
        Ok(())
    }

    /// Apply an environment's review state to its container: an environment
    /// that is waiting on the user, or that the user has settled, does not
    /// need a container.
    ///
    /// This is the "flagging stops the container" half of the review
    /// lifecycle, and it is deliberately the ordinary [`Supervisor::stop`]
    /// rather than a second kind of stopped-ness. Revival is the ordinary
    /// start too: the container comes back on the next
    /// [`Supervisor::reload`], which is what the fleet row's Start action
    /// and `devcontainer_reload` already call. Nothing here starts anything
    /// — a review state is never a reason to spend the user's machine.
    ///
    /// Returns whether a container was actually stopped, so a caller can
    /// tell the user "stopped calm-1" only when that is true.
    pub async fn apply_review_state(&self, review: taste_core::ReviewState) -> Result<bool> {
        if !stop_wanted(review, &self.state()) {
            return Ok(false);
        }
        self.log(format!(
            "{} is {} — stopping its container",
            self.env.id,
            review.as_str()
        ));
        self.stop().await?;
        Ok(true)
    }

    /// Nuke: remove the container *and* its image, so the next start is a
    /// from-scratch rebuild. Named volumes are deliberately untouched —
    /// they are caches with their own removal affordance.
    ///
    /// The image is content-addressed, so it is shared with anything on the
    /// machine whose config hashes the same — any environment of this
    /// workspace, and any environment of any OTHER window's workspace, since
    /// N windows are open at once by design. The removal is therefore
    /// best-effort on purpose: `rmi` without `-f` is refused while any
    /// container anywhere still references the image, and that refusal is
    /// the right answer whoever the other container belongs to. Nuking one
    /// environment must not tear the floor out from under another.
    ///
    /// The one case that gets through is an image built but not yet run by
    /// somebody else, and its whole cost is that they rebuild. Nothing is
    /// lost, because there is nothing in an image that its config does not
    /// already determine.
    pub async fn nuke(&self) -> Result<()> {
        if self.inside {
            bail!("cannot nuke the container the IDE itself runs in");
        }
        let _lifecycle = self.lifecycle.lock().await;
        let name = self.container_name();
        self.log(format!("nuking {name}: removing container and image"));
        let _ = self
            .run_captured(vec![
                "rm".into(),
                "-f".into(),
                "-t".into(),
                "2".into(),
                name,
            ])
            .await;
        if let Some(tag) = self.current_image_tag() {
            if let Err(e) = self.run_captured(vec!["rmi".into(), tag.clone()]).await {
                self.log(format!(
                    "image {tag} kept: {e} (something else on this machine \
                     shares it — the tag is the config's content hash)"
                ));
            }
        }
        *self.running_hash.lock().unwrap() = None;
        self.exec.set_host();
        // Whatever that container could host, it can host nothing now.
        self.forget_agent_hosting();
        self.set_state(SupervisorState::Stopped);
        self.set_pending(false);
        Ok(())
    }

    /// Every podman volume this environment owns: the agent home plus the
    /// namespaced form of each volume the config declares.
    pub fn env_volumes(&self) -> Vec<String> {
        let mut volumes = vec![environment::env_home_volume(
            &self.env.workspace_root,
            &self.env.id,
        )];
        if let Ok(Some(config)) = DevcontainerConfig::discover(&self.config_root()) {
            volumes.extend(
                config
                    .named_volumes()
                    .iter()
                    .map(|declared| self.namespaced_volume(declared)),
            );
        }
        volumes
    }

    /// Remove one named volume — but only one this ENVIRONMENT owns.
    /// Anything else is refused: the environment view manages this
    /// environment, not podman at large, and certainly not a sibling
    /// environment's cache.
    pub async fn remove_volume(&self, volume: &str) -> Result<()> {
        if !self.env_volumes().iter().any(|v| v == volume) {
            bail!(
                "volume {volume} does not belong to environment {}",
                self.env.id
            );
        }
        self.log(format!("removing volume {volume}"));
        self.run_captured(vec![
            "volume".into(),
            "rm".into(),
            "-f".into(),
            volume.into(),
        ])
        .await?;
        Ok(())
    }

    /// What this environment costs on disk: its checkout plus the volumes
    /// it owns.
    ///
    /// Deliberately measured, never estimated, and deliberately **not**
    /// something the fleet view computes on its own: walking a checkout
    /// (`target/` and all) is filesystem work, so it happens here, on
    /// demand, off the caller's thread, and the fleet view caches what
    /// comes back. Volumes that cannot be measured — no mountpoint, or one
    /// this process cannot read, which is the Flatpak case — are counted as
    /// unmeasured rather than folded in as zero. A footprint that quietly
    /// under-reports is worse than one that says how much it could not see.
    pub async fn disk_usage(&self) -> DiskUsage {
        let mut usage = DiskUsage::default();
        let walk = self.walk(false).await;
        usage.checkout_bytes = walk.apparent_bytes;
        let volumes = self.volume_usage().await;
        usage.volume_bytes = volumes.apparent_bytes;
        usage.volumes_measured = volumes.measured;
        usage.volumes_unmeasured = volumes.unmeasured;
        // This walk saw everything, so it can answer the budget's question
        // under either scope — and a user who pressed Refresh has paid for
        // the artifacts already. Recording it here is what lets the whole
        // footprint be known at all under the clone scope, whose own
        // cadence never descends that far.
        self.record_disk(DiskSample {
            budget_bytes: match taste_core::environment::DISK_BUDGET_SCOPE {
                DiskBudgetScope::ClonesOnly => walk.clone_on_disk_bytes,
                DiskBudgetScope::WholeEnvironments => walk.on_disk_bytes + volumes.on_disk_bytes,
            },
            whole_bytes: Some(walk.on_disk_bytes + volumes.on_disk_bytes),
            unmeasured_volumes: volumes.unmeasured,
            at: std::time::Instant::now(),
        });
        usage
    }

    /// This environment's footprint against the disk budget, measured now
    /// and cached for the gates to read.
    ///
    /// The walk is the scope's: under
    /// [`DiskBudgetScope::ClonesOnly`] it prunes at every ignored directory
    /// and never touches the volumes, which is what makes a background
    /// cadence affordable at all — the clone is hundreds of megabytes and
    /// `target/` is a hundred gigabytes. Under
    /// [`DiskBudgetScope::WholeEnvironments`] it walks everything, volumes
    /// included, and the cost of that is part of what choosing that scope
    /// chooses.
    pub async fn measure_disk(&self, scope: DiskBudgetScope) -> DiskSample {
        let artifacts = scope.counts_build_artifacts();
        let walk = self.walk(!artifacts).await;
        let volumes = if artifacts {
            self.volume_usage().await
        } else {
            VolumeUsage::default()
        };
        let sample = DiskSample {
            budget_bytes: if artifacts {
                walk.on_disk_bytes + volumes.on_disk_bytes
            } else {
                walk.clone_on_disk_bytes
            },
            // A pruned walk saw none of the build output, so it has nothing
            // to say about the whole. Whatever an earlier full walk learned
            // stands until something walks it again.
            whole_bytes: (!walk.pruned_ignored)
                .then_some(walk.on_disk_bytes + volumes.on_disk_bytes)
                .or_else(|| self.measured_disk().and_then(|old| old.whole_bytes)),
            unmeasured_volumes: volumes.unmeasured,
            at: std::time::Instant::now(),
        };
        self.record_disk(sample);
        sample
    }

    fn record_disk(&self, sample: DiskSample) {
        *self.disk.lock().unwrap() = Some(sample);
    }

    /// What the last walk found, or `None` if nothing has walked this
    /// environment yet. Cheap by construction: this is the accessor the
    /// gates use, and it touches no filesystem.
    pub fn measured_disk(&self) -> Option<DiskSample> {
        *self.disk.lock().unwrap()
    }

    /// Test seam, the disk's counterpart to [`Self::set_state_for_tests`]:
    /// say what this environment costs, with nothing walked.
    ///
    /// The gates read the cache, so a test about a budget that is already
    /// spent would otherwise have to write ten gibibytes into a tempdir to
    /// pose the question. Nothing outside a test calls it: the sample is the
    /// walk's own account of what it found, and a second author of it would
    /// be a second account.
    #[doc(hidden)]
    pub fn set_disk_for_tests(&self, budget_bytes: u64) {
        self.record_disk(DiskSample {
            budget_bytes,
            whole_bytes: None,
            unmeasured_volumes: 0,
            at: std::time::Instant::now(),
        });
    }

    /// The volumes this environment owns, summed. Podman-side work — one
    /// `volume inspect` apiece — so it is skipped entirely by the scope
    /// that does not count volumes.
    async fn volume_usage(&self) -> VolumeUsage {
        let mut usage = VolumeUsage::default();
        if self.inside {
            // Self-hosting: podman is not reachable in here, so the volumes
            // are honestly unknown rather than zero.
            usage.unmeasured = self.env_volumes().len();
            return usage;
        }
        for volume in self.env_volumes() {
            let mountpoint = self
                .run_captured(vec![
                    "volume".into(),
                    "inspect".into(),
                    volume.clone(),
                    "--format".into(),
                    "{{.Mountpoint}}".into(),
                ])
                .await
                .ok()
                .map(|out| PathBuf::from(out.trim()))
                .filter(|path| path.is_dir());
            let Some(mountpoint) = mountpoint else {
                // Absent volumes cost nothing and are not "unmeasured";
                // only one that exists and could not be read is.
                continue;
            };
            match tokio::task::spawn_blocking(move || walk_checkout(&mountpoint, false)).await {
                Ok(walk) => {
                    usage.apparent_bytes += walk.apparent_bytes;
                    usage.on_disk_bytes += walk.on_disk_bytes;
                    usage.measured += 1;
                }
                Err(_) => usage.unmeasured += 1,
            }
        }
        usage
    }

    /// Everything podman-side associated with this environment: the
    /// container, its image, and the config's named volumes.
    pub async fn list_resources(&self) -> Vec<ResourceInfo> {
        if self.inside {
            return vec![ResourceInfo {
                kind: ResourceKind::Container,
                name: "this container (self-hosted session)".into(),
                id: "self".into(),
                status: "running — manage from the host IDE".into(),
            }];
        }
        let mut resources = Vec::new();

        // The substrate first, when it is not the user's own host. It is
        // not this environment's resource — one machine hosts every
        // environment — but it is the line that explains a footprint no
        // per-environment number accounts for, and the Resources view is
        // where the user goes to ask where the memory and the disk went.
        resources.extend(self.substrate().resource());

        // By label, not by name: same reason as adoption — the container's
        // own claim about which environment it is outlives our naming.
        if let Ok(out) = self
            .run_captured(vec![
                "ps".into(),
                "-a".into(),
                "--filter".into(),
                format!("label={LABEL_WORKSPACE}={}", self.workspace_key()),
                "--filter".into(),
                format!("label={LABEL_ENV}={}", self.env.id),
                "--format".into(),
                "{{.ID}}\t{{.Names}}\t{{.Status}}".into(),
            ])
            .await
        {
            for line in out.lines() {
                let mut fields = line.split('\t');
                if let (Some(id), Some(name), Some(status)) =
                    (fields.next(), fields.next(), fields.next())
                {
                    resources.push(ResourceInfo {
                        kind: ResourceKind::Container,
                        name: name.to_string(),
                        id: id.to_string(),
                        status: status.to_string(),
                    });
                }
            }
        }

        if let Some(tag) = self.current_image_tag() {
            if let Ok(out) = self
                .run_captured(vec![
                    "images".into(),
                    "--filter".into(),
                    format!("reference={tag}"),
                    "--format".into(),
                    "{{.ID}}\t{{.Repository}}\t{{.Size}}".into(),
                ])
                .await
            {
                for line in out.lines() {
                    let mut fields = line.split('\t');
                    if let (Some(id), Some(repo), Some(size)) =
                        (fields.next(), fields.next(), fields.next())
                    {
                        resources.push(ResourceInfo {
                            kind: ResourceKind::Image,
                            name: repo.to_string(),
                            id: id.to_string(),
                            status: size.to_string(),
                        });
                    }
                }
            }
        }

        for volume in self.env_volumes() {
            let exists = self
                .run_captured(vec![
                    "volume".into(),
                    "ls".into(),
                    "-q".into(),
                    "--filter".into(),
                    format!("name={volume}"),
                ])
                .await
                .map(|out| out.lines().any(|l| l == volume))
                .unwrap_or(false);
            resources.push(ResourceInfo {
                kind: ResourceKind::Volume,
                name: volume,
                id: String::new(),
                status: if exists { "present" } else { "absent" }.to_string(),
            });
        }

        resources
    }
}

/// Copy a build context somewhere podman can read it and the repo cannot
/// change it, and hand back the staged path.
///
/// Regular files and directories only. **Symlinks are refused, not
/// followed**: a link is the one entry that can mean something outside the
/// tree being copied, and whether `COPY` would dereference it is podman
/// business we would rather not depend on. Refusing is also the honest
/// error — a devcontainer context that needs a symlink out of itself is
/// not machine-independent and would not survive being sent to Codespaces.
///
/// Staged fresh each build, under the IDE own cache rather than a
/// world-writable temp dir, so no other user can plant the bytes we are
/// about to build.
pub(crate) fn stage_build_context(source: &Path, name: &str) -> Result<PathBuf> {
    let staged = staging_root().join(name);
    let _ = std::fs::remove_dir_all(&staged);
    std::fs::create_dir_all(&staged)
        .with_context(|| format!("creating build staging dir {}", staged.display()))?;
    copy_context_into(source, &staged)?;
    Ok(staged)
}

fn staging_root() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            Path::new(&home).join(".cache")
        })
        .join("taste-ide")
        .join("build-context")
}

fn copy_context_into(source: &Path, target: &Path) -> Result<()> {
    for entry in std::fs::read_dir(source)
        .with_context(|| format!("reading build context {}", source.display()))?
    {
        let entry = entry?;
        let kind = entry.file_type()?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        if kind.is_symlink() {
            bail!(
                "{}: symlinks are not allowed in a devcontainer build context — a link \
                 can point outside the repository, and a context that needs one is not \
                 portable",
                from.display()
            );
        } else if kind.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_context_into(&from, &to)?;
        } else if kind.is_file() {
            std::fs::copy(&from, &to).with_context(|| format!("staging {}", from.display()))?;
        }
        // Anything else (fifo, socket, device) is silently skipped: it
        // cannot contribute to an image build.
    }
    Ok(())
}

/// How long a build may print nothing before the log says why.
const QUIET_AFTER: std::time::Duration = std::time::Duration::from_secs(20);

/// The clock on a `podman build`: which step is running, when it began,
/// when it last printed, and whether it printed at all — from which the
/// quiet-time note is written.
#[derive(Debug, Default)]
struct BuildProgress {
    /// `STEP 2/5: RUN dnf install …`, as podman printed it.
    step: Option<String>,
    step_began: Option<std::time::Instant>,
    last_output: Option<std::time::Instant>,
    /// Lines the step's own command has printed.
    step_lines: u32,
    /// Quiet notes already written for this step, so the second one can
    /// say "still" and none of them repeat the explanation.
    notes: u32,
}

impl BuildProgress {
    /// A line arrived. Returns a note to log *before* it when the line
    /// ends a stretch of silence the log had already remarked on — so the
    /// reader learns the commit landed, and how long it took.
    fn saw(&mut self, line: &str) -> Option<String> {
        let now = std::time::Instant::now();
        let mut note = None;
        if line.starts_with("STEP ") {
            self.step = Some(line.trim().to_string());
            self.step_began = Some(now);
            self.step_lines = 0;
            self.notes = 0;
        } else if line.starts_with("--> ") || line.starts_with("COMMIT ") {
            // The layer id: the silence was the commit, and it is over.
            if self.notes > 0 {
                if let Some(since) = self.last_output {
                    note = Some(format!(
                        "… committed after {}s of silence",
                        since.elapsed().as_secs()
                    ));
                }
            }
        } else {
            self.step_lines += 1;
        }
        self.last_output = Some(now);
        note
    }

    /// Nothing has arrived for [`QUIET_AFTER`]: what to say about it, if
    /// the silence is inside a step. Between commands there is nothing to
    /// explain, and nothing is said.
    fn quiet(&mut self) -> Option<String> {
        let step = self.step.as_deref()?;
        let since = self.last_output?.elapsed().as_secs();
        let for_ = self.step_began?.elapsed().as_secs();
        self.notes += 1;
        Some(quiet_note(step, self.step_lines, since, for_, self.notes))
    }
}

/// The sentence for a quiet stretch. The first one explains; the rest keep
/// the clock running.
fn quiet_note(step: &str, step_lines: u32, quiet_secs: u64, step_secs: u64, nth: u32) -> String {
    let name = step.split_once(": ").map(|(head, _)| head).unwrap_or(step);
    let is_run = step.contains(": RUN ");
    if nth > 1 {
        return format!("… still {name}: quiet for {quiet_secs}s, {step_secs}s in total");
    }
    if is_run && step_lines > 0 {
        format!(
            "… {name} has printed nothing for {quiet_secs}s. Its command is done; podman \
             is committing the layer — every file it wrote is read back out through the \
             overlay and hashed, which for a multi-gigabyte layer under rootless \
             fuse-overlayfs takes minutes and prints nothing until the layer id lands."
        )
    } else if is_run {
        format!(
            "… {name} has printed nothing for {quiet_secs}s. The command is running \
             without output (a download, a long compile); podman prints nothing more \
             until it does."
        )
    } else {
        format!(
            "… {name} has printed nothing for {quiet_secs}s (copying into the layer, or \
             committing it)."
        )
    }
}

/// The command a `podman exec` ran, for a log line: everything after the
/// container's name.
fn argv_for_log(exec_args: &[String]) -> String {
    let at = exec_args
        .iter()
        .position(|arg| arg.starts_with("taste-"))
        .map(|i| i + 1)
        .unwrap_or(exec_args.len());
    exec_args[at..].join(" ")
}

/// The first line of a message, for a toast or a row.
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

/// `--userns=keep-id:uid=U,gid=G` for a non-root container user when the
/// config has not chosen a user namespace itself; nothing otherwise.
fn keep_id_flag(run_args: &[String], uid: u32, gid: u32) -> Option<String> {
    if run_args.iter().any(|arg| arg.starts_with("--userns")) || uid == 0 {
        return None;
    }
    Some(format!("--userns=keep-id:uid={uid},gid={gid}"))
}

/// Whether nothing on this machine holds `port` on loopback right now: a
/// bind that succeeds, released at once. Another environment's published
/// port, or a server of the user's, makes it fail.
fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// The first port above `port` that is free on loopback, not privileged
/// (1024 and up), and `eligible` — the caller's word on numbers already
/// spoken for. `None` when the range runs out.
fn next_free_port_above(port: u16, eligible: impl Fn(u16) -> bool) -> Option<u16> {
    (port.saturating_add(1).max(1024)..=u16::MAX)
        .find(|candidate| eligible(*candidate) && port_is_free(*candidate))
}

/// A loopback port nothing holds, chosen by the kernel: a test's way to
/// name one that is certainly free.
#[cfg(test)]
fn free_port() -> Option<u16> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .ok()
}

/// `LABEL_PORTS`'s value for these pairs: `container:host`, comma-joined.
fn ports_label(pairs: &[(u16, u16)]) -> String {
    pairs
        .iter()
        .map(|(port, host)| format!("{port}:{host}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// The pairs back out of a `LABEL_PORTS` value; anything malformed is
/// skipped, and an absent label is no pairs.
fn parse_ports_label(value: &str) -> Vec<(u16, u16)> {
    value
        .split(',')
        .filter_map(|pair| {
            let (port, host) = pair.trim().split_once(':')?;
            Some((port.parse().ok()?, host.parse().ok()?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    /// A forwarded port whose number is taken on this machine is published
    /// on the nearest free number above it, said in the log, and
    /// remembered for the rows; a free one stays where the config put it.
    /// The label carries the pairs to an adopting IDE and back.
    #[test]
    fn a_taken_port_is_published_elsewhere_and_the_label_says_where() {
        let dir = tempfile::tempdir().unwrap();
        let sup = make(dir.path());
        // Hold one loopback port for the duration, so it reads as taken.
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let taken = held.local_addr().unwrap().port();
        let free = free_port().unwrap();
        let config_path = dir.path().join("devcontainer.json");
        std::fs::write(
            &config_path,
            format!(r#"{{"image": "x", "forwardPorts": [{taken}, {free}]}}"#),
        )
        .unwrap();
        let config = DevcontainerConfig::load(&config_path).unwrap();
        *sup.declared_ports.lock().unwrap() = config.ports();
        let published = sup.publish_ports(&config);
        let (moved, stayed) = if published[0].0 == taken {
            (published[0], published[1])
        } else {
            (published[1], published[0])
        };
        assert_ne!(moved.1, taken, "{published:?}");
        // The nearest free number above, not one from the kernel: every
        // number between the taken one and the one chosen is itself taken
        // (or is the other port this config forwards).
        assert!(moved.1 > taken, "{published:?}");
        assert!(moved.1 >= 1024);
        for skipped in (taken + 1)..moved.1 {
            assert!(
                skipped == free || !port_is_free(skipped),
                "{skipped} was free and nearer than {}",
                moved.1
            );
        }
        assert_eq!(stayed, (free, free), "{published:?}");
        let rows = sup.ports();
        let row = rows.iter().find(|spec| spec.port == taken).unwrap();
        assert_eq!(row.host, moved.1);
        assert!(row.moved());
        assert!(row.url().ends_with(&format!(":{}", moved.1)));
        assert!(!rows.iter().find(|spec| spec.port == free).unwrap().moved());
        assert!(sup
            .logs_tail(5)
            .iter()
            .any(|line| line.contains(&format!("port {taken} is in use"))));

        let label = ports_label(&published);
        assert_eq!(parse_ports_label(&label), published);
        assert!(parse_ports_label("").is_empty());
        assert!(parse_ports_label(crate::reconcile::label("<no value>")).is_empty());
    }

    /// One config's own ports are never each other's landing place: with
    /// 8000 taken and 8001 forwarded too, 8000 moves past 8001.
    #[test]
    fn a_moved_port_skips_the_numbers_its_own_config_forwards() {
        let dir = tempfile::tempdir().unwrap();
        let sup = make(dir.path());
        // Two adjacent free ports, the lower one then held.
        let (lower, upper) = loop {
            let a = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let p = a.local_addr().unwrap().port();
            if p < u16::MAX - 2 && port_is_free(p + 1) {
                break (a, p + 1);
            }
        };
        let taken = lower.local_addr().unwrap().port();
        let config_path = dir.path().join("devcontainer.json");
        std::fs::write(
            &config_path,
            format!(r#"{{"image": "x", "forwardPorts": [{taken}, {upper}]}}"#),
        )
        .unwrap();
        let config = DevcontainerConfig::load(&config_path).unwrap();
        let published = sup.publish_ports(&config);
        let host_of = |port: u16| published.iter().find(|(p, _)| *p == port).unwrap().1;
        assert_eq!(host_of(upper), upper, "{published:?}");
        assert!(host_of(taken) > upper, "{published:?}");
    }

    #[test]
    fn a_quiet_run_step_is_explained_once_and_then_timed() {
        let step = "STEP 2/5: RUN dnf install -y gcc";
        let first = super::quiet_note(step, 400, 20, 95, 1);
        assert!(
            first.starts_with("… STEP 2/5 has printed nothing for 20s"),
            "{first}"
        );
        assert!(first.contains("committing the layer"), "{first}");
        let second = super::quiet_note(step, 400, 40, 115, 2);
        assert_eq!(second, "… still STEP 2/5: quiet for 40s, 115s in total");
        // A RUN that never printed is not being committed yet.
        let silent = super::quiet_note(step, 0, 20, 20, 1);
        assert!(silent.contains("running without output"), "{silent}");
        // COPY has no command output to speak of.
        let copy = super::quiet_note("STEP 3/5: COPY . /src", 0, 20, 20, 1);
        assert!(copy.contains("copying into the layer"), "{copy}");
    }

    #[test]
    fn the_clock_follows_steps_and_the_commit_closes_the_silence() {
        let mut progress = super::BuildProgress::default();
        // Before any step, quiet means nothing.
        assert!(progress.quiet().is_none());
        assert!(progress.saw("STEP 1/2: FROM fedora:44").is_none());
        assert!(progress.saw("STEP 2/2: RUN dnf install -y gcc").is_none());
        assert!(progress.saw("Installing: gcc").is_none());
        assert!(progress.saw("Complete!").is_none());
        let note = progress.quiet().expect("a quiet note inside a step");
        assert!(note.contains("committing the layer"), "{note}");
        // The layer id lands: the note before it says the silence is over.
        let landed = progress
            .saw("--> a1b2c3d4e5f6")
            .expect("the commit closes the silence");
        assert!(landed.starts_with("… committed after"), "{landed}");
        // No commit was pending: no note.
        assert!(progress.saw("STEP 3/3: COPY . /src").is_none());
        assert!(progress.saw("--> 0f0f0f").is_none());
    }

    use super::*;

    fn make(root: &std::path::Path) -> Arc<Supervisor> {
        make_env(root, EnvironmentIdentity::primary(root))
    }

    /// The situation an agent is told is the one the environment is in,
    /// and it always ends in something to do: a status with no next step
    /// is a status the smallest model cannot act on.
    #[test]
    fn the_situation_names_the_next_call_in_every_state() {
        let dir = tempfile::tempdir().unwrap();
        let supervisor = make(dir.path());
        // Nothing built, nothing configured: write the config, then reload.
        let fresh = supervisor.situation();
        assert_eq!(fresh.mode, "safe");
        assert!(fresh.failure.is_none(), "{fresh:?}");
        assert!(
            fresh.writable.starts_with("only .devcontainer/"),
            "{fresh:?}"
        );
        assert!(fresh.next.contains("devcontainer_reload"), "{fresh:?}");
        // A failed build is named, and the log is where to look.
        supervisor.set_state_for_tests(SupervisorState::Failed {
            message: "manifest unknown".into(),
        });
        let failed = supervisor.situation();
        assert_eq!(
            failed.failure.as_deref(),
            Some("the build failed: manifest unknown")
        );
        assert!(failed.next.contains("devcontainer_reload"), "{failed:?}");
        // Mid-build, the only thing to do is wait — and the situation says so
        // rather than sending the agent to repair a config still building.
        supervisor.set_state_for_tests(SupervisorState::Building);
        let building = supervisor.situation();
        assert!(building.next.contains("Wait"), "{building:?}");
        assert!(
            !building.next.contains("devcontainer_reload"),
            "{building:?}"
        );
    }

    /// The fleet view's disk column is only as honest as this walk: it must
    /// count nested files and must not follow a link out of the tree (which
    /// would charge an environment for a directory it does not own — or
    /// loop).
    #[test]
    fn a_checkout_is_measured_by_walking_it_without_following_links() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), vec![b'x'; 100]).unwrap();
        std::fs::create_dir_all(dir.path().join("nested/deep")).unwrap();
        std::fs::write(dir.path().join("nested/deep/b"), vec![b'y'; 250]).unwrap();
        assert_eq!(dir_size(dir.path()), 350);

        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("huge"), vec![b'z'; 10_000]).unwrap();
            std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
            assert_eq!(dir_size(dir.path()), 350, "a link is not this env's disk");
        }
        // An unreadable path is zero, not a panic and not a refusal.
        assert_eq!(dir_size(&dir.path().join("no-such-dir")), 0);
    }

    /// The clone and its build output are one tree and two numbers, and the
    /// disk budget only ever weighs the first. `.gitignore` is what
    /// separates them — the project's own statement of which files are a
    /// cache nobody has to keep — and a pruned walk must not so much as
    /// descend into what it excludes: on this repository that side of the
    /// line is a hundred gigabytes, and walking it to subtract it would cost
    /// exactly as much as counting it.
    #[test]
    fn the_clone_is_what_git_does_not_ignore_and_a_pruned_walk_never_enters_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git2::Repository::init(root).unwrap();
        std::fs::write(root.join(".gitignore"), "target\n").unwrap();

        // What the repository costs before any of this test's files: `.git`
        // is real disk and is counted, and how much of it there is belongs
        // to libgit2 rather than to this assertion.
        let empty_whole = walk_checkout(root, false);
        let empty_clone = walk_checkout(root, true);

        std::fs::write(root.join("main.rs"), vec![b'x'; 4_096]).unwrap();
        std::fs::create_dir_all(root.join("target/debug/deps")).unwrap();
        std::fs::write(
            root.join("target/debug/deps/libtaste.rlib"),
            vec![b'z'; 200_000],
        )
        .unwrap();

        // Counting everything: the artifacts dominate, and the clone is the
        // small remainder the budget is a budget of.
        let whole = walk_checkout(root, false);
        assert!(!whole.pruned_ignored);
        assert_eq!(whole.apparent_bytes - empty_whole.apparent_bytes, 204_096);
        assert!(
            whole.on_disk_bytes > whole.clone_on_disk_bytes + 100_000,
            "the ignored build output is the bulk of the tree: {whole:?}"
        );

        // Pruning: `target/` is never descended into, so its bytes are
        // absent from every total rather than subtracted from one of them —
        // and the clone's own number is the same either way, which is what
        // makes the cheap walk answer the expensive walk's question.
        let clone = walk_checkout(root, true);
        assert!(clone.pruned_ignored);
        assert_eq!(clone.apparent_bytes - empty_clone.apparent_bytes, 4_096);
        assert_eq!(clone.on_disk_bytes, clone.clone_on_disk_bytes);
        assert_eq!(clone.clone_on_disk_bytes, whole.clone_on_disk_bytes);

        // Apparent and allocated are answers to different questions, and
        // which of them is larger is the filesystem's business — one that
        // allocates in blocks rounds every file up, one that compresses can
        // go the other way — so what is pinned here is only that the budget
        // reads the allocated number and that it is a real measurement.
        assert!(clone.on_disk_bytes > 0);
    }

    /// A directory that is not a checkout at all has no ignore rules to ask
    /// about, and neither has one whose repository is somebody else's: a
    /// stand-in workspace nested under another clone must be measured
    /// against its own `.gitignore` or against none, never against the
    /// ancestor's.
    #[test]
    fn a_tree_with_no_repository_of_its_own_ignores_nothing() {
        let outer = tempfile::tempdir().unwrap();
        git2::Repository::init(outer.path()).unwrap();
        std::fs::write(outer.path().join(".gitignore"), "target\n").unwrap();
        let inner = outer.path().join("nested");
        std::fs::create_dir_all(inner.join("target")).unwrap();
        std::fs::write(inner.join("target/artifact"), vec![b'z'; 8_192]).unwrap();

        let walk = walk_checkout(&inner, true);
        assert_eq!(walk.apparent_bytes, 8_192);
        assert_eq!(
            walk.clone_on_disk_bytes, walk.on_disk_bytes,
            "the ancestor's rules are not this tree's: {walk:?}"
        );
    }

    #[test]
    fn disk_usage_says_when_part_of_the_footprint_is_unknown() {
        let complete = DiskUsage {
            checkout_bytes: 10,
            volume_bytes: 5,
            volumes_measured: 1,
            volumes_unmeasured: 0,
        };
        assert_eq!(complete.total_bytes(), 15);
        assert!(!complete.partial());
        assert!(DiskUsage {
            volumes_unmeasured: 1,
            ..complete
        }
        .partial());
    }

    fn make_env(_root: &std::path::Path, env: EnvironmentIdentity) -> Arc<Supervisor> {
        Supervisor::new_outside_container_for_tests(
            env,
            EventBus::new(),
            ExecContext::host_unsandboxed_for_tests(),
            crate::substrate::Substrate::local_for_tests(),
        )
    }

    /// Flag → stop, as a decision. Only a live container is stopped, and
    /// only a review state that means "nobody is talking to this" stops it.
    #[test]
    fn only_a_live_container_of_a_settled_environment_is_stopped() {
        use taste_core::ReviewState;
        let live = [
            SupervisorState::Running {
                container_id: "abc".into(),
            },
            SupervisorState::Starting,
            SupervisorState::Building,
        ];
        let down = [
            SupervisorState::Stopped,
            SupervisorState::NoConfig,
            SupervisorState::ConfigDetected,
            SupervisorState::Failed {
                message: "boom".into(),
            },
        ];
        for state in live.iter().chain(down.iter()) {
            assert!(
                !stop_wanted(ReviewState::Working, state),
                "work in progress keeps its container: {state:?}"
            );
        }
        for review in [
            ReviewState::FlaggedForReview,
            ReviewState::Merged,
            ReviewState::Rejected,
        ] {
            for state in &live {
                assert!(stop_wanted(review, state), "{review:?} / {state:?}");
            }
            for state in &down {
                assert!(
                    !stop_wanted(review, state),
                    "nothing to stop: {review:?} / {state:?}"
                );
            }
        }
    }

    /// The same partition, stated on the predicate itself — because it now
    /// answers a second question with a cost attached: how many
    /// environments the orchestration cap counts
    /// ([`taste_core::environment::MAX_ORCHESTRATED_ENVIRONMENTS`]). A
    /// variant that drifted from one side to the other would move a
    /// container and a slot at once.
    #[test]
    fn a_container_up_or_on_its_way_is_the_machine_being_spent() {
        for state in [
            SupervisorState::Running {
                container_id: "abc".into(),
            },
            SupervisorState::Starting,
            SupervisorState::Building,
        ] {
            assert!(state.holds_a_container(), "{state:?}");
        }
        for state in [
            SupervisorState::Stopped,
            SupervisorState::NoConfig,
            SupervisorState::ConfigDetected,
            SupervisorState::Failed {
                message: "boom".into(),
            },
        ] {
            assert!(
                !state.holds_a_container(),
                "a clone on disk and nothing more: {state:?}"
            );
        }
    }

    /// ...and the async wrapper touches podman for none of the cases the
    /// decision says no to. A supervisor with no container answers without
    /// running anything.
    #[tokio::test]
    async fn applying_a_review_state_to_a_stopped_environment_asks_podman_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let supervisor = make_env(
            dir.path(),
            EnvironmentIdentity::cloned(dir.path(), env("calm-1")),
        );
        assert_eq!(supervisor.state(), SupervisorState::NoConfig);
        assert!(!supervisor
            .apply_review_state(taste_core::ReviewState::FlaggedForReview)
            .await
            .unwrap());
        assert!(!supervisor
            .apply_review_state(taste_core::ReviewState::Working)
            .await
            .unwrap());
        assert_eq!(supervisor.state(), SupervisorState::NoConfig);
    }

    fn env(slug: &str) -> EnvironmentId {
        EnvironmentId::parse(slug).unwrap()
    }

    fn write_config(root: &std::path::Path) {
        let dc = root.join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();
        std::fs::write(
            dc.join("devcontainer.json"),
            r#"{"image": "registry.example/img:1"}"#,
        )
        .unwrap();
    }

    /// The top of the ladder: a project config that parses, validates and
    /// passes the security validator is what runs. The baseline exists for
    /// when that is not true, not as something to prefer.
    #[test]
    fn a_healthy_project_config_is_what_runs() {
        let dir = tempfile::tempdir().unwrap();
        let sup = make(dir.path());
        write_config(dir.path());

        let resolved = sup.resolve_config().unwrap();
        assert_eq!(resolved.authority, ConfigAuthority::Project);
        assert_eq!(
            resolved.config.image.as_deref(),
            Some("registry.example/img:1")
        );
        assert!(resolved.reason.is_none(), "nothing to explain");
    }

    /// A clone is made from a commit, so a config the user wrote and did
    /// not commit is exactly what it lacks — and the row, the tool, and the
    /// orientation all say so instead of an unexplained "safe mode".
    #[test]
    fn a_clone_missing_the_uncommitted_main_config_says_why() {
        let main = tempfile::tempdir().unwrap();
        write_config(main.path());
        let clone_root = tempfile::tempdir().unwrap();
        let clone = make_env(
            main.path(),
            EnvironmentIdentity::local_at(
                main.path(),
                EnvironmentId::parse("i-0001").unwrap(),
                clone_root.path().to_path_buf(),
            ),
        );
        let resolved = clone.resolve_config().unwrap();
        assert_eq!(resolved.authority, ConfigAuthority::Baseline);
        let reason = resolved.reason.expect("the missing config is explained");
        assert!(reason.contains("not committed"), "{reason}");
        let situation = clone.situation();
        assert!(
            situation.next.contains("commit .devcontainer/"),
            "{situation:?}"
        );
        assert!(situation.next.contains("update_from_main"), "{situation:?}");
        // The primary with no config at all is still not a fault.
        let bare = tempfile::tempdir().unwrap();
        assert!(make(bare.path()).resolve_config().unwrap().reason.is_none());
    }

    /// The middle rung, reached three ways. Each of these used to be a
    /// workspace where nothing could run; each is now a baseline container.
    #[test]
    fn absent_malformed_and_refused_configs_all_fall_to_the_baseline() {
        // (a) No config at all — the commonest case, and not an error.
        let dir = tempfile::tempdir().unwrap();
        let sup = make(dir.path());
        let resolved = sup.resolve_config().unwrap();
        assert_eq!(resolved.authority, ConfigAuthority::Baseline);
        assert!(
            resolved.reason.is_none(),
            "having no devcontainer is not a fault to report"
        );

        // (b) Present but unparseable. The agent is mid-edit; the
        // environment should still come up so it can finish the edit.
        let dir = tempfile::tempdir().unwrap();
        let sup = make(dir.path());
        let dc = dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();
        std::fs::write(dc.join("devcontainer.json"), "{ this is not json").unwrap();
        let resolved = sup.resolve_config().unwrap();
        assert_eq!(resolved.authority, ConfigAuthority::Baseline);
        assert!(
            resolved
                .reason
                .is_some_and(|r| r.contains("could not be read")),
            "the log should say why"
        );

        // (c) Parses, but names neither an image nor a dockerfile.
        let dir = tempfile::tempdir().unwrap();
        let sup = make(dir.path());
        let dc = dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();
        std::fs::write(dc.join("devcontainer.json"), r#"{"name": "empty"}"#).unwrap();
        let resolved = sup.resolve_config().unwrap();
        assert_eq!(resolved.authority, ConfigAuthority::Baseline);
        assert!(resolved.reason.is_some_and(|r| r.contains("not usable")));
    }

    /// A repo config that tries to reach outside the workspace is refused
    /// exactly as it was before the baseline existed — it does not get to
    /// *replace* the baseline, and the reason is reported rather than
    /// swallowed. The untrusted-repo gate runs before the rung is chosen.
    #[test]
    fn a_config_the_validator_refuses_does_not_become_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let sup = make(dir.path());
        let dc = dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();
        std::fs::write(
            dc.join("devcontainer.json"),
            r#"{"image": "img", "runArgs": ["--privileged", "--security-opt=label=disable"]}"#,
        )
        .unwrap();

        let resolved = sup.resolve_config().unwrap();
        assert_eq!(resolved.authority, ConfigAuthority::Baseline);
        let reason = resolved.reason.expect("a refusal is worth explaining");
        assert!(reason.contains("refused"), "{reason}");
        assert!(reason.contains("security-opt"), "{reason}");
    }

    /// The clone is read-only in the baseline and writable under the
    /// project's own config — on BOTH binds. The second bind exists so the
    /// agent's paths mean the same thing on both sides; if it kept `rw`
    /// while the first went `ro`, it would simply be the way around it.
    #[test]
    fn the_baseline_mounts_the_checkout_read_only_on_every_bind() {
        let dir = tempfile::tempdir().unwrap();
        let sup = make(dir.path());
        let config =
            crate::baseline::ensure_baseline_config_in(&tempfile::tempdir().unwrap().keep())
                .unwrap();

        let baseline = sup.ide_mounts(&config, ConfigAuthority::Baseline).join(" ");
        let host_path = dir.path().display().to_string();
        assert!(
            baseline.contains(&format!("{host_path}:{host_path}:ro,Z")),
            "the host-path bind must be read-only under the baseline: {baseline}"
        );

        // ...except the config the agent is there to author, which is
        // bound writable over it at both container paths.
        let workdir = config.workspace_folder().to_string();
        assert!(
            baseline.contains(&format!(
                "{host_path}/.devcontainer:{workdir}/.devcontainer:Z"
            )),
            "the config directory must be writable at the workdir: {baseline}"
        );
        assert!(
            baseline.contains(&format!(
                "{host_path}/.devcontainer:{host_path}/.devcontainer:Z"
            )),
            "the config directory must be writable at the host path: {baseline}"
        );

        // And the project's own config keeps the workspace writable, with
        // no config bind — the checkout is already writable whole.
        let project = sup.ide_mounts(&config, ConfigAuthority::Project).join(" ");
        assert!(
            project.contains(&format!("{host_path}:{host_path}:Z")) && !project.contains("ro,Z"),
            "container mode is writable: {project}"
        );
        assert!(!project.contains("/.devcontainer:"), "{project}");

        // Both flag sets are hashed, so a container started under one
        // authority reads as stale the moment the other is resolved —
        // which is what makes the mode change survive a reload.
        assert_ne!(
            config_hash(&config, &sup.ide_mounts(&config, ConfigAuthority::Baseline)).unwrap(),
            config_hash(&config, &sup.ide_mounts(&config, ConfigAuthority::Project)).unwrap(),
        );
    }

    /// A project image that would not build or pull is passed over for the
    /// baseline, with podman's reason, until the config builds a different
    /// image — which is a new attempt.
    #[test]
    fn a_failed_image_is_passed_over_until_the_config_moves() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("devcontainer.json");
        std::fs::write(&config_path, r#"{"image": "example.invalid/php:nope"}"#).unwrap();
        let sup = make(dir.path());
        let (authority, _) = sup.resolve_authority();
        assert_eq!(authority, ConfigAuthority::Project);

        let config = DevcontainerConfig::discover(dir.path()).unwrap().unwrap();
        sup.remember_build_failure(
            &config,
            &anyhow::anyhow!("podman pull failed: exit status: 125"),
        );
        let (authority, reason) = sup.resolve_authority();
        assert_eq!(authority, ConfigAuthority::Baseline);
        assert!(
            reason
                .as_deref()
                .is_some_and(|r| r.contains("exit status: 125")),
            "{reason:?}"
        );

        // Same files, still passed over; any edit — here one that leaves
        // the image alone — is a new attempt.
        assert_eq!(sup.resolve_authority().0, ConfigAuthority::Baseline);
        std::fs::write(
            &config_path,
            r#"{"image": "example.invalid/php:nope", "forwardPorts": [8000]}"#,
        )
        .unwrap();
        assert_eq!(sup.resolve_authority().0, ConfigAuthority::Project);
        assert!(sup.build_failed.lock().unwrap().is_none());
    }

    /// A non-root container user gets the host user mapped onto it; root,
    /// or a config with its own userns, gets nothing added.
    #[test]
    fn the_host_user_is_mapped_onto_a_non_root_container_user() {
        assert_eq!(
            keep_id_flag(&[], 1000, 1000).as_deref(),
            Some("--userns=keep-id:uid=1000,gid=1000")
        );
        assert_eq!(keep_id_flag(&[], 0, 0), None);
        assert_eq!(
            keep_id_flag(&["--userns=keep-id".to_string()], 1000, 1000),
            None
        );
        assert_eq!(
            keep_id_flag(
                &["--init".to_string(), "--userns=host".to_string()],
                1000,
                1000
            ),
            None
        );
    }

    /// The agent's two invariants must hold in the baseline exactly as they
    /// do in a project devcontainer, or `session/load` loses the
    /// conversation the moment a user repairs their config and reloads.
    /// This is the property the whole relocation design rests on.
    #[test]
    fn the_baseline_preserves_the_cwd_and_home_invariants() {
        let dir = tempfile::tempdir().unwrap();
        let sup = make(dir.path());
        let config =
            crate::baseline::ensure_baseline_config_in(&tempfile::tempdir().unwrap().keep())
                .unwrap();
        let host_path = dir.path().display().to_string();

        for authority in [ConfigAuthority::Project, ConfigAuthority::Baseline] {
            let mounts = sup.ide_mounts(&config, authority).join(" ");
            // cwd: the checkout at its REAL host path, both topologies.
            assert!(
                mounts.contains(&format!("{host_path}:{host_path}")),
                "{authority:?} must bind the checkout at its host path: {mounts}"
            );
            // HOME: this environment's own volume, at the one path.
            assert!(
                mounts.contains(&format!(
                    "{}:{}",
                    environment::env_home_volume(dir.path(), &EnvironmentId::primary()),
                    taste_core::policy::AGENT_HOME_IN_DEVCONTAINER
                )),
                "{authority:?} must mount this environment's agent home: {mounts}"
            );
        }
    }

    #[test]
    fn recheck_walks_noconfig_to_configdetected() {
        let dir = tempfile::tempdir().unwrap();
        let sup = make(dir.path());
        sup.recheck().unwrap();
        assert_eq!(sup.state(), SupervisorState::NoConfig);

        write_config(dir.path());
        sup.recheck().unwrap();
        assert_eq!(sup.state(), SupervisorState::ConfigDetected);
        assert!(!sup.pending_changes());
    }

    /// Staging is what makes the context ours: a directory validated at
    /// parse can be a symlink by the time podman reads it, and the config
    /// scope is the one thing an agent may write in either mode.
    #[test]
    fn staging_copies_the_context_and_refuses_symlinks() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("Containerfile"), "FROM scratch\n").unwrap();
        std::fs::create_dir(source.path().join("scripts")).unwrap();
        std::fs::write(source.path().join("scripts/setup.sh"), "echo hi\n").unwrap();

        let staged = stage_build_context(source.path(), "taste-test-ctx").unwrap();
        assert_eq!(
            std::fs::read_to_string(staged.join("Containerfile")).unwrap(),
            "FROM scratch\n"
        );
        assert!(
            staged.join("scripts/setup.sh").is_file(),
            "nested files come too"
        );

        // The escape a lexical check misses, and the reason staging exists.
        std::os::unix::fs::symlink("/etc", source.path().join("escape")).unwrap();
        let error = stage_build_context(source.path(), "taste-test-ctx")
            .unwrap_err()
            .to_string();
        assert!(error.contains("symlinks are not allowed"), "{error}");

        let _ = std::fs::remove_dir_all(staging_root().join("taste-test-ctx"));
    }

    /// Restaged every build: yesterday files must not ride along into
    /// today image.
    #[test]
    fn staging_is_fresh_each_time() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("Containerfile"), "FROM scratch\n").unwrap();
        let staged = stage_build_context(source.path(), "taste-test-fresh").unwrap();
        std::fs::write(staged.join("leftover"), "stale").unwrap();
        let staged = stage_build_context(source.path(), "taste-test-fresh").unwrap();
        assert!(!staged.join("leftover").exists());
        let _ = std::fs::remove_dir_all(staging_root().join("taste-test-fresh"));
    }

    /// Everything a relocated agent reaches the IDE through has to be in
    /// the container, and each of these is load-bearing: the checkout at
    /// its HOST path (so the adapter's history key does not move), the
    /// per-environment home volume (so the history survives a rebuild),
    /// the MCP socket (so it has tools), and the auth proxy socket (so it
    /// can pay for a turn from inside its own network namespace).
    #[test]
    fn the_ide_mounts_carry_everything_a_relocated_agent_needs() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path());
        let sup = make(dir.path());
        let config = DevcontainerConfig::discover(dir.path()).unwrap().unwrap();
        let mounts = sup.ide_mounts(&config, ConfigAuthority::Project).join(" ");

        let root = dir.path().display().to_string();
        assert!(mounts.contains(&format!("{root}:{root}:Z")), "{mounts}");
        assert!(
            mounts.contains(&format!(
                "{}:{}",
                environment::env_home_volume(dir.path(), sup.id()),
                taste_core::policy::AGENT_HOME_IN_DEVCONTAINER
            )),
            "{mounts}"
        );
        // And nothing else. The repo's own container gets ONE host path —
        // its checkout — plus volumes the IDE named. No IDE socket rides in
        // any more: a confined container may not dial one the unconfined
        // IDE bound, so the endpoints moved inside (see `crate::channel`),
        // and a mount that cannot be used is a mount that should not exist.
        assert!(!mounts.contains(".sock"), "{mounts}");
    }

    /// The hash covers the IDE's own mounts, so changing the set makes every
    /// running container stale by itself rather than by anyone remembering
    /// to say so — which is what carries containers across the move to the
    /// environment channel.
    #[test]
    fn the_ide_mounts_are_hashed_like_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path());
        let sup = make(dir.path());
        let config = DevcontainerConfig::discover(dir.path()).unwrap().unwrap();
        let full =
            config_hash(&config, &sup.ide_mounts(&config, ConfigAuthority::Project)).unwrap();
        let without: Vec<String> = sup
            .ide_mounts(&config, ConfigAuthority::Project)
            .into_iter()
            .filter(|m| !m.contains(&environment::env_home_volume(dir.path(), sup.id())))
            .collect();
        assert_ne!(full, config_hash(&config, &without).unwrap());
    }

    /// Relocation needs somewhere to point an agent, and there is nowhere
    /// until a container is up and its helper has bound. Guessing an
    /// address here would produce the failure this whole batch exists to
    /// remove: an agent that starts fine and has no tools.
    #[test]
    fn there_is_no_channel_address_without_a_channel() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path());
        let sup = make(dir.path());
        assert_eq!(sup.channel_paths(), None);
    }

    /// ...and the channel cannot even be attempted without a container, or
    /// without the IDE having said what it serves.
    #[tokio::test]
    async fn a_channel_needs_a_container_and_something_to_serve() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path());
        let sup = make(dir.path());
        let refusal = sup.ensure_channel().await.err().unwrap().to_string();
        assert!(refusal.contains("no container running"), "{refusal}");

        sup.set_state(SupervisorState::Running {
            container_id: "deadbeef".into(),
        });
        let refusal = sup.ensure_channel().await.err().unwrap().to_string();
        assert!(refusal.contains("has not wired"), "{refusal}");
    }

    /// ...but the start-time probe is not who that check is for.
    ///
    /// [`Supervisor::start`] probes the container it has just started
    /// *before* publishing `Running`, so a channel that asks the published
    /// state refuses the very container being probed. That refusal was not
    /// a transient miss: `probe_container` latches it as
    /// `AgentHosting::No` for the life of the container, and the reason it
    /// carries — "cannot reach the IDE through its environment channel
    /// (environment X has no container running)" — then surfaces in the
    /// chat as "agent not relocated", about an environment that is up.
    #[tokio::test]
    async fn the_start_time_probe_is_not_refused_for_being_unannounced() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path());
        let sup = make(dir.path());
        // Exactly `start`'s position: the container is up, and its state
        // has not been announced yet.
        assert!(!matches!(sup.state(), SupervisorState::Running { .. }));
        let refusal = sup.open_channel().await.err().unwrap().to_string();
        assert!(
            !refusal.contains("no container running"),
            "the start-time probe must not refuse its own container: {refusal}"
        );
        // It gets as far as the next real question, which is as far as a
        // test without podman can follow it.
        assert!(refusal.contains("has not wired"), "{refusal}");
    }

    /// Relocation is never assumed: until the container is asked, and
    /// whenever there is no container to ask, the answer is `Unknown` and a
    /// chat keeps the outside-confined topology.
    #[test]
    fn agent_hosting_starts_unknown_and_is_forgotten_with_the_container() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path());
        let sup = make(dir.path());
        assert_eq!(sup.agent_hosting(), AgentHosting::Unknown);

        *sup.hosting.lock().unwrap() = AgentHosting::Yes;
        sup.forget_agent_hosting();
        assert_eq!(
            sup.agent_hosting(),
            AgentHosting::Unknown,
            "a container that is gone hosts nothing, and cannot be assumed to again"
        );
    }

    /// Nothing to exec into: the probe answers `Unknown` rather than
    /// running a podman command against a container that is not there.
    #[tokio::test]
    async fn probing_a_stopped_environment_asks_podman_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path());
        let sup = make(dir.path());
        assert_eq!(sup.probe_agent_hosting().await, AgentHosting::Unknown);
    }

    #[test]
    fn drift_while_running_raises_pending() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path());
        let sup = make(dir.path());

        // Simulate a running container recorded at the current hash.
        let config = DevcontainerConfig::discover(dir.path()).unwrap().unwrap();
        *sup.running_hash.lock().unwrap() =
            Some(config_hash(&config, &sup.ide_mounts(&config, ConfigAuthority::Project)).unwrap());
        sup.set_state(SupervisorState::Running {
            container_id: "x".into(),
        });
        sup.recheck().unwrap();
        assert!(!sup.pending_changes());

        std::fs::write(
            dir.path().join(".devcontainer/devcontainer.json"),
            r#"{"image": "registry.example/img:2"}"#,
        )
        .unwrap();
        sup.recheck().unwrap();
        assert!(sup.pending_changes());
    }

    #[tokio::test]
    async fn remove_volume_refuses_unreferenced_volumes() {
        let dir = tempfile::tempdir().unwrap();
        let dc = dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();
        std::fs::write(
            dc.join("devcontainer.json"),
            r#"{"image": "img", "mounts": ["source=my-cache,target=/c,type=volume"]}"#,
        )
        .unwrap();
        let sup = make(dir.path());
        // Not this environment's: refused before podman is ever invoked.
        let err = sup.remove_volume("some-other-volume").await.unwrap_err();
        assert!(err.to_string().contains("does not belong"), "{err}");
        // Nor the name the config literally declares — that string is not
        // what podman ends up holding.
        let err = sup.remove_volume("my-cache").await.unwrap_err();
        assert!(err.to_string().contains("does not belong"), "{err}");
        // The namespaced form is this environment's, and is offered.
        let owned = sup.namespaced_volume("my-cache");
        assert!(
            sup.env_volumes().contains(&owned),
            "{:?}",
            sup.env_volumes()
        );
    }

    /// The new contract, replacing `container_name_is_stable_per_workspace`:
    /// a name is stable per workspace AND environment, and two environments
    /// of one workspace never collide.
    #[test]
    fn container_names_are_stable_per_workspace_and_environment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let primary = || make(root).container_name();
        assert_eq!(primary(), primary(), "stable across supervisors");
        assert!(primary().starts_with("taste-"));

        let review = make_env(root, EnvironmentIdentity::cloned(root, env("review")));
        assert_ne!(primary(), review.container_name());
        assert!(review.container_name().ends_with("-review"));

        // Same environment slug, different workspace: still distinct.
        let other = tempfile::tempdir().unwrap();
        assert_ne!(
            review.container_name(),
            make_env(
                other.path(),
                EnvironmentIdentity::cloned(other.path(), env("review"))
            )
            .container_name()
        );
    }

    /// A non-primary environment is rooted at its clone, and every podman
    /// name it derives keys off the WORKSPACE — otherwise two environments
    /// of one workspace would look like two unrelated workspaces and the
    /// fleet could never be enumerated.
    #[test]
    fn a_cloned_environment_roots_at_its_clone_but_keys_off_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let review = make_env(root, EnvironmentIdentity::cloned(root, env("review")));
        assert_ne!(
            review.checkout().path(),
            root,
            "the clone, not the main checkout"
        );
        assert!(review.checkout().path().ends_with("review/repo"));
        assert_eq!(review.peer(), review.checkout().path());
        assert_eq!(review.workspace_root(), root);
        assert_eq!(
            review.workspace_key(),
            taste_core::environment::workspace_key(root)
        );
    }

    /// Two environments of one workspace must not silently share the
    /// repo's declared caches, but must share its IMAGE.
    #[test]
    fn environments_split_volumes_and_share_images() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dc = root.join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();
        std::fs::write(
            dc.join("devcontainer.json"),
            r#"{"build": {"dockerfile": "Containerfile"},
                "mounts": ["source=cargo,target=/c,type=volume"]}"#,
        )
        .unwrap();
        std::fs::write(dc.join("Containerfile"), "FROM scratch\n").unwrap();

        let a = make(root);
        let b = make_env(root, EnvironmentIdentity::cloned(root, env("review")));
        let config = DevcontainerConfig::discover(root).unwrap().unwrap();

        assert_eq!(
            a.image_tag(&config).unwrap(),
            b.image_tag(&config).unwrap(),
            "identical config must not mean two copies of one image"
        );
        assert!(a.image_tag(&config).unwrap().starts_with("taste-img-"));

        let (va, vb) = (a.env_volumes(), b.env_volumes());
        assert!(va.iter().all(|v| !vb.contains(v)), "{va:?} vs {vb:?}");
        assert!(va.iter().any(|v| v.ends_with("-cfg-cargo")));
        assert!(va.iter().any(|v| v.ends_with("-home")));
    }

    /// The mount string podman receives carries the namespaced volume, not
    /// the verbatim one the repo wrote — and bind mounts are untouched.
    #[test]
    fn declared_volume_mounts_are_namespaced_at_run_time() {
        let dir = tempfile::tempdir().unwrap();
        let sup = make(dir.path());
        let rewritten = sup.namespaced_mount("source=cargo,target=/c,type=volume");
        assert!(
            rewritten.contains(&sup.namespaced_volume("cargo")),
            "{rewritten}"
        );
        assert!(rewritten.ends_with(",target=/c,type=volume"), "{rewritten}");

        let bind = "type=bind,source=/etc/hosts,target=/etc/hosts";
        assert_eq!(sup.namespaced_mount(bind), bind);
    }

    /// Self-hosting means the IDE's own container IS the environment, and
    /// lifecycle operations belong to a host-side IDE. There is no socket
    /// forwarded in that could make it a sibling — by design.
    #[tokio::test]
    async fn self_hosted_lifecycle_is_refused_not_remoted() {
        let dir = tempfile::tempdir().unwrap();
        let inside = Supervisor::with_inside(
            EnvironmentIdentity::primary(dir.path()),
            EventBus::new(),
            ExecContext::host_unsandboxed_for_tests(),
            crate::substrate::Substrate::local_for_tests(),
            true,
        );
        let error = inside.reload().await.unwrap_err().to_string();
        assert!(error.contains("host-side IDE"), "{error}");
        assert!(inside.stop().await.is_err());
        assert!(inside.nuke().await.is_err());
    }
}

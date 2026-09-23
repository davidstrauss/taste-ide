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
/// Copy a small directory tree out of a files service onto this host.
/// Symlinks are skipped — the build stages from a copy that refuses them
/// anyway — and the depth is capped, because a `.devcontainer/` that goes
/// four levels deep is not one this reads.
fn mirror_tree(
    files: &taste_core::files::Files,
    from: &Path,
    to: &Path,
    depth: usize,
) -> Result<()> {
    use taste_core::files::Kind;
    const MAX_DEPTH: usize = 4;
    if depth > MAX_DEPTH {
        return Ok(());
    }
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in files
        .list(from)
        .with_context(|| format!("listing {} from {}", from.display(), files.describe()))?
    {
        let source = from.join(&entry.name);
        let target = to.join(&entry.name);
        match entry.kind {
            Kind::Dir => mirror_tree(files, &source, &target, depth + 1)?,
            Kind::File => {
                let bytes = files
                    .read(&source)
                    .with_context(|| format!("reading {}", source.display()))?;
                std::fs::write(&target, bytes)
                    .with_context(|| format!("writing {}", target.display()))?;
            }
            Kind::Symlink | Kind::Other => {}
        }
    }
    Ok(())
}

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

/// How a checkout is put where its containers can run — the registry's
/// placement, as a blocking call the supervisor can make.
pub type Placer = Arc<dyn Fn() -> Result<()> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorState {
    /// Getting the environment somewhere it can run: the workspace's VM
    /// coming up, its files service connecting, the checkout being placed
    /// in it. `what` is the step under way, in the row's words (David,
    /// 2026-09-21: "show what it's doing or waiting on in the mean time").
    /// Before any of it the state is `NoConfig`, which reads as "not
    /// configured" — a claim about the project that is not the fact.
    Preparing {
        what: String,
    },
    NoConfig,
    ConfigDetected,
    Building,
    Starting,
    Running {
        container_id: String,
    },
    Failed {
        message: String,
    },
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
            SupervisorState::Preparing { what } => {
                DevcontainerStateEvent::Preparing { what: what.clone() }
            }
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

/// How much of a failed build's log is kept for its repair: enough for a
/// failing step's own output and podman's error after it.
const FAILED_BUILD_LOG_LINES: usize = 120;

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
    /// The next image build is from nothing — base image pulled, no layer
    /// cache — which is how an image's packages are refreshed
    /// (`crate::migration`, `Kind::Packages`). Consumed by that build.
    fresh_build: AtomicBool,
    /// The next start skips the image build when its tag is already there:
    /// how an environment restarts on the image another environment just
    /// rebuilt from nothing, rather than building it again. Consumed by
    /// that start.
    reuse_image: AtomicBool,
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
    /// The failed build's own log, as it stood when it failed — before the
    /// baseline that stands in writes its build after it. What a repair is
    /// handed (David, 2026-09-23: "the prompt agent button will direct the
    /// agent to troubleshoot the *failed* build, right?"): the tail of the
    /// live log by then is the baseline's, and podman's error is lines
    /// above it.
    failed_build_log: Mutex<Option<Vec<String>>>,
    /// The lifecycle command that failed on the last start, when one did,
    /// with its exit. The container stays up and the environment runs —
    /// the command is the project's, the environment it ran in is real,
    /// and a failed `composer install` is fixed FROM that environment, not
    /// from outside it. Failing the environment instead left a usable
    /// container up with no exec target, and the agent that could have
    /// fixed the command outside any container, blind (David, 2026-09-16:
    /// "So friggin tired of these read/write errors").
    hook_failure: Mutex<Option<String>>,
    /// What the project needs that its environment turned out not to have,
    /// found by probing the container after its start
    /// (`Supervisor::probe_capabilities`): each a gap and its remedy.
    capability_gaps: Mutex<Vec<String>>,
    /// Paths the folder and the checkout both changed, differently
    /// (`sync_primary_blocking`); the primary's only.
    folder_conflicts: Mutex<Vec<std::path::PathBuf>>,
    /// The watch on the user's folder (`watch_folder`), held for its life.
    folder_watch: Mutex<Option<notify::RecommendedWatcher>>,
    /// Held for the whole of a primary sync, fetch to resnapshot: one at a
    /// time. A sync starts from half a dozen places (the folder watch, the
    /// keeper watch, the chat cadence, the file tree, a publish, a close),
    /// and two at once raced on the folder's ref locks and index, and could
    /// send an older version of a file after a newer one (review,
    /// 2026-09-23).
    folder_sync: Mutex<()>,
    pending: AtomicBool,
    logs: Mutex<VecDeque<String>>,
    /// What the container itself wrote (`podman logs`), ring-buffered like
    /// the build log, and followed for as long as the container runs.
    container_logs: Arc<Mutex<VecDeque<String>>>,
    /// The `podman logs --follow` task, alive exactly while the state is
    /// `Running`. Aborting it drops the child, which is killed with it.
    /// The runtime log's followers while the container runs: `podman logs`
    /// and `podman events`, aborted together when it stops.
    log_follower: Mutex<Vec<tokio::task::AbortHandle>>,
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
    /// Where the working copy is now. Starts as the identity's and moves
    /// once: the primary's, when the registry places it in the VM.
    checkout: Mutex<Checkout>,
    /// How this environment's checkout is put where its containers can
    /// run, when it is not there yet — the registry's placement, handed to
    /// the primary. A start that finds the checkout unhosted runs it first
    /// rather than refusing, so Rebuild pressed while the VM is still
    /// coming up does what the user meant once it is up.
    placer: Mutex<Option<Placer>>,
    /// The grant the running container was started under, for the
    /// Resources row; `None` until one has been.
    applied_grant: Mutex<Option<crate::config::Grant>>,
    /// How this environment's files are reached when its checkout is in a
    /// VM: the keeper for that VM, once the registry has connected it.
    /// `None` until then, and always for a local checkout, whose files are
    /// simply this host's ([`Self::files`]).
    files: Mutex<Option<taste_core::files::Files>>,
    /// The keeper's watch on a remote checkout's tree, which is what drives
    /// config rechecks there in place of inotify. Dropped with the
    /// supervisor.
    remote_watch: Mutex<Option<crate::keeper::WatchHandle>>,
    /// A recheck the watch has asked for and not yet run: several events
    /// in a burst make one recheck.
    recheck_pending: Arc<AtomicBool>,
    /// A ref moved in the checkout in the VM — a commit, by whoever made
    /// it — and a snapshot is due once the writes settle.
    snapshot_pending: Arc<AtomicBool>,
    /// The ssh process forwarding a remote container's published ports to
    /// this host's loopback, while the container runs.
    tunnel: Mutex<Option<std::process::Child>>,
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
///
/// **`Z` on this host, `z` in a VM.** `Z` gives the bind a label private to
/// this one container, which on the host is what keeps one environment's
/// checkout out of another container's reach. In a VM it broke the design:
/// the keeper (`crate::keeper`) is a different container mounting the same
/// checkout, and once the environment's container had relabelled the files
/// for itself the keeper could neither read nor remove them (the first
/// live run's `destroy` left the checkout behind, 2026-09-21). A VM serves
/// one workspace, and every container in it is the same principal, so the
/// shared label is the honest one there.
/// A repo-supplied `--mount` spec with its private SELinux label made
/// shared: the bare `Z` option, or podman's long form `relabel=private`.
/// Everything else in the spec is left as written.
fn share_label_in_vm(mount: &str) -> String {
    mount
        .split(',')
        .map(|option| match option.trim() {
            "Z" => "z",
            "relabel=private" => "relabel=shared",
            _ => option,
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn workspace_bind_flags(authority: ConfigAuthority, shared_label: bool) -> &'static str {
    match (authority, shared_label) {
        (ConfigAuthority::Project, false) => "Z",
        (ConfigAuthority::Baseline, false) => "ro,Z",
        (ConfigAuthority::Project, true) => "z",
        (ConfigAuthority::Baseline, true) => "ro,z",
    }
}

/// How a container stands in its VM's queue when the VM is contended
/// (David, 2026-09-21: "My primary env should also get first priority
/// for any resource access"). The primary weighs four times an agent
/// environment for CPU (`cpu.weight`, through `--cpu-shares`) and has its
/// whole grant as a soft floor for memory (`memory.low`, through
/// `--memory-reservation`), so under pressure the agents' pages are
/// reclaimed first; an agent environment is the first the guest's OOM
/// killer reaches for. Each is a cgroup v2 knob rootless podman may set
/// in the guest: raising a process's OOM score needs no privilege, and
/// the cpu and memory controllers are delegated to `core`.
fn priority_args(primary: bool, grant: crate::config::Grant) -> Vec<String> {
    if primary {
        vec![
            "--cpu-shares".into(),
            "1024".into(),
            "--memory-reservation".into(),
            format!("{}m", grant.memory_mib),
        ]
    } else {
        vec![
            "--cpu-shares".into(),
            "256".into(),
            "--oom-score-adj".into(),
            "500".into(),
        ]
    }
}

/// The keeper's raw watch events for the primary's tree, turned into the
/// bus events the panes already subscribe to, with a quarter second of
/// debounce so a build writing a hundred files is one refresh.
struct TreeEvents {
    tx: std::sync::mpsc::Sender<(String, String)>,
}

impl TreeEvents {
    /// Directories whose churn is machine noise (`taste_core::watcher`'s
    /// list, applied to the same paths over there).
    const NOISE: [&'static str; 4] = [
        "target",
        "node_modules",
        ".flatpak-builder",
        "build-aux/flatpak/.build",
    ];

    fn start(events: EventBus, root: PathBuf) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<(String, String)>();
        std::thread::Builder::new()
            .name("taste-keeper-tree-events".into())
            .spawn(move || {
                let debounce = std::time::Duration::from_millis(250);
                while let Ok(first) = rx.recv() {
                    let mut batch = vec![first];
                    let deadline = std::time::Instant::now() + debounce;
                    while let Ok(more) = rx
                        .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                    {
                        batch.push(more);
                    }
                    let mut git = false;
                    let mut tree = false;
                    let mut changed: Vec<PathBuf> = Vec::new();
                    for (event, name) in batch {
                        if name == ".git" || name.starts_with(".git/") {
                            git = true;
                            continue;
                        }
                        if Self::NOISE.iter().any(|noise| name.starts_with(noise)) {
                            continue;
                        }
                        // node says `rename` for a file made, removed, or
                        // renamed, and `change` for one written to.
                        if event == "rename" {
                            tree = true;
                        }
                        let path = root.join(&name);
                        if !changed.contains(&path) {
                            changed.push(path);
                        }
                    }
                    if git {
                        events.publish(Event::GitStatusChanged);
                    }
                    for path in changed {
                        events.publish(Event::FileChanged(path));
                    }
                    if tree {
                        events.publish(Event::FileTreeChanged);
                    }
                }
            })
            .expect("spawning the tree events thread");
        Self { tx }
    }

    fn saw(&self, event: &str, name: &str) {
        let _ = self.tx.send((event.to_string(), name.to_string()));
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        // The ssh forwarding a remote container's ports has no other owner.
        if let Some(mut child) = self.tunnel.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
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
        let checkout = env.checkout.clone();
        Arc::new(Self {
            env,
            events,
            exec,
            state: Mutex::new(SupervisorState::NoConfig),
            authority: Mutex::new(ConfigAuthority::Project),
            hosting: Mutex::new(AgentHosting::Unknown),
            agent_reload: AtomicBool::new(false),
            fresh_build: AtomicBool::new(false),
            reuse_image: AtomicBool::new(false),
            channel: tokio::sync::Mutex::new(None),
            channel_services: Mutex::new(None),
            running_hash: Mutex::new(None),
            declared_ports: Mutex::new(Vec::new()),
            published_ports: Mutex::new(std::collections::HashMap::new()),
            passed_over: Mutex::new(None),
            build_failed: Mutex::new(None),
            failed_build_log: Mutex::new(None),
            hook_failure: Mutex::new(None),
            capability_gaps: Mutex::new(Vec::new()),
            folder_conflicts: Mutex::new(Vec::new()),
            folder_watch: Mutex::new(None),
            folder_sync: Mutex::new(()),
            pending: AtomicBool::new(false),
            logs: Mutex::new(VecDeque::new()),
            container_logs: Arc::new(Mutex::new(VecDeque::new())),
            log_follower: Mutex::new(Vec::new()),
            config_watch: Mutex::new(None),
            lifecycle: tokio::sync::Mutex::new(()),
            checkout: Mutex::new(checkout),
            placer: Mutex::new(None),
            applied_grant: Mutex::new(None),
            substrate: Mutex::new(substrate),
            files: Mutex::new(None),
            remote_watch: Mutex::new(None),
            recheck_pending: Arc::new(AtomicBool::new(false)),
            snapshot_pending: Arc::new(AtomicBool::new(false)),
            tunnel: Mutex::new(None),
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
    pub fn checkout(&self) -> Checkout {
        self.checkout.lock().unwrap().clone()
    }

    /// The registry's: the working copy is somewhere else now. The primary
    /// moves once, into the workspace's VM; the container it starts next
    /// binds the checkout where it is now.
    pub fn set_checkout(&self, checkout: Checkout) {
        *self.checkout.lock().unwrap() = checkout;
    }

    /// The registry's: say what is being done to get this environment
    /// somewhere it can run — the VM, the files service, the placement —
    /// while nothing else is known about it. Never over a state that
    /// knows more: a container that is running, or a start that failed,
    /// keeps its word.
    pub fn announce_preparing(&self, what: &str) {
        if matches!(
            self.state(),
            SupervisorState::NoConfig | SupervisorState::Preparing { .. }
        ) {
            self.set_state(SupervisorState::Preparing {
                what: what.to_string(),
            });
        }
    }

    /// The registry's: how to place this checkout where it can run
    /// (`EnvironmentRegistry::place_primary_now`). Blocking; run off the
    /// reactor by the start that needs it.
    pub fn set_placer(&self, placer: Placer) {
        *self.placer.lock().unwrap() = Some(placer);
    }

    /// What this environment's container is granted under `config` with
    /// `authority`: the baseline's fixed grant, or the project config's
    /// `hostRequirements` with the default for what it leaves unsaid.
    fn grant_for(
        &self,
        config: &DevcontainerConfig,
        authority: ConfigAuthority,
    ) -> crate::config::Grant {
        match authority {
            ConfigAuthority::Baseline => crate::config::Grant::BASELINE,
            ConfigAuthority::Project => config.grant(),
        }
    }

    /// What placing this environment costs a VM: its project config's
    /// grant when it has one, the baseline's otherwise. Read off the
    /// config where it is (the mirror, for a checkout in a VM).
    pub fn grant(&self) -> crate::config::Grant {
        match DevcontainerConfig::discover(&self.config_root()) {
            Ok(Some(config)) => config.grant(),
            _ => crate::config::Grant::BASELINE,
        }
    }

    /// Refuse to start, with the reason in the state the row and the
    /// banner read, and as the error the caller gets.
    fn refuse_start(&self, message: String) -> anyhow::Error {
        self.log(format!("refusing to start: {message}"));
        self.set_state(SupervisorState::Failed {
            message: message.clone(),
        });
        self.set_pending(false);
        anyhow::anyhow!("{message}")
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
        match self.checkout() {
            Checkout::Local(path) => path,
            Checkout::Remote { .. } => {
                environment::env_dir(&self.env.workspace_root, &self.env.id).join("config")
            }
        }
    }

    /// How this environment's files are reached: this host's filesystem
    /// for a local checkout, the VM's keeper for a remote one — or, until
    /// the registry has connected that keeper, a service that refuses
    /// every call with the reason, so a read of a path that is not on this
    /// host says where the files are rather than "no such file".
    pub fn files(&self) -> taste_core::files::Files {
        match self.checkout() {
            Checkout::Local(_) => taste_core::files::Files::Local,
            Checkout::Remote { vm, .. } => {
                self.files.lock().unwrap().clone().unwrap_or_else(|| {
                    taste_core::files::Files::unavailable(format!(
                        "the files service for VM {vm} is not connected"
                    ))
                })
            }
        }
    }

    /// The registry's: connect this environment's files to its VM's
    /// keeper, and let the keeper's watch on the checkout drive config
    /// rechecks — the job inotify does for a checkout on this host.
    pub fn set_keeper(self: &Arc<Self>, keeper: Arc<crate::keeper::Keeper>) {
        *self.files.lock().unwrap() = Some(taste_core::files::Files::Remote(keeper.clone()));
        let Checkout::Remote { path, .. } = self.checkout() else {
            return;
        };
        let weak = Arc::downgrade(self);
        let pending = self.recheck_pending.clone();
        let snapshot_pending = self.snapshot_pending.clone();
        let primary = self.env.id.is_primary();
        let folder_events = self.events.clone();
        if primary {
            self.watch_folder();
        }
        // The primary's tree is what the panes show, so its changes over
        // there become the events inotify would have raised here — the
        // editor reloads, the tree restyles, the dots move. Debounced the
        // way `taste_core::watcher` debounces, and never for an agent
        // environment's clone, whose churn belongs to no pane.
        let tree_events = self
            .env
            .id
            .is_primary()
            .then(|| TreeEvents::start(self.events.clone(), path.clone()));
        let watch = keeper.watch(&path, move |event, name| {
            if let Some(tree) = &tree_events {
                tree.saw(&event, &name);
            }
            // A ref moved — HEAD, a branch, the packed refs: a commit, a
            // checkout, a reset, by the tree, the agent, or a shell. The
            // snapshot and the peer sync follow two seconds later, once
            // git has finished its several writes (David, 2026-09-21:
            // "Snapshot on commit, too"), so the folder has the commit and
            // the ref that restores the working copy as it stands after it.
            let ref_moved = name == ".git/HEAD"
                || name == ".git/packed-refs"
                || name.starts_with(".git/refs/heads/");
            // And for the primary, any change to its working tree: the
            // folder mirrors it (`taste_git::mirror`), so a saved file or
            // an agent's edit reaches the folder two seconds after the
            // last write of a burst (David, 2026-09-23: "keep my local
            // checkout/working copy updated from the VM data"). Git's own
            // files are not the working tree, and a build's output churns
            // without being anything git would show.
            let worktree_changed =
                primary && !name.starts_with(".git/") && name != ".git" && !churn_path(&name);
            if worktree_changed && !snapshot_pending.load(Ordering::SeqCst) {
                folder_events.publish(Event::FolderSync(taste_core::FolderSync::Pending));
            }
            if (ref_moved || worktree_changed) && !snapshot_pending.swap(true, Ordering::SeqCst) {
                let weak = weak.clone();
                let snapshot_pending = snapshot_pending.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    snapshot_pending.store(false, Ordering::SeqCst);
                    if let Some(supervisor) = weak.upgrade() {
                        if let Err(e) = supervisor.snapshot_blocking() {
                            tracing::warn!("snapshot after a ref moved in the VM: {e:#}");
                        }
                        if let Err(e) = supervisor.sync_peer_blocking() {
                            tracing::warn!("peer sync after a ref moved in the VM: {e:#}");
                        }
                    }
                });
            }
            // The config: a burst of edits under .devcontainer/ is one
            // recheck, a quarter second after the first, on a thread of
            // its own — never on the watch's, which must keep draining.
            if !name.starts_with(".devcontainer") {
                return;
            }
            if pending.swap(true, Ordering::SeqCst) {
                return;
            }
            let weak = weak.clone();
            let pending = pending.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(250));
                pending.store(false, Ordering::SeqCst);
                if let Some(supervisor) = weak.upgrade() {
                    if let Err(e) = supervisor.recheck() {
                        tracing::warn!("recheck after a change in the VM failed: {e:#}");
                    }
                }
            });
        });
        match watch {
            Ok(handle) => *self.remote_watch.lock().unwrap() = Some(handle),
            Err(e) => self.log(format!(
                "the checkout in the VM cannot be watched ({e}); config changes there are \
                 noticed on the IDE's own cadence"
            )),
        }
    }

    /// Test seam: the files without a keeper to watch with.
    #[doc(hidden)]
    pub fn set_files(&self, files: taste_core::files::Files) {
        *self.files.lock().unwrap() = Some(files);
    }

    /// A remote container's published ports live on the VM's loopback;
    /// this brings them to this host's, where the port tab, the browser
    /// face, and the user's own tools expect them. One ssh per container,
    /// started when it runs and ended when it stops; nothing for a local
    /// checkout, whose ports were published here in the first place.
    fn sync_tunnel(&self, state: &SupervisorState) {
        if self.checkout().is_local() {
            return;
        }
        if !matches!(state, SupervisorState::Running { .. }) {
            self.stop_tunnel();
            return;
        }
        let forwards: Vec<u16> = {
            let mut hosts: Vec<u16> = self
                .published_ports
                .lock()
                .unwrap()
                .values()
                .copied()
                .collect();
            hosts.sort_unstable();
            hosts
        };
        self.stop_tunnel();
        let Some(vm) = self.substrate().vm_details().cloned() else {
            return;
        };
        let keys = crate::keys::Keys::for_workspace(&self.env.workspace_root);
        // The counters beside the forward: bytes through each published
        // port, both ways, counted in the guest (`crate::ports`). Off this
        // thread — it is an ssh — and removed with the ports.
        {
            let keys = keys.clone();
            let vm = vm.clone();
            let env = self.env.id.clone();
            let forwards = forwards.clone();
            std::thread::Builder::new()
                .name("taste-port-counters".into())
                .spawn(move || {
                    if let Err(e) = crate::ports::sync_counters(&keys, &vm, &env, &forwards) {
                        tracing::warn!("port counters for {env} in VM {}: {e:#}", vm.domain);
                    }
                })
                .ok();
        }
        if forwards.is_empty() {
            return;
        }
        let (program, args) = keys.ssh_tunnel_argv(vm.ssh_port, &forwards);
        match std::process::Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => {
                *self.tunnel.lock().unwrap() = Some(child);
                self.log(format!(
                    "forwarding localhost:{} from VM {}",
                    forwards
                        .iter()
                        .map(u16::to_string)
                        .collect::<Vec<_>>()
                        .join(", localhost:"),
                    vm.domain
                ));
            }
            Err(e) => self.log(format!(
                "the ports published in VM {} could not be forwarded here: {e}",
                vm.domain
            )),
        }
    }

    fn stop_tunnel(&self) {
        if let Some(mut child) = self.tunnel.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Run a blocking files call from wherever this is. A local checkout's
    /// call is `std::fs` and runs in place, as it always did; a remote one
    /// is a round trip to the VM, which on a runtime worker is moved off
    /// the reactor first so the worker is not held for it.
    fn with_files<T>(&self, f: impl FnOnce(&taste_core::files::Files) -> T) -> T {
        let files = self.files();
        if files.is_local() {
            return f(&files);
        }
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| f(&files))
            }
            _ => f(&files),
        }
    }

    /// The SELinux label flag a bind of this checkout carries: private on
    /// this host, shared in a VM. See `workspace_bind_flags`.
    fn label_flag(&self) -> &'static str {
        if self.checkout().is_local() {
            "Z"
        } else {
            "z"
        }
    }

    /// Make a directory inside the checkout, wherever the checkout is.
    fn make_dir_in_checkout(&self, dir: &Path) -> std::io::Result<()> {
        self.with_files(|files| files.mkdir_all(dir))
    }

    /// Whether a path exists inside the checkout, wherever the checkout is.
    fn exists_in_checkout(&self, path: &Path) -> bool {
        self.with_files(|files| files.exists(path))
    }

    /// Bring the host-side mirror of a remote checkout's `.devcontainer/`
    /// (and `.devcontainer.json`) up to date, so every config read the
    /// supervisor makes — discovery, hashing, staging the build context —
    /// reads the same bytes the checkout has without knowing where the
    /// checkout is. A local checkout has no mirror and this does nothing.
    ///
    /// Whole, not incremental: the config directory is a handful of small
    /// files, and a mirror that could hold a file the checkout no longer
    /// has would be a config that could not be removed.
    pub fn refresh_config_mirror(&self) -> Result<()> {
        if self.checkout().is_local() {
            return Ok(());
        }
        let mirror = self.config_root();
        let root = self.checkout().path().to_path_buf();
        self.with_files(|files| -> Result<()> {
            let _ = std::fs::remove_dir_all(&mirror);
            std::fs::create_dir_all(&mirror)
                .with_context(|| format!("creating the config mirror {}", mirror.display()))?;
            let single = root.join(".devcontainer.json");
            if files.is_file(&single) {
                std::fs::write(mirror.join(".devcontainer.json"), files.read(&single)?)?;
            }
            let dir = root.join(".devcontainer");
            if files.is_dir(&dir) {
                mirror_tree(files, &dir, &mirror.join(".devcontainer"), 0)?;
            }
            Ok(())
        })
    }

    /// Walk the checkout for its footprint — on this host. A checkout in a
    /// VM is not walked from here, and comes back as an empty walk, which
    /// the fleet reports as unmeasured rather than as zero.
    async fn walk(&self, prune_ignored: bool) -> CheckoutWalk {
        let Some(checkout) = self.checkout().local_path().map(Path::to_path_buf) else {
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
        match self.checkout() {
            Checkout::Local(root) => {
                let Some(git) = taste_git::GitWorkspace::discover(&root) else {
                    return Ok(None);
                };
                git.snapshot_worktree(&name).map(Some)
            }
            // Over there, by the same definition spelled as git plumbing
            // (`taste_git::snapshot::script`), then the ref — and the
            // branches with it — fetched home to the peer, which is what
            // review and publish read.
            Checkout::Remote { vm, path } => {
                let files = self.files();
                let script = taste_git::snapshot::script(&name)?;
                let out = files
                    .exec(&path, &["sh".into(), "-c".into(), script])
                    .with_context(|| format!("snapshotting {} in VM {vm}", self.env.id))?;
                if !out.success() {
                    bail!(
                        "snapshotting {} in VM {vm}: {}",
                        self.env.id,
                        out.stderr_utf8().trim()
                    );
                }
                let snapshot = taste_git::snapshot::parse_script_output(&out.stdout_utf8())?;
                if snapshot.wrote {
                    self.sync_peer_blocking()?;
                }
                Ok(Some(snapshot))
            }
        }
    }

    /// Bring this environment's peer up to date with its checkout in the
    /// VM: the refs, and for the primary the folder's working tree too
    /// (`crate::peer::sync_primary_peer`). Nothing for a local checkout,
    /// which is its own repository. Blocking; the file tree runs it after
    /// every commit, switch, or rebase it makes over there, and the
    /// snapshot cadence after every snapshot that wrote.
    pub fn sync_peer_blocking(&self) -> Result<()> {
        let Checkout::Remote { vm, path } = self.checkout() else {
            return Ok(());
        };
        let vm_info = self
            .substrate()
            .vm_details()
            .cloned()
            .with_context(|| format!("{}'s substrate is not its VM {vm}", self.env.id))?;
        let keys = crate::keys::Keys::for_workspace(&self.env.workspace_root);
        // The primary's peer is the user's own folder, with branches of
        // its own; an agent environment's is refs only, and takes the
        // checkout's word whole.
        if self.env.id.is_primary() {
            let _one = self.folder_sync.lock().unwrap_or_else(|e| e.into_inner());
            self.sync_primary_blocking(&vm_info, &keys, &path, false, 0)?;
        } else {
            crate::peer::fetch_from_guest(
                &self.env.peer,
                &vm_info,
                &keys,
                &path,
                &crate::peer::PEER_REFSPECS,
            )?;
        }
        Ok(())
    }

    /// The primary's sync, and what follows from it: the folder's own
    /// changes, once sent, come back as agreement only after the checkout
    /// is snapshotted again, so that is done here (a pass or two, never a
    /// loop: `depth` stops it); a conflict is published for the window to
    /// ask about, and its going is published too.
    fn sync_primary_blocking(
        &self,
        vm: &crate::provision::Vm,
        keys: &crate::keys::Keys,
        path: &Path,
        force: bool,
        depth: u8,
    ) -> Result<()> {
        let events = self.events.clone();
        events.publish(Event::FolderSync(taste_core::FolderSync::Running {
            step: "Comparing your folder with Personal".into(),
            done: 0,
            total: 0,
        }));
        let on_send = {
            let events = events.clone();
            move |done: usize, total: usize, next: &Path| {
                events.publish(Event::FolderSync(taste_core::FolderSync::Running {
                    step: format!("Sending {} to Personal", next.display()),
                    done,
                    total,
                }));
            }
        };
        let sync = crate::peer::sync_primary_peer_with(
            &self.env.peer,
            vm,
            keys,
            &self.files(),
            path,
            force,
            &on_send,
        );
        if let Err(e) = &sync {
            events.publish(Event::FolderSync(taste_core::FolderSync::Failed {
                reason: format!("{e:#}"),
            }));
        }
        let sync = sync?;
        let had_conflict = !self.folder_conflicts.lock().unwrap().is_empty();
        if sync.conflicts.is_empty() == had_conflict {
            self.events.publish(Event::FolderConflict {
                paths: sync.conflicts.clone(),
            });
        }
        *self.folder_conflicts.lock().unwrap() = sync.conflicts.clone();
        events.publish(Event::FolderSync(taste_core::FolderSync::Done {
            summary: folder_sync_summary(&sync),
        }));
        if sync.sent > 0 && depth < 2 {
            self.log(format!(
                "sent {} change(s) from your folder to the checkout in the VM",
                sync.sent
            ));
            let name = taste_git::snapshot_ref(self.env.id.as_str());
            let script = taste_git::snapshot::script(&name)?;
            let out = self
                .files()
                .exec(path, &["sh".into(), "-c".into(), script])
                .context("snapshotting the checkout after sending the folder's changes")?;
            if !out.success() {
                bail!("snapshotting the checkout: {}", out.stderr_utf8().trim());
            }
            return self.sync_primary_blocking(vm, keys, path, false, depth + 1);
        }
        Ok(())
    }

    /// Watch the user's folder — the primary's peer, on this host — so a
    /// file changed here reaches the checkout (the mirror's other way): a
    /// sync two seconds after the last change of a burst. Reads are not
    /// changes, and the mirror reads every file it compares, so an access
    /// event would be the sync triggering itself (`configwatch` learnt the
    /// same thing at 96,000 events a second). The mirror's own writes do
    /// start one more sync, which finds the two sides agreeing and does
    /// nothing: quieting the watch for a moment after each sync, as it once
    /// did, also dropped a save the user made in that moment, which then
    /// waited for the next change anywhere (review, 2026-09-23). The quiet
    /// that remains is the burst's: the sync runs two seconds after its
    /// LAST event, so a checkout or a build writing for longer is not sent
    /// half-written.
    fn watch_folder(self: &Arc<Self>) {
        use notify::Watcher;
        let folder = self.env.peer.clone();
        let weak = Arc::downgrade(self);
        let pending = Arc::new(AtomicBool::new(false));
        let last = Arc::new(Mutex::new(std::time::Instant::now()));
        let root = folder.clone();
        let events = self.events.clone();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else { return };
            if matches!(event.kind, notify::EventKind::Access(_)) {
                return;
            }
            let relevant = event.paths.iter().any(|path| {
                path.strip_prefix(&root).is_ok_and(|rel| {
                    let rel = rel.to_string_lossy();
                    !rel.is_empty()
                        && rel != ".git"
                        && !rel.starts_with(".git/")
                        && !churn_path(&rel)
                })
            });
            if !relevant {
                return;
            }
            *last.lock().unwrap() = std::time::Instant::now();
            if pending.swap(true, Ordering::SeqCst) {
                return;
            }
            events.publish(Event::FolderSync(taste_core::FolderSync::Pending));
            let weak = weak.clone();
            let pending = pending.clone();
            let last = last.clone();
            std::thread::spawn(move || {
                let settle = std::time::Duration::from_secs(2);
                loop {
                    let since = last.lock().unwrap().elapsed();
                    if since >= settle {
                        break;
                    }
                    std::thread::sleep(settle - since);
                }
                pending.store(false, Ordering::SeqCst);
                if let Some(supervisor) = weak.upgrade() {
                    if let Err(e) = supervisor.sync_peer_blocking() {
                        tracing::warn!("sync after a change in your folder: {e:#}");
                    }
                }
            });
        });
        match watcher {
            Ok(mut watcher) => match watcher.watch(&folder, notify::RecursiveMode::Recursive) {
                Ok(()) => *self.folder_watch.lock().unwrap() = Some(watcher),
                Err(e) => tracing::warn!(
                    "watching {} for changes to send to the checkout: {:#}",
                    folder.display(),
                    taste_core::watcher::name_the_inotify_limit(e)
                ),
            },
            Err(e) => tracing::warn!("watching {}: {e}", folder.display()),
        }
    }

    /// The paths the folder and the checkout both changed, differently,
    /// as the last sync found them; empty when they agree.
    pub fn folder_conflicts(&self) -> Vec<std::path::PathBuf> {
        self.folder_conflicts.lock().unwrap().clone()
    }

    /// The user's answer to a conflict between their folder and the
    /// primary's checkout: `keep_folder` sends every change the folder
    /// made since they last agreed — the conflicting paths among them —
    /// into the checkout; otherwise the checkout's side is written over
    /// the folder. Blocking.
    pub fn resolve_folder_conflict(&self, keep_folder: bool) -> Result<()> {
        let Checkout::Remote { vm, path } = self.checkout() else {
            return Ok(());
        };
        let vm_info = self
            .substrate()
            .vm_details()
            .cloned()
            .with_context(|| format!("{}'s substrate is not its VM {vm}", self.env.id))?;
        let keys = crate::keys::Keys::for_workspace(&self.env.workspace_root);
        let _one = self.folder_sync.lock().unwrap_or_else(|e| e.into_inner());
        if keep_folder {
            let git = taste_git::GitWorkspace::discover(&self.env.peer)
                .context("the folder is not a git working tree")?;
            let files = self.files();
            let changes = git.folder_changes()?;
            for change in &changes {
                crate::peer::send_change(&files, &path, change)?;
            }
            git.record_sent(&changes)?;
            let name = taste_git::snapshot_ref(self.env.id.as_str());
            let script = taste_git::snapshot::script(&name)?;
            let out = files.exec(&path, &["sh".into(), "-c".into(), script])?;
            if !out.success() {
                bail!("snapshotting the checkout: {}", out.stderr_utf8().trim());
            }
            self.sync_primary_blocking(&vm_info, &keys, &path, false, 0)
        } else {
            self.sync_primary_blocking(&vm_info, &keys, &path, true, 0)
        }
    }

    /// Write the peer's `refnames` into the checkout in the VM, forced: a
    /// branch the IDE settled on this host that the checkout is the
    /// authority for from now on — a published environment branch, which
    /// the coordinator reviews and merges over there, and which the sync
    /// then carries home like any other of the checkout's branches.
    /// Nothing for a local checkout, which is the peer. Blocking.
    pub fn push_refs_to_checkout_blocking(&self, refnames: &[String]) -> Result<()> {
        let Checkout::Remote { vm, path } = self.checkout() else {
            return Ok(());
        };
        let vm_info = self
            .substrate()
            .vm_details()
            .cloned()
            .with_context(|| format!("{}'s substrate is not its VM {vm}", self.env.id))?;
        let keys = crate::keys::Keys::for_workspace(&self.env.workspace_root);
        let specs: Vec<String> = refnames.iter().map(|r| format!("+{r}:{r}")).collect();
        let refs: Vec<&str> = specs.iter().map(String::as_str).collect();
        crate::peer::push_to_guest(&self.env.peer, &vm_info, &keys, &path, &refs)
    }

    /// Give the checkout in the VM the peer's remote-tracking refs, so a
    /// rebase over there has the tip the user just fetched here — with
    /// the user's keys, which never enter the VM. Nothing for a local
    /// checkout. Blocking.
    pub fn share_remotes_blocking(&self) -> Result<()> {
        let Checkout::Remote { vm, path } = self.checkout() else {
            return Ok(());
        };
        let vm_info = self
            .substrate()
            .vm_details()
            .cloned()
            .with_context(|| format!("{}'s substrate is not its VM {vm}", self.env.id))?;
        let keys = crate::keys::Keys::for_workspace(&self.env.workspace_root);
        crate::peer::push_to_guest(
            &self.env.peer,
            &vm_info,
            &keys,
            &path,
            &[crate::peer::REMOTES_REFSPEC],
        )
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
                })
                .or_else(|| {
                    let gaps = self.capability_gaps();
                    (!gaps.is_empty()).then(|| {
                        format!(
                            "the environment is up but lacks what the project needs: {}",
                            gaps.join("; ")
                        )
                    })
                }),
        };
        // The CONFIG decides, not the folder: the IDE makes `.devcontainer/`
        // itself as the bind source the agent writes into, so an empty one
        // is the ordinary state of a project with no config yet.
        let has_config =
            !matches!(
                state,
                SupervisorState::NoConfig | SupervisorState::Preparing { .. }
            ) && !matches!(DevcontainerConfig::discover(&self.config_root()), Ok(None));
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

    /// What the last start's probes found missing, each with its remedy
    /// (`probe_capabilities`). Empty when nothing was, or nothing asked.
    pub fn capability_gaps(&self) -> Vec<String> {
        self.capability_gaps.lock().unwrap().clone()
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

        if let Err(e) = self.refresh_config_mirror() {
            return baseline(Some(format!(
                "the project config could not be read from its VM: {e:#}"
            )));
        }
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
        if let Err(e) =
            crate::security::validate_security_via(&self.files(), &config, self.checkout().path())
        {
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
            self.failed_build_log.lock().unwrap().take();
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
        let log = self.logs_tail(FAILED_BUILD_LOG_LINES);
        // podman's exit status says only that it failed; its `Error:` line
        // says what, and rides in the reason every surface quotes.
        let said = log
            .iter()
            .rev()
            .map(|line| line.trim())
            .find(|line| line.starts_with("Error:") || line.starts_with("error:"))
            .map(str::to_string);
        let reason = match said {
            Some(said) => format!("{error} — {said}"),
            None => error.to_string(),
        };
        *self.build_failed.lock().unwrap() = Some((hash, reason));
        *self.failed_build_log.lock().unwrap() = Some(log);
    }

    /// The last failed build's own log, while that failure stands.
    pub fn failed_build_log(&self) -> Option<Vec<String>> {
        if self.build_failed.lock().unwrap().is_none() {
            return None;
        }
        self.failed_build_log.lock().unwrap().clone()
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
            let root = self.checkout().path().display().to_string();
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
        self.sync_tunnel(&state);
    }

    /// Last `n` lines the container itself wrote — its main process's
    /// stdout and stderr, as `podman logs` keeps them.
    pub fn container_logs_tail(&self, n: usize) -> Vec<String> {
        let logs = self.container_logs.lock().unwrap();
        logs.iter().rev().take(n).rev().cloned().collect()
    }

    /// Keep the runtime log's followers alive while the container runs,
    /// and none otherwise. Two streams, because the one the devcontainer
    /// spec formally gives a container — its main process's output,
    /// `podman logs --follow` — is empty for nearly every devcontainer:
    /// `overrideCommand` is on by default and PID 1 is `sleep infinity`,
    /// which writes nothing, ever (David, 2026-09-21: "I never see
    /// anything in runtime logs"). So podman's own events about the
    /// container are followed beside it — started, died with its exit
    /// code, OOM-killed, an exec session that ended badly — and the
    /// commands agents run in it are told here by the MCP server
    /// (`ide_exec`). For a systemd image the first stream is the
    /// journal's console, and it carries the rest of the story.
    fn sync_log_follower(&self, state: &SupervisorState) {
        let running = matches!(state, SupervisorState::Running { .. });
        let mut slot = self.log_follower.lock().unwrap();
        if !running {
            for follower in slot.drain(..) {
                follower.abort();
            }
            return;
        }
        if slot.iter().any(|follower| !follower.is_finished()) {
            return;
        }
        slot.clear();
        // Only where there is a runtime to follow on: a state set from a
        // test's thread has nobody to read the stream for it.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        self.push_container_output(
            "[taste-ide] following the container: its main process's output, podman's events \
             about it, and the commands agents run in it"
                .to_string(),
        );
        slot.push(self.spawn_events_follower(&runtime));
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
        slot.push(task.abort_handle());
    }

    /// One line into the runtime log's ring and onto the bus, from the
    /// IDE's side of the container.
    pub fn push_container_output(&self, line: String) {
        {
            let mut ring = self.container_logs.lock().unwrap();
            if ring.len() >= LOG_RING_CAPACITY {
                ring.pop_front();
            }
            ring.push_back(line.clone());
        }
        self.events.publish(Event::ContainerOutput {
            env: self.env.id.clone(),
            line,
        });
    }

    /// `podman events --stream` for this container, into the runtime log:
    /// the lifecycle podman sees — start, died with its exit code, oom,
    /// kill, stop, restart, pause, health — and an exec session that ended
    /// with a non-zero exit. Not every `exec` and `exec_died`: the IDE
    /// itself execs into the container constantly (the files service, a
    /// recheck, a probe), and a log of those is a log of the IDE.
    fn spawn_events_follower(&self, runtime: &tokio::runtime::Handle) -> tokio::task::AbortHandle {
        let args: Vec<String> = vec![
            "events".into(),
            "--stream".into(),
            "--filter".into(),
            format!("container={}", self.container_name()),
            "--format".into(),
            "json".into(),
        ];
        let mut command = self.podman(&args);
        let ring = self.container_logs.clone();
        let events = self.events.clone();
        let env = self.env.id.clone();
        let task = runtime.spawn(async move {
            let mut child = match command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
            {
                Ok(child) => child,
                Err(e) => {
                    tracing::debug!("{env}: podman events --stream did not start: {e}");
                    return;
                }
            };
            let Some(stdout) = child.stdout.take() else {
                return;
            };
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(raw)) = lines.next_line().await {
                let Some(line) = podman_event_line(&raw) else {
                    continue;
                };
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
            }
            let _ = child.wait().await;
        });
        task.abort_handle()
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
        // A checkout in a VM is read through its mirror, which is brought
        // up to date first: a recheck reads what the checkout has now.
        if let Err(e) = self.refresh_config_mirror() {
            self.log(format!("the config mirror could not be refreshed: {e:#}"));
        }
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
            SupervisorState::NoConfig | SupervisorState::Preparing { .. } => {
                // A previous IDE instance may have left a container running
                // — of either authority. Adopt it rather than sitting in
                // safe mode next to a healthy environment.
                if let Some(container_id) = self.adopt_running_container() {
                    self.set_state(SupervisorState::Running { container_id });
                } else if project.is_some() {
                    self.set_state(SupervisorState::ConfigDetected);
                    self.set_pending(false);
                } else {
                    // NoConfig — which is no longer a dead end. It is the
                    // state a workspace with no devcontainer starts in, and
                    // `reload` will bring the baseline up from here. Said
                    // when the stages left it in Preparing: the window
                    // brings the baseline up on the NoConfig it is TOLD,
                    // and a primary left in "placing the checkout" never
                    // told it anything — the startup sat on "the container
                    // comes next" for an hour (2026-09-22).
                    if matches!(current, SupervisorState::Preparing { .. }) {
                        self.set_state(SupervisorState::NoConfig);
                    }
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
            self.checkout().local_path(),
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
        // The adopted container's working directory is the one its OWN
        // config gave it, which its authority label names: the baseline's
        // `/workspace` for a baseline container, the project's
        // `workspaceFolder` for a project one. It used to be the resolved
        // config's regardless, and while the project image built in a new
        // VM the baseline ran with the project's workdir on the exec
        // target — every shell and agent exec died on `crun: chdir to
        // /workspaces/taste-ide: No such file or directory` (David,
        // 2026-09-22). The drift flag below still says the two rungs
        // disagree; the workdir just stops lying about the container.
        let workdir = match authority {
            ConfigAuthority::Baseline => crate::baseline::ensure_baseline_config()
                .map(|config| config.workspace_folder().to_string())
                .unwrap_or_else(|_| resolved.config.workspace_folder().to_string()),
            ConfigAuthority::Project => resolved.config.workspace_folder().to_string(),
        };
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
        let host_path = self.checkout().path().display().to_string();
        if host_path != workdir {
            mounts.push("-v".into());
            mounts.push(format!(
                "{host_path}:{host_path}:{}",
                workspace_bind_flags(authority, !self.checkout().is_local())
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
            // The checkout where it is NOW — the identity's is where it
            // started, which for a placed primary is a folder the VM does
            // not have (the first launch after the move failed here with
            // `statfs …/.devcontainer: no such file or directory`).
            let source = self
                .checkout()
                .path()
                .join(".devcontainer")
                .display()
                .to_string();
            mounts.push("-v".into());
            let label = self.label_flag();
            mounts.push(format!("{source}:{workdir}/.devcontainer:{label}"));
            if host_path != workdir {
                mounts.push("-v".into());
                mounts.push(format!("{source}:{host_path}/.devcontainer:{label}"));
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

    /// The image `config` runs from, as podman names it: the tag it
    /// builds to, or the registry image it pulls. `None` for a config that
    /// names neither.
    fn project_image_ref(&self, config: &DevcontainerConfig) -> Option<String> {
        if config.dockerfile_path().is_some() {
            self.image_tag(config).ok()
        } else {
            config.image.clone()
        }
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
        // The exec context resolves `podman exec` against a target of its
        // own, aimed at construction; a substrate that moves must move it
        // too, or terminals and `ide_exec` keep dialling the podman the
        // container is no longer on.
        self.exec.set_podman_target(substrate.target().clone());
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

    /// Rebuild this environment's image from nothing and start its
    /// container on it: the base image pulled, no layer cached, so every
    /// package step installs what is current. What the weekly package
    /// refresh does (`crate::migration`).
    pub async fn refresh_packages(&self) -> Result<()> {
        self.fresh_build.store(true, Ordering::SeqCst);
        let result = self.reload().await;
        self.fresh_build.store(false, Ordering::SeqCst);
        result
    }

    /// Start this environment's container again on the image its tag
    /// names now, building nothing when the tag is there — the refresh of
    /// an environment whose image another environment rebuilt from
    /// nothing today (`crate::migration`).
    pub async fn restart_on_current_image(&self) -> Result<()> {
        self.reuse_image.store(true, Ordering::SeqCst);
        let result = self.reload().await;
        self.reuse_image.store(false, Ordering::SeqCst);
        result
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
    /// be its own containerized environment"). A baseline already running
    /// is left alone.
    ///
    /// **A project config is built, not set aside** (David, 2026-09-23).
    /// The automatic start used to put a config whose image this VM had
    /// never built on the baseline and wait for a Rebuild, because building
    /// runs the config's lifecycle commands and that was the user's act
    /// while they ran on the user's kernel. They run in the VM now and the
    /// reload asks nobody, so an environment placed in a VM that had not
    /// built its image — a new issue's, most often — came up in safe mode
    /// for no reason anyone could see, its agent oriented to it, and
    /// rebuilt from there. Now the start builds it, the startup page shows
    /// the build, and the baseline follows only a failure
    /// (`reload_reporting`). A config whose last build failed is still
    /// passed over by `resolve_config` until one of its files changes, so
    /// a broken build is not retried at every launch.
    pub async fn reload_baseline(&self) -> Result<()> {
        if self.inside {
            bail!("the IDE is running inside this devcontainer; nothing to bring up from here");
        }
        let project = matches!(
            self.resolve_config().map(|r| r.authority),
            Ok(ConfigAuthority::Project)
        );
        if project {
            if matches!(self.state(), SupervisorState::Running { .. })
                && self.config_authority() == ConfigAuthority::Project
            {
                return Ok(());
            }
            return self.reload_reporting().await;
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
        // The first line of every lifecycle run says what asked for it and
        // what it found: a baseline started under a running one is a
        // container replaced under its agent, and the state at entry is
        // what decided that (2026-09-21: two baseline starts fifty seconds
        // apart, and nothing in the log to say why the second ran).
        self.log(format!(
            "{} requested; the environment was {}",
            if baseline_only {
                "baseline start"
            } else {
                "reload"
            },
            match self.state() {
                SupervisorState::Running { container_id } => {
                    format!("running ({container_id}), which this replaces")
                }
                other => format!("{other:?}").to_lowercase(),
            }
        ));
        // Nowhere to run: no VM was supplied, or the checkout is somewhere
        // this substrate cannot reach. Refused with the reason, in the
        // state the row and the banner read — never started on a lesser
        // rung, because there is none (docs/ENVIRONMENTS.md → "There is
        // no rung below VM isolation").
        if !self.substrate().can_host(&self.checkout()) {
            // Before the ladder has run there is nothing to refuse and
            // nothing to place into: the start waits. Reconcile places the
            // checkout and checks the environment on its own once the VM is
            // up, which is what starts it (2026-09-21: a start pressed for
            // in the first seconds put the primary in Failed with "the
            // workspace's VM is still coming up").
            if self.substrate().is_pending() {
                self.log(
                    "the workspace's VM is not up yet; the environment starts on its own once \
                     its checkout is placed there",
                );
                self.set_state(SupervisorState::Preparing {
                    what: "waiting for the workspace's VM".into(),
                });
                self.set_pending(false);
                return Ok(());
            }
            // A checkout that can be put where it runs is put there first:
            // the primary's, pressed for before reconcile got to it.
            let placer = self.placer.lock().unwrap().clone();
            if let Some(placer) = placer {
                self.log("placing this environment's checkout in the workspace's VM first");
                match tokio::task::spawn_blocking(move || placer()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => return Err(self.refuse_start(format!("{e:#}"))),
                    Err(e) => {
                        return Err(
                            self.refuse_start(format!("placing the checkout did not finish: {e}"))
                        )
                    }
                }
            }
            let substrate = self.substrate();
            let checkout = self.checkout();
            if !substrate.can_host(&checkout) {
                return Err(self.refuse_start(substrate.refusal(&checkout)));
            }
        }
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
        // A new start is a new chance for the lifecycle commands, and a
        // new image a new answer to what it has.
        self.hook_failure.lock().unwrap().take();
        self.capability_gaps.lock().unwrap().clear();
        // An automatic start sets a project config aside for the baseline
        // when its image has never been built: building it runs the
        // config's lifecycle commands, and that is the user's Rebuild (or
        // the agent's approved reload), never a launch's. A config whose
        // image IS on the substrate was built by that very act, so it runs
        // — the launch after a Rebuild lands in the project's environment,
        // not in safe mode with a banner asking for the Rebuild again
        // (David, 2026-09-21). On this host a container of the last
        // session was still running to adopt; in a VM the containers stop
        // with the window, and this is what takes adoption's place.
        let resolved = if baseline_only && resolved.authority == ConfigAuthority::Project {
            let built = match self.project_image_ref(&resolved.config) {
                Some(reference) => self
                    .run_captured(vec!["image".into(), "exists".into(), reference])
                    .await
                    .is_ok(),
                None => false,
            };
            if built {
                self.log(
                    "the project's environment was built before, so it runs; the baseline \
                     stands in only for a config that has not been built",
                );
                resolved
            } else {
                ResolvedConfig {
                    config: crate::baseline::ensure_baseline_config()?,
                    authority: ConfigAuthority::Baseline,
                    reason: Some(
                        "the project's configuration has not been built yet; Rebuild builds it"
                            .to_string(),
                    ),
                }
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
            let mut args = crate::image::build_args(
                &config,
                &tag,
                &staged_dockerfile,
                &staged,
                &self.workspace_key(),
            );
            let reuse = self.reuse_image.swap(false, Ordering::SeqCst)
                && self
                    .run_captured(vec!["image".into(), "exists".into(), tag.clone()])
                    .await
                    .is_ok();
            if reuse {
                self.log(format!(
                    "starting on {tag} as it stands: another environment rebuilt it with \
                     updated packages today"
                ));
                args.clear();
            } else if self.fresh_build.swap(false, Ordering::SeqCst) {
                self.log("building from nothing: the base image pulled, no layer cached, so every package is current".to_string());
                args.splice(1..1, ["--no-cache".to_string(), "--pull=newer".to_string()]);
            }
            let built = if args.is_empty() {
                Ok(())
            } else {
                self.run_logged(args).await
            };
            built.inspect_err(|e| {
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
        let local_workspace_folder = self.checkout().path().display().to_string();
        // A repo's mount that asks for a private label (`Z`) is written for
        // a host, where it keeps one environment's files from another
        // container. In a VM the keeper is another container over the same
        // files by design, and a private label locked it out: every `git`
        // it ran in the checkout failed with EACCES the moment the
        // project's container came up (2026-09-21). So the label is shared
        // there, as `workspace_bind_flags` already makes the IDE's own.
        let in_vm = !self.checkout().is_local();
        let repo_mount = |mount: &str| -> String {
            let expanded = mount.replace("${localWorkspaceFolder}", &local_workspace_folder);
            if in_vm {
                share_label_in_vm(&expanded)
            } else {
                expanded
            }
        };
        match &config.workspace_mount {
            Some(mount) => {
                args.push("--mount".into());
                args.push(self.namespaced_mount(&repo_mount(mount)));
            }
            None => {
                args.push("-v".into());
                args.push(format!(
                    "{local_workspace_folder}:{workdir}:{}",
                    workspace_bind_flags(authority, !self.checkout().is_local())
                ));
            }
        }
        for mount in &config.mounts {
            if let Some(m) = mount.as_str() {
                args.push("--mount".into());
                args.push(self.namespaced_mount(&repo_mount(m)));
            }
        }

        // The bind source for the config the agent may write (`ide_mounts`):
        // a bind needs one, and a project with no config has none yet.
        if authority == ConfigAuthority::Baseline {
            let config_dir = self.checkout().path().join(".devcontainer");
            // On this host. A checkout in a VM has the directory made over
            // there, by the files service, before the run.
            if let Err(e) = self.make_dir_in_checkout(&config_dir) {
                tracing::warn!(
                    "could not make {} for the agent to write its config into: {e}",
                    config_dir.display()
                );
            }
        }
        // The grant, enforced: what the pool placed this environment by is
        // what its container may use. Swap equal to memory, so the ceiling
        // is a ceiling; the guest delegates the cpu and memory controllers
        // to rootless podman (verified on the guest, 2026-09-21).
        let grant = self.grant_for(&config, authority);
        args.push("--cpus".into());
        args.push(grant.cpus.to_string());
        args.push("--memory".into());
        args.push(format!("{}m", grant.memory_mib));
        args.push("--memory-swap".into());
        args.push(format!("{}m", grant.memory_mib));
        args.extend(priority_args(self.env.id.is_primary(), grant));
        *self.applied_grant.lock().unwrap() = Some(grant);
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
                continue;
            }
            args.push(arg.clone());
        }
        let nesting = crate::security::privileged_run_args(&config);
        if !nesting.is_empty() {
            self.log(format!(
                "privileged: not passed as asked; granted as what running podman inside the \
                 container needs ({})",
                nesting.join(" ")
            ));
            args.extend(nesting);
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
            for source in crate::security::bind_sources(&config, self.checkout().path()) {
                if self.exists_in_checkout(&source) {
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

        if authority == ConfigAuthority::Project {
            self.probe_capabilities(&config, &name, &workdir).await;
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

    /// Ask the started container for what the project will need of it,
    /// and remember each gap with its remedy (`capability_gaps`).
    ///
    /// These are the dead ends an agent writing a devcontainer met only by
    /// failing, each a fact about one distribution or one image build
    /// rather than about the project (2026-09-23): Task packaged under
    /// another name, nested podman's `newuidmap` stripped of its file
    /// capabilities by the image build, the user's subordinate IDs outside
    /// the container's range, fuse-overlayfs missing. The container is up
    /// either way, so a gap is reported, never fatal; what it buys is that
    /// the agent reads the remedy in its next prompt, rather than finding
    /// the cause by bisecting its own Containerfile. As the container's
    /// user, in its workdir, since that is who runs the project's work.
    async fn probe_capabilities(&self, config: &DevcontainerConfig, name: &str, workdir: &str) {
        let mut gaps = Vec::new();
        let exec = |script: &str| {
            let mut args: Vec<String> =
                vec!["exec".into(), "--workdir".into(), workdir.to_string()];
            if let Some(user) = config.effective_user() {
                args.push("--user".into());
                args.push(user.to_string());
            }
            args.extend([
                name.to_string(),
                "sh".into(),
                "-c".into(),
                script.to_string(),
            ]);
            args
        };
        let has_taskfile = taste_core::conventions::TASKFILE_NAMES
            .iter()
            .any(|file| self.exists_in_checkout(&self.checkout().path().join(file)));
        if has_taskfile {
            let found = self
                .podman(&exec("command -v task || command -v go-task"))
                .output()
                .await
                .is_ok_and(|out| out.status.success());
            self.log(format!(
                "check: task (the project has a Taskfile) — {}",
                if found { "ok" } else { "missing" }
            ));
            if !found {
                gaps.push(
                    "task (taskfile.dev) is not in the image, as task or as Fedora's go-task; \
                     install it in the Containerfile (Fedora: dnf install go-task)"
                        .to_string(),
                );
            }
        }
        if crate::security::asks_for_nesting(config) {
            let gap = match self.podman(&exec(NESTING_PROBE)).output().await {
                Ok(out) if out.status.success() => None,
                Ok(out) => Some(nesting_gap(&String::from_utf8_lossy(&out.stdout))),
                Err(e) => Some(format!("nested podman could not be checked: {e}")),
            };
            self.log(format!(
                "check: nested podman (the config is privileged) — {}",
                gap.as_deref().unwrap_or("ok")
            ));
            gaps.extend(gap);
        }
        *self.capability_gaps.lock().unwrap() = gaps;
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
    /// [`Self::stop`], after asking the container to stop on its own
    /// first: `podman stop` with a real grace period, so a systemd image
    /// runs its units' ExecStop and a dev server flushes what it was
    /// writing, where `stop`'s two seconds are for a container whose VM is
    /// staying up. For the VM's own Stop, Rebuild, and Delete (David,
    /// 2026-09-21: "attempt orderly shutdown of envs on a VM prior to
    /// stopping/rebuilding/deleting the VM").
    pub async fn stop_orderly(&self) -> Result<()> {
        if self.inside {
            bail!("cannot stop the container the IDE itself runs in");
        }
        let name = self.container_name();
        self.log(format!(
            "asking {name} to stop (up to 15s) before its VM goes"
        ));
        let _ = self
            .run_captured(vec!["stop".into(), "-t".into(), "15".into(), name])
            .await;
        self.stop().await
    }

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
        // Nothing to list before the ladder has resolved, and nothing to
        // ask: every query would fail against the connection that does
        // not exist and land in the log as noise.
        if !self.substrate().is_resolved() {
            return resources;
        }

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
                    // The grant beside the status, so the VM's arithmetic
                    // is visible where the VM's own numbers are.
                    let status = match *self.applied_grant.lock().unwrap() {
                        Some(grant) => format!("{status} · {} granted", grant.describe()),
                        None => status.to_string(),
                    };
                    resources.push(ResourceInfo {
                        kind: ResourceKind::Container,
                        name: name.to_string(),
                        id: id.to_string(),
                        status,
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
/// What a sync pass moved, in the words the title bar's status lists it
/// with; `None` when nothing moved.
fn folder_sync_summary(sync: &crate::peer::PeerSync) -> Option<String> {
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut parts = Vec::new();
    if sync.sent > 0 {
        parts.push(format!(
            "Sent {} to Personal",
            plural(sync.sent, "change", "changes")
        ));
    }
    if sync.switched {
        if let Some(branch) = &sync.branch {
            parts.push(format!("Switched your folder to {branch}"));
        }
    }
    if sync.received > 0 {
        parts.push(format!(
            "Brought {} into your folder",
            plural(sync.received, "change", "changes")
        ));
    }
    if !sync.conflicts.is_empty() {
        parts.push(format!(
            "{} changed on both sides",
            plural(sync.conflicts.len(), "file", "files")
        ));
    }
    // Why the folder was left as it is — a branch ahead, a merge under
    // way, a switch of the user's — is the one thing the title bar must
    // not keep to itself, or the sync just looks stuck.
    if let Some(note) = &sync.note {
        let mut chars = note.chars();
        if let Some(first) = chars.next() {
            parts.push(first.to_uppercase().chain(chars).collect());
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// A path a build or a package manager churns, which the mirror's
/// watches do not react to: none of it is anything git would show in a
/// project that ignores its build output, and reacting to it would
/// snapshot the checkout every two seconds through a build. A project
/// that TRACKS a file under one of these still has it mirrored, on the
/// next change anywhere else, since the snapshot is the whole working
/// copy.
fn churn_path(rel: &str) -> bool {
    const CHURN: [&str; 6] = ["target", "node_modules", ".cache", "dist", "build", ".venv"];
    rel.split('/').any(|part| CHURN.contains(&part))
}

/// Nested podman, end to end and offline: a user namespace, then an image
/// built FROM scratch and a container run on it — which mounts storage,
/// sets up `/dev/pts` and `/proc`, starts its network (pasta), and names
/// its host exactly as a real one does, with the DEFAULT flags a
/// project's own `podman run` uses. The image is `true` and the libraries
/// it links, copied from the container itself, because a real one is
/// what is needed: podman checks an image for its command before it
/// creates anything, so a probe of a missing executable (which this was)
/// exited 127 before a namespace, a network, or a hostname existed, and
/// passed containers that could not run anything. It prints the stage
/// that failed and podman's `Error:` line; the IDE's remedy is chosen
/// from them (`nesting_gap`).
const NESTING_PROBE: &str = r#"say() { printf '%s %s\n' "$1" "$(printf '%s\n' "$2" | grep -m1 -E 'uid_map|gid_map|Error:|Failed' || printf '%s\n' "$2" | grep -v '^[[:space:]]*$' | tail -n1)"; }
command -v podman >/dev/null || { echo "missing podman"; exit 1; }
out=$(podman unshare true 2>&1) || { say userns "$out"; exit 1; }
t=/usr/bin/true; [ -x "$t" ] || t=/bin/true; d=$(mktemp -d); mkdir -p "$d/r"
for f in "$t" $( (ldd "$t" 2>/dev/null || /lib64/ld-linux-x86-64.so.2 --list "$t" 2>/dev/null) | grep -o '/[^ ]*'); do mkdir -p "$d/r${f%/*}"; cp -L "$f" "$d/r$f"; done
printf 'FROM scratch\nCOPY r/ /\nLABEL taste.probe=1\n' > "$d/Containerfile"
out=$(podman build -q -t localhost/taste-ide-probe "$d" 2>&1); rc=$?; rm -rf "$d"
[ $rc = 0 ] || { say storage "$out"; exit 1; }
out=$(podman run --rm localhost/taste-ide-probe "$t" 2>&1); rc=$?
podman rmi -f localhost/taste-ide-probe >/dev/null 2>&1
[ $rc = 0 ] || { say run "$out"; exit 1; }"#;

/// A failed nesting probe's report, as the gap and what to change. Each
/// remedy is one measured in a Fedora CoreOS 44 guest against an image
/// built the way an agent builds one (2026-09-23): setuid on `newuidmap`
/// did NOT fix it there, a `setcap` in a RUN step after the package
/// install did.
fn nesting_gap(report: &str) -> String {
    let report = report.trim();
    let (stage, said) = report.split_once(' ').unwrap_or((report, ""));
    let remedy = if stage == "missing" {
        "install podman, fuse-overlayfs, and passt in the Containerfile"
    } else if said.contains("pasta") && said.contains("not found") {
        "podman's network helper is missing; install passt in the Containerfile"
    } else if said.contains("should have setuid or have filecaps") {
        "newuidmap and newgidmap lost their file capabilities in the image build; after the \
         package install, add RUN setcap cap_setuid+ep /usr/bin/newuidmap && setcap \
         cap_setgid+ep /usr/bin/newgidmap"
    } else if said.contains("uid_map") || said.contains("gid_map") || said.contains("subuid") {
        "the user's subordinate IDs are outside the container's range (0-65536; useradd's \
         default is not); for a user with uid 1000, write USER:1:999 and USER:1001:64535 to \
         /etc/subuid and /etc/subgid"
    } else if said.contains("sethostname") {
        "the container's seccomp filter refuses sethostname to a nested container; rebuild \
         so the IDE grants CAP_SYS_ADMIN (it comes with \"privileged\": true)"
    } else if said.contains("/dev/net/tun") || said.contains("pasta failed") {
        "the container has no /dev/net/tun for pasta; rebuild so the IDE grants it (it comes \
         with \"privileged\": true)"
    } else if said.contains("fuse-overlayfs") || said.contains("mount program") {
        "container storage is on the container's overlay root and needs fuse-overlayfs; \
         install fuse-overlayfs in the Containerfile"
    } else {
        "podman's error above is the cause"
    };
    // podman's cause is at the END of its line, after the storage paths
    // and container ids: keep that end.
    let said = match said.char_indices().rev().nth(179) {
        _ if said.is_empty() => "no output".to_string(),
        Some((cut, _)) => format!("…{}", &said[cut..]),
        None => said.to_string(),
    };
    format!("nested podman fails at {stage} ({said}); {remedy}")
}

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

/// One `podman events --format json` record as a runtime-log line, or
/// nothing for the statuses that are the IDE's own doing or say nothing:
/// `exec` (every files-service call is one), `exec_died` that exited
/// zero, `cleanup`, `attach`, and everything not about a container.
fn podman_event_line(raw: &str) -> Option<String> {
    let event: serde_json::Value = serde_json::from_str(raw).ok()?;
    if event.get("Type").and_then(|t| t.as_str()) != Some("container") {
        return None;
    }
    let status = event.get("Status").and_then(|s| s.as_str())?;
    let exit = event.get("ContainerExitCode").and_then(|c| c.as_i64());
    let line = match status {
        "start" => "[podman] container started".to_string(),
        "died" => match exit {
            Some(code) => format!("[podman] container died — exit {code}"),
            None => "[podman] container died".to_string(),
        },
        "oom" => "[podman] container hit its memory limit (OOM)".to_string(),
        "kill" => "[podman] container was sent a signal".to_string(),
        "stop" => "[podman] container stopped".to_string(),
        "restart" => "[podman] container restarted".to_string(),
        "pause" => "[podman] container paused".to_string(),
        "unpause" => "[podman] container unpaused".to_string(),
        "health_status" => format!(
            "[podman] health: {}",
            event
                .get("HealthStatus")
                .and_then(|h| h.as_str())
                .unwrap_or("reported")
        ),
        "exec_died" => match exit {
            Some(code) if code != 0 => format!("[podman] an exec session ended with exit {code}"),
            _ => return None,
        },
        _ => return None,
    };
    Some(line)
}

#[cfg(test)]
mod tests {
    #[test]
    fn podman_events_become_lines_only_when_they_say_something() {
        let died = r#"{"ContainerExitCode":137,"Name":"x","Status":"died","Type":"container"}"#;
        assert_eq!(
            super::podman_event_line(died).as_deref(),
            Some("[podman] container died — exit 137")
        );
        let exec = r#"{"Name":"x","Status":"exec","Type":"container"}"#;
        assert_eq!(super::podman_event_line(exec), None);
        let exec_ok = r#"{"ContainerExitCode":0,"Status":"exec_died","Type":"container"}"#;
        assert_eq!(super::podman_event_line(exec_ok), None);
        let exec_bad = r#"{"ContainerExitCode":2,"Status":"exec_died","Type":"container"}"#;
        assert!(super::podman_event_line(exec_bad).is_some());
        let system = r#"{"Status":"refresh","Type":"system"}"#;
        assert_eq!(super::podman_event_line(system), None);
    }

    /// A repo's private label is shared in a VM, and nothing else in the
    /// mount changes.
    #[test]
    fn a_repo_mounts_private_label_is_shared_in_a_vm() {
        assert_eq!(
            super::share_label_in_vm("source=/w,target=/workspaces/x,type=bind,Z"),
            "source=/w,target=/workspaces/x,type=bind,z"
        );
        assert_eq!(
            super::share_label_in_vm("type=bind,source=/w,target=/x,relabel=private,ro"),
            "type=bind,source=/w,target=/x,relabel=shared,ro"
        );
        assert_eq!(
            super::share_label_in_vm("type=volume,source=cargo,target=/home/dev/.cargo"),
            "type=volume,source=cargo,target=/home/dev/.cargo"
        );
    }

    /// The primary outranks agent environments on a contended VM: more CPU
    /// weight, a memory floor, and never the first the OOM killer takes.
    #[test]
    fn the_primary_outranks_agent_environments() {
        let grant = crate::config::Grant::DEFAULT;
        assert_eq!(
            super::priority_args(true, grant),
            ["--cpu-shares", "1024", "--memory-reservation", "4096m"]
        );
        assert_eq!(
            super::priority_args(false, grant),
            ["--cpu-shares", "256", "--oom-score-adj", "500"]
        );
    }

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
        assert_eq!(walk_checkout(dir.path(), false).apparent_bytes, 350);

        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("huge"), vec![b'z'; 10_000]).unwrap();
            std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
            assert_eq!(
                walk_checkout(dir.path(), false).apparent_bytes,
                350,
                "a link is not this env's disk"
            );
        }
        // An unreadable path is zero, not a panic and not a refusal.
        assert_eq!(
            walk_checkout(&dir.path().join("no-such-dir"), false).apparent_bytes,
            0
        );
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
            crate::substrate::Substrate::host_for_tests(),
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
    /// The probe's reports as the guest printed them (2026-09-23), each to
    /// the remedy measured for it.
    #[test]
    fn a_nesting_report_names_its_remedy() {
        let caps = nesting_gap(
            "userns Error: cannot set up namespace using \"/usr/bin/newuidmap\": should have \
             setuid or have filecaps setuid: exit status 1",
        );
        assert!(
            caps.contains("setcap cap_setuid+ep /usr/bin/newuidmap"),
            "{caps}"
        );
        let ids = nesting_gap(
            "userns time=\"…\" level=error msg=\"running `/usr/bin/newuidmap 13 0 1000 1 1 \
             524288 65536`: newuidmap: write to uid_map failed: Operation not permitted\"",
        );
        assert!(ids.contains("/etc/subuid"), "{ids}");
        let fuse = nesting_gap(&format!(
            "storage Error: mounting new container: {}: using mount program \
             /usr/bin/fuse-overlayfs: fuse: device /dev/fuse not found. Kernel module not loaded?",
            "x".repeat(400)
        ));
        assert!(fuse.contains("install fuse-overlayfs"), "{fuse}");
        assert!(
            fuse.contains("/dev/fuse not found"),
            "the cause survives the cut: {fuse}"
        );
        assert!(fuse.len() < 500, "{fuse}");
        assert!(nesting_gap("missing podman").contains("install podman"));
        let tun = nesting_gap("run Error: pasta failed with exit code 1:");
        assert!(tun.contains("/dev/net/tun"), "{tun}");
        let uts = nesting_gap(
            "run Error: crun: sethostname: Operation not permitted: OCI permission denied",
        );
        assert!(uts.contains("CAP_SYS_ADMIN"), "{uts}");
    }

    #[test]
    fn a_config_the_validator_refuses_does_not_become_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let sup = make(dir.path());
        let dc = dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();
        std::fs::write(
            dc.join("devcontainer.json"),
            r#"{"image": "img", "runArgs": ["--privileged", "--security-opt=seccomp=unconfined"]}"#,
        )
        .unwrap();

        let resolved = sup.resolve_config().unwrap();
        assert_eq!(resolved.authority, ConfigAuthority::Baseline);
        let reason = resolved.reason.expect("a refusal is worth explaining");
        assert!(reason.contains("refused"), "{reason}");
        assert!(reason.contains("security-opt"), "{reason}");
    }

    /// In a VM the binds carry the shared label, so the keeper — another
    /// container over the same files — can still read and remove them.
    #[test]
    fn a_checkout_in_a_vm_is_bound_with_the_shared_label() {
        let dir = tempfile::tempdir().unwrap();
        let identity = EnvironmentIdentity {
            id: EnvironmentId::parse("i-0001").unwrap(),
            workspace_root: dir.path().to_path_buf(),
            checkout: Checkout::Remote {
                vm: "taste-x".into(),
                path: PathBuf::from("/var/home/core/taste/x/i-0001"),
            },
            peer: dir.path().to_path_buf(),
        };
        let remote = make_env(dir.path(), identity);
        let config =
            crate::baseline::ensure_baseline_config_in(&dir.path().join("baseline")).unwrap();
        let mounts = remote
            .ide_mounts(&config, ConfigAuthority::Baseline)
            .join(" ");
        assert!(
            mounts.contains("/var/home/core/taste/x/i-0001:/var/home/core/taste/x/i-0001:ro,z"),
            "{mounts}"
        );
        assert!(mounts.contains("/.devcontainer:z"), "{mounts}");
        assert!(!mounts.contains(":Z"), "no private label in a VM: {mounts}");
        assert_eq!(workspace_bind_flags(ConfigAuthority::Project, true), "z");
        assert_eq!(workspace_bind_flags(ConfigAuthority::Project, false), "Z");
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
    /// A failed build keeps its own lines for the repair, whatever the
    /// baseline writes after it, and the reason says podman's error rather
    /// than only its exit status.
    #[test]
    fn a_failed_build_keeps_its_own_log_and_names_podmans_error() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("devcontainer.json"),
            r#"{"image": "example.invalid/php:nope"}"#,
        )
        .unwrap();
        let sup = make(dir.path());
        sup.log("STEP 2/3: RUN dnf install -y gti");
        sup.log("Error: building at STEP \"RUN dnf install -y gti\": exit status 1");
        let config = DevcontainerConfig::discover(dir.path()).unwrap().unwrap();
        sup.remember_build_failure(
            &config,
            &anyhow::anyhow!("podman build failed: exit status: 1"),
        );
        for n in 0..200 {
            sup.log(format!("the baseline's build, line {n}"));
        }
        let kept = sup.failed_build_log().expect("the failure's log is kept");
        assert!(
            kept.last().unwrap().starts_with("Error: building at STEP"),
            "{kept:?}"
        );
        assert!(!kept.iter().any(|line| line.contains("baseline")));
        let (_, reason) = sup.resolve_authority();
        assert!(
            reason.as_deref().is_some_and(|r| r.contains("gti")),
            "{reason:?}"
        );
    }

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

    /// A checkout with no config, left in Preparing by the startup's
    /// stages, is checked back to NoConfig — the state the window brings
    /// the baseline up on — rather than staying in "placing the checkout"
    /// with nothing ever told.
    #[test]
    fn recheck_ends_the_stages_when_there_is_no_config() {
        let dir = tempfile::tempdir().unwrap();
        let sup = make(dir.path());
        sup.announce_preparing("placing the checkout in the VM");
        assert!(matches!(sup.state(), SupervisorState::Preparing { .. }));
        sup.recheck().unwrap();
        assert_eq!(sup.state(), SupervisorState::NoConfig);
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
            crate::substrate::Substrate::host_for_tests(),
            true,
        );
        let error = inside.reload().await.unwrap_err().to_string();
        assert!(error.contains("host-side IDE"), "{error}");
        assert!(inside.stop().await.is_err());
        assert!(inside.nuke().await.is_err());
    }
}

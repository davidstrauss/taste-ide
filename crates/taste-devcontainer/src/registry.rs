//! The workspace's environments: N supervisors, one per environment.
//!
//! This is what replaced "the devcontainer". A workspace has a **primary**
//! environment — the main checkout, always present — plus any number of
//! named environments, each with its own clone, its own container, its own
//! [`ExecContext`], its own log ring and its own drift flag. The registry
//! owns them; nothing else holds a supervisor it did not ask the registry
//! for.
//!
//! The primary is not privileged in here. It exists at startup because a
//! workspace always has a main checkout, and it is the environment the
//! window's panes happen to be aimed at — but the registry knows it only as
//! the environment whose slug is `primary`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use taste_core::environment::{self, EnvironmentId};
use taste_core::{Event, EventBus, ExecContext};

use crate::keeper::Keeper;
use crate::provision::Vm;
use crate::reconcile::{self, SweepReport};
use crate::substrate::Substrate;
use crate::supervisor::{EnvironmentIdentity, Supervisor};
use taste_core::environment::Checkout;
use taste_core::files::Files;

/// What was found and what was cleaned up when the IDE opened a workspace.
#[derive(Debug, Clone, Default)]
pub struct ReconcileReport {
    /// Non-primary environments whose clone directories were still on disk
    /// and are now supervised again.
    pub restored: Vec<EnvironmentId>,
    /// Old-scheme podman resources that were removed.
    pub swept: SweepReport,
}

/// What destroying an environment found and freed.
#[derive(Debug, Clone, Default)]
pub struct DestroyReport {
    /// Work in the clone that the main checkout has never seen. Enumerated
    /// BEFORE anything is removed, because the clone may hold the only copy.
    pub unpublished: Vec<taste_git::UnpublishedBranch>,
    /// Files modified in the clone's working tree but never committed —
    /// also unrecoverable, and also worth saying out loud.
    pub dirty_files: usize,
    pub removed_volumes: Vec<String>,
    /// The clone the removal could not take, if it could not take it.
    ///
    /// Reported for the same reason as `kept_volumes`, and it matters
    /// more: this one used to abort the destroy. See the `?` that is not
    /// in `destroy` any more.
    pub kept_clone: Option<PathBuf>,
    /// Volumes podman would not remove.
    ///
    /// Said out loud because it is unrecoverable by name: the environment
    /// is gone from the registry a moment later, so nothing can compute
    /// this list again, and what is left behind is a record pointing at
    /// storage nobody will ever ask for. Two of them broke `podman system
    /// df` on the author's host for a day (David, 2026-09-17), which is
    /// how anyone found out this was silent.
    pub kept_volumes: Vec<String>,
    /// A checkout in a VM that could not be removed from it, with the
    /// reason. The peer and the record on this host are gone regardless.
    pub kept_checkout: Option<String>,
    pub removed_clone: Option<PathBuf>,
    /// Issues this environment held a claim on, handed back to the queue
    /// with a comment saying why. Not "unsaved work" — nothing is lost —
    /// but worth telling the user, because those issues are open again.
    pub released_claims: Vec<String>,
}

impl DestroyReport {
    /// What the destroy could not take, in the words both surfaces use.
    /// Empty when it took everything, so a caller can append it
    /// unconditionally.
    pub fn leftovers_clause(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.kept_volumes.is_empty() {
            parts.push(format!(
                "{} could not be removed ({})",
                if self.kept_volumes.len() == 1 {
                    "1 volume".to_string()
                } else {
                    format!("{} volumes", self.kept_volumes.len())
                },
                self.kept_volumes.join(", ")
            ));
        }
        if let Some(path) = &self.kept_clone {
            parts.push(format!("the clone is still at {}", path.display()));
        }
        if let Some(checkout) = &self.kept_checkout {
            parts.push(format!("the checkout is still at {checkout}"));
        }
        if parts.is_empty() {
            return String::new();
        }
        format!(" · {}", parts.join(" · "))
    }

    /// Whether anything was lost that nobody else has a copy of.
    pub fn had_unsaved_work(&self) -> bool {
        !self.unpublished.is_empty() || self.dirty_files > 0
    }
}

/// What this workspace's agent environments take on disk, weighed against
/// [`environment::MAX_ORCHESTRATED_DISK_BYTES`].
///
/// Summed from what each supervisor last measured, never from a walk taken
/// now: this is read on a tool call's request path, and the walk it
/// summarises is minutes of filesystem work.
///
/// The primary is not in it. The user's own checkout is not the tool's
/// spend — the same reason it is outside the running cap — and folding a
/// hundred gigabytes of the user's own `target/` into the agents' budget
/// would refuse every start forever, for a reason no agent could act on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskBudget {
    /// Bytes the measured environments account for. A **floor** when
    /// [`Self::unmeasured`] or [`Self::unmeasured_volumes`] is non-zero.
    pub used_bytes: u64,
    /// The ceiling those bytes are held against.
    pub budget_bytes: u64,
    /// How many agent environments contributed a measurement.
    pub measured: usize,
    /// How many have never been walked — nothing has measured them *yet*,
    /// which is a few seconds at startup and a bug if it persists.
    pub unmeasured: usize,
    /// Volumes that exist and could not be read, across all of them.
    pub unmeasured_volumes: usize,
    /// Age of the oldest measurement in the sum, in seconds.
    pub oldest_seconds: Option<u64>,
}

impl DiskBudget {
    /// Whether the budget is spent.
    ///
    /// A floor that has already crossed the ceiling has crossed it — an
    /// unmeasured environment can only add to the sum — so this is honest
    /// while the measurements are still coming in. The other direction is
    /// the deliberate one: with nothing measured the sum is zero and this
    /// is `false`, because a ceiling that refused on a number nobody has
    /// taken would refuse for a reason no one could check.
    pub fn spent(&self) -> bool {
        self.used_bytes >= self.budget_bytes
    }

    /// What is left, or zero when the budget is spent.
    pub fn remaining_bytes(&self) -> u64 {
        self.budget_bytes.saturating_sub(self.used_bytes)
    }
}

/// What the volume the environments are written to has left, weighed
/// against [`environment::MIN_FREE_DISK_BYTES`].
///
/// Neither a sum nor a cache: one `statvfs` at the moment the question is
/// asked. The budget is read off measurements taken minutes ago because
/// walking a checkout costs minutes; free space costs a syscall, and it is
/// the one number that moves under you while a build runs — a cached answer
/// would be a promise about a disk that has since filled.
///
/// It is also the ceiling that binds, and the two are independent by
/// construction: the budget's scope prunes at `target/`, so the clones can
/// be well inside ten gibibytes on a disk with nothing left on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreeDisk {
    /// Bytes an ordinary user could still write there, or `None` when the
    /// kernel would not say.
    pub free_bytes: Option<u64>,
    /// The floor those bytes are held against.
    pub floor_bytes: u64,
    /// The directory the question was asked about — the environments' own,
    /// which is the honest one: if the clones and the user's checkout sit on
    /// different filesystems, it is the clones' filesystem that a clone
    /// fills.
    pub volume: PathBuf,
}

impl FreeDisk {
    /// Whether taking any more would put the disk under the floor.
    ///
    /// An unanswerable `statvfs` does not refuse, the same way an unmeasured
    /// workspace does not (see [`DiskBudget::spent`]): a ceiling enforced on
    /// a number nobody has would refuse for a reason no one could check.
    /// Here that is the rarer case by far — the kernel answers this question
    /// for any path that exists, and the walk up handles the paths that do
    /// not yet.
    pub fn below_floor(&self) -> bool {
        self.free_bytes.is_some_and(|free| free < self.floor_bytes)
    }

    /// How much would have to be freed to clear the floor, or zero when it
    /// is already clear. This is the number the user acts on, and it is
    /// theirs to act on: it is usually not ours to free.
    pub fn shortfall_bytes(&self) -> u64 {
        self.free_bytes
            .map_or(0, |free| self.floor_bytes.saturating_sub(free))
    }
}

/// Where an environment's checkout was placed, recorded beside its peer
/// when it was made. The one fact the disk cannot say on its own: a peer
/// with an empty working tree is a peer, but of a checkout in WHICH VM is
/// this file's to answer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Placement {
    vm: String,
    path: PathBuf,
}

impl Placement {
    const FILE: &'static str = "placement.json";

    fn read(env_dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(env_dir.join(Self::FILE)).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn write(&self, env_dir: &Path) -> Result<()> {
        let path = env_dir.join(Self::FILE);
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}

pub struct EnvironmentRegistry {
    workspace_root: PathBuf,
    events: EventBus,
    /// Where environment clones live —
    /// `$XDG_STATE_HOME/taste-ide/environments` in real use, a tempdir
    /// under test. Held rather than recomputed so the tests do not have to
    /// mutate process-global environment variables to say where "state" is.
    environments_base: PathBuf,
    /// Test seam, mirrored from [`Supervisor`]: the suite runs inside a
    /// container and must not get self-hosting semantics.
    outside_container_for_tests: bool,
    /// Which podman service this workspace's containers live on.
    ///
    /// Held by the registry rather than by each supervisor because it is a
    /// property of the workspace, not of an environment: **one machine
    /// hosts every environment**, which is the whole reason the substrate
    /// is affordable. Every supervisor gets a handle to this one, and every
    /// [`ExecContext`] is pointed at it, so `ide_exec`, terminals and the
    /// language server land wherever the containers actually are.
    /// Swappable, because resolving it can take twenty seconds — a VM has
    /// to boot — and the GTK thread may not wait for that. The registry
    /// therefore opens on the local host and learns the truth in
    /// [`Self::reconcile`], which runs on the runtime. Nothing is lost in
    /// the gap: environments are lazy, so there is no container yet that
    /// could be in the wrong place, and [`Self::set_substrate`] re-points
    /// every supervisor and every [`ExecContext`] the moment there is one.
    substrate: Mutex<Arc<Substrate>>,
    environments: Mutex<BTreeMap<EnvironmentId, Arc<Supervisor>>>,
    /// One keeper per VM the workspace has checkouts in, keyed by domain.
    /// The files service for every environment on that VM; connected on
    /// first need and kept for as long as it answers.
    keepers: Mutex<BTreeMap<String, Arc<Keeper>>>,
    /// One substrate per VM of the pool that hosts an environment, by
    /// domain: the resolved one and every other the registry brought up
    /// for a restored or newly placed environment. An environment's
    /// substrate is its VM's (`substrate_for`).
    substrates: Mutex<BTreeMap<String, Arc<Substrate>>>,
    /// Held while the primary is being placed: reconcile and a Rebuild
    /// pressed during the boot can both ask, and a checkout is made once.
    placing_primary: Mutex<()>,
    /// What the IDE serves down every environment channel, once the window
    /// has said. Held here as well as on each supervisor so an environment
    /// created later inherits it.
    channel_services: Mutex<Option<Arc<dyn crate::channel::ChannelServices>>>,
    /// **One** inotify instance for every environment's config, held here
    /// because the registry is the only thing that knows what the fleet is
    /// (`crate::configwatch` says why one rather than one each).
    config_watch: Arc<crate::configwatch::ConfigWatch>,
    /// Whether the disk budget's background walk is already running. One
    /// per workspace: a second would double the filesystem work and agree
    /// with the first about every number it produced.
    disk_meter_started: AtomicBool,
    /// Test seam: what `statvfs` would have said about the environments'
    /// volume. The floor is the one ceiling whose input the suite has to
    /// substitute for — a unit test cannot fill a disk, and a test that
    /// only passed on a full one would never run — so this stands in for
    /// the syscall and nothing else. `None` means ask the kernel;
    /// `Some(None)` means the kernel would not say, which is the
    /// fail-open case.
    free_disk_for_tests: Mutex<Option<Option<u64>>>,
}

impl EnvironmentRegistry {
    /// Open a workspace's environments. The primary is created immediately
    /// (a workspace always has a main checkout) with the workspace's own
    /// [`ExecContext`], which is why terminals and `ide_exec` keep working
    /// unchanged. Everything else appears through [`Self::reconcile`] or
    /// [`Self::create`].
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        events: EventBus,
        primary_exec: ExecContext,
    ) -> Arc<Self> {
        Self::build(
            workspace_root,
            events,
            primary_exec,
            environment::environments_base(),
            // Nothing until reconcile resolves the ladder: no environment
            // starts before its substrate is known.
            Arc::new(Substrate::unresolved()),
            false,
        )
    }

    #[doc(hidden)]
    pub fn new_for_tests(
        workspace_root: impl Into<PathBuf>,
        events: EventBus,
        primary_exec: ExecContext,
        environments_base: impl Into<PathBuf>,
    ) -> Arc<Self> {
        Self::build(
            workspace_root,
            events,
            primary_exec,
            environments_base.into(),
            Substrate::host_for_tests(),
            true,
        )
    }

    fn build(
        workspace_root: impl Into<PathBuf>,
        events: EventBus,
        primary_exec: ExecContext,
        environments_base: PathBuf,
        substrate: Arc<Substrate>,
        outside_container_for_tests: bool,
    ) -> Arc<Self> {
        let workspace_root = workspace_root.into();
        // The primary's context predates the registry (the workspace hands
        // it in), so it is pointed at the substrate here rather than at
        // construction. Every other context is created below and pointed at
        // the same one.
        primary_exec.set_podman_target(substrate.target().clone());
        let registry = Arc::new(Self {
            workspace_root: workspace_root.clone(),
            events: events.clone(),
            environments_base,
            outside_container_for_tests,
            substrate: Mutex::new(substrate),
            environments: Mutex::new(BTreeMap::new()),
            channel_services: Mutex::new(None),
            config_watch: crate::configwatch::ConfigWatch::new(),
            disk_meter_started: AtomicBool::new(false),
            free_disk_for_tests: Mutex::new(None),
            keepers: Mutex::new(BTreeMap::new()),
            substrates: Mutex::new(BTreeMap::new()),
            placing_primary: Mutex::new(()),
        });
        let primary =
            registry.make_supervisor(EnvironmentIdentity::primary(workspace_root), primary_exec);
        // The primary can put itself where it runs when a start finds it
        // still on this host — a Rebuild pressed before reconcile placed
        // it — through the registry, which owns placement.
        primary.set_placer(Arc::new({
            let registry = Arc::downgrade(&registry);
            move || {
                let registry = registry
                    .upgrade()
                    .context("the environment registry is gone")?;
                registry.place_primary_now().map(|_| ())
            }
        }));
        registry
            .environments
            .lock()
            .unwrap()
            .insert(EnvironmentId::primary(), primary);
        registry
    }

    fn make_supervisor(&self, identity: EnvironmentIdentity, exec: ExecContext) -> Arc<Supervisor> {
        // The substrate this checkout can run on — and the exec context
        // aimed at it before the supervisor can resolve a single command
        // against it.
        let substrate = self.substrate_for(&identity.checkout);
        exec.set_podman_target(substrate.target().clone());
        let supervisor = if self.outside_container_for_tests {
            Supervisor::new_outside_container_for_tests(
                identity,
                self.events.clone(),
                exec,
                substrate,
            )
        } else {
            Supervisor::new(identity, self.events.clone(), exec, substrate)
        };
        // An environment created after the window wired itself up must be
        // able to host an agent too — the alternative is relocation working
        // for environments that existed at startup and silently not for the
        // ones a chat makes for itself, which is the common case.
        if let Some(services) = self.channel_services.lock().unwrap().clone() {
            supervisor.set_channel_services(services);
        }
        supervisor
    }

    /// Tell every environment — present and future — what the IDE serves
    /// down its channel.
    ///
    /// Called once by the window. It cannot be a constructor argument: the
    /// MCP server on the other end is built *from* this registry.
    pub fn set_channel_services(&self, services: Arc<dyn crate::channel::ChannelServices>) {
        *self.channel_services.lock().unwrap() = Some(services.clone());
        for supervisor in self.environments.lock().unwrap().values() {
            supervisor.set_channel_services(services.clone());
        }
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// The substrate the ladder resolved for this workspace: its VM, when
    /// it has one, or the machine, connection, or local podman the ladder
    /// ended on. Where a checkout that is IN the VM runs, and what the
    /// window stops when it closes — not necessarily where every
    /// environment's containers are; see [`Self::substrate_for`].
    pub fn substrate(&self) -> Arc<Substrate> {
        self.substrate.lock().unwrap().clone()
    }

    /// Where a checkout's containers run. **The substrate follows the
    /// checkout.** A checkout in a VM runs in that VM; a checkout on this
    /// host runs on the host's podman even when the workspace has a VM,
    /// because a VM shares no filesystem and a host path bound into a
    /// container there would fail at the bind. Until every checkout has
    /// moved, a workspace is therefore half on its VM and half on the host
    /// by design, and each environment's exec context is aimed at its own
    /// half.
    pub fn substrate_for(&self, checkout: &taste_core::environment::Checkout) -> Arc<Substrate> {
        let resolved = self.substrate();
        if resolved.can_host(checkout) {
            return resolved;
        }
        // A checkout in another VM of the pool: that VM's substrate, when
        // the registry has brought it up.
        if let taste_core::environment::Checkout::Remote { vm, .. } = checkout {
            if let Some(substrate) = self.substrates.lock().unwrap().get(vm) {
                return substrate.clone();
            }
        }
        // Nothing can run it — a checkout still on this host beside a VM
        // substrate, or one in a VM the pool no longer has. The resolved
        // substrate is returned so the refusal names the real situation
        // (`Substrate::refusal`); there is no host rung to fall to.
        resolved
    }

    /// Every VM of the pool the registry has brought up, by domain: what
    /// the window stops when it closes.
    pub fn vm_domains(&self) -> Vec<String> {
        let mut domains: Vec<String> = self.substrates.lock().unwrap().keys().cloned().collect();
        if let Some(vm) = self.substrate().vm_details() {
            if !domains.contains(&vm.domain) {
                domains.push(vm.domain.clone());
            }
        }
        domains
    }

    /// The substrate of one VM of the pool, when the registry has it.
    pub fn substrate_of_vm(&self, domain: &str) -> Option<Arc<Substrate>> {
        if let Some(vm) = self.substrate().vm_details() {
            if vm.domain == domain {
                return Some(self.substrate());
            }
        }
        self.substrates.lock().unwrap().get(domain).cloned()
    }

    /// Record a VM of the pool as brought up, and point every environment
    /// whose checkout is in it at it.
    fn register_vm(&self, vm: &Vm, facts: crate::provision::VmFacts) -> Arc<Substrate> {
        let sandboxed = self.substrate().target().sandboxed();
        let substrate = Arc::new(Substrate::vm(vm, facts, sandboxed));
        self.substrates
            .lock()
            .unwrap()
            .insert(vm.domain.clone(), substrate.clone());
        for supervisor in self.list() {
            if supervisor.checkout().vm() == Some(vm.domain.as_str()) {
                supervisor.set_substrate(substrate.clone());
            }
        }
        substrate
    }

    /// Point the workspace at the substrate the ladder resolved.
    ///
    /// Every supervisor and every [`ExecContext`] together, in one place —
    /// each at the substrate its own checkout can run on
    /// ([`Self::substrate_for`]).
    pub fn set_substrate(&self, substrate: Arc<Substrate>) {
        if let (Some(vm), Some(facts)) = (substrate.vm_details(), substrate.vm_facts()) {
            self.substrates.lock().unwrap().insert(
                vm.domain.clone(),
                Arc::new(Substrate::vm(
                    vm,
                    facts.clone(),
                    substrate.target().sandboxed(),
                )),
            );
        }
        *self.substrate.lock().unwrap() = substrate;
        for supervisor in self.environments.lock().unwrap().values() {
            supervisor.set_substrate(self.substrate_for(&supervisor.checkout()));
        }
    }

    /// Ask every environment whether the container it believes in is still
    /// there, and demote the ones that are not.
    ///
    /// The case this exists for is a **recreated machine**: the substrate
    /// is cattle, the containers die with it, and the supervisors' state
    /// lives on the host and does not. Called at reconcile time and
    /// available to anything that has reason to suspect the substrate moved
    /// under it.
    pub async fn reconcile_containers(&self) {
        for supervisor in self.list() {
            supervisor.reconcile_container_presence().await;
        }
    }

    /// What the agent environments take on disk right now, as last
    /// measured. Cheap: a sum over cached samples, no filesystem touched.
    pub fn disk_budget(&self) -> DiskBudget {
        let now = std::time::Instant::now();
        let mut budget = DiskBudget {
            budget_bytes: environment::MAX_ORCHESTRATED_DISK_BYTES,
            ..DiskBudget::default()
        };
        for supervisor in self.list() {
            if supervisor.id().is_primary() {
                continue;
            }
            match supervisor.measured_disk() {
                None => budget.unmeasured += 1,
                Some(sample) => {
                    budget.used_bytes += sample.budget_bytes;
                    budget.measured += 1;
                    budget.unmeasured_volumes += sample.unmeasured_volumes;
                    let age = now.saturating_duration_since(sample.at).as_secs();
                    budget.oldest_seconds =
                        Some(budget.oldest_seconds.map_or(age, |old| old.max(age)));
                }
            }
        }
        budget
    }

    /// What the disk under the environments has left, asked now.
    ///
    /// The environments' own volume rather than the workspace's: the clone
    /// lands here, so this is the filesystem a clone can fill. Answered even
    /// before the directory exists — `free_bytes` walks up to the nearest
    /// ancestor that does — which is what a workspace whose first
    /// environment has yet to be created needs.
    pub fn free_disk(&self) -> FreeDisk {
        FreeDisk {
            free_bytes: match *self.free_disk_for_tests.lock().unwrap() {
                Some(pretend) => pretend,
                None => environment::free_bytes(&self.environments_base),
            },
            floor_bytes: environment::MIN_FREE_DISK_BYTES,
            volume: self.environments_base.clone(),
        }
    }

    /// Answer [`Self::free_disk`] with this instead of asking the kernel.
    #[doc(hidden)]
    pub fn set_free_disk_for_tests(&self, free_bytes: Option<u64>) {
        *self.free_disk_for_tests.lock().unwrap() = Some(free_bytes);
    }

    /// Walk every agent environment once and cache what it costs.
    ///
    /// One at a time rather than joined: this is filesystem work on a
    /// machine whose user is compiling, and six concurrent walks would be
    /// the IDE competing with the build it exists to run.
    pub async fn measure_disk(&self) {
        for supervisor in self.list() {
            if supervisor.id().is_primary() {
                continue;
            }
            supervisor
                .measure_disk(environment::DISK_BUDGET_SCOPE)
                .await;
        }
    }

    /// Start the background measurement: a walk now, and another every
    /// [`environment::DiskBudgetScope::measurement_interval`].
    ///
    /// The cadence is here rather than in the window because the budget is
    /// a fact about the workspace, and the gates that read it are served on
    /// sockets that answer whether or not anyone has the IDE open. It stops
    /// when the registry is dropped — the task holds a `Weak`, so a closed
    /// workspace does not keep walking its own disk.
    pub fn start_disk_meter(self: &Arc<Self>) {
        if self.disk_meter_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let interval = environment::DISK_BUDGET_SCOPE.measurement_interval();
            loop {
                let Some(registry) = weak.upgrade() else {
                    break;
                };
                registry.measure_disk().await;
                drop(registry);
                tokio::time::sleep(interval).await;
            }
        });
    }

    /// The IDE-owned directory holding one environment's state.
    pub fn env_dir(&self, id: &EnvironmentId) -> PathBuf {
        self.environments_base
            .join(environment::workspace_key(&self.workspace_root))
            .join(id.as_str())
    }

    /// An environment's checkout: the main one for the primary, its clone
    /// otherwise.
    pub fn env_repo(&self, id: &EnvironmentId) -> PathBuf {
        if id.is_primary() {
            self.workspace_root.clone()
        } else {
            self.env_dir(id).join("repo")
        }
    }

    /// The environment backing the main checkout. Always present.
    pub fn primary(&self) -> Arc<Supervisor> {
        self.get(&EnvironmentId::primary())
            .expect("the primary environment always exists")
    }

    pub fn get(&self, id: &EnvironmentId) -> Option<Arc<Supervisor>> {
        self.environments.lock().unwrap().get(id).cloned()
    }

    /// Every environment, primary first, then the rest by slug.
    pub fn list(&self) -> Vec<Arc<Supervisor>> {
        let environments = self.environments.lock().unwrap();
        let mut out: Vec<Arc<Supervisor>> = environments
            .values()
            .filter(|s| !s.id().is_primary())
            .cloned()
            .collect();
        if let Some(primary) = environments.get(&EnvironmentId::primary()) {
            out.insert(0, primary.clone());
        }
        out
    }

    pub fn ids(&self) -> Vec<EnvironmentId> {
        self.list().iter().map(|s| s.id().clone()).collect()
    }

    /// Register a supervisor for an environment whose clone already exists.
    ///
    /// Both entry points come through here — a freshly cloned environment
    /// and one restored from disk — and both announce themselves the same
    /// way, because a restored environment needs its MCP socket bound
    /// exactly as much as a new one does.
    fn adopt(&self, id: EnvironmentId) -> Arc<Supervisor> {
        self.adopt_identity(self.identity_on_disk(&id))
    }

    fn adopt_identity(&self, identity: EnvironmentIdentity) -> Arc<Supervisor> {
        let id = identity.id.clone();
        // A fresh context per environment: each supervisor points its own
        // at its own container. There is no shared target to race over, and
        // a clone never inherits the self-hosting "the IDE's container is
        // the environment" flag — that is true of the primary alone.
        let supervisor = self.make_supervisor(identity, ExecContext::for_cloned_environment());
        // A checkout in a VM whose keeper is already connected gets its
        // files at once; one whose keeper is not is connected by
        // `reconcile`, and refuses reads by name until then.
        if let Checkout::Remote { vm, .. } = supervisor.checkout() {
            let keeper = self.keepers.lock().unwrap().get(&vm).cloned();
            if let Some(keeper) = keeper {
                supervisor.set_keeper(keeper);
            }
        }
        self.environments
            .lock()
            .unwrap()
            .insert(id.clone(), supervisor.clone());
        self.events.publish(Event::EnvironmentCreated { env: id });
        supervisor
    }

    /// [`Self::place_primary`] in the workspace's resolved VM, one caller
    /// at a time. Refuses, saying so, while the VM is still coming up —
    /// reconcile places the primary and starts it on its own once it is —
    /// and when the workspace resolved to something with no VM in it.
    pub fn place_primary_now(&self) -> Result<Option<crate::peer::PeerSync>> {
        let _one_at_a_time = self.placing_primary.lock().unwrap();
        let substrate = self.substrate();
        let Some(vm) = substrate.vm_details().cloned() else {
            if substrate.is_resolved() {
                bail!(
                    "this workspace runs on {}, which has no VM to place the checkout in",
                    substrate.provider().describe()
                );
            }
            bail!(
                "the workspace's VM is still coming up; the environment is placed in it and \
                 started on its own once it is"
            );
        };
        self.place_primary(&vm)
    }

    /// Put the primary's checkout in `vm`, or bring it and the user's
    /// folder up to date with each other when it is already there.
    ///
    /// The folder the user opened becomes the primary's **peer**: its refs
    /// go into a checkout made in the VM by push, its uncommitted work goes
    /// as a snapshot ref and is restored over there, and from then on the
    /// panes show the checkout in the VM (`Event::CheckoutMoved`). The
    /// folder's own working tree is fast-forwarded when it is clean, and
    /// otherwise left with a note (`crate::peer::sync_primary_peer`). A
    /// folder that is not a git repository has nothing to move and stays.
    /// Blocking; the peer sync it did is returned for the log.
    pub fn place_primary(&self, vm: &Vm) -> Result<Option<crate::peer::PeerSync>> {
        let primary = self.primary();
        let peer = self.workspace_root.clone();
        let Some(host) = taste_git::GitWorkspace::discover(&peer) else {
            tracing::info!(
                "{} is not a git repository; the primary environment stays on this machine",
                peer.display()
            );
            return Ok(None);
        };
        let keeper = self.keeper_for(vm)?;
        let files = Files::Remote(keeper.clone());
        let path = crate::provision::guest_checkout_path(&peer, &EnvironmentId::primary());
        let keys = crate::keys::Keys::for_workspace(&peer);
        let sync = if files.exists(&path.join(".git")) {
            Some(crate::peer::sync_primary_peer(&peer, vm, &keys, &path)?)
        } else {
            let branch = host.branch_name().unwrap_or_else(|| "main".to_string());
            let workspace_dir = crate::provision::guest_workspace_dir(&peer);
            let run = |cwd: &Path, argv: &[&str]| -> Result<()> {
                let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
                let out = files
                    .exec(cwd, &argv)
                    .with_context(|| format!("running {} in VM {}", argv.join(" "), vm.domain))?;
                if !out.success() {
                    bail!(
                        "{} in VM {}: {}",
                        argv.join(" "),
                        vm.domain,
                        out.stderr_utf8().trim()
                    );
                }
                Ok(())
            };
            files.mkdir_all(&workspace_dir)?;
            let target = path.display().to_string();
            run(
                &workspace_dir,
                &["git", "init", "-q", "--initial-branch", &branch, &target],
            )?;
            run(
                &path,
                &[
                    "git",
                    "config",
                    "receive.denyCurrentBranch",
                    "updateInstead",
                ],
            )?;
            // The user's identity for commits made over there, when the
            // folder has one: git refuses to commit as nobody.
            for key in ["user.name", "user.email"] {
                let value = std::process::Command::new("git")
                    .args(["-C", &peer.display().to_string(), "config", "--get", key])
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .filter(|v| !v.is_empty());
                if let Some(value) = value {
                    run(&path, &["git", "config", key, &value])?;
                }
            }
            // Uncommitted work travels as a snapshot ref and is put back
            // over there as exactly what it was. Whose: the folder's, when
            // the folder has changes of its own — the user edited here, on
            // purpose — and otherwise the peer's last snapshot, which is
            // the checkout's own last state in the VM that is gone
            // (docs/ENVIRONMENTS.md → "Uncommitted work, backups, and
            // artifacts": moving is restore).
            let snapshot_ref = taste_git::snapshot_ref(EnvironmentId::primary().as_str());
            let previous = host.read_ref(&snapshot_ref)?;
            let dirty = !host.status()?.is_empty();
            if dirty {
                host.snapshot_worktree(&snapshot_ref)?;
            }
            crate::peer::push_to_guest(
                &peer,
                vm,
                &keys,
                &path,
                &crate::peer::PRIMARY_SEED_REFSPECS,
            )?;
            if dirty || previous.is_some() {
                let restore = taste_git::snapshot::restore_script(&snapshot_ref, true)?;
                run(&path, &["sh", "-c", &restore])?;
                tracing::info!(
                    "restored {} uncommitted work in VM {} from {snapshot_ref}",
                    if dirty {
                        "the folder's"
                    } else {
                        "the last snapshot's"
                    },
                    vm.domain
                );
            }
            None
        };
        let checkout = Checkout::Remote {
            vm: vm.domain.clone(),
            path: path.clone(),
        };
        primary.set_checkout(checkout.clone());
        primary.set_substrate(self.substrate_for(&checkout));
        primary.set_keeper(keeper);
        self.events.publish(Event::CheckoutMoved {
            env: EnvironmentId::primary(),
            checkout,
            files,
        });
        Ok(sync)
    }

    /// Where the environment's checkout is, as recorded when it was made:
    /// in a VM, by the placement file beside its peer; on this host
    /// otherwise, where the clone is the checkout.
    fn identity_on_disk(&self, id: &EnvironmentId) -> EnvironmentIdentity {
        let repo = self.env_repo(id);
        match Placement::read(&self.env_dir(id)) {
            Some(placement) => EnvironmentIdentity {
                id: id.clone(),
                workspace_root: self.workspace_root.clone(),
                checkout: Checkout::Remote {
                    vm: placement.vm,
                    path: placement.path,
                },
                peer: repo,
            },
            None => EnvironmentIdentity::local_at(self.workspace_root.clone(), id.clone(), repo),
        }
    }

    /// The keeper for `vm` — the files service every checkout in that VM
    /// is reached through — connected on first need. Blocking: it may
    /// build the baseline image in the guest and exec into a container.
    pub fn keeper_for(&self, vm: &Vm) -> Result<Arc<Keeper>> {
        if let Some(keeper) = self.keepers.lock().unwrap().get(&vm.domain) {
            if keeper.alive() {
                return Ok(keeper.clone());
            }
        }
        let Some(substrate) = self.substrate_of_vm(&vm.domain) else {
            bail!(
                "{} is not a VM this workspace has brought up (its substrate is {})",
                vm.domain,
                self.substrate().provider().describe()
            );
        };
        let container = crate::keeper::ensure_container(&substrate, vm, &self.workspace_root)?;
        let keeper = Keeper::in_container(&substrate, &container, format!("VM {}", vm.domain))?;
        self.keepers
            .lock()
            .unwrap()
            .insert(vm.domain.clone(), keeper.clone());
        Ok(keeper)
    }

    /// How many environments each VM of the pool hosts now.
    fn occupancy(&self) -> std::collections::HashMap<String, usize> {
        let mut occupancy = std::collections::HashMap::new();
        for supervisor in self.list() {
            if let Some(vm) = supervisor.checkout().vm() {
                *occupancy.entry(vm.to_string()).or_insert(0) += 1;
            }
        }
        occupancy
    }

    /// The VM one more environment goes on: the pool's choice by capacity
    /// (`Pool::place`), brought up and registered. Blocking, from the
    /// runtime's blocking pool — creation runs there.
    fn place_by_capacity(&self) -> Result<Vm> {
        let pool = crate::pool::Pool::new(&self.workspace_root);
        let occupancy = self.occupancy();
        let handle = tokio::runtime::Handle::try_current()
            .context("placing an environment needs the runtime")?;
        let placed =
            handle.block_on(pool.place(&occupancy, Arc::new(crate::substrate::report_download)));
        let (vm, facts) = match placed {
            Ok(placed) => placed,
            Err(crate::pool::PoolError::AtCapacity {
                running,
                committed_mib,
                host_mib,
            }) => bail!(
                "every VM of this workspace hosts {} environments and the host has no room \
                 for another ({running} running, {:.1} of {:.1} GiB committed)",
                crate::pool::MAX_ENVIRONMENTS_PER_VM,
                committed_mib as f64 / 1024.0,
                host_mib as f64 / 1024.0
            ),
            Err(crate::pool::PoolError::Skipped) => bail!("a probe run provisions nothing"),
            Err(crate::pool::PoolError::Unavailable(e)) => {
                return Err(e).context("the workspace's provisioner is not available")
            }
            Err(crate::pool::PoolError::Failed { domain, error }) => {
                return Err(error).with_context(|| match domain {
                    Some(domain) => format!("bringing up VM {domain}"),
                    None => "making a VM".to_string(),
                })
            }
        };
        self.register_vm(&vm, facts);
        Ok(vm)
    }

    /// Move an environment whose checkout is a clone on this host — made
    /// before checkouts lived in VMs — into a VM of the pool.
    ///
    /// Its uncommitted work is snapshotted first, so the strip that turns
    /// the clone into a peer loses nothing; the checkout is made over
    /// there the way a new one is (`place_in_vm`: refs pushed, the
    /// snapshot ref among them, the clone stripped, the placement
    /// recorded), and the snapshot is restored over it. Blocking; returns
    /// the VM.
    fn migrate_environment(&self, id: &EnvironmentId) -> Result<String> {
        let supervisor = self
            .get(id)
            .with_context(|| format!("no environment {id}"))?;
        let Checkout::Local(repo) = supervisor.checkout() else {
            bail!("environment {id}'s checkout is already in a VM");
        };
        let git = taste_git::GitWorkspace::discover(&repo)
            .with_context(|| format!("{} is not a git repository", repo.display()))?;
        let snapshot_ref = taste_git::snapshot_ref(id.as_str());
        let dirty = !git.status()?.is_empty();
        if dirty {
            git.snapshot_worktree(&snapshot_ref)
                .context("snapshotting the clone's uncommitted work before the move")?;
        }
        let vm = self.place_by_capacity()?;
        let identity = self.place_in_vm(id, &repo, &vm)?;
        let keeper = self.keeper_for(&vm)?;
        if dirty {
            let files = Files::Remote(keeper.clone());
            let restore = taste_git::snapshot::restore_script(&snapshot_ref, true)?;
            let out = files
                .exec(
                    identity.checkout.path(),
                    &["sh".into(), "-c".into(), restore],
                )
                .with_context(|| {
                    format!("restoring {id}'s uncommitted work in VM {}", vm.domain)
                })?;
            if !out.success() {
                bail!(
                    "restoring {id}'s uncommitted work in VM {}: {}",
                    vm.domain,
                    out.stderr_utf8().trim()
                );
            }
        }
        supervisor.set_checkout(identity.checkout.clone());
        supervisor.set_substrate(self.substrate_for(&identity.checkout));
        supervisor.set_keeper(keeper);
        Ok(vm.domain)
    }

    /// Place an environment whose VM is gone into a VM the pool has, from
    /// what this host kept: the peer's refs and its last snapshot.
    ///
    /// The checkout is made the way a new one is (`place_in_vm`'s
    /// transport, without the strip — the peer is already refs only), the
    /// branch the snapshot was taken on is checked out (the branch whose
    /// tip is the snapshot's parent; detached at that commit when no branch
    /// names it), and the snapshot is restored over it
    /// (`taste_git::snapshot::restore_script`). What the agent had is what
    /// it has again, as `git status` shows it; the chat comes back from
    /// this host's stash on its own. Blocking; returns the VM.
    fn replace_environment(&self, id: &EnvironmentId) -> Result<String> {
        let supervisor = self
            .get(id)
            .with_context(|| format!("no environment {id}"))?;
        let peer = supervisor.peer().to_path_buf();
        let git = taste_git::GitWorkspace::discover(&peer)
            .with_context(|| format!("{} is not a git repository", peer.display()))?;
        let vm = self.place_by_capacity()?;
        let keeper = self.keeper_for(&vm)?;
        let files = Files::Remote(keeper.clone());
        let path = crate::provision::guest_checkout_path(&self.workspace_root, id);
        let workspace_dir = crate::provision::guest_workspace_dir(&self.workspace_root);
        let run = |cwd: &Path, argv: &[&str]| -> Result<String> {
            let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            let out = files
                .exec(cwd, &argv)
                .with_context(|| format!("running {} in VM {}", argv.join(" "), vm.domain))?;
            if !out.success() {
                bail!(
                    "{} in VM {}: {}",
                    argv.join(" "),
                    vm.domain,
                    out.stderr_utf8().trim()
                );
            }
            Ok(out.stdout_utf8())
        };
        if files.exists(&path) {
            bail!(
                "{} already exists in VM {}; it is not this environment's",
                path.display(),
                vm.domain
            );
        }
        files.mkdir_all(&workspace_dir)?;
        run(
            &workspace_dir,
            &["git", "init", "-q", &path.display().to_string()],
        )?;
        run(
            &path,
            &[
                "git",
                "config",
                "receive.denyCurrentBranch",
                "updateInstead",
            ],
        )?;
        let keys = crate::keys::Keys::for_workspace(&self.workspace_root);
        crate::peer::push_to_guest(&peer, &vm, &keys, &path, &crate::peer::PEER_REFSPECS)?;
        // Where HEAD was: the snapshot's first parent, and the branch that
        // names it. Without a snapshot, the one branch the peer has.
        let snapshot_ref = taste_git::snapshot_ref(id.as_str());
        let base = git
            .read_ref(&snapshot_ref)?
            .and_then(|snapshot| git.first_parent(snapshot).ok().flatten());
        let branches: Vec<_> = git
            .refs_under("refs/heads/")?
            .into_iter()
            .filter(|(name, _)| {
                name != &format!("refs/heads/{}", taste_git::clone::PEER_HEAD_BRANCH)
            })
            .collect();
        let at = base.and_then(|base| {
            branches
                .iter()
                .find(|(_, oid)| *oid == base)
                .map(|(name, _)| name.trim_start_matches("refs/heads/").to_string())
        });
        match (at, base, branches.as_slice()) {
            (Some(branch), _, _) => {
                run(&path, &["git", "checkout", "-q", &branch])?;
            }
            (None, Some(base), _) => {
                run(
                    &path,
                    &["git", "checkout", "-q", "--detach", &base.to_string()],
                )?;
            }
            (None, None, [(name, _)]) => {
                let branch = name.trim_start_matches("refs/heads/").to_string();
                run(&path, &["git", "checkout", "-q", &branch])?;
            }
            (None, None, _) => {
                bail!("environment {id}'s peer has no snapshot and no single branch to check out")
            }
        }
        if base.is_some() {
            let restore = taste_git::snapshot::restore_script(&snapshot_ref, true)?;
            run(&path, &["sh", "-c", &restore])?;
        }
        Placement {
            vm: vm.domain.clone(),
            path: path.clone(),
        }
        .write(&self.env_dir(id))?;
        let checkout = Checkout::Remote {
            vm: vm.domain.clone(),
            path,
        };
        supervisor.set_checkout(checkout.clone());
        supervisor.set_substrate(self.substrate_for(&checkout));
        supervisor.set_keeper(keeper);
        Ok(vm.domain)
    }

    /// Put a freshly cloned environment's checkout into `vm`, leaving the
    /// clone behind as its peer.
    ///
    /// The checkout is made by git's own transport: an empty repository in
    /// the guest, the peer's refs pushed into it over the VM's ssh forward
    /// (`receive.denyCurrentBranch=updateInstead` makes the push check the
    /// branch out), then the peer stripped to refs and objects. Blocking,
    /// on the caller's thread — this runs from `create`, off the GTK
    /// thread by its callers' contract.
    fn place_in_vm(&self, id: &EnvironmentId, peer: &Path, vm: &Vm) -> Result<EnvironmentIdentity> {
        let keeper = self.keeper_for(vm)?;
        let files = Files::Remote(keeper);
        let path = crate::provision::guest_checkout_path(&self.workspace_root, id);
        let branch = taste_git::GitWorkspace::discover(peer)
            .and_then(|git| git.branch_name())
            .unwrap_or_else(|| "main".to_string());
        let workspace_dir = crate::provision::guest_workspace_dir(&self.workspace_root);
        let run = |cwd: &Path, argv: &[&str]| -> Result<()> {
            let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            let out = files
                .exec(cwd, &argv)
                .with_context(|| format!("running {} in VM {}", argv.join(" "), vm.domain))?;
            if !out.success() {
                bail!(
                    "{} in VM {}: {}",
                    argv.join(" "),
                    vm.domain,
                    out.stderr_utf8().trim()
                );
            }
            Ok(())
        };
        if files.exists(&path) {
            bail!(
                "{} already exists in VM {}; an environment's checkout is made once",
                path.display(),
                vm.domain
            );
        }
        files.mkdir_all(&workspace_dir)?;
        run(
            &workspace_dir,
            &[
                "git",
                "init",
                "-q",
                "--initial-branch",
                &branch,
                &path.display().to_string(),
            ],
        )?;
        run(
            &path,
            &[
                "git",
                "config",
                "receive.denyCurrentBranch",
                "updateInstead",
            ],
        )?;
        let keys = crate::keys::Keys::for_workspace(&self.workspace_root);
        crate::peer::push_to_guest(peer, vm, &keys, &path, &crate::peer::PEER_REFSPECS)?;
        taste_git::strip_worktree(peer)?;
        Placement {
            vm: vm.domain.clone(),
            path: path.clone(),
        }
        .write(&self.env_dir(id))?;
        Ok(EnvironmentIdentity {
            id: id.clone(),
            workspace_root: self.workspace_root.clone(),
            checkout: Checkout::Remote {
                vm: vm.domain.clone(),
                path,
            },
            peer: peer.to_path_buf(),
        })
    }

    /// Watch one environment's config for drift, on the fleet's single
    /// inotify instance.
    ///
    /// This replaced `Supervisor::start_watching`, which opened an instance
    /// per environment — a per-uid resource capped at 128 that the user's
    /// desktop session spends from as well, so a fleet could quietly run
    /// the IDE out of it and each environment's failure was a logged
    /// warning nobody would ever see (`crate::configwatch`).
    pub fn watch_config(&self, supervisor: &Arc<Supervisor>) -> Result<()> {
        self.config_watch.add(supervisor)
    }

    /// How many environments the fleet's one watcher is watching. For the
    /// tests, and for saying so in a log.
    pub fn watching_config(&self) -> usize {
        self.config_watch.watching()
    }

    /// Create a new environment: clone the main checkout, then supervise
    /// that clone.
    ///
    /// The clone is made with libgit2, which runs no hooks — cloning an
    /// untrusted repository must not execute any of its code. The container
    /// is *not* built here: environments are lazy by policy (clone on
    /// create, build on first need, agent on first prompt).
    pub fn create(&self, id: EnvironmentId) -> Result<Arc<Supervisor>> {
        if id.is_primary() {
            bail!("the primary environment is the main checkout; it is never created");
        }
        if self.get(&id).is_some() {
            bail!("environment {id} already exists");
        }
        let repo = self.env_repo(&id);
        if repo.exists() {
            bail!("{} already exists", repo.display());
        }
        taste_git::clone_local(&self.workspace_root, &repo)
            .with_context(|| format!("creating environment {id}"))?;
        // Where the checkout lives: in a VM of the workspace's pool, chosen
        // by capacity, and this clone becomes the peer that holds the
        // refs. On this host only for the test suites' host substrate,
        // where the clone is the checkout.
        let identity = if self.substrate().vm_details().is_some() {
            let placed = self
                .place_by_capacity()
                .and_then(|vm| self.place_in_vm(&id, &repo, &vm));
            match placed {
                Ok(identity) => identity,
                Err(e) => {
                    // Half an environment is worse than none: the peer goes
                    // with the failure, so a retry starts clean.
                    let _ = std::fs::remove_dir_all(self.env_dir(&id));
                    return Err(e).with_context(|| format!("placing environment {id} in a VM"));
                }
            }
        } else if self.substrate().can_host(&Checkout::Local(repo.clone())) {
            EnvironmentIdentity::local_at(self.workspace_root.clone(), id.clone(), repo)
        } else {
            let _ = std::fs::remove_dir_all(self.env_dir(&id));
            bail!(
                "environment {id} has nowhere to run: {}",
                self.substrate().refusal(&Checkout::Local(repo))
            );
        };
        let supervisor = self.adopt_identity(identity);
        // Supervised for real from its first second, the way a restored
        // environment is (see `reconcile`): the clone carries the project's
        // .devcontainer, and a supervisor left in NoConfig would report a
        // perfectly configured environment as "not configured" until the
        // next restart — which is what it did.
        if let Err(e) = supervisor.recheck() {
            tracing::warn!("environment {id} recheck failed: {e:#}");
        }
        if let Err(e) = self.watch_config(&supervisor) {
            tracing::warn!("environment {id} watcher failed: {e:#}");
        }
        Ok(supervisor)
    }

    /// Destroy an environment — but say what it held first.
    ///
    /// The enumeration happens before a single byte is removed, and the
    /// result comes back to the caller whether or not anything was found:
    /// the clone can be the only copy of an agent's unreviewed work, and a
    /// cleanup that quietly eats it is the worst failure this subsystem
    /// has. Images are deliberately left alone — they are shared between
    /// environments with identical config, and reclaiming them is a
    /// separate, explicit garbage-collection action.
    pub async fn destroy(&self, id: &EnvironmentId) -> Result<DestroyReport> {
        if id.is_primary() {
            bail!("the primary environment is the main checkout; it cannot be destroyed");
        }
        let Some(supervisor) = self.get(id) else {
            bail!("no environment {id}");
        };
        // The peer: what the main checkout has never seen is a question
        // about refs, and the dirty count is what the last snapshot holds
        // when the working copy is not here to ask.
        let repo = supervisor.peer().to_path_buf();

        let mut report = DestroyReport::default();
        if repo.is_dir() {
            report.unpublished =
                taste_git::unpublished_work(&repo, &self.workspace_root).unwrap_or_default();
            report.dirty_files = taste_git::GitWorkspace::discover(&repo)
                .and_then(|git| git.status().ok())
                .map(|status| status.len())
                .unwrap_or(0);
        }

        // Claims come off before the world holding them goes away. An
        // issue assigned to an environment that no longer exists is
        // unclaimable by anyone else and looks, in the queue, exactly like
        // work in progress — so the release leaves a comment saying what
        // happened rather than a silently unassigned issue.
        let workspace_root = self.workspace_root.clone();
        let claimant = id.to_string();
        report.released_claims = taste_git::GitWorkspace::discover(&workspace_root)
            .and_then(|git| {
                git.issue_release(&claimant, &format!("environment {claimant} was destroyed"))
                    .map_err(|e| tracing::warn!("releasing {claimant}'s claims: {e:#}"))
                    .ok()
            })
            .unwrap_or_default();

        // Container next: a running container holds the clone's mount.
        let _ = supervisor.stop().await;
        // A checkout in a VM goes after its container, through the files
        // service. Not fatal when it cannot — the VM may be gone — but
        // named in the report, for the reason on `kept_volumes`: the first
        // live run left one behind and the only trace was a log line.
        if let Checkout::Remote { vm, path } = supervisor.checkout().clone() {
            let files = supervisor.files();
            let target = path.clone();
            let removed = tokio::task::spawn_blocking(move || files.remove(&target, true)).await;
            match removed {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!("removing {id}'s checkout from VM {vm}: {e}");
                    report.kept_checkout = Some(format!("{} in VM {vm} ({e})", path.display()));
                }
                Err(e) => {
                    report.kept_checkout = Some(format!(
                        "{} in VM {vm} (the removal did not finish: {e})",
                        path.display()
                    ));
                }
            }
        }
        for volume in supervisor.env_volumes() {
            match supervisor.remove_volume(&volume).await {
                Ok(()) => report.removed_volumes.push(volume),
                // Not fatal — the clone and the container still go, and an
                // environment half-destroyed would be worse than one that
                // left a volume behind — but never silent, for the reason
                // on `kept_volumes`.
                Err(e) => {
                    tracing::warn!("destroying {id}: leaving volume {volume} behind: {e:#}");
                    report.kept_volumes.push(volume);
                }
            }
        }

        let env_dir = self.env_dir(id);
        if env_dir.is_dir() {
            // **Not `?`.** This was the one fallible step between the world
            // changing and anyone being told, and `remove_dir_all` is not
            // atomic: it deletes depth-first and can stop partway on a busy
            // file, a mount a container still holds, or a directory written
            // by a mapped uid. When it did, the early return skipped
            // everything below — the registry kept the environment, the
            // supervisor stayed in the map, and `EnvironmentRemoved` was
            // never published, so the fan-out that aims the panes home
            // never ran. The panes then sat on a directory that was mostly
            // deleted, and the file tree read `not a git repository` off a
            // fresh look while its cached handle still answered `main`
            // (David, 2026-09-17: "Seems like a bug").
            //
            // An environment whose container and volumes are gone and whose
            // clone is half-deleted is gone. Saying so and naming the
            // leftover is strictly better than believing in it.
            match std::fs::remove_dir_all(&env_dir) {
                Ok(()) => report.removed_clone = Some(env_dir),
                Err(e) => {
                    tracing::warn!(
                        "destroying {id}: leaving {} behind: {e:#}",
                        env_dir.display()
                    );
                    report.kept_clone = Some(env_dir);
                }
            }
        }
        // The fleet's watcher holds descriptors on a directory that no
        // longer exists. A dropped supervisor used to take its own watcher
        // with it; now the only thing that can let go is this.
        self.config_watch.forget(&repo);
        self.environments.lock().unwrap().remove(id);
        // Said last, when the environment really is gone: the MCP server
        // unbinds its socket on this, and a socket that still answered
        // would be an identity with nothing behind it.
        self.events
            .publish(Event::EnvironmentRemoved { env: id.clone() });
        Ok(report)
    }

    /// Match the registry to what is actually on disk and in podman.
    ///
    /// Two jobs, both at startup: pick the workspace's existing environment
    /// clones back up, and remove what the single-environment naming scheme
    /// left behind. The sweep reports itself once through the event bus and
    /// the app log — a reset the user is not told about looks like a bug.
    pub async fn reconcile(self: &Arc<Self>) -> ReconcileReport {
        // Where the containers live, before anything asks podman anything.
        // This is the first await of the workspace's life and the only
        // place a VM is allowed to cost a minute — or, once per machine,
        // the guest image's download, which is said before it starts
        // because a gigabyte with no explanation is a hang.
        let pool = crate::pool::Pool::new(&self.workspace_root);
        if pool.will_download() {
            let notice = "Fetching the guest image for this machine's VMs (about 1 GiB, once)";
            taste_core::app_log::push("info", "substrate", notice);
            self.events.publish(Event::Toast(notice.to_string()));
        }
        // The download drawn in the window as it runs: every phase change
        // and every few megabytes, never every chunk — the bus reaches the
        // GTK thread, and a gigabyte arrives in a great many chunks.
        let reporter = {
            let events = self.events.clone();
            let last: Mutex<Option<(std::time::Instant, taste_core::GuestImageFetch)>> =
                Mutex::new(None);
            Arc::new(move |fetch: taste_core::GuestImageFetch| {
                crate::substrate::report_download(fetch.clone());
                let mut last = last.lock().unwrap();
                let publish = match &*last {
                    None => true,
                    Some((at, previous)) => {
                        previous.phase != fetch.phase
                            || fetch.done == fetch.total
                            || fetch.done.saturating_sub(previous.done) >= 4 * 1024 * 1024
                            || at.elapsed() >= std::time::Duration::from_millis(500)
                    }
                };
                if publish {
                    *last = Some((std::time::Instant::now(), fetch.clone()));
                    events.publish(Event::GuestImage(fetch));
                }
            })
        };
        self.set_substrate(Substrate::resolve_with(&self.workspace_root, reporter).await);

        let substrate = self.substrate();
        // The legacy scheme's containers were made before any checkout
        // could be in a VM, so they are wherever a local checkout runs.
        let mut swept = reconcile::sweep_legacy_resources(
            &self.workspace_root,
            &self.substrate_for(&taste_core::environment::Checkout::Local(
                self.workspace_root.clone(),
            )),
        )
        .await;
        // VMs whose workspaces have left this machine. Asked only where a
        // VM could have been made, so a host without libvirt is not asked
        // anything.
        if substrate.vm_details().is_some() || crate::pool::Pool::provisioning_allowed() {
            if let Ok(stale) = pool.stale().await {
                swept.stale_vms = stale.into_iter().map(|vm| vm.domain).collect();
            }
        }
        let mut report = ReconcileReport {
            restored: self.restore_from_disk(),
            swept,
        };
        report.restored.sort();

        // The pool's other VMs: every VM a restored environment's checkout
        // is in is brought up and registered, so its environments have
        // their substrate; a VM the pool no longer has leaves its
        // environments to be placed anew below.
        if substrate.vm_details().is_some() {
            let wanted: std::collections::BTreeSet<String> = self
                .list()
                .into_iter()
                .filter_map(|s| s.checkout().vm().map(str::to_string))
                .filter(|vm| self.substrate_of_vm(vm).is_none())
                .collect();
            for domain in wanted {
                match pool.ensure_vm(&domain).await {
                    Ok((vm, facts)) => {
                        self.register_vm(&vm, facts);
                    }
                    Err(e) => {
                        let note = format!(
                            "VM {domain} holds environments of this workspace and could not \
                             be brought up ({e:?})"
                        );
                        taste_core::app_log::push("warn", "environments", &note);
                    }
                }
            }
        }
        // Checkouts in a VM get their files service now that the VM is
        // up, one keeper per VM. Off the reactor: connecting a keeper may
        // build an image.
        let mut by_vm: std::collections::BTreeMap<String, Vec<Arc<Supervisor>>> =
            std::collections::BTreeMap::new();
        for supervisor in self.list() {
            if let Some(vm) = supervisor.checkout().vm() {
                by_vm.entry(vm.to_string()).or_default().push(supervisor);
            }
        }
        for (domain, remote) in by_vm {
            let Some(vm) = self
                .substrate_of_vm(&domain)
                .and_then(|s| s.vm_details().cloned())
            else {
                continue;
            };
            let registry = self.clone();
            let connected = tokio::task::spawn_blocking(move || registry.keeper_for(&vm)).await;
            match connected {
                Ok(Ok(keeper)) => {
                    for supervisor in &remote {
                        supervisor.set_keeper(keeper.clone());
                    }
                }
                Ok(Err(e)) => {
                    let note = format!(
                        "the files service for VM {domain} could not be connected ({e:#}); \
                         environments whose checkouts are in it cannot be read"
                    );
                    taste_core::app_log::push("warn", "environments", &note);
                    self.events.publish(Event::Toast(note));
                }
                Err(e) => tracing::warn!("connecting the keeper did not finish: {e}"),
            }
        }
        // The primary too. Its checkout moves into the VM — seeded from the
        // user's folder, uncommitted work included — or, when it is already
        // there, the folder is brought up to date with it.
        if substrate.vm_details().is_some() {
            let registry = self.clone();
            match tokio::task::spawn_blocking(move || registry.place_primary_now()).await {
                Ok(Ok(Some(sync))) => {
                    if let Some(note) = sync.note {
                        let note = format!("the folder you opened: {note}");
                        taste_core::app_log::push("info", "environments", &note);
                        self.events.publish(Event::Toast(note));
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(e)) => {
                    let note = format!(
                        "the primary environment's checkout could not be placed in the \
                         workspace's VM ({e:#}); it stays on this machine"
                    );
                    taste_core::app_log::push("warn", "environments", &note);
                    self.events.publish(Event::Toast(note));
                }
                Err(e) => tracing::warn!("placing the primary did not finish: {e}"),
            }
        }
        // Environments made before the flip have their checkouts on this
        // host, where nothing runs any more. Each is moved into a VM of the
        // pool with its uncommitted work — the clone becomes its peer, as
        // a new environment's would — before its first check, so the check
        // finds it where it can run rather than refusing it.
        if substrate.vm_details().is_some() {
            for id in report.restored.clone() {
                let Some(supervisor) = self.get(&id) else {
                    continue;
                };
                if !supervisor.checkout().is_local() {
                    continue;
                }
                let registry = self.clone();
                let moved = {
                    let id = id.clone();
                    tokio::task::spawn_blocking(move || registry.migrate_environment(&id)).await
                };
                match moved {
                    Ok(Ok(vm)) => {
                        let note = format!(
                            "environment {id}'s checkout moved from this host into VM {vm}, \
                             with its uncommitted work; the clone here is its peer now"
                        );
                        taste_core::app_log::push("info", "environments", &note);
                        self.events.publish(Event::Toast(note));
                    }
                    Ok(Err(e)) => {
                        let note = format!(
                            "environment {id}'s checkout is on this host, where nothing runs, \
                             and moving it into the workspace's VM failed ({e:#}); it cannot \
                             run until it is moved"
                        );
                        taste_core::app_log::push("warn", "environments", &note);
                        self.events.publish(Event::Toast(note));
                    }
                    Err(e) => tracing::warn!("moving {id} into the VM did not finish: {e}"),
                }
            }
        }

        // The restore path: an environment whose VM is gone — undefined by
        // hand, lost with a disk, or left on another machine — is placed
        // anew from its peer and its last snapshot (docs/ENVIRONMENTS.md →
        // "Uncommitted work, backups, and artifacts"). Moving is restore.
        if substrate.vm_details().is_some() {
            let orphaned: Vec<Arc<Supervisor>> = self
                .list()
                .into_iter()
                .filter(|s| !s.id().is_primary())
                .filter(|s| {
                    s.checkout()
                        .vm()
                        .is_some_and(|vm| self.substrate_of_vm(vm).is_none())
                })
                .collect();
            for supervisor in orphaned {
                let id = supervisor.id().clone();
                let registry = self.clone();
                let restored =
                    tokio::task::spawn_blocking(move || registry.replace_environment(&id)).await;
                match restored {
                    Ok(Ok(vm)) => {
                        let note = format!(
                            "environment {} was placed anew in VM {vm} from its peer and its \
                             last snapshot; the VM it was in is gone",
                            supervisor.id()
                        );
                        taste_core::app_log::push("info", "environments", &note);
                        self.events.publish(Event::Toast(note));
                    }
                    Ok(Err(e)) => {
                        let note = format!(
                            "environment {}'s checkout is in a VM this workspace no longer \
                             has, and placing it anew failed ({e:#}); it cannot run until it is",
                            supervisor.id()
                        );
                        taste_core::app_log::push("warn", "environments", &note);
                        self.events.publish(Event::Toast(note));
                    }
                    Err(e) => tracing::warn!("placing an environment anew did not finish: {e}"),
                }
            }
        }

        // A restored environment is supervised for real from here: it
        // re-adopts its own running container (by label) and starts
        // watching its own config. Leaving it in NoConfig would make a
        // perfectly healthy environment report safe mode to whatever binds
        // to it next.
        for id in &report.restored {
            let Some(supervisor) = self.get(id) else {
                continue;
            };
            // Before anything mounts it. A clone made by an older build
            // hardlinked its object store to the main checkout's, and a
            // `:Z` bind mount relabels an inode for everyone holding it —
            // so starting one environment took git away from all the
            // others and rewrote labels inside the user's home on the way
            // (`taste_git::clone_local` says the whole of it). New clones
            // no longer share; the ones already on disk are repaired here,
            // once, at the only moment the IDE is certainly the only thing
            // touching them.
            //
            // Idempotent and cheap after the first pass — one `stat` per
            // file in `.git` — so it needs no marker on disk saying it has
            // run. Blocking: it copies the object store the first time.
            let peer = supervisor.peer().to_path_buf();
            let unshared =
                tokio::task::spawn_blocking(move || taste_git::unshare_inodes(&peer)).await;
            match unshared {
                Ok(Ok(0)) => {}
                Ok(Ok(broken)) => {
                    let note = format!(
                        "environment {id}: gave {broken} git file{} of its own back to it \
                         (they were shared with another checkout, which is what took \
                         git away from this environment)",
                        if broken == 1 { "" } else { "s" }
                    );
                    tracing::info!("{note}");
                    taste_core::app_log::push("info", "environments", &note);
                }
                // Worth saying and not worth stopping for: the environment
                // still works for everything that is not git, and the next
                // startup tries again.
                Ok(Err(e)) => tracing::warn!("environment {id}: unsharing git objects: {e:#}"),
                Err(e) => tracing::warn!("environment {id}: the unshare task did not finish: {e}"),
            }
            if let Err(e) = supervisor.recheck() {
                tracing::warn!("environment {id} recheck failed: {e:#}");
            }
            if let Err(e) = self.watch_config(&supervisor) {
                tracing::warn!("environment {id} watcher failed: {e:#}");
            }
            // An ADOPTED container has never been asked whether it can host
            // an agent — `recheck` is synchronous and adoption happens
            // inside it, so the question waits for here. Without this, every
            // chat in an environment the IDE did not itself start would keep
            // the outside-confined topology until something restarted it.
            supervisor.probe_agent_hosting().await;
        }

        // The primary is not in `restored` (it is not a clone). Its first
        // check waits for here rather than running with the window's first
        // frame, because its checkout has just been placed: a container
        // started before that would bind the folder on this host, and the
        // one started now binds the checkout in the VM. The window's banner
        // says NoConfig until then, which is what is true — the primary has
        // nowhere to run before the VM is up.
        let primary = self.primary();
        if let Err(e) = primary.recheck() {
            tracing::warn!("the primary environment's recheck failed: {e:#}");
        }
        if let Err(e) = self.watch_config(&primary) {
            tracing::warn!("the primary environment's watcher failed: {e:#}");
        }
        primary.probe_agent_hosting().await;

        // Anything that adopted a container now confirms it is really
        // there, on the substrate that was just resolved. Usually a no-op
        // — adoption reads `podman ps`, so it cannot adopt something that
        // is gone — and it is here for the ordering rather than for today:
        // the substrate is resolved a few lines above this, and an
        // environment must never end up believing in a container that
        // belongs to a podman it is no longer talking to.
        self.reconcile_containers().await;

        // The substrate says once, at the top, when it is not the host —
        // and says loudly when it is the host and should not have been.
        // A VM the user believes they have and do not is the one substrate
        // failure that must never be silent.
        if let Some(note) = substrate.note() {
            taste_core::app_log::push("warn", "substrate", note);
            self.events.publish(Event::Toast(note.to_string()));
        } else if let Some(line) = substrate.log() {
            // Something to record, nothing to interrupt anyone for: the
            // ladder ended where it was always going to end. See
            // `substrate::Descent`.
            taste_core::app_log::push("info", "substrate", line);
        } else if substrate.is_resolved() {
            taste_core::app_log::push(
                "info",
                "substrate",
                &format!(
                    "this workspace's containers run on {}",
                    substrate.provider().describe()
                ),
            );
        }

        // Every environment that will ever be restored is supervised by
        // now, so this is the moment the disk budget can start counting a
        // complete fleet. Here rather than in the constructor because this
        // is the registry's first code to run on the runtime, and a task
        // spawned off the main thread has nowhere to go.
        self.start_disk_meter();

        if !report.swept.is_empty() {
            let message = report.swept.summary();
            taste_core::app_log::push("warn", "environments", &message);
            self.events.publish(Event::Toast(message));
        }
        report
    }

    /// Environments whose clone directory survived a restart. The directory
    /// is the inventory of record — a state file that disagreed with the
    /// disk would be a second source of truth about what exists.
    fn restore_from_disk(self: &Arc<Self>) -> Vec<EnvironmentId> {
        let base = self
            .environments_base
            .join(environment::workspace_key(&self.workspace_root));
        let Ok(entries) = std::fs::read_dir(&base) else {
            return Vec::new();
        };
        let mut restored = Vec::new();
        for entry in entries.flatten() {
            if !entry.path().join("repo").is_dir() {
                continue;
            }
            let Ok(id) = EnvironmentId::parse(entry.file_name().to_string_lossy()) else {
                continue;
            };
            if id.is_primary() || self.get(&id).is_some() {
                continue;
            }
            self.adopt(id.clone());
            restored.push(id);
        }
        restored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The placement file is what tells a restored environment its
    /// checkout is in a VM; without it, the clone is the checkout.
    #[test]
    fn a_placed_environment_is_restored_as_remote_and_an_unplaced_one_as_local() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let id = env("i-0001");
        let dir = registry.env_dir(&id);
        std::fs::create_dir_all(&dir).unwrap();
        let local = registry.identity_on_disk(&id);
        assert!(local.checkout.is_local());
        assert_eq!(local.peer, registry.env_repo(&id));

        Placement {
            vm: "taste-799f-k7m2qx".into(),
            path: PathBuf::from("/var/home/core/taste/799f/i-0001"),
        }
        .write(&dir)
        .unwrap();
        assert_eq!(
            Placement::read(&dir),
            Some(Placement {
                vm: "taste-799f-k7m2qx".into(),
                path: PathBuf::from("/var/home/core/taste/799f/i-0001"),
            })
        );
        let remote = registry.identity_on_disk(&id);
        assert_eq!(remote.checkout.vm(), Some("taste-799f-k7m2qx"));
        assert_eq!(
            remote.checkout.path(),
            Path::new("/var/home/core/taste/799f/i-0001")
        );
        assert_eq!(remote.peer, registry.env_repo(&id), "the clone is the peer");
        // Adopted, it refuses file reads by name until its keeper is
        // connected — never reads the VM path off this host.
        let supervisor = registry.adopt_identity(remote);
        let err = supervisor
            .files()
            .read(Path::new("/var/home/core/taste/799f/i-0001/x"))
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotConnected);
        assert!(err.to_string().contains("taste-799f-k7m2qx"), "{err}");
    }

    /// A VM resolved for the workspace never becomes a local checkout's
    /// substrate: the primary's container would be started in the VM with
    /// a host bind that does not exist there. Caught after the batch that
    /// auto-provisioned every workspace had already pointed every
    /// supervisor at the VM.
    #[test]
    fn a_workspaces_vm_does_not_reach_a_local_checkout() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let vm = crate::provision::Vm {
            domain: "taste-799f-k7m2qx".into(),
            ssh_port: 40022,
            workspace_root: fixture.workspace.path().to_path_buf(),
            state: crate::provision::DomainState::Running,
        };
        let facts = crate::provision::VmFacts {
            running: true,
            cpus: 2,
            memory_mib: 4096,
            disk_ceiling_gib: 64,
            host_storage_bytes: None,
        };
        registry.set_substrate(Arc::new(Substrate::vm(&vm, facts, false)));
        // The workspace knows its VM...
        assert_eq!(registry.substrate().connection(), Some("taste-799f-k7m2qx"));
        // ...and the primary, a checkout still on this host, has nowhere
        // to run until it is placed there: no host rung. Its refusal names
        // the folder and the VM, and nothing it composes reaches the host.
        let primary = registry.primary();
        assert!(!primary.substrate().can_host(&primary.checkout()));
        let refusal = primary.substrate().refusal(&primary.checkout());
        assert!(refusal.contains("taste-799f-k7m2qx"), "{refusal}");
        let (_, args) = primary.exec().podman_target().argv(["ps"]);
        assert_eq!(args[0], "-c");
        // A new environment of a workspace with a VM is placed IN a VM of
        // the pool — which this test has none of (no runtime, no libvirt),
        // so creation fails at placement, saying so, and leaves no
        // half-made environment behind for a retry to trip on.
        let refused = match registry.create(env("review")) {
            Ok(_) => panic!("a workspace with a VM must not make a local checkout"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            refused.contains("placing environment review in a VM"),
            "{refused}"
        );
        assert!(registry.get(&env("review")).is_none());
        assert!(!registry.env_dir(&env("review")).exists());
    }

    /// What a destroy left behind is named, because nothing can name it
    /// later.
    #[test]
    fn leftovers_are_named_in_the_words_both_surfaces_use() {
        let mut report = DestroyReport::default();
        assert_eq!(report.leftovers_clause(), "", "silence when there are none");

        report.kept_volumes.push("taste-env-ws-i-0001-home".into());
        assert_eq!(
            report.leftovers_clause(),
            " · 1 volume could not be removed (taste-env-ws-i-0001-home)"
        );

        report.kept_volumes.push("taste-env-ws-i-0001-cargo".into());
        assert_eq!(
            report.leftovers_clause(),
            " · 2 volumes could not be removed \
             (taste-env-ws-i-0001-home, taste-env-ws-i-0001-cargo)"
        );

        report.kept_clone = Some(PathBuf::from("/state/ws/i-0001"));
        assert_eq!(
            report.leftovers_clause(),
            " · 2 volumes could not be removed \
             (taste-env-ws-i-0001-home, taste-env-ws-i-0001-cargo) \
             · the clone is still at /state/ws/i-0001"
        );
    }

    struct Fixture {
        workspace: tempfile::TempDir,
        state: tempfile::TempDir,
    }

    impl Fixture {
        /// A workspace that is a real git repository with one commit — the
        /// main checkout an environment clones from.
        fn new() -> Self {
            let workspace = tempfile::tempdir().unwrap();
            let repo = git2::Repository::init(workspace.path()).unwrap();
            commit(&repo, "base");
            Self {
                workspace,
                state: tempfile::tempdir().unwrap(),
            }
        }

        fn registry(&self) -> Arc<EnvironmentRegistry> {
            EnvironmentRegistry::new_for_tests(
                self.workspace.path(),
                EventBus::new(),
                ExecContext::host_unsandboxed_for_tests(),
                self.state.path(),
            )
        }
    }

    fn commit(repo: &git2::Repository, name: &str) -> git2::Oid {
        let root = repo.workdir().unwrap().to_path_buf();
        std::fs::write(root.join(name), name).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(name)).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let parents: Vec<git2::Commit> = repo
            .head()
            .ok()
            .and_then(|h| h.peel_to_commit().ok())
            .into_iter()
            .collect();
        let refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, name, &tree, &refs)
            .unwrap()
    }

    fn env(slug: &str) -> EnvironmentId {
        EnvironmentId::parse(slug).unwrap()
    }

    #[test]
    fn the_primary_exists_from_the_start_and_is_the_main_checkout() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let primary = registry.primary();
        assert!(primary.id().is_primary());
        assert_eq!(primary.checkout().path(), fixture.workspace.path());
        assert_eq!(primary.peer(), fixture.workspace.path());
        assert_eq!(registry.ids(), vec![EnvironmentId::primary()]);
    }

    #[tokio::test]
    async fn the_primary_is_never_created_or_destroyed() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        assert!(registry.create(EnvironmentId::primary()).is_err());
        assert!(registry.destroy(&EnvironmentId::primary()).await.is_err());
    }

    #[test]
    fn create_clones_the_main_checkout_and_supervises_the_clone() {
        let fixture = Fixture::new();
        let registry = fixture.registry();

        let review = registry.create(env("review")).unwrap();
        assert_eq!(review.checkout().path(), registry.env_repo(&env("review")));
        assert_eq!(review.peer(), review.checkout().path());
        assert!(
            review.checkout().path().join("base").is_file(),
            "checked out"
        );
        assert!(review.checkout().path().join(".git").exists());
        assert_eq!(review.workspace_root(), fixture.workspace.path());

        // It is in the fleet, after the primary, with its own container.
        assert_eq!(
            registry.ids(),
            vec![EnvironmentId::primary(), env("review")]
        );
        assert_ne!(review.container_name(), registry.primary().container_name());

        // Creating it twice is an error, not a silent re-clone.
        assert!(registry.create(env("review")).is_err());
    }

    /// A new environment has no container yet, so it is in safe mode — even
    /// when the IDE (and this suite) is itself running inside one. The
    /// self-hosting shortcut belongs to the primary environment alone; a
    /// clone that claimed it would send agent commands into the IDE's own
    /// container, against a checkout that is not mounted there.
    #[test]
    fn a_new_environment_starts_in_safe_mode_even_when_self_hosting() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let review = registry.create(env("review")).unwrap();
        assert!(
            !review.exec().is_container(),
            "a clone with no container of its own is in safe mode"
        );
        assert!(!review.exec().is_inside_container());
    }

    /// Sockets follow the registry, so the registry has to say when an
    /// environment appears or goes — for a fresh clone and a restored one
    /// alike. An environment nobody announced is an environment no agent
    /// can reach.
    #[tokio::test]
    async fn appearing_and_disappearing_are_announced() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let events = registry.events.subscribe();
        registry.create(env("review")).unwrap();
        assert!(matches!(
            events.recv().await.unwrap(),
            Event::EnvironmentCreated { env: ref id } if *id == env("review")
        ));

        registry.destroy(&env("review")).await.unwrap();
        let removal = loop {
            match events.recv().await.unwrap() {
                Event::EnvironmentRemoved { env: id } => break id,
                _ => continue, // stop/volume churn on the way down
            }
        };
        assert_eq!(removal, env("review"));
        // Once, and once only. That event IS the app's forget fan-out — the
        // window's arm for it drops the chat, the editor's stowed tabs and
        // the console's caches — and it is where the panel's Destroy button
        // and the coordinator's `environment_destroy` both arrive, neither
        // of them forgetting anything itself. A second publish here would
        // be a second fan-out (i-0022).
        let mut again = 0;
        while let Ok(event) = events.try_recv() {
            if matches!(&event, Event::EnvironmentRemoved { env: id } if *id == env("review")) {
                again += 1;
            }
        }
        assert_eq!(again, 0, "one EnvironmentRemoved per destroy, no more");

        // A restart that finds a clone on disk announces it the same way:
        // a restored environment needs its socket as much as a new one.
        registry.create(env("later")).unwrap();
        let restarted = fixture.registry();
        let restored_events = restarted.events.subscribe();
        let seen = restarted.reconcile().await;
        assert_eq!(seen.restored, vec![env("later")]);
        assert!(matches!(
            restored_events.recv().await.unwrap(),
            Event::EnvironmentCreated { env: ref id } if *id == env("later")
        ));
    }

    /// The budget is what the *agents* spend, so the sum runs over their
    /// environments and not over the user's own checkout — folding the
    /// primary's `target/` into it would refuse every start forever, for a
    /// reason no agent could act on. And before anything has been walked the
    /// sum says so: zero used, two unmeasured, and nothing refused.
    #[tokio::test]
    async fn the_disk_budget_sums_the_agents_environments_and_not_the_users() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        registry.create(env("one")).unwrap();
        registry.create(env("two")).unwrap();

        let before = registry.disk_budget();
        assert_eq!(
            before.budget_bytes,
            environment::MAX_ORCHESTRATED_DISK_BYTES
        );
        assert_eq!((before.measured, before.unmeasured), (0, 2));
        assert_eq!(before.used_bytes, 0);
        assert!(
            !before.spent(),
            "a workspace nobody has measured refuses nothing: {before:?}"
        );

        registry.measure_disk().await;
        let after = registry.disk_budget();
        assert_eq!(
            (after.measured, after.unmeasured),
            (2, 0),
            "the primary is in neither count: {after:?}"
        );
        assert!(after.used_bytes > 0, "two clones are not free: {after:?}");
        assert!(!after.spent());
        assert_eq!(
            after.remaining_bytes(),
            after.budget_bytes - after.used_bytes
        );
        assert!(
            registry.primary().measured_disk().is_none(),
            "the user's own checkout is never walked for this"
        );
    }

    /// The ceiling, in the unit David gave it. A partial sum that has
    /// already crossed it has crossed it — an environment nobody has walked
    /// yet can only add — so the refusal stands while the measurements are
    /// still coming in.
    #[test]
    fn the_disk_budget_is_spent_when_the_measured_clones_reach_the_ceiling() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let one = registry.create(env("one")).unwrap();
        registry.create(env("two")).unwrap();

        one.set_disk_for_tests(environment::MAX_ORCHESTRATED_DISK_BYTES);
        let budget = registry.disk_budget();
        assert!(budget.spent(), "{budget:?}");
        assert_eq!(budget.remaining_bytes(), 0);
        assert_eq!(
            (budget.measured, budget.unmeasured),
            (1, 1),
            "and the unwalked one can only add to it: {budget:?}"
        );
    }

    /// The third ceiling, and the only one that reads the machine rather
    /// than this workspace: the volume asked about is the environments'
    /// own, because that is where a clone lands, and the user's checkout
    /// may be on another disk entirely.
    #[test]
    fn the_floor_is_read_from_the_volume_the_clones_are_written_to() {
        let fixture = Fixture::new();
        let free = fixture.registry().free_disk();
        assert_eq!(free.floor_bytes, environment::MIN_FREE_DISK_BYTES);
        assert!(
            free.volume.starts_with(fixture.state.path()),
            "the environments' own volume, not the workspace's: {free:?}"
        );
        assert!(
            free.free_bytes.is_some(),
            "and the kernel answers for it before any clone exists: {free:?}"
        );
    }

    /// What the gates ask of it. A byte under the floor is under it; the
    /// floor exactly met is not; and an unanswerable `statvfs` refuses
    /// nothing, which is the same posture the budget takes towards an
    /// unmeasured workspace — a ceiling enforced on a number nobody has
    /// would refuse for a reason nobody could check.
    #[test]
    fn the_floor_binds_by_a_byte_and_never_binds_on_a_number_nobody_has() {
        let fixture = Fixture::new();
        let registry = fixture.registry();

        registry.set_free_disk_for_tests(Some(environment::MIN_FREE_DISK_BYTES - 1));
        let free = registry.free_disk();
        assert!(free.below_floor(), "{free:?}");
        assert_eq!(free.shortfall_bytes(), 1);

        registry.set_free_disk_for_tests(Some(environment::MIN_FREE_DISK_BYTES));
        let free = registry.free_disk();
        assert!(
            !free.below_floor(),
            "the floor is met, not breached: {free:?}"
        );
        assert_eq!(free.shortfall_bytes(), 0);

        registry.set_free_disk_for_tests(None);
        let free = registry.free_disk();
        assert!(!free.below_floor(), "unknown is not refused: {free:?}");
        assert_eq!(free.shortfall_bytes(), 0);
    }

    /// The clone directory is the inventory of record: an IDE restart picks
    /// the environments back up without consulting any state file.
    #[tokio::test]
    async fn reconcile_restores_environments_from_their_clones() {
        let fixture = Fixture::new();
        fixture.registry().create(env("review")).unwrap();

        let restarted = fixture.registry();
        assert_eq!(restarted.ids(), vec![EnvironmentId::primary()]);
        let report = restarted.reconcile().await;
        assert_eq!(report.restored, vec![env("review")]);
        assert!(restarted.get(&env("review")).is_some());
    }

    /// The destroy contract: enumerate first, warn with what was found,
    /// and only then remove.
    #[tokio::test]
    async fn destroy_reports_work_the_main_checkout_has_never_seen() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let review = registry.create(env("review")).unwrap();

        let clone = git2::Repository::open(review.peer()).unwrap();
        commit(&clone, "unreviewed-work");
        std::fs::write(review.checkout().path().join("scratch.txt"), "wip").unwrap();

        let report = registry.destroy(&env("review")).await.unwrap();
        assert!(report.had_unsaved_work());
        assert_eq!(report.unpublished.len(), 1, "{report:?}");
        assert_eq!(report.unpublished[0].summary, "unreviewed-work");
        assert!(report.dirty_files >= 1);

        assert!(report.removed_clone.is_some());
        assert!(!registry.env_dir(&env("review")).exists());
        assert!(registry.get(&env("review")).is_none());
    }

    /// A clone that will not delete does not keep the environment alive.
    ///
    /// `remove_dir_all` is not atomic, and it used to be the one `?`
    /// between the world changing and anyone hearing about it: a failure
    /// there left the supervisor in the map and `EnvironmentRemoved`
    /// unpublished, so the panes stayed aimed at a directory that was
    /// already mostly gone.
    #[tokio::test]
    async fn a_clone_that_will_not_delete_still_forgets_the_environment() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        registry.create(env("stuck")).unwrap();

        // A directory whose contents cannot be unlinked: removing a file
        // needs write permission on the directory holding it, and this one
        // has none. Root ignores that, so the assertions below stand down
        // rather than lie when the tests run as root.
        let locked = registry.env_dir(&env("stuck")).join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("held"), "x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();

        let report = registry.destroy(&env("stuck")).await;
        // Put it back first, so the tempdir can clean up whatever happened.
        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700));

        let report = report.expect("a clone that will not delete is not a failed destroy");
        let Some(kept) = report.kept_clone.as_ref() else {
            return; // running as root; the removal succeeded after all
        };
        assert_eq!(kept, &registry.env_dir(&env("stuck")));
        assert!(report.removed_clone.is_none());
        assert!(
            report.leftovers_clause().contains("the clone is still at"),
            "{}",
            report.leftovers_clause()
        );
        // The invariant this test exists for: the registry has let go, so
        // the fan-out behind `EnvironmentRemoved` runs and the panes come
        // home.
        assert!(
            registry.get(&env("stuck")).is_none(),
            "a leftover directory must not keep the environment in the registry"
        );
    }

    #[tokio::test]
    async fn destroying_a_clean_environment_reports_nothing_lost() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        registry.create(env("scratch")).unwrap();

        let report = registry.destroy(&env("scratch")).await.unwrap();
        assert!(!report.had_unsaved_work(), "{report:?}");
        assert!(report.unpublished.is_empty());
        assert_eq!(report.dirty_files, 0);
        assert!(!registry.env_dir(&env("scratch")).exists());
        assert_eq!(registry.ids(), vec![EnvironmentId::primary()]);
    }

    #[tokio::test]
    async fn destroying_an_unknown_environment_is_an_error() {
        let fixture = Fixture::new();
        assert!(fixture.registry().destroy(&env("ghost")).await.is_err());
    }
}

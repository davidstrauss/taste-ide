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
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use taste_core::environment::{self, EnvironmentId};
use taste_core::{Event, EventBus, ExecContext};

use crate::reconcile::{self, SweepReport};
use crate::substrate::Substrate;
use crate::supervisor::{EnvironmentIdentity, Supervisor};

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
    pub removed_clone: Option<PathBuf>,
}

impl DestroyReport {
    /// Whether anything was lost that nobody else has a copy of.
    pub fn had_unsaved_work(&self) -> bool {
        !self.unpublished.is_empty() || self.dirty_files > 0
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
    /// What the IDE serves down every environment channel, once the window
    /// has said. Held here as well as on each supervisor so an environment
    /// created later inherits it.
    channel_services: Mutex<Option<Arc<dyn crate::channel::ChannelServices>>>,
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
            Arc::new(Substrate::local()),
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
            Substrate::local_for_tests(),
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
        });
        let primary =
            registry.make_supervisor(EnvironmentIdentity::primary(workspace_root), primary_exec);
        registry
            .environments
            .lock()
            .unwrap()
            .insert(EnvironmentId::primary(), primary);
        registry
    }

    fn make_supervisor(&self, identity: EnvironmentIdentity, exec: ExecContext) -> Arc<Supervisor> {
        // One substrate, every environment — and the exec context aimed at
        // it before the supervisor can resolve a single command against it.
        exec.set_podman_target(self.substrate().target().clone());
        let supervisor = if self.outside_container_for_tests {
            Supervisor::new_outside_container_for_tests(
                identity,
                self.events.clone(),
                exec,
                self.substrate(),
            )
        } else {
            Supervisor::new(identity, self.events.clone(), exec, self.substrate())
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

    /// Where this workspace's containers live.
    pub fn substrate(&self) -> Arc<Substrate> {
        self.substrate.lock().unwrap().clone()
    }

    /// Point the whole workspace at a substrate.
    ///
    /// Every supervisor and every [`ExecContext`] together, in one place,
    /// because a workspace half on a VM and half on the host is not a state
    /// this design has a meaning for: one machine hosts every environment.
    pub fn set_substrate(&self, substrate: Arc<Substrate>) {
        *self.substrate.lock().unwrap() = substrate.clone();
        for supervisor in self.environments.lock().unwrap().values() {
            supervisor.set_substrate(substrate.clone());
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
        let identity = EnvironmentIdentity {
            id: id.clone(),
            workspace_root: self.workspace_root.clone(),
            root: self.env_repo(&id),
        };
        // A fresh context per environment: each supervisor points its own
        // at its own container. There is no shared target to race over, and
        // a clone never inherits the self-hosting "the IDE's container is
        // the environment" flag — that is true of the primary alone.
        let supervisor = self.make_supervisor(identity, ExecContext::for_cloned_environment());
        self.environments
            .lock()
            .unwrap()
            .insert(id.clone(), supervisor.clone());
        self.events.publish(Event::EnvironmentCreated { env: id });
        supervisor
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
        Ok(self.adopt(id))
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
        let repo = supervisor.root().to_path_buf();

        let mut report = DestroyReport::default();
        if repo.is_dir() {
            report.unpublished =
                taste_git::unpublished_work(&repo, &self.workspace_root).unwrap_or_default();
            report.dirty_files = taste_git::GitWorkspace::discover(&repo)
                .and_then(|git| git.status().ok())
                .map(|status| status.len())
                .unwrap_or(0);
        }

        // Container first: a running container holds the clone's mount.
        let _ = supervisor.stop().await;
        for volume in supervisor.env_volumes() {
            if supervisor.remove_volume(&volume).await.is_ok() {
                report.removed_volumes.push(volume);
            }
        }

        let env_dir = self.env_dir(id);
        if env_dir.is_dir() {
            std::fs::remove_dir_all(&env_dir)
                .with_context(|| format!("removing {}", env_dir.display()))?;
            report.removed_clone = Some(env_dir);
        }
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
        // place a VM is allowed to cost twenty seconds.
        self.set_substrate(Substrate::resolve().await);

        let substrate = self.substrate();
        let mut report = ReconcileReport {
            restored: self.restore_from_disk(),
            swept: reconcile::sweep_legacy_resources(&self.workspace_root, &substrate).await,
        };
        report.restored.sort();

        // A restored environment is supervised for real from here: it
        // re-adopts its own running container (by label) and starts
        // watching its own config. Leaving it in NoConfig would make a
        // perfectly healthy environment report safe mode to whatever binds
        // to it next.
        for id in &report.restored {
            let Some(supervisor) = self.get(id) else {
                continue;
            };
            if let Err(e) = supervisor.recheck() {
                tracing::warn!("environment {id} recheck failed: {e:#}");
            }
            if let Err(e) = supervisor.start_watching() {
                tracing::warn!("environment {id} watcher failed: {e:#}");
            }
            // An ADOPTED container has never been asked whether it can host
            // an agent — `recheck` is synchronous and adoption happens
            // inside it, so the question waits for here. Without this, every
            // chat in an environment the IDE did not itself start would keep
            // the outside-confined topology until something restarted it.
            supervisor.probe_agent_hosting().await;
        }

        // The primary is not in `restored` (it is not a clone) and its
        // container is adopted the same way.
        self.primary().probe_agent_hosting().await;

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
        } else if !substrate.is_local() {
            taste_core::app_log::push(
                "info",
                "substrate",
                &format!(
                    "this workspace's containers run on {}",
                    substrate.provider().describe()
                ),
            );
        }

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
        assert_eq!(primary.root(), fixture.workspace.path());
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
        assert_eq!(review.root(), registry.env_repo(&env("review")));
        assert!(review.root().join("base").is_file(), "checked out");
        assert!(review.root().join(".git").exists());
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

        let clone = git2::Repository::open(review.root()).unwrap();
        commit(&clone, "unreviewed-work");
        std::fs::write(review.root().join("scratch.txt"), "wip").unwrap();

        let report = registry.destroy(&env("review")).await.unwrap();
        assert!(report.had_unsaved_work());
        assert_eq!(report.unpublished.len(), 1, "{report:?}");
        assert_eq!(report.unpublished[0].summary, "unreviewed-work");
        assert!(report.dirty_files >= 1);

        assert!(report.removed_clone.is_some());
        assert!(!registry.env_dir(&env("review")).exists());
        assert!(registry.get(&env("review")).is_none());
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

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
    sandboxed: bool,
    environments: Mutex<BTreeMap<EnvironmentId, Arc<Supervisor>>>,
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
            true,
        )
    }

    fn build(
        workspace_root: impl Into<PathBuf>,
        events: EventBus,
        primary_exec: ExecContext,
        environments_base: PathBuf,
        outside_container_for_tests: bool,
    ) -> Arc<Self> {
        let workspace_root = workspace_root.into();
        let registry = Arc::new(Self {
            workspace_root: workspace_root.clone(),
            events: events.clone(),
            environments_base,
            outside_container_for_tests,
            sandboxed: Path::new("/.flatpak-info").exists(),
            environments: Mutex::new(BTreeMap::new()),
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
        if self.outside_container_for_tests {
            Supervisor::new_outside_container_for_tests(identity, self.events.clone(), exec)
        } else {
            Supervisor::new(identity, self.events.clone(), exec)
        }
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
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
    fn adopt(&self, id: EnvironmentId) -> Arc<Supervisor> {
        let identity = EnvironmentIdentity {
            id: id.clone(),
            workspace_root: self.workspace_root.clone(),
            root: self.env_repo(&id),
        };
        // A fresh context per environment: each supervisor points its own
        // at its own container. There is no shared target to race over.
        let supervisor = self.make_supervisor(identity, ExecContext::host());
        self.environments
            .lock()
            .unwrap()
            .insert(id, supervisor.clone());
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
        Ok(report)
    }

    /// Match the registry to what is actually on disk and in podman.
    ///
    /// Two jobs, both at startup: pick the workspace's existing environment
    /// clones back up, and remove what the single-environment naming scheme
    /// left behind. The sweep reports itself once through the event bus and
    /// the app log — a reset the user is not told about looks like a bug.
    pub async fn reconcile(self: &Arc<Self>) -> ReconcileReport {
        let mut report = ReconcileReport {
            restored: self.restore_from_disk(),
            swept: reconcile::sweep_legacy_resources(&self.workspace_root, self.sandboxed).await,
        };
        report.restored.sort();

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

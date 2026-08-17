//! Where commands run: the host, or inside the supervised devcontainer.
//!
//! This is the single indirection that makes "reload without interruption"
//! work. Terminals, build tasks, and agent-brokered commands hold an
//! [`ExecContext`]; when the devcontainer is rebuilt, the supervisor swaps
//! the container id here and *new* executions land in the new container,
//! while nothing holding the context is torn down.

use std::sync::{Arc, RwLock};

/// A concrete command to spawn, already resolved against the current
/// execution context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Host,
    Container { id: String, workdir: String },
}

/// Shared, swappable execution target.
#[derive(Clone)]
pub struct ExecContext {
    target: Arc<RwLock<Target>>,
    /// True when running inside a Flatpak sandbox: host commands must be
    /// wrapped in `flatpak-spawn --host`.
    sandboxed: bool,
    /// True when the IDE process itself is inside a container (the
    /// self-hosting bootstrap). "Host" execution then already happens in
    /// the container, and the IDE is in container mode by construction.
    inside_container: bool,
}

impl ExecContext {
    pub fn host() -> Self {
        Self {
            target: Arc::new(RwLock::new(Target::Host)),
            sandboxed: std::path::Path::new("/.flatpak-info").exists(),
            inside_container: std::path::Path::new("/run/.containerenv").exists()
                || std::path::Path::new("/.dockerenv").exists(),
        }
    }

    #[doc(hidden)]
    pub fn host_unsandboxed_for_tests() -> Self {
        Self {
            target: Arc::new(RwLock::new(Target::Host)),
            sandboxed: false,
            inside_container: false,
        }
    }

    /// Point subsequent executions into a container. Existing spawned
    /// processes are unaffected — that is the point.
    pub fn set_container(&self, id: impl Into<String>, workdir: impl Into<String>) {
        *self.target.write().unwrap() = Target::Container {
            id: id.into(),
            workdir: workdir.into(),
        };
    }

    /// Point subsequent executions back at the host.
    pub fn set_host(&self) {
        *self.target.write().unwrap() = Target::Host;
    }

    /// Whether work happens in a container — either one the supervisor
    /// started, or the container the IDE itself runs in (self-hosting).
    /// Safe mode is the negation of this.
    pub fn is_container(&self) -> bool {
        self.inside_container || matches!(*self.target.read().unwrap(), Target::Container { .. })
    }

    /// The self-hosting case specifically.
    pub fn is_inside_container(&self) -> bool {
        self.inside_container
    }

    pub fn container_id(&self) -> Option<String> {
        match &*self.target.read().unwrap() {
            Target::Container { id, .. } => Some(id.clone()),
            Target::Host => None,
        }
    }

    /// Resolve a command against the current target.
    ///
    /// Container targets become `podman exec`; inside Flatpak everything is
    /// additionally wrapped in `flatpak-spawn --host` because podman itself
    /// lives on the host.
    pub fn resolve(&self, program: &str, args: &[&str], interactive: bool) -> CommandSpec {
        self.resolve_as(None, program, args, interactive)
    }

    /// Like [`Self::resolve`], but container targets exec as root inside
    /// the container. Under rootless podman container-root is the user's
    /// own uid seen through the user namespace, so this grants nothing on
    /// the host — it is what `systemctl`/`journalctl` need. Host targets
    /// are unchanged (never sudo).
    pub fn resolve_root(&self, program: &str, args: &[&str], interactive: bool) -> CommandSpec {
        self.resolve_as(Some("root"), program, args, interactive)
    }

    fn resolve_as(
        &self,
        user: Option<&str>,
        program: &str,
        args: &[&str],
        interactive: bool,
    ) -> CommandSpec {
        let inner: Vec<String> = match &*self.target.read().unwrap() {
            Target::Host => std::iter::once(program.to_string())
                .chain(args.iter().map(|s| s.to_string()))
                .collect(),
            Target::Container { id, workdir } => {
                let mut v = vec!["podman".into(), "exec".into()];
                if interactive {
                    v.push("-it".into());
                }
                if let Some(user) = user {
                    v.push("--user".into());
                    v.push(user.to_string());
                }
                v.push("--workdir".into());
                v.push(workdir.clone());
                v.push(id.clone());
                v.push(program.to_string());
                v.extend(args.iter().map(|s| s.to_string()));
                v
            }
        };
        let full: Vec<String> = if self.sandboxed {
            ["flatpak-spawn", "--host"]
                .iter()
                .map(|s| s.to_string())
                .chain(inner)
                .collect()
        } else {
            inner
        };
        CommandSpec {
            program: full[0].clone(),
            args: full[1..].to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_resolution_is_passthrough() {
        let ctx = ExecContext::host_unsandboxed_for_tests();
        let spec = ctx.resolve("cargo", &["build"], false);
        assert_eq!(spec.program, "cargo");
        assert_eq!(spec.args, vec!["build"]);
    }

    #[test]
    fn container_resolution_wraps_in_podman_exec() {
        let ctx = ExecContext::host_unsandboxed_for_tests();
        ctx.set_container("abc123", "/workspace");
        let spec = ctx.resolve("cargo", &["build"], false);
        assert_eq!(spec.program, "podman");
        assert_eq!(
            spec.args,
            vec![
                "exec",
                "--workdir",
                "/workspace",
                "abc123",
                "cargo",
                "build"
            ]
        );
    }

    #[test]
    fn root_resolution_execs_as_container_root_but_not_on_host() {
        let ctx = ExecContext::host_unsandboxed_for_tests();
        let host = ctx.resolve_root("systemctl", &["status"], false);
        assert_eq!(host.program, "systemctl");
        ctx.set_container("abc123", "/workspace");
        let spec = ctx.resolve_root("systemctl", &["status"], false);
        assert_eq!(spec.program, "podman");
        assert_eq!(spec.args[..4], ["exec", "--user", "root", "--workdir"]);
    }

    #[test]
    fn swapping_context_does_not_affect_resolved_specs() {
        let ctx = ExecContext::host_unsandboxed_for_tests();
        ctx.set_container("old", "/workspace");
        let before = ctx.resolve("bash", &[], true);
        ctx.set_container("new", "/workspace");
        // The already-resolved spec still points at the old container: a
        // running terminal keeps its process; only new spawns see "new".
        assert!(before.args.contains(&"old".to_string()));
        assert!(ctx
            .resolve("bash", &[], true)
            .args
            .contains(&"new".to_string()));
    }
}

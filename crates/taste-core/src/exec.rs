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
    /// self-hosting bootstrap). No container runtime is reachable from in
    /// there by design — forwarding one would hand the agent and the
    /// repo's own build the host — so the container the IDE runs in IS
    /// the environment.
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

    /// The execution context for an environment whose checkout is a
    /// **clone** — every environment but the primary.
    ///
    /// [`Self::host`] except that it never inherits the self-hosting flag.
    /// That flag says "the container the IDE runs in IS the environment",
    /// and it is true of exactly one environment: the primary, whose
    /// checkout is what that container has mounted. A clone that inherited
    /// it would report container mode with no container of its own, and
    /// `ide_exec` would land an agent's commands in the IDE's container
    /// against a path that does not exist there — the host-adjacent
    /// fallback the exec rules exist to refuse. A cloned environment is in
    /// safe mode until its own supervisor starts its own container.
    pub fn for_cloned_environment() -> Self {
        Self {
            target: Arc::new(RwLock::new(Target::Host)),
            sandboxed: std::path::Path::new("/.flatpak-info").exists(),
            inside_container: false,
        }
    }

    #[doc(hidden)]
    pub fn host_unsandboxed_for_tests() -> Self {
        Self::for_tests(false)
    }

    #[doc(hidden)]
    pub fn for_tests(inside_container: bool) -> Self {
        Self {
            target: Arc::new(RwLock::new(Target::Host)),
            sandboxed: false,
            inside_container,
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

    /// Whether work happens in a container — one the supervisor started,
    /// or the container the IDE itself runs in (self-hosting). Safe mode is
    /// the negation of this.
    pub fn is_container(&self) -> bool {
        self.inside_container || matches!(*self.target.read().unwrap(), Target::Container { .. })
    }

    /// The raw fact (labels, terminal context) — not the mode.
    pub fn is_inside_container(&self) -> bool {
        self.inside_container
    }

    pub fn container_id(&self) -> Option<String> {
        match &*self.target.read().unwrap() {
            Target::Container { id, .. } => Some(id.clone()),
            Target::Host => None,
        }
    }

    /// Where the workspace lives *inside* the container — the path
    /// container-side tools (language servers) speak in. `None` on the
    /// host, where workspace paths need no translation.
    pub fn container_workdir(&self) -> Option<String> {
        match &*self.target.read().unwrap() {
            Target::Container { workdir, .. } => Some(workdir.clone()),
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

    /// Resolve a command an AGENT asked for.
    ///
    /// Same target as [`Self::resolve`] — agent commands land in the
    /// project's devcontainer, the environment of record — plus the git
    /// policy from [`crate::policy::agent_git_config`]. The user's own
    /// terminals in that same container are deliberately unaffected: the
    /// policy rides on the agent's command, not on the container.
    ///
    /// The environment travels in the command line rather than in a
    /// `CommandSpec` field so that every spawner gets it for free and none
    /// can forget to apply it: `podman exec --env` for a container target,
    /// `env(1)` for the self-hosting case where the IDE's own container is
    /// the environment.
    pub fn resolve_for_agent(&self, program: &str, args: &[&str]) -> CommandSpec {
        self.resolve_for_agent_in(None, &[], program, args)
    }

    /// [`Self::resolve_for_agent`] with the two things a client-served ACP
    /// terminal carries that an `ide_exec` call does not: the working
    /// directory the agent named, and the environment variables it asked
    /// for.
    ///
    /// `cwd` is passed through as-is because relocation made host and
    /// container paths the same string (see `taste_acp::relocate`) — there
    /// is no translation to do, and inventing one would be the bug.
    ///
    /// **The git policy is applied last, after the agent's own variables.**
    /// podman lets the later `--env` win, and `env(1)` likewise, so an
    /// agent cannot un-set its own push block by naming `GIT_CONFIG_COUNT`
    /// in a terminal request. This is hygiene, not a wall: in container
    /// mode the agent has a shell in that container either way, and
    /// CLAUDE.md refuses to defend mediation on security grounds. It costs
    /// one ordering decision to not hand it away for free.
    pub fn resolve_for_agent_in(
        &self,
        cwd: Option<&str>,
        extra_env: &[(String, String)],
        program: &str,
        args: &[&str],
    ) -> CommandSpec {
        let mut env: Vec<(String, String)> = extra_env.to_vec();
        env.extend(crate::policy::agent_git_config_env());
        match &*self.target.read().unwrap() {
            Target::Container { .. } => {
                let mut prefix: Vec<String> = Vec::new();
                for (key, value) in &env {
                    prefix.push("--env".into());
                    prefix.push(format!("{key}={value}"));
                }
                self.resolve_with_exec_flags(&prefix, cwd, program, args)
            }
            Target::Host => {
                // No podman to carry the environment: `env` does it — and
                // its own `--chdir` carries the working directory.
                let mut argv: Vec<String> = Vec::new();
                if let Some(cwd) = cwd {
                    argv.push(format!("--chdir={cwd}"));
                }
                argv.extend(env.iter().map(|(k, v)| format!("{k}={v}")));
                argv.push(program.to_string());
                argv.extend(args.iter().map(|s| s.to_string()));
                let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                self.resolve("env", &refs, false)
            }
        }
    }

    /// [`Self::resolve`] with extra flags for the `podman exec` itself
    /// (not for the command being run), and optionally a working directory
    /// other than the container's own. Host targets have nowhere to put
    /// them, and never call this.
    ///
    /// `workdir_override` is one flag and not two: `--workdir` given twice
    /// is a coin flip on which podman honours, and a command that runs in
    /// the wrong directory fails in ways nobody reads as a flag bug.
    fn resolve_with_exec_flags(
        &self,
        exec_flags: &[String],
        workdir_override: Option<&str>,
        program: &str,
        args: &[&str],
    ) -> CommandSpec {
        let inner: Vec<String> = match &*self.target.read().unwrap() {
            Target::Host => std::iter::once(program.to_string())
                .chain(args.iter().map(|s| s.to_string()))
                .collect(),
            Target::Container { id, workdir } => {
                let mut v = vec!["podman".into(), "exec".into()];
                v.extend(exec_flags.iter().cloned());
                v.push("--workdir".into());
                v.push(workdir_override.unwrap_or(workdir).to_string());
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
    fn inside_a_container_is_container_mode() {
        // Self-hosting: the IDE's own container is the environment. No
        // runtime is reachable from in there, so there is nothing else it
        // could be.
        assert!(ExecContext::for_tests(true).is_container());
    }

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

    /// Agent git carries its own policy, and it rides on the COMMAND —
    /// the user's terminals in the same container must be untouched.
    #[test]
    fn agent_commands_carry_the_git_policy_into_the_container() {
        let ctx = ExecContext::host_unsandboxed_for_tests();
        ctx.set_container("abc123", "/workspace");
        let spec = ctx.resolve_for_agent("git", &["push"]);
        assert_eq!(spec.program, "podman");
        let joined = spec.args.join(" ");
        assert!(joined.contains("--env GIT_CONFIG_COUNT=5"), "{joined}");
        assert!(joined.contains("pushInsteadOf"), "{joined}");
        assert!(joined.contains("core.hooksPath"), "{joined}");
        // The command still runs in the workspace, as the user's would.
        assert!(joined.contains("--workdir /workspace"), "{joined}");
        assert!(joined.ends_with("git push"), "{joined}");

        // A plain resolve — the user's terminal — carries none of it.
        let plain = ctx.resolve("git", &["push"], false);
        assert!(!plain.args.join(" ").contains("GIT_CONFIG"));
    }

    /// Self-hosting: the IDE's own container IS the environment, and there
    /// is no podman to carry `--env`. `env(1)` carries it instead.
    #[test]
    fn agent_commands_carry_the_git_policy_without_a_container_target() {
        let ctx = ExecContext::for_tests(true);
        let spec = ctx.resolve_for_agent("git", &["push"]);
        assert_eq!(spec.program, "env");
        let joined = spec.args.join(" ");
        assert!(joined.contains("GIT_CONFIG_COUNT=5"), "{joined}");
        assert!(joined.ends_with("git push"), "{joined}");
    }

    /// What a client-served ACP terminal resolves to: the agent's own
    /// working directory and variables, in this environment's container,
    /// with the git policy still on top of them.
    #[test]
    fn an_agent_terminal_carries_its_cwd_and_env_into_the_container() {
        let ctx = ExecContext::host_unsandboxed_for_tests();
        ctx.set_container("abc123", "/workspace");
        let spec = ctx.resolve_for_agent_in(
            Some("/workspace/crates/taste-core"),
            &[("RUST_LOG".into(), "debug".into())],
            "cargo",
            &["test"],
        );
        let joined = spec.args.join(" ");
        assert_eq!(spec.program, "podman");
        assert!(joined.contains("--env RUST_LOG=debug"), "{joined}");
        assert!(
            joined.contains("--workdir /workspace/crates/taste-core"),
            "{joined}"
        );
        assert!(
            !joined.contains("--workdir /workspace "),
            "one workdir only, or podman picks: {joined}"
        );
        assert!(joined.ends_with("cargo test"), "{joined}");
    }

    /// The agent's own variables must not be able to undo the git policy:
    /// the policy is applied last, and the last `--env` is the one podman
    /// hands the process.
    #[test]
    fn agent_supplied_env_cannot_shadow_the_git_policy() {
        let ctx = ExecContext::host_unsandboxed_for_tests();
        ctx.set_container("abc123", "/workspace");
        let spec = ctx.resolve_for_agent_in(
            None,
            &[("GIT_CONFIG_COUNT".into(), "0".into())],
            "git",
            &["push"],
        );
        let joined = spec.args.join(" ");
        let theirs = joined.find("--env GIT_CONFIG_COUNT=0").expect("theirs");
        let ours = joined.find("--env GIT_CONFIG_COUNT=5").expect("ours");
        assert!(ours > theirs, "the policy must come last: {joined}");
    }

    /// Self-hosting has no podman to carry either, so `env(1)` carries
    /// both — including the directory, via its own `--chdir`.
    #[test]
    fn without_a_container_target_env_carries_the_cwd_too() {
        let ctx = ExecContext::for_tests(true);
        let spec = ctx.resolve_for_agent_in(Some("/work/p"), &[], "ls", &[]);
        assert_eq!(spec.program, "env");
        assert_eq!(spec.args[0], "--chdir=/work/p");
        assert!(spec.args.join(" ").ends_with("ls"));
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

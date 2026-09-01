//! **Where podman runs** — the one fact every podman invocation in the IDE
//! has to agree on.
//!
//! Until now that fact had exactly two values and was rediscovered
//! independently at seven sites: `podman`, or `flatpak-spawn --host podman`
//! when the IDE is sandboxed. It has more values now. A [`PodmanTarget`] is
//! the whole of what a caller needs to know to reach the right podman
//! service, and it deliberately lives here — in the crate that links no GTK
//! and depends on nothing — because `taste-devcontainer` (lifecycle),
//! `taste-acp` (agent spawns) and [`crate::exec`] (`ide_exec`, terminals)
//! all compose podman command lines and none of them may depend on the
//! others.
//!
//! # Why a connection, not a machine flag
//!
//! The substrate work could have added "run this in the VM" as a boolean.
//! It does not, because the interesting cases are not local-or-VM:
//!
//! - a **podman machine** is a local VM, reached by the connection podman
//!   registered for it when it was created;
//! - an **arbitrary remote host** with podman on it is reached by a
//!   connection over ssh — no VM anywhere, and no nested virtualization
//!   asked of the far end; and
//! - a **cloud VM** the IDE provisions later is that same case, arrived at
//!   by a provisioner that authenticates, creates a host, and registers a
//!   connection.
//!
//! All three reduce to "a name podman knows". So the abstraction's output
//! is a connection name, the machine is one producer of one, and the tier
//! above it needs no new plumbing — only a provisioner. See
//! `docs/ENVIRONMENTS.md` → "Remote substrate".
//!
//! # The two forms, and why both are needed
//!
//! [`PodmanTarget::argv`] is for command lines the IDE spawns itself:
//! `podman -c taste-ide exec …`. [`PodmanTarget::child_env`] is for
//! processes the IDE spawns that go on to run podman *themselves* — podman
//! reads `CONTAINER_CONNECTION` from its environment, and that is the
//! documented way to point a child at the same service without rewriting
//! its argv.
//!
//! One retargeting trap, recorded because it cost the substrate spike an
//! hour: the variable is **`CONTAINER_CONNECTION`**, singular. The plural
//! `CONTAINERS_CONNECTION` — the spelling the `containers.conf` family
//! trains you to write — is silently ignored, and everything then runs
//! against the local service while looking like it ran against the VM.

/// Which podman service a command should reach, and how to get there from
/// this process.
///
/// `Default` is the local service, unsandboxed — which is also exactly what
/// every call site meant before connections existed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PodmanTarget {
    /// A registered podman connection, or `None` for the local service.
    ///
    /// Never a URL: `podman system connection add` is what turns a URL into
    /// a name, and holding the name means the identity file, the port and
    /// the user are podman's business rather than a second copy of them
    /// here.
    connection: Option<String>,
    /// True when the IDE runs inside a Flatpak sandbox. podman lives on the
    /// host in that case whatever the connection is — the connection says
    /// which podman *service*, the sandbox says which podman *binary*.
    sandboxed: bool,
}

/// The environment variable podman reads to pick a connection.
pub const CONNECTION_ENV: &str = "CONTAINER_CONNECTION";

impl PodmanTarget {
    /// The local rootless service, detecting the Flatpak sandbox the way
    /// every call site used to detect it for itself.
    pub fn detect_local() -> Self {
        Self {
            connection: None,
            sandboxed: std::path::Path::new("/.flatpak-info").exists(),
        }
    }

    pub fn local(sandboxed: bool) -> Self {
        Self {
            connection: None,
            sandboxed,
        }
    }

    /// A named connection — a machine's, or any podman service reachable
    /// over ssh.
    pub fn connection(name: impl Into<String>, sandboxed: bool) -> Self {
        Self {
            connection: Some(name.into()),
            sandboxed,
        }
    }

    /// The same target with its connection replaced. `None` returns to the
    /// local service.
    pub fn with_connection(&self, name: Option<String>) -> Self {
        Self {
            connection: name,
            sandboxed: self.sandboxed,
        }
    }

    pub fn connection_name(&self) -> Option<&str> {
        self.connection.as_deref()
    }

    pub fn sandboxed(&self) -> bool {
        self.sandboxed
    }

    /// Whether this is the local rootless service — the case where the
    /// substrate adds nothing and behaviour is exactly what it always was.
    pub fn is_local(&self) -> bool {
        self.connection.is_none()
    }

    /// The program to spawn.
    pub fn program(&self) -> &'static str {
        if self.sandboxed {
            "flatpak-spawn"
        } else {
            "podman"
        }
    }

    /// Everything that goes before the podman subcommand.
    ///
    /// `-c` is a **global** podman flag and must precede the subcommand;
    /// `podman exec -c name` is a different flag on a different command and
    /// would be a confusing failure rather than a clear one.
    pub fn prefix(&self) -> Vec<String> {
        let mut prefix: Vec<String> = Vec::new();
        if self.sandboxed {
            prefix.push("--host".into());
            prefix.push("podman".into());
        }
        if let Some(connection) = &self.connection {
            prefix.push("-c".into());
            prefix.push(connection.clone());
        }
        prefix
    }

    /// A complete `(program, args)` for a podman invocation.
    pub fn argv<I, S>(&self, args: I) -> (String, Vec<String>)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut argv = self.prefix();
        argv.extend(args.into_iter().map(Into::into));
        (self.program().to_string(), argv)
    }

    /// What a child process that runs podman itself needs in its
    /// environment to reach the same service.
    ///
    /// Empty for the local target, so nothing is set that was not set
    /// before — an inherited `CONTAINER_CONNECTION` from the user's own
    /// shell is left exactly as podman would have honoured it anyway.
    pub fn child_env(&self) -> Vec<(String, String)> {
        match &self.connection {
            Some(connection) => vec![(CONNECTION_ENV.to_string(), connection.clone())],
            None => Vec::new(),
        }
    }

    /// How to say this target in a log line or an environment fact.
    pub fn describe(&self) -> String {
        match &self.connection {
            Some(connection) => format!("podman connection {connection}"),
            None => "local rootless podman".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The local target must compose exactly what the pre-connection code
    /// composed, byte for byte. Every existing installation is this case,
    /// and "unchanged behaviour" is a property, not an intention.
    #[test]
    fn the_local_target_is_what_the_code_always_did() {
        let (program, args) = PodmanTarget::local(false).argv(["ps", "-a"]);
        assert_eq!(program, "podman");
        assert_eq!(args, vec!["ps", "-a"]);

        let (program, args) = PodmanTarget::local(true).argv(["ps", "-a"]);
        assert_eq!(program, "flatpak-spawn");
        assert_eq!(args, vec!["--host", "podman", "ps", "-a"]);

        assert!(PodmanTarget::local(true).child_env().is_empty());
        assert!(PodmanTarget::default().is_local());
    }

    /// `-c` is global, so it goes before the subcommand — and after
    /// `flatpak-spawn --host podman`, which is not podman's argv at all but
    /// the way to reach the binary.
    #[test]
    fn a_connection_becomes_a_global_flag_ahead_of_the_subcommand() {
        let (program, args) =
            PodmanTarget::connection("taste-ide", false).argv(["exec", "-i", "c"]);
        assert_eq!(program, "podman");
        assert_eq!(args, vec!["-c", "taste-ide", "exec", "-i", "c"]);

        let (program, args) = PodmanTarget::connection("taste-ide", true).argv(["exec", "-i", "c"]);
        assert_eq!(program, "flatpak-spawn");
        assert_eq!(
            args,
            vec!["--host", "podman", "-c", "taste-ide", "exec", "-i", "c"]
        );
        // The subcommand is never separated from its own flags by ours.
        let position = args.iter().position(|a| a == "exec").unwrap();
        assert!(args.iter().position(|a| a == "-c").unwrap() < position);
    }

    /// A child that runs podman itself is retargeted by environment, and
    /// the spelling is the singular one. The plural is silently ignored by
    /// podman, which is the failure mode this test exists to prevent.
    #[test]
    fn a_child_is_retargeted_by_the_singular_environment_variable() {
        let env = PodmanTarget::connection("taste-ide", false).child_env();
        assert_eq!(
            env,
            vec![("CONTAINER_CONNECTION".to_string(), "taste-ide".to_string())]
        );
        assert_ne!(CONNECTION_ENV, "CONTAINERS_CONNECTION");
    }

    #[test]
    fn a_target_can_be_repointed_without_losing_the_sandbox_fact() {
        let sandboxed = PodmanTarget::local(true);
        let remote = sandboxed.with_connection(Some("elsewhere".into()));
        assert!(remote.sandboxed());
        assert_eq!(remote.connection_name(), Some("elsewhere"));
        assert!(remote.with_connection(None).is_local());
        assert!(remote.describe().contains("elsewhere"));
        assert!(PodmanTarget::default().describe().contains("local"));
    }
}

//! Who the out-of-memory killer takes in an environment: the work, never
//! the IDE's own processes.
//!
//! An environment's container has a memory limit, and the builds and model
//! checks its agent runs fill it: TLC's `java` was killed twenty-six times
//! in one day of remise's (2026-11-17). The kernel picks its victim by
//! size plus `oom_score_adj`, and every process in the container had the
//! same 200, the agent and the IDE's channel included, so a big enough
//! agent beside small enough work would have been the one taken. David,
//! that day: "We should take measures to not have the agent and IDE
//! integrations OOM-killed."
//!
//! Two halves, because the kernel lets an unprivileged process raise its
//! own score freely and lower it only as far as a floor that a privileged
//! write set above it. In the VM that floor is 100, written by systemd for
//! `user@.service`, which every container lives under:
//!
//! - **Everything starts at [`WORKLOAD_SCORE`].** The VM's user podman
//!   service runs with it (`taste_devcontainer::provision::GUEST_PODMAN_OOM`),
//!   and every process a `podman exec` starts takes the score the service
//!   had when it started — read once then, not at each fork. So a command
//!   no one thought to mark — the agent's, the user's, a task's, a language
//!   server — is the kind the killer takes first, and a spawn site added
//!   later is too, without knowing this module exists.
//! - **The IDE's own processes lower themselves** to the floor before they
//!   become what they are: the agent (and with it its MCP bridge, which it
//!   starts), the channel helper that carries the IDE's sockets, and the
//!   files service's keeper. [`protected`] is that, as a prefix to their
//!   argv.
//!
//! In a container capped at 4 GiB, 1000 against 100 is 3.6 GiB of
//! difference: the agent is chosen over a piece of the work only if it is
//! that much bigger than it, which is to say only if the agent is what
//! filled the container.

/// The score the VM's podman service runs with, and so every process it
/// starts until it lowers itself. The kernel's maximum: what is taken
/// first, the largest of it first.
pub const WORKLOAD_SCORE: i32 = 1000;

/// Lowers its own score as far as the kernel lets it — 0 where nothing
/// privileged set a floor (a shell's own session), 100 under the VM's
/// user manager — and execs its arguments, which keep the score and hand
/// it to what they start. A write the kernel refuses changes nothing, and
/// the command runs regardless: a process left at the workload's score is
/// worse off, not broken.
pub const PROTECT: &str = "for score in 0 100; do \
{ echo \"$score\" > /proc/self/oom_score_adj; } 2>/dev/null && break; done; exec \"$@\"";

/// `argv`, run through [`PROTECT`]: `sh -c PROTECT taste-ide argv…`. The
/// container's `sh`, which the IDE already relies on to stop what it ran
/// there (`crate::exec::Stopper`).
pub fn protected(argv: impl IntoIterator<Item = String>) -> Vec<String> {
    ["sh", "-c", PROTECT, "taste-ide"]
        .into_iter()
        .map(String::from)
        .chain(argv)
        .collect()
}

/// How many arguments [`protected`] puts in front of the command, for a
/// reader of the argv that wants the command itself.
pub const PROTECTED_PREFIX: usize = 4;

#[cfg(test)]
mod tests {
    use super::*;

    /// The command runs with its arguments intact, quoting and all, and
    /// its exit status is the wrapper's.
    #[test]
    fn the_command_runs_as_given() {
        let argv = protected(
            [
                "sh",
                "-c",
                "echo \"$#:$1:$2\"; exit 7",
                "inner",
                "one",
                "two words",
            ]
            .map(String::from),
        );
        let mut argv = argv.into_iter();
        let out = std::process::Command::new(argv.next().unwrap())
            .args(argv)
            .output()
            .unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        assert_eq!(text, "2:one:two words\n");
        assert_eq!(out.status.code(), Some(7));
    }

    /// The score it ends at is the lowest the kernel allows, and never
    /// higher than the one it started with.
    #[test]
    fn the_score_comes_down_where_it_may() {
        let Ok(before) = std::fs::read_to_string("/proc/self/oom_score_adj") else {
            return; // not Linux
        };
        let before: i32 = before.trim().parse().unwrap();
        let mut argv = protected(["cat".into(), "/proc/self/oom_score_adj".into()]).into_iter();
        let out = std::process::Command::new(argv.next().unwrap())
            .args(argv)
            .output()
            .unwrap();
        let after: i32 = String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(after <= before.max(100), "{before} → {after}");
    }
}

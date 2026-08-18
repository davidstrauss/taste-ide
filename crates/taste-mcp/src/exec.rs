//! Agent-brokered command execution, in the environment of record.
//!
//! The agent has no toolchain of its own and no workspace mounted where it
//! runs — its commands land in the *project's* devcontainer, the same one
//! the user's builds and terminals use. That is the point: an agent's
//! `cargo test` is the user's `cargo test`, same image, same cache, same
//! failures. Where that container is lives in
//! [`taste_core::ExecContext`], which the supervisor re-points on reload,
//! so a rebuild moves later commands without disturbing this registry.
//!
//! **There is no host fallback.** `ExecContext` resolves to the host when
//! no container is running, and running an untrusted agent's commands
//! unconfined on the user's host is the one thing this project must never
//! do. The caller checks the mode; this module refuses to spawn without a
//! container target as belt and braces.
//!
//! Jobs outlive a single tool call. The MCP watchdog bounds every call to
//! well under three minutes, but a cold `cargo build` does not care about
//! that — so a command that has not finished when its call must return
//! becomes a handle the agent polls. Short commands never notice.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

/// Per-stream output kept in memory. Both ends are kept because either can
/// hold the answer: a compiler's first error is usually the real one, and
/// the summary is always last. The middle is what gets dropped, loudly.
const HEAD_CAP: usize = 96 * 1024;
const TAIL_CAP: usize = 96 * 1024;

/// Bounded capture: head, tail, and an honest count of what fell out.
#[derive(Default)]
struct CappedOutput {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    total: usize,
}

impl CappedOutput {
    fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len();
        for &byte in bytes {
            if self.head.len() < HEAD_CAP {
                self.head.push(byte);
                continue;
            }
            if self.tail.len() == TAIL_CAP {
                self.tail.pop_front();
            }
            self.tail.push_back(byte);
        }
    }

    fn render(&self) -> String {
        let head = String::from_utf8_lossy(&self.head).to_string();
        if self.total <= HEAD_CAP {
            return head;
        }
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        let elided = self.total - self.head.len() - self.tail.len();
        format!(
            "{head}\n… {elided} bytes elided by the IDE (output cap) …\n{}",
            String::from_utf8_lossy(&tail)
        )
    }

    fn truncated(&self) -> bool {
        self.total > HEAD_CAP
    }
}

#[derive(Default)]
struct JobState {
    stdout: CappedOutput,
    stderr: CappedOutput,
    /// `None` while running.
    exit_code: Option<i32>,
    /// Set when the process died from a signal or could not be reaped.
    failure: Option<String>,
}

struct Job {
    /// The resolved command line, echoed back so the agent can see what
    /// actually ran — `podman exec …` and all.
    command: String,
    state: Arc<Mutex<JobState>>,
    /// Fires on completion; `ide_exec` waits on it instead of polling.
    done: Arc<tokio::sync::Notify>,
    kill: Arc<tokio::sync::Notify>,
}

/// A point-in-time view of a job, which is all a tool call can honestly
/// report.
#[derive(Debug)]
pub struct Snapshot {
    pub command: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub failure: Option<String>,
    pub truncated: bool,
}

/// Running and recently-finished agent commands, keyed by handle.
#[derive(Clone, Default)]
pub struct Jobs {
    inner: Arc<Mutex<HashMap<u64, Job>>>,
    next_id: Arc<AtomicU64>,
}

impl Jobs {
    /// Spawn `spec` and start draining it. Returns the job handle.
    ///
    /// `spec` must already have been resolved against a container target;
    /// `container` is that target's id, and its absence is a bug in the
    /// caller, not a reason to run on the host.
    pub fn spawn(
        &self,
        spec: taste_core::CommandSpec,
        container: Option<String>,
        inside_container: bool,
    ) -> Result<u64> {
        if container.is_none() && !inside_container {
            anyhow::bail!(
                "refusing to run an agent command with no container target — \
                 agent commands never run unconfined on the host"
            );
        }
        let command = std::iter::once(spec.program.clone())
            .chain(spec.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");
        let mut child = tokio::process::Command::new(&spec.program)
            .args(&spec.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {}", spec.program))?;

        let state = Arc::new(Mutex::new(JobState::default()));
        let done = Arc::new(tokio::sync::Notify::new());
        let kill = Arc::new(tokio::sync::Notify::new());
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;

        let stdout = child.stdout.take().context("child stdout")?;
        let stderr = child.stderr.take().context("child stderr")?;
        spawn_drain(stdout, state.clone(), true);
        spawn_drain(stderr, state.clone(), false);

        {
            let state = state.clone();
            let done = done.clone();
            let kill = kill.clone();
            tokio::spawn(async move {
                let status = tokio::select! {
                    status = child.wait() => status,
                    _ = kill.notified() => {
                        let _ = child.start_kill();
                        child.wait().await
                    }
                };
                let mut state = state.lock().unwrap();
                match status {
                    // A signal death has no exit code; say so rather than
                    // inventing a number the agent would read as a result.
                    Ok(status) => match status.code() {
                        Some(code) => state.exit_code = Some(code),
                        None => {
                            state.exit_code = Some(-1);
                            state.failure = Some(format!("terminated by signal ({status})"));
                        }
                    },
                    Err(e) => {
                        state.exit_code = Some(-1);
                        state.failure = Some(format!("could not reap the process: {e}"));
                    }
                }
                done.notify_waiters();
            });
        }

        self.inner.lock().unwrap().insert(
            id,
            Job {
                command,
                state,
                done,
                kill,
            },
        );
        Ok(id)
    }

    /// Wait up to `timeout` for the job to finish, then snapshot it either
    /// way. A finished job is dropped from the registry on the way out —
    /// its output is in the snapshot, and nothing else will ever read it.
    pub async fn wait(&self, id: u64, timeout: std::time::Duration) -> Result<Snapshot> {
        let (state, done) = {
            let jobs = self.inner.lock().unwrap();
            let job = jobs.get(&id).with_context(|| unknown_handle(id, &jobs))?;
            (job.state.clone(), job.done.clone())
        };
        // Register interest BEFORE checking, or a job that finishes in
        // between leaves this waiting for a notification already sent.
        let notified = done.notified();
        if state.lock().unwrap().exit_code.is_none() {
            let _ = tokio::time::timeout(timeout, notified).await;
        }
        let snapshot = {
            let state = state.lock().unwrap();
            let jobs = self.inner.lock().unwrap();
            Snapshot {
                command: jobs.get(&id).map(|j| j.command.clone()).unwrap_or_default(),
                stdout: state.stdout.render(),
                stderr: state.stderr.render(),
                exit_code: state.exit_code,
                failure: state.failure.clone(),
                truncated: state.stdout.truncated() || state.stderr.truncated(),
            }
        };
        if snapshot.exit_code.is_some() {
            self.inner.lock().unwrap().remove(&id);
        }
        Ok(snapshot)
    }

    /// Ask a running job to die. The drain tasks end with the pipes.
    pub fn kill(&self, id: u64) -> Result<()> {
        let jobs = self.inner.lock().unwrap();
        let job = jobs.get(&id).with_context(|| unknown_handle(id, &jobs))?;
        job.kill.notify_waiters();
        Ok(())
    }
}

/// The "no such handle" message, naming the handles that DO exist. A
/// collected handle is spent, and an agent holding a stale one should be
/// able to tell that from having invented one.
///
/// Takes the map rather than the registry: both callers are already
/// holding the lock, and `Mutex` is not reentrant.
fn unknown_handle(id: u64, jobs: &HashMap<u64, Job>) -> String {
    if jobs.is_empty() {
        return format!(
            "no such command handle: {id} — it either finished and was collected (a \
             handle is spent once its exit_code has been reported) or never existed. \
             Nothing is running."
        );
    }
    let live: Vec<String> = jobs.keys().map(u64::to_string).collect();
    format!(
        "no such command handle: {id} — a handle is spent once its exit_code has been \
         reported. Still running: {}",
        live.join(", ")
    )
}

fn spawn_drain<R>(reader: R, state: Arc<Mutex<JobState>>, is_stdout: bool)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut reader = reader;
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut state = state.lock().unwrap();
                    if is_stdout {
                        state.stdout.push(&buffer[..n]);
                    } else {
                        state.stderr.push(&buffer[..n]);
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jobs() -> Jobs {
        Jobs::default()
    }

    fn spec(program: &str, args: &[&str]) -> taste_core::CommandSpec {
        taste_core::CommandSpec {
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// The one rule this module exists to keep: no container, no run. An
    /// agent command on the user's bare host is the failure this whole
    /// topology is arranged to prevent.
    #[tokio::test]
    async fn refuses_to_run_without_a_container_target() {
        let error = jobs()
            .spawn(spec("echo", &["hi"]), None, false)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("never run unconfined on the host"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_short_command_finishes_within_the_first_call() {
        let jobs = jobs();
        // `inside_container` stands in for the self-hosting case, where the
        // IDE's own container IS the environment.
        let id = jobs.spawn(spec("echo", &["hello"]), None, true).unwrap();
        let snapshot = jobs
            .wait(id, std::time::Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(snapshot.exit_code, Some(0));
        assert_eq!(snapshot.stdout.trim(), "hello");
        // Reaped: its output has been delivered, so the handle is spent.
        assert!(jobs
            .wait(id, std::time::Duration::from_secs(1))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_failing_command_reports_its_code_and_stderr() {
        let jobs = jobs();
        let id = jobs
            .spawn(spec("sh", &["-c", "echo oops >&2; exit 3"]), None, true)
            .unwrap();
        let snapshot = jobs
            .wait(id, std::time::Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(snapshot.exit_code, Some(3));
        assert_eq!(snapshot.stderr.trim(), "oops");
    }

    /// A build outliving one tool call is the normal case, not an error:
    /// the first call returns a handle, a later one collects the result.
    #[tokio::test]
    async fn a_slow_command_becomes_a_pollable_handle() {
        let jobs = jobs();
        let id = jobs
            .spawn(spec("sh", &["-c", "sleep 0.4; echo done"]), None, true)
            .unwrap();
        let early = jobs
            .wait(id, std::time::Duration::from_millis(50))
            .await
            .unwrap();
        assert_eq!(early.exit_code, None, "still running");

        let later = jobs
            .wait(id, std::time::Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(later.exit_code, Some(0));
        assert_eq!(later.stdout.trim(), "done");
        // Collected, so spent — and the refusal says so rather than
        // leaving the agent to guess whether it invented the handle.
        let spent = jobs
            .wait(id, std::time::Duration::from_secs(1))
            .await
            .unwrap_err()
            .to_string();
        assert!(spent.contains("Nothing is running"), "{spent}");
    }

    #[tokio::test]
    async fn a_runaway_command_can_be_killed() {
        let jobs = jobs();
        let id = jobs.spawn(spec("sleep", &["600"]), None, true).unwrap();
        assert_eq!(
            jobs.wait(id, std::time::Duration::from_millis(50))
                .await
                .unwrap()
                .exit_code,
            None
        );
        jobs.kill(id).unwrap();
        let snapshot = jobs
            .wait(id, std::time::Duration::from_secs(10))
            .await
            .unwrap();
        assert!(snapshot.exit_code.is_some(), "kill must end the job");
    }

    #[test]
    fn output_beyond_the_cap_keeps_both_ends_and_says_what_it_dropped() {
        let mut output = CappedOutput::default();
        output.push(b"FIRST");
        output.push(&vec![b'x'; HEAD_CAP + TAIL_CAP]);
        output.push(b"LAST");
        let rendered = output.render();
        assert!(rendered.starts_with("FIRST"), "the first error survives");
        assert!(rendered.ends_with("LAST"), "the summary survives");
        assert!(rendered.contains("bytes elided"), "and the loss is stated");
        assert!(output.truncated());
    }
}

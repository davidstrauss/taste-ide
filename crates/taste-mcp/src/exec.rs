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
//!
//! **Every job also mirrors into its environment's shell roster**
//! ([`taste_core::shells`]), which is what puts it in the console as a
//! read-only tab beside the agent's ACP terminals. Same rendering, same
//! Kill: from the user's side an `ide_exec` build and a `terminal/create`
//! build are the same thing seen twice, and they should not look different
//! for a reason that is purely about which protocol carried the request.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use taste_core::shells::{ShellControl, ShellKind, ShellRoster, ShellSink, ShellState};
use taste_core::{CappedOutput, EnvironmentId};

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
    /// This job's row in the shell roster, when there is one. `None` in
    /// tests and anywhere the registry runs without a workspace.
    shell: Option<ShellSink>,
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
    /// Where jobs surface for the user. Optional because the registry is
    /// perfectly usable without one — a headless test has no console — and
    /// because an absent roster must degrade to "no mirror tab", never to
    /// a job that will not run.
    mirror: Option<(ShellRoster, EnvironmentId)>,
}

impl Jobs {
    /// A registry whose jobs appear in that environment's shell roster.
    pub fn for_environment(roster: ShellRoster, env: EnvironmentId) -> Self {
        Self {
            mirror: Some((roster, env)),
            ..Self::default()
        }
    }

    /// Spawn `spec` and start draining it. Returns the job handle.
    ///
    /// `spec` must already have been resolved against a container target;
    /// `container` is that target's id, and its absence is a bug in the
    /// caller, not a reason to run on the host.
    ///
    /// `display` is the command line as the USER should read it — what the
    /// agent asked for, before the execution context wrapped it. The caller
    /// passes it because the caller HAS it: recovering `cargo test` from
    /// `podman exec --env … abc123 cargo test --workspace` is a guess, and
    /// a guess in a tab title is a lie that looks like a feature.
    pub fn spawn(
        &self,
        spec: taste_core::CommandSpec,
        display: &str,
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

        // The console's mirror of this job. It gets the command the AGENT
        // asked for, not the `podman exec --env …` wrapper in `command`
        // above: that string exists so the agent can see what really ran,
        // and it is noise in a tab title.
        let shell = self.mirror.as_ref().map(|(roster, env)| {
            let control: Arc<dyn ShellControl> = Arc::new({
                let kill = kill.clone();
                // `notify_one`, not `notify_waiters`: the latter wakes only
                // waiters already parked, so a Kill that lands before the
                // wait task reaches its `select!` would be dropped and the
                // button would silently do nothing. `notify_one` stores a
                // permit, and there is exactly one waiter to spend it.
                move || kill.notify_one()
            });
            roster.register(env.clone(), ShellKind::ExecJob, display, Some(control))
        });

        let stdout = child.stdout.take().context("child stdout")?;
        let stderr = child.stderr.take().context("child stderr")?;
        spawn_drain(stdout, state.clone(), shell.clone(), true);
        spawn_drain(stderr, state.clone(), shell.clone(), false);

        {
            let state = state.clone();
            let done = done.clone();
            let kill = kill.clone();
            let shell = shell.clone();
            tokio::spawn(async move {
                let status = tokio::select! {
                    status = child.wait() => status,
                    _ = kill.notified() => {
                        let _ = child.start_kill();
                        child.wait().await
                    }
                };
                let ended = {
                    let mut state = state.lock().unwrap();
                    match status {
                        // A signal death has no exit code; say so rather than
                        // inventing a number the agent would read as a result.
                        Ok(status) => match status.code() {
                            Some(code) => {
                                state.exit_code = Some(code);
                                ShellState::Exited {
                                    code: Some(code),
                                    signal: None,
                                }
                            }
                            None => {
                                state.exit_code = Some(-1);
                                state.failure = Some(format!("terminated by signal ({status})"));
                                ShellState::Exited {
                                    code: None,
                                    signal: Some(signal_of(&status)),
                                }
                            }
                        },
                        Err(e) => {
                            state.exit_code = Some(-1);
                            state.failure = Some(format!("could not reap the process: {e}"));
                            ShellState::Exited {
                                code: None,
                                signal: None,
                            }
                        }
                    }
                };
                if let Some(shell) = &shell {
                    shell.finish(ended);
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
                shell,
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
            let job = self.inner.lock().unwrap().remove(&id);
            // A collected handle is spent, so the roster row goes with it.
            // The console TAB does not: its output is the record of what
            // happened, and it stays until the user closes it.
            if let Some(shell) = job.and_then(|job| job.shell) {
                shell.remove();
            }
        }
        Ok(snapshot)
    }

    /// Ask a running job to die. The drain tasks end with the pipes.
    pub fn kill(&self, id: u64) -> Result<()> {
        let jobs = self.inner.lock().unwrap();
        let job = jobs.get(&id).with_context(|| unknown_handle(id, &jobs))?;
        job.kill.notify_one();
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

/// The conventional name for the signal that ended a process, for the
/// roster. The agent gets `state.failure`, which already spells out the
/// whole `ExitStatus`.
fn signal_of(status: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match status.signal() {
        Some(1) => "SIGHUP".into(),
        Some(2) => "SIGINT".into(),
        Some(6) => "SIGABRT".into(),
        Some(9) => "SIGKILL".into(),
        Some(15) => "SIGTERM".into(),
        Some(other) => format!("SIG{other}"),
        None => "unknown signal".into(),
    }
}

fn spawn_drain<R>(reader: R, state: Arc<Mutex<JobState>>, shell: Option<ShellSink>, is_stdout: bool)
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
                    {
                        let mut state = state.lock().unwrap();
                        if is_stdout {
                            state.stdout.push(&buffer[..n]);
                        } else {
                            state.stderr.push(&buffer[..n]);
                        }
                    }
                    // The mirror interleaves both streams, as a terminal
                    // does; the agent still gets them apart.
                    if let Some(shell) = &shell {
                        shell.push(&buffer[..n]);
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
            .spawn(spec("echo", &["hi"]), "echo hi", None, false)
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
        let id = jobs
            .spawn(spec("echo", &["hello"]), "echo hello", None, true)
            .unwrap();
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
            .spawn(
                spec("sh", &["-c", "echo oops >&2; exit 3"]),
                "sh -c ...",
                None,
                true,
            )
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
            .spawn(
                spec("sh", &["-c", "sleep 0.4; echo done"]),
                "sh -c ...",
                None,
                true,
            )
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
        let id = jobs
            .spawn(spec("sleep", &["600"]), "sleep 600", None, true)
            .unwrap();
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

    fn env() -> EnvironmentId {
        EnvironmentId::parse("review").unwrap()
    }

    /// An `ide_exec` job is a console tab too: it shows up in ITS
    /// environment's roster, streams what it produced, and is killable from
    /// there — the same supervision an agent's ACP terminal gets, because
    /// which protocol carried the request is not the user's problem.
    #[tokio::test]
    async fn a_job_mirrors_into_its_environments_shell_roster() {
        let roster = ShellRoster::new();
        let jobs = Jobs::for_environment(roster.clone(), env());
        let id = jobs
            .spawn(
                spec("sh", &["-c", "echo mirrored"]),
                "sh -c echo mirrored",
                None,
                true,
            )
            .unwrap();

        let listed = roster.list(Some(&env()));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].kind, ShellKind::ExecJob);
        assert!(listed[0].killable);
        // The tab shows what the AGENT asked to run, not the wrapper.
        assert_eq!(listed[0].command, "sh -c echo mirrored");

        let (_, updates) = roster.watch(listed[0].id).unwrap();
        let snapshot = jobs
            .wait(id, std::time::Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(snapshot.exit_code, Some(0));

        let mut seen = String::new();
        while let Ok(update) = updates.try_recv() {
            if let taste_core::ShellUpdate::Output(bytes) = update {
                seen.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
        assert!(seen.contains("mirrored"), "{seen}");

        // Collected means spent: the row goes, and the console keeps its
        // tab because the output is the record of what happened.
        assert!(roster.list(None).is_empty());
    }

    /// Killing from the roster (the user's Kill button) reaches the job,
    /// not merely the row.
    #[tokio::test]
    async fn the_roster_can_stop_a_runaway_job() {
        let roster = ShellRoster::new();
        let jobs = Jobs::for_environment(roster.clone(), env());
        let id = jobs
            .spawn(spec("sleep", &["600"]), "sleep 600", None, true)
            .unwrap();
        roster.kill(roster.list(None)[0].id);
        let snapshot = jobs
            .wait(id, std::time::Duration::from_secs(10))
            .await
            .unwrap();
        assert!(snapshot.exit_code.is_some(), "the roster must reach it");
    }

    /// Without a roster the registry still works: a headless server has no
    /// console, and that must cost it no jobs.
    #[tokio::test]
    async fn a_registry_without_a_roster_still_runs_jobs() {
        let jobs = Jobs::default();
        let id = jobs
            .spawn(spec("echo", &["hi"]), "echo hi", None, true)
            .unwrap();
        assert_eq!(
            jobs.wait(id, std::time::Duration::from_secs(10))
                .await
                .unwrap()
                .exit_code,
            Some(0)
        );
    }

    /// The tab title is the agent's own command line, handed over by the
    /// caller rather than recovered from the wrapper the execution context
    /// built. A title reading `podman exec --env GIT_CONFIG_COUNT=5 …` is
    /// the IDE talking to itself in the user's console.
    #[tokio::test]
    async fn the_tab_title_is_what_the_agent_asked_for() {
        let ctx = taste_core::ExecContext::for_tests(true);
        let roster = ShellRoster::new();
        let jobs = Jobs::for_environment(roster.clone(), env());
        let resolved = ctx.resolve_for_agent("echo", &["hi"]);
        assert!(
            resolved.args.join(" ").contains("GIT_CONFIG_COUNT"),
            "the wrapper is what we are keeping out of the title"
        );
        jobs.spawn(resolved, "echo hi", None, true).unwrap();
        assert_eq!(roster.list(None)[0].command, "echo hi");
    }
}

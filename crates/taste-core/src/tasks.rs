//! The project's tasks (taskfile.dev): listed, run in the environment,
//! and their output kept — for the Tasks section and its tabs, and for the
//! agents' `task_*` tools alike (David, 2026-09-23: "I want Taskfile.dev
//! support … Also, expose tasks to the agent").
//!
//! **Listed by `task` itself, where it can be.** `task --list-all --json`,
//! run in the environment's container, is the list as `task` sees it —
//! includes, namespaces, and all — and it needs no YAML reader here.
//! Without `task`, or without a container to run it in, nothing is listed:
//! the section and the tools say what is missing instead.
//!
//! **Run where the files are, never on the host.** A task runs through
//! the environment's exec target, which is the container; a context with
//! no target refuses rather than falling back (CLAUDE.md → the boundary is
//! the host). One [`TaskBoard`] per workspace holds every run, whoever
//! started it — the user's button or an agent's `task_run` — so a run
//! either starts shows in the same row and the same tab, and its output
//! goes out on the bus in batches (`Event::TaskOutput`) rather than a line
//! at a time. A run is one task on the tokio runtime the workspace's other
//! processes use: its two streams, the flush tick, and Stop in one
//! `select!`, the process killed if the run is dropped.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::environment::EnvironmentId;
use crate::event::{Event, EventBus};
use crate::{ExecContext, Files};

/// Lines a run keeps, for a tab opened late and for `task_output`.
const KEPT_LINES: usize = 5000;

/// One task, as the section lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskInfo {
    /// The name `task` runs it by — `build`, or `docker:build` from an
    /// include.
    pub name: String,
    /// Its `desc`, when it has one.
    pub desc: String,
}

/// What a task's last run came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Running,
    Succeeded,
    Failed,
}

impl RunState {
    pub fn words(self) -> &'static str {
        match self {
            RunState::Idle => "",
            RunState::Running => "running",
            RunState::Succeeded => "succeeded",
            RunState::Failed => "failed",
        }
    }
}

/// What the section shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing {
    /// The checkout has no Taskfile: the ghost row alone.
    NoTaskfile,
    Tasks {
        /// The tasks as `task` lists them; empty when it could not be asked.
        tasks: Vec<TaskInfo>,
        /// Why `task` could not list them, when it could not: no container,
        /// or no `task` in it — and so none can be run either.
        cannot_run: Option<String>,
    },
}

/// The Task the IDE brings when the image has none: pinned, fetched into
/// each VM beside the agent's node and checked against these sums, and
/// mounted read-only into every container at [`IDE_TASK_DIR`]
/// (`taste_devcontainer::agentnode`). Task is one static binary, so it runs
/// whatever the image's C library. Bump all three from
/// `https://github.com/go-task/task/releases/download/v<version>/task_checksums.txt`.
pub const IDE_TASK_VERSION: &str = "3.54.0";
/// `task_linux_amd64.tar.gz`.
pub const IDE_TASK_SHA256_AMD64: &str =
    "680859dbb4d881a9c72d4d9a8f510825450849af8567deacd7302c01124416fb";
/// `task_linux_arm64.tar.gz`.
pub const IDE_TASK_SHA256_ARM64: &str =
    "d7189d439d4a6058e39e1b2acd7c8b27acd4856691b408f23b242e4294e1df5b";
/// Where the IDE's runtime directory is in a container.
pub const IDE_TASK_DIR: &str = "/opt/taste-agent";

/// A shell line setting `t` to the Task to run: the image's own, under
/// either of its names, first — a project that installs Task chose its
/// version — and the IDE's otherwise (David, 2026-10-04: "Can I have tasks
/// always work in devcontainers, similarly to how agents always work by
/// the IDE loading in the support?"). `t` is empty when there is none.
///
/// Fedora packages Task as `go-task`, binary and all, so an image that
/// installed it the obvious way had no `task` and the section said it was
/// not installed — a dead end an agent met writing a devcontainer, with the
/// tool sitting right there (2026-09-23).
pub fn locate_task() -> String {
    format!(
        "t=$(command -v task || command -v go-task || for f in \
         {IDE_TASK_DIR}/task-v{IDE_TASK_VERSION}-linux-*/task; do \
         if [ -x \"$f\" ]; then echo \"$f\"; break; fi; done)"
    )
}

/// The prelude a user's terminal in an environment starts with: `task` made
/// to resolve exactly as the GUI's runs resolve it ([`locate_task`]) — the
/// image's own `task` first, else its `go-task` (as a `task` function, since
/// that is the name everyone types), else the IDE's pinned Task, put last on
/// `PATH` — and then the interactive bash the terminal always ran, in its
/// place. Without it, `task` was the agents' and the GUI's and not the
/// user's: an agent's "run `task gmail:add` in the terminal" met
/// "task: command not found" in an image that has no Task of its own (David,
/// 2026-10-11). It runs the task as itself, with the terminal's input, so a
/// task that asks for a password or takes arguments works as it would
/// anywhere.
///
/// `wrapper` is the environment's `task` wrapper directory
/// (`taste_core::environment::container_task_wrapper_dir`): when the IDE's
/// channel has written its wrapper there, it goes first on `PATH`, and it
/// resolves the real Task this same way while telling the IDE the run is
/// happening, so it shows in the Tasks section.
pub fn terminal_prelude(wrapper: &Path) -> String {
    let wrapper = wrapper.display();
    format!(
        "if [ -x \"{wrapper}/task\" ]; then PATH=\"{wrapper}:$PATH\"; export PATH; fi; \
         if ! command -v task >/dev/null 2>&1; then \
           if command -v go-task >/dev/null 2>&1; then \
             task() {{ go-task \"$@\"; }}; export -f task; \
           else \
             for d in {IDE_TASK_DIR}/task-v{IDE_TASK_VERSION}-linux-*; do \
               if [ -x \"$d/task\" ]; then PATH=\"$PATH:$d\"; export PATH; break; fi; \
             done; \
           fi; \
         fi; \
         exec /bin/bash"
    )
}

/// `task`, wherever [`locate_task`] finds it, with `args`: the program and
/// argv to resolve in the container. The shell hands over with `exec`, so
/// there is one process, and exits 127 when there is no Task at all, which
/// is what "not installed" is read from.
fn task_command(args: &[&str]) -> (&'static str, Vec<String>) {
    let mut argv = vec![
        "-c".to_string(),
        format!(
            "{}; [ -n \"$t\" ] || exit 127; exec \"$t\" \"$@\"",
            locate_task()
        ),
        "task".to_string(),
    ];
    argv.extend(args.iter().map(|arg| arg.to_string()));
    ("sh", argv)
}

/// The tasks of the checkout at `root`, as `task` in the environment
/// lists them, else as the Taskfile names them. Blocking: a stat, maybe a
/// read, and a process in the container.
pub fn list(exec: &ExecContext, files: &Files, root: &Path) -> Listing {
    let has_taskfile = crate::conventions::TASKFILE_NAMES
        .iter()
        .any(|name| files.is_file(&root.join(name)));
    if !has_taskfile {
        return Listing::NoTaskfile;
    }
    let cannot_run = if exec.has_exec_target() {
        let (program, argv) = task_command(&["--list-all", "--json"]);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let spec = exec.resolve(program, &argv, false);
        match std::process::Command::new(&spec.program)
            .args(&spec.args)
            .stdin(std::process::Stdio::null())
            .output()
        {
            Ok(out) if out.status.success() => {
                if let Some(tasks) = parse_list_json(&out.stdout) {
                    return Listing::Tasks {
                        tasks,
                        cannot_run: None,
                    };
                }
                "task listed nothing it could be read from".to_string()
            }
            Ok(out) => {
                let said = String::from_utf8_lossy(&out.stderr);
                if said.contains("executable file not found") || out.status.code() == Some(127) {
                    "task is not in the environment's image (as task, or as Fedora's \
                     go-task), and the IDE's own Task is not deployed in its VM — the \
                     Virtual Machine log says why; add Task to the image to list and run \
                     these tasks"
                        .to_string()
                } else {
                    format!(
                        "task could not read the Taskfile: {}",
                        said.lines().last().unwrap_or("").trim()
                    )
                }
            }
            Err(e) => format!("the environment could not be asked: {e}"),
        }
    } else {
        "no container is running for task to list and run them in".to_string()
    };
    // No guess at the list without `task`: a list it cannot run is no use,
    // and `task` is the only reader of a Taskfile that is right about it
    // (David, 2026-09-23: "You shouldn't bother with a fallback reader if
    // we need \"task\" installed to run tasks").
    Listing::Tasks {
        tasks: Vec::new(),
        cannot_run: Some(cannot_run),
    }
}

/// A line as a terminal left it: a task runs in one, so its lines end in
/// `\r`, and a progress bar redraws itself with bare `\r`s — of which only
/// what the last one left is what a terminal ended up showing.
fn terminal_line(line: &str) -> String {
    let line = line.trim_end_matches('\r');
    line.rsplit('\r')
        .find(|part| !part.is_empty())
        .unwrap_or("")
        .to_string()
}

/// A line without its terminal escapes: what an agent reads of a task's
/// output (`task_output`), where colour is bytes to pay for and nothing to
/// see. CSI sequences (`ESC [ … final`) and OSC ones (`ESC ] … BEL` or
/// `ESC \\`) go; anything else is kept.
pub fn plain_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // A two-character escape (`ESC (`, `ESC =`, …): both go.
            Some(_) | None => {}
        }
    }
    out
}

/// `task --list-all --json`'s tasks: `{"tasks": [{"name", "desc", …}]}`.
fn parse_list_json(bytes: &[u8]) -> Option<Vec<TaskInfo>> {
    let doc: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let tasks = doc.get("tasks")?.as_array()?;
    Some(
        tasks
            .iter()
            .filter_map(|task| {
                Some(TaskInfo {
                    name: task.get("name")?.as_str()?.to_string(),
                    desc: task
                        .get("desc")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
            })
            .collect(),
    )
}

/// One line of the Tasks section's outline: a task, or a heading for the
/// namespace tasks share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineEntry {
    /// How deep it sits: 0 at the top.
    pub depth: usize,
    /// What the line says — the name's last part for a task, the
    /// namespace for a heading.
    pub label: String,
    /// The task this line is, by its index in the list; `None` for a
    /// namespace's heading.
    pub task: Option<usize>,
    /// The namespace a heading opens, `dev` or `db:migrate`; for a task,
    /// the namespace it sits in (empty at the top) — what folding keys on.
    pub path: String,
    /// A heading: it folds the lines under it. A heading can be a task too
    /// (`dev` beside `dev:init`), and then it runs as one.
    pub folds: bool,
}

/// The tasks as an outline on `task`'s own separator, in the order they
/// were listed: `dev:init` is `init` under a `dev` heading, and a task
/// named for a namespace (`dev` beside `dev:init`) is the heading itself
/// (David, 2026-09-23: "Show tasks as a hierarchy using the colon
/// separator").
pub fn outline(names: &[String]) -> Vec<OutlineEntry> {
    struct Node {
        label: String,
        task: Option<usize>,
        children: Vec<Node>,
    }
    let mut roots: Vec<Node> = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let parts: Vec<&str> = name.split(':').filter(|p| !p.is_empty()).collect();
        let mut level = &mut roots;
        for (depth, part) in parts.iter().enumerate() {
            let at = match level.iter().position(|node| node.label == *part) {
                Some(at) => at,
                None => {
                    level.push(Node {
                        label: part.to_string(),
                        task: None,
                        children: Vec::new(),
                    });
                    level.len() - 1
                }
            };
            let node = &mut level[at];
            if depth + 1 == parts.len() {
                node.task = Some(index);
            }
            level = &mut node.children;
        }
    }
    // A namespace is a heading. One that is only a namespace runs nothing;
    // one that is also a task (`dev` beside `dev:init`) is that task, and
    // runs as one (David, 2026-09-23: "It also doesn't make sense to
    // \"play\" dev if it's just a header … But you are correct that a
    // heading that's also a task gets a play button").
    fn flatten(nodes: &[Node], depth: usize, prefix: &str, out: &mut Vec<OutlineEntry>) {
        for node in nodes {
            if node.children.is_empty() {
                out.push(OutlineEntry {
                    depth,
                    label: node.label.clone(),
                    task: node.task,
                    path: prefix.to_string(),
                    folds: false,
                });
                continue;
            }
            let path = if prefix.is_empty() {
                node.label.clone()
            } else {
                format!("{prefix}:{}", node.label)
            };
            out.push(OutlineEntry {
                depth,
                label: node.label.clone(),
                task: node.task,
                path: path.clone(),
                folds: true,
            });
            flatten(&node.children, depth + 1, &path, out);
        }
    }
    let mut out = Vec::new();
    flatten(&roots, 0, "", &mut out);
    out
}

/// One run's record: what it said, how it ended, and how to stop it.
struct Run {
    lines: Vec<String>,
    state: RunState,
    stop: Option<Arc<tokio::sync::Notify>>,
}

/// Every task run in the workspace, by environment and name — the user's
/// and the agents' alike.
#[derive(Clone)]
pub struct TaskBoard {
    runs: Arc<Mutex<HashMap<(EnvironmentId, String), Run>>>,
    events: EventBus,
}

impl TaskBoard {
    pub fn new(events: EventBus) -> Self {
        Self {
            runs: Arc::new(Mutex::new(HashMap::new())),
            events,
        }
    }

    pub fn state(&self, env: &EnvironmentId, name: &str) -> RunState {
        self.runs
            .lock()
            .unwrap()
            .get(&(env.clone(), name.to_string()))
            .map_or(RunState::Idle, |run| run.state)
    }

    /// What the last run of `name` has said so far.
    pub fn lines(&self, env: &EnvironmentId, name: &str) -> Vec<String> {
        self.runs
            .lock()
            .unwrap()
            .get(&(env.clone(), name.to_string()))
            .map(|run| run.lines.clone())
            .unwrap_or_default()
    }

    /// Stop a running task: the process running it killed.
    pub fn stop(&self, env: &EnvironmentId, name: &str) {
        let stop = self
            .runs
            .lock()
            .unwrap()
            .get(&(env.clone(), name.to_string()))
            .and_then(|run| run.stop.clone());
        if let Some(stop) = stop {
            stop.notify_one();
        }
    }

    /// Run `name` in `exec`'s container, resolved as the user's command or
    /// — `by_agent` — as an agent's, with the agent's git policy. Refused,
    /// never run on the host, when there is no container to run it in, and
    /// while a run of it is under way. Runs on the tokio runtime the caller
    /// is in (the GTK side enters the app's for the call).
    pub fn start(
        &self,
        env: &EnvironmentId,
        name: &str,
        exec: &ExecContext,
        by_agent: bool,
    ) -> Result<(), String> {
        if !exec.has_exec_target() {
            return Err("no container is running to run it in".to_string());
        }
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| "no runtime to run it on (the caller must be in one)".to_string())?;
        let key = (env.clone(), name.to_string());
        if self.state(env, name) == RunState::Running {
            return Err(format!("{name} is already running"));
        }
        let (program, argv) = task_command(&[name]);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let spec = if by_agent {
            exec.resolve_for_agent_in_terminal(program, &argv)
        } else {
            exec.resolve_in_terminal(program, &argv)
        };
        let mut child = {
            let _in = runtime.enter();
            tokio::process::Command::new(&spec.program)
                .args(&spec.args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| format!("could not run task: {e}"))?
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stopper = spec.stopper.clone();
        let stop = Arc::new(tokio::sync::Notify::new());
        let first = format!("$ task {name}");
        self.runs.lock().unwrap().insert(
            key.clone(),
            Run {
                lines: Vec::new(),
                state: RunState::Running,
                stop: Some(stop.clone()),
            },
        );
        self.publish_lines(&key, vec![first]);
        self.events.publish(Event::TaskState {
            env: env.clone(),
            name: name.to_string(),
        });

        let board = self.clone();
        runtime.spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
                return;
            };
            let mut out = tokio::io::BufReader::new(stdout).lines();
            let mut err = tokio::io::BufReader::new(stderr).lines();
            let (mut out_done, mut err_done) = (false, false);
            // Lines in batches, a tenth of a second or two hundred at a
            // time: the bus reaches the GTK thread, and a build says a
            // great many lines.
            let mut batch: Vec<String> = Vec::new();
            let mut flush = tokio::time::interval(std::time::Duration::from_millis(100));
            // When a stop's Ctrl+C has had its grace and SIGKILL follows.
            let mut killing: Option<tokio::time::Instant> = None;
            while !(out_done && err_done) {
                tokio::select! {
                    line = out.next_line(), if !out_done => match line {
                        Ok(Some(line)) => batch.push(terminal_line(&line)),
                        _ => out_done = true,
                    },
                    line = err.next_line(), if !err_done => match line {
                        Ok(Some(line)) => batch.push(terminal_line(&line)),
                        _ => err_done = true,
                    },
                    _ = flush.tick() => {
                        if !batch.is_empty() {
                            board.publish_lines(&key, std::mem::take(&mut batch));
                        }
                    }
                    // Stopped where it runs: Ctrl+C to the task and all it
                    // started in the container, as a terminal's would be,
                    // and SIGKILL after the grace (`exec::Stopper`). Killing
                    // only the `podman exec` here left the build running
                    // there, holding its lock. The streams close when it
                    // ends, and the loop ends on its own.
                    _ = stop.notified(), if killing.is_none() => {
                        match &stopper {
                            Some(stopper) => {
                                stopper.interrupt();
                                killing = Some(
                                    tokio::time::Instant::now() + crate::exec::Stopper::GRACE,
                                );
                            }
                            None => {
                                let _ = child.start_kill();
                            }
                        }
                        batch.push("— stopped —".to_string());
                    }
                    _ = tokio::time::sleep_until(killing.unwrap_or_else(tokio::time::Instant::now)),
                        if killing.is_some() =>
                    {
                        if let Some(stopper) = &stopper {
                            stopper.kill();
                        }
                        let _ = child.start_kill();
                        killing = None;
                    }
                }
                if batch.len() >= 200 {
                    board.publish_lines(&key, std::mem::take(&mut batch));
                }
            }
            if !batch.is_empty() {
                board.publish_lines(&key, batch);
            }
            let (state, last) = match child.wait().await {
                Ok(status) if status.success() => (RunState::Succeeded, "— done —".to_string()),
                Ok(status) => (RunState::Failed, format!("— failed: {status} —")),
                Err(e) => (RunState::Failed, format!("— could not run: {e} —")),
            };
            if let Some(run) = board.runs.lock().unwrap().get_mut(&key) {
                run.state = state;
                run.stop = None;
            }
            board.publish_lines(&key, vec![last]);
            board.events.publish(Event::TaskState {
                env: key.0.clone(),
                name: key.1.clone(),
            });
        });
        Ok(())
    }

    /// A run started somewhere other than this board — the user's terminal,
    /// an agent's shell — as the IDE's `task` wrapper in that environment
    /// reports it (`taste_devcontainer::channel`, `Service::Tasks`): shown
    /// as this board shows its own, its row's light and its output tab, so
    /// a run is seen whoever started it (David, 2026-10-11: "I'd still like
    /// to see when it's kicked off by the agent or CLI"). `header` is its
    /// first line. What comes back is how the board's stop reaches it: the
    /// reporter is told, and stops the task where it runs.
    pub fn begin_external(
        &self,
        env: &EnvironmentId,
        name: &str,
        header: String,
    ) -> Arc<tokio::sync::Notify> {
        let key = (env.clone(), name.to_string());
        let stop = Arc::new(tokio::sync::Notify::new());
        self.runs.lock().unwrap().insert(
            key.clone(),
            Run {
                lines: Vec::new(),
                state: RunState::Running,
                stop: Some(stop.clone()),
            },
        );
        self.publish_lines(&key, vec![header]);
        self.events.publish(Event::TaskState {
            env: env.clone(),
            name: name.to_string(),
        });
        stop
    }

    /// Lines an external run printed.
    pub fn external_lines(&self, env: &EnvironmentId, name: &str, lines: &[String]) {
        let key = (env.clone(), name.to_string());
        self.publish_lines(&key, lines.iter().map(|l| terminal_line(l)).collect());
    }

    /// An external run over: `code` its exit status, `None` when the
    /// reporter went without saying.
    pub fn end_external(&self, env: &EnvironmentId, name: &str, code: Option<i32>) {
        let key = (env.clone(), name.to_string());
        let (state, last) = match code {
            Some(0) => (RunState::Succeeded, "— done —".to_string()),
            Some(code) => (RunState::Failed, format!("— failed: exit status {code} —")),
            None => (
                RunState::Failed,
                "— ended without saying how (the terminal or shell went away) —".to_string(),
            ),
        };
        if let Some(run) = self.runs.lock().unwrap().get_mut(&key) {
            run.state = state;
            run.stop = None;
        }
        self.publish_lines(&key, vec![last]);
        self.events.publish(Event::TaskState {
            env: env.clone(),
            name: name.to_string(),
        });
    }

    fn publish_lines(&self, key: &(EnvironmentId, String), lines: Vec<String>) {
        if let Some(run) = self.runs.lock().unwrap().get_mut(key) {
            run.lines.extend(lines.iter().cloned());
            if run.lines.len() > KEPT_LINES {
                let over = run.lines.len() - KEPT_LINES;
                run.lines.drain(..over);
            }
        }
        self.events.publish(Event::TaskOutput {
            env: key.0.clone(),
            name: key.1.clone(),
            lines,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A terminal's `task` resolves as the GUI's runs do: the image's
    /// `task`, then its `go-task`, then the IDE's pinned one — and the
    /// terminal is still the interactive bash it was.
    #[test]
    fn a_terminal_resolves_task_as_the_gui_does() {
        let prelude = terminal_prelude(Path::new("/tmp/taste-ide-primary/bin"));
        let wrapper = prelude.find("/tmp/taste-ide-primary/bin/task").unwrap();
        let own = prelude.find("command -v task").unwrap();
        assert!(wrapper < own, "the IDE's wrapper first: {prelude}");
        let go = prelude.find("command -v go-task").unwrap();
        let ide = prelude
            .find(&format!("{IDE_TASK_DIR}/task-v{IDE_TASK_VERSION}-linux-"))
            .unwrap();
        assert!(own < go && go < ide, "{prelude}");
        assert!(prelude.contains("PATH=\"$PATH:$d\""), "the IDE's goes last");
        assert!(prelude.trim_end().ends_with("exec /bin/bash"));
    }

    #[test]
    fn terminal_lines_lose_their_returns_and_keep_what_a_redraw_left() {
        assert_eq!(terminal_line("   Compiling foo\r"), "   Compiling foo");
        assert_eq!(terminal_line(" 10%\r 50%\r100%\r"), "100%");
        assert_eq!(terminal_line("plain"), "plain");
        assert_eq!(terminal_line("\r"), "");
    }

    #[test]
    fn a_plain_line_has_no_escapes() {
        assert_eq!(
            plain_line("\u{1b}[1;32m   Compiling\u{1b}[0m foo v0.1"),
            "   Compiling foo v0.1"
        );
        assert_eq!(
            plain_line("\u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{7}"),
            "link"
        );
        assert_eq!(plain_line("no escapes"), "no escapes");
    }

    /// The line itself, run: an image with only Fedora's `go-task` runs
    /// it with the arguments intact, and one with neither exits 127.
    #[test]
    fn task_runs_under_either_name_and_127_names_neither() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let run = |path: &std::path::Path| {
            let (program, argv) = task_command(&["--list-all", "a b"]);
            assert_eq!(program, "sh");
            std::process::Command::new("/bin/sh")
                .args(&argv)
                .env("PATH", path)
                .output()
                .unwrap()
        };
        let neither = run(dir.path());
        assert_eq!(neither.status.code(), Some(127));
        let go_task = dir.path().join("go-task");
        std::fs::write(&go_task, "#!/bin/sh\nprintf '%s|' \"$@\"\n").unwrap();
        std::fs::set_permissions(&go_task, std::fs::Permissions::from_mode(0o755)).unwrap();
        let found = run(dir.path());
        assert!(found.status.success(), "{found:?}");
        assert_eq!(String::from_utf8_lossy(&found.stdout), "--list-all|a b|");
    }

    /// The IDE's Task is looked for where it is mounted, at the pinned
    /// version, and only after the image's own under either name.
    #[test]
    fn the_ides_task_is_the_last_one_looked_for() {
        let line = locate_task();
        let ide = line
            .find(&format!("{IDE_TASK_DIR}/task-v{IDE_TASK_VERSION}-linux-"))
            .expect(&line);
        assert!(line.find("command -v task").unwrap() < ide);
        assert!(line.find("command -v go-task").unwrap() < ide);
        for sum in [IDE_TASK_SHA256_AMD64, IDE_TASK_SHA256_ARM64] {
            assert_eq!(sum.len(), 64);
            assert!(sum.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn namespaced_tasks_sit_under_their_namespace() {
        let names: Vec<String> = ["build", "dev:init", "dev:serve", "dev", "db:migrate:up"]
            .map(str::to_string)
            .to_vec();
        let outlined = outline(&names);
        let lines: Vec<(usize, &str, Option<usize>)> = outlined
            .iter()
            .map(|e| (e.depth, e.label.as_str(), e.task))
            .collect();
        assert_eq!(
            lines,
            [
                (0, "build", Some(0)),
                (0, "dev", Some(3)),
                (1, "init", Some(1)),
                (1, "serve", Some(2)),
                (0, "db", None),
                (1, "migrate", None),
                (2, "up", Some(4)),
            ]
        );
    }

    #[test]
    fn tasks_own_list_is_read_from_its_json() {
        let json = br#"{"tasks":[{"name":"build","desc":"Build it","summary":"","up_to_date":false},{"name":"docs:serve","desc":""}],"location":"/w/Taskfile.yml"}"#;
        let tasks = parse_list_json(json).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[1].name, "docs:serve");
        assert_eq!(parse_list_json(b"not json"), None);
    }
}

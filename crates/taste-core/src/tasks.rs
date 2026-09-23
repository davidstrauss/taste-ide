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

/// `task`, under whichever of its two names the image has, with `args`:
/// the program and argv to resolve in the container.
///
/// Fedora packages Task as `go-task`, binary and all, so an image that
/// installed it the obvious way had no `task` and the section said it was
/// not installed — a dead end an agent met writing a devcontainer, with the
/// tool sitting right there (2026-09-23). The shell finds either and hands
/// over with `exec`, so there is one process, and exits 127 when neither
/// exists, which is what "not installed" is read from.
fn task_command(args: &[&str]) -> (&'static str, Vec<String>) {
    let mut argv = vec![
        "-c".to_string(),
        "t=$(command -v task || command -v go-task) || exit 127; exec \"$t\" \"$@\"".to_string(),
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
                    "task is not installed in the environment (as task, or as Fedora's \
                     go-task); add it to the devcontainer's image to list and run these tasks"
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
                    // Killing the process closes its streams, and the loop
                    // ends on its own.
                    _ = stop.notified() => {
                        let _ = child.start_kill();
                        batch.push("— stopped —".to_string());
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

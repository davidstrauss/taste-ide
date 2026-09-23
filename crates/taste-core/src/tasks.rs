//! The project's tasks (taskfile.dev): listed, run in the environment,
//! and their output kept — for the Tasks section and its tabs, and for the
//! agents' `task_*` tools alike (David, 2026-09-23: "I want Taskfile.dev
//! support … Also, expose tasks to the agent").
//!
//! **Listed by `task` itself, where it can be.** `task --list-all --json`,
//! run in the environment's container, is the list as `task` sees it —
//! includes, namespaces, and all — and it needs no YAML reader here. A
//! container without `task`, or no container at all, still lists the
//! Taskfile's own top-level tasks, read off the file ([`read_names`]), so
//! the section says what the project has even when it cannot run it — and
//! says why it cannot.
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
        tasks: Vec<TaskInfo>,
        /// Why they cannot be run, when they cannot: no container, or no
        /// `task` in it.
        cannot_run: Option<String>,
    },
}

/// The tasks of the checkout at `root`, as `task` in the environment
/// lists them, else as the Taskfile names them. Blocking: a stat, maybe a
/// read, and a process in the container.
pub fn list(exec: &ExecContext, files: &Files, root: &Path) -> Listing {
    let Some(taskfile) = crate::conventions::TASKFILE_NAMES
        .iter()
        .map(|name| root.join(name))
        .find(|path| files.is_file(path))
    else {
        return Listing::NoTaskfile;
    };
    let cannot_run = if exec.has_exec_target() {
        let spec = exec.resolve("task", &["--list-all", "--json"], false);
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
                    "task is not installed in the environment".to_string()
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
        "no container is running to run them in".to_string()
    };
    let text = files.read_to_string(&taskfile).unwrap_or_default();
    Listing::Tasks {
        tasks: read_names(&text),
        cannot_run: Some(cannot_run),
    }
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

/// The top-level tasks a Taskfile's own text names, with their `desc`:
/// the keys one indent under `tasks:`. Not a YAML reader — includes,
/// anchors, and flow style are `task`'s to read — only enough to list
/// what the file plainly says when `task` is not there to say it.
pub fn read_names(text: &str) -> Vec<TaskInfo> {
    let indent = |line: &str| line.len() - line.trim_start().len();
    let mut tasks: Vec<TaskInfo> = Vec::new();
    let mut in_tasks = false;
    let mut key_indent: Option<usize> = None;
    for line in text.lines() {
        let content = line.trim();
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        let at = indent(line);
        if at == 0 {
            in_tasks = content.trim_end_matches(':') == "tasks" && content.ends_with(':');
            key_indent = None;
            continue;
        }
        if !in_tasks {
            continue;
        }
        let key_at = *key_indent.get_or_insert(at);
        if at == key_at {
            if let Some((key, _)) = content.split_once(':') {
                let key = key.trim().trim_matches(['"', '\'']);
                if !key.is_empty() && !key.contains(' ') {
                    tasks.push(TaskInfo {
                        name: key.to_string(),
                        desc: String::new(),
                    });
                }
            }
        } else if at > key_at {
            if let (Some(task), Some(desc)) = (tasks.last_mut(), content.strip_prefix("desc:")) {
                if task.desc.is_empty() {
                    task.desc = desc.trim().trim_matches(['"', '\'']).to_string();
                }
            }
        }
    }
    tasks
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
    /// heading with no task of its own.
    pub task: Option<usize>,
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
    fn flatten(nodes: &[Node], depth: usize, out: &mut Vec<OutlineEntry>) {
        for node in nodes {
            out.push(OutlineEntry {
                depth,
                label: node.label.clone(),
                task: node.task,
            });
            flatten(&node.children, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    flatten(&roots, 0, &mut out);
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
        let spec = if by_agent {
            exec.resolve_for_agent("task", &[name])
        } else {
            exec.resolve("task", &[name], false)
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
                        Ok(Some(line)) => batch.push(line),
                        _ => out_done = true,
                    },
                    line = err.next_line(), if !err_done => match line {
                        Ok(Some(line)) => batch.push(line),
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
    fn a_taskfile_names_its_top_level_tasks_and_their_descriptions() {
        let text = "version: '3'\n\nvars:\n  GREETING: hi\n\ntasks:\n  default:\n    desc: Say hello\n    cmds:\n      - echo {{.GREETING}}\n  # a comment\n  build:\n    cmds:\n      - cargo build\n  \"lint\": cargo clippy\n\nincludes:\n  docs: ./docs\n";
        let tasks = read_names(text);
        let names: Vec<&str> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["default", "build", "lint"]);
        assert_eq!(tasks[0].desc, "Say hello");
        assert_eq!(tasks[1].desc, "");
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

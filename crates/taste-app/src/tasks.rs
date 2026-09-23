//! The project's tasks (taskfile.dev): listed under Tasks in the flank,
//! run in the environment, and read as a log in the editor (David,
//! 2026-09-23: "I want Taskfile.dev support").
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
//! the environment's exec target (`ExecContext::resolve`), which is the
//! container; a context with no target refuses rather than falling back
//! (CLAUDE.md → the boundary is the host). Each run streams its lines to
//! its log tab and keeps them, so a tab opened after the run started — or
//! after it ended — shows the whole of it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use gtk::glib;
use taste_core::environment::EnvironmentId;
use taste_core::{ExecContext, Files};

/// The Tasks section's glyph and a task tab's.
pub const TASK_ICON: &str = "system-run-symbolic";

/// Lines a run keeps for a tab opened late; the tab itself trims to its
/// own cap.
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
    /// The row's dot (`env-dot` classes, the fleet's own colours).
    pub fn dot(self) -> &'static str {
        match self {
            RunState::Idle => "unknown",
            RunState::Running => "amber",
            RunState::Succeeded => "green",
            RunState::Failed => "red",
        }
    }

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
    let Some(taskfile) = taste_core::conventions::TASKFILE_NAMES
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

/// One run's record: what it said, how it ended, and how to stop it.
struct Run {
    lines: Vec<String>,
    state: RunState,
    stop: Option<Arc<tokio::sync::Notify>>,
}

/// Every task run this window started, by environment and name.
#[derive(Default)]
pub struct TaskRuns {
    runs: RefCell<HashMap<(EnvironmentId, String), Run>>,
}

impl TaskRuns {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    pub fn state(&self, env: &EnvironmentId, name: &str) -> RunState {
        self.runs
            .borrow()
            .get(&(env.clone(), name.to_string()))
            .map_or(RunState::Idle, |run| run.state)
    }

    /// What the last run of `name` has said so far, for a tab opened now.
    pub fn lines(&self, env: &EnvironmentId, name: &str) -> Vec<String> {
        self.runs
            .borrow()
            .get(&(env.clone(), name.to_string()))
            .map(|run| run.lines.clone())
            .unwrap_or_default()
    }

    /// Stop a running task: its process in the container killed.
    pub fn stop(&self, env: &EnvironmentId, name: &str) {
        if let Some(stop) = self
            .runs
            .borrow()
            .get(&(env.clone(), name.to_string()))
            .and_then(|run| run.stop.clone())
        {
            stop.notify_one();
        }
    }

    /// Run `name` in `exec`'s container: `lines` hears each line as it
    /// comes, `changed` each change of state. Refused — never run on the
    /// host — when there is no container to run it in.
    pub fn start(
        self: &Rc<Self>,
        env: &EnvironmentId,
        name: &str,
        exec: &ExecContext,
        lines: impl Fn(&[String]) + 'static,
        changed: impl Fn() + 'static,
    ) -> Result<(), String> {
        if !exec.has_exec_target() {
            return Err("no container is running to run it in".to_string());
        }
        let key = (env.clone(), name.to_string());
        if self.state(env, name) == RunState::Running {
            return Err(format!("{name} is already running"));
        }
        let spec = exec.resolve("task", &[name], false);
        let stop = Arc::new(tokio::sync::Notify::new());
        let first = format!("$ task {name}");
        self.runs.borrow_mut().insert(
            key.clone(),
            Run {
                lines: vec![first.clone()],
                state: RunState::Running,
                stop: Some(stop.clone()),
            },
        );
        lines(std::slice::from_ref(&first));
        changed();

        enum Said {
            Line(String),
            Done(Result<std::process::ExitStatus, String>),
        }
        let (tx, rx) = async_channel::unbounded::<Said>();
        crate::runtime::runtime().spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let child = tokio::process::Command::new(&spec.program)
                .args(&spec.args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn();
            let mut child = match child {
                Ok(child) => child,
                Err(e) => {
                    let _ = tx.send(Said::Done(Err(e.to_string()))).await;
                    return;
                }
            };
            let mut out = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
            let mut err = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
            let (mut out_done, mut err_done) = (false, false);
            // The output until both streams close — or Stop, which kills
            // the process and so closes them — then how it exited.
            while !(out_done && err_done) {
                tokio::select! {
                    line = out.next_line(), if !out_done => match line {
                        Ok(Some(line)) => { let _ = tx.send(Said::Line(line)).await; }
                        _ => out_done = true,
                    },
                    line = err.next_line(), if !err_done => match line {
                        Ok(Some(line)) => { let _ = tx.send(Said::Line(line)).await; }
                        _ => err_done = true,
                    },
                    _ = stop.notified() => {
                        let _ = child.start_kill();
                        let _ = tx.send(Said::Line("— stopped —".to_string())).await;
                        break;
                    }
                }
            }
            let status = child.wait().await.map_err(|e| e.to_string());
            let _ = tx.send(Said::Done(status)).await;
        });

        let runs = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(said) = rx.recv().await {
                let Some(runs) = runs.upgrade() else { return };
                match said {
                    Said::Line(line) => {
                        if let Some(run) = runs.runs.borrow_mut().get_mut(&key) {
                            run.lines.push(line.clone());
                            if run.lines.len() > KEPT_LINES {
                                let over = run.lines.len() - KEPT_LINES;
                                run.lines.drain(..over);
                            }
                        }
                        lines(std::slice::from_ref(&line));
                    }
                    Said::Done(status) => {
                        let (state, last) = match status {
                            Ok(status) if status.success() => {
                                (RunState::Succeeded, "— done —".to_string())
                            }
                            Ok(status) => (RunState::Failed, format!("— failed: {status} —")),
                            Err(e) => (RunState::Failed, format!("— could not run: {e} —")),
                        };
                        if let Some(run) = runs.runs.borrow_mut().get_mut(&key) {
                            run.state = state;
                            run.stop = None;
                            run.lines.push(last.clone());
                        }
                        lines(std::slice::from_ref(&last));
                        changed();
                        return;
                    }
                }
            }
        });
        Ok(())
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
    fn tasks_own_list_is_read_from_its_json() {
        let json = br#"{"tasks":[{"name":"build","desc":"Build it","summary":"","up_to_date":false},{"name":"docs:serve","desc":""}],"location":"/w/Taskfile.yml"}"#;
        let tasks = parse_list_json(json).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[1].name, "docs:serve");
        assert_eq!(parse_list_json(b"not json"), None);
    }
}

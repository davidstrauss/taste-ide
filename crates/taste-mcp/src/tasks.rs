//! MCP tasks: a tool call that answers with a handle the client follows,
//! instead of holding the agent until it ends.
//!
//! The extension is `io.modelcontextprotocol/tasks`, on protocol revision
//! 2026-07-28. A client that declares it on a `tools/call` may be answered
//! with a task (`resultType: "task"`) and then asks `tasks/get` until the
//! task is over, on its own — the agent spends no turn polling, and goes on
//! working meanwhile. Claude Code follows a task on its own and hands the
//! result to the agent when it lands; after a restart, it follows it again
//! once the server answers (Claude Code docs, MCP → "MCP tasks that run on
//! the server").
//!
//! Which call becomes a task is the server's to decide. Here it is any call
//! still running after [`PROMOTE_AFTER`]: a quick call answers as it always
//! has, and the IDE's slow ones — a build through `ide_exec`, a rebuild, a
//! wait — answer with a task, and wait for what they wait on up to
//! [`TASK_WAIT_SECS`] rather than the bound a call holding the agent has
//! (`as_task`).
//!
//! **A task is its environment's.** A task id is drawn from the system's
//! randomness and is unguessable, but that is not the check: every
//! `tasks/*` request is answered only on the socket of the environment
//! that started the task, which is the identity the IDE gave that
//! connection before it read a byte. Another environment's agent asking
//! for it is told it does not exist. This is also why the store is the
//! server's rather than a connection's: an agent that died with its
//! container in a rebuild connects again on the same socket, and the task
//! it started before is still its own.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use taste_core::EnvironmentId;

/// The extension's identifier, in `server/discover` and in a request's
/// declared client capabilities.
pub const EXTENSION: &str = "io.modelcontextprotocol/tasks";

/// How long a call runs before it becomes a task. Long enough that what a
/// call usually is — a search, a read, a status — never becomes one, and
/// short enough that a build does not hold the agent.
#[cfg(not(test))]
pub const PROMOTE_AFTER: Duration = Duration::from_secs(5);
#[cfg(test)]
pub const PROMOTE_AFTER: Duration = Duration::from_millis(300);

/// The longest a task-backed call waits on what it waits for — a command,
/// a reload, a task run — where a call holding the agent waits two
/// minutes at most.
pub const TASK_WAIT_SECS: u64 = 30 * 60;

/// The ceiling on a task's work, past which it ends failed: the waits
/// above, and the time a call takes to say how one ended.
pub const TASK_WATCHDOG: Duration = Duration::from_secs(TASK_WAIT_SECS + 60);

/// How often the client is asked to look. Claude Code clamps it to
/// between a quarter of a second and thirty.
pub const POLL_INTERVAL_MS: u64 = 2000;

/// Tasks working at once, across the workspace. A call that would be one
/// more answers as a call does, within the ordinary bounds.
pub const MAX_WORKING: usize = 16;

/// How long a finished task is kept for the client to collect.
pub const RETAIN: Duration = Duration::from_secs(60 * 60);

tokio::task_local! {
    static AS_TASK: bool;
}

/// Whether the call running on this task will answer as an MCP task, and
/// so may wait up to [`TASK_WAIT_SECS`] for what it waits on.
pub fn as_task() -> bool {
    AS_TASK.try_with(|task| *task).unwrap_or(false)
}

/// Run `work` as a call that may become a task.
pub async fn scope<F: std::future::Future>(work: F) -> F::Output {
    AS_TASK.scope(true, work).await
}

/// How long a wait inside a tool lasts: what the agent asked for, bounded
/// by `max` in a call that holds the agent and by [`TASK_WAIT_SECS`] in
/// one that will answer as a task, where it waits as long as it needs to
/// unless it asked for less.
pub fn wait_secs(asked: Option<u64>, default: u64, max: u64) -> u64 {
    if as_task() {
        asked.unwrap_or(TASK_WAIT_SECS).min(TASK_WAIT_SECS)
    } else {
        asked.unwrap_or(default).min(max)
    }
}

/// Whether a request's declared capabilities include tasks.
pub fn declared(params: &Value) -> bool {
    params["_meta"]["io.modelcontextprotocol/clientCapabilities"]["extensions"][EXTENSION]
        .is_object()
}

#[derive(Debug, Clone)]
enum State {
    Working,
    Completed(Value),
    Failed(String),
    Cancelled,
}

struct Entry {
    env: EnvironmentId,
    tool: String,
    created: i64,
    updated: i64,
    state: State,
    /// When it stopped working, for [`RETAIN`].
    ended: Option<Instant>,
    /// The work, for a cancel to stop.
    work: Option<tokio::task::AbortHandle>,
}

/// Every task of a workspace's MCP server.
#[derive(Default)]
pub struct Tasks {
    entries: Mutex<HashMap<String, Entry>>,
}

impl Tasks {
    /// Whether one more task may start working.
    pub fn has_room(&self) -> bool {
        let mut entries = self.entries.lock().unwrap();
        purge(&mut entries);
        working(&entries) < MAX_WORKING
    }

    /// A new working task for `env`'s call to `tool`, whose work `work`
    /// stops: its id, or `None` when no more may work at once. Stored
    /// before it is returned, so a `tasks/get` for it answers from the
    /// moment the client can know it.
    pub fn create(
        &self,
        env: &EnvironmentId,
        tool: &str,
        work: tokio::task::AbortHandle,
    ) -> Option<String> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).ok()?;
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let now = now();
        let mut entries = self.entries.lock().unwrap();
        purge(&mut entries);
        if working(&entries) >= MAX_WORKING {
            return None;
        }
        entries.insert(
            id.clone(),
            Entry {
                env: env.clone(),
                tool: tool.to_string(),
                created: now,
                updated: now,
                state: State::Working,
                ended: None,
                work: Some(work),
            },
        );
        Some(id)
    }

    /// The task's call ended with `result`, a `CallToolResult`; a tool's
    /// own error is in it (`isError`) and the task is still completed.
    pub fn complete(&self, id: &str, result: Value) {
        self.end(id, State::Completed(result));
    }

    /// The task's work ended without a result to give.
    pub fn fail(&self, id: &str, why: &str) {
        self.end(id, State::Failed(why.to_string()));
    }

    fn end(&self, id: &str, state: State) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get_mut(id) {
            // A cancel is the client's last word: work that finishes after
            // it does not take the task back.
            if matches!(entry.state, State::Working) {
                entry.state = state;
                entry.updated = now();
                entry.ended = Some(Instant::now());
                entry.work = None;
            }
        }
    }

    /// The task as `tasks/get` answers it, if `env` has one by this id.
    pub fn get(&self, env: &EnvironmentId, id: &str) -> Option<Value> {
        let mut entries = self.entries.lock().unwrap();
        purge(&mut entries);
        let entry = entries.get(id).filter(|entry| &entry.env == env)?;
        Some(detailed(id, entry))
    }

    /// Stop the task's work and say it was cancelled: whether `env` has a
    /// task by this id. One already over keeps how it ended.
    pub fn cancel(&self, env: &EnvironmentId, id: &str) -> bool {
        let mut entries = self.entries.lock().unwrap();
        let Some(entry) = entries.get_mut(id).filter(|entry| &entry.env == env) else {
            return false;
        };
        if let Some(work) = entry.work.take() {
            work.abort();
        }
        if matches!(entry.state, State::Working) {
            entry.state = State::Cancelled;
            entry.updated = now();
            entry.ended = Some(Instant::now());
        }
        true
    }

    /// Whether `env` has a task by this id.
    pub fn knows(&self, env: &EnvironmentId, id: &str) -> bool {
        let entries = self.entries.lock().unwrap();
        entries.get(id).is_some_and(|entry| &entry.env == env)
    }
}

/// Seconds since the epoch, for a task's timestamps.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

fn working(entries: &HashMap<String, Entry>) -> usize {
    entries
        .values()
        .filter(|entry| matches!(entry.state, State::Working))
        .count()
}

fn purge(entries: &mut HashMap<String, Entry>) {
    entries.retain(|_, entry| entry.ended.is_none_or(|ended| ended.elapsed() < RETAIN));
}

/// The fields every task carries, in every status.
fn base(id: &str, entry: &Entry, status: &str) -> Value {
    json!({
        "taskId": id,
        "status": status,
        "createdAt": taste_git::issues::format_utc(entry.created),
        "lastUpdatedAt": taste_git::issues::format_utc(entry.updated),
        "ttlMs": Value::Null,
        "pollIntervalMs": POLL_INTERVAL_MS,
    })
}

/// The task as it stands: its status, and its result or error once over.
fn detailed(id: &str, entry: &Entry) -> Value {
    let mut task = match &entry.state {
        State::Working => {
            let mut task = base(id, entry, "working");
            task["statusMessage"] = json!(format!("{} is running", entry.tool));
            task
        }
        State::Completed(result) => {
            let mut task = base(id, entry, "completed");
            task["result"] = result.clone();
            task
        }
        State::Failed(why) => {
            let mut task = base(id, entry, "failed");
            task["statusMessage"] = json!(why);
            task["error"] = json!({ "code": -32603, "message": why });
            task
        }
        State::Cancelled => base(id, entry, "cancelled"),
    };
    task["resultType"] = json!("complete");
    task
}

/// What a `tools/call` answers when its call became task `id`: the task as
/// it starts, flat, with `resultType: "task"`.
pub fn created(tasks: &Tasks, env: &EnvironmentId, id: &str) -> Value {
    let mut task = tasks.get(env, id).unwrap_or_else(|| {
        json!({
            "taskId": id,
            "status": "working",
            "ttlMs": Value::Null,
            "pollIntervalMs": POLL_INTERVAL_MS,
        })
    });
    // A call that ended between becoming a task and this answer is still
    // announced as the task it is; the client's first `tasks/get` says how
    // it ended.
    if let Some(object) = task.as_object_mut() {
        object.remove("result");
        object.remove("error");
        object.insert("status".into(), json!("working"));
    }
    task["resultType"] = json!("task");
    task
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work() -> tokio::task::AbortHandle {
        tokio::spawn(std::future::pending::<()>()).abort_handle()
    }

    #[tokio::test]
    async fn a_task_is_its_environments_and_ends_once() {
        let tasks = Tasks::default();
        let mine = EnvironmentId::parse("i-0001").unwrap();
        let theirs = EnvironmentId::parse("i-0002").unwrap();
        let id = tasks.create(&mine, "ide_exec", work()).unwrap();
        assert_eq!(id.len(), 32);

        let task = tasks.get(&mine, &id).unwrap();
        assert_eq!(task["status"], "working");
        assert_eq!(task["resultType"], "complete");
        assert!(task["createdAt"].as_str().unwrap().ends_with('Z'));
        // Another environment is told nothing, and cannot cancel it.
        assert!(tasks.get(&theirs, &id).is_none());
        assert!(!tasks.cancel(&theirs, &id));

        let created = created(&tasks, &mine, &id);
        assert_eq!(created["resultType"], "task");
        assert_eq!(created["status"], "working");

        let result = json!({"content": [{"type": "text", "text": "ok"}], "isError": false});
        tasks.complete(&id, result.clone());
        let task = tasks.get(&mine, &id).unwrap();
        assert_eq!(task["status"], "completed");
        assert_eq!(task["result"], result);
        // Over is over: a cancel after it changes nothing.
        assert!(tasks.cancel(&mine, &id));
        assert_eq!(tasks.get(&mine, &id).unwrap()["status"], "completed");

        let stopped = tasks.create(&mine, "wait", work()).unwrap();
        assert!(tasks.cancel(&mine, &stopped));
        tasks.complete(&stopped, result);
        assert_eq!(tasks.get(&mine, &stopped).unwrap()["status"], "cancelled");
    }

    #[tokio::test]
    async fn only_so_many_work_at_once() {
        let tasks = Tasks::default();
        let env = EnvironmentId::parse("i-0001").unwrap();
        let ids: Vec<String> = (0..MAX_WORKING)
            .map(|_| tasks.create(&env, "wait", work()).unwrap())
            .collect();
        assert!(!tasks.has_room());
        assert!(tasks.create(&env, "wait", work()).is_none());
        tasks.fail(&ids[0], "the IDE stopped waiting");
        assert!(tasks.has_room());
        assert_eq!(tasks.get(&env, &ids[0]).unwrap()["error"]["code"], -32603);
    }

    #[tokio::test]
    async fn waits_are_longer_in_a_task() {
        assert_eq!(wait_secs(None, 60, 120), 60);
        assert_eq!(wait_secs(Some(600), 60, 120), 120);
        scope(async {
            assert!(as_task());
            assert_eq!(wait_secs(None, 60, 120), TASK_WAIT_SECS);
            assert_eq!(wait_secs(Some(600), 60, 120), 600);
            assert_eq!(wait_secs(Some(99_999), 60, 120), TASK_WAIT_SECS);
        })
        .await;
        assert!(!as_task());
    }
}

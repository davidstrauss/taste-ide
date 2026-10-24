//! What the IDE serves down an environment channel.
//!
//! `taste-devcontainer` opens the channel and demultiplexes it, but it
//! deliberately knows nothing about what rides on it — it depends on
//! neither the MCP server nor the auth proxy, and should not. This is the
//! one crate that can see both, so this is where the two ends are tied
//! together, in the smallest amount of code that can do it.
//!
//! The whole of the routing is: a connection tagged `Mcp` goes to this
//! workspace's MCP server as the environment the channel belongs to, a
//! connection tagged `Auth` goes to the workspace's auth proxy, and one
//! tagged `Tasks` is a run of `task` the IDE's wrapper is reporting, which
//! goes to the workspace's task board as that environment's. There is no
//! default — [`Service`] is a closed set, so a container can ask for one of
//! those things and nothing else.

use std::sync::Arc;

use taste_core::environment::EnvironmentId;
use taste_devcontainer::channel::{ChannelServices, ChannelStream, Service};
use taste_mcp::McpServer;

pub struct IdeChannelServices {
    mcp: Arc<McpServer>,
    tasks: taste_core::tasks::TaskBoard,
}

impl IdeChannelServices {
    pub fn new(mcp: Arc<McpServer>, tasks: taste_core::tasks::TaskBoard) -> Arc<Self> {
        Arc::new(Self { mcp, tasks })
    }
}

/// The longest line a task report may send, and the longest task name it
/// may give: a report is the wrapper's, and anything past these is a
/// stream that is not one.
const MAX_REPORT_LINE: u64 = 1 << 20;
const MAX_TASK_NAME: usize = 200;

/// One run of `task` as the wrapper in `env`'s container reports it:
/// `start`, its output as `out`, and `exit`, as JSON lines; the board's
/// stop goes back as a `stop` line. The run is the environment's whose
/// channel it came on — nothing in the report says otherwise.
///
/// A run the wrapper went on with while no IDE was listening comes back as
/// `resume`, with the end of its output, and carries on as a `start` does;
/// one that ended meanwhile is a single `ended`, from the channel helper at
/// its start, with the end of its output and its exit code.
async fn serve_task_report(
    board: taste_core::tasks::TaskBoard,
    env: EnvironmentId,
    stream: ChannelStream,
) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = tokio::io::BufReader::new(read);
    let mut run: Option<String> = None;
    let mut stop: Option<Arc<tokio::sync::Notify>> = None;
    let mut partial = String::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let stopped = async {
            match &stop {
                Some(stop) => stop.notified().await,
                None => std::future::pending().await,
            }
        };
        let mut limited = (&mut reader).take(MAX_REPORT_LINE);
        let read = tokio::select! {
            read = limited.read_until(b'\n', &mut line) => read,
            () = stopped => {
                let _ = write.write_all(b"stop\n").await;
                continue;
            }
        };
        if !matches!(read, Ok(n) if n > 0) {
            break;
        }
        let Ok(message) = serde_json::from_slice::<serde_json::Value>(&line) else {
            continue;
        };
        let event = message["ev"].as_str();
        match (event, &run) {
            (Some("start" | "resume" | "ended"), None) => {
                let Some(name) = message["name"].as_str().filter(|n| {
                    !n.is_empty() && n.len() <= MAX_TASK_NAME && !n.chars().any(char::is_control)
                }) else {
                    break;
                };
                let header = report_header(&message, event == Some("start"));
                let earlier: Vec<String> = message["text"]
                    .as_str()
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect();
                if event == Some("ended") {
                    let code = message["code"].as_i64().map(|c| c as i32);
                    board.record_ended(&env, name, header, &earlier, code);
                    return;
                }
                stop = Some(board.begin_external(&env, name, header));
                if !earlier.is_empty() {
                    board.external_lines(&env, name, &earlier);
                }
                run = Some(name.to_string());
            }
            (Some("out"), Some(name)) => {
                partial.push_str(message["text"].as_str().unwrap_or_default());
                if let Some(end) = partial.rfind('\n') {
                    let done: Vec<String> =
                        partial[..end].split('\n').map(str::to_string).collect();
                    partial.drain(..=end);
                    board.external_lines(&env, name, &done);
                }
            }
            (Some("exit"), Some(name)) => {
                if !partial.is_empty() {
                    board.external_lines(&env, name, &[std::mem::take(&mut partial)]);
                }
                let code = message["code"].as_i64().map(|c| c as i32).or(Some(1));
                board.end_external(&env, name, code);
                return;
            }
            _ => {}
        }
    }
    // Gone without an exit: the terminal closed, or the shell was killed.
    if let Some(name) = run {
        if !partial.is_empty() {
            board.external_lines(&env, &name, &[partial]);
        }
        board.end_external(&env, &name, None);
    }
}

/// A reported run's first line: the command, and who ran it — and, for one
/// found again rather than started, that it was going before this IDE was.
fn report_header(message: &serde_json::Value, fresh: bool) -> String {
    let args: Vec<String> = message["args"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a.as_str())
        .map(|a| {
            if a.is_empty() || a.chars().any(|c| c.is_whitespace() || c == '\'') {
                format!("'{}'", a.replace('\'', "'\\''"))
            } else {
                a.to_string()
            }
        })
        .collect();
    let from = match message["origin"].as_str() {
        Some("terminal") => Some("from a terminal"),
        Some("board") => None,
        Some("agent") => Some("by the agent"),
        _ => Some("from a shell (an agent's, or a script's)"),
    };
    let found = match message["ev"].as_str() {
        _ if fresh => None,
        Some("ended") => Some("its last run, which ended while the IDE was closed"),
        _ => Some("still running from before the IDE opened; the start of its output may be gone"),
    };
    let mut header = format!("$ task {}", args.join(" "));
    for part in [from, found].into_iter().flatten() {
        header.push_str(" — ");
        header.push_str(part);
    }
    header
}

impl ChannelServices for IdeChannelServices {
    /// The MCP server is always there. The auth proxy may not be — it is
    /// off with `TASTE_AUTH_PROXY=0`, and it may have failed to bind when
    /// the workspace opened it — and saying so is what stops the hosting
    /// probe from failing an environment over a door the IDE never opened.
    /// This is a read either way: the proxy is started once, from the
    /// runtime, by `open_workspace`.
    fn serves(&self, service: Service) -> bool {
        match service {
            Service::Mcp | Service::Tasks => true,
            Service::Auth => taste_acp::authproxy::handle().is_some(),
        }
    }

    fn accept(&self, env: &EnvironmentId, service: Service, stream: ChannelStream) {
        match service {
            // The environment comes from the channel, never from the
            // caller: this is the same unforgeable identity the per-
            // environment socket gave, established the same way — by which
            // container the IDE exec'd into.
            Service::Mcp => self.mcp.clone().serve_stream(env.clone(), stream),
            Service::Auth => {
                if let Some(handle) = taste_acp::authproxy::handle() {
                    handle.serve_stream(stream);
                }
            }
            Service::Tasks => {
                tokio::spawn(serve_task_report(self.tasks.clone(), env.clone(), stream));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use taste_core::tasks::{RunState, TaskBoard};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    /// A reported run lands on the task board as the GUI's own do: running
    /// on its start, its output as lines (a line split across reports kept
    /// whole), and its exit as the light; the board's stop goes back down
    /// the connection for the wrapper to act on.
    #[test]
    fn a_reported_run_is_on_the_board_and_its_stop_goes_back() {
        crate::runtime::runtime().block_on(async {
            let board = TaskBoard::new(taste_core::EventBus::new());
            let env = EnvironmentId::primary();
            let (ide, mut wrapper) = tokio::io::duplex(64 * 1024);
            let serving = tokio::spawn(serve_task_report(board.clone(), env.clone(), ide));
            let say = |line: &str| format!("{line}\n");
            wrapper
                .write_all(say(r#"{"ev":"start","name":"gmail:add","args":["gmail:add","--","personal","a b"],"origin":"terminal"}"#).as_bytes())
                .await
                .unwrap();
            wrapper
                .write_all(say(r#"{"ev":"out","text":"signing in\nsaved per"}"#).as_bytes())
                .await
                .unwrap();
            wrapper
                .write_all(say(r#"{"ev":"out","text":"sonal\n"}"#).as_bytes())
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(board.state(&env, "gmail:add"), RunState::Running);
            let lines = board.lines(&env, "gmail:add");
            assert!(lines[0].contains("task gmail:add -- personal 'a b'"), "{lines:?}");
            assert!(lines[0].contains("from a terminal"));
            assert_eq!(&lines[1..], ["signing in", "saved personal"]);

            // The board's stop reaches the wrapper as a line.
            board.stop(&env, "gmail:add");
            let mut back = tokio::io::BufReader::new(&mut wrapper);
            let mut heard = String::new();
            back.read_line(&mut heard).await.unwrap();
            assert_eq!(heard, "stop\n");

            wrapper
                .write_all(say(r#"{"ev":"exit","code":0}"#).as_bytes())
                .await
                .unwrap();
            serving.await.unwrap();
            assert_eq!(board.state(&env, "gmail:add"), RunState::Succeeded);
        });
    }

    /// A wrapper that goes without an exit — its terminal closed — leaves a
    /// failed run saying so, not one running for ever.
    #[test]
    fn a_report_cut_off_ends_the_run() {
        crate::runtime::runtime().block_on(async {
            let board = TaskBoard::new(taste_core::EventBus::new());
            let env = EnvironmentId::primary();
            let (ide, mut wrapper) = tokio::io::duplex(4096);
            let serving = tokio::spawn(serve_task_report(board.clone(), env.clone(), ide));
            wrapper
                .write_all(b"{\"ev\":\"start\",\"name\":\"index\",\"args\":[\"index\"],\"origin\":\"shell\"}\n")
                .await
                .unwrap();
            drop(wrapper);
            serving.await.unwrap();
            assert_eq!(board.state(&env, "index"), RunState::Failed);
            assert!(board.lines(&env, "index").last().unwrap().contains("went away"));
        });
    }

    /// A run found again after the IDE was away comes back running, with
    /// the end of what it printed meanwhile, and says it is one found.
    #[test]
    fn a_resumed_run_is_running_with_what_it_printed() {
        crate::runtime::runtime().block_on(async {
            let board = TaskBoard::new(taste_core::EventBus::new());
            let env = EnvironmentId::primary();
            let (ide, mut wrapper) = tokio::io::duplex(4096);
            let serving = tokio::spawn(serve_task_report(board.clone(), env.clone(), ide));
            wrapper
                .write_all(b"{\"ev\":\"resume\",\"name\":\"dev\",\"args\":[\"dev\"],\"origin\":\"board\",\"text\":\"listening on 5173\\nready\\n\"}\n")
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(board.state(&env, "dev"), RunState::Running);
            let lines = board.lines(&env, "dev");
            assert!(lines[0].starts_with("$ task dev — still running"), "{lines:?}");
            assert_eq!(&lines[1..], ["listening on 5173", "ready"]);
            wrapper.write_all(b"{\"ev\":\"exit\",\"code\":0}\n").await.unwrap();
            serving.await.unwrap();
            assert_eq!(board.state(&env, "dev"), RunState::Succeeded);
        });
    }

    /// A run that ended while the IDE was away is shown as the task's last
    /// run — but never over a run the board already has, which is newer.
    #[test]
    fn an_ended_run_is_shown_unless_the_board_knows_better() {
        crate::runtime::runtime().block_on(async {
            let board = TaskBoard::new(taste_core::EventBus::new());
            let env = EnvironmentId::primary();
            let report = |name: &str, code: i32| {
                format!(
                    "{{\"ev\":\"ended\",\"name\":\"{name}\",\"args\":[\"{name}\"],\"origin\":\"terminal\",\"code\":{code},\"text\":\"error: oops\\n\"}}\n"
                )
            };
            let (ide, mut wrapper) = tokio::io::duplex(4096);
            let serving = tokio::spawn(serve_task_report(board.clone(), env.clone(), ide));
            wrapper.write_all(report("build", 2).as_bytes()).await.unwrap();
            serving.await.unwrap();
            assert_eq!(board.state(&env, "build"), RunState::Failed);
            let lines = board.lines(&env, "build");
            assert!(lines[0].contains("ended while the IDE was closed"), "{lines:?}");
            assert_eq!(lines[1], "error: oops");

            // Again, saying it succeeded: the board's own record stands.
            let (ide, mut wrapper) = tokio::io::duplex(4096);
            let serving = tokio::spawn(serve_task_report(board.clone(), env.clone(), ide));
            wrapper.write_all(report("build", 0).as_bytes()).await.unwrap();
            serving.await.unwrap();
            assert_eq!(board.state(&env, "build"), RunState::Failed);
        });
    }
}

//! A notebook's kernel, run in the environment the notebook belongs to.
//!
//! Running a notebook runs the project's code, so the kernel lives where
//! the project's code always runs: in that environment's container, in its
//! VM (David, 2026-10-11: "Jupyter needs to execute code from the project
//! to run. Obviously, I expect that to happen in the sandboxed
//! workspace"). Nothing of it runs here: this module spawns `podman exec`
//! through the environment's [`taste_core::ExecContext`], the same route
//! rust-analyzer takes (`taste_mcp::lsp`), and refuses outright when there
//! is no container — never the host passthrough an `ExecContext` falls
//! back to.
//!
//! **The runtime is the container's.** The IDE does not ship or install
//! Jupyter (David: "It's okay to require Jupyter tooling of the
//! container's choice in the container"): the launcher picks the first of
//! the project's `.venv/bin/python`, `venv/bin/python`, `python3`, and
//! `python` that can import `jupyter_client` — the Jupyter project's own
//! client library, which ships with `ipykernel` — and runs the bridge
//! (`kernel_bridge.py`) on it. The bridge is the one thing the IDE brings:
//! a page of glue that starts the notebook's kernel through
//! `jupyter_client` and relays the Jupyter messaging protocol as JSON
//! lines over the exec's stdio. The kernel's ZeroMQ sockets stay on the
//! container's loopback; this process reads lines and nothing else.
//!
//! When it cannot start, what it said comes back as [`Event::Failed`], and
//! the notebook offers it to the agent to fix in the container.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The bridge, run in the container on the container's Python.
const BRIDGE: &str = include_str!("kernel_bridge.py");

/// Finds a Python that has Jupyter and runs the bridge on it, from the
/// container's working directory — the workspace — so the project's own
/// virtual environment is found first. `$1` is the bridge.
const LAUNCHER: &str = r#"for p in "$PWD/.venv/bin/python" "$PWD/venv/bin/python" python3 python; do
  case "$p" in
    /*) [ -x "$p" ] || continue ;;
    *) command -v "$p" >/dev/null 2>&1 || continue ;;
  esac
  if "$p" -c 'import jupyter_client' >/dev/null 2>&1; then exec "$p" -u -c "$1"; fi
done
echo '{"ev": "failed", "message": "no Python in this environment can import jupyter_client (tried .venv/bin/python, venv/bin/python, python3, and python)", "kernels": []}'
"#;

/// How much of the bridge's stderr is kept, for a failure's report.
const STDERR_LINES: usize = 60;
/// The longest line read from the bridge. An output is one line, and a
/// kernel can print without end; a line past this is dropped, and said,
/// rather than held in this process whole.
const MAX_LINE: usize = 64 << 20;
/// How many events wait for the GTK thread before the bridge is made to
/// wait: a kernel flooding messages is slowed, not queued without bound.
const EVENT_QUEUE: usize = 256;
/// How many events the GTK thread handles before it lets the rest of the
/// main loop run.
const EVENT_BATCH: usize = 64;

/// What the kernel says, on the GTK thread.
#[derive(Debug, Clone)]
pub enum Event {
    /// The kernel is up.
    Ready { display_name: String },
    /// It did not start, and why — the bridge's own words, and its stderr.
    Failed { message: String, report: String },
    /// An iopub message something this side sent set off: `kind` is the
    /// Jupyter message type (`stream`, `display_data`, `execute_result`,
    /// `error`, `execute_input`, `clear_output`, …), `content` its content,
    /// `parent` the id of the request it answers. `cell` names the cell for
    /// a run's output, and is `None` for a widget callback's — which an
    /// Output widget may be capturing by that parent.
    Output {
        cell: Option<String>,
        parent: String,
        kind: String,
        content: Value,
    },
    /// A widget's comm: `comm_open`, `comm_msg`, or `comm_close`, with its
    /// binary buffers decoded (`crate::widgets`).
    Comm {
        kind: String,
        content: Value,
        buffers: Vec<Vec<u8>>,
    },
    /// A cell's `execute_reply`: done, with its status and count.
    Reply { cell: String, content: Value },
    /// The kernel's state: `busy`, `idle`, `restarting`, or `dead`, and
    /// the cell it is about when it is a run's — a run is over on its reply
    /// AND its idle, since outputs can come after the reply.
    State { state: String, cell: Option<String> },
    /// Something the bridge could not do (an interrupt of a kernel already
    /// gone, an output too large to take), said.
    Error { message: String },
    /// The bridge exited — the kernel with it — and what it said last.
    Exited { report: String },
}

/// A running kernel. Dropping it ends the bridge's input, which shuts the
/// kernel down in the container; the exec is killed if it has not gone
/// within a few seconds of that.
pub struct Kernel {
    lines: tokio::sync::mpsc::UnboundedSender<String>,
}

impl Kernel {
    /// Start the kernel `name` (empty: the bridge's choice) with `cwd` as
    /// its directory, in the container `exec` reaches. `on_event` hears
    /// everything it says, on the GTK thread.
    pub fn start(
        exec: &taste_core::ExecContext,
        name: &str,
        cwd: &Path,
        on_event: impl Fn(Event) + 'static,
    ) -> Result<Self> {
        // The host passthrough would run the project's code on the user's
        // own machine. A notebook has nowhere to run without a container,
        // and says so.
        if !exec.has_exec_target() {
            anyhow::bail!(
                "no container is running for this environment, so the notebook's kernel has \
                 nowhere to run — and project code never runs on your machine"
            );
        }
        let spec = exec.resolve("sh", &["-c", LAUNCHER, "sh", BRIDGE], false);
        let (lines_tx, mut lines_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel::<Event>(EVENT_QUEUE);
        let start = serde_json::json!({
            "op": "start",
            "kernel": name,
            "cwd": cwd.display().to_string(),
        })
        .to_string();
        lines_tx.send(start).ok();

        let mut command = tokio::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = {
            let _guard = crate::runtime::runtime().enter();
            command
                .spawn()
                .with_context(|| format!("starting {}", spec.program))?
        };
        let mut stdin = child.stdin.take().context("the kernel's stdin")?;
        let stdout = child.stdout.take().context("the kernel's stdout")?;
        let stderr = child.stderr.take().context("the kernel's stderr")?;
        let tail = std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::<String>::new(),
        ));

        // Requests in, until the kernel is dropped; then the input ends,
        // which is the bridge's signal to shut the kernel down.
        let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
        crate::runtime::runtime().spawn(async move {
            while let Some(line) = lines_rx.recv().await {
                if stdin
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
                let _ = stdin.flush().await;
            }
            drop(stdin);
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let _ = kill_tx.send(());
        });
        {
            let tail = tail.clone();
            crate::runtime::runtime().spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    taste_core::app_log::push("INFO", "notebook-kernel", &line);
                    let mut tail = tail.lock().unwrap();
                    tail.push_back(line);
                    if tail.len() > STDERR_LINES {
                        tail.pop_front();
                    }
                }
            });
        }
        {
            let events = events_tx.clone();
            let tail = tail.clone();
            crate::runtime::runtime().spawn(async move {
                let report = || {
                    tail.lock()
                        .unwrap()
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let mut reader = BufReader::new(stdout);
                let mut kill_rx = kill_rx;
                let mut line = Vec::new();
                loop {
                    line.clear();
                    let read = tokio::select! {
                        read = read_line_capped(&mut reader, &mut line, MAX_LINE) => read,
                        // The input ended and the bridge did not go: a
                        // bridge hung with its output open is killed all
                        // the same.
                        _ = &mut kill_rx => {
                            let _ = child.start_kill();
                            break;
                        }
                    };
                    let event = match read {
                        Ok(Line::End) | Err(_) => break,
                        Ok(Line::TooLong) => Event::Error {
                            message: format!(
                                "an output larger than {} MiB was dropped",
                                MAX_LINE >> 20
                            ),
                        },
                        Ok(Line::Whole) => {
                            match parse_event(&String::from_utf8_lossy(&line), &report) {
                                Some(event) => event,
                                None => continue,
                            }
                        }
                    };
                    if events.send(event).await.is_err() {
                        break;
                    }
                }
                let _ = child.wait().await;
                let _ = events.send(Event::Exited { report: report() }).await;
            });
        }
        gtk::glib::spawn_future_local(async move {
            let mut handled = 0usize;
            while let Some(event) = events_rx.recv().await {
                on_event(event);
                handled += 1;
                // A flood is taken in batches, with the rest of the main
                // loop — drawing, input — let in between.
                if handled.is_multiple_of(EVENT_BATCH) {
                    gtk::glib::timeout_future(std::time::Duration::ZERO).await;
                }
            }
        });
        Ok(Self { lines: lines_tx })
    }

    fn send(&self, request: Value) {
        let _ = self.lines.send(request.to_string());
    }

    /// Run `code` as `cell`; its output and reply come back as events
    /// naming `cell`.
    pub fn execute(&self, cell: &str, code: &str) {
        self.send(serde_json::json!({"op": "execute", "cell": cell, "code": code}));
    }

    /// A message to a widget's model in the kernel, on its comm.
    pub fn comm_msg(&self, comm_id: &str, data: Value, buffers: &[Vec<u8>]) {
        use base64::Engine as _;
        let buffers: Vec<String> = buffers
            .iter()
            .map(|b| base64::engine::general_purpose::STANDARD.encode(b))
            .collect();
        self.send(serde_json::json!({
            "op": "comm_msg", "comm_id": comm_id, "data": data, "buffers": buffers,
        }));
    }

    pub fn interrupt(&self) {
        self.send(serde_json::json!({"op": "interrupt"}));
    }

    pub fn restart(&self) {
        self.send(serde_json::json!({"op": "restart"}));
    }
}

/// What one read of the bridge's output came to.
enum Line {
    Whole,
    /// Longer than [`MAX_LINE`]: read past and dropped.
    TooLong,
    End,
}

/// Read one line into `line`, at most `cap` bytes of it; a longer one is
/// read to its end without being kept.
async fn read_line_capped<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<Line> {
    use tokio::io::AsyncReadExt;
    let read = (&mut *reader)
        .take(cap as u64 + 1)
        .read_until(b'\n', line)
        .await?;
    if read == 0 {
        return Ok(Line::End);
    }
    if line.last() == Some(&b'\n') || line.len() <= cap {
        return Ok(Line::Whole);
    }
    // Past the cap: the rest of this line, a chunk at a time, thrown away.
    loop {
        let mut chunk = Vec::new();
        let read = (&mut *reader)
            .take(1 << 20)
            .read_until(b'\n', &mut chunk)
            .await?;
        if read == 0 || chunk.last() == Some(&b'\n') {
            line.clear();
            return Ok(Line::TooLong);
        }
    }
}

/// One line of the bridge's output as an event; `None` for a line that is
/// not one of its messages. `stderr` gives the bridge's last words, read
/// only for a failure's report.
fn parse_event(line: &str, stderr: &dyn Fn() -> String) -> Option<Event> {
    let message: Value = serde_json::from_str(line).ok()?;
    let text = |key: &str| message[key].as_str().unwrap_or_default().to_string();
    Some(match message["ev"].as_str()? {
        "ready" => Event::Ready {
            display_name: text("display_name"),
        },
        "failed" => {
            let kernels: Vec<&str> = message["kernels"]
                .as_array()
                .map(|k| k.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let mut report = text("message");
            if !kernels.is_empty() {
                report.push_str(&format!(
                    "\nKernels this environment has: {}",
                    kernels.join(", ")
                ));
            }
            let stderr = stderr();
            if !stderr.trim().is_empty() {
                report.push_str(&format!("\n\nThe bridge's stderr:\n{stderr}"));
            }
            Event::Failed {
                message: text("message"),
                report,
            }
        }
        "output" => Event::Output {
            cell: message["cell"].as_str().map(str::to_string),
            parent: text("parent"),
            kind: text("type"),
            content: message["content"].clone(),
        },
        "comm" => Event::Comm {
            kind: text("type"),
            content: message["content"].clone(),
            buffers: message["buffers"]
                .as_array()
                .map(|buffers| {
                    use base64::Engine as _;
                    buffers
                        .iter()
                        .filter_map(Value::as_str)
                        .filter_map(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                        .collect()
                })
                .unwrap_or_default(),
        },
        "reply" => Event::Reply {
            cell: text("cell"),
            content: message["content"].clone(),
        },
        "state" => Event::State {
            state: text("state"),
            cell: message["cell"].as_str().map(str::to_string),
        },
        "error" => Event::Error {
            message: text("message"),
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bridges_lines_are_read_as_events() {
        assert!(matches!(
            parse_event(r#"{"ev": "ready", "display_name": "Python 3", "language": "python"}"#, &String::new),
            Some(Event::Ready { display_name }) if display_name == "Python 3"
        ));
        match parse_event(
            r#"{"ev": "failed", "message": "no kernel named 'ir'", "kernels": ["python3"]}"#,
            &|| "Traceback: x".to_string(),
        ) {
            Some(Event::Failed { message, report }) => {
                assert_eq!(message, "no kernel named 'ir'");
                assert!(report.contains("python3"));
                assert!(report.contains("Traceback: x"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse_event(r#"{"ev": "output", "cell": "c1", "type": "stream", "content": {"name": "stdout", "text": "hi"}}"#, &String::new),
            Some(Event::Output { cell, kind, .. }) if cell.as_deref() == Some("c1") && kind == "stream"
        ));
        match parse_event(
            r#"{"ev": "comm", "type": "comm_msg", "content": {"comm_id": "m1"}, "buffers": ["aGk="], "parent": "p"}"#,
            &String::new,
        ) {
            Some(Event::Comm { kind, buffers, .. }) => {
                assert_eq!(kind, "comm_msg");
                assert_eq!(buffers, vec![b"hi".to_vec()]);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse_event(r#"{"ev": "output", "cell": null, "parent": "p", "type": "stream", "content": {}}"#, &String::new),
            Some(Event::Output { cell: None, parent, .. }) if parent == "p"
        ));
        assert!(parse_event("[IPKernelApp] WARNING | not json", &String::new).is_none());
    }

    /// A line past the cap is read past and dropped, and the next line is
    /// read whole.
    #[test]
    fn a_line_past_the_cap_is_dropped_and_the_next_is_read() {
        crate::runtime::runtime().block_on(async {
            let input: &[u8] = b"short\nmuch too long for the cap\nnext\n";
            let mut reader = tokio::io::BufReader::new(input);
            let mut line = Vec::new();
            let mut seen = Vec::new();
            loop {
                line.clear();
                match read_line_capped(&mut reader, &mut line, 8).await.unwrap() {
                    Line::End => break,
                    Line::TooLong => seen.push("<dropped>".to_string()),
                    Line::Whole => seen.push(String::from_utf8_lossy(&line).trim().to_string()),
                }
            }
            assert_eq!(seen, ["short", "<dropped>", "next"]);
        });
    }

    /// The exec gate: no container, no kernel — never the host.
    #[test]
    fn no_container_is_refused_rather_than_run_here() {
        let exec = taste_core::ExecContext::for_tests(false);
        let refused = Kernel::start(&exec, "python3", Path::new("/tmp"), |_| {});
        let Err(why) = refused else {
            panic!("a kernel started with no container");
        };
        assert!(format!("{why:#}").contains("never runs on your machine"));
    }

    /// The launcher names every place it looked when none has Jupyter, as
    /// the bridge's own failure message, so the notebook can say it.
    #[test]
    fn the_launcher_says_where_it_looked() {
        let line = LAUNCHER.lines().last().unwrap();
        let echoed = line.trim_start_matches("echo '").trim_end_matches('\'');
        assert!(matches!(
            parse_event(echoed, &String::new),
            Some(Event::Failed { message, .. }) if message.contains(".venv/bin/python")
        ));
    }
}

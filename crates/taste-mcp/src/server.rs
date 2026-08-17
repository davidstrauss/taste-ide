//! The MCP server proper: unix-socket listener + tool dispatch.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use taste_devcontainer::{Supervisor, SupervisorState};
use taste_flatpak::{Packager, PackagerState};
use taste_git::GitWorkspace;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::protocol::{tool, tool_result, Request, Response, PROTOCOL_VERSION};

/// Socket path for a workspace, keyed by the supervisor's stable
/// per-workspace name. Prefers `$XDG_RUNTIME_DIR` (world-unreadable by
/// construction) but falls back to `/tmp` when that directory isn't
/// writable — notably in the self-hosting bootstrap, where only the Wayland
/// socket is mounted at the runtime dir path.
pub fn socket_path(container_name: &str) -> PathBuf {
    let candidates: Vec<PathBuf> = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .into_iter()
        .chain([PathBuf::from("/tmp")])
        .collect();
    let file_name = format!("{container_name}-mcp.sock");
    for dir in &candidates {
        let probe = dir.join(format!(".taste-probe-{container_name}"));
        if std::fs::write(&probe, b"").is_ok() {
            let _ = std::fs::remove_file(&probe);
            return dir.join(&file_name);
        }
    }
    Path::new("/tmp").join(file_name)
}

pub struct McpServer {
    supervisor: Arc<Supervisor>,
    packager: Arc<Packager>,
    workspace: taste_core::Workspace,
}

impl McpServer {
    pub fn new(
        supervisor: Arc<Supervisor>,
        packager: Arc<Packager>,
        workspace: taste_core::Workspace,
    ) -> Arc<Self> {
        Arc::new(Self {
            supervisor,
            packager,
            workspace,
        })
    }

    fn safe_mode(&self) -> bool {
        !self.workspace.exec.is_container()
    }

    /// Bind the socket and serve until dropped. Each connection is handled
    /// concurrently; the protocol is stateless enough that this is safe.
    pub async fn serve(self: Arc<Self>, socket: PathBuf) -> Result<()> {
        // A second window on the same workspace must not unlink a live
        // server's socket out from under it; the first window's server
        // serves both (same workspace, same state sources).
        if UnixStream::connect(&socket).await.is_ok() {
            tracing::info!(
                "MCP server already live at {}; this window shares it",
                socket.display()
            );
            return Ok(());
        }
        let _ = std::fs::remove_file(&socket);
        let listener =
            UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
        tracing::info!("MCP server listening on {}", socket.display());
        loop {
            let (stream, _addr) = listener.accept().await?;
            let this = self.clone();
            tokio::spawn(async move {
                if let Err(e) = this.handle_connection(stream).await {
                    tracing::warn!("MCP connection ended with error: {e:#}");
                }
            });
        }
    }

    async fn handle_connection(&self, stream: UnixStream) -> Result<()> {
        use tokio::io::AsyncReadExt;
        const MAX_LINE_BYTES: u64 = 4 * 1024 * 1024;

        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        loop {
            line.clear();
            // Cap per-line memory: a runaway client streaming an
            // unterminated line must not grow the IDE unboundedly.
            let bytes = (&mut reader)
                .take(MAX_LINE_BYTES)
                .read_line(&mut line)
                .await?;
            if bytes == 0 {
                break;
            }
            if !line.ends_with('\n') && bytes as u64 >= MAX_LINE_BYTES {
                anyhow::bail!("MCP line exceeded {MAX_LINE_BYTES} bytes; closing connection");
            }
            if line.trim().is_empty() {
                continue;
            }
            let request: Request = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("unparseable MCP request: {e}");
                    continue;
                }
            };
            let Some(id) = request.id.clone() else {
                continue; // notification — nothing requires action yet
            };
            let response = self.dispatch(&request.method, request.params, id).await;
            let mut payload = serde_json::to_vec(&response)?;
            payload.push(b'\n');
            write.write_all(&payload).await?;
        }
        Ok(())
    }

    async fn dispatch(&self, method: &str, params: Value, id: Value) -> Response {
        match method {
            "initialize" => Response::ok(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "taste-ide", "version": env!("CARGO_PKG_VERSION") },
                }),
            ),
            "ping" => Response::ok(id, json!({})),
            "tools/list" => Response::ok(id, json!({ "tools": self.tool_list() })),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default().to_string();
                let args = params["arguments"].clone();
                match self.call_tool(&name, args).await {
                    Ok(value) => Response::ok(id, tool_result(&value, false)),
                    Err(e) => {
                        Response::ok(id, tool_result(&json!({"error": format!("{e:#}")}), true))
                    }
                }
            }
            _ => Response::err(id, -32601, format!("method not found: {method}")),
        }
    }

    fn tool_list(&self) -> Vec<Value> {
        let empty = json!({ "type": "object", "properties": {} });
        vec![
            tool(
                "devcontainer_status",
                "Current devcontainer state: lifecycle phase, whether the on-disk \
                 configuration has pending (unapplied) changes, and the container id.",
                empty.clone(),
            ),
            tool(
                "devcontainer_reload",
                "Rebuild and restart the devcontainer from the current configuration. \
                 Safe with respect to the IDE: editor buffers and AI sessions are \
                 never interrupted. Returns immediately; poll devcontainer_status.",
                empty.clone(),
            ),
            tool(
                "devcontainer_resources",
                "The podman resources backing this workspace's devcontainer: \
                 container (with status), image (with size), and the config's \
                 named volumes. Read-only; stop/nuke/volume-removal are \
                 user-only UI actions (devcontainer_reload remains available).",
                empty.clone(),
            ),
            tool(
                "devcontainer_logs",
                "Tail of the devcontainer build/startup log.",
                json!({
                    "type": "object",
                    "properties": {
                        "lines": { "type": "integer", "description": "max lines (default 100)" }
                    }
                }),
            ),
            tool(
                "ide_git_status",
                "Git status as the IDE's file tree sees it: per-file state \
                 (modified/staged/untracked/conflicted) and the current branch.",
                empty.clone(),
            ),
            tool(
                "ide_open_files",
                "The files open in the IDE's editor: path, unsaved-changes \
                 flag, and which one is focused.",
                empty.clone(),
            ),
            tool(
                "ide_selection",
                "The user's current text selection in the editor (path, line \
                 range, text), if any. This is what the user is looking at \
                 right now.",
                empty.clone(),
            ),
            tool(
                "ide_open_file",
                "Show a file in the IDE's editor (optionally at a line). Use \
                 to direct the user's attention; non-destructive.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "workspace-relative or absolute path" },
                        "line": { "type": "integer", "description": "1-based line to jump to" }
                    },
                    "required": ["path"]
                }),
            ),
            tool(
                "ide_write_policy",
                "The IDE's write policy and current mode. Call this when a \
                 file write fails (e.g. read-only file system) or before \
                 editing outside the devcontainer scope: it explains what is \
                 writable, why, and how to proceed.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "optional path to check" }
                    }
                }),
            ),
            // Flatpak tools are read-only by design: build+install deploys
            // to the host, which only the user may trigger (via the IDE's
            // button). Agents can see state and logs to debug the manifest.
            tool(
                "flatpak_status",
                "State of the Flatpak packaging pipeline (idle/building/\
                 launching/succeeded/failed), the discovered manifest, and \
                 its app id. Triggering a build is user-only.",
                empty,
            ),
            tool(
                "flatpak_logs",
                "Tail of the Flatpak build/install log.",
                json!({
                    "type": "object",
                    "properties": {
                        "lines": { "type": "integer", "description": "max lines (default 100)" }
                    }
                }),
            ),
        ]
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<Value> {
        match name {
            "devcontainer_status" => {
                let state = match self.supervisor.state() {
                    SupervisorState::NoConfig => json!({"phase": "no-config"}),
                    SupervisorState::ConfigDetected => json!({"phase": "config-detected"}),
                    SupervisorState::Building => json!({"phase": "building"}),
                    SupervisorState::Starting => json!({"phase": "starting"}),
                    SupervisorState::Running { container_id } => {
                        json!({"phase": "running", "container_id": container_id})
                    }
                    SupervisorState::Failed { message } => {
                        json!({"phase": "failed", "message": message})
                    }
                    SupervisorState::Stopped => json!({"phase": "stopped"}),
                };
                let running = matches!(self.supervisor.state(), SupervisorState::Running { .. });
                Ok(json!({
                    "state": state,
                    "mode": if running { "container" } else { "safe" },
                    "pending_config_changes": self.supervisor.pending_changes(),
                    "container_name": self.supervisor.container_name(),
                }))
            }
            "devcontainer_reload" => {
                let supervisor = self.supervisor.clone();
                tokio::spawn(async move {
                    if let Err(e) = supervisor.reload().await {
                        tracing::warn!("agent-initiated reload failed: {e:#}");
                    }
                });
                Ok(json!({
                    "started": true,
                    "note": "reload running in background; poll devcontainer_status"
                }))
            }
            "devcontainer_resources" => {
                let resources: Vec<Value> = self
                    .supervisor
                    .list_resources()
                    .await
                    .into_iter()
                    .map(|r| {
                        json!({
                            "kind": format!("{:?}", r.kind).to_lowercase(),
                            "name": r.name,
                            "id": r.id,
                            "status": r.status,
                        })
                    })
                    .collect();
                Ok(json!({ "resources": resources }))
            }
            "devcontainer_logs" => {
                let n = args["lines"].as_u64().unwrap_or(100) as usize;
                Ok(json!({ "lines": self.supervisor.logs_tail(n) }))
            }
            "flatpak_status" => {
                let state = match self.packager.state() {
                    PackagerState::Idle => json!({"phase": "idle"}),
                    PackagerState::Building => json!({"phase": "building"}),
                    PackagerState::Launching => json!({"phase": "launching"}),
                    PackagerState::Succeeded => json!({"phase": "succeeded"}),
                    PackagerState::Failed { message } => {
                        json!({"phase": "failed", "message": message})
                    }
                };
                let manifest = self.packager.manifest().map(|m| {
                    json!({
                        "path": m.path.display().to_string(),
                        "app_id": m.app_id,
                    })
                });
                Ok(json!({
                    "state": state,
                    "manifest": manifest,
                    "note": "build/install/launch is user-triggered only",
                }))
            }
            "flatpak_logs" => {
                let n = args["lines"].as_u64().unwrap_or(100) as usize;
                Ok(json!({ "lines": self.packager.logs_tail(n) }))
            }
            "ide_open_files" => {
                let files: Vec<Value> = self
                    .workspace
                    .ide
                    .open_files()
                    .into_iter()
                    .map(|f| {
                        json!({
                            "path": f.path.display().to_string(),
                            "dirty": f.dirty,
                            "active": f.active,
                        })
                    })
                    .collect();
                Ok(json!({ "files": files }))
            }
            "ide_selection" => {
                let selection = self.workspace.ide.selection().map(|s| {
                    json!({
                        "path": s.path.display().to_string(),
                        "start_line": s.start_line,
                        "end_line": s.end_line,
                        "text": s.text,
                    })
                });
                Ok(json!({ "selection": selection }))
            }
            "ide_open_file" => {
                let raw = args["path"].as_str().context("path is required")?;
                let requested = PathBuf::from(raw);
                let path = if requested.is_absolute() {
                    requested
                } else {
                    self.workspace.root().join(requested)
                };
                if !path.starts_with(self.workspace.root()) || raw.contains("..") {
                    anyhow::bail!("path must be inside the workspace");
                }
                let line = args["line"].as_u64().map(|l| l as u32);
                self.workspace
                    .events
                    .publish(taste_core::Event::OpenFileRequested {
                        path: path.clone(),
                        line,
                    });
                Ok(json!({ "opened": path.display().to_string() }))
            }
            "ide_write_policy" => {
                let safe_mode = self.safe_mode();
                let root = self.workspace.root().to_path_buf();
                let writable: Vec<String> = if safe_mode {
                    taste_core::policy::safe_mode_scope(&root)
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect()
                } else {
                    vec![format!("{} (the whole workspace)", root.display())]
                };
                let path_check = args["path"].as_str().map(|raw| {
                    let requested = PathBuf::from(raw);
                    let path = if requested.is_absolute() {
                        requested
                    } else {
                        root.join(requested)
                    };
                    json!({
                        "path": path.display().to_string(),
                        "writable": taste_core::policy::write_allowed(&root, safe_mode, &path),
                    })
                });
                Ok(json!({
                    "mode": if safe_mode { "safe" } else { "container" },
                    "writable": writable,
                    "path": path_check,
                    "philosophy": "taste-ide runs all real work inside a project devcontainer. \
                        Until that container is running, the IDE is in safe mode: a recovery \
                        console whose sole purpose is getting the devcontainer working. In safe \
                        mode, writes are limited to the devcontainer setup (.devcontainer/) and \
                        workspace dotfiles (.editorconfig, .gitignore, .gitattributes); the rest \
                        of the workspace is readable context. The home directory is never \
                        writable, and remote git is fetch-only, in every mode.",
                    "conventions": "Devcontainer house style: base the image on a \
                        Containerfile in .devcontainer/; use --userns=keep-id in runArgs; \
                        named volumes for caches (never bind mounts outside the workspace); \
                        forwardPorts for services (published on localhost only, ports ≥1024); \
                        for background services prefer systemd: a systemd-capable image \
                        with overrideCommand false (add runArgs [\"--privileged\"] only for \
                        Docker/VS Code compatibility — this IDE strips it; rootless podman \
                        needs no extra privilege). Prefer socket activation: pair each \
                        foo.service with a foo.socket so services start on demand, restart \
                        cleanly, and clients never race the daemon. Keep unit files in the \
                        repo and install them in the Containerfile so the config stays \
                        portable to VS Code and GitHub Codespaces.",
                    "act_accordingly": if safe_mode {
                        "Focus on authoring or fixing the devcontainer configuration \
                         following the conventions above; use devcontainer_status and \
                         devcontainer_logs to diagnose, then devcontainer_reload to build \
                         and start it. Once it runs, the whole workspace becomes writable \
                         and your work continues uninterrupted."
                    } else {
                        "The devcontainer is running: the workspace is writable. Keep writes \
                         inside it; build and run things in the container rather than \
                         expecting host access."
                    },
                }))
            }
            "ide_git_status" => {
                let git = GitWorkspace::discover(self.workspace.root())
                    .context("workspace is not a git repository")?;
                let status = git.status()?;
                let files: Vec<Value> = status
                    .iter()
                    .map(|(path, state)| {
                        json!({ "path": path.display().to_string(), "state": format!("{state:?}") })
                    })
                    .collect();
                Ok(json!({ "branch": git.branch_name(), "files": files }))
            }
            other => anyhow::bail!("unknown tool: {other}"),
        }
    }
}

/// Bridge stdio ↔ the IDE's MCP socket. Agents get this as a normal MCP
/// stdio server (`taste-ide --mcp-bridge <socket>`), so any MCP-capable
/// agent can reach the IDE without knowing about the socket.
pub async fn stdio_bridge(socket: &Path) -> Result<()> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to {}", socket.display()))?;
    let (sock_read, mut sock_write) = stream.into_split();
    let mut stdin_lines = BufReader::new(tokio::io::stdin()).lines();
    let mut sock_lines = BufReader::new(sock_read).lines();
    let mut stdout = tokio::io::stdout();
    loop {
        tokio::select! {
            line = stdin_lines.next_line() => match line? {
                Some(l) => {
                    sock_write.write_all(l.as_bytes()).await?;
                    sock_write.write_all(b"\n").await?;
                }
                None => break,
            },
            line = sock_lines.next_line() => match line? {
                Some(l) => {
                    stdout.write_all(l.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                }
                None => break,
            },
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use taste_core::ExecContext;

    async fn start_test_server(root: &Path) -> (PathBuf, taste_core::Workspace) {
        let mut workspace = taste_core::Workspace::open(root.to_path_buf());
        workspace.exec = ExecContext::host_unsandboxed_for_tests();
        let supervisor = Supervisor::new_outside_container_for_tests(
            root.to_path_buf(),
            workspace.events.clone(),
            ExecContext::host_unsandboxed_for_tests(),
        );
        let packager = Packager::new(root.to_path_buf(), workspace.events.clone());
        let server = McpServer::new(supervisor, packager, workspace.clone());
        let socket = root.join("mcp.sock");
        let s = socket.clone();
        tokio::spawn(async move { server.serve(s).await });
        // Wait for the socket to exist.
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        (socket, workspace)
    }

    async fn roundtrip(stream: &mut UnixStream, request: Value) -> Value {
        let mut payload = serde_json::to_vec(&request).unwrap();
        payload.push(b'\n');
        stream.write_all(&payload).await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn initialize_and_list_tools() {
        let dir = tempfile::tempdir().unwrap();
        let (socket, _workspace) = start_test_server(dir.path()).await;
        let mut stream = UnixStream::connect(&socket).await.unwrap();

        let init = roundtrip(
            &mut stream,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        )
        .await;
        assert_eq!(init["result"]["serverInfo"]["name"], "taste-ide");

        let list = roundtrip(
            &mut stream,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
        )
        .await;
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"devcontainer_status"));
        assert!(names.contains(&"devcontainer_reload"));
    }

    #[tokio::test]
    async fn devcontainer_status_reports_pending_flag() {
        let dir = tempfile::tempdir().unwrap();
        let (socket, _workspace) = start_test_server(dir.path()).await;
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let response = roundtrip(
            &mut stream,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                   "params": {"name": "devcontainer_status", "arguments": {}}}),
        )
        .await;
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        let status: Value = serde_json::from_str(text).unwrap();
        assert_eq!(status["pending_config_changes"], false);
    }

    async fn call_tool(stream: &mut UnixStream, name: &str, arguments: Value) -> Value {
        let response = roundtrip(
            stream,
            json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                   "params": {"name": name, "arguments": arguments}}),
        )
        .await;
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    #[tokio::test]
    async fn write_policy_explains_safe_mode_and_checks_paths() {
        let dir = tempfile::tempdir().unwrap();
        let (socket, _workspace) = start_test_server(dir.path()).await;
        let mut stream = UnixStream::connect(&socket).await.unwrap();

        let denied = call_tool(
            &mut stream,
            "ide_write_policy",
            json!({"path": "src/main.rs"}),
        )
        .await;
        assert_eq!(denied["mode"], "safe");
        assert_eq!(denied["path"]["writable"], false);
        assert!(denied["philosophy"]
            .as_str()
            .unwrap()
            .contains("devcontainer"));
        assert!(denied["act_accordingly"]
            .as_str()
            .unwrap()
            .contains("devcontainer_reload"));

        let allowed = call_tool(
            &mut stream,
            "ide_write_policy",
            json!({"path": ".devcontainer/devcontainer.json"}),
        )
        .await;
        assert_eq!(allowed["path"]["writable"], true);
    }

    #[tokio::test]
    async fn open_file_publishes_event_and_stays_inside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let (socket, workspace) = start_test_server(dir.path()).await;
        let events = workspace.events.subscribe();
        let mut stream = UnixStream::connect(&socket).await.unwrap();

        let opened = call_tool(
            &mut stream,
            "ide_open_file",
            json!({"path": "src/main.rs", "line": 12}),
        )
        .await;
        assert!(opened["opened"].as_str().unwrap().ends_with("src/main.rs"));
        match events.recv().await.unwrap() {
            taste_core::Event::OpenFileRequested { path, line } => {
                assert!(path.ends_with("src/main.rs"));
                assert_eq!(line, Some(12));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let escape = call_tool(
            &mut stream,
            "ide_open_file",
            json!({"path": "../../etc/passwd"}),
        )
        .await;
        assert!(escape["error"]
            .as_str()
            .unwrap()
            .contains("inside the workspace"));
    }
}

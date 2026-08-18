//! A deliberately small LSP client for one purpose: `ide_references`.
//!
//! Agents sizing a refactor were grep-and-counting call sites — exactly the
//! question a language server answers exactly and cheaply. rust-analyzer
//! already ships in the devcontainer image, so this client spawns it
//! *there* (through [`taste_core::ExecContext`], i.e. `podman exec` in
//! container mode) and keeps it alive across calls: the first call pays
//! for indexing, the rest are instant. Container reloads are detected by
//! container id and respawn the server; the IDE and the agent session are
//! never involved.
//!
//! Paths are translated at the boundary: rust-analyzer speaks
//! container-side URIs (`/workspaces/<name>/…`), agents and the IDE speak
//! host paths. Nothing else in the IDE knows the server exists.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::oneshot;

/// One `workspace/symbol` hit we ran references on.
#[derive(Debug)]
pub struct Declaration {
    pub kind: String,
    pub container: Option<String>,
    pub path: PathBuf,
    /// 1-based.
    pub line: u32,
}

#[derive(Debug)]
pub struct Reference {
    pub path: PathBuf,
    /// 1-based.
    pub line: u32,
    /// 1-based character column.
    pub column: u32,
    /// The line's text, trimmed.
    pub text: String,
}

#[derive(Debug, Default)]
pub struct ReferencesResult {
    pub declarations: Vec<Declaration>,
    pub references: Vec<Reference>,
    /// Non-exact `workspace/symbol` hits, offered when the exact name
    /// missed — the agent probably misspelled or half-remembered.
    pub near_misses: Vec<String>,
    /// The reference list hit its cap; there are more sites than shown.
    pub truncated: bool,
}

/// Bounds: an IDE tool answers questions, it does not stream databases.
const MAX_DECLARATIONS: usize = 3;
const MAX_REFERENCES: usize = 200;
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// First call after a (re)start waits for indexing this long before
/// telling the agent to retry. Indexing continues regardless — so this is
/// a "how long may one tool call sit there" budget, not an indexing
/// budget, and a prompt retry beats a call that looks hung. It stays well
/// inside the server's per-call watchdog together with REQUEST_TIMEOUT.
const INDEX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// In-flight requests: id → the caller waiting on the response.
type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>;

/// The persistent server handle, owned by the MCP server.
pub struct RaServer {
    host_root: PathBuf,
    exec: taste_core::ExecContext,
    client: tokio::sync::Mutex<Option<RaClient>>,
}

impl RaServer {
    pub fn new(host_root: PathBuf, exec: taste_core::ExecContext) -> Self {
        Self {
            host_root,
            exec,
            client: tokio::sync::Mutex::new(None),
        }
    }

    /// Find references to `symbol`, workspace-wide.
    pub async fn references(&self, symbol: &str) -> Result<ReferencesResult> {
        let mut slot = self.client.lock().await;
        // Respawn when the execution context moved (container reload) or
        // the server died; both are ordinary lifecycle, not errors.
        let key = self.exec.container_id();
        let stale = match slot.as_mut() {
            Some(client) => client.container_key != key || client.is_dead(),
            None => true,
        };
        if stale {
            *slot = None; // drop (and kill) the old server first
            *slot = Some(RaClient::spawn(&self.host_root, &self.exec).await?);
        }
        let client = slot.as_mut().expect("just ensured");
        let result = client.references(symbol).await;
        if result.is_err() && client.is_dead() {
            *slot = None; // a dead server never answers twice
        }
        result
    }
}

struct RaClient {
    child: Child,
    stdin: ChildStdin,
    pending: PendingMap,
    next_id: i64,
    container_key: Option<String>,
    host_root: PathBuf,
    /// The workspace root as the server sees it (container path, or the
    /// host root when running host-side).
    remote_root: PathBuf,
    /// Indexing state, fed by `$/progress`: tokens currently active, and
    /// whether any was ever seen (a server that never reports progress
    /// should not be waited on).
    active_progress: Arc<Mutex<std::collections::HashSet<String>>>,
    saw_progress: Arc<AtomicBool>,
    progress_changed: Arc<tokio::sync::Notify>,
    indexed: bool,
    /// Server→client requests (configuration, registration) queued by the
    /// reader; answered between our own requests. rust-analyzer's are all
    /// answerable with nulls, and the volume (a handful at startup) makes
    /// a dedicated writer task overkill.
    server_requests: tokio::sync::mpsc::UnboundedReceiver<Value>,
}

impl RaClient {
    async fn spawn(host_root: &Path, exec: &taste_core::ExecContext) -> Result<Self> {
        // Safe mode has no container, and `resolve` would degrade to a bare
        // host passthrough — an agent-triggered process on the user's own
        // machine. Today that only fails because a bare host has no
        // rust-analyzer; absence is not a policy. Refuse it like any other
        // agent-brokered command (see `crate::exec::Jobs::spawn`).
        if !exec.is_container() {
            anyhow::bail!(
                "no devcontainer is running, so rust-analyzer has nowhere to run — and \
                 agent-triggered processes never fall back to the user's host. This is \
                 safe mode: author .devcontainer/, check devcontainer_logs, call \
                 devcontainer_reload, and symbol search comes back with it."
            );
        }
        let container_key = exec.container_id();
        let remote_root = exec
            .container_workdir()
            .map(PathBuf::from)
            .unwrap_or_else(|| host_root.to_path_buf());
        let spec = exec.resolve("rust-analyzer", &[], false);
        let mut child = tokio::process::Command::new(&spec.program)
            .args(&spec.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| match &container_key {
                Some(_) => format!("spawning {} (podman missing?)", spec.program),
                None => "spawning rust-analyzer on the host — it ships in the \
                         devcontainer image; start the container and retry"
                    .to_string(),
            })?;
        let stdin = child.stdin.take().context("rust-analyzer stdin")?;
        let stdout = child.stdout.take().context("rust-analyzer stdout")?;
        // Stderr feeds the app log: "rust-analyzer not found in the image"
        // arrives here, and nowhere else.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    taste_core::app_log::push("WARN", "rust-analyzer", &line);
                }
            });
        }

        let pending: PendingMap = Arc::default();
        let active_progress: Arc<Mutex<std::collections::HashSet<String>>> = Arc::default();
        let saw_progress = Arc::new(AtomicBool::new(false));
        let progress_changed = Arc::new(tokio::sync::Notify::new());

        // The reader: responses to us, requests/notifications from the
        // server. Server requests must be answered — rust-analyzer stalls
        // waiting on workspace/configuration otherwise.
        let (server_msg_tx, server_msg_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let mut client = Self {
            child,
            stdin,
            pending: pending.clone(),
            next_id: 0,
            container_key,
            host_root: host_root.to_path_buf(),
            remote_root,
            active_progress: active_progress.clone(),
            saw_progress: saw_progress.clone(),
            progress_changed: progress_changed.clone(),
            indexed: false,
            server_requests: server_msg_rx,
        };
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let message = match read_message(&mut reader).await {
                    Ok(Some(message)) => message,
                    Ok(None) | Err(_) => break,
                };
                if message.get("method").is_none() {
                    // A response to one of ours.
                    let Some(id) = message["id"].as_i64() else {
                        continue;
                    };
                    let Some(sender) = pending.lock().unwrap().remove(&id) else {
                        continue;
                    };
                    let outcome = match message.get("error") {
                        Some(error) => Err(error.to_string()),
                        None => Ok(message["result"].clone()),
                    };
                    let _ = sender.send(outcome);
                    continue;
                }
                match message["method"].as_str().unwrap_or_default() {
                    "$/progress" => {
                        let token = message["params"]["token"].to_string();
                        let kind = message["params"]["value"]["kind"].as_str().unwrap_or("");
                        let mut active = active_progress.lock().unwrap();
                        match kind {
                            "begin" => {
                                saw_progress.store(true, Ordering::Release);
                                active.insert(token);
                            }
                            "end" => {
                                active.remove(&token);
                            }
                            _ => {}
                        }
                        drop(active);
                        progress_changed.notify_waiters();
                    }
                    _ => {
                        // Server→client REQUESTS need answers; the writer
                        // side owns stdin, so hand them over.
                        if message.get("id").is_some() && server_msg_tx.send(message).is_err() {
                            break;
                        }
                    }
                }
            }
            // EOF: fail everything still waiting.
            for (_, sender) in pending.lock().unwrap().drain() {
                let _ = sender.send(Err("rust-analyzer exited".into()));
            }
        });

        // Handshake.
        let root_uri = file_uri(&client.remote_root);
        let initialize = client
            .request(
                "initialize",
                json!({
                    "processId": null,
                    "rootUri": root_uri,
                    "workspaceFolders": [{ "uri": root_uri, "name": "workspace" }],
                    "capabilities": {
                        "window": { "workDoneProgress": true },
                        "workspace": { "symbol": {} },
                        "textDocument": { "references": {} },
                    },
                }),
            )
            .await;
        initialize.map_err(|e| {
            anyhow::anyhow!(
                "rust-analyzer failed to initialize: {e} — if it is not in the \
                 devcontainer image, add it (dnf install rust-analyzer) and reload"
            )
        })?;
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    fn is_dead(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)) | Err(_))
    }

    /// Answer any queued server→client requests. rust-analyzer's are all
    /// answerable with nulls: we have no configuration to override and
    /// register no dynamic capabilities.
    async fn pump_server_requests(&mut self) -> Result<()> {
        while let Ok(message) = self.server_requests.try_recv() {
            answer_server_request(&mut self.stdin, &message).await?;
        }
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let (tx, mut rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        send_message(&mut self.stdin, &message)
            .await
            .map_err(|e| e.to_string())?;
        // While our response is pending, keep answering the server's own
        // requests — rust-analyzer must never end up waiting on us while
        // we wait on it.
        let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
        loop {
            tokio::select! {
                outcome = &mut rx => {
                    return match outcome {
                        Ok(outcome) => outcome,
                        Err(_) => Err("rust-analyzer exited".into()),
                    };
                }
                incoming = self.server_requests.recv() => {
                    match incoming {
                        Some(message) => {
                            answer_server_request(&mut self.stdin, &message)
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                        None => {
                            // Reader ended: the pending drain resolves rx;
                            // stop selecting on the closed queue (it would
                            // spin) and just wait the response out.
                            return match tokio::time::timeout_at(deadline, rx).await {
                                Ok(Ok(outcome)) => outcome,
                                Ok(Err(_)) => Err("rust-analyzer exited".into()),
                                Err(_) => {
                                    self.pending.lock().unwrap().remove(&id);
                                    Err(format!(
                                        "{method} timed out after {REQUEST_TIMEOUT:?}"
                                    ))
                                }
                            };
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    self.pending.lock().unwrap().remove(&id);
                    return Err(format!("{method} timed out after {REQUEST_TIMEOUT:?}"));
                }
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        send_message(
            &mut self.stdin,
            &json!({ "jsonrpc": "2.0", "method": method, "params": params }),
        )
        .await
    }

    /// Wait for the initial index. Progress is the signal — but the phases
    /// (fetch metadata, build scripts, index, prime caches) run one after
    /// another with quiet gaps between their tokens, so "no active token"
    /// only counts once it has held for a beat.
    async fn wait_for_index(&mut self) -> Result<()> {
        if self.indexed {
            return Ok(());
        }
        const QUIET_WINDOW: std::time::Duration = std::time::Duration::from_millis(700);
        let deadline = tokio::time::Instant::now() + INDEX_TIMEOUT;
        let mut quiet_since: Option<tokio::time::Instant> = None;
        loop {
            self.pump_server_requests().await?;
            let busy = !self.active_progress.lock().unwrap().is_empty();
            let started = self.saw_progress.load(Ordering::Acquire);
            if started && !busy {
                match quiet_since {
                    Some(since) if since.elapsed() >= QUIET_WINDOW => {
                        self.indexed = true;
                        return Ok(());
                    }
                    None => quiet_since = Some(tokio::time::Instant::now()),
                    Some(_) => {}
                }
            } else {
                quiet_since = None;
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "rust-analyzer is still indexing the workspace (waited {}s) — it \
                     keeps going in the background; retry this call shortly",
                    INDEX_TIMEOUT.as_secs()
                );
            }
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                self.progress_changed.notified(),
            )
            .await;
        }
    }

    async fn references(&mut self, symbol: &str) -> Result<ReferencesResult> {
        self.wait_for_index().await?;
        let hits = self
            .request("workspace/symbol", json!({ "query": symbol }))
            .await
            .map_err(|e| anyhow::anyhow!("workspace/symbol failed: {e}"))?;
        let hits = hits.as_array().cloned().unwrap_or_default();
        let mut exact: Vec<&Value> = hits
            .iter()
            .filter(|hit| hit["name"].as_str() == Some(symbol))
            .collect();
        let mut result = ReferencesResult::default();
        if exact.is_empty() {
            result.near_misses = hits
                .iter()
                .filter_map(|hit| hit["name"].as_str())
                .take(10)
                .map(String::from)
                .collect();
            return Ok(result);
        }
        exact.truncate(MAX_DECLARATIONS);

        for hit in exact {
            let uri = hit["location"]["uri"].as_str().unwrap_or_default();
            let Some(host_path) = self.to_host(uri) else {
                continue; // outside the workspace (a dependency): skip
            };
            let range = &hit["location"]["range"];
            let start_line = range["start"]["line"].as_u64().unwrap_or(0) as u32;
            let start_char = range["start"]["character"].as_u64().unwrap_or(0) as u32;
            // workspace/symbol ranges are not guaranteed to start ON the
            // name, and references only answers on the name. Find it in
            // the actual text.
            let text = tokio::fs::read_to_string(&host_path)
                .await
                .with_context(|| format!("reading {}", host_path.display()))?;
            let Some((line, character)) =
                find_symbol_position(&text, symbol, start_line, start_char)
            else {
                continue;
            };
            result.declarations.push(Declaration {
                kind: symbol_kind_name(hit["kind"].as_u64().unwrap_or(0)),
                container: hit["containerName"].as_str().map(String::from),
                path: host_path,
                line: line + 1,
            });
            let refs = self
                .request(
                    "textDocument/references",
                    json!({
                        "textDocument": { "uri": uri },
                        "position": { "line": line, "character": character },
                        "context": { "includeDeclaration": false },
                    }),
                )
                .await
                .map_err(|e| anyhow::anyhow!("textDocument/references failed: {e}"))?;
            let mut lines_cache: HashMap<PathBuf, Vec<String>> = HashMap::new();
            for location in refs.as_array().cloned().unwrap_or_default() {
                if result.references.len() >= MAX_REFERENCES {
                    result.truncated = true; // a cap must never read as "that's all"
                    break;
                }
                let Some(path) = self.to_host(location["uri"].as_str().unwrap_or_default()) else {
                    continue;
                };
                let line = location["range"]["start"]["line"].as_u64().unwrap_or(0) as u32;
                let column = location["range"]["start"]["character"]
                    .as_u64()
                    .unwrap_or(0) as u32;
                let text = match lines_cache.entry(path.clone()) {
                    std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        let content = tokio::fs::read_to_string(&path).await.unwrap_or_default();
                        entry.insert(content.lines().map(String::from).collect())
                    }
                }
                .get(line as usize)
                .map(|l| {
                    let trimmed = l.trim();
                    let mut clipped: String = trimmed.chars().take(160).collect();
                    if clipped.len() < trimmed.len() {
                        clipped.push('…');
                    }
                    clipped
                })
                .unwrap_or_default();
                result.references.push(Reference {
                    path,
                    line: line + 1,
                    column: column + 1,
                    text,
                });
            }
        }
        Ok(result)
    }

    /// Server-side URI → host path, when it is inside the workspace.
    fn to_host(&self, uri: &str) -> Option<PathBuf> {
        let remote = PathBuf::from(percent_decode(uri.strip_prefix("file://")?));
        if let Ok(relative) = remote.strip_prefix(&self.remote_root) {
            return Some(self.host_root.join(relative));
        }
        None
    }
}

/// Write one LSP message (Content-Length framing).
async fn send_message(stdin: &mut ChildStdin, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    stdin.write_all(header.as_bytes()).await?;
    stdin.write_all(&body).await?;
    stdin.flush().await?;
    Ok(())
}

/// Answer one server→client request. Nulls across the board: we have no
/// configuration to override and register no dynamic capabilities;
/// `workspace/configuration` wants one null per asked-for item.
async fn answer_server_request(stdin: &mut ChildStdin, message: &Value) -> Result<()> {
    let result = match message["method"].as_str().unwrap_or_default() {
        "workspace/configuration" => {
            let count = message["params"]["items"]
                .as_array()
                .map(|items| items.len())
                .unwrap_or(0);
            Value::Array(vec![Value::Null; count])
        }
        _ => Value::Null,
    };
    send_message(
        stdin,
        &json!({ "jsonrpc": "2.0", "id": message["id"].clone(), "result": result }),
    )
    .await
}

/// One LSP message off the wire (Content-Length framing). `None` on EOF.
async fn read_message<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break; // header/body separator
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = value.trim().parse().ok();
        }
    }
    let length = content_length.context("LSP message without Content-Length")?;
    anyhow::ensure!(length <= 64 * 1024 * 1024, "LSP message too large");
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}

/// `/workspaces/my project` → `file:///workspaces/my%20project`.
fn file_uri(path: &Path) -> String {
    const KEEP: &[u8] = b"-_.~/";
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || KEEP.contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&input[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Find `symbol` in `text` at or after the given LSP position; returns the
/// (0-based) line and UTF-16 character of its first character.
fn find_symbol_position(
    text: &str,
    symbol: &str,
    start_line: u32,
    start_char: u32,
) -> Option<(u32, u32)> {
    for (offset, line) in text.lines().skip(start_line as usize).take(50).enumerate() {
        let from = if offset == 0 {
            // The range's character is UTF-16; identifiers and the code
            // before them are overwhelmingly ASCII, where it equals the
            // byte offset. Clamp — to the line, and to a char boundary.
            let mut from = (start_char as usize).min(line.len());
            while from > 0 && !line.is_char_boundary(from) {
                from -= 1;
            }
            from
        } else {
            0
        };
        if let Some(byte) = line[from..].find(symbol) {
            let byte = from + byte;
            let character = line[..byte].encode_utf16().count() as u32;
            return Some((start_line + offset as u32, character));
        }
    }
    None
}

/// The LSP SymbolKind table, humanized.
fn symbol_kind_name(kind: u64) -> String {
    match kind {
        2 => "module",
        5 => "class",
        6 => "method",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        22 => "enum member",
        23 => "struct",
        26 => "type parameter",
        _ => return format!("kind {kind}"),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Safe mode has nowhere to run rust-analyzer, and "nowhere" must not
    /// quietly become "the user's host". Unlike the pipeline test below,
    /// this one spawns nothing, so it runs everywhere.
    #[tokio::test]
    async fn references_refuse_safe_mode_rather_than_spawning_on_the_host() {
        let server = RaServer::new(
            Path::new("/work/p").to_path_buf(),
            taste_core::ExecContext::host_unsandboxed_for_tests(),
        );
        let error = server
            .references("write_allowed")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("never fall back"), "{error}");
        assert!(error.contains("devcontainer_reload"), "{error}");
    }

    /// The whole pipeline against the real rust-analyzer and this very
    /// workspace: handshake, server-request answering, index wait, symbol
    /// lookup, name-positioned references, path mapping. Ignored because
    /// it needs rust-analyzer on PATH and a cold index takes a while.
    #[tokio::test]
    #[ignore = "spawns rust-analyzer over the real workspace"]
    async fn references_against_this_workspace() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .to_path_buf();
        // `for_tests(true)`: this suite runs inside the devcontainer, which
        // is where rust-analyzer lives — the container IS the environment,
        // which is exactly what the safe-mode guard checks for.
        let server = RaServer::new(root, taste_core::ExecContext::for_tests(true));
        let mut result = None;
        for _ in 0..5 {
            match server.references("write_allowed").await {
                Ok(found) => {
                    result = Some(found);
                    break;
                }
                Err(e) if e.to_string().contains("indexing") => continue,
                Err(e) => panic!("references failed: {e:#}"),
            }
        }
        let result = result.expect("indexing never settled");
        assert_eq!(result.declarations.len(), 1, "{:?}", result.declarations);
        assert_eq!(result.declarations[0].kind, "function");
        assert!(result.declarations[0]
            .path
            .ends_with("crates/taste-core/src/policy.rs"));
        // Known call sites: the MCP server consults it, per the rules.
        assert!(
            result
                .references
                .iter()
                .any(|r| r.path.ends_with("crates/taste-mcp/src/server.rs")),
            "{:#?}",
            result.references
        );
        assert!(result
            .references
            .iter()
            .all(|r| r.text.contains("write_allowed")));
    }

    #[test]
    fn file_uri_escapes_and_decodes_roundtrip() {
        let uri = file_uri(Path::new("/workspaces/my project"));
        assert_eq!(uri, "file:///workspaces/my%20project");
        assert_eq!(
            percent_decode(uri.strip_prefix("file://").unwrap()),
            "/workspaces/my project"
        );
    }

    #[test]
    fn symbol_position_lands_on_the_name_not_the_keyword() {
        let text = "mod x;\n\npub fn write_allowed(root: &Path) {}\n";
        // A full-declaration range starts at "pub"; the name is found.
        let (line, character) = find_symbol_position(text, "write_allowed", 2, 0).unwrap();
        assert_eq!((line, character), (2, 7));
    }

    #[test]
    fn symbol_position_survives_multiline_declarations() {
        let text = "#[derive(Debug)]\npub struct Thing {\n}\n";
        let (line, character) = find_symbol_position(text, "Thing", 0, 0).unwrap();
        assert_eq!((line, character), (1, 11));
    }
}

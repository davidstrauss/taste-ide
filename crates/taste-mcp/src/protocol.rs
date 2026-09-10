//! Minimal MCP wire types (JSON-RPC 2.0, 2025-06-18 protocol revision).

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Deserialize)]
pub struct Request {
    #[allow(dead_code)]
    pub jsonrpc: String,
    /// Absent for notifications.
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// What calling a tool does to the world.
///
/// This is the input to MCP's tool annotations, and annotations are how a
/// client decides what it may run without asking. **A tool with none
/// declares the worst of itself**: the spec's defaults are
/// `readOnlyHint: false` and `destructiveHint: true`, so an unannotated
/// listing reads as a possibly-destructive mutation, and a client set to
/// approve reads automatically is right to stop and ask about it. Ours
/// carried no annotations at all, which is why `issue_list` — a query
/// against a git ref — put a Yes/No card in front of the user on a chat
/// whose permission mode was Auto (David, 2026-09-08: "it keeps giving me
/// these prompts even though the agent is set to use AI review").
///
/// Stated here, once, for every tool the IDE serves, rather than beside
/// each declaration: this is the same question `orchestration::is_write`
/// answers for its own six, and the answer belongs in one place where it
/// can be read down as a list and tested for completeness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Reads and reports. Changes nothing, so a client may run it freely.
    Read,
    /// Changes something, and nothing it changes is beyond recovery: a
    /// filed issue, a reordered queue, a prompt sent to another agent.
    Write,
    /// Changes something the user could not simply undo, or runs code.
    Destructive,
}

impl Effect {
    /// The MCP annotations for this effect.
    ///
    /// `openWorldHint` is false throughout: these tools act on the
    /// workspace, the fleet and the IDE's own state, none of which is an
    /// open-ended external world. `ide_exec` is the exception and says so
    /// for itself.
    fn annotations(self, open_world: bool) -> Value {
        let (read_only, destructive, idempotent) = match self {
            // Idempotent by definition: asking twice gives the same answer
            // and costs nothing either time.
            Effect::Read => (true, false, true),
            Effect::Write => (false, false, false),
            Effect::Destructive => (false, true, false),
        };
        serde_json::json!({
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": idempotent,
            "openWorldHint": open_world,
        })
    }
}

/// What each tool does to the world — the whole list, in one place.
///
/// The fallback is [`Effect::Destructive`], which is the direction a
/// mistake has to fall: a tool nobody classified asks the user, rather
/// than being approved automatically because we forgot to say what it
/// does. `every_tool_says_what_it_does` is the test that stops the
/// fallback from ever being what actually runs.
pub fn effect(tool: &str) -> Effect {
    match tool {
        // --- reads: the environment, the workspace, the fleet ----------
        "devcontainer_status"
        | "devcontainer_resources"
        | "devcontainer_logs"
        | "flatpak_status"
        | "flatpak_logs"
        | "ide_git_status"
        | "ide_open_files"
        | "ide_selection"
        | "ide_find"
        | "ide_search"
        | "ide_semantic_search"
        | "ide_list_files"
        | "ide_references"
        | "ide_write_policy"
        | "ide_conventions"
        | "ide_environment"
        | "ide_screenshot"
        | "ide_widget_geometry"
        | "ide_app_log"
        | "ide_permission_log"
        // The output of a command already run. Reading it changes nothing;
        // starting the command was `ide_exec`'s business.
        | "ide_exec_output"
        | "issue_list"
        | "issue_attachment"
        | "issue_status"
        | "chat_status"
        | "chat_transcript_tail"
        | "review_list" => Effect::Read,

        // Moves a VIEW, not data: it brings a file to the front of the
        // user's editor and touches nothing on disk. The one judgement
        // call in this list, and it goes with reads because what
        // `readOnlyHint` protects is the user's work, and this cannot
        // reach it.
        "ide_open_file" => Effect::Read,

        // --- writes: recoverable, and the user can see all of them -----
        "issue_create" | "issue_update" | "issue_link" | "issue_reorder" => Effect::Write,
        // Served only on a clone's socket. `publish` moves that
        // environment's one branch of record in the user's checkout, and
        // is fast-forward only — forcing a divergence is refused and left
        // to the user, so there is nothing irreversible in it.
        // `update_from_main` fetches into the clone's remote-tracking
        // refs and moves nothing in the working tree.
        "publish" | "update_from_main" => Effect::Write,
        // Makes an environment: a clone and, later, a container. Heavy,
        // but additive, and `env_remove` is how it is undone.
        "issue_start" => Effect::Write,
        // Spends another agent's turn — and so the user's allowance — but
        // destroys nothing.
        "chat_send" => Effect::Write,
        // Stops a command this agent started. The output stays and a
        // re-run costs nothing, so it is not destructive; it is not a read
        // either, since something dies.
        "ide_exec_kill" => Effect::Write,

        // --- destructive ------------------------------------------------
        // Runs a command. Whatever the command does, this does.
        "ide_exec" => Effect::Destructive,
        // Tears down the container and builds it again FROM the config on
        // disk, which runs that config's lifecycle hooks. CLAUDE.md's
        // "configuration authority is execution authority" is about this
        // call, and it is the one the IDE asks the user about by name.
        "devcontainer_reload" => Effect::Destructive,

        // A tool nobody classified. Says the worst of itself, on purpose.
        _ => Effect::Destructive,
    }
}

/// The tools that must reach the USER, whatever the client's permission
/// mode says.
///
/// Claude Code's auto mode has a second model review actions instead of
/// the user, and it is the shipped default. That is the right trade for
/// almost everything here — but not for applying a devcontainer config.
/// "Configuration authority is execution authority": applying a config
/// runs its lifecycle hooks, safe mode grants the agent precisely the
/// write that authors it, and the split this project keeps is that **the
/// agent authors and the USER applies**. A classifier approving that
/// closes the split.
///
/// `_meta["anthropic/requiresUserInteraction"]` is the documented way to
/// say so: a tool marked with it prompts on every call in `acceptEdits`,
/// `auto` and `bypassPermissions` alike, is never skipped by an allow
/// rule, and is offered no "don't ask again". Saying it here is better
/// than the IDE refusing later, because the client can then never get as
/// far as thinking it had permission.
///
/// `publish` is deliberately NOT here. It is fast-forward only: a rewrite
/// the user has already seen is reported and refused rather than forced,
/// so the irreversible case CLAUDE.md pairs with a reload does not exist
/// in the tool.
fn must_ask(tool: &str) -> bool {
    matches!(tool, "devcontainer_reload")
}

/// Declarative tool description for `tools/list`, annotated so a client
/// can tell a query from a command ([`effect`]) and told when it must ask
/// the user whatever its mode ([`must_ask`]).
pub fn tool(name: &str, description: &str, schema: Value) -> Value {
    // Only `ide_exec` reaches past the workspace and the fleet: the
    // command it is given may do anything, the network included.
    let open_world = name == "ide_exec";
    let mut value = serde_json::json!({
        "name": name,
        "description": description,
        "inputSchema": schema,
        "annotations": effect(name).annotations(open_world),
    });
    if must_ask(name) {
        value["_meta"] = serde_json::json!({ "anthropic/requiresUserInteraction": true });
    }
    value
}

/// MCP tool results wrap content blocks; ours are always JSON-as-text.
pub fn tool_result(value: &Value, is_error: bool) -> Value {
    serde_json::json!({
        "content": [{ "type": "text", "text": value.to_string() }],
        "isError": is_error,
    })
}

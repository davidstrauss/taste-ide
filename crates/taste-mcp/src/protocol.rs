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
        "environment"
        | "devcontainer_status"
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
        // but additive, and `environment_destroy` is how it is undone.
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
        // Tears down the container and builds it again from the config on
        // disk, which runs that config's lifecycle hooks — in the VM, on
        // the VM's kernel, with nothing of the user's in reach. Buffers and
        // chats survive it, the agent picks its work back up, and the user
        // is told how it ended. Nothing about it is beyond recovery, so it
        // is a write and asks nobody (David, 2026-09-21: "I don't think we
        // need to gate devcontainer rebuild approval on the human anymore
        // ... If build and run are VM-sandboxed, then what's the risk?").
        // It was `Destructive` and asked by name while a config's hooks ran
        // on the host kernel; that is the gap the VM closed.
        "devcontainer_reload" => Effect::Write,
        // The two removals (i-0022). A clone can be the only copy of an
        // agent's unreviewed work and an issue is the only record of why
        // something was wanted; neither comes back. Both refuse on the
        // facts before they take anyone's word, and both put the question
        // to the user themselves on the `force` path — in the IDE, naming
        // what dies. This annotation is the whole of what they say to the
        // client, and it is what a client needs in order to decide whether
        // to run one unasked; why they carry no must-ask flag on top of it
        // is at [`must_ask`].
        "environment_destroy" | "issue_delete" => Effect::Destructive,

        // A tool nobody classified. Says the worst of itself, on purpose.
        _ => Effect::Destructive,
    }
}

/// The tools that must reach the USER, whatever the client's permission
/// mode says.
///
/// Which tools must reach the USER on every call, whatever the client's
/// permission mode — `_meta["anthropic/requiresUserInteraction"]`, the
/// documented way to say so: a tool marked with it prompts in
/// `acceptEdits`, `auto` and `bypassPermissions` alike, is never skipped
/// by an allow rule, and is offered no "don't ask again".
///
/// **Nobody, as of 2026-09-21.** `devcontainer_reload` was the one, while
/// applying a config ran its lifecycle hooks on the user's kernel —
/// "configuration authority is execution authority", and the agent
/// authored while the user applied. Every hook now runs in the VM, so
/// applying a config the agent wrote costs the user nothing they cannot
/// get back, and the split is closed on purpose (David: "Drop the
/// confirmation dialogs"). The hook stays for the next tool whose stakes
/// are the user's own.
///
/// `publish` is deliberately NOT here. It is fast-forward only: a rewrite
/// the user has already seen is reported and refused rather than forced,
/// so the irreversible case CLAUDE.md pairs with a reload does not exist
/// in the tool.
///
/// The two removals are not here either, and it is the flag's shape that
/// keeps them out rather than any doubt about their stakes. `_meta` rides
/// on the DESCRIPTOR: it is written once, at `tools/list` time, long
/// before anyone has named an environment or set `force`, so it cannot see
/// what is at stake and cannot vary from one call to the next. Marking
/// them would put a card in front of every call alike — including the
/// reclaim of an environment the user has already merged, where nothing is
/// lost and there is nothing to decide — and clearing six of those would
/// cost six cards, which is the dialog per environment the coordinator's
/// brief exists to replace, moved one layer up.
///
/// What they have instead is narrower, not weaker. Each asks the user
/// itself, through `ui_probe`'s `Confirm`, on the `force` path only, with
/// the branches and the commit counts in the body, and fails closed when
/// there is nobody to ask — so the question arrives exactly when something
/// would be lost, and says which thing. A static card can do neither. What
/// the client is told is what they are: [`effect`] classes both
/// `Destructive`, which is what it needs in order to decide whether to run
/// one unasked (i-0022).
fn must_ask(tool: &str) -> bool {
    let _ = tool;
    false
}

/// Whether this is a tool the IDE actually declares — classified in
/// [`effect`] rather than merely falling through its `Destructive`
/// fallback.
///
/// The fallback is what makes this worth asking. `effect` answers for any
/// string, and a caller that wants to know "is this one of ours" would
/// otherwise read "yes, and it is destructive" for `Bash`, for another
/// server's tools, and for a typo. The chat pane asks because a standing
/// permission answer is a judgement about what a tool DOES, and the table
/// above is that judgement for this server's tools alone; the IDE has no
/// business ruling on GitHub's.
///
/// The three destructive names are spelled out because they are the only
/// place the fallback and a real classification collide. `ide_exec` was
/// the first; `environment_destroy` and `issue_delete` joined it when the
/// coordinator's own tools landed (i-0022); `devcontainer_reload` was
/// among them until its hooks moved into the VM and it became a write.
/// Naming one here settles nothing about what may be answered for it:
/// each is ours, each is [`Effect::Destructive`], and [`may_stand`]
/// refuses a standing yes to every destructive tool, so the only thing
/// that changes is that the IDE recognises these as its own rather than
/// reading them as some other server's. A new destructive tool that
/// forgets to join them is reported as "not ours" — so it goes on asking,
/// which is the direction a mistake has to fall — and
/// `every_tool_says_what_it_does` fails until it does.
pub fn is_ide_tool(tool: &str) -> bool {
    effect(tool) != Effect::Destructive
        || matches!(tool, "ide_exec" | "environment_destroy" | "issue_delete")
}

/// Whether the IDE answers this tool's permission request itself, with a
/// yes, before any card: the tool's whole effect lands in the VM, the
/// user is told how it ended, and a card would be a question whose answer
/// is always yes (David, 2026-09-21: "I only need a notification on the
/// final success/failure of the rebuild"). Narrow on purpose: it is not
/// "every write", because a write the user might want to see coming —
/// filing an issue in their backlog — is still theirs to allow or settle.
pub fn asks_nobody(tool: &str) -> bool {
    matches!(tool, "devcontainer_reload")
}

/// Whether the IDE may remember a standing **allow** for this tool.
///
/// Two refusals, both of them the point rather than caution:
///
/// - [`must_ask`] tools are never offered a "don't ask again" — that is
///   what the `requiresUserInteraction` annotation means, and the IDE
///   saying it to the client and then keeping a standing yes of its own
///   would be the IDE going behind its own declaration.
/// - [`Effect::Destructive`] tools are refused because the tool is the
///   wrong grain for them. `ide_exec` runs whatever command it is handed,
///   so "always allow `ide_exec`" is not a permission about a tool at all;
///   it is a shell with no gate. A tool nobody classified falls here too,
///   which is the direction a mistake has to fall.
///
/// There is no matching refusal for a standing **deny**, and the asymmetry
/// has a reason: a standing no is a refusal, and refusing is never a
/// widening. The user may tell this project to stop letting agents run
/// commands; they may not tell it to stop asking.
pub fn may_stand(tool: &str) -> bool {
    !must_ask(tool) && effect(tool) != Effect::Destructive
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
///
/// Pretty-printed, not minified. A result too large for the client's
/// context is saved to a file and read back in pieces, and the piece is a
/// line: a 130,000-character answer on one line could not be read back at
/// all, which is how a small local model met the backlog (David,
/// 2026-09-16). The bytes cost a few percent; the shape costs nothing.
pub fn tool_result(value: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

//! Right pane: the AI chat interface, a view over `taste-acp`.
//!
//! The transcript is a message list, not a log: user prompts as cards,
//! agent responses rendered as markdown, thinking behind an expander,
//! tool calls as live-updating cards with inline diffs, plans as checklists
//! — the Claude Code chat experience, over ACP.
//!
//! The pane holds only channel ends. The agent subprocess and its ACP
//! connection live on the tokio runtime (host-side, sandboxed), which is why
//! nothing here — including a live streaming turn — is disturbed when the
//! devcontainer reloads. Conversations survive IDE restarts through ACP's
//! own `session/load`; the IDE persists nothing but the session id.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;
use taste_acp::session::{allow_option, first_allow_outcome, outcome_for, reject_option};
use taste_acp::{builtin_agents, AgentAim, AgentClient, SessionEvent};
use taste_core::environment::EnvironmentId;
use taste_core::Workspace;
use taste_devcontainer::EnvironmentRegistry;

use agent_client_protocol::schema::v1::{
    AuthMethod, ContentBlock, Diff, EmbeddedResource, EmbeddedResourceResource, ImageContent, Plan,
    RequestPermissionOutcome, RequestPermissionRequest, SessionConfigId, SessionConfigKind,
    SessionConfigOption, SessionConfigSelectOptions, SessionModeId, SessionModeState,
    SessionUpdate, TextContent, TextResourceContents, ToolCallContent, ToolCallStatus, ToolKind,
    Usage,
};

/// The permission mode a chat runs in unless the user has chosen another:
/// the agent decides routine tool calls itself and the IDE stays out of
/// the way. Applied to every session — fresh or restored — because "what
/// permission mode am I in" is a property of the CHAT, not of whichever
/// agent process happens to be serving it right now.
const DEFAULT_PERMISSION_MODE: &str = "auto";

/// A pasted essay should not become the transcript. Past either bound the
/// card shows a clipped preview and a button that opens the whole thing.
const PROMPT_CLIP_LINES: i32 = 12;
const PROMPT_CLIP_CHARS: usize = 600;

/// The inset every row of a prompt card gets — text, thumbnails, the
/// attachment list. Stated once: the thumbnail strip drifted to a different
/// figure on three sides and no top margin at all, which reads as the
/// picture being nailed to the text above it.
const CARD_INSET: i32 = 10;

/// Preview size for an image attachment — the same in the composer chip as
/// in the transcript card, because they are the same picture.
const ATTACHMENT_THUMBNAIL_PX: i32 = 56;

const MAX_TEXT_ATTACHMENT_BYTES: u64 = 256 * 1024;
const MAX_IMAGE_ATTACHMENT_BYTES: u64 = 5 * 1024 * 1024;
/// The keyboard contract, where a tooltip can carry it: a label under the
/// composer would be chrome the user reads once and then looks past forever.
const SEND_TOOLTIP: &str = "Send (Enter) · Shift+Enter for a new line";
/// The same contract while a turn is running. Saying "queued" up front
/// matters: the alternative reading of a live Send button mid-turn is that
/// it interrupts the answer being written.
const SEND_TOOLTIP_QUEUED: &str =
    "Queue (Enter) — sends when the current turn ends · Shift+Enter for a new line";

/// What the working line says when the turn is between tool calls — the
/// model is writing and there is genuinely nothing more specific to report.
const BUSY_IDLE: &str = "Working…";

/// How far the composer grows before it starts scrolling instead. Eight
/// lines is enough for a real paragraph of instruction; past that the
/// composer would be eating the transcript it is a reply to.
const COMPOSER_MAX_LINES: i32 = 8;

const MAX_DIFF_LINES: usize = 400;
const MAX_TRANSCRIPT_ROWS: u32 = 200;

/// Lines kept in the text mirror `chat_transcript_tail` reads (see
/// [`ChatPane::record_line`]). Matched to the widget cap on purpose: the
/// mirror is a shadow of what the tab shows, and one outliving the other
/// would be a second, disagreeing transcript.
const MAX_TRANSCRIPT_MIRROR_LINES: usize = 200;
/// ...and how much of one line survives into it. An orchestrator reading a
/// sub-agent's tail wants the shape of the conversation, not a pasted file;
/// the tab still holds the whole thing for the user.
const TRANSCRIPT_LINE_CHARS: usize = 400;

type PendingPermission = (RequestPermissionRequest, taste_acp::PermissionReply);

/// The chat column's hooks into one chat: "my restorable state changed"
/// (persist it) and "a turn is / is not in flight" (the environment's busy
/// indicator, which is where a chat the user is not looking at reports).
pub type PersistHook = Rc<dyn Fn()>;
pub type BusyHook = Rc<dyn Fn(bool)>;

/// The orchestrator's glyph, beside the chat's own name in its header.
///
/// It rode on the tab's indicator slot until there were tabs no longer.
/// Still an icon rather than a badge or a colour: it is a role marker, and
/// a quiet one — what this chat MAY do, never what it is doing.
const ORCHESTRATOR_ICON: &str = "system-users-symbolic";

/// A live tool-call card in the transcript, updated in place.
struct ToolCard {
    status_icon: gtk::Image,
    /// Shown instead of the icon while the call is actually running.
    status_spinner: gtk::Spinner,
    title_label: gtk::Label,
    /// How this call's permission was answered — hidden until it was.
    permission: gtk::Image,
    content: gtk::Box,
    /// The disclosure, so the card can be opened without a synthetic click.
    revealer: gtk::Revealer,
    arrow: gtk::Image,
    /// The header button. Insensitive, and showing no arrow, until the call
    /// has produced something worth opening the card onto.
    toggle: gtk::Button,
    /// What `content` was last built from. An ACP content update is a
    /// SNAPSHOT of the whole collection, so the card is rebuilt when this
    /// changes and left alone when it does not — which is what keeps a card
    /// from growing a second copy of its own output mid-stream, and keeps a
    /// restated snapshot from destroying the widgets under the pointer.
    signature: RefCell<Option<Vec<String>>>,
    /// The tool's category, which decides how its content is rendered:
    /// `Execute` output is terminal output and gets the terminal treatment.
    kind: Cell<ToolKind>,
}

impl ToolCard {
    fn set_expanded(&self, open: bool) {
        self.revealer.set_reveal_child(open);
        self.arrow.set_icon_name(Some(if open {
            "pan-down-symbolic"
        } else {
            "pan-end-symbolic"
        }));
    }
}

pub struct ChatPane {
    pub widget: gtk::Box,
    workspace: Workspace,
    transcript: gtk::ListBox,
    transcript_scroller: gtk::ScrolledWindow,
    /// Pinned copy of the last user prompt: overlays the transcript's top
    /// edge whenever the real card is scrolled out of view above it, so
    /// the question stays readable while the answer scrolls.
    pinned_prompt: gtk::Box,
    pinned_prompt_label: gtk::Label,
    last_prompt_row: RefCell<Option<gtk::ListBoxRow>>,
    entry: sourceview5::View,
    agent_picker: adw::ComboRow,
    /// The client-side permission policy: on = auto-approve.
    approval_picker: adw::SwitchRow,
    /// Agent-provided controls (permission mode, model, …), per session:
    /// switches for booleans, expanded radio lists for exclusive choices —
    /// every option a single click away.
    controls: gtk::Box,
    /// Sign-in methods, revealed when the agent asks for authentication.
    auth_box: gtk::Box,
    permission_bar: gtk::Revealer,
    allow_button: gtk::Button,
    deny_button: gtk::Button,
    /// The permission card's glyph, title and context line.
    permission_icon: gtk::Image,
    permission_label: gtk::Label,
    permission_subtitle: gtk::Label,
    status_label: gtk::Label,
    status_spinner: gtk::Spinner,
    busy_row: gtk::Box,
    /// What the turn is doing, when the agent has said: the tool call in
    /// flight, or `BUSY_IDLE` between them.
    busy_label: gtk::Label,
    /// The options shade: full-height session controls over the chat.
    options_panel: gtk::ScrolledWindow,
    options_toggle: gtk::ToggleButton,
    chat_tab: gtk::ToggleButton,
    /// Names the conversation: the agent, and the environment it works in.
    identity_label: gtk::Label,
    /// The orchestrator mark beside it.
    identity_glyph: gtk::Image,
    composer_area: gtk::Box,
    /// Detail under the permission label: the proposed diff, when there is one.
    permission_detail: gtk::Box,
    client: RefCell<Option<AgentClient>>,
    pending_permission: RefCell<Option<PendingPermission>>,
    /// Context queued for the next prompt (files, selections, images).
    attachments: RefCell<Vec<(String, ContentBlock)>>,
    chips: gtk::FlowBox,
    send_button: gtk::Button,
    stop_button: gtk::Button,
    /// An input method is mid-composition in the composer. Enter belongs to
    /// the IM while this is set (see `composer_key`).
    preedit: Rc<Cell<bool>>,
    /// The last prompt sent from this composer, for Up-arrow recall. One
    /// step, deliberately: a history browser is a different feature.
    last_sent: RefCell<Option<String>>,
    usage_bar: gtk::LevelBar,
    usage_tab: gtk::ToggleButton,
    usage_panel: gtk::ScrolledWindow,
    usage_list: gtk::ListBox,
    /// Tokens currently in context, as the AGENT reports them. Inferring
    /// this from the model name only ever gave the window size, never the
    /// fill.
    context_used: Cell<u64>,
    /// Latest cumulative session usage from a finished turn.
    session_usage: RefCell<Option<Usage>>,
    /// Cumulative session cost, if the agent reports one: amount, currency.
    session_cost: RefCell<Option<(f64, String)>>,
    /// Context-window size of the applied model (drives the usage bar).
    context_limit: Cell<u64>,
    /// Tail latch: true while the transcript is parked at the bottom.
    /// Shared with the adjustment handlers, which is why it is an `Rc`.
    stick_to_bottom: Rc<Cell<bool>>,
    /// "Jump to the newest" banner, revealed when content lands while the
    /// user is reading further up.
    jump_banner: gtk::Revealer,
    /// A re-pin is already queued; further growth in the same frame must
    /// not queue another.
    scroll_pending: Rc<Cell<bool>>,
    /// Permission outcomes still looking for their tool card, keyed by tool
    /// call id: the request can arrive before the update that creates the
    /// card, so the mark waits here until there is something to put it on.
    pending_marks: RefCell<HashMap<String, (String, String)>>,
    /// The permissions controls, for syncing CurrentModeUpdate.
    mode_sync: RefCell<Option<ModeControls>>,
    /// Structure of the last-built controls (option ids + value sets).
    /// Value-only echoes never rebuild — rebuilding mid-drag destroys the
    /// slider under the pointer and makes the UI jump.
    controls_signature: RefCell<Option<ControlsSignature>>,
    /// Last known mode state, so mid-session ConfigOptionUpdate rebuilds
    /// (new model lists, etc.) keep the permissions row current.
    last_modes: RefCell<Option<SessionModeState>>,
    /// Guards dropdown updates driven by the agent from re-triggering sends.
    syncing: Cell<bool>,
    /// The workspace's environments. This chat asks it for its own
    /// environment's mode at every spawn, because the mode can change
    /// between one connection and the next.
    environments: Arc<EnvironmentRegistry>,
    /// The IDE binary's own path — half of the MCP bridge command. The
    /// other half is the socket, which is per environment, so the bridge
    /// is composed at spawn time rather than handed down.
    bridge_command: String,
    /// The environment this chat's agent works in — its clone, its
    /// devcontainer, its exec target.
    ///
    /// Fixed at construction and never reassigned, because it is this
    /// chat's *identity*: one environment, one conversation. A chat that
    /// could be re-aimed at another world would be a second answer to
    /// "which conversation does this environment have", and which pane the
    /// user sees is the environment panel's decision alone.
    environment: EnvironmentId,
    // --- streaming state -------------------------------------------------
    current_agent: RefCell<Option<gtk::TextBuffer>>,
    current_agent_view: RefCell<Option<gtk::TextView>>,
    current_thought: RefCell<Option<gtk::TextBuffer>>,
    /// The open thought's expander and the moment it started, so closing it
    /// can say how long it took. A thought that still says "Thinking…" after
    /// the answer has arrived is the pane lying about what it is doing.
    current_thought_header: RefCell<Option<(gtk::Expander, std::time::Instant)>>,
    tool_cards: RefCell<HashMap<String, ToolCard>>,
    /// A user message being assembled from its chunks (replayed history,
    /// and any prompt the agent echoes back). One chunk per content block,
    /// so the card can only be drawn once they have all arrived.
    pending_user: RefCell<Option<PendingUserMessage>>,
    /// The current turn's plan card, updated in place. An ACP plan update
    /// is a SNAPSHOT of the whole checklist, not an addition to it.
    plan_card: RefCell<Option<gtk::Box>>,
    /// The checklist as last drawn (entry text + status), for the whole
    /// session. A snapshot identical to this one is a restatement, not
    /// news, and earns no space in the transcript.
    plan_snapshot: RefCell<Option<Vec<(String, String)>>>,
    // --- slash commands ----------------------------------------------------
    command_provider: crate::command_completion::CommandProvider,
    /// Live transcript row count (capped; see append_row).
    transcript_rows: Cell<u32>,
    /// (agent registry id, ACP session id) — persisted for session/load.
    session_info: RefCell<Option<(String, String)>>,
    /// "This fresh chat was forced" alert in the empty-transcript placeholder.
    restore_notice: gtk::Label,
    /// The empty-transcript page, whose title names the selected agent.
    placeholder: adw::StatusPage,
    /// The (agent, session) pair this chat is worth restoring FROM — the
    /// live one once it has content, or the one it was restored with until
    /// then. Persisted; a sterile fresh session never displaces it.
    persisted_session: RefCell<Option<(String, String)>>,
    /// A session id armed but not yet connected: a background tab restores
    /// lazily, so N tabs at startup are N tab labels, not N agent
    /// processes. Consumed by `activate`.
    pending_restore: RefCell<Option<String>>,
    /// This chat's model choice (config option value id), or None to follow
    /// the agent's default. Per chat, not per project: the tab owns its
    /// session settings.
    model_value: RefCell<Option<String>>,
    /// This chat's permission mode (an ACP session mode id), or None for
    /// [`DEFAULT_PERMISSION_MODE`]. Re-applied to every session this chat
    /// connects, so the setting survives restarts and respawns.
    permission_mode: RefCell<Option<String>>,
    /// The agent's "mode" CONFIG option, when it exposes one: some agents
    /// carry the permission mode there instead of (or as well as) in the
    /// modes state, and that is then the only channel to set it through.
    mode_config: RefCell<Option<(SessionConfigId, Vec<String>)>>,
    /// Is this the tab the user is looking at? Only the selected chat may
    /// raise window-level toasts, whose actions route to the selected pane.
    selected: Cell<bool>,
    /// This pane's identity for GNotifications: what scopes its
    /// notification ids so two chats each needing the user are two
    /// notifications, and what a notification click routes back on.
    ///
    /// Process-unique and never reused. Deliberately not the tab title
    /// (it is renamed by the agent mid-conversation) and not the session
    /// id (a chat has none until it connects, which is before it can ask
    /// for anything).
    notify_key: String,
    /// A turn is in flight. Mirrored here (as well as into the tab's
    /// spinner) so the fleet view can say which environments are working.
    busy: Cell<bool>,
    /// The owner's "this chat's restorable state changed" hook (session id,
    /// agent, model, permission mode, title): persist the tab list.
    on_persist: RefCell<Option<PersistHook>>,
    /// The owner's "a turn is / is not in flight" hook: the tab's spinner.
    on_busy: RefCell<Option<BusyHook>>,
    /// True once the current session has at least one prompt behind it.
    /// The SDK writes a conversation to disk only on the first prompt, so
    /// an unprompted session id is unloadable — persisting one would
    /// clobber the stored, restorable id with a sterile one (which is how
    /// "session/load failed" became every launch's greeting).
    session_has_content: Cell<bool>,
    /// Latched on AuthRequired; cleared by a completed turn. While set,
    /// Ready must NOT close the options shade over the sign-in buttons.
    needs_auth: Cell<bool>,
    /// Consecutive automatic reconnects since the last live session.
    /// An agent process is mortal — it dies with a devcontainer rebuild,
    /// or a crash — and the conversation is not, so the pane brings it
    /// back. Counted so a session that cannot start does not spin.
    reconnect_attempts: Cell<u32>,
    /// The mode running before an optimistic switch — restored if the
    /// agent refuses the change.
    mode_revert: RefCell<Option<SessionModeId>>,
    /// Prompts sent but not yet completed (the wire queues mid-turn sends):
    /// (restore-text, transcript card, "queued" badge). A failed or dead
    /// prompt leaves the transcript and returns to the composer.
    pending_prompts: RefCell<std::collections::VecDeque<PendingPrompt>>,
    /// One-shot capture of the next agent reply (commit-message suggestions
    /// and similar utility prompts). The exchange still renders in the
    /// transcript — the IDE never talks to the agent behind the user's back.
    capture: RefCell<Option<Capture>>,
    /// Whether the live agent process runs INSIDE this chat's environment
    /// container. Not a setting — a record of the topology the current
    /// process was spawned in, so a transition is detected by comparing it
    /// with what the environment now allows rather than by counting events.
    relocated: Cell<bool>,
    /// The last reason relocation was declined, so it is said once rather
    /// than on every reconnect.
    hosting_refusal: RefCell<Option<String>>,
    /// A topology change that arrived mid-turn and is owed a respawn once
    /// the turn ends. The alternative — moving the process immediately —
    /// would throw away the turn the user is watching.
    relocation_pending: Cell<bool>,
    // --- orchestration ----------------------------------------------------
    /// Is this the workspace's orchestrator? Persisted as
    /// [`taste_core::state::ChatRole`], and mirrored to the MCP server as
    /// "serve the orchestration tools on this chat's environment socket".
    orchestrator: Cell<bool>,
    /// The row that designates this chat as the orchestrator — and, for a
    /// chat with no environment yet, offers to make it one in the same
    /// gesture, because an unbound orchestrator would be sharing the
    /// primary's socket with every other unbound chat.
    orchestrator_row: adw::SwitchRow,
    /// The owner's "this chat wants (or gives up) the orchestrator role"
    /// hook. The strip owns the role: it is one per workspace, and only
    /// something that can see every tab can move it.
    on_role_changed: RefCell<Option<RoleHook>>,
    /// A plain-text mirror of the transcript, for `chat_transcript_tail`.
    ///
    /// The widgets are the transcript; this is a bounded shadow of them,
    /// kept because an orchestrator supervising four sub-agents needs to
    /// read what they said without a person opening four tabs. Capped in
    /// lines and forgetful at the front — and the count of what it forgot
    /// travels with it, so a truncated view never reads as a quiet agent.
    transcript_log: RefCell<std::collections::VecDeque<taste_core::orchestration::TranscriptLine>>,
    /// Lines this mirror has dropped off the front.
    transcript_dropped: Cell<u64>,
    /// When anything last happened here. `chat_status` reports the gap;
    /// an orchestrator's main question about a sub-agent is "is it stuck".
    last_activity: Cell<Option<std::time::Instant>>,
    /// Turns completed in this session.
    turns: Cell<u64>,
    /// The model config option this session advertises: its id, and its
    /// (value id, label) pairs. Recorded at Ready so a `chat_create` asking
    /// for a model can be refused with the ids that actually exist rather
    /// than with a shrug.
    advertised_models: RefCell<Option<ModelOptions>>,
    /// One-shot: run this the next time the session reaches Ready. How
    /// `chat_create` waits for the sub-chat to come up before it validates
    /// a model and seeds the task.
    on_ready_once: RefCell<Option<ReadyHook>>,
}

/// How a pane tells its strip that the orchestrator role moved.
pub type RoleHook = Rc<dyn Fn(bool)>;

/// A session's model choice: the config option's id, and its (value id,
/// label) pairs as the agent advertised them.
type ModelOptions = (SessionConfigId, Vec<(String, String)>);

/// Something owed the next Ready — `chat_create`'s "the sub-chat is up,
/// check its model and give it the task".
pub type ReadyHook = Box<dyn FnOnce(Rc<ChatPane>)>;

type Capture = (String, Box<dyn FnOnce(String)>);

/// A user message arriving as chunks: its text, and the blocks that are not
/// text (images above all — a restored conversation used to lose them,
/// because only text chunks were rendered).
#[derive(Default)]
struct PendingUserMessage {
    text: String,
    attachments: Vec<(String, ContentBlock)>,
}

/// A prompt the agent has not finished with yet: restore text, the prompt's
/// card, and — for a prompt that had to wait behind a running turn — its
/// queued badge and the moment it started waiting.
struct PendingPrompt {
    /// The text to hand back to the composer if the prompt is rejected.
    restore: Option<String>,
    card: gtk::Box,
    /// The "queued" badge and when it started waiting — absent when nothing
    /// was running to wait behind.
    queued: Option<(gtk::Label, std::time::Instant)>,
    /// Once the card has been moved to where the agent accepted it, the
    /// marker left behind at the point it was typed.
    origin: Option<gtk::ListBoxRow>,
}
type ControlsSignature = Vec<(String, Vec<String>)>;

/// The agent's permission modes as a plain dropdown (the mode names are
/// self-descriptive; the description rides in the tooltip), plus the id
/// list its indices map onto.
struct ModeControls {
    dropdown: gtk::DropDown,
    ids: Vec<SessionModeId>,
    auto_id: Option<SessionModeId>,
}

/// Within a pixel of the end. The slack matters: fractional page sizes mean
/// an exact equality test reads as "not at the bottom" and tailing stops.
fn at_bottom(adjustment: &gtk::Adjustment) -> bool {
    adjustment.value() + adjustment.page_size() >= adjustment.upper() - 1.0
}

/// What a change in the transcript's extent means for the tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailAction {
    /// Follow the new bottom.
    Repin,
    /// Content landed below the fold while the user reads further up: offer
    /// the jump rather than taking the view off them.
    Announce,
    Nothing,
}

/// The whole scroll-anchoring rule, in one place.
///
/// `sticking` is the latch — true exactly while the view is parked at the
/// bottom, which a `value-changed` handler keeps current. `grew` says the
/// content got taller, as opposed to the viewport getting shorter; only the
/// former is news, or the banner would fire every time the composer grew a
/// line under it.
fn tail_action(sticking: bool, grew: bool) -> TailAction {
    match (sticking, grew) {
        (true, _) => TailAction::Repin,
        (false, true) => TailAction::Announce,
        (false, false) => TailAction::Nothing,
    }
}

/// Send is live only when there is something to send. Whitespace is not
/// something to send; an attachment with no prose is.
fn send_ready(text: &str, attachments: usize) -> bool {
    !text.trim().is_empty() || attachments > 0
}

/// What the composer does with a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerKey {
    Send,
    /// Refuse the permission card that is up (Escape, while one is).
    DenyPermission,
    /// Cancel the turn in flight (Escape, while something is running).
    Stop,
    /// Put the last prompt back for editing (Up, in an empty composer).
    RecallLast,
    /// Not ours: let the TextView have it.
    Insert,
}

/// The state a key press is judged against.
#[derive(Debug, Clone, Copy)]
struct ComposerState {
    /// An input method has an uncommitted composition on screen.
    preedit: bool,
    /// A turn is in flight, so there is something for Escape to stop.
    streaming: bool,
    /// Nothing typed (whitespace does not count).
    empty: bool,
    /// A permission card is up, waiting to be answered.
    awaiting_permission: bool,
}

/// Decide what a key press means, away from any widget — the part worth
/// testing, since two of these three rules are invisible until they are
/// wrong in front of somebody.
///
/// The preedit rule is the subtle one. While an input method is composing —
/// every CJK user, every time they type — Enter COMMITS the composition; it
/// does not end the sentence. Sending there truncates the message mid-word
/// and there is no way to get it back. So during preedit the composer
/// claims no key at all, and the IM gets everything.
fn composer_key(
    key: gtk::gdk::Key,
    modifier: gtk::gdk::ModifierType,
    state: ComposerState,
) -> ComposerKey {
    if state.preedit {
        return ComposerKey::Insert;
    }
    let plain = !modifier.intersects(
        gtk::gdk::ModifierType::SHIFT_MASK
            | gtk::gdk::ModifierType::CONTROL_MASK
            | gtk::gdk::ModifierType::ALT_MASK,
    );
    match key {
        gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter
            if !modifier.contains(gtk::gdk::ModifierType::SHIFT_MASK) =>
        {
            ComposerKey::Send
        }
        // A question on screen owns Escape before the turn behind it does:
        // dismissing a permission card is refusing it, which is the answer
        // that is always safe to give by reflex. Enter is deliberately NOT
        // its counterpart — nothing approves without the user putting focus
        // on the button and meaning it.
        gtk::gdk::Key::Escape if state.awaiting_permission => ComposerKey::DenyPermission,
        // Escape matches the Stop button's semantics exactly, and does
        // nothing at all when there is nothing running — an Escape that
        // cleared the composer would throw away typing nobody asked it to.
        gtk::gdk::Key::Escape if state.streaming => ComposerKey::Stop,
        // One step back, not a history browser: the overwhelmingly common
        // want is "that prompt, but fix the typo". In a composer with text
        // in it Up is a cursor key and stays one.
        gtk::gdk::Key::Up if state.empty && plain => ComposerKey::RecallLast,
        _ => ComposerKey::Insert,
    }
}

/// This session's model option: its id and its (value id, label) pairs.
///
/// The same predicate `build_controls` uses to decide which select is the
/// model picker, in one place — the list an orchestrator's `chat_create`
/// validates a `model` against must be the list the pane would render, or
/// a model the user can pick from a slider becomes one the tool calls
/// unknown.
fn model_choices(options: &[SessionConfigOption]) -> Option<ModelOptions> {
    let option = options
        .iter()
        .find(|option| option.id.to_string().eq_ignore_ascii_case("model"))?;
    let SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    let choices: Vec<(String, String)> = match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options
            .iter()
            .map(|o| (o.value.to_string(), o.name.clone()))
            .collect(),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|g| &g.options)
            .map(|o| (o.value.to_string(), o.name.clone()))
            .collect(),
        _ => Vec::new(),
    };
    (!choices.is_empty()).then(|| (option.id.clone(), choices))
}

/// Model quality order, worst → best ("default" = the recommended best).
fn model_rank(base: &str) -> usize {
    match base {
        "haiku" => 0,
        "sonnet" => 1,
        "opus" => 2,
        "default" => 3,
        _ => 50,
    }
}

/// One stop on the model slider: a base model plus its optional
/// expanded-context variant (the `x[1m]` convention).
struct ModelStop {
    name: String,
    normal: Option<String>,
    expanded: Option<String>,
}

impl ChatPane {
    /// One environment's chat pane. The environment is handed in and never
    /// changes — see [`ChatPane::environment`].
    pub fn new(
        workspace: Workspace,
        environments: Arc<EnvironmentRegistry>,
        bridge_command: String,
        environment: EnvironmentId,
    ) -> Rc<Self> {
        // Session controls are all AdwComboRows in boxed lists — labeled,
        // ellipsizing, native. The static group (agent, approvals) never
        // rebuilds; the agent-provided group below does, per session.
        let agents = builtin_agents();
        let names: Vec<String> = agents.iter().map(|a| a.display_name.clone()).collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let agent_picker = adw::ComboRow::builder()
            .title("Agent")
            .model(&gtk::StringList::new(&name_refs))
            .build();
        let approval_picker = adw::SwitchRow::builder()
            .title("Auto-approve")
            .subtitle("Approve agent permission requests without asking")
            .build();
        let session_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .margin_top(12)
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(6)
            .build();
        let new_session_row = adw::ButtonRow::builder()
            .title("New Session")
            .start_icon_name("view-refresh-symbolic")
            .build();
        // The designation. A switch, because the role is a state one chat
        // is in and any chat can be moved into — and one per workspace, so
        // turning it on here turns it off wherever it was.
        //
        // The primary's chat cannot hold it, and the switch says so rather
        // than failing later: orchestration tools are served on an
        // environment's MCP socket, and the primary's is the hub. There is
        // nothing to create in the same gesture any more — a chat lives in
        // the environment it was opened in, and the way to another world is
        // the environment panel's own New Environment.
        let orchestrator_row = adw::SwitchRow::builder()
            .title("Orchestrator")
            .subtitle("This chat can create and drive other chats")
            .build();
        orchestrator_row.set_tooltip_text(Some(
            "Give this chat the orchestration tools: list environments, create chats \
             with tasks of their own, prompt them and read what they said. One chat per \
             workspace has them.",
        ));
        if environment.is_primary() {
            orchestrator_row.set_sensitive(false);
            orchestrator_row.set_subtitle(
                "Only an agent environment's chat can orchestrate — \
                 the tools ride on its own MCP socket",
            );
        }
        session_list.append(&agent_picker);
        session_list.append(&approval_picker);
        session_list.append(&orchestrator_row);
        session_list.append(&new_session_row);

        // One status line, updated in place — connection plumbing never
        // accumulates in the transcript.
        let status_label = gtk::Label::builder()
            .xalign(0.0)
            // Centred against the tabs. It carried a 4px bottom margin and
            // nothing on top, which pushed its whole box up and left the
            // text riding above the tab row it sits beside.
            .valign(gtk::Align::Center)
            .css_classes(["dim-label", "caption"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .margin_start(8)
            .visible(false)
            .build();

        let controls = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(6)
            .build();

        let auth_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        auth_box.set_margin_start(12);
        auth_box.set_margin_end(12);
        auth_box.set_visible(false);

        // The transcript: a list of message cards. When empty, a native
        // placeholder invites the first prompt instead of blank space.
        let transcript = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["background"])
            .build();
        // Under the placeholder's logo when a restore fell through: the
        // fresh chat was forced, not chosen. A transcript row would hide
        // the placeholder and strand tiny text in empty space; this keeps
        // the normal new-conversation view.
        let restore_notice = gtk::Label::builder()
            .label("Couldn't restore the previous conversation — this is a fresh chat")
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["warning"])
            .visible(false)
            .build();
        let placeholder;
        {
            // Says what this chat can reach, which is the one thing a new
            // chat's blank page should answer. No exclamation, no invented
            // personality, no list of suggested prompts — the composer is
            // right below and the shortcuts are already on screen.
            // The title is filled in from the agent actually selected —
            // it said "Ask Claude Code" over a Gemini session.
            placeholder = adw::StatusPage::builder()
                .icon_name("chat-message-new-symbolic")
                .description("It can read and edit this project, and run commands in it.")
                .css_classes(["compact"])
                .build();
            // One shortcut per line, keys aligned against effects, with
            // real keycaps (ShortcutLabel renders native key symbols).
            let keys = gtk::Grid::builder()
                .row_spacing(8)
                .column_spacing(12)
                .halign(gtk::Align::Center)
                .build();
            for (row, accel, effect) in [(0, "Return", "send"), (1, "<Shift>Return", "new line")] {
                let shortcut = gtk::ShortcutLabel::new(accel);
                shortcut.set_halign(gtk::Align::End);
                keys.attach(&shortcut, 0, row, 1, 1);
                let label = gtk::Label::builder()
                    .label(effect)
                    .xalign(0.0)
                    .css_classes(["dim-label"])
                    .build();
                keys.attach(&label, 1, row, 1, 1);
            }
            let body = gtk::Box::new(gtk::Orientation::Vertical, 16);
            body.append(&restore_notice);
            body.append(&keys);
            placeholder.set_child(Some(&body));
            transcript.set_placeholder(Some(&placeholder));
        }
        let transcript_scroller = gtk::ScrolledWindow::builder()
            .child(&transcript)
            .name("transcript")
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            // Measured: an empty TextView's natural height is 14px (the
            // margins alone); without vexpand it sat top-pinned inside the
            // 34px field. Fill the field instead.
            .vexpand(true)
            .build();

        // Permission requests surface inline, above the entry: a card in the
        // shape libadwaita gives every other "here is a thing, decide about
        // it" surface — a glyph that types the ask, the question in heading
        // type, one dim line of context under it, then the specifics, then
        // the buttons. The old bar was the question set in bold and two
        // buttons crammed under it, which read as a developer's dialog in the
        // one pane held to the highest bar.
        let permission_icon = gtk::Image::builder()
            .icon_name("dialog-question-symbolic")
            .pixel_size(20)
            // Top-aligned against a title that may wrap to three lines: a
            // centred glyph beside a two-line question floats in the gap.
            .valign(gtk::Align::Start)
            .css_classes(["permission-icon"])
            .build();
        let permission_label = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .xalign(0.0)
            .lines(3)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build();
        let permission_subtitle = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption", "dim-label"])
            .build();
        let permission_text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        permission_text.set_hexpand(true);
        permission_text.append(&permission_label);
        permission_text.append(&permission_subtitle);
        let permission_header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        permission_header.append(&permission_icon);
        permission_header.append(&permission_text);
        // The specifics: the command, the proposed diff, whatever text the
        // request carries. Empty for an ask whose title already says it all,
        // and an empty box takes no space, so the card closes up around it.
        // Indented to the title's column — the glyph gets a gutter of its
        // own, and everything that is words lines up down one edge.
        let permission_detail = gtk::Box::new(gtk::Orientation::Vertical, 8);
        permission_detail.set_margin_start(32);
        // `pill-action`: the composer region's radius scale (main.rs) —
        // a card's answers are actions, and actions here are pills.
        let allow = gtk::Button::builder()
            .label("Allow")
            .css_classes(["suggested-action", "pill-action"])
            .build();
        let deny = gtk::Button::builder()
            .label("Deny")
            .css_classes(["pill-action"])
            .build();
        // GNOME's order: the affirmative is rightmost, and neither button is
        // the one the keyboard lands on by accident — approving is a
        // deliberate act, so nothing here takes focus when the card appears.
        // Escape denies (see `composer_key`), which is the direction that is
        // safe to reach for.
        let permission_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        permission_buttons.set_halign(gtk::Align::End);
        permission_buttons.append(&deny);
        permission_buttons.append(&allow);
        // A group, not a stack of loose labels: a screen reader announces the
        // card as one thing, and the question is its name (set per request).
        let permission_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .accessible_role(gtk::AccessibleRole::Group)
            .build();
        permission_box.set_margin_top(6);
        permission_box.set_margin_bottom(6);
        permission_box.set_margin_start(6);
        permission_box.set_margin_end(6);
        permission_box.add_css_class("card");
        permission_box.add_css_class("permission-card");
        permission_box.set_widget_name("permission-bar");
        permission_box.append(&permission_header);
        permission_box.append(&permission_detail);
        permission_box.append(&permission_buttons);
        // Slide, don't blink: the card pushes the composer down, and a
        // question that appears instantly under a moving cursor is how a
        // click lands on a button nobody read.
        let permission_bar = gtk::Revealer::builder()
            .child(&permission_box)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .transition_duration(200)
            .build();

        // The composer: ONE bordered card (Claude Code's shape, Adwaita's
        // skin) — chips on top, the text line, then a toolbar row inside
        // the card: attach + usage on the left, stop/send on the right.
        // A GtkSourceView, not a plain TextView — it subclasses TextView, so
        // every property below is unchanged, and it brings GtkSourceCompletion
        // with it. That framework owns the slash-command popup: navigation,
        // filtering, scrolling, sizing and cursor-relative placement, none of
        // which is ours to hand-roll.
        let entry = sourceview5::View::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            // One inset, all four sides, stated here rather than split
            // between a margin and the container's CSS padding — text sat
            // 12px from the left (4px padding + 8px margin) and 7px from
            // the top, which reads as crooked however carefully each half
            // was chosen.
            .top_margin(12)
            .bottom_margin(12)
            .left_margin(12)
            .right_margin(12)
            // Leading between the wrapped lines of one paragraph: without
            // it a prompt that wraps reads as a solid block. Applies only
            // when a paragraph actually wraps, so the measured single-line
            // height above is untouched.
            .pixels_inside_wrap(3)
            .build();
        // The composer is prose, not code. A GtkSourceView arrives wearing
        // GSV's default style scheme — a LIGHT one, black text, deaf to the
        // dark preference — which painted black-on-grey the moment the
        // composer switched to sourceview for completion. No scheme means
        // ordinary theme colors, and the .prompt-entry wash stays visible
        // through the view's transparent background.
        match entry.buffer().downcast::<sourceview5::Buffer>() {
            Ok(buffer) => {
                sourceview5::prelude::BufferExt::set_style_scheme(
                    &buffer,
                    None::<&sourceview5::StyleScheme>,
                );
            }
            // A silent skip here is how the black-on-grey composer shipped.
            Err(buffer) => tracing::warn!(
                "composer buffer is {}, not a GtkSourceBuffer — style scheme not cleared",
                buffer.type_()
            ),
        }
        // An expandable multiline input: entry-styled (the same class the
        // commit box wears), one line tall until content grows it — the
        // External scrollbar policy is what prevents pre-multiline sizing.
        // A TextView carries this as a tag rather than a property, and the tag
        // has to cover text that does not exist yet — so it is re-applied over
        // the whole buffer on every change. Cheap: the composer is capped at
        // 120px of text, and applying a tag does not itself emit `changed`.
        {
            let unhyphenated = gtk::TextTag::builder().insert_hyphens(false).build();
            entry.buffer().tag_table().add(&unhyphenated);
            entry.buffer().connect_changed(move |buffer| {
                let (start, end) = buffer.bounds();
                buffer.apply_tag(&unhyphenated, &start, &end);
            });
        }

        let entry_inner_scroller = gtk::ScrolledWindow::builder()
            .child(&entry)
            // Probe-measured: the default (automatic) vscrollbar policy
            // pre-sizes an EMPTY scroller to 58px — multiline before any
            // input. External policy measures exactly the search box's 34;
            // past the height cap the TextView still scrolls to its cursor.
            .vscrollbar_policy(gtk::PolicyType::External)
            // Natural height: exactly one line when empty, growing upward
            // (to the cap) as the text does.
            .min_content_height(0)
            .max_content_height(120)
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .build();

        // Starts out with nothing to send, so it starts insensitive and
        // WITHOUT the accent class — a disabled suggested-action button is
        // still a faded blue, and "nothing to send" should read as plain
        // grey. `sync_send` owns both from here on.
        // `pill-action` here and on the two beside it: the row is three
        // actions, and actions in this region are pills (the scale is
        // stated in main.rs).
        let send = gtk::Button::builder()
            .label("Send")
            .tooltip_text(SEND_TOOLTIP)
            .css_classes(["pill-action"])
            .sensitive(false)
            .build();
        // Small and quiet, like the attach button beside it: Stop and
        // Send are both live at once, and the row should read as one
        // affordance (Send) with two small controls, not three buttons
        // fighting for width. Round, not square — see `send`.
        let stop_button = gtk::Button::builder()
            .icon_name("media-playback-stop-symbolic")
            .tooltip_text("Stop this turn")
            .css_classes(["pill-action"])
            .visible(false)
            .build();
        let attach_menu = gtk::gio::Menu::new();
        attach_menu.append(Some("Current Selection"), Some("chat.attach-selection"));
        attach_menu.append(Some("Active File"), Some("chat.attach-active"));
        attach_menu.append(Some("File…"), Some("chat.attach-file"));
        attach_menu.append(Some("Image…"), Some("chat.attach-image"));
        // A round + button: the menu names its contents; Send gets the
        // rest of the row.
        let attach_button = gtk::MenuButton::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add context to the next prompt (images can also be pasted)")
            .css_classes(["pill-action"])
            .menu_model(&attach_menu)
            .build();

        let chips = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(4)
            .margin_start(6)
            .margin_end(6)
            .margin_top(6)
            .visible(false)
            .build();

        // Context-window fill, graphically; the numbers live in the
        // tooltip. Standard LevelBar offsets recolor it as it fills.
        let usage_bar = gtk::LevelBar::builder()
            .min_value(0.0)
            .max_value(1.0)
            .width_request(90)
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        usage_bar.add_offset_value("low", 0.6);
        usage_bar.add_offset_value("high", 0.85);
        usage_bar.add_offset_value("full", 1.0);
        let usage_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        usage_box.append(&usage_bar);

        // Plain and honest: a multiline box, then two buttons below —
        // Context (~20%) and Send (~80%, swapping to Stop while working).
        let field = gtk::Box::new(gtk::Orientation::Vertical, 0);
        field.add_css_class("prompt-entry");
        // Probe names: "chat.composer" / "chat.composer-entry" targets for
        // the agents' ide_screenshot / ide_widget_geometry tools.
        field.set_widget_name("composer");
        entry.set_widget_name("composer-entry");
        field.append(&entry_inner_scroller);
        // Probe matrix verdict: NO scrollbar policy measures both states
        // (External never grows for wrapped text; Automatic/Always pin
        // 58px even empty), so the scroller's height is ours to drive.
        //
        // Measuring the TextView to get it was the mistake. GtkTextView does
        // not do height-for-width: `measure` reports the layout it last
        // ALLOCATED, so the moment a line wraps it answers with the old
        // height, the box stays a line short, and following the cursor
        // slides the top line half out of view — until enough further typing
        // forces a reallocation and it snaps back. Re-measuring on an idle
        // only narrowed the window, because GtkTextView validates lines at
        // its own priority.
        //
        // The adjustment already knows: `upper` is the laid-out content,
        // `page_size` is what fits, and it announces every change. So ask it
        // how much does not fit and add exactly that. Self-correcting in
        // both directions, and it never has to assume whether `upper`
        // counts the view's margins.
        {
            let measured_entry = entry.clone();
            let scroller = entry_inner_scroller.clone();
            let adjustment = entry_inner_scroller.vadjustment();
            let queued = std::rc::Rc::new(Cell::new(false));
            let fit = std::rc::Rc::new(move |adjustment: &gtk::Adjustment| {
                let visible = adjustment.page_size();
                if visible <= 0.0 {
                    return; // not allocated yet
                }
                let metrics = measured_entry.pango_context().metrics(None, None);
                // Pango's LINE HEIGHT, not ascent + descent: the difference
                // is the font's line gap, and being short by it left the
                // scroller a pixel or two under a real single line. The view
                // then scrolls to keep the cursor visible, and the first
                // thing a scroll-to-cursor hides is the top margin — so the
                // top inset rendered smaller than the left one however
                // carefully both were set to the same number.
                let line = match metrics.height() {
                    h if h > 0 => h / gtk::pango::SCALE,
                    _ => (metrics.ascent() + metrics.descent()) / gtk::pango::SCALE,
                };
                let floor = line + 24; // the view's top and bottom margins
                                       // The ceiling is stated in LINES and resolved against the
                                       // font actually in use, rather than as a pixel count that
                                       // means five lines at one text size and three at another.
                let ceiling = line * COMPOSER_MAX_LINES + 24;
                if scroller.max_content_height() != ceiling {
                    scroller.set_max_content_height(ceiling);
                }
                let overflow = (adjustment.upper() - visible).ceil() as i32;
                if overflow == 0 {
                    return;
                }
                let current = scroller.min_content_height();
                let target = (current + overflow).clamp(floor, ceiling);
                if target != current {
                    scroller.set_min_content_height(target);
                }
            });
            adjustment.connect_changed(move |adjustment| {
                // Deferred: this fires from size-allocate, and resizing the
                // scroller inside the layout pass that is still running is
                // how the old code ended up a frame behind.
                if queued.replace(true) {
                    return;
                }
                let adjustment = adjustment.clone();
                let queued = queued.clone();
                let fit = fit.clone();
                glib::idle_add_local_once(move || {
                    queued.set(false);
                    fit(&adjustment);
                });
            });
        }
        attach_button.set_hexpand(false);
        // As wide as the row is tall, so the pill radius resolves to a
        // circle rather than a lozenge.
        attach_button.set_size_request(34, -1);
        send.set_hexpand(true);
        stop_button.set_hexpand(false);
        stop_button.set_size_request(34, -1);
        let button_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        button_row.append(&attach_button);
        button_row.append(&stop_button);
        button_row.append(&send);
        let composer_row = gtk::Box::new(gtk::Orientation::Vertical, 6);
        composer_row.append(&field);
        composer_row.append(&button_row);

        let entry_scroller = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(8)
            .build();
        entry_scroller.append(&chips);
        entry_scroller.append(&composer_row);

        // Slash-command completion popover, anchored to the composer.
        let command_provider = crate::command_completion::CommandProvider::default();
        sourceview5::prelude::ViewExt::completion(&entry).add_provider(&command_provider);

        let entry_row = entry_scroller.clone();

        // The working line. It says what the turn is actually doing when the
        // agent has told us — "Working…" over a running `cargo test` is a
        // spinner pretending to be information — and it goes away entirely
        // while a permission card is up, because nothing is working then:
        // the turn is stopped, waiting on the person reading it.
        let busy_spinner = gtk::Spinner::new();
        busy_spinner.start();
        let busy_label = gtk::Label::builder()
            .label(BUSY_IDLE)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        let busy_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        busy_row.set_margin_start(12);
        busy_row.set_margin_end(12);
        busy_row.set_margin_top(6);
        busy_row.set_margin_bottom(4);
        busy_row.append(&busy_spinner);
        busy_row.append(&busy_label);
        busy_row.set_visible(false);

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.set_width_request(320);
        // Session options live in a shade that takes the whole vertical
        // area when open — the transcript never competes with them.
        // Chat | Settings as real tabs (a grouped toggle pair); the
        // transcript stays allocated underneath (overlay, not a stack —
        // hidden stack pages mis-measure ListBox rows).
        let chat_tab = gtk::ToggleButton::builder()
            .icon_name("taste-chat-symbolic")
            .tooltip_text("Chat")
            .css_classes(["flat"])
            .active(true)
            .build();
        let options_toggle = gtk::ToggleButton::builder()
            .icon_name("emblem-system-symbolic")
            .tooltip_text("Settings")
            .css_classes(["flat"])
            .build();
        options_toggle.set_group(Some(&chat_tab));
        // Utilization: how close this session is to running out of room.
        // The icon is tinted by how bad it is, so the answer is legible
        // without opening the tab.
        // A meter, drawn in-tree: the stock system-monitor symbolic is a
        // rounded rectangle with a wave in it, and beside
        // `taste-chat-symbolic` it read as a second, dimmer speech bubble.
        // Ascending bars say "level" at a glance, which is the whole
        // question this tab answers.
        let usage_tab = gtk::ToggleButton::builder()
            .icon_name("taste-utilization-symbolic")
            .tooltip_text("Utilization")
            .css_classes(["flat"])
            .build();
        usage_tab.set_group(Some(&chat_tab));
        let tab_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["linked"])
            .build();
        tab_box.append(&chat_tab);
        tab_box.append(&usage_tab);
        tab_box.append(&options_toggle);
        let top_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        top_bar.set_margin_start(6);
        top_bar.set_margin_end(6);
        top_bar.set_margin_top(4);
        top_bar.set_margin_bottom(4);
        // Connection progress: a fixed-width prefix of the status text,
        // right of the tabs. Always allocated (a stopped spinner draws
        // nothing), so neither tabs nor status shift when it runs.
        let identity_label = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .max_width_chars(18)
            .build();
        // The orchestrator's mark, in the slot the tab's indicator used to
        // hold: a role marker beside the name it qualifies, quiet on
        // purpose — it says what this chat may do, not what it is doing.
        let identity_glyph = gtk::Image::builder()
            .icon_name(ORCHESTRATOR_ICON)
            .css_classes(["dim-label"])
            .pixel_size(12)
            .visible(false)
            .tooltip_text("Orchestrator — this chat can create and drive other chats")
            .build();
        let status_spinner = gtk::Spinner::new();
        status_spinner.set_size_request(16, 16);
        // Centred for the same reason, and so the size request is the size
        // it actually gets rather than being stretched to the row.
        status_spinner.set_valign(gtk::Align::Center);
        status_label.set_visible(false);
        top_bar.append(&tab_box);
        top_bar.append(&status_spinner);
        status_label.set_hexpand(true);
        top_bar.append(&status_label);
        // Whose conversation this is. It used to be the tab's title, and
        // when the tab strip went away the fact had nowhere to live: the
        // panel says which environment the panes are aimed at, but the
        // chat still has to say that IT is that environment's — a
        // transcript with no name on it could be anyone's. Quiet, and at
        // the end of the row, because it is an identity rather than a
        // status.
        top_bar.append(&identity_glyph);
        top_bar.append(&identity_label);
        top_bar.append(&usage_box);

        let controls_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        controls_column.append(&session_list);
        controls_column.append(&controls);
        controls_column.append(&auth_box);
        let controls_scroller = gtk::ScrolledWindow::builder()
            .child(&controls_column)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();

        // An OVERLAY, deliberately not a Stack: a hidden Stack page gets
        // measured at zero width, and GtkListBox rows cache that bogus
        // height — transcript rows born while the shade was open came back
        // hundreds of pixels tall. Under an overlay the transcript stays
        // allocated at real width the whole time.
        controls_scroller.add_css_class("background");
        controls_scroller.set_visible(false);
        // The pinned prompt: a clamped copy of the last user card, floating
        // at the transcript's top edge while the real card is scrolled off
        // above. Clicking it jumps back to the card. Under the options
        // shade so Settings still covers everything.
        let pinned_prompt_label = gtk::Label::builder()
            .attributes(&no_hyphens())
            .wrap(true)
            .lines(3)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .xalign(0.0)
            .hexpand(true)
            .margin_top(10)
            .margin_bottom(10)
            .margin_start(10)
            .margin_end(10)
            .build();
        let pinned_prompt = gtk::Box::new(gtk::Orientation::Vertical, 0);
        pinned_prompt.set_widget_name("pinned-prompt");
        pinned_prompt.add_css_class("card");
        // Opaque, or the transcript scrolling underneath reads straight
        // through it — Adwaita's .card colour is a translucent overlay.
        pinned_prompt.add_css_class("pinned-prompt");
        pinned_prompt.set_margin_top(4);
        pinned_prompt.set_margin_start(24);
        pinned_prompt.set_margin_end(6);
        pinned_prompt.set_valign(gtk::Align::Start);
        pinned_prompt.set_visible(false);
        pinned_prompt.set_tooltip_text(Some("Jump back to this prompt"));
        // Clickable card: without a pointer cursor it reads as static text.
        pinned_prompt.set_cursor_from_name(Some("pointer"));
        pinned_prompt.append(&pinned_prompt_label);

        // "Jump to latest": content arriving below the fold announces
        // itself instead of stealing the view. A ROW, not a floating pill —
        // laid over the transcript it covered the very lines it was
        // advertising. Here it sits with the rest of the status chrome above
        // the composer and costs the transcript only its own height. Taking
        // the jump is what re-arms tailing, so the offer and the tail latch
        // are one gesture.
        let jump_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        jump_content.append(&gtk::Image::from_icon_name("go-bottom-symbolic"));
        jump_content.append(
            &gtk::Label::builder()
                .label("New messages below — jump to latest")
                .css_classes(["caption"])
                .build(),
        );
        let jump_button = gtk::Button::builder()
            .child(&jump_content)
            .tooltip_text("Scroll to the newest message and start following again")
            .css_classes(["flat"])
            .build();
        let jump_banner = gtk::Revealer::builder()
            .child(&jump_button)
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .build();

        let usage_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .margin_top(12)
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(12)
            .build();
        let usage_panel = gtk::ScrolledWindow::builder()
            .child(&usage_list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        // Opaque over the transcript, exactly as the settings shade is.
        usage_panel.add_css_class("background");
        usage_panel.set_visible(false);

        let options_overlay = gtk::Overlay::new();
        options_overlay.set_vexpand(true);
        options_overlay.set_child(Some(&transcript_scroller));
        options_overlay.add_overlay(&pinned_prompt);
        options_overlay.add_overlay(&usage_panel);
        options_overlay.add_overlay(&controls_scroller);

        widget.append(&top_bar);
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(&options_overlay);
        widget.append(&jump_banner);
        widget.append(&busy_row);
        widget.append(&permission_bar);
        widget.append(&entry_row);

        let pane = Rc::new(Self {
            widget,
            workspace,
            transcript,
            transcript_scroller,
            pinned_prompt: pinned_prompt.clone(),
            pinned_prompt_label,
            last_prompt_row: RefCell::new(None),
            entry: entry.clone(),
            agent_picker,
            approval_picker,
            controls,
            auth_box,
            options_panel: controls_scroller.clone(),
            options_toggle: options_toggle.clone(),
            chat_tab: chat_tab.clone(),
            identity_label: identity_label.clone(),
            identity_glyph: identity_glyph.clone(),
            composer_area: entry_scroller.clone(),
            permission_bar,
            allow_button: allow.clone(),
            deny_button: deny.clone(),
            permission_icon,
            permission_label,
            permission_subtitle,
            status_label,
            status_spinner: status_spinner.clone(),
            busy_row: busy_row.clone(),
            busy_label,
            permission_detail,
            client: RefCell::new(None),
            pending_permission: RefCell::new(None),
            pending_marks: RefCell::new(HashMap::new()),
            attachments: RefCell::new(Vec::new()),
            chips,
            send_button: send.clone(),
            preedit: Rc::new(Cell::new(false)),
            last_sent: RefCell::new(None),
            stop_button: stop_button.clone(),
            usage_bar,
            usage_tab: usage_tab.clone(),
            usage_panel: usage_panel.clone(),
            usage_list,
            context_used: Cell::new(0),
            session_usage: RefCell::new(None),
            session_cost: RefCell::new(None),
            context_limit: Cell::new(200_000),
            stick_to_bottom: Rc::new(Cell::new(true)),
            jump_banner: jump_banner.clone(),
            scroll_pending: Rc::new(Cell::new(false)),
            mode_sync: RefCell::new(None),
            controls_signature: RefCell::new(None),
            last_modes: RefCell::new(None),
            syncing: Cell::new(false),
            environments,
            bridge_command,
            environment,
            current_agent: RefCell::new(None),
            current_agent_view: RefCell::new(None),
            current_thought: RefCell::new(None),
            current_thought_header: RefCell::new(None),
            pending_user: RefCell::new(None),
            plan_card: RefCell::new(None),
            plan_snapshot: RefCell::new(None),
            tool_cards: RefCell::new(HashMap::new()),
            command_provider,
            transcript_rows: Cell::new(0),
            session_info: RefCell::new(None),
            persisted_session: RefCell::new(None),
            pending_restore: RefCell::new(None),
            model_value: RefCell::new(None),
            permission_mode: RefCell::new(None),
            mode_config: RefCell::new(None),
            selected: Cell::new(false),
            notify_key: next_notify_key(),
            busy: Cell::new(false),
            on_persist: RefCell::new(None),
            on_busy: RefCell::new(None),
            restore_notice,
            placeholder,
            session_has_content: Cell::new(false),
            needs_auth: Cell::new(false),
            reconnect_attempts: Cell::new(0),
            mode_revert: RefCell::new(None),
            pending_prompts: RefCell::new(std::collections::VecDeque::new()),
            capture: RefCell::new(None),
            relocated: Cell::new(false),
            hosting_refusal: RefCell::new(None),
            relocation_pending: Cell::new(false),
            orchestrator: Cell::new(false),
            orchestrator_row: orchestrator_row.clone(),
            on_role_changed: RefCell::new(None),
            transcript_log: RefCell::new(std::collections::VecDeque::new()),
            transcript_dropped: Cell::new(0),
            last_activity: Cell::new(None),
            turns: Cell::new(0),
            advertised_models: RefCell::new(None),
            on_ready_once: RefCell::new(None),
        });

        // Tail behaviour, in one place. Everything that moves the bottom —
        // an appended row, a streaming chunk landing in the live buffer,
        // the working row appearing, the composer growing under the
        // transcript — surfaces as a change to this adjustment's upper or
        // page-size, so re-pinning HERE covers all of them and no call site
        // has to remember to scroll. Scrolling away turns tailing off;
        // scrolling back turns it on.
        {
            let adjustment = pane.transcript_scroller.vadjustment();
            let stick = pane.stick_to_bottom.clone();
            let banner = jump_banner.clone();
            adjustment.connect_value_changed(move |adjustment| {
                let bottom = at_bottom(adjustment);
                stick.set(bottom);
                if bottom {
                    // Arrived under their own steam: nothing left to offer.
                    banner.set_reveal_child(false);
                }
            });
            // Growth in `upper` is new content; a change in `page-size`
            // alone is the viewport resizing. Only the former is worth
            // announcing, or the banner would fire every time the composer
            // grew a line.
            let last_upper = Rc::new(Cell::new(0.0f64));
            let stick = pane.stick_to_bottom.clone();
            let pending = pane.scroll_pending.clone();
            let banner = jump_banner.clone();
            adjustment.connect_changed(move |adjustment| {
                let upper = adjustment.upper();
                let grew = upper > last_upper.get() + 1.0;
                last_upper.set(upper);
                match tail_action(stick.get(), grew) {
                    TailAction::Announce => {
                        banner.set_reveal_child(true);
                        return;
                    }
                    TailAction::Nothing => return,
                    TailAction::Repin => {}
                }
                banner.set_reveal_child(false);
                if pending.get() {
                    return;
                }
                // Deferred, never inline: this fires from size-allocate, and
                // moving the adjustment mid-allocation fights the layout
                // pass that is still running.
                pending.set(true);
                let adjustment = adjustment.clone();
                let stick = stick.clone();
                let pending = pending.clone();
                glib::idle_add_local_once(move || {
                    pending.set(false);
                    // Re-checked: the user may have scrolled away while
                    // this was queued, and their scroll wins.
                    if stick.get() {
                        adjustment.set_value(adjustment.upper() - adjustment.page_size());
                    }
                });
            });
        }

        {
            let adjustment = pane.transcript_scroller.vadjustment();
            let stick = pane.stick_to_bottom.clone();
            let banner = jump_banner.clone();
            jump_button.connect_clicked(move |_| {
                stick.set(true);
                banner.set_reveal_child(false);
                adjustment.set_value(adjustment.upper() - adjustment.page_size());
            });
        }

        // Pin/unpin as the transcript scrolls or grows. Per-scroll work is
        // one bounds transform — bounded, no layout forced.
        {
            let adjustment = pane.transcript_scroller.vadjustment();
            let weak = Rc::downgrade(&pane);
            adjustment.connect_value_changed(move |_| {
                if let Some(pane) = weak.upgrade() {
                    pane.sync_pinned_prompt();
                }
            });
            let weak = Rc::downgrade(&pane);
            adjustment.connect_changed(move |_| {
                if let Some(pane) = weak.upgrade() {
                    pane.sync_pinned_prompt();
                }
            });
        }
        {
            let click = gtk::GestureClick::new();
            let weak = Rc::downgrade(&pane);
            click.connect_released(move |_, _, _, _| {
                let Some(pane) = weak.upgrade() else { return };
                let row = pane.last_prompt_row.borrow().clone();
                if let Some(row) = row {
                    if let Some(bounds) = row.compute_bounds(&pane.transcript) {
                        pane.transcript_scroller
                            .vadjustment()
                            .set_value(f64::from(bounds.y()));
                    }
                }
            });
            pinned_prompt.add_controller(click);
        }

        // An input method's composition, tracked so Enter can stay out of
        // its way. GtkTextView announces preedit changes; an empty string is
        // the composition ending. Without this, Enter-to-send fires on the
        // keystroke that COMMITS a composition and sends a half-typed word.
        {
            let preedit = pane.preedit.clone();
            entry.connect_preedit_changed(move |_, text| {
                preedit.set(!text.is_empty());
            });
        }

        // Enter sends; Shift+Enter inserts a newline; Escape stops a running
        // turn; Up in an empty composer brings the last prompt back. Nothing
        // here mentions the completion list: while it is open the framework's
        // own controller takes the arrows, Escape and Enter first, and its
        // key_activates says Enter picks the highlighted command.
        {
            let controller = gtk::EventControllerKey::new();
            let weak = Rc::downgrade(&pane);
            controller.connect_key_pressed(move |_, key, _, modifier| {
                let Some(pane) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                let state = ComposerState {
                    preedit: pane.preedit.get(),
                    streaming: pane.stop_button.get_visible(),
                    empty: pane.entry_text().trim().is_empty(),
                    awaiting_permission: pane.pending_permission.borrow().is_some(),
                };
                match composer_key(key, modifier, state) {
                    ComposerKey::Send => {
                        pane.send();
                        glib::Propagation::Stop
                    }
                    ComposerKey::DenyPermission => {
                        pane.deny_button.emit_clicked();
                        glib::Propagation::Stop
                    }
                    ComposerKey::Stop => {
                        pane.stop_button.emit_clicked();
                        glib::Propagation::Stop
                    }
                    ComposerKey::RecallLast => {
                        let last = pane.last_sent.borrow().clone();
                        match last {
                            Some(text) => {
                                pane.entry.buffer().set_text(&text);
                                let end = pane.entry.buffer().end_iter();
                                pane.entry.buffer().place_cursor(&end);
                                glib::Propagation::Stop
                            }
                            // Nothing to recall: Up is still a cursor key.
                            None => glib::Propagation::Proceed,
                        }
                    }
                    ComposerKey::Insert => glib::Propagation::Proceed,
                }
            });
            entry.add_controller(controller);
        }

        // Files dropped on the composer become attachments. The "+" menu
        // already queues them; dragging is the same act with the mouse, and
        // its absence was the one place the composer looked inert.
        {
            let drop = gtk::DropTarget::new(
                gtk::gdk::FileList::static_type(),
                gtk::gdk::DragAction::COPY,
            );
            let weak = Rc::downgrade(&pane);
            drop.connect_drop(move |_, value, _, _| {
                let Some(pane) = weak.upgrade() else {
                    return false;
                };
                let Ok(files) = value.get::<gtk::gdk::FileList>() else {
                    return false;
                };
                for file in files.files() {
                    let Some(path) = file.path() else { continue };
                    // An image dropped in is an image, not a text blob:
                    // whichever reading works is the one the agent gets.
                    let attachment = image_attachment(&path).or_else(|_| text_attachment(&path));
                    match attachment {
                        Ok((label, block)) => pane.add_attachment(label, block),
                        Err(e) => pane.meta_row(&format!("cannot attach: {e}")),
                    }
                }
                true
            });
            entry_row.add_controller(drop);
        }
        {
            let weak = Rc::downgrade(&pane);
            entry.buffer().connect_changed(move |_| {
                if let Some(pane) = weak.upgrade() {
                    pane.sync_send();
                }
            });
        }
        // Pasting an image queues it as an attachment.
        {
            let weak = Rc::downgrade(&pane);
            entry.connect_paste_clipboard(move |view| {
                let Some(pane) = weak.upgrade() else { return };
                let clipboard = view.clipboard();
                if !clipboard
                    .formats()
                    .contains_type(gtk::gdk::Texture::static_type())
                {
                    return;
                }
                let weak = Rc::downgrade(&pane);
                clipboard.read_texture_async(gtk::gio::Cancellable::NONE, move |result| {
                    let Some(pane) = weak.upgrade() else { return };
                    if let Ok(Some(texture)) = result {
                        use base64::Engine;
                        let png = texture.save_to_png_bytes();
                        let data = base64::engine::general_purpose::STANDARD.encode(png.as_ref());
                        pane.add_attachment(
                            "pasted image".into(),
                            ContentBlock::Image(ImageContent::new(data, "image/png")),
                        );
                    }
                });
            });
        }

        pane.refresh_placeholder();

        let weak = Rc::downgrade(&pane);
        send.connect_clicked(move |_| {
            if let Some(pane) = weak.upgrade() {
                pane.send();
            }
        });
        let weak = Rc::downgrade(&pane);
        allow.connect_clicked(move |_| {
            if let Some(pane) = weak.upgrade() {
                pane.answer_permission(true);
            }
        });
        let weak = Rc::downgrade(&pane);
        deny.connect_clicked(move |_| {
            if let Some(pane) = weak.upgrade() {
                pane.answer_permission(false);
            }
        });
        let weak = Rc::downgrade(&pane);
        // One handler for the whole group: with three tabs, each toggle
        // firing its own half of the answer is how panels end up visible
        // together.
        for toggle in [&chat_tab, &options_toggle, &usage_tab] {
            let weak = weak.clone();
            toggle.connect_toggled(move |_| {
                let Some(pane) = weak.upgrade() else { return };
                pane.sync_tabs();
            });
        }

        let weak = Rc::downgrade(&pane);
        orchestrator_row.connect_active_notify(move |row| {
            let Some(pane) = weak.upgrade() else { return };
            // `syncing` covers every programmatic write to this switch —
            // restoring a tab, the strip taking the role away because
            // another chat claimed it — none of which is the user asking
            // for anything.
            if pane.syncing.get() {
                return;
            }
            let hook = pane.on_role_changed.borrow().clone();
            match hook {
                Some(hook) => hook(row.is_active()),
                // No strip (a probe instance): honour the switch locally so
                // the row is never a control that does nothing.
                None => pane.set_orchestrator_role(row.is_active()),
            }
        });

        let weak = Rc::downgrade(&pane);
        new_session_row.connect_activated(move |_| {
            let Some(pane) = weak.upgrade() else { return };
            // Same agent, fresh conversation. Controls keep their shape
            // (disabled until Ready re-enables them) — nothing jumps.
            pane.reset_session(false);
            pane.ensure_client(None);
        });

        // Auto-approve is a per-chat setting like the rest of them: it
        // rides in the tab's persisted entry rather than resetting to off
        // every launch.
        let weak = Rc::downgrade(&pane);
        pane.approval_picker.connect_active_notify(move |_| {
            let Some(pane) = weak.upgrade() else { return };
            if pane.syncing.get() {
                return;
            }
            pane.notify_persist();
        });

        // The empty page names its agent whether the change came from the
        // user or from a restored tab, so this sits OUTSIDE the `syncing`
        // guard below — a restored Gemini tab sets the picker under it.
        let weak = Rc::downgrade(&pane);
        pane.agent_picker.connect_selected_notify(move |_| {
            if let Some(pane) = weak.upgrade() {
                pane.refresh_placeholder();
            }
        });

        // Switching agents starts a fresh session (never a new window).
        let weak = Rc::downgrade(&pane);
        pane.agent_picker.connect_selected_notify(move |_| {
            let Some(pane) = weak.upgrade() else { return };
            if pane.syncing.get() {
                return;
            }
            pane.reset_session(true);
            // The tab is labelled with its agent, and the stored session id
            // belonged to the old one.
            pane.persisted_session.borrow_mut().take();
            pane.notify_persist();
            pane.set_status(&format!(
                "{} · new session on next prompt",
                pane.agent_name()
            ));
        });
        let weak = Rc::downgrade(&pane);
        stop_button.connect_clicked(move |_| {
            let Some(pane) = weak.upgrade() else { return };
            let result = match pane.client.borrow().as_ref() {
                Some(client) => client.cancel(),
                None => Ok(()),
            };
            if let Err(e) = result {
                tracing::warn!("cancel failed: {e}");
            } else {
                // The agent will see Cancelled everywhere this turn; the
                // log keeps the fact that it was a Stop, not a refusal.
                pane.workspace.ide.record_permission(
                    "(the in-flight turn)",
                    "cancelled",
                    "the user pressed Stop; the turn and any tool calls in \
                     it resolve as cancelled, not as refusals",
                );
            }
        });

        // Attach actions (native GAction group on the pane).
        let actions = gtk::gio::SimpleActionGroup::new();
        let add_action = |name: &str, pane: &Rc<Self>, f: fn(&Rc<Self>)| {
            let action = gtk::gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(pane);
            action.connect_activate(move |_, _| {
                if let Some(pane) = weak.upgrade() {
                    f(&pane);
                }
            });
            actions.add_action(&action);
        };
        add_action("attach-selection", &pane, Self::attach_selection);
        add_action("attach-active", &pane, Self::attach_active_file);
        add_action("attach-file", &pane, |p| p.attach_via_dialog(false));
        add_action("attach-image", &pane, |p| p.attach_via_dialog(true));
        pane.widget.insert_action_group("chat", Some(&actions));
        // The header says whose conversation this is from the first frame,
        // not from the first thing that happens to persist.
        pane.refresh_identity();

        pane
    }

    /// Send a utility prompt and hand the agent's next full reply to
    /// `on_done`. Renders in the transcript like any exchange.
    pub fn request_text(self: &Rc<Self>, prompt: String, on_done: Box<dyn FnOnce(String)>) {
        // A tab that has not been opened yet still holds its conversation:
        // activate resumes it rather than stranding the utility prompt in
        // a fresh session.
        self.activate();
        if self.client.borrow().is_none() {
            on_done(String::new());
            return;
        }
        self.finalize_stream();
        let card = self.user_card(&prompt, &[]);
        self.pending_prompts.borrow_mut().push_back(PendingPrompt {
            restore: None,
            card,
            queued: None,
            origin: None,
        });
        *self.capture.borrow_mut() = Some((String::new(), on_done));
        let result = match self.client.borrow().as_ref() {
            Some(client) => client.prompt(prompt),
            None => Ok(()),
        };
        match result {
            Ok(()) => {
                self.mark_session_content();
                self.stop_button.set_visible(true);
            }
            Err(e) => {
                self.meta_row(&format!("error: {e}"));
                if let Some((_, on_done)) = self.capture.borrow_mut().take() {
                    on_done(String::new());
                }
            }
        }
    }

    // --- orchestration ----------------------------------------------------

    /// Is this the workspace's orchestrator?
    pub fn is_orchestrator(&self) -> bool {
        self.orchestrator.get()
    }

    /// Take on (or give up) the orchestrator role.
    ///
    /// Only the strip calls this: the role is one per workspace, and a
    /// pane can see no other pane. It does not touch the MCP server
    /// either — the strip does that first, so the tools are already
    /// served (or already gone) by the time the session respawns and
    /// re-lists them.
    pub fn set_orchestrator_role(&self, on: bool) {
        if self.orchestrator.get() == on {
            self.sync_orchestrator_row();
            return;
        }
        self.orchestrator.set(on);
        self.sync_orchestrator_row();
        self.notify_persist();
    }

    /// Redraw the header's identity: which agent, in which environment,
    /// and whether it orchestrates.
    fn refresh_identity(&self) {
        let name = self.agent_name();
        self.identity_label
            .set_label(&if self.environment.is_primary() {
                name
            } else {
                format!("{name} · {}", self.environment)
            });
        self.identity_label
            .set_tooltip_text(Some(&if self.environment.is_primary() {
                format!("{} works in your own checkout", self.agent_name())
            } else {
                format!(
                    "{} works in {} — its own clone of the workspace, with its own devcontainer",
                    self.agent_name(),
                    self.environment
                )
            }));
        self.identity_glyph.set_visible(self.orchestrator.get());
    }

    fn sync_orchestrator_row(&self) {
        let on = self.orchestrator.get();
        self.syncing.set(true);
        self.orchestrator_row.set_active(on);
        self.identity_glyph.set_visible(on);
        self.syncing.set(false);
        self.orchestrator_row.set_subtitle(if on {
            "Orchestration tools are served to this chat only"
        } else {
            "This chat can create and drive other chats"
        });
    }

    /// Bring the agent back on the same conversation.
    ///
    /// The tool list is sent once per session, at `initialize`, so a chat
    /// that gains or loses the orchestration tools has to reconnect to
    /// see the change — the same mechanism relocation uses, for the same
    /// reason, and the conversation crosses on `session/load` exactly as
    /// it does there. Doing nothing instead would leave a freshly
    /// designated orchestrator without the tools it was just given, with
    /// no way to tell.
    pub fn respawn_keeping_conversation(self: &Rc<Self>) {
        if self.client.borrow().is_none() {
            // Nothing to respawn: the next activation spawns with the
            // list as it now stands.
            return;
        }
        let resume = self
            .persisted_session
            .borrow()
            .as_ref()
            .map(|(_, session)| session.clone());
        self.reset_session(false);
        self.ensure_client(resume);
    }

    /// This chat as the orchestration tools observe it.
    pub fn chat_facts(&self, chat: EnvironmentId) -> taste_core::orchestration::ChatFacts {
        use taste_core::orchestration::{ChatFacts, ChatState, UsageSummary};
        let state = if self.client.borrow().is_none() {
            ChatState::Disconnected
        } else if self.pending_permission.borrow().is_some() {
            // Ahead of `busy` deliberately: a chat waiting on the user IS
            // mid-turn, and reporting that as "streaming" is how an
            // orchestrator waits forever for a turn only a person can
            // unblock.
            ChatState::AwaitingPermission
        } else if self.session_info.borrow().is_none() {
            ChatState::Starting
        } else if self.busy.get() {
            ChatState::Streaming
        } else {
            ChatState::Idle
        };
        let usage = self
            .session_usage
            .borrow()
            .as_ref()
            .map(|usage| UsageSummary {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                total_tokens: usage.total_tokens,
                context_used: self.context_used.get(),
                context_limit: self.context_limit.get(),
            });
        ChatFacts {
            chat,
            agent: self.agent_name(),
            model: self.model_value.borrow().clone(),
            session: self
                .session_info
                .borrow()
                .as_ref()
                .map(|(_, session)| session.clone()),
            state,
            idle_for_secs: self.last_activity.get().map(|at| at.elapsed().as_secs()),
            turns: self.turns.get(),
            usage,
            orchestrator: self.orchestrator.get(),
        }
    }

    /// The tail of this chat's text mirror.
    pub fn transcript_tail(&self, max: usize) -> taste_core::orchestration::TranscriptTail {
        let log = self.transcript_log.borrow();
        let take = max.min(log.len());
        let lines: Vec<_> = log.iter().skip(log.len() - take).cloned().collect();
        taste_core::orchestration::TranscriptTail {
            elided_by_the_cap: (log.len() - take) as u64,
            dropped_by_the_pane: self.transcript_dropped.get(),
            lines,
        }
    }

    /// Send a prompt that did not come from the composer.
    ///
    /// The whole of the composer's send path except the composer: the
    /// prompt lands in the transcript as an ordinary user message, queues
    /// behind a running turn exactly as a typed one does, and the tab
    /// shows both halves. An orchestrator talking to a sub-agent is not a
    /// back channel — the user reads every word of it in that tab.
    pub fn submit_prompt(
        self: &Rc<Self>,
        text: String,
    ) -> Result<taste_core::orchestration::SendOutcome, String> {
        if text.trim().is_empty() {
            return Err("an empty prompt is not a message".into());
        }
        self.activate();
        if self.client.borrow().is_none() {
            return Err(format!(
                "{} has no live agent in this chat right now; it reconnects on its own, \
                 so try again shortly",
                self.agent_name()
            ));
        }
        self.stick_to_bottom.set(true);
        self.jump_banner.set_reveal_child(false);
        self.finalize_stream();
        let card = self.user_card(text.trim(), &[]);
        let blocks = vec![ContentBlock::Text(TextContent::new(text.clone()))];
        let result = match self.client.borrow().as_ref() {
            Some(client) => client.prompt_blocks(blocks),
            None => Ok(()),
        };
        match result {
            Ok(()) => {
                self.mark_session_content();
                let queued = self.stop_button.get_visible();
                let badge = queued.then(|| {
                    let badge = gtk::Label::builder()
                        .label("queued — sends when the current turn ends")
                        .xalign(0.0)
                        .css_classes(["dim-label", "caption"])
                        .margin_top(CARD_INSET)
                        .margin_bottom(CARD_INSET)
                        .margin_start(CARD_INSET)
                        .margin_end(CARD_INSET)
                        .build();
                    card.append(&badge);
                    (badge, std::time::Instant::now())
                });
                self.pending_prompts.borrow_mut().push_back(PendingPrompt {
                    // Nothing to hand back to a composer nobody typed in.
                    restore: None,
                    card,
                    queued: badge,
                    origin: None,
                });
                self.stop_button.set_visible(true);
                self.set_busy(true);
                self.set_status(&format!("{} · working…", self.agent_name()));
                self.touch();
                Ok(taste_core::orchestration::SendOutcome { queued })
            }
            Err(e) => {
                self.meta_row(&format!("error: {e}"));
                Err(format!("the agent refused the prompt: {e}"))
            }
        }
    }

    /// Choose this chat's agent by registry id. False when no such agent
    /// exists — the caller names the ones that do.
    pub fn set_agent_id(&self, id: &str) -> bool {
        let agents = builtin_agents();
        let Some(index) = agents.iter().position(|a| a.id == id) else {
            return false;
        };
        self.syncing.set(true);
        self.agent_picker.set_selected(index as u32);
        self.syncing.set(false);
        true
    }

    /// Set the model this chat's sessions run on (a config option *value*
    /// id). Applied to the session at Ready, like a restored one.
    pub fn set_model_value(&self, model: Option<String>) {
        *self.model_value.borrow_mut() = model;
    }

    /// The model options this session advertises: (value id, label).
    /// Empty before Ready, and for an agent that exposes no model choice.
    pub fn advertised_models(&self) -> Vec<(String, String)> {
        self.advertised_models
            .borrow()
            .as_ref()
            .map(|(_, values)| values.clone())
            .unwrap_or_default()
    }

    /// Run something once, the next time this chat's session is ready.
    /// Replaces any previous arming — there is one creation in flight per
    /// chat, by construction.
    pub fn on_ready_once(&self, action: ReadyHook) {
        *self.on_ready_once.borrow_mut() = Some(action);
    }

    /// Something happened in this chat.
    fn touch(&self) {
        self.last_activity.set(Some(std::time::Instant::now()));
    }

    /// Mirror one line into the text transcript.
    ///
    /// Bounded like the widget list it shadows, and forgetful at the same
    /// end: what falls off the front is counted, never silently lost.
    fn record_line(&self, speaker: &'static str, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.touch();
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let mut log = self.transcript_log.borrow_mut();
        log.push_back(taste_core::orchestration::TranscriptLine {
            speaker,
            // One line per entry: a transcript tail is for reading, and a
            // pasted file in the middle of it is not.
            text: single_line(text, TRANSCRIPT_LINE_CHARS),
            at,
        });
        while log.len() > MAX_TRANSCRIPT_MIRROR_LINES {
            log.pop_front();
            self.transcript_dropped
                .set(self.transcript_dropped.get() + 1);
        }
    }

    /// Wire the owning tab strip: `persist` fires when this chat's
    /// restorable identity changes (session, agent, model, permission
    /// mode) and `busy` while a turn is in flight.
    pub fn set_hooks(&self, persist: PersistHook, busy: BusyHook) {
        *self.on_persist.borrow_mut() = Some(persist);
        *self.on_busy.borrow_mut() = Some(busy);
    }

    /// How this chat asks the strip to move the orchestrator role.
    pub fn set_on_role_changed(&self, hook: RoleHook) {
        *self.on_role_changed.borrow_mut() = Some(hook);
    }

    /// The environment this chat's agent works in. Every chat has one; the
    /// primary's chat is the one about the user's own checkout.
    pub fn environment(&self) -> &EnvironmentId {
        &self.environment
    }

    /// Whether a turn is in flight — what the fleet view's busy indicator
    /// is showing for this chat's environment.
    pub fn is_busy(&self) -> bool {
        self.busy.get()
    }

    /// Whether this chat is stopped on a question only the user can
    /// answer: an unanswered permission request, or a sign-in it cannot
    /// perform for itself.
    ///
    /// Both, not just the permission — the distinction the fleet cares
    /// about is "will this move again without me", and a chat waiting to be
    /// signed in will not. It is the same pair notify.rs refuses to
    /// withdraw on focus, for the same reason.
    ///
    /// This is what lights the attention marker on the environment's row in
    /// the panel. With one chat per environment and only the selected one on
    /// screen, an unseen chat is exactly the one that would otherwise wait
    /// unnoticed, so the fact has to leave the pane.
    ///
    /// Two O(1) reads of state this pane already owns, so a row can ask per
    /// render.
    ///
    /// Deliberately wider than [`ChatPane::chat_facts`]'s
    /// `AwaitingPermission`, which is the narrower thing the orchestration
    /// contract names and reports for the permission case alone. Not drift:
    /// one is a light in a sidebar, the other is a state token an
    /// orchestrator branches on, and widening that token is a change to the
    /// tool surface rather than to this panel.
    pub fn awaits_user(&self) -> bool {
        self.pending_permission.borrow().is_some() || self.needs_auth.get()
    }

    /// Where this chat's next agent gets spawned: its environment's
    /// checkout, socket and mode, read fresh because the mode moves under
    /// us (a container coming up unlocks the workspace mid-conversation).
    fn aim(&self) -> AgentAim {
        let environment = self.environment.clone();
        // An environment with no supervisor is one that no longer exists;
        // safe mode is the only honest answer, and never the host.
        let running = self
            .environments
            .get(&environment)
            .is_some_and(|supervisor| supervisor.exec().is_container());
        AgentAim::new(
            self.workspace.root(),
            environment,
            &self.bridge_command,
            running,
        )
    }

    /// Where this chat's next agent PROCESS runs: inside its environment's
    /// container when that container is up and can host it, outside-confined
    /// otherwise.
    ///
    /// Separate from [`Self::aim`] on purpose — the aim is the address and
    /// is identical either way, which is exactly why a chat can move between
    /// the two topologies and keep its conversation. Everything here is read
    /// fresh at spawn time, because all of it moves under us.
    ///
    /// Four things must all hold, and each `None` below is a chat that keeps
    /// working rather than one that breaks:
    ///
    /// - the IDE is not itself inside a container (self-hosting already runs
    ///   the agent beside the files, and there is no podman in there);
    /// - this environment has a supervisor with a container up — *any*
    ///   container, the project's or the IDE's baseline. The question here
    ///   is "is there somewhere to be", not "whose config is in force":
    ///   [`taste_core::ExecContext::is_container`] answers the second and
    ///   stays the mode predicate that [`Self::aim`] reports, but an agent
    ///   in the baseline is beside the files and an agent outside it is
    ///   not, which is the whole of what this decides;
    /// - that container answered yes when asked whether it can host an agent
    ///   — node, a writable agent home, and a channel the IDE answers on;
    /// - that channel is still up, because its in-container endpoints are
    ///   the agent's only route to the IDE's tools and to the credential it
    ///   spends. Relocating a Claude agent away from a reachable proxy would
    ///   trade a working chat for a topology.
    fn relocation(&self, spec: &taste_acp::AgentSpec) -> Option<taste_acp::Relocation> {
        use taste_devcontainer::AgentHosting;
        if taste_acp::sandbox::inside_container() {
            return None;
        }
        let environment = self.environment.clone();
        let supervisor = self.environments.get(&environment)?;
        if !supervisor.exec().has_exec_target() {
            return None;
        }
        match supervisor.agent_hosting() {
            AgentHosting::Yes => {}
            // `Unknown` is not `No`: the probe simply has not come back, and
            // the environment republishes `Running` when it does, which is
            // when this chat gets its second look.
            AgentHosting::Unknown => return None,
            AgentHosting::No { reason } => {
                self.report_hosting_refusal(&reason);
                return None;
            }
        }
        // The addresses a relocated agent dials are the channel's, inside
        // the container, and they are only real while the channel is up.
        // `AgentHosting::Yes` means one answered a probe on this container,
        // but a helper can die (a `podman restart`, an OOM) between then and
        // now — and an agent pointed at a socket nothing serves is the exact
        // failure this batch exists to remove.
        let Some(paths) = supervisor.channel_paths() else {
            self.report_hosting_refusal(
                "this environment's channel to the IDE is not up — the chat's agent \
                 stays outside the container, where it can still reach the IDE's \
                 tools and the auth proxy",
            );
            return None;
        };
        let auth = taste_acp::authproxy::proxies(spec).then_some(taste_acp::AuthForward {
            socket: paths.auth.clone(),
        });
        Some(taste_acp::Relocation {
            container: supervisor.container_name(),
            // Which podman that container is on. The agent follows its
            // environment onto the substrate; the name alone is not an
            // address.
            podman: supervisor.substrate().target().clone(),
            mcp_socket: paths.mcp,
            auth,
        })
    }

    /// Whether this chat's session serves the ACP terminal extension, and
    /// with what.
    ///
    /// **The gate is relocation's gate**, which is the point: the position
    /// ENVIRONMENTS.md changed is about one topology only. Client-served
    /// terminals are justified exactly where the agent process already runs
    /// beside the files in this environment's container — there they add
    /// *visibility* of commands the agent can already run, and no
    /// authority. Outside-confined (this environment is down, or its
    /// container cannot host an agent) they would be a genuinely new route
    /// into a container, which is what ARCHITECTURE.md's "no third route to
    /// a process" refused and still refuses — and with no container at all
    /// there is no exec target, so there is nothing to advertise.
    ///
    /// Deriving it from the relocation this same spawn computed — rather
    /// than re-deciding from `AgentHosting` — is deliberate: two predicates
    /// that have to agree eventually do not.
    ///
    /// Self-hosting is the one case relocation does not cover and this
    /// does. There the IDE's own container IS the primary environment, so
    /// the agent is already beside the files without a `podman exec` to get
    /// it there; the condition ("the agent runs in this environment's
    /// container") holds, and terminals are as justified as they are after
    /// a relocation.
    fn terminal_host(
        &self,
        relocation: Option<&taste_acp::Relocation>,
    ) -> Option<taste_acp::TerminalHost> {
        let environment = self.environment.clone();
        let supervisor = self.environments.get(&environment)?;
        let self_hosted =
            taste_acp::sandbox::inside_container() && supervisor.exec().is_container();
        if relocation.is_none() && !self_hosted {
            return None;
        }
        Some(taste_acp::TerminalHost {
            environment,
            // This environment's own context, so a reload re-points the
            // agent's terminals and its `ide_exec` together and neither
            // holds a container id of its own.
            exec: supervisor.exec().clone(),
            // The environment's checkout at its host path, which relocation
            // made the container path too — no translation, by design.
            cwd: supervisor.root().to_path_buf(),
            roster: self.workspace.shells.clone(),
        })
    }

    /// Is this chat's environment on its way somewhere? Building and
    /// starting are the two states that will produce a settled one shortly,
    /// and the only two worth waiting for rather than reacting to.
    fn environment_in_transition(&self) -> bool {
        let environment = self.environment.clone();
        self.environments.get(&environment).is_some_and(|s| {
            matches!(
                s.state(),
                taste_devcontainer::SupervisorState::Building
                    | taste_devcontainer::SupervisorState::Starting
            )
        })
    }

    /// Say once, in the transcript, why this chat's agent is not running
    /// beside its files. Repeating it on every reconnect would bury the
    /// conversation under a fact that has not changed.
    fn report_hosting_refusal(&self, reason: &str) {
        if self.hosting_refusal.borrow().as_deref() == Some(reason) {
            return;
        }
        *self.hosting_refusal.borrow_mut() = Some(reason.to_string());
        self.meta_row(&format!("agent not relocated: {reason}"));
    }

    /// This chat's environment changed lifecycle state: move the agent if
    /// the topology it should be running in has changed.
    ///
    /// **Settled states only.** A rebuild is stop → build → start, three
    /// events for one intent, and respawning on each would tear down a
    /// conversation twice on the way to the answer. `Building` and
    /// `Starting` are therefore ignored outright — the container is going
    /// somewhere and will say so when it arrives. That is the whole
    /// debounce: not a timer, but the observation that only settled states
    /// carry information about where the agent belongs.
    ///
    /// **Only when it changes something.** `Running` is republished when
    /// the hosting probe answers, so this runs more than once per start; a
    /// process already in the right topology is left alone.
    ///
    /// **Never mid-turn.** Relocating means killing the process, which
    /// would lose the turn in flight. The intent is remembered and acted on
    /// when the turn ends, which for a container that just came up is a few
    /// seconds later and invisible.
    ///
    /// The way DOWN needs no case here at all: an agent inside a container
    /// dies with it, and the existing bounded reconnect brings it back —
    /// outside-confined, because that is what the environment now is.
    pub fn on_environment_state(
        self: &Rc<Self>,
        state: &taste_core::event::DevcontainerStateEvent,
    ) {
        use taste_core::event::DevcontainerStateEvent as S;
        if matches!(state, S::Building | S::Starting) {
            return;
        }
        // A settled state is a fresh chance to say why relocation was
        // declined, if it still is.
        self.hosting_refusal.borrow_mut().take();
        self.retopologize();
    }

    /// Respawn if the live agent is in the wrong topology for what its
    /// environment now offers. Idempotent and cheap when nothing changed.
    fn retopologize(self: &Rc<Self>) {
        let live = self.client.borrow().is_some();
        let resume = self
            .persisted_session
            .borrow()
            .as_ref()
            .map(|(_, session)| session.clone());
        if !live {
            // The process is gone — it lived in a container that stopped,
            // or it crashed — and the environment has just settled, which
            // is the first moment a respawn can land somewhere real. The
            // budget resets because a settled environment is new
            // information, not another failed retry.
            if self.needs_auth.get() {
                return;
            }
            let Some(resume) = resume else {
                // No conversation to carry: the user ended the session, or
                // never started one. Silence is the right answer.
                return;
            };
            self.reconnect_attempts.set(0);
            self.ensure_client(Some(resume));
            return;
        }
        let agents = builtin_agents();
        let index = (self.agent_picker.selected() as usize).min(agents.len() - 1);
        let wanted = self.relocation(&agents[index]).is_some();
        if wanted == self.relocated.get() {
            return;
        }
        // `busy`, not the working line's visibility: the row now hides while
        // a permission card is up, and a turn waiting on the user is still a
        // turn in flight.
        if self.busy.get() {
            // A turn is in flight. Killing it to change where the process
            // runs would cost the user work for no gain they asked for.
            self.relocation_pending.set(true);
            return;
        }
        self.relocation_pending.set(false);
        // The conversation does not restart; the process does. Same agent,
        // same settings, same session id — `session/load` carries the
        // history across, exactly as `bind_environment` does for a change
        // of address.
        self.reset_session(false);
        self.ensure_client(resume);
        self.set_status(&format!(
            "{} · {}",
            self.agent_name(),
            if wanted {
                "now running in its environment's container"
            } else {
                "now running outside the container"
            }
        ));
    }

    /// Is this the tab the user is looking at? Only the selected chat
    /// raises window-level toasts, because their actions come back to
    /// whichever pane is selected.
    pub fn set_selected(&self, selected: bool) {
        self.selected.set(selected);
    }

    fn notify_persist(&self) {
        // Everything that changes what this chat IS comes through here —
        // the agent, the session, the role — so the header's identity is
        // redrawn from one place rather than from each of them.
        self.refresh_identity();
        let hook = self.on_persist.borrow().clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// This chat as restorable state: which agent, which conversation,
    /// which session settings.
    pub fn chat_entry(&self) -> taste_core::state::ChatEntry {
        let persisted = self.persisted_session.borrow().clone();
        taste_core::state::ChatEntry {
            agent_id: Some(match &persisted {
                Some((agent, _)) => agent.clone(),
                None => self.agent_id(),
            }),
            session_id: persisted.map(|(_, session)| session),
            model_value: self.model_value.borrow().clone(),
            permission_mode: self.permission_mode.borrow().clone(),
            auto_approve: self.approval_picker.is_active(),
            environment: self.environment.clone(),
            role: self
                .orchestrator
                .get()
                .then_some(taste_core::state::ChatRole::Orchestrator),
        }
    }

    /// The registry id of the agent this chat is set to.
    pub fn agent_id(&self) -> String {
        let agents = builtin_agents();
        let index = (self.agent_picker.selected() as usize).min(agents.len() - 1);
        agents[index].id.clone()
    }

    /// Adopt a persisted tab's settings WITHOUT connecting: the agent, its
    /// session id (armed for `session/load` on first activation), and the
    /// per-chat model, permission mode and auto-approve choices.
    ///
    /// Lazy on purpose. Restoring five tabs at startup must cost five tab
    /// labels, not five agent processes; the conversation comes back when
    /// the user opens the tab, through exactly today's `ensure_client`.
    pub fn arm_from_entry(&self, entry: &taste_core::state::ChatEntry) {
        if let Some(agent_id) = &entry.agent_id {
            let agents = builtin_agents();
            if let Some(index) = agents.iter().position(|a| a.id == *agent_id) {
                self.syncing.set(true);
                self.agent_picker.set_selected(index as u32);
                self.syncing.set(false);
            }
        }
        *self.model_value.borrow_mut() = entry.model_value.clone();
        *self.permission_mode.borrow_mut() = entry.permission_mode.clone();
        // The role comes back with the tab, but is NOT announced from
        // here: one workspace has one orchestrator, and a state file that
        // somehow named two would want the strip to settle it — which it
        // does, once every tab is armed.
        self.orchestrator.set(matches!(
            entry.role,
            Some(taste_core::state::ChatRole::Orchestrator)
        ));
        self.sync_orchestrator_row();
        self.syncing.set(true);
        self.approval_picker.set_active(entry.auto_approve);
        self.syncing.set(false);
        if let (Some(agent_id), Some(session_id)) = (&entry.agent_id, &entry.session_id) {
            *self.persisted_session.borrow_mut() = Some((agent_id.clone(), session_id.clone()));
            *self.pending_restore.borrow_mut() = Some(session_id.clone());
            self.set_status(&format!("{} · opens this conversation", self.agent_name()));
        }
    }

    /// Bring this chat up: resume its armed conversation if it has one,
    /// otherwise start a fresh session. Idempotent — a live client is left
    /// alone — so it is safe on every tab selection.
    ///
    /// Eager at the point of first use: the user should be greeted by the
    /// sign-in invitation (or a ready session), never an inert empty box.
    pub fn activate(self: &Rc<Self>) {
        let resume = self.pending_restore.borrow_mut().take();
        self.ensure_client(resume);
    }

    /// End this chat for good: the ACP session goes, and so does the
    /// handle that would bring it back. The widgets go with the tab page.
    pub fn close(&self) {
        self.reset_session(true);
        self.persisted_session.borrow_mut().take();
    }

    /// Seed a brand-new chat's model from the one it was opened beside:
    /// "new chat, same setup" is what opening a tab means.
    ///
    /// The environment is NOT inherited. One chat has at most one
    /// environment and an environment backs at most one chat; a new tab
    /// opened beside a bound one starts in the primary, and asks for a
    /// world of its own if it wants one.
    pub fn inherit_settings(&self, from: &ChatPane) {
        *self.model_value.borrow_mut() = from.model_value.borrow().clone();
        *self.permission_mode.borrow_mut() = from.permission_mode.borrow().clone();
        self.syncing.set(true);
        self.approval_picker
            .set_active(from.approval_picker.is_active());
        self.agent_picker.set_selected(from.agent_picker.selected());
        self.syncing.set(false);
    }

    fn auto_approve(&self) -> bool {
        self.approval_picker.is_active()
    }

    /// The single, in-place connection/status line.
    /// Open or close the options shade (syncs the toggle; the toggle
    /// handler moves the stack).
    /// Exactly one of the three tabs owns the pane.
    fn sync_tabs(&self) {
        let settings = self.options_toggle.is_active();
        let usage = self.usage_tab.is_active();
        self.options_panel.set_visible(settings);
        self.usage_panel.set_visible(usage);
        // The composer belongs to the transcript.
        self.composer_area.set_visible(!settings && !usage);
    }

    /// Rebuild the Utilization tab and re-tint its badge.
    ///
    /// Everything here is measured, not estimated: the context figures come
    /// from the agent's own UsageUpdate, the token totals from the usage a
    /// finished turn reports. What is NOT here is plan quota and time to
    /// reset — ACP carries no such field, so there is nothing honest to
    /// show for it and the row says so rather than guessing.
    fn refresh_usage(&self) {
        let limit = self.context_limit.get().max(1);
        let used = self.context_used.get();
        let fraction = (used as f64 / limit as f64).min(1.0);

        // Same thresholds as the usage bar's offsets, so the badge and the
        // bar can never disagree.
        for class in ["success", "warning", "error"] {
            self.usage_tab.remove_css_class(class);
        }
        let (class, verdict) = match fraction {
            f if f >= 0.85 => ("error", "very little room left"),
            f if f >= 0.6 => ("warning", "filling up"),
            _ => ("success", "plenty of room"),
        };
        self.usage_tab.add_css_class(class);
        self.usage_tab
            .set_tooltip_text(Some(&format!("Utilization — {verdict}")));

        while let Some(child) = self.usage_list.first_child() {
            self.usage_list.remove(&child);
        }
        let row = |title: &str, subtitle: &str| {
            let row = adw::ActionRow::builder()
                .title(title)
                .subtitle(subtitle)
                .build();
            self.usage_list.append(&row);
        };
        row(
            "Context window",
            &format!(
                "{} of {} — {:.0}% ({verdict})",
                token_count(used),
                token_count(limit),
                fraction * 100.0
            ),
        );
        match self.session_usage.borrow().as_ref() {
            Some(usage) => {
                row(
                    "Session tokens",
                    &format!(
                        "{} in · {} out · {} total",
                        token_count(usage.input_tokens),
                        token_count(usage.output_tokens),
                        token_count(usage.total_tokens)
                    ),
                );
                let cached =
                    usage.cached_read_tokens.unwrap_or(0) + usage.cached_write_tokens.unwrap_or(0);
                if cached > 0 {
                    row(
                        "Cached",
                        &format!("{} read and written", token_count(cached)),
                    );
                }
                if let Some(thought) = usage.thought_tokens.filter(|t| *t > 0) {
                    row("Thinking", &token_count(thought));
                }
            }
            None => row("Session tokens", "nothing reported yet"),
        }
        if let Some((amount, currency)) = self.session_cost.borrow().as_ref() {
            row("Cost", &format!("{amount:.2} {currency}"));
        }
        row(
            "Plan quota",
            "not reported — ACP carries no quota or reset-window field, so \
             the agent has no way to tell us",
        );
    }

    fn show_options(&self, open: bool) {
        if open {
            self.options_toggle.set_active(true);
        } else {
            self.chat_tab.set_active(true);
        }
    }

    fn set_status(&self, text: &str) {
        self.status_label.set_label(text);
        self.status_label.set_visible(!text.is_empty());
    }

    pub fn agent_name(&self) -> String {
        let agents = builtin_agents();
        let index = (self.agent_picker.selected() as usize).min(agents.len() - 1);
        agents[index].display_name.clone()
    }

    /// Name the agent this chat will actually ask. Cheap enough to call on
    /// every agent change, and the empty page is the only thing reading it.
    fn refresh_placeholder(&self) {
        self.placeholder
            .set_title(&format!("Ask {}", self.agent_name()));
    }

    /// The sign-in TUI ended. On success, drop the latch optimistically
    /// and reconnect — a wrong guess re-latches on the next failed prompt,
    /// so this can't wedge.
    pub fn on_sign_in_finished(self: &Rc<Self>, ok: bool) {
        if ok {
            self.clear_notification("taste-auth");
        }
        if !ok || !self.needs_auth.get() {
            return;
        }
        self.needs_auth.set(false);
        self.auth_box.set_visible(false);
        self.show_options(false);
        self.reset_session(false);
        // Credentials decide which models the agent offers, so the list
        // that arrives now may differ from the one we last built. Drop the
        // structure signature: the next config update rebuilds rather than
        // matching against a pre-sign-in shape.
        self.controls_signature.borrow_mut().take();
        self.ensure_client(None);
        self.set_status(&format!(
            "{} · signed in — reconnecting…",
            self.agent_name()
        ));
    }

    /// Toast action: permanently discard the restored session. The id is
    /// forgotten on disk immediately — not at window close — so it cannot
    /// come back after a crash or kill. Then start fresh; the chat's
    /// preferred permission mode is re-applied once it is ready.
    pub fn destroy_stale_session(self: &Rc<Self>) {
        self.persisted_session.borrow_mut().take();
        self.notify_persist();
        self.reset_session(false);
        self.ensure_client(None);
    }

    /// Persist the live session id now. Waiting for window close is how
    /// stale ids kept resurrecting after unclean exits. Only sessions with
    /// content qualify (see `session_has_content`); a sterile id leaves
    /// the stored — still restorable — one alone.
    fn persist_session_id(&self) {
        if !self.session_has_content.get() {
            return;
        }
        *self.persisted_session.borrow_mut() = self.session_info.borrow().clone();
        self.notify_persist();
    }

    /// The first prompt of a session is what makes it restorable: the
    /// agent-side conversation file now exists, so the id is worth keeping.
    fn mark_session_content(&self) {
        if !self.session_has_content.replace(true) {
            self.persist_session_id();
        }
    }

    /// End the session. `clear_controls` only when the control structure is
    /// obsolete (switching agents, escorted fresh session); a plain
    /// disconnect keeps the controls visible and merely disables them.
    fn reset_session(&self, clear_controls: bool) {
        self.client.borrow_mut().take();
        self.session_info.borrow_mut().take();
        self.session_has_content.set(false);
        // An armed-but-unopened conversation belongs to the session that is
        // being ended (and to the agent it was recorded against): starting
        // over must not silently resume it.
        self.pending_restore.borrow_mut().take();
        self.mode_config.borrow_mut().take();
        if clear_controls {
            self.needs_auth.set(false);
            self.mode_sync.borrow_mut().take();
            self.last_modes.borrow_mut().take();
            self.controls_signature.borrow_mut().take();
            clear_children(&self.controls);
        } else {
            self.controls.set_sensitive(false);
        }
        if clear_controls || !self.auth_box.is_visible() {
            // Agent switches rebuild everything; a plain disconnect keeps
            // a visible sign-in invitation — it is the way back in.
            clear_children(&self.auth_box);
            self.auth_box.set_visible(false);
        }
        self.permission_bar.set_reveal_child(false);
        self.stop_button.set_visible(false);
        self.set_busy(false);
        self.current_agent.borrow_mut().take();
        self.current_thought.borrow_mut().take();
        self.tool_cards.borrow_mut().clear();
        self.plan_card.borrow_mut().take();
        self.plan_snapshot.borrow_mut().take();
        self.pending_marks.borrow_mut().clear();
        self.context_used.set(0);
        self.session_usage.borrow_mut().take();
        self.session_cost.borrow_mut().take();
    }

    // --- transcript --------------------------------------------------------

    fn append_row(&self, child: &impl IsA<gtk::Widget>) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::builder()
            .activatable(false)
            .child(child)
            .build();
        self.transcript.append(&row);
        // GtkListBox measures every row on every width change, so pane
        // resizing costs O(rows × text). Cap the live widgets — the full
        // conversation stays with the agent (session/load restores it).
        let mut rows = self.transcript_rows.get() + 1;
        while rows > MAX_TRANSCRIPT_ROWS {
            if let Some(oldest) = self.transcript.first_child() {
                self.transcript.remove(&oldest);
                rows -= 1;
            } else {
                break;
            }
        }
        self.transcript_rows.set(rows);
        // No scroll here: appending grows the adjustment, and the tail
        // policy in `new` decides whether that should move the view. This
        // used to yank the view to the bottom even mid-history.
        row
    }

    /// The pinned copy shows exactly while the last prompt's own card is
    /// FULLY above the viewport — scrolled past (or capped out of the
    /// list). Any part still visible means no pin: overlaying a duplicate
    /// on the real card would cover it and its copy button.
    fn sync_pinned_prompt(&self) {
        let visible = match self.last_prompt_row.borrow().as_ref() {
            Some(row) => match row.compute_bounds(&self.transcript_scroller) {
                Some(bounds) => bounds.y() + bounds.height() < 0.0,
                // No shared root: the row was capped out of the list, so
                // the prompt certainly isn't visible.
                None => true,
            },
            None => false,
        };
        self.pinned_prompt.set_visible(visible);
    }

    /// Show or hide the working indicator. It is a sibling below the
    /// transcript, so toggling it resizes the viewport — the tail policy
    /// picks that up as a page-size change and re-pins the bottom.
    /// Something an environment's row renders about this chat has changed
    /// — a turn starting or ending, a permission request arriving or being
    /// answered.
    ///
    /// A chat in an environment nobody has selected has no other way to
    /// reach the user inside the window, and the *arrival* of a permission
    /// request is the moment that matters: waiting for the next fleet
    /// refresh to light the row would make the marker late exactly when it
    /// is urgent.
    fn note_activity(&self) {
        let hook = self.on_busy.borrow().clone();
        if let Some(hook) = hook {
            hook(self.busy.get());
        }
    }

    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        // A turn that has ended has nothing in flight to name.
        if !busy {
            self.busy_label.set_label(BUSY_IDLE);
        }
        self.sync_busy_row();
        // Mid-turn sends are QUEUED by the session layer, not refused — so
        // the button says "Queue" rather than pretending to send now or
        // going dead and stranding what the user just typed. Disabling it
        // would be the dishonest choice here: the send genuinely works.
        if busy {
            self.send_button.set_label("Queue");
            self.send_button.set_tooltip_text(Some(SEND_TOOLTIP_QUEUED));
        } else {
            self.send_button.set_label("Send");
            self.send_button.set_tooltip_text(Some(SEND_TOOLTIP));
        }
        // The tab strip mirrors this as the page's spinner, so a background
        // chat still shows it is working.
        let hook = self.on_busy.borrow().clone();
        if let Some(hook) = hook {
            hook(busy);
        }
    }

    /// Show the working line only when the turn is genuinely working.
    ///
    /// A permission card up means the turn is *stopped*, waiting on the person
    /// reading it — and a spinner over that is a lie about who is holding
    /// things up. The card itself is the honest indicator meanwhile, so the
    /// row goes away rather than spinning beside it.
    fn sync_busy_row(&self) {
        let waiting = self.pending_permission.borrow().is_some();
        self.busy_row.set_visible(self.busy.get() && !waiting);
    }

    /// Name what the turn is doing, from the tool call that just started.
    ///
    /// It reads as an echo of the card above it only while the transcript is
    /// pinned to the bottom. Scrolled up — which is what the jump banner
    /// exists for — the cards are gone and this line is the only thing on
    /// screen that says what is happening.
    ///
    /// Bounded on purpose: one line, clipped, and only from a call the agent
    /// actually reported as running. Everything else keeps saying "Working…",
    /// which is the truth when the model is writing.
    fn set_activity(&self, title: &str) {
        let text = single_line(title, 72);
        self.busy_label
            .set_label(if text.is_empty() { BUSY_IDLE } else { &text });
    }

    /// Move an accepted prompt's card to where the conversation actually
    /// reached it, and leave a marker at the point it was typed.
    ///
    /// A prompt sent mid-turn is appended where it was TYPED — but by the time
    /// the agent takes it, a whole turn of replies and tool calls sits in
    /// between, so that is not where it joins the conversation. Reading the
    /// transcript back, the prompt appeared to come before answers it never
    /// saw. Returns the marker's row so it can be cleaned up with the card.
    fn reseat_accepted_prompt(self: &Rc<Self>, card: &gtk::Box) -> Option<gtk::ListBoxRow> {
        let origin = card.parent().and_downcast::<gtk::ListBoxRow>()?;
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        content.append(&gtk::Image::from_icon_name("go-down-symbolic"));
        content.append(
            &gtk::Label::builder()
                .label("sent here — jump to where it was taken")
                .css_classes(["dim-label", "caption"])
                .build(),
        );
        let marker = gtk::Button::builder()
            .child(&content)
            .css_classes(["flat"])
            .tooltip_text("Jump to this prompt, further down")
            .halign(gtk::Align::Center)
            .margin_top(4)
            .margin_bottom(4)
            .margin_start(12)
            .margin_end(12)
            .build();
        // Setting the marker as the child unparents the card, which is what
        // frees it to be seated again at the end.
        origin.set_child(Some(&marker));
        let seat = self.append_row(card);
        // The pin mirrors the newest prompt, and that is now this row.
        *self.last_prompt_row.borrow_mut() = Some(seat.clone());
        {
            let weak = Rc::downgrade(self);
            let seat = seat.downgrade();
            marker.connect_clicked(move |_| {
                let (Some(pane), Some(seat)) = (weak.upgrade(), seat.upgrade()) else {
                    return;
                };
                if let Some(bounds) = seat.compute_bounds(&pane.transcript) {
                    pane.transcript_scroller
                        .vadjustment()
                        .set_value(f64::from(bounds.y()));
                }
            });
        }
        Some(origin)
    }

    /// Take a prompt out of the transcript — its card, and the marker left
    /// where it was typed if it had already been reseated. Rejected prompts
    /// and prompts orphaned by a dropped connection are not part of the
    /// conversation, and neither is a signpost to one.
    fn drop_prompt_rows(&self, prompt: &PendingPrompt) {
        let mut rows: Vec<gtk::Widget> = Vec::new();
        if let Some(row) = prompt.card.parent() {
            rows.push(row);
        }
        if let Some(origin) = &prompt.origin {
            rows.push(origin.clone().upcast());
        }
        for row in rows {
            self.forget_prompt_row(&row);
            self.transcript.remove(&row);
            self.transcript_rows
                .set(self.transcript_rows.get().saturating_sub(1));
        }
    }

    /// A prompt card leaving the transcript (rejected prompt, dropped
    /// connection) takes its pin with it — the pin must never advertise a
    /// card that no longer exists.
    fn forget_prompt_row(&self, row: &gtk::Widget) {
        let is_last = self
            .last_prompt_row
            .borrow()
            .as_ref()
            .is_some_and(|last| last.clone().upcast::<gtk::Widget>() == *row);
        if is_last {
            self.last_prompt_row.replace(None);
            self.pinned_prompt.set_visible(false);
        }
    }

    fn meta_row(&self, text: &str) {
        let label = gtk::Label::builder()
            .label(text)
            .xalign(0.5)
            .wrap(true)
            // One line, always. A note is an aside between two cards; at
            // caption size, wrapped across the full width of the pane it
            // stops reading as an aside and starts reading as a paragraph
            // of small grey text.
            .lines(1)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .margin_top(4)
            .margin_bottom(4)
            .margin_start(12)
            .margin_end(12)
            .build();
        if text.lines().count() > 1 || text.chars().count() > 80 {
            label.set_tooltip_text(Some(text));
        }
        self.record_line("note", text);
        self.append_row(&label);
    }

    fn user_card(&self, text: &str, attachments: &[(String, ContentBlock)]) -> gtk::Box {
        // A prompt starts a turn, and a turn owns its plan: the next plan
        // update writes a new card below this one instead of rewriting the
        // last turn's checklist further up the transcript.
        self.plan_card.borrow_mut().take();
        self.record_line("you", text);
        // No spacing: every row carries the inset itself, so spacing here
        // would quietly add to it and only between some pairs of rows.
        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.add_css_class("card");
        card.set_margin_top(4);
        card.set_margin_bottom(4);
        card.set_margin_start(24);
        card.set_margin_end(6);
        // Long prompts are clipped, not dropped: `set_lines` counts RENDERED
        // lines, so one pasted paragraph and a pasted file are both caught.
        let clipped = text.lines().count() > PROMPT_CLIP_LINES as usize
            || text.chars().count() > PROMPT_CLIP_CHARS;
        if !text.is_empty() {
            let label = gtk::Label::builder()
                .label(text)
                .attributes(&no_hyphens())
                .wrap(true)
                .xalign(0.0)
                .hexpand(true)
                .selectable(true)
                // Selectable labels are focusable and draw a persistent
                // text caret once clicked. Pointer selection works without
                // focus, and the Copy button covers keyboard use.
                .focusable(false)
                .margin_top(CARD_INSET)
                .margin_bottom(CARD_INSET)
                .margin_start(CARD_INSET)
                .margin_end(CARD_INSET)
                .build();
            // Quick copy: reuse a prompt without hand-selecting it.
            let copy = gtk::Button::builder()
                .icon_name("edit-copy-symbolic")
                .tooltip_text("Copy prompt")
                .css_classes(["flat", "circular"])
                .valign(gtk::Align::Start)
                .margin_top(4)
                .margin_end(4)
                .build();
            let prompt = text.to_string();
            copy.connect_clicked(move |button| {
                button.clipboard().set_text(&prompt);
            });
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            line.append(&label);
            line.append(&copy);
            card.append(&line);
            if clipped {
                label.set_lines(PROMPT_CLIP_LINES);
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                let open = gtk::Button::builder()
                    .label("Show full prompt")
                    .tooltip_text("Open the whole prompt in a window")
                    .css_classes(["flat"])
                    .halign(gtk::Align::Start)
                    .margin_start(CARD_INSET)
                    .margin_end(CARD_INSET)
                    .margin_bottom(CARD_INSET)
                    .build();
                let full = text.to_string();
                open.connect_clicked(move |button| {
                    present_text_dialog(button, "Prompt", &full);
                });
                card.append(&open);
            }
        }
        // Images get an openable thumbnail; everything else stays a name on
        // the paperclip row. Decoding is bounded (attachments are capped at
        // 5MB) and happens once, here — the texture is what both the
        // thumbnail and the viewer draw.
        let mut thumbnails: Vec<(String, gtk::gdk::Texture)> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for (label, block) in attachments {
            match block {
                ContentBlock::Image(image) => match decode_image(image) {
                    Some(texture) => thumbnails.push((label.clone(), texture)),
                    // Undecodable (unknown codec, truncated): it still went
                    // to the agent, so still say it was attached.
                    None => names.push(label.clone()),
                },
                _ => names.push(label.clone()),
            }
        }
        if !thumbnails.is_empty() {
            let strip = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            strip.set_margin_top(CARD_INSET);
            strip.set_margin_bottom(CARD_INSET);
            strip.set_margin_start(CARD_INSET);
            strip.set_margin_end(CARD_INSET);
            for (label, texture) in thumbnails {
                let picture = image_thumbnail(&texture);
                let button = gtk::Button::builder()
                    .child(&picture)
                    .tooltip_text(format!("Open {label}"))
                    .css_classes(["flat"])
                    .build();
                button.connect_clicked(move |button| {
                    present_image_dialog(button, &label, &texture);
                });
                strip.append(&button);
            }
            card.append(&strip);
        }
        if !names.is_empty() {
            let attached = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            attached.set_margin_top(CARD_INSET);
            attached.set_margin_bottom(CARD_INSET);
            attached.set_margin_start(CARD_INSET);
            attached.set_margin_end(CARD_INSET);
            let clip = gtk::Image::from_icon_name("mail-attachment-symbolic");
            clip.add_css_class("dim-label");
            attached.append(&clip);
            let label = gtk::Label::builder()
                .label(names.join(", "))
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .css_classes(["dim-label", "caption"])
                .build();
            attached.append(&label);
            card.append(&attached);
        }
        let row = self.append_row(&card);
        // This is now the prompt the pin mirrors; it starts visible in the
        // transcript, so the pin starts hidden.
        let pin_text = if text.is_empty() {
            attachments
                .iter()
                .map(|(label, _)| label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            text.to_string()
        };
        self.pinned_prompt_label.set_label(&pin_text);
        self.last_prompt_row.replace(Some(row));
        self.pinned_prompt.set_visible(false);
        card
    }

    /// The agent's streaming buffer: created on first chunk, markdown-styled
    /// when the stream ends (tool call, plan, or turn end).
    fn agent_buffer(&self) -> gtk::TextBuffer {
        if let Some(buffer) = self.current_agent.borrow().as_ref() {
            return buffer.clone();
        }
        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .margin_top(4)
            .margin_bottom(4)
            .margin_start(6)
            .margin_end(24)
            .build();
        let buffer = view.buffer();
        self.append_row(&view);
        *self.current_agent.borrow_mut() = Some(buffer.clone());
        *self.current_agent_view.borrow_mut() = Some(view);
        buffer
    }

    fn thought_buffer(&self) -> gtk::TextBuffer {
        if let Some(buffer) = self.current_thought.borrow().as_ref() {
            return buffer.clone();
        }
        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .css_classes(["dim-label"])
            .build();
        // Caption-weight and dim: reasoning is an aside to the answer, and
        // an expander wearing body text competes with the reply beneath it.
        let header = gtk::Label::builder()
            .label("Thinking…")
            .css_classes(["dim-label", "caption"])
            .build();
        let expander = gtk::Expander::builder()
            .label_widget(&header)
            .child(&view)
            .margin_start(6)
            .margin_end(24)
            .margin_top(2)
            .margin_bottom(2)
            .build();
        self.append_row(&expander);
        let buffer = view.buffer();
        *self.current_thought.borrow_mut() = Some(buffer.clone());
        *self.current_thought_header.borrow_mut() = Some((expander, std::time::Instant::now()));
        buffer
    }

    /// Close whatever is streaming — and first, draw any user message that
    /// was still being assembled, so it lands above what follows it.
    fn finalize_stream(&self) {
        self.flush_user_message();
        self.close_stream();
    }

    /// Draw the accumulated user message, blocks and all. Idempotent.
    fn flush_user_message(&self) {
        let Some(pending) = self.pending_user.borrow_mut().take() else {
            return;
        };
        if pending.text.trim().is_empty() && pending.attachments.is_empty() {
            return;
        }
        // The agent's block, if one is open, belongs above this message.
        self.close_stream();
        self.user_card(pending.text.trim_end(), &pending.attachments);
    }

    /// Close out the current streamed message: style it as markdown.
    fn close_stream(&self) {
        self.current_agent.borrow_mut().take();
        if let Some(view) = self.current_agent_view.borrow_mut().take() {
            let buffer = view.buffer();
            let text = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), true)
                .to_string();
            // The mirror takes the message once it is whole, not chunk by
            // chunk: a tail assembled from stream fragments would show a
            // supervisor half-sentences that no reader ever saw.
            self.record_line("agent", &text);
            // The stream is done: replace the plain text with the real
            // renderer (same one the markdown preview uses).
            if let Some(row) = view.parent().and_downcast::<gtk::ListBoxRow>() {
                let events = self.workspace.events.clone();
                let on_link: std::rc::Rc<dyn Fn(&str)> = std::rc::Rc::new(move |url: &str| {
                    events.publish(taste_core::Event::OpenUrlRequested(url.to_string()));
                });
                let rendered = crate::markdown_view::render(&text, on_link);
                rendered.set_margin_end(12);
                row.set_child(Some(&rendered));
            }
        }
        self.current_thought.borrow_mut().take();
        // The thought is over: say how long it took, rather than leaving
        // "Thinking…" over a finished block for the rest of the session.
        if let Some((expander, since)) = self.current_thought_header.borrow_mut().take() {
            if let Some(label) = expander.label_widget().and_downcast::<gtk::Label>() {
                label.set_label(&thought_duration(since.elapsed()));
            }
        }
    }

    fn upsert_tool_card(
        &self,
        id: String,
        title: Option<String>,
        status: Option<ToolCallStatus>,
        kind: Option<ToolKind>,
        // `None` means "this update says nothing about content" — which is
        // not the same as "this call has no content", and must leave what
        // the card already shows exactly where it is.
        content: Option<&[ToolCallContent]>,
    ) {
        self.finalize_stream();
        // What the agent DID belongs in the mirror as much as what it
        // said: an orchestrator reading a stuck sub-agent's tail needs to
        // see the command it is sitting in. Recorded once, when the card
        // first appears — the updates that follow rewrite one card, and
        // would otherwise write a line each.
        if let Some(title) = &title {
            if !self.tool_cards.borrow().contains_key(&id) {
                self.record_line("tool", title);
            }
        }
        let marked = id.clone();
        let mut cards = self.tool_cards.borrow_mut();
        let card = cards.entry(id).or_insert_with(|| {
            // Hand-built disclosure rather than GtkExpander, for the hover
            // feedback and the full-width click target its title cannot
            // give. Everything in the header is centred on the one line the
            // title is collapsed to: the icons were pinned to the top back
            // when a title could be a whole multi-line command, and against
            // a single line that just leaves them riding high.
            let arrow = gtk::Image::from_icon_name("pan-end-symbolic");
            arrow.set_valign(gtk::Align::Center);
            let status_icon = gtk::Image::from_icon_name("content-loading-symbolic");
            status_icon.set_valign(gtk::Align::Center);
            // A call in flight SPINS. A static three-dot glyph is
            // indistinguishable from a finished call at a glance, which is
            // the one thing the status slot exists to answer.
            let status_spinner = gtk::Spinner::new();
            status_spinner.set_valign(gtk::Align::Center);
            status_spinner.set_visible(false);
            let status_slot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            status_slot.append(&status_spinner);
            status_slot.append(&status_icon);
            // Normal weight, explicitly: Adwaita sets button labels bold, and
            // the header IS a button. A shell command in bold outweighs the
            // agent's prose around it, when what the card actually needs to
            // signal — ran, failed, running — is carried by the status icon.
            let title_label = gtk::Label::builder()
                .xalign(0.0)
                .valign(gtk::Align::Center)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["tool-title"])
                .build();
            // Right-hand end of the header, opposite the status icon so
            // the two never read as one signal: this one is about who said
            // yes, not about how the call went.
            let permission = gtk::Image::new();
            permission.set_valign(gtk::Align::Center);
            permission.set_visible(false);
            permission.add_css_class("dim-label");
            let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            header.append(&arrow);
            header.append(&status_slot);
            header.append(&title_label);
            header.append(&permission);
            let content_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
            // The inset belongs on the CONTENT, not the whole body: the
            // header is an interactive row, and its background has to reach
            // the frame's edge or it reads as a narrower box inside a box.
            content_box.set_margin_start(12);
            content_box.set_margin_end(12);
            content_box.set_margin_bottom(6);
            let revealer = gtk::Revealer::builder().child(&content_box).build();
            // A button, not a click gesture: this keeps the keyboard
            // activation and the a11y role GtkExpander was giving us.
            let toggle = gtk::Button::builder()
                .child(&header)
                .css_classes(["flat"])
                .build();
            {
                let revealer = revealer.clone();
                let arrow = arrow.clone();
                toggle.connect_clicked(move |_| {
                    let open = !revealer.reveals_child();
                    revealer.set_reveal_child(open);
                    arrow.set_icon_name(Some(if open {
                        "pan-down-symbolic"
                    } else {
                        "pan-end-symbolic"
                    }));
                });
            }
            // A card with nothing in it must not offer to open onto nothing.
            // The arrow appears with the first content the call produces.
            //
            // Untargetable rather than INSENSITIVE: an insensitive button
            // dims its label, and a failed call that produced no output is
            // exactly the card that must not read as faded and unimportant.
            arrow.set_visible(false);
            toggle.set_can_target(false);
            toggle.set_can_focus(false);
            let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
            body.append(&toggle);
            body.append(&revealer);
            let frame = gtk::Frame::builder().child(&body).build();
            // Clip to the frame's rounded border: a full-width header paints
            // square corners that would otherwise sit proud of it.
            frame.set_overflow(gtk::Overflow::Hidden);
            frame.set_margin_top(2);
            frame.set_margin_bottom(2);
            frame.set_margin_start(6);
            frame.set_margin_end(24);
            self.append_row(&frame);
            ToolCard {
                status_icon,
                status_spinner,
                title_label,
                permission,
                content: content_box,
                revealer,
                arrow,
                toggle,
                signature: RefCell::new(None),
                kind: Cell::new(ToolKind::Other),
            }
        });
        if let Some(title) = title {
            // The title is whatever the agent called the call — for a shell
            // tool, the entire script. A collapsed card summarises in one
            // line; the whole thing stays a hover away, and the card's own
            // content carries the detail when it is opened.
            card.title_label.set_label(&single_line(&title, 200));
            card.title_label.set_tooltip_text(Some(&title));
        }
        if let Some(status) = status {
            let (icon, css) = match status {
                ToolCallStatus::Pending | ToolCallStatus::InProgress => {
                    ("content-loading-symbolic", None)
                }
                ToolCallStatus::Completed => ("object-select-symbolic", Some("success")),
                ToolCallStatus::Failed => ("dialog-error-symbolic", Some("error")),
                _ => ("content-loading-symbolic", None),
            };
            card.status_icon.set_icon_name(Some(icon));
            // Cleared, not just added to: a call that fails after reporting
            // progress used to keep every colour it had ever worn, so a red
            // error glyph could still be carrying the success class.
            card.status_icon.remove_css_class("success");
            card.status_icon.remove_css_class("error");
            if let Some(css) = css {
                card.status_icon.add_css_class(css);
            }
            // Spinning is reserved for calls that are genuinely running.
            let running = matches!(status, ToolCallStatus::Pending | ToolCallStatus::InProgress);
            card.status_spinner.set_visible(running);
            card.status_icon.set_visible(!running);
            if running {
                card.status_spinner.start();
                // The working line says what is running rather than that
                // something is.
                self.set_activity(&card.title_label.text());
            } else {
                card.status_spinner.stop();
                // This call is done; whether another is running or the model
                // is writing, "Working…" is the most we can honestly claim.
                self.busy_label.set_label(BUSY_IDLE);
            }
        }
        if let Some(kind) = kind {
            card.kind.set(kind);
        }
        // An ACP content update REPLACES the collection, it does not extend
        // it — and agents restate the whole of a shell call's output on
        // every update. Appending it grew the card by a full copy of itself
        // each time, which is the transcript jumping under the reader while
        // the turn streams. So: rebuild when the snapshot actually differs,
        // and leave the card completely alone when it does not.
        if let Some(content) = content {
            let signature = content_signature(content);
            let unchanged = card.signature.borrow().as_ref() == Some(&signature);
            if !unchanged {
                *card.signature.borrow_mut() = Some(signature);
                clear_children(&card.content);
                let terminal = card.kind.get() == ToolKind::Execute;
                for item in content {
                    match item {
                        ToolCallContent::Diff(diff) => {
                            card.content.append(&diff_widget(diff));
                        }
                        ToolCallContent::Content(block) => {
                            if let Some(text) = content_text(&block.content) {
                                card.content.append(&if terminal {
                                    terminal_output_widget(&text)
                                } else {
                                    gtk::Label::builder()
                                        .label(text)
                                        .attributes(&no_hyphens())
                                        .wrap(true)
                                        .xalign(0.0)
                                        .selectable(true)
                                        .css_classes(["caption"])
                                        .build()
                                        .upcast()
                                });
                            }
                        }
                        _ => {}
                    }
                }
                let has_content = card.content.first_child().is_some();
                card.arrow.set_visible(has_content);
                card.toggle.set_can_target(has_content);
                card.toggle.set_can_focus(has_content);
            }
        }
        drop(cards);
        self.apply_permission_mark(&marked);
    }

    /// Record how a permission was answered, on the card for the call it
    /// belongs to.
    ///
    /// This used to be a transcript row each. With auto-approve on that is
    /// one grey line per tool call, interleaved with the cards and repeating
    /// what the card directly above already said — the outcome belongs to
    /// the call, so it belongs on the call's card.
    fn note_permission(&self, id: String, icon: &str, tooltip: String) {
        self.pending_marks
            .borrow_mut()
            .insert(id.clone(), (icon.to_string(), tooltip));
        self.apply_permission_mark(&id);
    }

    /// Put a waiting mark on its card, if the card exists yet.
    fn apply_permission_mark(&self, id: &str) {
        let mark = self.pending_marks.borrow().get(id).cloned();
        let Some((icon, tooltip)) = mark else { return };
        let applied = match self.tool_cards.borrow().get(id) {
            Some(card) => {
                card.permission.set_icon_name(Some(&icon));
                card.permission.set_tooltip_text(Some(&tooltip));
                card.permission.set_visible(true);
                true
            }
            None => false,
        };
        if applied {
            self.pending_marks.borrow_mut().remove(id);
        }
    }

    /// Render the agent's plan — when it actually says something new.
    ///
    /// An ACP plan update restates the WHOLE checklist every time, and
    /// agents restate it freely: on every edit to it, and again when they
    /// pick up the next prompt. Drawing each one filled the transcript with
    /// copies of one list, and put last turn's finished checklist directly
    /// under the prompt just sent, where it read as a stale answer to the
    /// new question.
    ///
    /// So: an identical snapshot changes nothing on screen, a changed one
    /// rewrites this turn's card in place, and the first change in a turn
    /// starts that turn's card where the change happened.
    fn plan_card(&self, plan: &Plan) {
        if plan.entries.is_empty() {
            return;
        }
        let snapshot: Vec<(String, String)> = plan
            .entries
            .iter()
            .map(|entry| (entry.content.clone(), format!("{:?}", entry.status)))
            .collect();
        // Bound the borrow: the write below must not meet a live read.
        let restated = self.plan_snapshot.borrow().as_ref() == Some(&snapshot);
        if restated {
            return; // the same checklist, said again
        }
        *self.plan_snapshot.borrow_mut() = Some(snapshot);
        self.finalize_stream();
        // A card capped out of the transcript (see append_row) is no longer
        // on screen; updating it would write into nowhere.
        let existing = self
            .plan_card
            .borrow()
            .clone()
            .filter(|card| card.parent().is_some());
        let card = match existing {
            Some(card) => {
                clear_children(&card);
                card
            }
            None => {
                let card = gtk::Box::new(gtk::Orientation::Vertical, 2);
                card.set_widget_name("plan-card");
                card.add_css_class("card");
                card.set_margin_start(6);
                card.set_margin_end(24);
                self.append_row(&card);
                *self.plan_card.borrow_mut() = Some(card.clone());
                card
            }
        };
        for entry in &plan.entries {
            use agent_client_protocol::schema::v1::PlanEntryStatus;
            let icon = gtk::Image::from_icon_name(match entry.status {
                PlanEntryStatus::Completed => "object-select-symbolic",
                PlanEntryStatus::InProgress => "content-loading-symbolic",
                _ => "radio-symbolic",
            });
            icon.set_valign(gtk::Align::Start);
            icon.set_margin_top(3);
            let label = gtk::Label::builder()
                .label(&entry.content)
                .wrap(true)
                .xalign(0.0)
                .hexpand(true)
                .build();
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.set_margin_start(8);
            row.set_margin_end(8);
            row.append(&icon);
            row.append(&label);
            card.append(&row);
        }
        if let Some(first) = card.first_child() {
            first.set_margin_top(6);
        }
        if let Some(last) = card.last_child() {
            last.set_margin_bottom(6);
        }
    }

    // --- context attachments ---------------------------------------------

    fn add_attachment(self: &Rc<Self>, label: String, block: ContentBlock) {
        self.attachments.borrow_mut().push((label, block));
        self.refresh_chips();
    }

    fn refresh_chips(self: &Rc<Self>) {
        while let Some(child) = self.chips.first_child() {
            self.chips.remove(&child);
        }
        let attachments = self.attachments.borrow();
        self.chips.set_visible(!attachments.is_empty());
        for (index, (label, block)) in attachments.iter().enumerate() {
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            // An image queued for sending shows the picture, exactly as the
            // card will once it is sent — "pasted image" told you a file was
            // attached but not WHICH one, which is the only thing worth
            // checking before you send it.
            let thumbnail = match block {
                ContentBlock::Image(image) => decode_image(image),
                _ => None,
            };
            match &thumbnail {
                Some(texture) => content.append(&image_thumbnail(texture)),
                None => content.append(
                    &gtk::Label::builder()
                        .label(label)
                        .ellipsize(gtk::pango::EllipsizeMode::Middle)
                        .css_classes(["caption"])
                        .build(),
                ),
            }
            // The remove affordance goes AFTER what it removes: leading it
            // read as a bullet, and put the destructive half of the chip
            // under the pointer on the way to the label.
            let close = gtk::Image::from_icon_name("window-close-symbolic");
            close.add_css_class("dim-label");
            content.append(&close);
            // A chip, not a run of text with an x: an attachment is a
            // discrete object, and the pill is what says so. Buttons are
            // focusable and activate on Space/Enter, so Tab through the
            // chips and press Space still removes one.
            let chip = gtk::Button::builder()
                .child(&content)
                .tooltip_text(format!("Remove {label}"))
                .css_classes(["flat", "attachment-chip"])
                .build();
            chip.update_property(&[gtk::accessible::Property::Label(&format!(
                "Remove attachment {label}"
            ))]);
            let weak = Rc::downgrade(self);
            chip.connect_clicked(move |_| {
                let Some(pane) = weak.upgrade() else { return };
                pane.attachments.borrow_mut().remove(index);
                pane.refresh_chips();
            });
            self.chips.append(&chip);
        }
        drop(attachments);
        self.sync_send();
    }

    /// Send is live only when there is something to send: prompt text or at
    /// least one attachment. Whitespace does not count.
    fn sync_send(&self) {
        let ready = send_ready(&self.entry_text(), self.attachments.borrow().len());
        if self.send_button.is_sensitive() == ready {
            return;
        }
        self.send_button.set_sensitive(ready);
        if ready {
            self.send_button.add_css_class("suggested-action");
        } else {
            self.send_button.remove_css_class("suggested-action");
        }
    }

    fn attach_selection(self: &Rc<Self>) {
        let Some(selection) = self.workspace.ide.selection() else {
            self.set_status("no selection to attach");
            return;
        };
        let label = format!(
            "{}:{}–{}",
            selection
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            selection.start_line,
            selection.end_line
        );
        let uri = format!("file://{}", selection.path.display());
        let block = ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                selection.text,
                uri,
            )),
        ));
        self.add_attachment(label, block);
    }

    fn attach_active_file(self: &Rc<Self>) {
        let Some(active) = self
            .workspace
            .ide
            .open_files()
            .into_iter()
            .find(|f| f.active)
        else {
            self.set_status("no active file to attach");
            return;
        };
        match text_attachment(&active.path) {
            Ok((label, block)) => self.add_attachment(label, block),
            Err(e) => self.meta_row(&format!("cannot attach: {e}")),
        }
    }

    fn attach_via_dialog(self: &Rc<Self>, image: bool) {
        let Some(window) = self
            .widget
            .root()
            .and_then(|r| r.downcast::<gtk::Window>().ok())
        else {
            return;
        };
        let weak = Rc::downgrade(self);
        gtk::FileDialog::new().open(Some(&window), gtk::gio::Cancellable::NONE, move |result| {
            let Some(pane) = weak.upgrade() else { return };
            let Ok(file) = result else { return };
            let Some(path) = file.path() else { return };
            let attachment = if image {
                image_attachment(&path)
            } else {
                text_attachment(&path)
            };
            match attachment {
                Ok((label, block)) => pane.add_attachment(label, block),
                Err(e) => pane.meta_row(&format!("cannot attach: {e}")),
            }
        });
    }

    // --- slash commands ----------------------------------------------------

    fn entry_text(&self) -> String {
        let buffer = self.entry.buffer();
        let (start, end) = buffer.bounds();
        buffer.text(&start, &end, true).to_string()
    }

    fn send(self: &Rc<Self>) {
        let text = self.entry_text();
        if text.trim().is_empty() && self.attachments.borrow().is_empty() {
            return;
        }
        // Nothing typed is cleared until the agent is actually accepting:
        // a failed launch must not eat the prompt.
        self.activate();
        if self.client.borrow().is_none() {
            return; // ensure_client already reported why; input intact
        }

        // Sending returns you to the end of the conversation. Reading back
        // through history does turn tailing off — but your own new prompt
        // is not history, and leaving the view parked mid-transcript made
        // the answer to it arrive somewhere you weren't looking, under an
        // old exchange that read like a reply.
        self.stick_to_bottom.set(true);
        self.jump_banner.set_reveal_child(false);

        let attachments: Vec<(String, ContentBlock)> =
            self.attachments.borrow_mut().drain(..).collect();
        // Recallable with Up while the composer is empty. Recorded before
        // the buffer is cleared, and only for a prompt that carried text.
        if !text.trim().is_empty() {
            *self.last_sent.borrow_mut() = Some(text.clone());
        }
        self.entry.buffer().set_text("");
        self.refresh_chips();
        // Sending returns the caret to the composer: the next thing anyone
        // does after sending is type again, and a send triggered from the
        // button otherwise left focus on the button.
        self.entry.grab_focus();
        self.finalize_stream();

        let card = self.user_card(text.trim(), &attachments);
        let mut blocks: Vec<ContentBlock> =
            attachments.into_iter().map(|(_, block)| block).collect();
        if !text.trim().is_empty() {
            blocks.push(ContentBlock::Text(TextContent::new(text.clone())));
        }
        let result = match self.client.borrow().as_ref() {
            Some(client) => client.prompt_blocks(blocks),
            None => Ok(()),
        };
        match result {
            Ok(()) => {
                self.mark_session_content();
                // Sending mid-turn is fine: the session layer queues it.
                // Say so on the card until its turn starts.
                let badge = self.stop_button.get_visible().then(|| {
                    let badge = gtk::Label::builder()
                        .label("queued — sends when the current turn ends")
                        .xalign(0.0)
                        .css_classes(["dim-label", "caption"])
                        .margin_top(CARD_INSET)
                        .margin_bottom(CARD_INSET)
                        .margin_start(CARD_INSET)
                        .margin_end(CARD_INSET)
                        .build();
                    card.append(&badge);
                    (badge, std::time::Instant::now())
                });
                self.pending_prompts.borrow_mut().push_back(PendingPrompt {
                    restore: Some(text.trim().to_string()),
                    card,
                    queued: badge,
                    origin: None,
                });
                self.stop_button.set_visible(true);
                self.set_busy(true);
                self.set_status(&format!("{} · working…", self.agent_name()));
            }
            Err(e) => self.meta_row(&format!("error: {e}")),
        }
    }

    /// Spawn the selected agent's subprocess if there is no live session.
    fn ensure_client(self: &Rc<Self>, resume: Option<String>) {
        if self.client.borrow().is_some() {
            return;
        }
        let agents = builtin_agents();
        let index = (self.agent_picker.selected() as usize).min(agents.len() - 1);
        let spec = agents[index].clone();
        // Where this agent works, in one value: its environment's checkout,
        // that environment's MCP socket (which is how the IDE will know
        // which environment is calling), and that environment's mode.
        let aim = self.aim();
        // ...and the topology it runs in, which the aim deliberately does
        // not decide. Read fresh, every spawn: a container coming up is the
        // ordinary way a chat's agent moves in beside its files.
        let relocation = self.relocation(&spec);
        self.relocated.set(relocation.is_some());
        // ...and whether this session serves terminals, which follows the
        // topology rather than being decided a second time. A respawn is
        // how the advertisement changes: ACP v1 sends capabilities once, at
        // `initialize`, and a chat that moves between topologies respawns.
        let terminals = self.terminal_host(relocation.as_ref());
        self.status_spinner.start();
        self.status_label.set_visible(true);
        // The one status that earns screen space; safe mode still rides
        // along because it changes what prompts can do, and the environment
        // because it changes where the work lands.
        self.status_label
            .set_label(&match (aim.safe_mode, aim.environment.is_primary()) {
                (true, true) => "Connecting… (safe mode)".to_string(),
                (false, true) => "Connecting…".to_string(),
                (true, false) => format!("Connecting to {}… (safe mode)", aim.environment),
                (false, false) => format!("Connecting to {}…", aim.environment),
            });
        let _ = &spec.display_name;

        // AgentClient::spawn uses tokio::spawn internally; enter the runtime.
        let _guard = crate::runtime::runtime().enter();
        let client = match AgentClient::spawn_aimed(
            spec,
            aim,
            relocation,
            terminals,
            resume,
            // Buffer-aware reads: the agent sees unsaved editor buffers.
            Some(self.workspace.ui.clone()),
        ) {
            Ok(client) => client,
            Err(e) => {
                self.meta_row(&format!("agent launch refused: {e}"));
                return;
            }
        };
        let events = client.events.clone();
        *self.client.borrow_mut() = Some(client);

        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(event) = events.recv().await {
                let Some(pane) = weak.upgrade() else { break };
                pane.handle_event(event);
            }
        });
    }

    fn handle_event(self: &Rc<Self>, event: SessionEvent) {
        match event {
            SessionEvent::AuthRequired { methods } => self.show_auth(methods),
            SessionEvent::Ready {
                session_id,
                restored,
                restore_failed,
                modes,
                config_options,
            } => {
                // A restored session replays history as ordinary updates
                // BEFORE this event. Every streamed block gets its markdown
                // pass from whatever follows it (tool call, plan, turn
                // end) — except the last one, whose "whatever follows" is
                // Ready itself. Without this, the final message of every
                // restored conversation sat there as raw markdown.
                self.finalize_stream();
                // A silent blank where a conversation was expected reads
                // as data loss; the placeholder alert says why it's fresh.
                self.restore_notice.set_visible(restore_failed);
                self.auth_box.set_visible(false);
                let agent_id = self
                    .client
                    .borrow()
                    .as_ref()
                    .map(|c| c.spec.id.clone())
                    .unwrap_or_default();
                self.status_spinner.stop();
                self.status_label.set_visible(false);
                // A live session clears the reconnect budget: the next
                // death starts counting from zero, however many it took.
                self.reconnect_attempts.set(0);
                *self.session_info.borrow_mut() = Some((agent_id, session_id));
                // A restored session has history on disk by definition; a
                // fresh one earns persistence with its first prompt.
                self.session_has_content.set(restored);
                self.persist_session_id();
                if !self.needs_auth.get() {
                    // While sign-in is pending, the shade (with its
                    // sign-in buttons) stays put across respawns.
                    self.show_options(false);
                }
                *self.last_modes.borrow_mut() = modes.clone();
                // What this session will actually accept as a model, kept
                // before the controls consume it: `chat_create` refuses an
                // unknown model by naming these, and a list rebuilt from
                // widgets would be a list of what got rendered rather than
                // of what the agent advertised.
                *self.advertised_models.borrow_mut() = model_choices(&config_options);
                self.build_controls(modes, config_options);
                self.set_status(&format!(
                    "{} · ready{}",
                    self.agent_name(),
                    if restored { " · session restored" } else { "" }
                ));
                // The chat's permission mode is the CHAT's, not the
                // process's: apply it to every session this tab connects,
                // restored ones included. Leaving restored sessions in
                // whatever the adapter defaulted them to is what made the
                // setting look like it evaporated between launches.
                self.apply_preferred_mode();
                self.touch();
                // Whoever was waiting for this session to come up — today,
                // an orchestrator's `chat_create` — runs now, once, after
                // the controls have applied this chat's model.
                if let Some(action) = self.on_ready_once.borrow_mut().take() {
                    action(self.clone());
                }
            }
            SessionEvent::Update(update) => self.render_update(update),
            SessionEvent::Permission { request, reply } => {
                self.finalize_stream();
                let title = permission_title(&request);
                let note = single_line(&title, 120);
                // Auto-approve only when there is something to approve
                // WITH. A request carrying no allow option is a real
                // question, and answering it by taking whatever came first
                // is how a refusal came to be announced as an approval.
                let automatic = self
                    .auto_approve()
                    .then(|| allow_option(&request.options).map(|o| o.name.clone()))
                    .flatten();
                if let Some(name) = automatic {
                    let _ = reply.send(first_allow_outcome(&request));
                    // Name the option that was taken — "approved" was a
                    // claim about intent, and intent is not what the agent
                    // got.
                    self.note_permission(
                        request.tool_call.tool_call_id.to_string(),
                        "changes-allow-symbolic",
                        format!("Auto-approved “{name}”"),
                    );
                    self.workspace.ide.record_permission(
                        &note,
                        "approved",
                        &format!("auto-approve is on; took the “{name}” option"),
                    );
                } else {
                    if self.auto_approve() {
                        // Falling back to the bar beats refusing silently:
                        // the user can see options we have no answer for.
                        self.meta_row(&format!(
                            "auto-approve found no allow option — asking: {note}"
                        ));
                    }
                    // The card must show enough to decide on: the question,
                    // who is asking and where it lands, then the literal
                    // thing — with the whole of it a hover away.
                    let face =
                        permission_face(&request, &self.agent_name(), self.environment.as_str());
                    self.permission_icon.set_icon_name(Some(face.icon));
                    self.permission_label.set_label(&face.title);
                    self.permission_label.set_tooltip_text(Some(&title));
                    self.permission_subtitle.set_label(&face.subtitle);
                    // What the card is called when it is heard rather than
                    // seen: the agent's own phrasing of the ask, which is
                    // more than the kind-derived question above says.
                    if let Some(card) = self.permission_bar.child() {
                        card.update_property(&[gtk::accessible::Property::Label(&title)]);
                    }
                    // The buttons say what the AGENT offers rather than a
                    // generic Allow/Deny: "don't ask again" is a different
                    // answer from "yes, this once" and must not read alike.
                    let allow = allow_option(&request.options);
                    let reject = reject_option(&request.options);
                    let allow_name = allow.map_or("Allow", |o| o.name.as_str());
                    let deny_name = reject.map_or("Deny", |o| o.name.as_str());
                    self.allow_button.set_label(allow_name);
                    self.deny_button.set_label(deny_name);
                    // The tooltips carry the full option name (a long one
                    // ellipsizes in a narrow pane) and, on the safe side
                    // only, the key that reaches it. Nothing advertises a
                    // keystroke that approves.
                    self.allow_button.set_tooltip_text(Some(allow_name));
                    self.deny_button
                        .set_tooltip_text(Some(&format!("{deny_name} (Esc)")));
                    self.allow_button.set_sensitive(allow.is_some());
                    clear_children(&self.permission_detail);
                    // The specifics get said once. A request carrying a diff
                    // has already named its file in the diff's own header,
                    // and repeating the path above it is two answers to one
                    // question.
                    let has_diff =
                        request
                            .tool_call
                            .fields
                            .content
                            .as_ref()
                            .is_some_and(|content| {
                                content
                                    .iter()
                                    .any(|item| matches!(item, ToolCallContent::Diff(_)))
                            });
                    if let Some(code) = face.code.filter(|_| !has_diff) {
                        self.permission_detail
                            .append(&permission_code_widget(&code));
                    }
                    if let Some(content) = &request.tool_call.fields.content {
                        for item in content {
                            match item {
                                ToolCallContent::Diff(diff) => {
                                    self.permission_detail.append(&diff_widget(diff));
                                }
                                // A prompt names consequences, and this is
                                // where an agent puts them — dropping it on
                                // the floor left the user consenting to a
                                // title. Set as prose, under the question it
                                // qualifies.
                                ToolCallContent::Content(block) => {
                                    if let Some(text) = content_text(&block.content) {
                                        self.permission_detail.append(
                                            &gtk::Label::builder()
                                                .label(text.trim())
                                                .attributes(&no_hyphens())
                                                .wrap(true)
                                                .xalign(0.0)
                                                .lines(8)
                                                .ellipsize(gtk::pango::EllipsizeMode::End)
                                                .css_classes(["caption", "dim-label"])
                                                .build(),
                                        );
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    self.notify(crate::notify::Moment::PermissionRequested {
                        chat: self.notify_chat(),
                        detail: note.clone(),
                    });
                    // A newer request displaces an unanswered one: its
                    // dropped reply goes out as Cancelled, and that must
                    // not read as a user refusal on the agent's side.
                    let displaced = self
                        .pending_permission
                        .borrow_mut()
                        .replace((request, reply));
                    if let Some((displaced, _)) = displaced {
                        self.workspace.ide.record_permission(
                            &single_line(&permission_title(&displaced), 120),
                            "cancelled",
                            "a newer permission request arrived before the user \
                             answered this one; nobody refused it",
                        );
                    }
                    self.permission_bar.set_reveal_child(true);
                    // The row in the environment panel lights now, not at
                    // the next refresh.
                    self.note_activity();
                    // Nothing is running while this is up: the working line
                    // steps aside for the card that says what is really
                    // happening.
                    self.sync_busy_row();
                }
            }
            SessionEvent::PromptFailed { message } => {
                self.finalize_stream();
                self.stop_button.set_visible(false);
                self.set_busy(false);
                if let Some((_, on_done)) = self.capture.borrow_mut().take() {
                    on_done(String::new());
                }
                // A rejected prompt is not part of the conversation: take
                // its card out of the transcript and hand the text back.
                let rejected = self.pending_prompts.borrow_mut().pop_front();
                if let Some(prompt) = rejected {
                    self.drop_prompt_rows(&prompt);
                    if let Some(text) = prompt.restore {
                        let current = self.entry_text();
                        let combined = if current.trim().is_empty() {
                            text
                        } else {
                            format!("{text}\n\n{current}")
                        };
                        self.entry.buffer().set_text(&combined);
                    }
                }
                self.set_status(&format!("{} · prompt rejected", self.agent_name()));
                self.meta_row(&format!("prompt failed: {message}"));
            }
            SessionEvent::TurnEnded { reason, usage } => {
                use agent_client_protocol::schema::v1::StopReason;
                self.turns.set(self.turns.get() + 1);
                self.touch();
                self.finalize_stream();
                self.pending_prompts.borrow_mut().pop_front();
                // The next queued prompt (if any) starts now.
                let accepted = {
                    let mut pending = self.pending_prompts.borrow_mut();
                    pending.front_mut().and_then(|prompt| {
                        let card = prompt.card.clone();
                        prompt.queued.take().map(|queued| (card, queued))
                    })
                };
                if let Some((card, (badge, queued_at))) = accepted {
                    // The card was appended where the prompt was TYPED, which
                    // is not where it joins the conversation — a whole turn's
                    // output has landed in between. Move it to the end and
                    // leave a marker behind.
                    let origin = self.reseat_accepted_prompt(&card);
                    if let Some(prompt) = self.pending_prompts.borrow_mut().front_mut() {
                        prompt.origin = origin;
                    }
                    // The badge is rewritten, not removed: how long a prompt
                    // sat behind the previous turn is the one place that
                    // cost is ever visible, and it is worth keeping in the
                    // transcript after the fact.
                    badge.set_label(&format!("queued for {}", elapsed(queued_at)));
                }
                // What is still queued decides everything below: either the
                // next prompt starts now, or the turn is genuinely over and
                // its indicators come down with it. The working row used to
                // be left behind here — the status said ready while it went
                // on claiming otherwise next to the composer.
                let more_queued = !self.pending_prompts.borrow().is_empty();
                if !more_queued {
                    self.stop_button.set_visible(false);
                    self.set_busy(false);
                    // The environment came up (or went down) while this
                    // turn was running, and moving the process had to wait
                    // for it. Now it can.
                    if self.relocation_pending.replace(false) {
                        self.retopologize();
                    }
                }
                // The turn is over: a permission prompt from it is moot.
                // Dropping the reply answers it as Cancelled on the wire;
                // the log keeps why, and the bar comes down with the turn
                // it belonged to.
                self.clear_notification("permission");
                let abandoned = self.pending_permission.borrow_mut().take();
                if let Some((request, _)) = abandoned {
                    self.permission_bar.set_reveal_child(false);
                    self.note_activity();
                    self.workspace.ide.record_permission(
                        &single_line(&permission_title(&request), 120),
                        "cancelled",
                        "its turn ended before the user answered; nobody refused it",
                    );
                }
                if !more_queued {
                    self.notify(crate::notify::Moment::TurnEnded {
                        chat: self.notify_chat(),
                    });
                }
                // A completed turn proves auth: retire the invitation.
                self.needs_auth.set(false);
                if self.auth_box.is_visible() {
                    self.auth_box.set_visible(false);
                    self.show_options(false);
                }
                // Ordinary turn ends are silent; only surprising stops earn
                // a transcript note.
                match reason {
                    StopReason::EndTurn => {}
                    StopReason::Cancelled => self.meta_row("stopped"),
                    other => self.meta_row(&format!("turn ended early: {other:?}")),
                }
                self.set_status(&format!(
                    "{} · {}",
                    self.agent_name(),
                    if more_queued { "working…" } else { "ready" }
                ));
                if let Some(usage) = usage {
                    *self.session_usage.borrow_mut() = Some(usage.clone());
                    // Only a stand-in until an UsageUpdate lands: session
                    // totals count every turn, the context holds one
                    // conversation's worth.
                    if self.context_used.get() == 0 {
                        self.context_used.set(usage.total_tokens);
                    }
                    self.refresh_usage();
                    let limit = self.context_limit.get().max(1);
                    let fraction = (usage.total_tokens as f64 / limit as f64).min(1.0);
                    self.usage_bar.set_value(fraction);
                    self.usage_bar.set_visible(true);
                    let details = format!(
                        "{:.0}% of {} — {}",
                        fraction * 100.0,
                        if limit >= 1_000_000 { "1M" } else { "200k" },
                        format_usage(&usage)
                    );
                    self.usage_bar.set_tooltip_text(Some(&details));
                }
                if let Some((captured, on_done)) = self.capture.borrow_mut().take() {
                    on_done(captured);
                }
            }
            SessionEvent::ModeChangeFailed { mode, message } => {
                self.set_status(&format!("{} · mode unchanged", self.agent_name()));
                // Restore the checkmark to the mode that actually runs.
                if let Some(previous) = self.mode_revert.borrow_mut().take() {
                    if let Some(state) = self.last_modes.borrow_mut().as_mut() {
                        state.current_mode_id = previous;
                    }
                }
                self.sync_mode_widgets();
                let is_auto = self
                    .mode_sync
                    .borrow()
                    .as_ref()
                    .is_some_and(|c| Some(&mode) == c.auto_id.as_ref());
                // Only the adapter's own "not available in this session"
                // means a stale session blocks the mode. Anything else (a
                // dead connection, a signed-out agent) is reported as-is —
                // no misdiagnosis, no destroy-session loop.
                if is_auto && message.contains("not available in this session") {
                    // The toast's action comes back to whichever chat is
                    // SELECTED, so only the selected chat may raise it —
                    // a background tab would otherwise get a foreground
                    // conversation destroyed on its behalf.
                    if self.selected.get() {
                        self.workspace
                            .events
                            .publish(taste_core::Event::ToastAction {
                                message: "This restored session can't switch to Auto".into(),
                                label: "Destroy Old Session".into(),
                                action: "chat-destroy-session".into(),
                            });
                    } else {
                        self.meta_row(
                            "this restored session can't switch to Auto — start a new \
                             session from this chat's settings",
                        );
                    }
                } else {
                    self.meta_row(&format!("mode change failed: {message}"));
                }
            }
            SessionEvent::CommandFailed { message } => {
                self.set_status(&format!("{} · setting unchanged", self.agent_name()));
                self.meta_row(&format!("setting failed: {message}"));
            }
            SessionEvent::Closed(error) => {
                self.status_spinner.stop();
                self.status_label.set_visible(false);
                self.finalize_stream();
                self.stop_button.set_visible(false);
                self.set_busy(false);
                if let Some((_, on_done)) = self.capture.borrow_mut().take() {
                    on_done(String::new());
                }
                if self.auth_box.is_visible() {
                    self.set_status(&format!(
                        "{} · signed out — use the sign-in buttons, then send again",
                        self.agent_name()
                    ));
                } else {
                    self.set_status(&format!("{} · disconnected", self.agent_name()));
                }
                self.clear_notification("permission");
                // Error details are transcript-worthy; clean closes are not.
                if let Some(e) = error {
                    self.notify(crate::notify::Moment::AgentDisconnected {
                        chat: self.notify_chat(),
                        reason: e.to_string(),
                    });
                    self.meta_row(&format!("connection closed: {e}"));
                }
                // Unfinished prompts go back to the composer, not the log.
                let pending: Vec<PendingPrompt> =
                    self.pending_prompts.borrow_mut().drain(..).collect();
                let mut restored: Vec<String> = Vec::new();
                for prompt in pending {
                    self.drop_prompt_rows(&prompt);
                    if let Some(text) = prompt.restore {
                        restored.push(text);
                    }
                }
                if !restored.is_empty() {
                    let current = self.entry_text();
                    if !current.trim().is_empty() {
                        restored.push(current);
                    }
                    self.entry.buffer().set_text(&restored.join("\n\n"));
                }
                // Captured before reset_session, which clears it.
                let resume = self
                    .session_info
                    .borrow()
                    .as_ref()
                    .map(|(_, session)| session.clone());
                // Disconnect is not a different agent: keep the controls
                // on screen, just disabled.
                self.reset_session(false);
                self.schedule_reconnect(resume);
            }
        }
    }

    /// Bring a dead agent back, restoring the conversation behind it.
    ///
    /// The process is mortal and the conversation is not: the session id
    /// outlives the agent (persisted in `taste_core::state`), the history
    /// lives with the agent, and `session/load` reassembles them. Without
    /// this the pane just goes quiet — survivable when an agent crashes
    /// once, but not when a devcontainer rebuild is the ordinary way to
    /// end one.
    ///
    /// Deliberately does nothing when:
    /// - there is no session id — the user ended the session, or switched
    ///   agents, and both clear it. Silence is the correct answer to a
    ///   disconnect the user asked for.
    /// - sign-in is required — reconnecting cannot fix that, and retrying
    ///   would bury the sign-in buttons under a spinner.
    /// - the budget is spent — an agent that will not start must say so
    ///   once, not forever.
    fn schedule_reconnect(self: &Rc<Self>, resume: Option<String>) {
        const MAX_ATTEMPTS: u32 = 3;
        let Some(session_id) = resume else { return };
        if self.needs_auth.get() {
            return;
        }
        // A rebuild is the ordinary reason an agent dies, and a relocated
        // agent dies with its container by construction. Coming back before
        // that container does would spawn outside-confined and then again
        // when it arrives; `on_environment_state` brings the chat back
        // exactly once, when the environment settles. Waiting is not a
        // timer — it is the environment telling us where the agent goes.
        if self.environment_in_transition() {
            self.set_status(&format!(
                "{} · waiting for its environment…",
                self.agent_name()
            ));
            return;
        }
        let attempt = self.reconnect_attempts.get() + 1;
        if attempt > MAX_ATTEMPTS {
            self.set_status(&format!(
                "{} · disconnected — send a message to try again",
                self.agent_name()
            ));
            return;
        }
        self.reconnect_attempts.set(attempt);
        // Backing off matters more than being quick: the common cause is a
        // devcontainer rebuild, and the agent has nowhere to land until it
        // finishes.
        let delay = std::time::Duration::from_secs(match attempt {
            1 => 2,
            2 => 5,
            _ => 15,
        });
        self.set_status(&format!(
            "{} · reconnecting… ({attempt}/{MAX_ATTEMPTS})",
            self.agent_name()
        ));
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(delay, move || {
            let Some(pane) = weak.upgrade() else { return };
            // Something started a session while we waited (the user sent a
            // prompt, or switched agents): leave it alone.
            if pane.client.borrow().is_some() {
                return;
            }
            pane.ensure_client(Some(session_id));
        });
    }

    /// Sign-in required: one button per method the agent offers.
    fn show_auth(self: &Rc<Self>, methods: Vec<AuthMethod>) {
        self.notify(crate::notify::Moment::SignInRequired {
            chat: self.notify_chat(),
        });
        self.status_spinner.stop();
        self.needs_auth.set(true);
        self.set_status(&format!("{} · sign-in required", self.agent_name()));
        clear_children(&self.auth_box);
        let label = gtk::Label::builder()
            .label("This agent requires sign-in:")
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .build();
        self.auth_box.append(&label);
        let agents = builtin_agents();
        let index = (self.agent_picker.selected() as usize).min(agents.len() - 1);
        let spec = agents[index].clone();
        for method in methods {
            let name = if method.name().is_empty() {
                format!("{:?}", method.id())
            } else {
                method.name().to_string()
            };
            let button = gtk::Button::with_label(&name);
            if let Some(description) = method.description() {
                button.set_tooltip_text(Some(description));
            }
            let weak = Rc::downgrade(self);
            match method {
                // Terminal methods: the client runs the agent's login TUI
                // in a console tab (same execution context and HOME as the
                // agent, so the credentials land where it reads them).
                AuthMethod::Terminal(terminal) => {
                    let spec = spec.clone();
                    button.connect_clicked(move |_| {
                        let Some(pane) = weak.upgrade() else { return };
                        // The login must run in the SAME confinement as
                        // the agent (same home, same container) or the
                        // credentials land where the agent never looks.
                        let extra_env: Vec<(String, String)> = terminal
                            .env
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        // Sign-in is outside-confined always, even for a
                        // chat whose agent is relocated: it writes to the
                        // agent's home, and the home is the same volume in
                        // both topologies, so the credentials land where
                        // the agent reads them either way.
                        let aim = pane.aim();
                        match taste_acp::login_command(
                            &spec,
                            &aim.cwd,
                            &aim.home_volume,
                            &terminal.args,
                            &extra_env,
                        ) {
                            Ok(login) => {
                                pane.workspace
                                    .events
                                    .publish(taste_core::Event::RunInTerminal {
                                        title: "Sign In".into(),
                                        program: login.program,
                                        args: login.args,
                                        env: login.env,
                                        wrapped: true,
                                    });
                                pane.set_status(&format!(
                                    "{} · finish signing in below, then send your prompt again",
                                    pane.agent_name()
                                ));
                            }
                            Err(e) => {
                                pane.meta_row(&format!("sign-in launch refused: {e}"));
                            }
                        }
                    });
                }
                other => {
                    let method_id = other.id().clone();
                    button.connect_clicked(move |_| {
                        let Some(pane) = weak.upgrade() else { return };
                        let result = match pane.client.borrow().as_ref() {
                            Some(client) => client.authenticate(method_id.clone()),
                            None => Ok(()),
                        };
                        match result {
                            Ok(()) => {
                                pane.set_status(&format!("{} · authenticating…", pane.agent_name()))
                            }
                            Err(e) => pane.meta_row(&format!("error: {e}")),
                        }
                        pane.auth_box.set_visible(false);
                    });
                }
            }
            self.auth_box.append(&button);
        }
        self.auth_box.set_visible(true);
        // The sign-in invitation must be seen, not discovered: open the
        // options shade for it.
        self.show_options(true);
    }

    /// Render the agent's control surface: its permission modes and its
    /// config options (model selection arrives as a select option).
    fn build_controls(
        self: &Rc<Self>,
        modes: Option<SessionModeState>,
        config_options: Vec<SessionConfigOption>,
    ) {
        *self.controls_signature.borrow_mut() =
            Some(Self::options_signature(&modes, &config_options));
        self.controls.set_sensitive(true);
        clear_children(&self.controls);
        self.mode_sync.borrow_mut().take();
        let has_modes_row = modes
            .as_ref()
            .is_some_and(|m| !m.available_modes.is_empty());

        if let Some(state) = modes {
            if !state.available_modes.is_empty() {
                self.build_permission_controls(&state);
            }
        }

        for option in config_options {
            let is_mode = option.id.to_string().eq_ignore_ascii_case("mode")
                || option.name.eq_ignore_ascii_case("mode");
            // The permission mode's other channel: recorded even when the
            // modes state renders it, because a later session may arrive
            // with only this one (see `apply_preferred_mode`).
            if is_mode {
                if let SessionConfigKind::Select(select) = &option.kind {
                    let values: Vec<String> = match &select.options {
                        SessionConfigSelectOptions::Ungrouped(options) => {
                            options.iter().map(|o| o.value.to_string()).collect()
                        }
                        SessionConfigSelectOptions::Grouped(groups) => groups
                            .iter()
                            .flat_map(|g| &g.options)
                            .map(|o| o.value.to_string())
                            .collect(),
                        _ => Vec::new(),
                    };
                    if !values.is_empty() {
                        *self.mode_config.borrow_mut() = Some((option.id.clone(), values));
                    }
                }
            }
            // Some agents expose their session mode BOTH as modes state and
            // as a "mode" config option; one "Permissions" group is enough.
            if has_modes_row && is_mode {
                continue;
            }
            match &option.kind {
                SessionConfigKind::Boolean(boolean) => {
                    let row = self.append_switch_row(&option.name, boolean.current_value);
                    let weak = Rc::downgrade(self);
                    let config_id = option.id.clone();
                    row.connect_active_notify(move |row| {
                        let Some(pane) = weak.upgrade() else { return };
                        if pane.syncing.get() {
                            return;
                        }
                        let result = match pane.client.borrow().as_ref() {
                            Some(client) => {
                                client.set_config_bool(config_id.clone(), row.is_active())
                            }
                            None => Ok(()),
                        };
                        if let Err(e) = result {
                            pane.meta_row(&format!("error: {e}"));
                        }
                    });
                }
                SessionConfigKind::Select(select) => {
                    let choices: Vec<(String, String)> = match &select.options {
                        SessionConfigSelectOptions::Ungrouped(options) => options
                            .iter()
                            .map(|o| (o.value.to_string(), o.name.clone()))
                            .collect(),
                        SessionConfigSelectOptions::Grouped(groups) => groups
                            .iter()
                            .flat_map(|g| &g.options)
                            .map(|o| (o.value.to_string(), o.name.clone()))
                            .collect(),
                        _ => Vec::new(),
                    };
                    if choices.is_empty() {
                        continue;
                    }
                    let current_value = select.current_value.to_string();
                    // On/off selects ARE switches, whatever the wire says.
                    if let Some((on_value, off_value)) = switch_values(&choices) {
                        let row = self.append_switch_row(&option.name, current_value == on_value);
                        let weak = Rc::downgrade(self);
                        let config_id = option.id.clone();
                        row.connect_active_notify(move |row| {
                            let Some(pane) = weak.upgrade() else { return };
                            if pane.syncing.get() {
                                return;
                            }
                            let value = if row.is_active() {
                                on_value.clone()
                            } else {
                                off_value.clone()
                            };
                            let result = match pane.client.borrow().as_ref() {
                                Some(client) => {
                                    client.set_config_option(config_id.clone(), value.into())
                                }
                                None => Ok(()),
                            };
                            if let Err(e) = result {
                                pane.meta_row(&format!("error: {e}"));
                            }
                        });
                        continue;
                    }
                    if option.id.to_string().eq_ignore_ascii_case("model") {
                        self.build_model_controls(&option, &choices, &current_value);
                        continue;
                    }
                    let names: Vec<String> = choices.iter().map(|(_, name)| name.clone()).collect();
                    let current = choices
                        .iter()
                        .position(|(value, _)| *value == current_value)
                        .unwrap_or(0);
                    // Ordered sets read best as a compact slider.
                    if option.id.to_string().eq_ignore_ascii_case("effort") {
                        let scale = self.append_slider_card(&option.name, &names, current);
                        let weak = Rc::downgrade(self);
                        let config_id = option.id.clone();
                        let values: Vec<String> = choices.iter().map(|(v, _)| v.clone()).collect();
                        scale.connect_value_changed(move |scale| {
                            let Some(pane) = weak.upgrade() else { return };
                            if pane.syncing.get() {
                                return;
                            }
                            let index = scale.value().round() as usize;
                            let Some(value) = values.get(index) else {
                                return;
                            };
                            let result = match pane.client.borrow().as_ref() {
                                Some(client) => client
                                    .set_config_option(config_id.clone(), value.clone().into()),
                                None => Ok(()),
                            };
                            if let Err(e) = result {
                                pane.meta_row(&format!("error: {e}"));
                            }
                        });
                        continue;
                    }
                    let checks = self.append_radio_group(&option.name, &names, current);
                    for (index, check) in checks.iter().enumerate() {
                        let weak = Rc::downgrade(self);
                        let config_id = option.id.clone();
                        let value = choices[index].0.clone();
                        check.connect_toggled(move |check| {
                            let Some(pane) = weak.upgrade() else { return };
                            if pane.syncing.get() || !check.is_active() {
                                return;
                            }
                            let result = match pane.client.borrow().as_ref() {
                                Some(client) => client
                                    .set_config_option(config_id.clone(), value.clone().into()),
                                None => Ok(()),
                            };
                            match result {
                                // When the permission mode lives here rather
                                // than in a modes state, this is where the
                                // user makes their choice — remember it.
                                Ok(()) if is_mode => pane.remember_mode(&value),
                                Ok(()) => {}
                                Err(e) => pane.meta_row(&format!("error: {e}")),
                            }
                        });
                    }
                }
                _ => {}
            }
        }
    }

    /// Structure signature: option ids + their value sets. Equal signature
    /// = same widgets suffice; only values moved.
    fn options_signature(
        modes: &Option<SessionModeState>,
        options: &[SessionConfigOption],
    ) -> ControlsSignature {
        let mut signature = Vec::new();
        if let Some(state) = modes {
            signature.push((
                "__modes__".to_string(),
                state
                    .available_modes
                    .iter()
                    .map(|m| m.id.to_string())
                    .collect(),
            ));
        }
        for option in options {
            let values = match &option.kind {
                SessionConfigKind::Boolean(_) => vec!["<bool>".to_string()],
                SessionConfigKind::Select(select) => match &select.options {
                    SessionConfigSelectOptions::Ungrouped(o) => {
                        o.iter().map(|v| v.value.to_string()).collect()
                    }
                    SessionConfigSelectOptions::Grouped(groups) => groups
                        .iter()
                        .flat_map(|g| &g.options)
                        .map(|v| v.value.to_string())
                        .collect(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            signature.push((option.id.to_string(), values));
        }
        signature
    }

    /// The permission mode this chat wants: the user's remembered choice,
    /// else [`DEFAULT_PERMISSION_MODE`].
    fn preferred_mode(&self) -> String {
        self.permission_mode
            .borrow()
            .clone()
            .unwrap_or_else(|| DEFAULT_PERMISSION_MODE.to_string())
    }

    /// Put the freshly-ready session into this chat's permission mode.
    ///
    /// Two channels, because agents differ: the session-modes state
    /// (`session/set_mode`) when one was advertised, and the "mode" config
    /// option otherwise — a session restored through `session/load` often
    /// comes back with no modes state at all, which is precisely when the
    /// old code gave up and left the user in "ask me everything".
    fn apply_preferred_mode(self: &Rc<Self>) {
        if self.needs_auth.get() {
            return; // nothing runs until they are signed in
        }
        let want = self.preferred_mode();
        // 1. The modes state, when the agent has one.
        let target = self.mode_sync.borrow().as_ref().and_then(|controls| {
            controls
                .ids
                .iter()
                .find(|id| id.to_string().eq_ignore_ascii_case(&want))
                .or(controls.auto_id.as_ref())
                .cloned()
        });
        if let Some(target) = target {
            let current = self
                .last_modes
                .borrow()
                .as_ref()
                .map(|m| m.current_mode_id.clone());
            if current.as_ref() == Some(&target) {
                return; // already there
            }
            let result = match self.client.borrow().as_ref() {
                Some(client) => client.set_mode(target.clone()),
                None => return,
            };
            if let Err(e) = result {
                tracing::warn!("applying permission mode failed: {e}");
                return;
            }
            // Optimistic, WITH the revert memo the old code forgot: a
            // refused change must put the dropdown back on the mode that
            // actually runs rather than leaving a comfortable lie on screen.
            *self.mode_revert.borrow_mut() = current;
            if let Some(state) = self.last_modes.borrow_mut().as_mut() {
                state.current_mode_id = target;
            }
            self.sync_mode_widgets();
            return;
        }
        // 2. The "mode" config option, for agents that carry it there.
        let config = self.mode_config.borrow().clone();
        let Some((config_id, values)) = config else {
            return;
        };
        let Some(value) = values
            .iter()
            .find(|v| v.eq_ignore_ascii_case(&want))
            .or_else(|| {
                values
                    .iter()
                    .find(|v| v.eq_ignore_ascii_case(DEFAULT_PERMISSION_MODE))
            })
        else {
            return;
        };
        if let Some(client) = self.client.borrow().as_ref() {
            if let Err(e) = client.set_config_option(config_id, value.clone().into()) {
                tracing::warn!("applying permission mode failed: {e}");
            }
        }
    }

    /// Remember the user's permission-mode choice for this chat and write
    /// it to the tab list.
    fn remember_mode(&self, id: &str) {
        let mut current = self.permission_mode.borrow_mut();
        if current.as_deref() == Some(id) {
            return;
        }
        *current = Some(id.to_string());
        drop(current);
        self.notify_persist();
    }

    /// Reflect `last_modes` into the mode list's checkmarks in place.
    fn sync_mode_widgets(&self) {
        let modes = self.last_modes.borrow();
        let Some(state) = modes.as_ref() else { return };
        let controls = self.mode_sync.borrow();
        let Some(controls) = controls.as_ref() else {
            return;
        };
        if let Some(index) = controls
            .ids
            .iter()
            .position(|id| *id == state.current_mode_id)
        {
            self.syncing.set(true);
            controls.dropdown.set_selected(index as u32);
            self.syncing.set(false);
        }
    }

    /// A compact ordered-choice slider in a card: title, live value label,
    /// and a snapping scale with one stop per option.
    fn append_slider_card(&self, title: &str, names: &[String], selected: usize) -> gtk::Scale {
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.set_margin_top(8);
        header.set_margin_start(12);
        header.set_margin_end(12);
        let heading = gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["caption-heading", "dim-label"])
            .build();
        let value_label = gtk::Label::builder()
            .label(names.get(selected).map(String::as_str).unwrap_or(""))
            .css_classes(["caption"])
            .build();
        header.append(&heading);
        header.append(&value_label);
        let scale = gtk::Scale::with_range(
            gtk::Orientation::Horizontal,
            0.0,
            (names.len().saturating_sub(1)) as f64,
            1.0,
        );
        scale.set_round_digits(0);
        scale.set_draw_value(false);
        scale.set_hexpand(true);
        scale.set_margin_start(6);
        scale.set_margin_end(6);
        scale.set_margin_bottom(4);
        for index in 0..names.len() {
            scale.add_mark(index as f64, gtk::PositionType::Bottom, None);
        }
        self.syncing.set(true);
        scale.set_value(selected as f64);
        self.syncing.set(false);
        {
            let value_label = value_label.clone();
            let names = names.to_vec();
            scale.connect_value_changed(move |scale| {
                let index = scale.value().round() as usize;
                if let Some(name) = names.get(index) {
                    value_label.set_label(name);
                }
            });
        }
        let card = gtk::Box::new(gtk::Orientation::Vertical, 2);
        card.add_css_class("card");
        card.append(&header);
        card.append(&scale);
        self.controls.append(&card);
        scale
    }

    /// Modes: the agent's permission modes as a plain selection list —
    /// name, description, and a checkmark on the one that runs. One click
    /// to any mode; a refused switch reverts with a toast, never a dialog.
    fn build_permission_controls(self: &Rc<Self>, state: &SessionModeState) {
        let auto_id = state
            .available_modes
            .iter()
            .find(|m| m.id.to_string().eq_ignore_ascii_case("auto"))
            .map(|m| m.id.clone());

        // A dropdown, not a list: modes must not push the actual calls to
        // action (sign-in, model, effort) below the fold.
        let ids: Vec<SessionModeId> = state.available_modes.iter().map(|m| m.id.clone()).collect();
        let names: Vec<String> = state
            .available_modes
            .iter()
            .map(|m| m.name.clone())
            .collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        // No title row, no subtitle: at chat-pane widths a labeled row
        // ellipsized the value into "A…". The names speak for themselves;
        // the description becomes the tooltip.
        let dropdown = gtk::DropDown::builder()
            .model(&gtk::StringList::new(&name_refs))
            .hexpand(true)
            .build();
        if let Some(index) = ids.iter().position(|id| *id == state.current_mode_id) {
            self.syncing.set(true);
            dropdown.set_selected(index as u32);
            self.syncing.set(false);
        }
        let descriptions: Vec<Option<String>> = state
            .available_modes
            .iter()
            .map(|m| m.description.clone())
            .collect();
        if let Some(description) = descriptions
            .get(dropdown.selected() as usize)
            .and_then(|d| d.as_deref())
        {
            dropdown.set_tooltip_text(Some(description));
        }
        {
            let weak = Rc::downgrade(self);
            let ids = ids.clone();
            let descriptions = descriptions.clone();
            dropdown.connect_selected_notify(move |dropdown| {
                let Some(pane) = weak.upgrade() else { return };
                let index = dropdown.selected() as usize;
                dropdown.set_tooltip_text(descriptions.get(index).and_then(|d| d.as_deref()));
                if pane.syncing.get() {
                    return;
                }
                let Some(id) = ids.get(index).cloned() else {
                    return;
                };
                let previous = pane
                    .last_modes
                    .borrow()
                    .as_ref()
                    .map(|s| s.current_mode_id.clone());
                if previous.as_ref() == Some(&id) {
                    return;
                }
                let result = match pane.client.borrow().as_ref() {
                    Some(client) => client.set_mode(id.clone()),
                    None => return,
                };
                match result {
                    Ok(()) => {
                        *pane.mode_revert.borrow_mut() = previous;
                        // The user's own pick: this chat runs in it from
                        // now on, this launch and the next.
                        pane.remember_mode(&id.to_string());
                        if let Some(state) = pane.last_modes.borrow_mut().as_mut() {
                            state.current_mode_id = id;
                        }
                    }
                    Err(e) => pane.meta_row(&format!("error: {e}")),
                }
            });
        }
        self.controls.append(&dropdown);
        *self.mode_sync.borrow_mut() = Some(ModeControls {
            dropdown,
            ids,
            auto_id,
        });
    }

    /// Model, compact: a worst→best quality slider over base models, plus
    /// an expanded-context toggle (on by default) where a `x[1m]` variant
    /// exists.
    fn build_model_controls(
        self: &Rc<Self>,
        option: &SessionConfigOption,
        choices: &[(String, String)],
        current_value: &str,
    ) {
        // This chat's own choice, not the project's: tabs carry their model
        // with them, and a new tab inherits it from the one it was opened
        // beside (`inherit_settings`).
        let persisted = self.model_value.borrow().clone();
        // Group `base` / `base[1m]` pairs into stops; the agent's "default"
        // alias is not a stop — it is what runs until the user picks.
        let mut stops: Vec<(String, ModelStop)> = Vec::new();
        for (value, name) in choices {
            if value.eq_ignore_ascii_case("default") {
                continue;
            }
            let (base, expanded) = match value.find('[') {
                Some(i) => (value[..i].to_string(), true),
                None => (value.clone(), false),
            };
            let stop = match stops.iter_mut().find(|(b, _)| *b == base) {
                Some((_, stop)) => stop,
                None => {
                    stops.push((
                        base.clone(),
                        ModelStop {
                            name: name.clone(),
                            normal: None,
                            expanded: None,
                        },
                    ));
                    &mut stops.last_mut().unwrap().1
                }
            };
            if expanded {
                stop.expanded = Some(value.clone());
            } else {
                stop.normal = Some(value.clone());
                stop.name = name.clone();
            }
        }
        stops.sort_by_key(|(base, _)| model_rank(base));
        let names: Vec<String> = stops
            .iter()
            .map(|(_, stop)| {
                stop.name
                    .split(" (")
                    .next()
                    .unwrap_or(&stop.name)
                    .to_string()
            })
            .collect();
        // Effective value: the project's persisted choice wins; otherwise
        // whatever the agent reports (its default).
        let effective = persisted
            .clone()
            .unwrap_or_else(|| current_value.to_string());
        self.context_limit.set(if effective.contains("[1m]") {
            1_000_000
        } else {
            200_000
        });
        let using_default = persisted.is_none();
        let current_stop = stops
            .iter()
            .position(|(_, stop)| {
                stop.normal.as_deref() == Some(effective.as_str())
                    || stop.expanded.as_deref() == Some(effective.as_str())
            })
            .unwrap_or(names.len().saturating_sub(1));

        let scale = self.append_slider_card(&option.name, &names, current_stop);
        if using_default {
            // No explicit pick yet: the agent's recommended default runs.
            if let Some(card) = scale.parent() {
                card.set_tooltip_text(Some(
                    "Using the agent's recommended default; move to choose explicitly",
                ));
            }
        }
        // Re-apply the project's persisted choice to the fresh session.
        if let Some(saved) = &persisted {
            if saved != current_value {
                let result = match self.client.borrow().as_ref() {
                    Some(client) => {
                        client.set_config_option(option.id.clone(), saved.clone().into())
                    }
                    None => Ok(()),
                };
                if let Err(e) = result {
                    self.meta_row(&format!("error: {e}"));
                }
            }
        }
        let expanded_row = adw::SwitchRow::builder()
            .title("Expanded Context Window")
            .subtitle("Use the model's larger token window when available")
            .build();
        let expanded_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        expanded_list.append(&expanded_row);
        self.controls.append(&expanded_list);

        let stops = Rc::new(stops);
        let sync_expanded = {
            let stops = stops.clone();
            let expanded_list = expanded_list.clone();
            let expanded_row = expanded_row.clone();
            move |index: usize, current: Option<&str>| {
                let Some((_, stop)) = stops.get(index) else {
                    return;
                };
                let has_both = stop.normal.is_some() && stop.expanded.is_some();
                let only_expanded = stop.normal.is_none() && stop.expanded.is_some();
                // Always visible; disabled when there is no choice to make.
                let _ = &expanded_list;
                expanded_row.set_sensitive(has_both);
                expanded_row.set_subtitle(if only_expanded {
                    "This model always uses its expanded window"
                } else if stop.expanded.is_none() {
                    "Not available for this model"
                } else {
                    "Use the model's larger token window when available"
                });
                let active = if stop.expanded.is_none() {
                    false
                } else {
                    only_expanded
                        || match current {
                            Some(v) => stop.expanded.as_deref() == Some(v),
                            None => true, // on by default
                        }
                };
                expanded_row.set_active(active);
            }
        };
        self.syncing.set(true);
        sync_expanded(current_stop, Some(current_value));
        self.syncing.set(false);

        let send_model = {
            let stops = stops.clone();
            let weak = Rc::downgrade(self);
            let config_id = option.id.clone();
            let expanded_row = expanded_row.clone();
            move |index: usize| {
                let Some(pane) = weak.upgrade() else { return };
                let Some((_, stop)) = stops.get(index) else {
                    return;
                };
                let value = if expanded_row.is_active() {
                    stop.expanded.clone().or_else(|| stop.normal.clone())
                } else {
                    stop.normal.clone().or_else(|| stop.expanded.clone())
                };
                let result = match (value.clone(), pane.client.borrow().as_ref()) {
                    (Some(value), Some(client)) => {
                        client.set_config_option(config_id.clone(), value.into())
                    }
                    _ => Ok(()),
                };
                match result {
                    Ok(()) => {
                        // Per-chat persistence: this choice survives
                        // restarts and re-applies to this tab's future
                        // sessions.
                        if let Some(value) = value {
                            pane.context_limit.set(if value.contains("[1m]") {
                                1_000_000
                            } else {
                                200_000
                            });
                            *pane.model_value.borrow_mut() = Some(value);
                            pane.notify_persist();
                        }
                    }
                    Err(e) => pane.meta_row(&format!("error: {e}")),
                }
            }
        };
        {
            let weak = Rc::downgrade(self);
            let send_model = send_model.clone();
            let sync_expanded = sync_expanded.clone();
            scale.connect_value_changed(move |scale| {
                let Some(pane) = weak.upgrade() else { return };
                let index = scale.value().round() as usize;
                pane.syncing.set(true);
                sync_expanded(index, None);
                pane.syncing.set(false);
                if pane.syncing.get() {
                    return;
                }
                send_model(index);
            });
        }
        {
            let weak = Rc::downgrade(self);
            let scale = scale.clone();
            expanded_row.connect_active_notify(move |_| {
                let Some(pane) = weak.upgrade() else { return };
                if pane.syncing.get() {
                    return;
                }
                send_model(scale.value().round() as usize);
            });
        }
    }

    /// An expanded exclusive-choice group: every option visible, one click
    /// away, radio-selected — no dropdown between the user and the choice.
    fn append_radio_group(
        &self,
        title: &str,
        names: &[String],
        selected: usize,
    ) -> Vec<gtk::CheckButton> {
        let heading = gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .css_classes(["dim-label", "caption-heading"])
            .margin_start(8)
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let mut checks: Vec<gtk::CheckButton> = Vec::new();
        self.syncing.set(true);
        for (index, name) in names.iter().enumerate() {
            let check = gtk::CheckButton::new();
            if let Some(first) = checks.first() {
                check.set_group(Some(first));
            }
            check.set_active(index == selected);
            let row = adw::ActionRow::builder()
                .title(name)
                .activatable(true)
                .build();
            row.add_prefix(&check);
            row.set_activatable_widget(Some(&check));
            list.append(&row);
            checks.push(check);
        }
        self.syncing.set(false);
        self.controls.append(&heading);
        self.controls.append(&list);
        checks
    }

    /// A boolean option as the switch row it always should be.
    fn append_switch_row(&self, title: &str, active: bool) -> adw::SwitchRow {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let row = adw::SwitchRow::builder().title(title).build();
        self.syncing.set(true);
        row.set_active(active);
        self.syncing.set(false);
        list.append(&row);
        self.controls.append(&list);
        row
    }

    fn render_update(self: &Rc<Self>, update: SessionUpdate) {
        // Anything that is not another piece of the user's message ends it.
        if !matches!(update, SessionUpdate::UserMessageChunk(_)) {
            self.flush_user_message();
        }
        match update {
            SessionUpdate::UserMessageChunk(chunk) => {
                // Replayed history (session/load) and echoed prompts arrive
                // as chunks — ONE PER CONTENT BLOCK. Collect the whole
                // message before drawing it: rendering each chunk on its own
                // both split multi-part prompts across cards and dropped
                // every non-text block, which is why restored conversations
                // came back without the images they were sent with.
                let mut pending = self.pending_user.borrow_mut();
                let pending = pending.get_or_insert_with(PendingUserMessage::default);
                match content_text(&chunk.content) {
                    Some(text) => {
                        if !pending.text.is_empty() && !pending.text.ends_with('\n') {
                            pending.text.push('\n');
                        }
                        pending.text.push_str(&text);
                    }
                    None => {
                        if let Some(attachment) = replayed_attachment(&chunk.content) {
                            pending.attachments.push(attachment);
                        }
                    }
                }
            }
            SessionUpdate::AgentMessageChunk(chunk) => {
                if let Some(text) = content_text(&chunk.content) {
                    if let Some((captured, _)) = self.capture.borrow_mut().as_mut() {
                        captured.push_str(&text);
                    }
                    let buffer = self.agent_buffer();
                    let mut end = buffer.end_iter();
                    buffer.insert(&mut end, &text);
                }
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                if let Some(text) = content_text(&chunk.content) {
                    let buffer = self.thought_buffer();
                    let mut end = buffer.end_iter();
                    buffer.insert(&mut end, &text);
                }
            }
            SessionUpdate::ToolCall(call) => {
                // Close the streaming text block first: narration after
                // this call must start a NEW block below the card, so the
                // transcript reads as interleaved progress (text, tool,
                // text…), not one pre-tool blob. Updates to an existing
                // card deliberately don't finalize — they arrive while
                // later text is already streaming.
                self.finalize_stream();
                self.upsert_tool_card(
                    call.tool_call_id.to_string(),
                    Some(call.title.clone()),
                    Some(call.status),
                    Some(call.kind),
                    Some(&call.content),
                );
            }
            SessionUpdate::ToolCallUpdate(update) => {
                self.upsert_tool_card(
                    update.tool_call_id.to_string(),
                    update.fields.title.clone(),
                    update.fields.status,
                    update.fields.kind,
                    update.fields.content.as_deref(),
                );
            }
            SessionUpdate::Plan(plan) => {
                self.finalize_stream();
                self.plan_card(&plan);
            }
            SessionUpdate::UsageUpdate(update) => {
                // The agent's own account of the context window. We used to
                // sniff the size out of the model name and never knew the
                // fill at all.
                self.context_used.set(update.used);
                if update.size > 0 {
                    self.context_limit.set(update.size);
                }
                if let Some(cost) = update.cost {
                    *self.session_cost.borrow_mut() = Some((cost.amount, cost.currency));
                }
                let limit = self.context_limit.get().max(1);
                let fraction = (update.used as f64 / limit as f64).min(1.0);
                self.usage_bar.set_value(fraction);
                self.usage_bar.set_visible(true);
                self.refresh_usage();
            }
            SessionUpdate::AvailableCommandsUpdate(update) => {
                self.command_provider.set_commands(
                    update
                        .available_commands
                        .into_iter()
                        .map(|command| crate::command_completion::Command {
                            name: command.name,
                            description: command.description,
                        })
                        .collect(),
                );
            }
            SessionUpdate::CurrentModeUpdate(update) => {
                // The agent (or our own change echoed) settled on a mode:
                // it is authoritative, so the revert memo is obsolete.
                self.mode_revert.borrow_mut().take();
                if let Some(state) = self.last_modes.borrow_mut().as_mut() {
                    state.current_mode_id = update.current_mode_id.clone();
                }
                self.sync_mode_widgets();
            }
            SessionUpdate::ConfigOptionUpdate(update) => {
                // The agent revised its config surface (model list included)
                // mid-session: re-render live. Its "mode" option is the
                // authoritative current mode — absorb it first, or the
                // rebuild would revert a just-made switch.
                if let Some(state) = self.last_modes.borrow_mut().as_mut() {
                    if let Some(option) = update
                        .config_options
                        .iter()
                        .find(|o| o.id.to_string().eq_ignore_ascii_case("mode"))
                    {
                        if let SessionConfigKind::Select(select) = &option.kind {
                            let current = select.current_value.to_string();
                            if let Some(mode) = state
                                .available_modes
                                .iter()
                                .find(|m| m.id.to_string() == current)
                            {
                                state.current_mode_id = mode.id.clone();
                            }
                        }
                    }
                }
                let modes = self.last_modes.borrow().clone();
                let signature = Self::options_signature(&modes, &update.config_options);
                if self.controls_signature.borrow().as_ref() == Some(&signature) {
                    // Value-only echo (usually our own change coming back):
                    // sync in place; never rebuild — a rebuild rips the
                    // slider out from under a drag and jolts the layout.
                    self.sync_mode_widgets();
                    return;
                }
                self.build_controls(modes, update.config_options);
            }
            _ => {}
        }
    }

    // --- GNOME notifications: the AI needs the user -----------------------
    // The policy is in `crate::notify` and is pure; what is left here is
    // the gio call and the two facts only a live pane knows — whether the
    // window has focus, and whether this is the tab on screen. A prompt
    // waiting in a BACKGROUND chat notifies even with the window focused:
    // the user cannot see a tab they are not on.
    //
    // Every notification is withdrawn the moment it stops requiring a
    // response (answered permission, finished sign-in, turn seen).
    // Informational ones also clear when the window regains focus — the
    // window drives that through `ChatTabs::withdraw_informational`.

    /// This chat, as a notification's subject.
    fn notify_chat(&self) -> crate::notify::Chat {
        // The agent's name, and — for a chat with a world of its own —
        // which one, because "Claude needs permission" with three of them
        // running tells the user nothing they can act on.
        let label = if self.environment.is_primary() {
            self.agent_name()
        } else {
            format!("{} · {}", self.agent_name(), self.environment)
        };
        crate::notify::Chat {
            key: self.notify_key.clone(),
            label,
        }
    }

    /// Whether a notification click naming `key` belongs to this pane.
    /// The key itself is never handed out — asking is the only use for it,
    /// and a key in circulation is a key something can store and stale.
    pub fn answers_to(&self, key: &str) -> bool {
        self.notify_key == key
    }

    fn notify(&self, moment: crate::notify::Moment) {
        let Some(window) = self.widget.root().and_downcast::<gtk::Window>() else {
            return;
        };
        let Some(app) = window.application() else {
            return;
        };
        let attention = crate::notify::Attention {
            window_active: window.is_active(),
            chat_on_screen: self.selected.get(),
            ..Default::default()
        };
        let Some(notice) = crate::notify::decide(&moment, &attention) else {
            return;
        };
        crate::notify::send(&app, &notice);
    }

    /// Withdraw one of this chat's notifications by kind (`"permission"`,
    /// `"turn"`, `"disconnect"`) — the same scoping [`crate::notify`]
    /// builds the ids with.
    fn clear_notification(&self, kind: &str) {
        if let Some(app) = self
            .widget
            .root()
            .and_downcast::<gtk::Window>()
            .and_then(|w| w.application())
        {
            app.withdraw_notification(&format!("taste-{kind}-{}", self.notify_key));
        }
    }

    /// The user is back at the window: retire this chat's notifications
    /// that were only ever telling them to come back. A waiting permission
    /// prompt is not one of them — it is still waiting.
    pub fn withdraw_informational(&self) {
        self.clear_notification("turn");
        self.clear_notification("disconnect");
    }

    /// TASTE_PROBE_CHECK only: designate this chat, so a headless
    /// screenshot shows the tab glyph and the switch — without the
    /// strip's environment creation, an MCP server, or a respawn.
    #[doc(hidden)]
    pub fn seed_orchestrator_for_probe(&self, open_options: bool) {
        self.orchestrator.set(true);
        self.sync_orchestrator_row();
        if open_options {
            self.show_options(true);
        }
    }

    /// TASTE_PROBE_CHECK only: sample text in the composer, so a headless
    /// screenshot shows its text colors and not just an empty box.
    #[doc(hidden)]
    /// Probe harness: a transcript with one of everything, so a headless
    /// screenshot exercises the REAL surfaces rather than an empty list.
    ///
    /// It still replays the sequence that used to leave a stale checklist
    /// under a fresh prompt — plan, prompt, the SAME plan again — so
    /// `ide_widget_geometry` on "chat" can be counted for "plan-card" (one
    /// card, not two). Everything after that exists to be looked at: a
    /// finished thought, streamed markdown, a diff card, a shell card whose
    /// output carries ANSI, an in-flight card, a failed card, the permission
    /// banner and a pair of composer chips.
    pub fn seed_transcript_for_probe(self: &Rc<Self>) {
        // `TASTE_PROBE_CHAT=empty` leaves the transcript alone, so the other
        // face of the pane — the empty page, and the composer wearing the
        // focus ring — can be looked at too.
        if std::env::var("TASTE_PROBE_CHAT").as_deref() == Ok("empty") {
            self.entry.buffer().set_text("");
            let entry = self.entry.clone();
            glib::idle_add_local_once(move || {
                entry.grab_focus();
            });
            return;
        }
        use agent_client_protocol::schema::v1::{
            Content, ContentChunk, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, ToolCall,
            ToolCallUpdate, ToolCallUpdateFields, ToolKind,
        };
        let plan = || {
            Plan::new(vec![
                PlanEntry::new(
                    "Reproduce the scroll reset in the Inbox filter",
                    PlanEntryPriority::Medium,
                    PlanEntryStatus::Completed,
                ),
                PlanEntry::new(
                    "Keep the adjustment across the row rebuild",
                    PlanEntryPriority::High,
                    PlanEntryStatus::InProgress,
                ),
            ])
        };
        self.render_update(SessionUpdate::Plan(plan()));
        self.render_update(SessionUpdate::UserMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(
                "The Inbox filter jumps back to the top every time git status \
                 refreshes. Keep the scroll position across the rebuild.",
            )),
        )));
        // The agent restating its plan as it picks up the prompt: no news,
        // so nothing new on screen.
        self.render_update(SessionUpdate::Plan(plan()));

        // A thought, then something else — which is what closes it, and so
        // what turns "Thinking…" into a duration.
        self.render_update(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(
                "The row model is rebuilt on every status refresh, so the \
                 adjustment is reset before the new rows are bound.",
            )),
        )));
        for chunk in [
            "Reproduced it. `refresh_status` rebuilds the row model, and the \
             `ListView` drops its adjustment when the factory is reset.\n\n",
            "So the offset has to be **read before** the rebuild and restored \
             *after* the rows bind — restoring it inline lands while the list \
             is still empty and does nothing.\n\n",
            "- `keep_scroll` wraps the rebuild\n\
             - the offset is restored on the next idle tick\n",
        ] {
            self.render_update(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                ContentBlock::Text(TextContent::new(chunk)),
            )));
        }

        // A shell call whose output carries ANSI — the case a plain wrapped
        // label rendered as literal escape bytes.
        let mut shell = ToolCall::new("probe-shell", "cargo test -p taste-app filetree");
        shell.kind = ToolKind::Execute;
        shell.status = ToolCallStatus::Completed;
        shell.content = vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
            TextContent::new(
                "\u{1b}[32m   Compiling\u{1b}[0m taste-app v0.1.0\n\
                 \u{1b}[32m    Finished\u{1b}[0m `test` profile in 12.4s\n\
                 \u{1b}[1;32mtest result: ok\u{1b}[0m. 31 passed; 0 failed\n",
            ),
        )))];
        self.render_update(SessionUpdate::ToolCall(shell));
        self.expand_tool_card_for_probe("probe-shell");

        // An edit call carrying a diff — the syntax-highlighting surface.
        let mut edit = ToolCall::new("probe-edit", "Edit crates/taste-app/src/filetree.rs");
        edit.kind = ToolKind::Edit;
        edit.status = ToolCallStatus::Completed;
        let mut diff = Diff::new(
            std::path::PathBuf::from("crates/taste-app/src/filetree.rs"),
            "fn keep_scroll<R>(self: &Rc<Self>, apply: impl FnOnce() -> R) -> R {\n    \
             let adjustment = self.list_scroller.vadjustment();\n    \
             let offset = adjustment.value();\n    let result = apply();\n    \
             glib::idle_add_local_once(move || adjustment.set_value(offset));\n    \
             result\n}\n",
        );
        diff.old_text = Some(
            "fn keep_scroll<R>(self: &Rc<Self>, apply: impl FnOnce() -> R) -> R {\n    \
             let adjustment = self.list_scroller.vadjustment();\n    \
             let offset = adjustment.value();\n    let result = apply();\n    \
             adjustment.set_value(offset);\n    result\n}\n"
                .into(),
        );
        edit.content = vec![ToolCallContent::Diff(diff)];
        self.render_update(SessionUpdate::ToolCall(edit));
        self.expand_tool_card_for_probe("probe-edit");

        // In flight, and failed: the two states a glance has to tell apart.
        let mut running = ToolCall::new("probe-running", "Read crates/taste-app/src/filetree.rs");
        running.kind = ToolKind::Read;
        running.status = ToolCallStatus::InProgress;
        self.render_update(SessionUpdate::ToolCall(running));
        // Honest about the design: an agent has no push target, so this is
        // what reaching for one looks like from inside the transcript.
        let mut failed =
            ToolCall::new("probe-failed", "git push origin agents/calm-1/inbox-scroll");
        failed.kind = ToolKind::Execute;
        failed.status = ToolCallStatus::Failed;
        self.render_update(SessionUpdate::ToolCall(failed));
        // And an update that restates content already shown: the card must
        // not grow a second copy of it.
        self.render_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "probe-shell",
            ToolCallUpdateFields::new(),
        )));

        // A turn in flight, and the question it stopped on. Which question
        // depends on the variant: `TASTE_PROBE_CHAT=permission` asks about a
        // command, `permission-edit` about a file (with the diff on the
        // card), and `busy` asks nothing at all — that is the shot the
        // working line is in, since it steps aside for a card.
        self.stop_button.set_visible(true);
        self.set_busy(true);
        match std::env::var("TASTE_PROBE_CHAT").as_deref() {
            Ok("busy") => self.set_activity("cargo test -p taste-app filetree"),
            Ok(variant) => self.seed_permission_for_probe(variant),
            Err(_) => self.seed_permission_for_probe(""),
        }

        // Chips on the composer: they wrap, and each one is removable.
        self.add_attachment(
            "filetree.rs:4136–4152".into(),
            ContentBlock::Text(TextContent::new("…")),
        );
        self.add_attachment(
            "ENVIRONMENTS.md".into(),
            ContentBlock::Text(TextContent::new("…")),
        );

        // One pane gets one screenshot, and a transcript worth looking at is
        // taller than the pane. `TASTE_PROBE_CHAT=top` detaches the tail and
        // parks at the beginning, so the half that scrolls off the bottom —
        // the plan card, the prompt, the finished thought and its duration —
        // can be looked at too.
        if std::env::var("TASTE_PROBE_CHAT").as_deref() == Ok("top") {
            self.stick_to_bottom.set(false);
            let adjustment = self.transcript_scroller.vadjustment();
            glib::idle_add_local_once(move || adjustment.set_value(0.0));
        }
    }

    /// TASTE_PROBE_CHECK only: put a real permission request on screen.
    ///
    /// Through `handle_event`, not by poking the labels: a fixture that sets
    /// the card's text directly is a fixture that keeps looking right after
    /// the code that builds the card stops working. What is shot here is the
    /// same path an agent's `session/request_permission` takes, reply channel
    /// and all — the receiver is dropped on the spot, which is what any
    /// unanswered request does anyway.
    ///
    /// The variants are the card's shapes: an untyped ask whose title is the
    /// whole question (the devcontainer consent gate the design is built
    /// around — the agent authored the config, applying it is the user's
    /// move), a command, and a file edit carrying its diff.
    #[doc(hidden)]
    fn seed_permission_for_probe(self: &Rc<Self>, variant: &str) {
        use agent_client_protocol::schema::v1::{
            Content, Diff, PermissionOption, PermissionOptionKind, RequestPermissionRequest,
            ToolCallUpdate, ToolCallUpdateFields, ToolKind,
        };
        let mut fields = ToolCallUpdateFields::new();
        let (allow, deny) = match variant {
            "permission" => {
                fields.kind = Some(ToolKind::Execute);
                fields.title = Some("cargo test -p taste-app --all-features filetree".into());
                // The agent's own option names, which is what the buttons
                // say: "and don't ask again" is a different answer from
                // "yes, this once" and must not read alike.
                ("Allow, don't ask again", "Reject")
            }
            "permission-edit" => {
                fields.kind = Some(ToolKind::Edit);
                fields.title = Some("Edit crates/taste-app/src/filetree.rs".into());
                fields.locations = Some(vec![
                    agent_client_protocol::schema::v1::ToolCallLocation::new(
                        "crates/taste-app/src/filetree.rs",
                    ),
                ]);
                let mut diff = Diff::new(
                    std::path::PathBuf::from("crates/taste-app/src/filetree.rs"),
                    "    let offset = adjustment.value();\n    \
                     let result = apply();\n    \
                     glib::idle_add_local_once(move || adjustment.set_value(offset));\n",
                );
                diff.old_text = Some(
                    "    let offset = adjustment.value();\n    \
                     let result = apply();\n    \
                     adjustment.set_value(offset);\n"
                        .into(),
                );
                fields.content = Some(vec![ToolCallContent::Diff(diff)]);
                ("Allow", "Deny")
            }
            // The consent gate: no kind to lean on, so the agent's sentence
            // is the question, and what it will actually run is the body.
            _ => {
                fields.title = Some("Rebuild calm-1 from the changed devcontainer.json?".into());
                fields.content = Some(vec![ToolCallContent::Content(Content::new(
                    ContentBlock::Text(TextContent::new(
                        "The config on disk differs from the container that is \
                         running. Applying it rebuilds the container and runs \
                         its postCreateCommand.",
                    )),
                ))]);
                ("Allow", "Deny")
            }
        };
        let request = RequestPermissionRequest::new(
            "probe-session",
            ToolCallUpdate::new("probe-permission", fields),
            vec![
                PermissionOption::new("allow", allow, PermissionOptionKind::AllowOnce),
                PermissionOption::new("deny", deny, PermissionOptionKind::RejectOnce),
            ],
        );
        let (reply, _) = tokio::sync::oneshot::channel();
        self.handle_event(SessionEvent::Permission { request, reply });
    }

    /// TASTE_PROBE_CHECK only: open a tool card, so the screenshot shows its
    /// content and not just a row of collapsed headers.
    #[doc(hidden)]
    fn expand_tool_card_for_probe(&self, id: &str) {
        if let Some(card) = self.tool_cards.borrow().get(id) {
            card.set_expanded(true);
        }
    }

    /// Put the caret in this chat's composer. Called when its tab becomes
    /// the selected one: a chat is a conversation, and the thing you do with
    /// a conversation you have just switched to is type into it.
    pub fn focus_composer(&self) {
        self.entry.grab_focus();
    }

    pub fn seed_composer_for_probe(&self, text: &str) {
        self.entry.buffer().set_text(text);
    }

    fn answer_permission(&self, allowed: bool) {
        self.clear_notification("permission");
        self.permission_bar.set_reveal_child(false);
        let answered = self.pending_permission.borrow_mut().take();
        // The question is off the screen, so the turn is working again (if it
        // still is): the working line comes back with it.
        self.sync_busy_row();
        if let Some((request, reply)) = answered {
            // Answered: the row stops asking.
            self.note_activity();
            let title = single_line(&permission_title(&request), 120);
            let chosen = if allowed {
                allow_option(&request.options)
            } else {
                // A declined call and a cancelled turn are different facts,
                // and agents act on the difference — send the agent's own
                // reject option when it offered one.
                reject_option(&request.options)
            };
            // The record names the option actually sent: back-to-back
            // requests look identical, and a silent answer reads as a dead
            // button.
            let outcome = match chosen {
                Some(option) => {
                    self.note_permission(
                        request.tool_call.tool_call_id.to_string(),
                        if allowed {
                            "changes-allow-symbolic"
                        } else {
                            "changes-prevent-symbolic"
                        },
                        format!(
                            "{} “{}”",
                            if allowed { "Approved" } else { "Denied" },
                            option.name
                        ),
                    );
                    self.workspace.ide.record_permission(
                        &title,
                        if allowed { "approved" } else { "denied" },
                        &format!("the user clicked “{}” in the chat pane", option.name),
                    );
                    outcome_for(option)
                }
                None => {
                    // An anomaly, not an outcome: the agent offered nothing
                    // matching, so nobody's answer got through. That earns a
                    // line of its own.
                    self.meta_row(&format!(
                        "cancelled — no {} option offered for {title}",
                        if allowed { "allow" } else { "reject" }
                    ));
                    self.workspace.ide.record_permission(
                        &title,
                        "cancelled",
                        &format!(
                            "the user tried to {} but the request offered no \
                             matching option to say it with",
                            if allowed { "allow" } else { "reject" }
                        ),
                    );
                    RequestPermissionOutcome::Cancelled
                }
            };
            let _ = reply.send(outcome);
        }
    }
}

/// A finished thought's header: "Thought for 12s".
///
/// Sub-second reasoning rounds to "a moment" rather than "0s" — a duration
/// of zero reads as a bug, and the honest thing to say about 40ms of
/// thinking is that there is nothing to report.
fn thought_duration(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0 => "Thought for a moment".into(),
        1..=59 => format!("Thought for {secs}s"),
        _ => format!("Thought for {}m {}s", secs / 60, secs % 60),
    }
}

/// A wait, for a badge: "8s", "1m 4s". Whole seconds — this measures a
/// queue behind a model turn, so there is nothing finer worth showing.
fn elapsed(since: std::time::Instant) -> String {
    let secs = since.elapsed().as_secs_f64().round() as u64;
    match secs {
        0..=59 => format!("{secs}s"),
        _ => format!("{}m {}s", secs / 60, secs % 60),
    }
}

/// The next pane's notification key.
///
/// Process-unique and never reused, like the tab strip's ordinals and for
/// the same reason: a key that could be handed to a second pane would send
/// a notification click to the wrong conversation. Not derived from
/// anything the user or the agent can change.
fn next_notify_key() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("chat-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// A tool title is whatever the agent called the call — for a shell tool,
/// the entire script. Anywhere a title is shown AS a line (a transcript
/// note, a desktop notification) it has to be one.
fn single_line(text: &str, max: usize) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    if out.chars().count() > max {
        out = out.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
    }
    out
}

/// How one permission ask presents itself.
///
/// The card is a question, so it is shaped like one: a glyph that types the
/// ask at a glance, the question itself in heading type, one dim line saying
/// who is asking and where it lands, and — when the title is a *thing* rather
/// than a sentence — that thing set monospace below, where a command belongs.
struct PermissionFace {
    icon: &'static str,
    title: String,
    subtitle: String,
    /// The command or path itself. `None` when the title already IS the
    /// specifics, which is the untyped case: whatever the agent called the
    /// call is the best question anyone has.
    code: Option<String>,
}

/// Compose the card's face from the request, the agent's name and the
/// environment the answer applies to.
///
/// A typed call's title is the literal thing — a shell script, a path — and a
/// shell script set as a heading is a heading nobody can scan. So a typed
/// call asks about its KIND and shows the thing underneath; an untyped one
/// has no shape to lean on and asks in the agent's own words.
fn permission_face(
    request: &RequestPermissionRequest,
    agent: &str,
    environment: &str,
) -> PermissionFace {
    let detail = permission_title(request);
    // A call that names a file has said the exact thing better than its own
    // title does ("Edit a file? / Edit …/filetree.rs" says "edit" twice).
    let location = request
        .tool_call
        .fields
        .locations
        .as_ref()
        .and_then(|locations| locations.first())
        .map(|location| location.path.display().to_string());
    let (icon, question) = match request.tool_call.fields.kind {
        Some(ToolKind::Execute) => ("utilities-terminal-symbolic", Some("Run a command?")),
        Some(ToolKind::Edit) => ("document-edit-symbolic", Some("Edit a file?")),
        Some(ToolKind::Delete) => ("user-trash-symbolic", Some("Delete a file?")),
        Some(ToolKind::Move) => ("document-save-as-symbolic", Some("Move a file?")),
        Some(ToolKind::Read) => ("text-x-generic-symbolic", Some("Read a file?")),
        Some(ToolKind::Search) => ("system-search-symbolic", Some("Search the project?")),
        Some(ToolKind::Fetch) => (
            "network-transmit-receive-symbolic",
            Some("Fetch from the network?"),
        ),
        Some(ToolKind::SwitchMode) => ("view-refresh-symbolic", Some("Switch mode?")),
        // Think and Other alike: no kind to name, so the agent's title is
        // the question and there is nothing left to put underneath.
        _ => ("dialog-question-symbolic", None),
    };
    // The pane's header already names this pair, but a card that appears
    // mid-scroll is read where it sits: with one chat per environment, the
    // environment is always known, and the question says whose it is.
    let subtitle = format!("{agent} · {environment}");
    match question {
        Some(question) => PermissionFace {
            icon,
            title: question.to_string(),
            subtitle,
            // A command is its own best description; anything else says the
            // path it is about when it named one.
            code: Some(match request.tool_call.fields.kind {
                Some(ToolKind::Execute) => detail,
                _ => location.unwrap_or(detail),
            }),
        },
        None => PermissionFace {
            icon,
            title: detail,
            subtitle,
            code: None,
        },
    }
}

fn permission_title(request: &RequestPermissionRequest) -> String {
    let raw = request
        .tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| "a tool call".into());
    humanize_tool_title(&raw)
}

/// `mcp__taste-ide__devcontainer_status` → "Devcontainer Status (IDE)".
/// Raw tool ids are for wires, not people.
fn humanize_tool_title(raw: &str) -> String {
    let Some(rest) = raw.strip_prefix("mcp__") else {
        return raw.to_string();
    };
    let (server, tool) = match rest.split_once("__") {
        Some(parts) => parts,
        None => ("", rest),
    };
    let mut words: Vec<String> = tool
        .split(['_', '-'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    if words.is_empty() {
        words.push(tool.to_string());
    }
    let name = words.join(" ");
    if server.contains("taste") {
        format!("{name} (IDE)")
    } else {
        format!("{name} ({server})")
    }
}

/// What a card's content collection *is*, cheaply comparable. Only the
/// facts that reach the screen: a restated snapshot has to compare equal, or
/// the card rebuilds itself for nothing.
fn content_signature(content: &[ToolCallContent]) -> Vec<String> {
    content
        .iter()
        .map(|item| match item {
            ToolCallContent::Diff(diff) => format!(
                "diff\u{0}{}\u{0}{}\u{0}{}",
                diff.path.display(),
                diff.old_text.as_deref().unwrap_or_default(),
                diff.new_text
            ),
            ToolCallContent::Content(block) => {
                format!(
                    "text\u{0}{}",
                    content_text(&block.content).unwrap_or_default()
                )
            }
            other => format!("{other:?}"),
        })
        .collect()
}

/// One run of terminal output carrying a single SGR style.
#[derive(Debug, PartialEq)]
struct AnsiSpan {
    text: String,
    /// SGR foreground colour index (0–15), when one is in force.
    color: Option<u8>,
    bold: bool,
    dim: bool,
}

/// GNOME Console's ANSI palette, so a tool card's output is coloured the way
/// the same bytes are coloured in the Console tab. Legible on both
/// backgrounds — these are the terminal's own choices, not the theme's.
const ANSI_FG: [&str; 16] = [
    "#171421", "#c01c28", "#26a269", "#a2734c", "#12488b", "#a347ba", "#2aa1b3", "#d0cfcc",
    "#5e5c64", "#f66151", "#33d17a", "#e9ad0c", "#2a7bde", "#c061cb", "#33c7de", "#ffffff",
];

/// Split terminal output into styled runs, honouring the SGR escapes a build
/// log actually carries (colour, bold, dim, reset) and DISCARDING every other
/// escape sequence.
///
/// Discarding matters more than colouring: rendered into a plain label, a
/// cargo or npm log shows its escape bytes as literal `[32m` garbage
/// mid-sentence. Anything unrecognised is dropped rather than printed.
fn ansi_spans(text: &str) -> Vec<AnsiSpan> {
    let mut spans: Vec<AnsiSpan> = Vec::new();
    let (mut color, mut bold, mut dim) = (None, false, false);
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    let mut push = |current: &mut String, color, bold, dim| {
        if !current.is_empty() {
            spans.push(AnsiSpan {
                text: std::mem::take(current),
                color,
                bold,
                dim,
            });
        }
    };
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            current.push(c);
            continue;
        }
        // OSC: ESC ] … terminated by BEL or ST (ESC \). Its payload is a
        // window title and contains letters, so it must NOT be terminated on
        // the first alphabetic byte the way a CSI is.
        if chars.peek() == Some(&']') {
            chars.next();
            while let Some(next) = chars.next() {
                if next == '\u{7}' {
                    break;
                }
                if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                    chars.next();
                    break;
                }
            }
            continue;
        }
        // Any other escape is a single-character one (ESC c, ESC 7, …).
        if chars.peek() != Some(&'[') {
            chars.next();
            continue;
        }
        chars.next(); // the '['
        let mut params = String::new();
        let mut final_byte = None;
        for next in chars.by_ref() {
            if next.is_ascii_alphabetic() {
                final_byte = Some(next);
                break;
            }
            params.push(next);
        }
        if final_byte != Some('m') {
            continue; // a cursor move or an erase: not our business
        }
        push(&mut current, color, bold, dim);
        for param in params.split(';') {
            match param.parse::<u16>().unwrap_or(0) {
                0 => {
                    color = None;
                    bold = false;
                    dim = false;
                }
                1 => bold = true,
                2 => dim = true,
                22 => {
                    bold = false;
                    dim = false;
                }
                39 => color = None,
                n @ 30..=37 => color = Some((n - 30) as u8),
                n @ 90..=97 => color = Some((n - 90 + 8) as u8),
                _ => {}
            }
        }
    }
    push(&mut current, color, bold, dim);
    spans
}

/// The literal thing a permission card is asking about: the command, the
/// path. Monospace on the same wash a tool card's output wears, so the card
/// reads as one family with the transcript above it — and selectable, because
/// the first thing anyone does with a command they are unsure about is copy
/// it somewhere they can read it properly.
///
/// Clipped rather than scrolling: a permission card that can grow to fill the
/// pane is a card that pushes its own buttons off the screen. The whole title
/// is on the card's tooltip.
fn permission_code_widget(text: &str) -> gtk::Widget {
    let label = gtk::Label::builder()
        .label(text)
        .attributes(&no_hyphens())
        .wrap(true)
        // A path has no spaces to break at, so word wrapping alone would
        // overflow the pane rather than fold.
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .lines(4)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .selectable(true)
        .css_classes(["monospace", "caption"])
        .build();
    let wash = gtk::Box::new(gtk::Orientation::Vertical, 0);
    wash.add_css_class("terminal-output");
    wash.add_css_class("permission-code");
    wash.append(&label);
    wash.upcast()
}

/// A tool call's terminal output, looking like terminal output: monospace,
/// a dim wash to set it off from the prose around it, and its ANSI colours
/// honoured rather than printed.
fn terminal_output_widget(text: &str) -> gtk::Widget {
    let view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(6)
        .bottom_margin(6)
        .left_margin(8)
        .right_margin(8)
        .build();
    let buffer = view.buffer();
    let table = buffer.tag_table();
    for span in ansi_spans(text) {
        let mut end = buffer.end_iter();
        let start_offset = end.offset();
        buffer.insert(&mut end, &span.text);
        if span.color.is_none() && !span.bold && !span.dim {
            continue;
        }
        // One tag per distinct style, reused: a long log is thousands of
        // spans and a tag each would be thousands of objects.
        let name = format!(
            "ansi-{}-{}-{}",
            span.color.map_or(-1, i16::from),
            span.bold,
            span.dim
        );
        let tag = table.lookup(&name).unwrap_or_else(|| {
            let tag = gtk::TextTag::builder().name(&name).build();
            // Dim with no colour of its own is the palette's bright black,
            // which is what a terminal renders SGR 2 as: a grey that stays
            // legible against either background.
            let color = span.color.unwrap_or(8).min(15);
            if span.color.is_some() || span.dim {
                if let Ok(rgba) = ANSI_FG[color as usize].parse::<gtk::gdk::RGBA>() {
                    tag.set_foreground_rgba(Some(&rgba));
                }
            }
            if span.bold {
                tag.set_weight(700);
            }
            table.add(&tag);
            tag
        });
        let start = buffer.iter_at_offset(start_offset);
        buffer.apply_tag(&tag, &start, &end);
    }
    let scroller = gtk::ScrolledWindow::builder()
        .child(&view)
        .max_content_height(240)
        .propagate_natural_height(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .css_classes(["terminal-output"])
        .build();
    scroller.upcast()
}

/// A rendered unified diff: red for removals, green for additions, and the
/// code itself syntax-highlighted for the language the path implies.
///
/// A GtkSourceView, not a plain TextView: the machinery is already in the
/// binary for the editor, and an agent's proposed edit is the one place in
/// the transcript where the reader is being asked to judge *code*. The
/// per-line `+ `/`- ` prefixes stay — colour alone is not a signal everyone
/// receives, and they survive being copied out.
fn diff_widget(diff: &Diff) -> gtk::Widget {
    use similar::{ChangeTag, TextDiff};

    let source_buffer = sourceview5::Buffer::new(None);
    if let Some(language) = sourceview5::LanguageManager::default()
        .guess_language(Some(diff.path.to_string_lossy().as_ref()), None)
    {
        sourceview5::prelude::BufferExt::set_language(&source_buffer, Some(&language));
    }
    apply_diff_scheme(&source_buffer);
    // A transcript card outlives a theme switch, and a light scheme on a
    // dark background is unreadable. Weak, so a capped-out card's buffer is
    // still collectable.
    {
        let weak = source_buffer.downgrade();
        adw::StyleManager::default().connect_dark_notify(move |_| {
            if let Some(buffer) = weak.upgrade() {
                apply_diff_scheme(&buffer);
            }
        });
    }
    let view = sourceview5::View::builder()
        .buffer(&source_buffer)
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .build();
    let buffer = view.buffer();
    let table = buffer.tag_table();
    let add_tag = gtk::TextTag::builder().name("diff-add").build();
    add_tag.set_paragraph_background_rgba(Some(&gtk::gdk::RGBA::new(0.2, 0.7, 0.3, 0.18)));
    let del_tag = gtk::TextTag::builder().name("diff-del").build();
    del_tag.set_paragraph_background_rgba(Some(&gtk::gdk::RGBA::new(0.9, 0.2, 0.2, 0.18)));
    table.add(&add_tag);
    table.add(&del_tag);

    let old = diff.old_text.clone().unwrap_or_default();
    let text_diff = TextDiff::from_lines(&old, &diff.new_text);
    for (lines, change) in text_diff.iter_all_changes().enumerate() {
        if lines >= MAX_DIFF_LINES {
            let mut end = buffer.end_iter();
            buffer.insert(&mut end, "… (diff truncated)\n");
            break;
        }
        let (prefix, tag) = match change.tag() {
            ChangeTag::Insert => ("+ ", Some("diff-add")),
            ChangeTag::Delete => ("- ", Some("diff-del")),
            ChangeTag::Equal => ("  ", None),
        };
        let mut end = buffer.end_iter();
        let start_offset = end.offset();
        buffer.insert(&mut end, &format!("{prefix}{}", change.value()));
        if let Some(tag) = tag {
            let start = buffer.iter_at_offset(start_offset);
            buffer.apply_tag_by_name(tag, &start, &end);
        }
    }
    let scroller = gtk::ScrolledWindow::builder()
        .child(&view)
        .max_content_height(240)
        .propagate_natural_height(true)
        .build();
    // The path, as a caption above the code rather than as its first LINE.
    // Inside the buffer it was syntax-highlighted like source and read as
    // part of the edit; the file being edited is a label, not code.
    let path = gtk::Label::builder()
        .label(diff.path.to_string_lossy())
        .attributes(&no_hyphens())
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::Start)
        .tooltip_text(diff.path.to_string_lossy())
        .css_classes(["dim-label", "caption", "monospace"])
        .build();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 2);
    body.append(&path);
    body.append(&scroller);
    body.upcast()
}

/// The Adwaita scheme matching the current dark/light preference — the same
/// pairing the editor uses, so a diff in the transcript and the same file in
/// the editor are colored alike.
fn apply_diff_scheme(buffer: &sourceview5::Buffer) {
    let scheme_id = if adw::StyleManager::default().is_dark() {
        "Adwaita-dark"
    } else {
        "Adwaita"
    };
    if let Some(scheme) = sourceview5::StyleSchemeManager::default().scheme(scheme_id) {
        sourceview5::prelude::BufferExt::set_style_scheme(buffer, Some(&scheme));
    }
}

/// On/off-shaped select options render as switches: returns
/// (on_value, off_value) when the choice set is boolean in disguise.
fn switch_values(choices: &[(String, String)]) -> Option<(String, String)> {
    if choices.len() != 2 {
        return None;
    }
    const PAIRS: [(&str, &str); 3] = [("on", "off"), ("true", "false"), ("enabled", "disabled")];
    let a = choices[0].0.to_ascii_lowercase();
    let b = choices[1].0.to_ascii_lowercase();
    for (on, off) in PAIRS {
        if (a == on && b == off) || (a == off && b == on) {
            let on_value = if a == on {
                &choices[0].0
            } else {
                &choices[1].0
            };
            let off_value = if a == off {
                &choices[0].0
            } else {
                &choices[1].0
            };
            return Some((on_value.clone(), off_value.clone()));
        }
    }
    None
}

fn clear_children(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

/// A non-text block from a replayed user message, as the card's attachment
/// row wants it: (label, block). Labels come from whatever the block knows
/// about itself — a URI's file name, a resource's own name — because
/// replayed history carries no file picker to have named it.
fn replayed_attachment(block: &ContentBlock) -> Option<(String, ContentBlock)> {
    fn file_name(uri: &str) -> Option<String> {
        let trimmed = uri.split(['?', '#']).next().unwrap_or(uri);
        let name = trimmed.rsplit('/').next()?;
        (!name.is_empty()).then(|| name.to_string())
    }
    let label = match block {
        ContentBlock::Text(_) => return None,
        ContentBlock::Image(image) => image
            .uri
            .as_deref()
            .and_then(file_name)
            .unwrap_or_else(|| "image".to_string()),
        ContentBlock::Audio(_) => "audio".to_string(),
        ContentBlock::ResourceLink(link) => link.name.clone(),
        ContentBlock::Resource(resource) => {
            let uri = match &resource.resource {
                EmbeddedResourceResource::TextResourceContents(text) => &text.uri,
                EmbeddedResourceResource::BlobResourceContents(blob) => &blob.uri,
                _ => return Some(("attachment".to_string(), block.clone())),
            };
            file_name(uri).unwrap_or_else(|| uri.clone())
        }
        _ => "attachment".to_string(),
    };
    Some((label, block.clone()))
}

fn content_text(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text(text) => Some(text.text.clone()),
        _ => None,
    }
}

/// Session-cumulative token usage, humanized. Account-level quotas (5-hour
/// and weekly limits) are not modeled by ACP; agents announce those in-band
/// when relevant.
/// Pango breaks inside a word when the word cannot fit on a line, and marks
/// the break with a hyphen. For prose that is typography; for a pasted path,
/// URL, command or token it is a character the author never typed, sitting in
/// the middle of their text and getting copied back out with it.
fn no_hyphens() -> gtk::pango::AttrList {
    let attributes = gtk::pango::AttrList::new();
    attributes.insert(gtk::pango::AttrInt::new_insert_hyphens(false));
    attributes
}

/// Tokens, at a glance: "1.2M", "18.4k", "42".
fn token_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn format_usage(usage: &Usage) -> String {
    let k = token_count;
    let mut parts = vec![format!(
        "session tokens: {} in · {} out · {} total",
        k(usage.input_tokens),
        k(usage.output_tokens),
        k(usage.total_tokens)
    )];
    if let Some(cached) = usage.cached_read_tokens {
        if cached > 0 {
            parts.push(format!("{} cached", k(cached)));
        }
    }
    parts.join(" · ")
}

/// A file as an embedded text resource (the "Add context" shape).
fn text_attachment(path: &std::path::Path) -> anyhow::Result<(String, ContentBlock)> {
    let meta = std::fs::metadata(path)?;
    anyhow::ensure!(
        meta.len() <= MAX_TEXT_ATTACHMENT_BYTES,
        "{} is larger than {}KB",
        path.display(),
        MAX_TEXT_ATTACHMENT_BYTES / 1024
    );
    let text = std::fs::read_to_string(path)
        .map_err(|_| anyhow::anyhow!("{} is not text", path.display()))?;
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let uri = format!("file://{}", path.display());
    let block = ContentBlock::Resource(EmbeddedResource::new(
        EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(text, uri)),
    ));
    Ok((label, block))
}

/// An image as a base64 content block (the "Upload" shape).
/// The preview an image attachment gets, wherever it is shown.
fn image_thumbnail(texture: &gtk::gdk::Texture) -> gtk::Picture {
    let picture = gtk::Picture::for_paintable(texture);
    picture.set_content_fit(gtk::ContentFit::Cover);
    picture.set_size_request(ATTACHMENT_THUMBNAIL_PX, ATTACHMENT_THUMBNAIL_PX);
    picture
}

/// Base64 payload → texture. `None` for anything GDK cannot decode; the
/// caller falls back to naming the attachment.
fn decode_image(image: &ImageContent) -> Option<gtk::gdk::Texture> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .ok()?;
    gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).ok()
}

/// Full-size image viewer for an attachment in the transcript.
fn present_image_dialog(anchor: &impl IsA<gtk::Widget>, title: &str, texture: &gtk::gdk::Texture) {
    let picture = gtk::Picture::for_paintable(texture);
    picture.set_content_fit(gtk::ContentFit::ScaleDown);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&picture)
        .hexpand(true)
        .vexpand(true)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroller));
    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(760)
        .content_height(560)
        .build();
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(anchor));
}

/// The whole of a clipped prompt, scrollable and selectable.
fn present_text_dialog(anchor: &impl IsA<gtk::Widget>, title: &str, body: &str) {
    let view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(12)
        .bottom_margin(12)
        .left_margin(12)
        .right_margin(12)
        .build();
    view.buffer().set_text(body);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&view)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroller));
    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(640)
        .content_height(520)
        .build();
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(anchor));
}

fn image_attachment(path: &std::path::Path) -> anyhow::Result<(String, ContentBlock)> {
    use base64::Engine;
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        other => anyhow::bail!("unsupported image type: {other:?}"),
    };
    let meta = std::fs::metadata(path)?;
    anyhow::ensure!(
        meta.len() <= MAX_IMAGE_ATTACHMENT_BYTES,
        "{} is larger than {}MB",
        path.display(),
        MAX_IMAGE_ATTACHMENT_BYTES / (1024 * 1024)
    );
    let bytes = std::fs::read(path)?;
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok((label, ContentBlock::Image(ImageContent::new(data, mime))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::gdk::{Key, ModifierType};

    const CALM: ComposerState = ComposerState {
        preedit: false,
        streaming: false,
        empty: false,
        awaiting_permission: false,
    };

    #[test]
    fn enter_sends_and_shift_enter_does_not() {
        assert_eq!(
            composer_key(Key::Return, ModifierType::empty(), CALM),
            ComposerKey::Send
        );
        assert_eq!(
            composer_key(Key::KP_Enter, ModifierType::empty(), CALM),
            ComposerKey::Send
        );
        assert_eq!(
            composer_key(Key::Return, ModifierType::SHIFT_MASK, CALM),
            ComposerKey::Insert
        );
    }

    /// The rule that is invisible until it destroys somebody's sentence:
    /// while an input method is composing, Enter COMMITS the composition.
    /// Sending there truncates the message mid-word, unrecoverably, and it
    /// happens on ordinary typing for every CJK user.
    #[test]
    fn enter_belongs_to_the_input_method_mid_preedit() {
        let composing = ComposerState {
            preedit: true,
            ..CALM
        };
        assert_eq!(
            composer_key(Key::Return, ModifierType::empty(), composing),
            ComposerKey::Insert
        );
        // And nothing else is claimed either — the IM owns the keyboard
        // until the composition ends.
        let composing_busy = ComposerState {
            preedit: true,
            streaming: true,
            empty: true,
            awaiting_permission: true,
        };
        assert_eq!(
            composer_key(Key::Escape, ModifierType::empty(), composing_busy),
            ComposerKey::Insert
        );
        assert_eq!(
            composer_key(Key::Up, ModifierType::empty(), composing_busy),
            ComposerKey::Insert
        );
    }

    /// Escape matches the Stop button exactly: it cancels a turn, and it
    /// never throws away typing when there is no turn to cancel.
    #[test]
    fn escape_stops_only_while_streaming() {
        let streaming = ComposerState {
            streaming: true,
            ..CALM
        };
        assert_eq!(
            composer_key(Key::Escape, ModifierType::empty(), streaming),
            ComposerKey::Stop
        );
        assert_eq!(
            composer_key(Key::Escape, ModifierType::empty(), CALM),
            ComposerKey::Insert
        );
    }

    /// A permission card owns Escape while it is up — and it takes it from
    /// the turn behind it, which is always streaming when a card is up. The
    /// safe answer has to be the reflex answer.
    #[test]
    fn escape_denies_the_permission_card_before_it_stops_the_turn() {
        let asking = ComposerState {
            streaming: true,
            awaiting_permission: true,
            ..CALM
        };
        assert_eq!(
            composer_key(Key::Escape, ModifierType::empty(), asking),
            ComposerKey::DenyPermission
        );
        // Enter is never the counterpart: approving takes focus on the
        // button. From the composer, Enter still sends the prompt.
        assert_eq!(
            composer_key(Key::Return, ModifierType::empty(), asking),
            ComposerKey::Send
        );
    }

    /// Up recalls only from an EMPTY composer: with text in it, Up is a
    /// cursor key, and stealing it would strand the caret on line one.
    #[test]
    fn up_recalls_only_from_an_empty_composer() {
        let empty = ComposerState {
            empty: true,
            ..CALM
        };
        assert_eq!(
            composer_key(Key::Up, ModifierType::empty(), empty),
            ComposerKey::RecallLast
        );
        assert_eq!(
            composer_key(Key::Up, ModifierType::empty(), CALM),
            ComposerKey::Insert
        );
        // A modified Up is a selection or a scroll, never a recall.
        assert_eq!(
            composer_key(Key::Up, ModifierType::SHIFT_MASK, empty),
            ComposerKey::Insert
        );
        assert_eq!(
            composer_key(Key::Up, ModifierType::CONTROL_MASK, empty),
            ComposerKey::Insert
        );
    }

    #[test]
    fn send_is_live_only_with_something_to_send() {
        assert!(!send_ready("", 0));
        assert!(!send_ready("   \n\t ", 0));
        assert!(send_ready("hello", 0));
        // An attachment with no prose is still a prompt.
        assert!(send_ready("", 1));
        assert!(send_ready("  ", 2));
    }

    /// Streaming must never yank the view off a reader who scrolled up —
    /// and must never stop following one who did not.
    #[test]
    fn tailing_follows_the_bottom_and_announces_otherwise() {
        assert_eq!(tail_action(true, true), TailAction::Repin);
        // Still pinned when the viewport merely resized: re-pinning is
        // right, announcing would be a banner for nothing.
        assert_eq!(tail_action(true, false), TailAction::Repin);
        assert_eq!(tail_action(false, true), TailAction::Announce);
        // Detached, and nothing new arrived: the composer growing a line
        // under the transcript is not "new messages below".
        assert_eq!(tail_action(false, false), TailAction::Nothing);
    }

    #[test]
    fn a_finished_thought_reports_how_long_it_took() {
        use std::time::Duration;
        assert_eq!(
            thought_duration(Duration::from_millis(40)),
            "Thought for a moment"
        );
        assert_eq!(thought_duration(Duration::from_secs(12)), "Thought for 12s");
        assert_eq!(
            thought_duration(Duration::from_secs(64)),
            "Thought for 1m 4s"
        );
    }

    #[test]
    fn ansi_colour_is_read_and_the_escapes_never_reach_the_screen() {
        let spans = ansi_spans("\u{1b}[32mok\u{1b}[0m done");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "ok");
        assert_eq!(spans[0].color, Some(2));
        assert_eq!(spans[1].text, " done");
        assert_eq!(spans[1].color, None);
        // Bright colours map into the top half of the palette.
        assert_eq!(ansi_spans("\u{1b}[91mx")[0].color, Some(9));
        // Bold and dim are attributes, not colours.
        let bold = ansi_spans("\u{1b}[1;32mx");
        assert!(bold[0].bold && bold[0].color == Some(2));
    }

    /// Whatever we cannot interpret must be DROPPED, not printed: rendered
    /// literally, a cargo log shows "[2K[1G" mid-sentence.
    #[test]
    fn unhandled_escapes_are_swallowed_whole() {
        assert_eq!(ansi_spans("a\u{1b}[2K\u{1b}[1Gb")[0].text, "ab");
        assert_eq!(ansi_spans("a\u{1b}]0;title\u{7}b")[0].text, "ab");
        // Plain text with no escapes at all is one span, unchanged.
        let plain = ansi_spans("just text");
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].text, "just text");
        assert!(ansi_spans("").is_empty());
    }

    /// An ACP content update REPLACES the card's content, and agents restate
    /// the whole of a call's output on every update. The signature is what
    /// tells a restatement from news — get it wrong and the card either
    /// rebuilds under the pointer or grows a second copy of itself.
    #[test]
    fn a_restated_content_snapshot_compares_equal() {
        let block = |text: &str| {
            ToolCallContent::Content(agent_client_protocol::schema::v1::Content::new(
                ContentBlock::Text(TextContent::new(text)),
            ))
        };
        assert_eq!(
            content_signature(&[block("running")]),
            content_signature(&[block("running")])
        );
        assert_ne!(
            content_signature(&[block("running")]),
            content_signature(&[block("running\ndone")])
        );
        // Order and arity are part of it.
        assert_ne!(
            content_signature(&[block("a"), block("b")]),
            content_signature(&[block("b"), block("a")])
        );
        assert_ne!(content_signature(&[block("a")]), content_signature(&[]));
    }

    /// Profiling harness (run on demand):
    /// `cargo test -p taste-app perf_ -- --ignored --nocapture`
    ///
    /// These two run on the STREAMING path — once per tool-call update,
    /// against output that grows for the length of a shell command. The
    /// signature is what decides whether the card rebuilds at all, so it has
    /// to be cheaper than the rebuild it prevents, and it is compared on
    /// every update whether or not anything changed.
    #[test]
    #[ignore]
    fn perf_tool_card_update_path() {
        let line = "\u{1b}[32m   Compiling\u{1b}[0m some-crate v0.1.0 (/workspaces/x)\n";
        for kib in [16, 64, 256] {
            let log = line.repeat(kib * 1024 / line.len());
            let content = [ToolCallContent::Content(
                agent_client_protocol::schema::v1::Content::new(ContentBlock::Text(
                    TextContent::new(log.clone()),
                )),
            )];
            let start = std::time::Instant::now();
            let signature = content_signature(&content);
            let signed = start.elapsed();
            // The comparison a restated snapshot actually costs.
            let start = std::time::Instant::now();
            assert_eq!(signature, content_signature(&content));
            let compared = start.elapsed();
            let start = std::time::Instant::now();
            let spans = ansi_spans(&log);
            let parsed = start.elapsed();
            println!(
                "tool card: {:>4} KiB → signature {:>8.1?}, restated-compare {:>8.1?}, \
                 {:>6} ansi spans in {:>8.1?}",
                log.len() / 1024,
                signed,
                compared,
                spans.len(),
                parsed
            );
        }
    }
}

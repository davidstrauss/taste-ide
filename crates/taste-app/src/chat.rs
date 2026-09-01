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
    SessionUpdate, TextContent, TextResourceContents, ToolCallContent, ToolCallStatus, Usage,
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
const MAX_DIFF_LINES: usize = 400;
const MAX_TRANSCRIPT_ROWS: u32 = 200;

type PendingPermission = (RequestPermissionRequest, taste_acp::PermissionReply);

/// The tab strip's hooks into one chat: "my restorable state changed"
/// (persist the tab list, relabel the tab) and "a turn is / is not in
/// flight" (the tab's spinner).
pub type PersistHook = Rc<dyn Fn()>;
pub type BusyHook = Rc<dyn Fn(bool)>;
/// "This chat wants an environment of its own." The tab strip answers it:
/// it knows the ordinals a readable slug is built from, and the clone is
/// its to run off the main thread.
pub type EnvironmentHook = Rc<dyn Fn()>;

/// What the environment row says when this chat has no environment of its
/// own. Sentence case per the HIG's button style, and a verb: the row does
/// something rather than describing a setting.
const ENVIRONMENT_OFFER: &str = "Give This Chat Its Own Environment";

/// A live tool-call card in the transcript, updated in place.
struct ToolCard {
    status_icon: gtk::Image,
    title_label: gtk::Label,
    /// How this call's permission was answered — hidden until it was.
    permission: gtk::Image,
    content: gtk::Box,
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
    permission_label: gtk::Label,
    status_label: gtk::Label,
    status_spinner: gtk::Spinner,
    busy_row: gtk::Box,
    /// The options shade: full-height session controls over the chat.
    options_panel: gtk::ScrolledWindow,
    options_toggle: gtk::ToggleButton,
    chat_tab: gtk::ToggleButton,
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
    /// The environment this chat's agent works in, or `None` for the
    /// primary. One chat, at most one environment: the binding is what
    /// aims every spawn (`taste_acp::AgentAim`), and it is persisted so a
    /// restored tab comes back pointing at the same clone.
    environment: RefCell<Option<EnvironmentId>>,
    /// The row that offers — and then names — this chat's environment.
    environment_row: adw::ButtonRow,
    /// A clone is in flight. The row is insensitive meanwhile, so a slow
    /// clone cannot be asked for twice.
    environment_pending: Cell<bool>,
    /// The owner's "this chat wants an environment of its own" hook: the
    /// tab strip owns id generation (it knows the ordinals) and the clone.
    on_new_environment: RefCell<Option<EnvironmentHook>>,
    // --- streaming state -------------------------------------------------
    current_agent: RefCell<Option<gtk::TextBuffer>>,
    current_agent_view: RefCell<Option<gtk::TextView>>,
    current_thought: RefCell<Option<gtk::TextBuffer>>,
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
}

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
    pub fn new(
        workspace: Workspace,
        environments: Arc<EnvironmentRegistry>,
        bridge_command: String,
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
        // The environment affordance. One row that offers a world of this
        // chat's own and, once it has one, says which. Deliberately
        // one-way in this phase: there is no unbind, because the clone is
        // where the agent's work lives and a button that silently aimed
        // the chat away from it would be a way to lose that work.
        let environment_row = adw::ButtonRow::builder()
            .title(ENVIRONMENT_OFFER)
            .start_icon_name("folder-new-symbolic")
            .build();
        environment_row.set_tooltip_text(Some(
            "Clone the workspace and give this chat its own checkout and \
             devcontainer. Its agent works there instead of in your files.",
        ));
        session_list.append(&agent_picker);
        session_list.append(&approval_picker);
        session_list.append(&environment_row);
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
        {
            let placeholder = adw::StatusPage::builder()
                .icon_name("chat-message-new-symbolic")
                .title("Ask Claude Code")
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

        // Permission requests surface inline, above the entry, with the
        // proposed diff when the tool call carries one.
        let permission_label = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .css_classes(["heading"])
            .build();
        let permission_detail = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let allow = gtk::Button::builder()
            .label("Allow")
            .css_classes(["suggested-action"])
            .build();
        let deny = gtk::Button::with_label("Deny");
        let permission_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        permission_buttons.set_halign(gtk::Align::End);
        permission_buttons.append(&deny);
        permission_buttons.append(&allow);
        let permission_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        permission_box.set_margin_top(6);
        permission_box.set_margin_bottom(6);
        permission_box.set_margin_start(6);
        permission_box.set_margin_end(6);
        permission_box.add_css_class("card");
        permission_box.set_widget_name("permission-bar");
        permission_box.append(&permission_label);
        permission_box.append(&permission_detail);
        permission_box.append(&permission_buttons);
        let permission_bar = gtk::Revealer::builder().child(&permission_box).build();

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
        let send = gtk::Button::builder()
            .label("Send")
            .tooltip_text("Send (Enter)")
            .sensitive(false)
            .build();
        // Square and quiet, like the attach button beside it: Stop and
        // Send are both live at once, and the row should read as one
        // affordance (Send) with two small controls, not three buttons
        // fighting for width.
        let stop_button = gtk::Button::builder()
            .icon_name("media-playback-stop-symbolic")
            .tooltip_text("Stop this turn")
            .visible(false)
            .build();
        let attach_menu = gtk::gio::Menu::new();
        attach_menu.append(Some("Current Selection"), Some("chat.attach-selection"));
        attach_menu.append(Some("Active File"), Some("chat.attach-active"));
        attach_menu.append(Some("File…"), Some("chat.attach-file"));
        attach_menu.append(Some("Image…"), Some("chat.attach-image"));
        // A square + button: the menu names its contents; Send gets the
        // rest of the row.
        let attach_button = gtk::MenuButton::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add context to the next prompt (images can also be pasted)")
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
                let overflow = (adjustment.upper() - visible).ceil() as i32;
                if overflow == 0 {
                    return;
                }
                let current = scroller.min_content_height();
                let target = (current + overflow).clamp(floor, 120);
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
        // Square: match the row height the Send button establishes.
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

        let busy_spinner = gtk::Spinner::new();
        busy_spinner.start();
        let busy_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        busy_row.set_margin_start(12);
        busy_row.set_margin_top(6);
        busy_row.set_margin_bottom(4);
        busy_row.append(&busy_spinner);
        busy_row.append(
            &gtk::Label::builder()
                .label("Working…")
                .css_classes(["dim-label", "caption"])
                .build(),
        );
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
        let usage_tab = gtk::ToggleButton::builder()
            .icon_name("utilities-system-monitor-symbolic")
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
            composer_area: entry_scroller.clone(),
            permission_bar,
            allow_button: allow.clone(),
            deny_button: deny.clone(),
            permission_label,
            status_label,
            status_spinner: status_spinner.clone(),
            busy_row: busy_row.clone(),
            permission_detail,
            client: RefCell::new(None),
            pending_permission: RefCell::new(None),
            pending_marks: RefCell::new(HashMap::new()),
            attachments: RefCell::new(Vec::new()),
            chips,
            send_button: send.clone(),
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
            environment: RefCell::new(None),
            environment_row: environment_row.clone(),
            environment_pending: Cell::new(false),
            on_new_environment: RefCell::new(None),
            current_agent: RefCell::new(None),
            current_agent_view: RefCell::new(None),
            current_thought: RefCell::new(None),
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
            on_persist: RefCell::new(None),
            on_busy: RefCell::new(None),
            restore_notice,
            session_has_content: Cell::new(false),
            needs_auth: Cell::new(false),
            reconnect_attempts: Cell::new(0),
            mode_revert: RefCell::new(None),
            pending_prompts: RefCell::new(std::collections::VecDeque::new()),
            capture: RefCell::new(None),
            relocated: Cell::new(false),
            hosting_refusal: RefCell::new(None),
            relocation_pending: Cell::new(false),
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
                if !stick.get() {
                    if grew {
                        banner.set_reveal_child(true);
                    }
                    return;
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

        // Enter sends; Shift+Enter inserts a newline. Nothing here mentions
        // the completion list: while it is open the framework's own controller
        // takes the arrows, Escape and Enter first, and its key_activates says
        // Enter picks the highlighted command.
        {
            let controller = gtk::EventControllerKey::new();
            let weak = Rc::downgrade(&pane);
            controller.connect_key_pressed(move |_, key, _, modifier| {
                let Some(pane) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                if matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter)
                    && !modifier.contains(gtk::gdk::ModifierType::SHIFT_MASK)
                {
                    pane.send();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            entry.add_controller(controller);
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
        environment_row.connect_activated(move |_| {
            let Some(pane) = weak.upgrade() else { return };
            if pane.environment_pending.get() || pane.environment.borrow().is_some() {
                return; // the row is insensitive in both states; belt and braces
            }
            let hook = pane.on_new_environment.borrow().clone();
            if let Some(hook) = hook {
                hook();
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

    /// Wire the owning tab strip: `persist` fires when this chat's
    /// restorable identity changes (session, agent, model, permission
    /// mode), `busy` while a turn is in flight, `new_environment` when the
    /// user asks for a world of this chat's own.
    pub fn set_hooks(
        &self,
        persist: PersistHook,
        busy: BusyHook,
        new_environment: EnvironmentHook,
    ) {
        *self.on_persist.borrow_mut() = Some(persist);
        *self.on_busy.borrow_mut() = Some(busy);
        *self.on_new_environment.borrow_mut() = Some(new_environment);
    }

    /// The environment this chat's agent works in. `None` is the primary
    /// environment — the user's own checkout — not a missing value.
    pub fn environment(&self) -> Option<EnvironmentId> {
        self.environment.borrow().clone()
    }

    /// Where this chat's next agent gets spawned: its environment's
    /// checkout, socket and mode, read fresh because the mode moves under
    /// us (a container coming up unlocks the workspace mid-conversation).
    fn aim(&self) -> AgentAim {
        let environment = self
            .environment
            .borrow()
            .clone()
            .unwrap_or_else(EnvironmentId::primary);
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
    /// - this environment has a supervisor with a container up;
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
        let environment = self
            .environment
            .borrow()
            .clone()
            .unwrap_or_else(EnvironmentId::primary);
        let supervisor = self.environments.get(&environment)?;
        if !supervisor.exec().is_container() {
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
    /// a process" refused and still refuses. Safe mode has no exec target
    /// at all.
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
        let environment = self
            .environment
            .borrow()
            .clone()
            .unwrap_or_else(EnvironmentId::primary);
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
        let environment = self
            .environment
            .borrow()
            .clone()
            .unwrap_or_else(EnvironmentId::primary);
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

    /// The clone has started. Nothing is bound yet — a failure must leave
    /// the chat exactly where it was.
    pub fn environment_creating(&self, id: &EnvironmentId) {
        self.environment_pending.set(true);
        self.environment_row.set_title(&format!("Creating {id}…"));
        self.refresh_environment_row();
        self.set_status(&format!("Creating environment {id}…"));
    }

    /// The clone failed. Say so where the user asked, and put the offer
    /// back — an affordance that stays greyed out after a failure reads as
    /// a feature that broke.
    pub fn environment_failed(&self, reason: &str) {
        self.environment_pending.set(false);
        self.refresh_environment_row();
        self.meta_row(&format!("environment not created: {reason}"));
        self.set_status("Environment not created");
    }

    /// Bind this chat to its new environment and re-aim its agent at it.
    ///
    /// The conversation does not restart; the process does. The wire goes
    /// down and comes back pointed at the clone, and `session/load` carries
    /// the history across — the same continuity a devcontainer rebuild
    /// already relies on.
    pub fn bind_environment(self: &Rc<Self>, id: EnvironmentId) {
        self.environment_pending.set(false);
        *self.environment.borrow_mut() = Some(id.clone());
        self.refresh_environment_row();
        self.notify_persist();
        let resume = self
            .persisted_session
            .borrow()
            .as_ref()
            .map(|(_, session)| session.clone());
        // Controls stay: it is the same agent with the same settings,
        // working somewhere else.
        self.reset_session(false);
        self.ensure_client(resume);
        self.set_status(&format!("{} · now working in {id}", self.agent_name()));
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
        if self.busy_row.is_visible() {
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

    /// The row's three states: offering, working, bound.
    fn refresh_environment_row(&self) {
        if self.environment_pending.get() {
            self.environment_row.set_sensitive(false);
            return;
        }
        match self.environment.borrow().as_ref() {
            Some(id) => {
                self.environment_row
                    .set_title(&format!("Environment: {id}"));
                self.environment_row.set_sensitive(false);
                self.environment_row.set_tooltip_text(Some(&format!(
                    "This chat works in {id}: its own clone of the workspace, with its \
                     own devcontainer. The editor and file tree still show your checkout.",
                )));
            }
            None => {
                self.environment_row.set_title(ENVIRONMENT_OFFER);
                self.environment_row.set_sensitive(true);
            }
        }
    }

    /// Is this the tab the user is looking at? Only the selected chat
    /// raises window-level toasts, because their actions come back to
    /// whichever pane is selected.
    pub fn set_selected(&self, selected: bool) {
        self.selected.set(selected);
    }

    fn notify_persist(&self) {
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
            // `None` is a binding — the primary environment, the user's own
            // checkout — not a missing value.
            environment: self.environment.borrow().clone(),
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
        // The binding comes back before the agent does, so the first spawn
        // of a restored tab is already aimed at its own clone.
        *self.environment.borrow_mut() = entry.environment.clone();
        self.refresh_environment_row();
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
    fn set_busy(&self, busy: bool) {
        self.busy_row.set_visible(busy);
        // The tab strip mirrors this as the page's spinner, so a background
        // chat still shows it is working.
        let hook = self.on_busy.borrow().clone();
        if let Some(hook) = hook {
            hook(busy);
        }
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
        self.append_row(&label);
    }

    fn user_card(&self, text: &str, attachments: &[(String, ContentBlock)]) -> gtk::Box {
        // A prompt starts a turn, and a turn owns its plan: the next plan
        // update writes a new card below this one instead of rewriting the
        // last turn's checklist further up the transcript.
        self.plan_card.borrow_mut().take();
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
        let expander = gtk::Expander::builder()
            .label("Thinking…")
            .child(&view)
            .margin_start(6)
            .margin_end(24)
            .build();
        self.append_row(&expander);
        let buffer = view.buffer();
        *self.current_thought.borrow_mut() = Some(buffer.clone());
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
    }

    fn upsert_tool_card(
        &self,
        id: String,
        title: Option<String>,
        status: Option<ToolCallStatus>,
        content: &[ToolCallContent],
    ) {
        self.finalize_stream();
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
            let title_label = gtk::Label::builder()
                .xalign(0.0)
                .valign(gtk::Align::Center)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
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
            header.append(&status_icon);
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
                title_label,
                permission,
                content: content_box,
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
            if let Some(css) = css {
                card.status_icon.add_css_class(css);
            }
        }
        for item in content {
            match item {
                ToolCallContent::Diff(diff) => {
                    card.content.append(&diff_widget(diff));
                }
                ToolCallContent::Content(block) => {
                    if let Some(text) = content_text(&block.content) {
                        let label = gtk::Label::builder()
                            .label(text)
                            .attributes(&no_hyphens())
                            .wrap(true)
                            .xalign(0.0)
                            .selectable(true)
                            .css_classes(["caption"])
                            .build();
                        card.content.append(&label);
                    }
                }
                _ => {}
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
            content.append(&gtk::Image::from_icon_name("window-close-symbolic"));
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
            let chip = gtk::Button::builder()
                .child(&content)
                .tooltip_text(format!("Remove {label}"))
                .css_classes(["flat"])
                .build();
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
        let ready = !self.entry_text().trim().is_empty() || !self.attachments.borrow().is_empty();
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
        self.entry.buffer().set_text("");
        self.refresh_chips();
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
                    // The bar must show enough to decide on: a few lines,
                    // with the whole title in the tooltip.
                    self.permission_label.set_wrap(true);
                    self.permission_label.set_lines(4);
                    self.permission_label
                        .set_ellipsize(gtk::pango::EllipsizeMode::End);
                    self.permission_label.set_label(&title);
                    self.permission_label.set_tooltip_text(Some(&title));
                    // The buttons say what the AGENT offers rather than a
                    // generic Allow/Deny: "don't ask again" is a different
                    // answer from "yes, this once" and must not read alike.
                    let allow = allow_option(&request.options);
                    let reject = reject_option(&request.options);
                    self.allow_button
                        .set_label(allow.map_or("Allow", |o| o.name.as_str()));
                    self.deny_button
                        .set_label(reject.map_or("Deny", |o| o.name.as_str()));
                    self.allow_button.set_sensitive(allow.is_some());
                    clear_children(&self.permission_detail);
                    if let Some(content) = &request.tool_call.fields.content {
                        for item in content {
                            if let ToolCallContent::Diff(diff) = item {
                                self.permission_detail.append(&diff_widget(diff));
                            }
                        }
                    }
                    self.notify_attention(
                        "taste-permission",
                        "Claude Code needs permission",
                        &note,
                    );
                    // A newer request displaces an unanswered one: its
                    // dropped reply goes out as Cancelled, and that must
                    // not read as a user refusal on the agent's side.
                    if let Some((displaced, _)) = self
                        .pending_permission
                        .borrow_mut()
                        .replace((request, reply))
                    {
                        self.workspace.ide.record_permission(
                            &single_line(&permission_title(&displaced), 120),
                            "cancelled",
                            "a newer permission request arrived before the user \
                             answered this one; nobody refused it",
                        );
                    }
                    self.permission_bar.set_reveal_child(true);
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
                self.clear_notification("taste-permission");
                if let Some((request, _)) = self.pending_permission.borrow_mut().take() {
                    self.permission_bar.set_reveal_child(false);
                    self.workspace.ide.record_permission(
                        &single_line(&permission_title(&request), 120),
                        "cancelled",
                        "its turn ended before the user answered; nobody refused it",
                    );
                }
                if !more_queued {
                    self.notify_attention(
                        "taste-turn",
                        &format!("{} finished", self.agent_name()),
                        "The turn completed.",
                    );
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
                self.clear_notification("taste-permission");
                // Error details are transcript-worthy; clean closes are not.
                if let Some(e) = error {
                    self.notify_attention(
                        "taste-disconnect",
                        &format!("{} disconnected", self.agent_name()),
                        &e.to_string(),
                    );
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
        self.notify_attention(
            "taste-auth",
            "Sign-in required",
            &format!("{} needs you to sign in", self.agent_name()),
        );
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
                    &call.content,
                );
            }
            SessionUpdate::ToolCallUpdate(update) => {
                self.upsert_tool_card(
                    update.tool_call_id.to_string(),
                    update.fields.title.clone(),
                    update.fields.status,
                    update.fields.content.as_deref().unwrap_or(&[]),
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
    // Sent only while the window is unfocused; every notification is
    // withdrawn the moment it stops requiring a response (answered
    // permission, finished sign-in, turn seen). Informational ones also
    // clear when the window regains focus (window.rs).

    fn notify_attention(&self, id: &str, title: &str, body: &str) {
        let Some(window) = self.widget.root().and_downcast::<gtk::Window>() else {
            return;
        };
        if window.is_active() {
            return; // the user is already looking at us
        }
        let Some(app) = window.application() else {
            return;
        };
        let notification = gtk::gio::Notification::new(title);
        notification.set_body(Some(body));
        app.send_notification(Some(id), &notification);
    }

    fn clear_notification(&self, id: &str) {
        if let Some(app) = self
            .widget
            .root()
            .and_downcast::<gtk::Window>()
            .and_then(|w| w.application())
        {
            app.withdraw_notification(id);
        }
    }

    /// TASTE_PROBE_CHECK only: show what a chat bound to an environment
    /// looks like — the tab suffix and the row that names it — without
    /// cloning anything or spawning an agent. The shade is deliberately
    /// left closed: opening it hides the composer, which the probe's other
    /// checks measure.
    #[doc(hidden)]
    pub fn seed_environment_for_probe(&self, id: &str) {
        let Ok(id) = EnvironmentId::parse(id) else {
            return;
        };
        *self.environment.borrow_mut() = Some(id);
        self.refresh_environment_row();
    }

    /// TASTE_PROBE_CHECK only: sample text in the composer, so a headless
    /// screenshot shows its text colors and not just an empty box.
    #[doc(hidden)]
    /// Probe harness: replay the exact sequence that used to leave a stale
    /// checklist under a fresh prompt — plan, prompt, the SAME plan again —
    /// through the real render path, so `ide_widget_geometry` on "chat" can
    /// be counted for "plan-card" (one card, not two).
    pub fn seed_transcript_for_probe(self: &Rc<Self>) {
        use agent_client_protocol::schema::v1::{
            ContentChunk, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
        };
        let plan = || {
            Plan::new(vec![
                PlanEntry::new(
                    "Read the transcript code",
                    PlanEntryPriority::Medium,
                    PlanEntryStatus::Completed,
                ),
                PlanEntry::new(
                    "Fix the plan card",
                    PlanEntryPriority::High,
                    PlanEntryStatus::InProgress,
                ),
            ])
        };
        self.render_update(SessionUpdate::Plan(plan()));
        self.render_update(SessionUpdate::UserMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("Is chat working now?")),
        )));
        // The agent restating its plan as it picks up the prompt: no news,
        // so nothing new on screen.
        self.render_update(SessionUpdate::Plan(plan()));
    }

    pub fn seed_composer_for_probe(&self, text: &str) {
        self.entry.buffer().set_text(text);
    }

    fn answer_permission(&self, allowed: bool) {
        self.clear_notification("taste-permission");
        self.permission_bar.set_reveal_child(false);
        if let Some((request, reply)) = self.pending_permission.borrow_mut().take() {
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

/// A wait, for a badge: "8s", "1m 4s". Whole seconds — this measures a
/// queue behind a model turn, so there is nothing finer worth showing.
fn elapsed(since: std::time::Instant) -> String {
    let secs = since.elapsed().as_secs_f64().round() as u64;
    match secs {
        0..=59 => format!("{secs}s"),
        _ => format!("{}m {}s", secs / 60, secs % 60),
    }
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

/// A rendered unified diff: red for removals, green for additions.
fn diff_widget(diff: &Diff) -> gtk::Widget {
    use similar::{ChangeTag, TextDiff};

    let view = gtk::TextView::builder()
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

    let mut end = buffer.end_iter();
    buffer.insert(&mut end, &format!("{}\n", diff.path.display()));
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
    scroller.upcast()
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

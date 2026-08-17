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
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use taste_acp::session::first_allow_outcome;
use taste_acp::{builtin_agents, AgentClient, SessionEvent};
use taste_core::Workspace;

use agent_client_protocol::schema::v1::{
    AuthMethod, AvailableCommand, ContentBlock, Diff, EmbeddedResource, EmbeddedResourceResource,
    ImageContent, Plan, RequestPermissionOutcome, RequestPermissionRequest, SessionConfigKind,
    SessionConfigOption, SessionConfigSelectOptions, SessionModeId, SessionModeState,
    SessionUpdate, TextContent, TextResourceContents, ToolCallContent, ToolCallStatus, Usage,
};

const MAX_TEXT_ATTACHMENT_BYTES: u64 = 256 * 1024;
const MAX_IMAGE_ATTACHMENT_BYTES: u64 = 5 * 1024 * 1024;
const MAX_DIFF_LINES: usize = 400;
const MAX_TRANSCRIPT_ROWS: u32 = 200;

type PendingPermission = (RequestPermissionRequest, taste_acp::PermissionReply);

/// A live tool-call card in the transcript, updated in place.
struct ToolCard {
    status_icon: gtk::Image,
    title_label: gtk::Label,
    content: gtk::Box,
}

pub struct ChatPane {
    pub widget: gtk::Box,
    workspace: Workspace,
    transcript: gtk::ListBox,
    transcript_scroller: gtk::ScrolledWindow,
    entry: gtk::TextView,
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
    stop_button: gtk::Button,
    usage_bar: gtk::LevelBar,
    /// Context-window size of the applied model (drives the usage bar).
    context_limit: Cell<u64>,
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
    mcp_bridge: (String, Vec<String>),
    mcp_socket: PathBuf,
    // --- streaming state -------------------------------------------------
    current_agent: RefCell<Option<gtk::TextBuffer>>,
    current_agent_view: RefCell<Option<gtk::TextView>>,
    current_thought: RefCell<Option<gtk::TextBuffer>>,
    tool_cards: RefCell<HashMap<String, ToolCard>>,
    // --- slash commands ----------------------------------------------------
    commands: RefCell<Vec<AvailableCommand>>,
    command_popover: gtk::Popover,
    command_list: gtk::ListBox,
    /// Live transcript row count (capped; see append_row).
    transcript_rows: Cell<u32>,
    /// (agent registry id, ACP session id) — persisted for session/load.
    session_info: RefCell<Option<(String, String)>>,
    /// Re-apply automatic permissions as soon as a fresh session is ready
    /// (the escorted upgrade path for pre-auto restored sessions).
    pending_auto: Cell<bool>,
    /// Latched on AuthRequired; cleared by a completed turn. While set,
    /// Ready must NOT close the options shade over the sign-in buttons.
    needs_auth: Cell<bool>,
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
}

type Capture = (String, Box<dyn FnOnce(String)>);
type PendingPrompt = (Option<String>, gtk::Box, Option<gtk::Label>);
type ControlsSignature = Vec<(String, Vec<String>)>;

/// The agent's permission modes as a dropdown row, plus the id list its
/// indices map onto.
struct ModeControls {
    combo: adw::ComboRow,
    ids: Vec<SessionModeId>,
    auto_id: Option<SessionModeId>,
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
        mcp_bridge: (String, Vec<String>),
        mcp_socket: PathBuf,
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
        session_list.append(&agent_picker);
        session_list.append(&approval_picker);
        session_list.append(&new_session_row);

        // One status line, updated in place — connection plumbing never
        // accumulates in the transcript.
        let status_label = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .margin_start(8)
            .margin_bottom(4)
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
        {
            let placeholder = adw::StatusPage::builder()
                .icon_name("chat-message-new-symbolic")
                .title("Ask Claude Code")
                .description("Enter sends · Shift+Enter for a new line\n+ attaches context")
                .css_classes(["compact"])
                .build();
            transcript.set_placeholder(Some(&placeholder));
        }
        let transcript_scroller = gtk::ScrolledWindow::builder()
            .child(&transcript)
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
        permission_box.append(&permission_label);
        permission_box.append(&permission_detail);
        permission_box.append(&permission_buttons);
        let permission_bar = gtk::Revealer::builder().child(&permission_box).build();

        // The composer: ONE bordered card (Claude Code's shape, Adwaita's
        // skin) — chips on top, the text line, then a toolbar row inside
        // the card: attach + usage on the left, stop/send on the right.
        let entry = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            // Vertically centered caret within the 34px single line.
            .top_margin(7)
            .bottom_margin(7)
            .left_margin(8)
            .right_margin(8)
            .build();
        // An expandable multiline input: entry-styled (the same class the
        // commit box wears), one line tall until content grows it — the
        // External scrollbar policy is what prevents pre-multiline sizing.
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

        // Send is an up-arrow (core icon set — always present).
        let send = gtk::Button::builder()
            .label("Send")
            .tooltip_text("Send (Enter)")
            .css_classes(["suggested-action"])
            .build();
        let stop_button = gtk::Button::builder()
            .icon_name("media-playback-stop-symbolic")
            .tooltip_text("Stop this turn")
            .css_classes(["destructive-action", "circular"])
            .visible(false)
            .build();
        let attach_menu = gtk::gio::Menu::new();
        attach_menu.append(Some("Current Selection"), Some("chat.attach-selection"));
        attach_menu.append(Some("Active File"), Some("chat.attach-active"));
        attach_menu.append(Some("File…"), Some("chat.attach-file"));
        attach_menu.append(Some("Image…"), Some("chat.attach-image"));
        let attach_button = gtk::MenuButton::builder()
            .label("Context")
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
        field.append(&entry_inner_scroller);
        // Probe matrix verdict: NO scrollbar policy measures both states
        // (External never grows for wrapped text; Automatic/Always pin
        // 58px even empty). So measure the TextView wrap-aware ourselves
        // and drive the scroller height, like every real GTK chat app.
        {
            let measured_entry = entry.clone();
            let scroller = entry_inner_scroller.clone();
            let update = std::rc::Rc::new(move || {
                let width = scroller.width();
                if width <= 1 {
                    return; // not allocated yet; the idle below redoes it
                }
                let metrics = measured_entry.pango_context().metrics(None, None);
                let line = (metrics.ascent() + metrics.descent()) / gtk::pango::SCALE;
                let floor = line + 14; // margins
                let (_, natural, _, _) = measured_entry.measure(gtk::Orientation::Vertical, width);
                scroller.set_min_content_height(natural.max(floor).min(120));
            });
            let on_change = update.clone();
            entry.buffer().connect_changed(move |_| on_change());
            let on_map = update.clone();
            entry_inner_scroller.connect_map(move |_| {
                let on_map = on_map.clone();
                glib::idle_add_local_once(move || on_map());
            });
        }
        attach_button.set_hexpand(false);
        attach_button.set_size_request(72, -1);
        send.set_hexpand(true);
        stop_button.set_hexpand(true);
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
        let command_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        let command_popover = gtk::Popover::builder()
            .child(&command_list)
            .autohide(false)
            .has_arrow(false)
            .build();
        command_popover.set_parent(&entry_scroller);

        let entry_row = entry_scroller.clone();

        let busy_spinner = gtk::Spinner::new();
        busy_spinner.start();
        let busy_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        busy_row.set_margin_start(12);
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
        let tab_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["linked"])
            .build();
        tab_box.append(&chat_tab);
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
        let options_overlay = gtk::Overlay::new();
        options_overlay.set_vexpand(true);
        options_overlay.set_child(Some(&transcript_scroller));
        options_overlay.add_overlay(&controls_scroller);

        widget.append(&top_bar);
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(&options_overlay);
        widget.append(&busy_row);
        widget.append(&permission_bar);
        widget.append(&entry_row);

        let pane = Rc::new(Self {
            widget,
            workspace,
            transcript,
            transcript_scroller,
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
            permission_label,
            status_label,
            status_spinner: status_spinner.clone(),
            busy_row: busy_row.clone(),
            permission_detail,
            client: RefCell::new(None),
            pending_permission: RefCell::new(None),
            attachments: RefCell::new(Vec::new()),
            chips,
            stop_button: stop_button.clone(),
            usage_bar,
            context_limit: Cell::new(200_000),
            mode_sync: RefCell::new(None),
            controls_signature: RefCell::new(None),
            last_modes: RefCell::new(None),
            syncing: Cell::new(false),
            mcp_bridge,
            mcp_socket,
            current_agent: RefCell::new(None),
            current_agent_view: RefCell::new(None),
            current_thought: RefCell::new(None),
            tool_cards: RefCell::new(HashMap::new()),
            commands: RefCell::new(Vec::new()),
            command_popover,
            command_list,
            transcript_rows: Cell::new(0),
            session_info: RefCell::new(None),
            pending_auto: Cell::new(false),
            needs_auth: Cell::new(false),
            mode_revert: RefCell::new(None),
            pending_prompts: RefCell::new(std::collections::VecDeque::new()),
            capture: RefCell::new(None),
        });

        // Enter sends; Shift+Enter inserts a newline; Enter with the command
        // popover open completes the first match instead.
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
                    if pane.command_popover.is_visible() {
                        pane.complete_first_command();
                    } else {
                        pane.send();
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            entry.add_controller(controller);
        }
        // Slash-command completion follows the entry text.
        {
            let weak = Rc::downgrade(&pane);
            entry.buffer().connect_changed(move |_| {
                if let Some(pane) = weak.upgrade() {
                    pane.update_command_popover();
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
        options_toggle.connect_toggled(move |toggle| {
            let Some(pane) = weak.upgrade() else { return };
            let settings = toggle.is_active();
            pane.options_panel.set_visible(settings);
            // No chat input on the Settings tab.
            pane.composer_area.set_visible(!settings);
        });

        let weak = Rc::downgrade(&pane);
        new_session_row.connect_activated(move |_| {
            let Some(pane) = weak.upgrade() else { return };
            // Same agent, fresh conversation. Controls keep their shape
            // (disabled until Ready re-enables them) — nothing jumps.
            pane.reset_session(false);
            pane.ensure_client(None);
        });

        // Switching agents starts a fresh session (never a new window).
        let weak = Rc::downgrade(&pane);
        pane.agent_picker.connect_selected_notify(move |_| {
            let Some(pane) = weak.upgrade() else { return };
            if pane.syncing.get() {
                return;
            }
            pane.reset_session(true);
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

    /// (agent registry id, ACP session id) for restore-state persistence.
    pub fn session_info(&self) -> Option<(String, String)> {
        self.session_info.borrow().clone()
    }

    /// Send a utility prompt and hand the agent's next full reply to
    /// `on_done`. Renders in the transcript like any exchange.
    pub fn request_text(self: &Rc<Self>, prompt: String, on_done: Box<dyn FnOnce(String)>) {
        self.ensure_client(None);
        if self.client.borrow().is_none() {
            on_done(String::new());
            return;
        }
        self.finalize_stream();
        let card = self.user_card(&prompt, &[]);
        self.pending_prompts
            .borrow_mut()
            .push_back((None, card, None));
        *self.capture.borrow_mut() = Some((String::new(), on_done));
        let result = match self.client.borrow().as_ref() {
            Some(client) => client.prompt(prompt),
            None => Ok(()),
        };
        match result {
            Ok(()) => self.stop_button.set_visible(true),
            Err(e) => {
                self.meta_row(&format!("error: {e}"));
                if let Some((_, on_done)) = self.capture.borrow_mut().take() {
                    on_done(String::new());
                }
            }
        }
    }

    /// Eagerly start the default agent at startup: the user should be
    /// greeted by the sign-in invitation (or a ready session), never an
    /// inert empty box. Also front-loads the adapter download.
    pub fn connect_default(self: &Rc<Self>) {
        self.ensure_client(None);
    }

    /// Reconnect to a persisted conversation via `session/load`.
    pub fn restore_session(self: &Rc<Self>, agent_id: &str, session_id: &str) {
        let agents = builtin_agents();
        let Some(index) = agents.iter().position(|a| a.id == agent_id) else {
            return;
        };
        self.syncing.set(true);
        self.agent_picker.set_selected(index as u32);
        self.syncing.set(false);
        self.ensure_client(Some(session_id.to_string()));
    }

    fn auto_approve(&self) -> bool {
        self.approval_picker.is_active()
    }

    /// The single, in-place connection/status line.
    /// Open or close the options shade (syncs the toggle; the toggle
    /// handler moves the stack).
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

    fn agent_name(&self) -> String {
        let agents = builtin_agents();
        let index = (self.agent_picker.selected() as usize).min(agents.len() - 1);
        agents[index].display_name.clone()
    }

    /// The sign-in TUI ended. On success, drop the latch optimistically
    /// and reconnect — a wrong guess re-latches on the next failed prompt,
    /// so this can't wedge.
    pub fn on_sign_in_finished(self: &Rc<Self>, ok: bool) {
        if !ok || !self.needs_auth.get() {
            return;
        }
        self.needs_auth.set(false);
        self.auth_box.set_visible(false);
        self.show_options(false);
        self.reset_session(false);
        self.ensure_client(None);
        self.set_status(&format!(
            "{} · signed in — reconnecting…",
            self.agent_name()
        ));
    }

    /// Toast action: permanently discard the restored session. The id is
    /// forgotten on disk immediately — not at window close — so it cannot
    /// come back after a crash or kill. Then start fresh; `pending_auto`
    /// (set by the failed switch) applies Auto once the session is ready.
    pub fn destroy_stale_session(self: &Rc<Self>) {
        let root = self.workspace.root().to_path_buf();
        crate::runtime::runtime().spawn_blocking(move || {
            let mut state = taste_core::state::load(&root);
            state.session_id = None;
            if let Err(e) = taste_core::state::save(&root, &state) {
                tracing::warn!("forgetting stale session failed: {e:#}");
            }
        });
        self.reset_session(false);
        self.ensure_client(None);
    }

    /// Persist the live session id now. Waiting for window close is how
    /// stale ids kept resurrecting after unclean exits.
    fn persist_session_id(&self) {
        let root = self.workspace.root().to_path_buf();
        let info = self.session_info.borrow().clone();
        crate::runtime::runtime().spawn_blocking(move || {
            let mut state = taste_core::state::load(&root);
            state.agent_id = info.as_ref().map(|(agent, _)| agent.clone());
            state.session_id = info.map(|(_, session)| session);
            let _ = taste_core::state::save(&root, &state);
        });
    }

    /// End the session. `clear_controls` only when the control structure is
    /// obsolete (switching agents, escorted fresh session); a plain
    /// disconnect keeps the controls visible and merely disables them.
    fn reset_session(&self, clear_controls: bool) {
        self.client.borrow_mut().take();
        self.session_info.borrow_mut().take();
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
        self.busy_row.set_visible(false);
        self.current_agent.borrow_mut().take();
        self.current_thought.borrow_mut().take();
        self.tool_cards.borrow_mut().clear();
    }

    // --- transcript --------------------------------------------------------

    fn append_row(&self, child: &impl IsA<gtk::Widget>) {
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
        let adjustment = self.transcript_scroller.vadjustment();
        glib::idle_add_local_once(move || {
            adjustment.set_value(adjustment.upper());
        });
    }

    fn meta_row(&self, text: &str) {
        let label = gtk::Label::builder()
            .label(text)
            .xalign(0.5)
            .wrap(true)
            .css_classes(["dim-label", "caption"])
            .margin_top(4)
            .margin_bottom(4)
            .build();
        self.append_row(&label);
    }

    fn user_card(&self, text: &str, attachment_labels: &[&str]) -> gtk::Box {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 4);
        card.add_css_class("card");
        card.set_margin_top(4);
        card.set_margin_bottom(4);
        card.set_margin_start(24);
        card.set_margin_end(6);
        if !text.is_empty() {
            let label = gtk::Label::builder()
                .label(text)
                .wrap(true)
                .xalign(0.0)
                .hexpand(true)
                .selectable(true)
                .margin_top(8)
                .margin_bottom(8)
                .margin_start(10)
                .margin_end(10)
                .build();
            // Quick copy: reuse a prompt without hand-selecting it.
            let copy = gtk::Button::builder()
                .icon_name("edit-copy-symbolic")
                .tooltip_text("Copy prompt")
                .css_classes(["flat", "circular"])
                .valign(gtk::Align::Center)
                .margin_end(6)
                .build();
            let prompt = text.to_string();
            copy.connect_clicked(move |button| {
                button.clipboard().set_text(&prompt);
            });
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            line.append(&label);
            line.append(&copy);
            card.append(&line);
        }
        if !attachment_labels.is_empty() {
            let attached = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            attached.set_margin_start(10);
            attached.set_margin_bottom(6);
            let clip = gtk::Image::from_icon_name("mail-attachment-symbolic");
            clip.add_css_class("dim-label");
            attached.append(&clip);
            let names = gtk::Label::builder()
                .label(attachment_labels.join(", "))
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .css_classes(["dim-label", "caption"])
                .build();
            attached.append(&names);
            card.append(&attached);
        }
        self.append_row(&card);
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

    /// Close out the current streamed message: style it as markdown.
    fn finalize_stream(&self) {
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
        let mut cards = self.tool_cards.borrow_mut();
        let card = cards.entry(id).or_insert_with(|| {
            let status_icon = gtk::Image::from_icon_name("content-loading-symbolic");
            let title_label = gtk::Label::builder()
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            header.append(&status_icon);
            header.append(&title_label);
            let content_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
            let expander = gtk::Expander::builder()
                .label_widget(&header)
                .child(&content_box)
                .margin_start(6)
                .margin_end(12)
                .build();
            let frame = gtk::Frame::builder().child(&expander).build();
            frame.set_margin_top(2);
            frame.set_margin_bottom(2);
            frame.set_margin_start(6);
            frame.set_margin_end(24);
            self.append_row(&frame);
            ToolCard {
                status_icon,
                title_label,
                content: content_box,
            }
        });
        if let Some(title) = title {
            card.title_label.set_label(&title);
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
    }

    fn plan_card(&self, plan: &Plan) {
        self.finalize_stream();
        let card = gtk::Box::new(gtk::Orientation::Vertical, 2);
        card.add_css_class("card");
        card.set_margin_start(6);
        card.set_margin_end(24);
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
        if plan.entries.is_empty() {
            return;
        }
        if let Some(first) = card.first_child() {
            first.set_margin_top(6);
        }
        if let Some(last) = card.last_child() {
            last.set_margin_bottom(6);
        }
        self.append_row(&card);
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
        for (index, (label, _)) in attachments.iter().enumerate() {
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            content.append(&gtk::Image::from_icon_name("window-close-symbolic"));
            let text = gtk::Label::builder()
                .label(label)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .css_classes(["caption"])
                .build();
            content.append(&text);
            let chip = gtk::Button::builder()
                .child(&content)
                .tooltip_text("Remove this attachment")
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

    fn matching_commands(&self) -> Vec<AvailableCommand> {
        let text = self.entry_text();
        let Some(prefix) = text.strip_prefix('/') else {
            return Vec::new();
        };
        if prefix.contains('\n') || prefix.contains(' ') {
            return Vec::new();
        }
        self.commands
            .borrow()
            .iter()
            .filter(|c| c.name.starts_with(prefix))
            .take(8)
            .cloned()
            .collect()
    }

    fn update_command_popover(self: &Rc<Self>) {
        let matches = self.matching_commands();
        if matches.is_empty() {
            self.command_popover.popdown();
            return;
        }
        clear_listbox(&self.command_list);
        for command in &matches {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 0);
            row.set_margin_top(2);
            row.set_margin_bottom(2);
            let name = gtk::Label::builder()
                .label(format!("/{}", command.name))
                .xalign(0.0)
                .build();
            let description = gtk::Label::builder()
                .label(&command.description)
                .xalign(0.0)
                .css_classes(["dim-label", "caption"])
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            row.append(&name);
            row.append(&description);
            let button = gtk::Button::builder()
                .child(&row)
                .css_classes(["flat"])
                .build();
            let weak = Rc::downgrade(self);
            let insert = command.name.clone();
            button.connect_clicked(move |_| {
                if let Some(pane) = weak.upgrade() {
                    pane.entry.buffer().set_text(&format!("/{insert} "));
                    pane.command_popover.popdown();
                    pane.entry.grab_focus();
                    let buffer = pane.entry.buffer();
                    let end = buffer.end_iter();
                    buffer.place_cursor(&end);
                }
            });
            self.command_list.append(&button);
        }
        self.command_popover.popup();
    }

    fn complete_first_command(self: &Rc<Self>) {
        if let Some(first) = self.matching_commands().first() {
            self.entry.buffer().set_text(&format!("/{} ", first.name));
            let buffer = self.entry.buffer();
            let end = buffer.end_iter();
            buffer.place_cursor(&end);
        }
        self.command_popover.popdown();
    }

    // --- sending -----------------------------------------------------------

    fn send(self: &Rc<Self>) {
        let text = self.entry_text();
        if text.trim().is_empty() && self.attachments.borrow().is_empty() {
            return;
        }
        // Nothing typed is cleared until the agent is actually accepting:
        // a failed launch must not eat the prompt.
        self.ensure_client(None);
        if self.client.borrow().is_none() {
            return; // ensure_client already reported why; input intact
        }

        let attachments: Vec<(String, ContentBlock)> =
            self.attachments.borrow_mut().drain(..).collect();
        self.entry.buffer().set_text("");
        self.command_popover.popdown();
        self.refresh_chips();
        self.finalize_stream();

        let labels: Vec<&str> = attachments.iter().map(|(l, _)| l.as_str()).collect();
        let card = self.user_card(text.trim(), &labels);
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
                // Sending mid-turn is fine: the session layer queues it.
                // Say so on the card until its turn starts.
                let badge = self.stop_button.get_visible().then(|| {
                    let badge = gtk::Label::builder()
                        .label("queued — sends when the current turn ends")
                        .xalign(0.0)
                        .css_classes(["dim-label", "caption"])
                        .margin_start(10)
                        .margin_bottom(6)
                        .build();
                    card.append(&badge);
                    badge
                });
                self.pending_prompts.borrow_mut().push_back((
                    Some(text.trim().to_string()),
                    card,
                    badge,
                ));
                self.stop_button.set_visible(true);
                self.busy_row.set_visible(true);
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
        let safe_mode = !self.workspace.exec.is_container();
        self.status_spinner.start();
        self.status_label.set_visible(true);
        // The one status that earns screen space; safe mode still rides
        // along because it changes what prompts can do.
        self.status_label.set_label(if safe_mode {
            "Connecting… (safe mode)"
        } else {
            "Connecting…"
        });
        let _ = &spec.display_name;

        // AgentClient::spawn uses tokio::spawn internally; enter the runtime.
        let _guard = crate::runtime::runtime().enter();
        let client = match AgentClient::spawn(
            spec,
            self.workspace.root().to_path_buf(),
            Some(self.mcp_bridge.clone()),
            Some(self.mcp_socket.clone()),
            safe_mode,
            resume,
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
                modes,
                config_options,
            } => {
                self.auth_box.set_visible(false);
                let agent_id = self
                    .client
                    .borrow()
                    .as_ref()
                    .map(|c| c.spec.id.clone())
                    .unwrap_or_default();
                self.status_spinner.stop();
                self.status_label.set_visible(false);
                *self.session_info.borrow_mut() = Some((agent_id, session_id));
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
                // Fresh sessions default to Auto; restored ones keep
                // whatever they were left in.
                if !restored && !self.needs_auth.get() {
                    let auto = self
                        .mode_sync
                        .borrow()
                        .as_ref()
                        .and_then(|c| c.auto_id.clone());
                    if let Some(auto) = auto {
                        let differs = self
                            .last_modes
                            .borrow()
                            .as_ref()
                            .is_some_and(|m| m.current_mode_id != auto);
                        if differs {
                            if let Some(client) = self.client.borrow().as_ref() {
                                let _ = client.set_mode(auto.clone());
                            }
                            if let Some(state) = self.last_modes.borrow_mut().as_mut() {
                                state.current_mode_id = auto;
                            }
                            self.sync_mode_widgets();
                        }
                    }
                }
                // The escorted path: the user asked for automatic
                // permissions and we started this fresh session for it.
                if self.pending_auto.replace(false) {
                    let auto = self
                        .mode_sync
                        .borrow()
                        .as_ref()
                        .and_then(|c| c.auto_id.clone());
                    if let Some(auto) = auto {
                        let result = match self.client.borrow().as_ref() {
                            Some(client) => client.set_mode(auto.clone()),
                            None => Ok(()),
                        };
                        if result.is_ok() {
                            if let Some(state) = self.last_modes.borrow_mut().as_mut() {
                                state.current_mode_id = auto;
                            }
                            self.sync_mode_widgets();
                        }
                    }
                }
            }
            SessionEvent::Update(update) => self.render_update(update),
            SessionEvent::Permission { request, reply } => {
                self.finalize_stream();
                if self.auto_approve() {
                    let title = permission_title(&request);
                    let _ = reply.send(first_allow_outcome(&request));
                    self.meta_row(&format!("auto-approved: {title}"));
                } else {
                    self.permission_label.set_label(&permission_title(&request));
                    clear_children(&self.permission_detail);
                    if let Some(content) = &request.tool_call.fields.content {
                        for item in content {
                            if let ToolCallContent::Diff(diff) = item {
                                self.permission_detail.append(&diff_widget(diff));
                            }
                        }
                    }
                    *self.pending_permission.borrow_mut() = Some((request, reply));
                    self.permission_bar.set_reveal_child(true);
                }
            }
            SessionEvent::PromptFailed { message } => {
                self.finalize_stream();
                self.stop_button.set_visible(false);
                self.busy_row.set_visible(false);
                if let Some((_, on_done)) = self.capture.borrow_mut().take() {
                    on_done(String::new());
                }
                // A rejected prompt is not part of the conversation: take
                // its card out of the transcript and hand the text back.
                if let Some((restore, card, _)) = self.pending_prompts.borrow_mut().pop_front() {
                    if let Some(row) = card.parent() {
                        self.transcript.remove(&row);
                        self.transcript_rows
                            .set(self.transcript_rows.get().saturating_sub(1));
                    }
                    if let Some(text) = restore {
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
                let next = {
                    let mut pending = self.pending_prompts.borrow_mut();
                    pending
                        .front_mut()
                        .and_then(|(_, card, badge)| badge.take().map(|b| (card.clone(), b)))
                };
                match next {
                    Some((card, badge)) => {
                        card.remove(&badge);
                        self.set_status(&format!("{} · working…", self.agent_name()));
                    }
                    None => self.stop_button.set_visible(false),
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
                self.set_status(&format!("{} · ready", self.agent_name()));
                if let Some(usage) = usage {
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
                    self.pending_auto.set(true);
                    self.workspace
                        .events
                        .publish(taste_core::Event::ToastAction {
                            message: "This restored session can't switch to Auto".into(),
                            label: "Destroy Old Session".into(),
                            action: "chat-destroy-session".into(),
                        });
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
                self.busy_row.set_visible(false);
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
                // Error details are transcript-worthy; clean closes are not.
                if let Some(e) = error {
                    self.meta_row(&format!("connection closed: {e}"));
                }
                // Unfinished prompts go back to the composer, not the log.
                let pending: Vec<PendingPrompt> =
                    self.pending_prompts.borrow_mut().drain(..).collect();
                let mut restored: Vec<String> = Vec::new();
                for (restore, card, _) in pending {
                    if let Some(row) = card.parent() {
                        self.transcript.remove(&row);
                        self.transcript_rows
                            .set(self.transcript_rows.get().saturating_sub(1));
                    }
                    if let Some(text) = restore {
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
                // Disconnect is not a different agent: keep the controls
                // on screen, just disabled.
                self.reset_session(false);
            }
        }
    }

    /// Sign-in required: one button per method the agent offers.
    fn show_auth(self: &Rc<Self>, methods: Vec<AuthMethod>) {
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
                        let mut args = spec.args.clone();
                        args.extend(terminal.args.iter().cloned());
                        let mut env = spec.env.clone();
                        env.extend(terminal.env.iter().map(|(k, v)| (k.clone(), v.clone())));
                        pane.workspace
                            .events
                            .publish(taste_core::Event::RunInTerminal {
                                title: "Sign In".into(),
                                program: spec.command.clone(),
                                args,
                                env,
                            });
                        pane.set_status(&format!(
                            "{} · finish signing in below, then send your prompt again",
                            pane.agent_name()
                        ));
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
            // Some agents expose their session mode BOTH as modes state and
            // as a "mode" config option; one "Permissions" group is enough.
            if has_modes_row
                && (option.id.to_string().eq_ignore_ascii_case("mode")
                    || option.name.eq_ignore_ascii_case("mode"))
            {
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
                            if let Err(e) = result {
                                pane.meta_row(&format!("error: {e}"));
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
            controls.combo.set_selected(index as u32);
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
        let combo = adw::ComboRow::builder()
            .title("Mode")
            .model(&gtk::StringList::new(&name_refs))
            .build();
        if let Some(index) = ids.iter().position(|id| *id == state.current_mode_id) {
            self.syncing.set(true);
            combo.set_selected(index as u32);
            self.syncing.set(false);
        }
        {
            let weak = Rc::downgrade(self);
            let ids = ids.clone();
            let descriptions: Vec<Option<String>> = state
                .available_modes
                .iter()
                .map(|m| m.description.clone())
                .collect();
            combo.connect_selected_notify(move |combo| {
                let Some(pane) = weak.upgrade() else { return };
                let index = combo.selected() as usize;
                if let Some(description) = descriptions.get(index).and_then(|d| d.clone()) {
                    combo.set_subtitle(&description);
                }
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
                        if let Some(state) = pane.last_modes.borrow_mut().as_mut() {
                            state.current_mode_id = id;
                        }
                    }
                    Err(e) => pane.meta_row(&format!("error: {e}")),
                }
            });
        }
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        list.append(&combo);
        self.controls.append(&list);
        *self.mode_sync.borrow_mut() = Some(ModeControls {
            combo,
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
        let persisted = taste_core::state::load(self.workspace.root()).model_value;
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
                        // Project-level persistence: this choice survives
                        // restarts and re-applies to future sessions.
                        if let Some(value) = value {
                            pane.context_limit.set(if value.contains("[1m]") {
                                1_000_000
                            } else {
                                200_000
                            });
                            let root = pane.workspace.root().to_path_buf();
                            let mut state = taste_core::state::load(&root);
                            state.model_value = Some(value);
                            if let Err(e) = taste_core::state::save(&root, &state) {
                                tracing::warn!("persisting model choice failed: {e:#}");
                            }
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
        match update {
            SessionUpdate::UserMessageChunk(chunk) => {
                // Replayed history (session/load): user messages arrive as
                // chunks too.
                if let Some(text) = content_text(&chunk.content) {
                    self.finalize_stream();
                    self.user_card(&text, &[]);
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
            SessionUpdate::Plan(plan) => self.plan_card(&plan),
            SessionUpdate::AvailableCommandsUpdate(update) => {
                *self.commands.borrow_mut() = update.available_commands;
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

    fn answer_permission(&self, allowed: bool) {
        self.permission_bar.set_reveal_child(false);
        if let Some((request, reply)) = self.pending_permission.borrow_mut().take() {
            // The click leaves a record: back-to-back requests can look
            // identical, and a silent Allow reads as a dead button.
            self.meta_row(&format!(
                "{} — {}",
                if allowed { "allowed" } else { "denied" },
                permission_title(&request)
            ));
            let outcome = if allowed {
                first_allow_outcome(&request)
            } else {
                RequestPermissionOutcome::Cancelled
            };
            let _ = reply.send(outcome);
        }
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

fn clear_listbox(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
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
fn format_usage(usage: &Usage) -> String {
    fn k(n: u64) -> String {
        if n >= 1_000_000 {
            format!("{:.1}M", n as f64 / 1_000_000.0)
        } else if n >= 1_000 {
            format!("{:.1}k", n as f64 / 1_000.0)
        } else {
            n.to_string()
        }
    }
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

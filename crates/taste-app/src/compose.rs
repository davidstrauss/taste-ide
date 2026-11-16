//! The universal composer: one box, one place, three destinations.
//!
//! David, 2026-09-07: "a universal composition and dispatch box, that
//! actually only has one position in the interface, but allows speech
//! input, attachments, and then, using various keyboard or controller
//! commands allows sending it to either the backlog, as a commit message
//! for stuff that is currently staged, or sending it to chat." It sits
//! under the chat as a section of its own, folded like the flank's.
//!
//! **How the dispatch is directed.** Three buttons under the field, one per
//! destination, each sending directly (David, 2026-09-08: "each dispatch
//! option to have its own button under the compose box that directly
//! sends"): **Send to Chat** rightmost, where Enter and the controller's A
//! go; **To backlog** and **Commit** beside it, which are HOLDS — F5 or B
//! held files the issue, F6 or Y held commits — because a slip must not
//! file or commit a prompt, and a tap only lights the button to say so.
//! Not autodetection, which would make Enter mean something different
//! depending on what the project is doing; not a mode, which a half-typed
//! prompt would be sent by. A destination the draft cannot go to is
//! disabled and says why.
//!
//! **The controller** (Xbox layout): X taps focus the box, X held dictates
//! into it for as long as it is held, A sends to the chat, B held files,
//! Y held commits. Ctrl+D is the keyboard's X.
//!
//! **An issue's brief, drafted.** The user's Start brings an issue's
//! environment up with its agent idle and puts the brief Start used to send
//! here instead, aimed at that environment ([`Handoffs`]): Enter hands the
//! issue to the agent, and a draft deleted or ignored leaves the environment
//! the user's (David, 2026-11-15: "I want to be able to work in these other
//! environments without an agent taking over"). The header says whose brief
//! the box holds while it does.
//!
//! Nothing here knows how a chat sends, how an issue is filed or how a
//! commit is written: the window hands in three hooks.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::composer::{Attachment, Composer};
use taste_core::environment::EnvironmentId;
use taste_core::{ControllerButton, Event, Workspace};

/// The chat column's inset: where the transcript's cards stand, and so
/// where this section's header and box stand with them.
const COLUMN_INSET: i32 = 12;

/// Where a draft can go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    Chat,
    Backlog,
    Commit,
}

impl Destination {
    /// The three, in the order the buttons stand in; the tests walk it.
    #[cfg(test)]
    pub const ORDER: [Destination; 3] =
        [Destination::Chat, Destination::Backlog, Destination::Commit];

    pub fn label(self) -> &'static str {
        match self {
            Destination::Chat => "Chat",
            Destination::Backlog => "Backlog",
            Destination::Commit => "Commit",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Destination::Chat => "chat-message-new-symbolic",
            Destination::Backlog => crate::backlog::BACKLOG_ICON,
            Destination::Commit => "object-select-symbolic",
        }
    }

    /// The key that sends to it — held, for the two that are not the chat
    /// ([`Destination::held`]) — and the controller button that does. The
    /// tests pin them; the UI writes them only in F1's bubbles (reveal.rs).
    #[cfg(test)]
    pub fn key(self) -> &'static str {
        match self {
            Destination::Chat => "Enter",
            Destination::Backlog => "F5",
            Destination::Commit => "F6",
        }
    }

    /// Whether the key and the button must be held to send here.
    #[cfg(test)]
    pub fn held(self) -> bool {
        self != Destination::Chat
    }

    #[cfg(test)]
    pub fn button(self) -> &'static str {
        match self {
            Destination::Chat => "A",
            Destination::Backlog => "B",
            Destination::Commit => "Y",
        }
    }
}

/// What the box holds, in the terms a destination judges it by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Draft {
    pub has_text: bool,
    pub attachments: usize,
}

/// Whether a message typed now has a chat to go to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatReadiness {
    /// The environment on screen exists, and its chat — or the one a send
    /// starts there (`Chats::chat_for_send`) — has what it spends on.
    Ready,
    /// The environment on screen no longer exists.
    Gone,
    /// The agent behind the proxy, with no credential in this project:
    /// a turn could only be refused (`ChatPane::unconfigured`).
    NoCredential,
}

/// What the window is doing, in the terms a destination judges by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Surroundings {
    pub staged: usize,
    /// The Staged view has files unchecked: a commit takes the whole
    /// index, so there is no committing a selection.
    pub commit_blocked: bool,
    /// Whether the chat can take a message. Whether the environment has
    /// an agent yet does not matter: a send to one with none starts it
    /// there (`Chats::chat_for_send`), which is what its empty state
    /// invites.
    pub chat: ChatReadiness,
}

/// Whether `destination` can take the draft now, or the reason it cannot —
/// the sentence its disabled button carries (David: "If what's composed
/// isn't possible for some use (e.g., an image is attached), disable that
/// destination").
pub fn availability(
    destination: Destination,
    draft: Draft,
    surroundings: Surroundings,
) -> Result<(), &'static str> {
    match destination {
        Destination::Chat => {
            match surroundings.chat {
                ChatReadiness::Ready => {}
                ChatReadiness::Gone => {
                    return Err(
                        "the environment on screen no longer exists — pick another in the backlog",
                    )
                }
                ChatReadiness::NoCredential => {
                    return Err(
                        "this chat's agent has no credential in this project — add one in the \
                         chat's Settings",
                    )
                }
            }
            if !draft.has_text && draft.attachments == 0 {
                return Err("nothing to send yet");
            }
            Ok(())
        }
        Destination::Backlog => {
            if !draft.has_text {
                return Err("an issue needs a title — the first line");
            }
            Ok(())
        }
        Destination::Commit => {
            if surroundings.staged == 0 {
                return Err("nothing is staged");
            }
            if surroundings.commit_blocked {
                return Err(
                    "a commit takes every staged file — select them all in the Staged view, \
                     or unstage the ones to leave out",
                );
            }
            if draft.attachments > 0 {
                return Err("a commit message cannot carry attachments");
            }
            if !draft.has_text {
                return Err("a commit needs a message");
            }
            Ok(())
        }
    }
}

/// The briefs the user's Start drafted, and which one the box holds.
///
/// The box is one box for every environment, so a brief is aimed by where
/// it is shown: it is in the box only while the panes are aimed at its
/// environment. Going elsewhere parks it — as the user left it, edited or
/// not — and arriving back with the box empty brings it back. It is never
/// written over the user's own words: a brief drafted while the box holds
/// something waits, parked, for the next arrival at its environment with
/// the box empty. What ends a brief is the user's: emptying the box, which
/// leaves the environment theirs, or sending it — to the chat, which hands
/// the issue over, or anywhere else, which uses the words. A message of
/// the user's own sent to that environment's chat ends a parked one, since
/// they have chosen to work there.
#[derive(Debug, Default)]
pub struct Handoffs {
    parked: std::collections::HashMap<EnvironmentId, String>,
    in_box: Option<EnvironmentId>,
    aim: Option<EnvironmentId>,
}

/// What [`Handoffs`] asks of the box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoxEdit {
    Leave,
    /// Put this brief in the empty box, the caret at its end.
    Fill(String),
    /// Take the parked brief out: the box is empty again.
    Clear,
}

impl Handoffs {
    /// Start drafted `text` for `env`; `box_text` is what the box holds.
    pub fn offer(&mut self, env: &EnvironmentId, text: &str, box_text: &str) -> BoxEdit {
        if self.in_box.as_ref() == Some(env) {
            return BoxEdit::Leave;
        }
        self.parked.insert(env.clone(), text.to_string());
        self.arrive(box_text)
    }

    /// The panes are aimed at `env` now.
    pub fn aim(&mut self, env: &EnvironmentId, box_text: &str) -> BoxEdit {
        if self.aim.as_ref() == Some(env) {
            return BoxEdit::Leave;
        }
        self.aim = Some(env.clone());
        if let Some(held) = self.in_box.take() {
            if !box_text.trim().is_empty() {
                self.parked.insert(held, box_text.to_string());
            }
            return match self.arrive("") {
                BoxEdit::Leave => BoxEdit::Clear,
                fill => fill,
            };
        }
        self.arrive(box_text)
    }

    /// The parked brief for where the panes are, into an empty box.
    fn arrive(&mut self, box_text: &str) -> BoxEdit {
        let Some(aim) = self.aim.clone() else {
            return BoxEdit::Leave;
        };
        if !box_text.trim().is_empty() {
            return BoxEdit::Leave;
        }
        match self.parked.remove(&aim) {
            Some(text) => {
                self.in_box = Some(aim);
                BoxEdit::Fill(text)
            }
            None => BoxEdit::Leave,
        }
    }

    /// The box changed: emptied, the brief in it is gone.
    pub fn edited(&mut self, box_text: &str) {
        if box_text.trim().is_empty() {
            self.in_box = None;
        }
    }

    /// The environment whose brief the box holds, if it holds one.
    pub fn in_box(&self) -> Option<&EnvironmentId> {
        self.in_box.as_ref()
    }

    /// What the box held went to `destination`. Says which environment's
    /// brief went to the chat, if one did; a send of the user's own to the
    /// chat ends the brief parked for where the panes are.
    pub fn sent(&mut self, destination: Destination) -> Option<EnvironmentId> {
        let held = self.in_box.take();
        if destination != Destination::Chat {
            return None;
        }
        if held.is_none() {
            if let Some(aim) = &self.aim {
                self.parked.remove(aim);
            }
        }
        held
    }

    /// `env` is gone: so is its brief.
    pub fn forget(&mut self, env: &EnvironmentId) -> BoxEdit {
        self.parked.remove(env);
        if self.in_box.as_ref() == Some(env) {
            self.in_box = None;
            return BoxEdit::Clear;
        }
        BoxEdit::Leave
    }
}

/// The header's words while the box holds `env`'s brief.
pub fn handoff_caption(env: &EnvironmentId) -> String {
    format!("{env}'s brief, unsent")
}

/// The button's words for the destination — fixed, so three buttons keep
/// fitting the chat column's floor; the staged count is the tooltip's
/// ([`staged_words`]).
pub fn verb(destination: Destination) -> &'static str {
    match destination {
        Destination::Chat => "Send to Chat",
        Destination::Backlog => "Backlog",
        Destination::Commit => "Commit",
    }
}

/// What a commit would take, for Commit's tooltip.
pub fn staged_words(staged: usize) -> String {
    match staged {
        0 => "nothing staged".to_string(),
        1 => "1 staged file".to_string(),
        n => format!("{n} staged files"),
    }
}

/// Whether Enter sends now, away from any widget — the part worth testing.
/// While an input method is composing, Enter COMMITS the composition; it
/// does not end the sentence. Sending there truncates the message mid-word,
/// unrecoverably, and it happens on ordinary typing for every CJK user — so
/// a preedit in progress claims Enter before Shift does.
fn enter_sends(shift: bool, preedit: bool) -> bool {
    !shift && !preedit
}

const PLACEHOLDER: &str = "A message, an issue, or a commit message";

/// The send glyph, said once for the three buttons rather than on each.
pub(crate) const SEND_ICON: &str = "document-send-symbolic";

/// A destination's button face: its own glyph alone. The send glyph used
/// to open every button (David, 2026-09-08: "'<Send icon> <Type icon>' on
/// the buttons"); it stands before the set now, outside the buttons, so
/// three equal buttons read as three ways to send rather than three
/// things that each half-say it (David, 2026-09-16). The words are the
/// tooltip's.
fn button_content(destination: Destination) -> gtk::Image {
    gtk::Image::from_icon_name(destination.icon())
}

/// The glyph that stands before the set: what all three buttons do, said
/// once. Dim, because it is a caption on the buttons and not a fourth one.
fn send_glyph() -> gtk::Image {
    let glyph = gtk::Image::from_icon_name(SEND_ICON);
    glyph.add_css_class("dim-label");
    glyph.set_tooltip_text(Some(
        "Send — to the chat with Enter, to the backlog with F5 held, as a commit with F6 held",
    ));
    glyph
}

/// How long a key or button is down before a tap becomes a hold.
const HOLD: std::time::Duration = std::time::Duration::from_millis(350);
/// Two taps this close together are one gesture: clear the field (David,
/// 2026-09-08: "A rapid double-tap of Ctrl-D/F (or, always, controller
/// equivalent) clears the corresponding area").
const DOUBLE_TAP: std::time::Duration = std::time::Duration::from_millis(350);

/// A press that is a tap, a hold, or the second tap of a pair: X and Start
/// on the controller, Ctrl+D and Ctrl+F on the keyboard. `press` starts
/// the clock and runs `on_hold` once it has run out with the key still
/// down — and says whether this press came within [`DOUBLE_TAP`] of the
/// last tap's release, which is the caller's cue to clear; `release` says
/// which the press turned out to be, so the caller can finish the hold
/// (stop and transcribe) or do the tap's thing.
pub struct Hold {
    down: Cell<bool>,
    timer: RefCell<Option<glib::SourceId>>,
    held: Cell<bool>,
    last_tap: Cell<Option<std::time::Instant>>,
}

/// What a release was the end of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    /// Up before the clock ran out.
    Tap,
    /// Up after `on_hold` ran.
    Held,
    /// Up with nothing down — a repeat, or a release the press of which
    /// went elsewhere.
    Idle,
}

impl Default for Hold {
    fn default() -> Self {
        Self::new()
    }
}

impl Hold {
    pub fn new() -> Self {
        Self {
            down: Cell::new(false),
            timer: RefCell::new(None),
            held: Cell::new(false),
            last_tap: Cell::new(None),
        }
    }

    pub fn is_down(&self) -> bool {
        self.down.get()
    }

    /// True when this press is the second of a double tap.
    pub fn press(self: &Rc<Self>, on_hold: impl FnOnce() + 'static) -> bool {
        if self.down.replace(true) {
            return false; // autorepeat
        }
        self.held.set(false);
        let double = self
            .last_tap
            .take()
            .is_some_and(|at| at.elapsed() <= DOUBLE_TAP);
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(HOLD, move || {
            let Some(hold) = weak.upgrade() else { return };
            hold.timer.borrow_mut().take();
            if hold.down.get() {
                hold.held.set(true);
                on_hold();
            }
        });
        if let Some(previous) = self.timer.borrow_mut().replace(source) {
            previous.remove();
        }
        double
    }

    pub fn release(&self) -> Release {
        if !self.down.replace(false) {
            return Release::Idle;
        }
        if let Some(pending) = self.timer.borrow_mut().take() {
            pending.remove();
        }
        if self.held.replace(false) {
            Release::Held
        } else {
            self.last_tap.set(Some(std::time::Instant::now()));
            Release::Tap
        }
    }
}

pub type ChatHook = Box<dyn Fn(String, Vec<Attachment>) -> Result<(), String>>;
pub type BacklogHook = Box<dyn Fn(String, String, Vec<Attachment>) -> Result<(), String>>;
pub type CommitHook = Box<dyn Fn(String) -> Result<(), String>>;
/// The brief drafted for an environment went to its chat (`true`), or that
/// send was refused (`false`).
pub type HandoffHook = Box<dyn Fn(&EnvironmentId, bool)>;

pub struct Compose {
    /// The whole section: header, then the folding body.
    pub widget: gtk::Box,
    body: gtk::Box,
    composer: Rc<Composer>,
    /// One button per destination, each sending straight there.
    buttons: Vec<(Destination, gtk::Button)>,
    staged: Cell<usize>,
    commit_blocked: Cell<bool>,
    chat_available: RefCell<Option<Box<dyn Fn() -> ChatReadiness>>>,
    on_chat: RefCell<Option<ChatHook>>,
    on_backlog: RefCell<Option<BacklogHook>>,
    on_commit: RefCell<Option<CommitHook>>,
    on_handoff: RefCell<Option<HandoffHook>>,
    /// The briefs the user's Start drafted, and which the box holds.
    handoffs: RefCell<Handoffs>,
    /// The header's word on a brief in the box: whose it is, unsent.
    handoff_caption: gtk::Label,
    /// The last text sent, for Up in an empty box.
    last_sent: RefCell<Option<String>>,
    /// An input method is mid-composition in the box. Enter belongs to the
    /// IM while this is set — see the key controller below.
    preedit: Cell<bool>,
    /// The slash-command completion of the chat this box is talking to.
    provider: RefCell<Option<crate::command_completion::CommandProvider>>,
    events: taste_core::EventBus,
    /// X down: a tap focuses, a hold dictates until released.
    x_hold: Rc<Hold>,
    /// B and Y down: a hold sends, a tap lights the button.
    b_hold: Rc<Hold>,
    y_hold: Rc<Hold>,
}

impl Compose {
    pub fn new(workspace: &Workspace) -> Rc<Self> {
        let header = crate::filetree::section_header("taste-compose-symbolic", "Dispatch");
        // On the CHAT column, not the flank's. `section_header` insets to
        // the flank's chrome column (14, which is where its rows stand);
        // everything in this column — the transcript's cards, the box under
        // this header — stands at 12, so the header imported a two-pixel
        // disagreement with the only things it is ever seen against
        // (`near-miss.py`).
        header.set_margin_start(COLUMN_INSET);
        header.set_margin_end(COLUMN_INSET);
        if let Some(title) = header.last_child() {
            title.set_hexpand(true);
            // The title yields first at the chat column's floor.
            if let Ok(label) = title.downcast::<gtk::Label>() {
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                crate::hover::full_text_on_hover(&label);
            }
        }
        // Whose brief the box holds, at the header's end while it holds
        // one: the draft is in the box to be read and edited, and this is
        // what says it is a draft Start wrote and not words of the user's.
        let handoff_caption = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .xalign(1.0)
            .visible(false)
            .tooltip_text(
                "Start wrote the issue's brief here instead of sending it. Send it to hand \
                 the issue to the agent, or clear the box to work in the environment yourself.",
            )
            .build();
        header.append(&handoff_caption);
        // The three buttons, in the composer's action row: To backlog and
        // Commit between the microphone and the pill, Send to Chat the
        // pill itself, rightmost.
        let mut buttons: Vec<(Destination, gtk::Button)> = Vec::new();
        let mut extras = Vec::new();
        for destination in [Destination::Backlog, Destination::Commit] {
            let button = gtk::Button::builder()
                .child(&button_content(destination))
                .build();
            extras.push(button.clone());
            buttons.push((destination, button));
        }
        let composer = Composer::new(workspace, verb(Destination::Chat), &extras);
        composer
            .primary
            .set_child(Some(&button_content(Destination::Chat)));
        // Three equal buttons at the row's end, the send glyph before them.
        composer.trail_actions(&send_glyph());
        buttons.push((Destination::Chat, composer.primary.clone()));
        composer.set_placeholder(PLACEHOLDER);
        composer.widget.set_margin_start(COLUMN_INSET);
        composer.widget.set_margin_end(COLUMN_INSET);
        // The header's margin is the gap, as for the flank's sections.
        composer.widget.set_margin_top(1);
        composer.widget.set_margin_bottom(4);
        // No hint under the box (David, 2026-09-08: "There shouldn't be
        // keyboard shortcuts shown below the dispatch panel"): F1 and the
        // controller's logo button are where the keys are written
        // (reveal.rs), and the buttons' tooltips say theirs.
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&composer.widget);
        crate::filetree::wire_collapse(&header, &body);

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.set_widget_name("compose");
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(&header);
        widget.append(&body);

        let compose = Rc::new(Self {
            widget,
            body,
            composer: composer.clone(),
            buttons,
            staged: Cell::new(0),
            commit_blocked: Cell::new(false),
            chat_available: RefCell::new(None),
            on_chat: RefCell::new(None),
            on_backlog: RefCell::new(None),
            on_commit: RefCell::new(None),
            on_handoff: RefCell::new(None),
            handoffs: RefCell::new(Handoffs::default()),
            handoff_caption,
            last_sent: RefCell::new(None),
            preedit: Cell::new(false),
            provider: RefCell::new(None),
            events: workspace.events.clone(),
            x_hold: Rc::new(Hold::new()),
            b_hold: Rc::new(Hold::new()),
            y_hold: Rc::new(Hold::new()),
        });

        for (destination, button) in &compose.buttons {
            let weak = Rc::downgrade(&compose);
            let destination = *destination;
            button.connect_clicked(move |_| {
                if let Some(compose) = weak.upgrade() {
                    compose.dispatch(destination);
                }
            });
        }
        {
            let weak = Rc::downgrade(&compose);
            composer.set_on_change(move || {
                if let Some(compose) = weak.upgrade() {
                    let text = compose.composer.text();
                    compose.handoffs.borrow_mut().edited(&text);
                    compose.sync_handoff();
                    compose.sync();
                }
            });
        }
        {
            let events = workspace.events.clone();
            composer.set_on_notice(move |text| events.publish(Event::Toast(text)));
        }
        // An input method's composition, tracked so Enter can stay out of
        // its way. GtkTextView announces preedit changes; an empty string is
        // the composition ending. Without this, Enter-to-send fires on the
        // keystroke that COMMITS a composition and sends a half-typed word —
        // every CJK user, every time they type.
        {
            let weak = Rc::downgrade(&compose);
            composer.entry.connect_preedit_changed(move |_, text| {
                if let Some(compose) = weak.upgrade() {
                    compose.preedit.set(!text.is_empty());
                }
            });
        }
        // Enter sends to the chat; Shift+Enter is a new line; Up in an
        // empty box brings the last text back. Escape leaves nothing from
        // this box — it does not stop a streaming turn and it does not deny
        // a permission card, both of which stay on their buttons — so it
        // gets no arm at all and propagates.
        {
            let controller = gtk::EventControllerKey::new();
            let weak = Rc::downgrade(&compose);
            controller.connect_key_pressed(move |_, key, _, modifier| {
                let Some(compose) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                use gtk::gdk::Key;
                let shift = modifier.contains(gtk::gdk::ModifierType::SHIFT_MASK);
                match key {
                    Key::Return | Key::KP_Enter if enter_sends(shift, compose.preedit.get()) => {
                        compose.dispatch(Destination::Chat);
                        glib::Propagation::Stop
                    }
                    Key::Up if compose.composer.text().trim().is_empty() => {
                        let last = compose.last_sent.borrow().clone();
                        match last {
                            Some(text) => {
                                compose.composer.set_text(&text);
                                let buffer = compose.composer.entry.buffer();
                                let end = buffer.end_iter();
                                buffer.place_cursor(&end);
                                glib::Propagation::Stop
                            }
                            None => glib::Propagation::Proceed,
                        }
                    }
                    _ => glib::Propagation::Proceed,
                }
            });
            composer.entry.add_controller(controller);
        }
        compose.sync();
        compose
    }

    // --- the window's hooks ------------------------------------------------

    pub fn set_on_chat(
        &self,
        hook: impl Fn(String, Vec<Attachment>) -> Result<(), String> + 'static,
    ) {
        *self.on_chat.borrow_mut() = Some(Box::new(hook));
    }

    pub fn set_on_backlog(
        &self,
        hook: impl Fn(String, String, Vec<Attachment>) -> Result<(), String> + 'static,
    ) {
        *self.on_backlog.borrow_mut() = Some(Box::new(hook));
    }

    pub fn set_on_commit(&self, hook: impl Fn(String) -> Result<(), String> + 'static) {
        *self.on_commit.borrow_mut() = Some(Box::new(hook));
    }

    /// Who is told that a drafted brief went to its chat, or that the send
    /// was refused.
    pub fn set_on_handoff(&self, hook: impl Fn(&EnvironmentId, bool) + 'static) {
        *self.on_handoff.borrow_mut() = Some(Box::new(hook));
    }

    // --- the drafted briefs -------------------------------------------------

    /// The user's Start drafted `text` for `env` ([`Handoffs`]).
    pub fn offer_handoff(&self, env: &EnvironmentId, text: &str) {
        let edit = self
            .handoffs
            .borrow_mut()
            .offer(env, text, &self.composer.text());
        self.apply(edit);
    }

    /// The panes are aimed at `env` now: a brief follows its environment.
    pub fn follow_aim(&self, env: &EnvironmentId) {
        let edit = self.handoffs.borrow_mut().aim(env, &self.composer.text());
        self.apply(edit);
    }

    /// `env` was destroyed: its brief goes with it.
    pub fn forget_handoff(&self, env: &EnvironmentId) {
        let edit = self.handoffs.borrow_mut().forget(env);
        self.apply(edit);
    }

    /// Do what [`Handoffs`] asked of the box — outside its borrow, since
    /// changing the text tells it of the edit.
    fn apply(&self, edit: BoxEdit) {
        match edit {
            BoxEdit::Leave => {}
            BoxEdit::Fill(text) => {
                self.composer.set_text(&text);
                // The caret at the end, where Enter sends it as it stands.
                let buffer = self.composer.entry.buffer();
                buffer.place_cursor(&buffer.end_iter());
            }
            BoxEdit::Clear => self.composer.set_text(""),
        }
        self.sync_handoff();
    }

    /// The header's caption, as the box stands.
    fn sync_handoff(&self) {
        match self.handoffs.borrow().in_box() {
            Some(env) => {
                self.handoff_caption.set_label(&handoff_caption(env));
                self.handoff_caption.set_visible(true);
            }
            None => self.handoff_caption.set_visible(false),
        }
    }

    /// Whether the chat on screen can take a message, asked at every
    /// judging of the buttons.
    pub fn set_chat_available(&self, probe: impl Fn() -> ChatReadiness + 'static) {
        *self.chat_available.borrow_mut() = Some(Box::new(probe));
        self.sync();
    }

    /// Judge the buttons again: something they depend on changed outside
    /// the box — an agent connected, a credential was saved, another
    /// environment came on screen. Judged only on the box's own changes,
    /// Send to Chat stayed dim over a draft until the next keystroke.
    pub fn resync(&self) {
        self.sync();
    }

    /// The Staged view's facts: how many files, and whether a commit is
    /// blocked by an unchecked one.
    pub fn set_staged(&self, staged: usize, blocked: bool) {
        self.staged.set(staged);
        self.commit_blocked.set(blocked);
        self.sync();
    }

    /// The chat this box talks to changed: its slash commands complete here.
    pub fn set_command_provider(
        &self,
        provider: Option<&crate::command_completion::CommandProvider>,
    ) {
        let completion = sourceview5::prelude::ViewExt::completion(&self.composer.entry);
        if let Some(previous) = self.provider.borrow_mut().take() {
            completion.remove_provider(&previous);
        }
        if let Some(provider) = provider {
            completion.add_provider(provider);
            *self.provider.borrow_mut() = Some(provider.clone());
        }
    }

    // --- the box ----------------------------------------------------------

    /// The keyboard's and the controller's way in. Unfolds the section
    /// if it was folded: a box you cannot see is not focused.
    pub fn focus(&self) {
        self.body.set_visible(true);
        focus_when_ready(&self.composer.entry);
    }

    /// The double tap's clear: the text and the chips.
    pub fn clear(&self) {
        self.composer.clear();
        self.sync();
    }

    /// Put text in the box — a suggested commit message, a probe's fixture.
    pub fn set_text(&self, text: &str) {
        self.composer.set_text(text);
    }

    /// TASTE_PROBE_CHECK only: the words the microphone is still hearing
    /// ([`crate::composer::Composer::seed_dictating_for_probe`]).
    pub fn seed_dictating_for_probe(&self, text: &str) {
        self.composer.seed_dictating_for_probe(text);
    }

    pub fn add_attachment(
        &self,
        label: String,
        block: agent_client_protocol::schema::v1::ContentBlock,
    ) {
        self.composer.add_attachment(label, block);
    }

    /// The hotkey's gesture: start dictating, or stop and transcribe.
    pub fn toggle_dictation(&self) {
        self.focus();
        self.composer.toggle_dictation();
    }

    fn draft(&self) -> Draft {
        Draft {
            has_text: !self.composer.text().trim().is_empty(),
            attachments: self.composer.attachment_count(),
        }
    }

    fn surroundings(&self) -> Surroundings {
        Surroundings {
            staged: self.staged.get(),
            commit_blocked: self.commit_blocked.get(),
            chat: self
                .chat_available
                .borrow()
                .as_ref()
                .map_or(ChatReadiness::Ready, |probe| probe()),
        }
    }

    /// Each destination's button says whether the draft can go there and,
    /// when it cannot, why — disabled, with the reason as its tooltip.
    fn sync(&self) {
        let draft = self.draft();
        let surroundings = self.surroundings();
        let staged = self.staged.get();
        for (destination, button) in &self.buttons {
            let verdict = availability(*destination, draft, surroundings);
            // The words and, for Commit, what it takes; no keys (David,
            // 2026-09-08: "don't put keyboard/controller stuff in tool
            // tips, either. just let F1 do the job").
            let words = verb(*destination);
            let mut tip = match destination {
                Destination::Commit => format!("{words} {}", staged_words(staged)),
                _ => words.to_string(),
            };
            if let Err(why) = verdict {
                tip.push_str(&format!("\nNot now: {why}"));
            }
            button.set_tooltip_text(Some(&tip));
            if *destination == Destination::Chat {
                self.composer.set_primary_ready(verdict.is_ok());
            } else {
                button.set_sensitive(verdict.is_ok());
            }
        }
    }

    /// Send the draft to `destination`. Says whether it went; when it did
    /// not, the reason is a toast and the draft stays.
    pub fn dispatch(&self, destination: Destination) -> bool {
        let text = self.composer.text();
        if let Err(why) = availability(destination, self.draft(), self.surroundings()) {
            self.events.publish(Event::Toast(format!(
                "Not sent to the {}: {why}",
                destination.label().to_lowercase()
            )));
            return false;
        }
        let attachments = self.composer.take_attachments();
        // A drafted brief going to its chat hands the issue over, and the
        // chat hears so first, so the brief goes without the preamble meant
        // for an environment the user works in.
        let handing = match destination {
            Destination::Chat => self.handoffs.borrow().in_box().cloned(),
            _ => None,
        };
        if let (Some(env), Some(hook)) = (&handing, self.on_handoff.borrow().as_ref()) {
            hook(env, true);
        }
        let result = match destination {
            Destination::Chat => match self.on_chat.borrow().as_ref() {
                Some(hook) => hook(text.clone(), attachments.clone()),
                None => Err("no chat to send to".into()),
            },
            Destination::Backlog => {
                let (title, body) = crate::backlog::split_issue_text(&text)
                    .unwrap_or_else(|| ("Attachment".to_string(), String::new()));
                match self.on_backlog.borrow().as_ref() {
                    Some(hook) => hook(title, body, attachments.clone()),
                    None => Err("no backlog to file on".into()),
                }
            }
            Destination::Commit => match self.on_commit.borrow().as_ref() {
                Some(hook) => hook(text.trim().to_string()),
                None => Err("nothing to commit with".into()),
            },
        };
        match result {
            Ok(()) => {
                if !text.trim().is_empty() {
                    *self.last_sent.borrow_mut() = Some(text);
                }
                self.handoffs.borrow_mut().sent(destination);
                self.composer.clear();
                self.composer.entry.grab_focus();
                self.sync();
                true
            }
            Err(why) => {
                if let (Some(env), Some(hook)) = (&handing, self.on_handoff.borrow().as_ref()) {
                    hook(env, false);
                }
                // Nothing lost: the text is still in the box, and the
                // attachments go back on it.
                for attachment in attachments {
                    self.composer
                        .add_attachment(attachment.label, attachment.block);
                }
                self.events.publish(Event::Toast(format!(
                    "Not sent to the {}: {why}",
                    destination.label().to_lowercase()
                )));
                false
            }
        }
    }

    // --- the controller -----------------------------------------------------

    /// Start or stop listening — a held gesture's two ends (Ctrl+D, X).
    pub fn dictate(&self, on: bool) {
        if on {
            self.focus();
        }
        self.composer.dictate(on);
    }

    /// Whether the caret is in the box: while it is, the controller's A is
    /// Send; while it is not and a query stands, A opens a search result.
    pub fn has_focus(&self) -> bool {
        self.composer.entry.has_focus()
    }

    /// A button on the controller: X taps focus the box and X held dictates
    /// into it until released; A sends to the chat; B and Y held file and
    /// commit, and tapped only light their buttons.
    pub fn controller(self: &Rc<Self>, button: ControllerButton, pressed: bool) {
        match (button, pressed) {
            (ControllerButton::X, true) => {
                let weak = Rc::downgrade(self);
                self.focus();
                let double = self.x_hold.press(move || {
                    if let Some(compose) = weak.upgrade() {
                        compose.dictate(true);
                    }
                });
                if double {
                    self.clear();
                }
            }
            (ControllerButton::X, false) => match self.x_hold.release() {
                Release::Held => self.dictate(false),
                Release::Tap | Release::Idle => {}
            },
            (ControllerButton::A, true) => {
                self.dispatch(Destination::Chat);
            }
            (ControllerButton::B, true) => self.hold_to_send(&self.b_hold, Destination::Backlog),
            (ControllerButton::B, false) => {
                self.release_to_send(&self.b_hold, Destination::Backlog)
            }
            (ControllerButton::Y, true) => self.hold_to_send(&self.y_hold, Destination::Commit),
            (ControllerButton::Y, false) => self.release_to_send(&self.y_hold, Destination::Commit),
            _ => {}
        }
    }

    fn hold_to_send(self: &Rc<Self>, hold: &Rc<Hold>, destination: Destination) {
        let weak = Rc::downgrade(self);
        hold.press(move || {
            if let Some(compose) = weak.upgrade() {
                compose.dispatch(destination);
            }
        });
    }

    fn release_to_send(&self, hold: &Hold, destination: Destination) {
        if hold.release() == Release::Tap {
            self.pulse(destination);
        }
    }

    /// A tap where a hold was meant: the button lights for a moment, so
    /// the gesture teaches itself without sending anything.
    pub fn pulse(&self, destination: Destination) {
        let Some((_, button)) = self.buttons.iter().find(|(d, _)| *d == destination) else {
            return;
        };
        button.add_css_class("hold-hint");
        let button = button.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(700), move || {
            button.remove_css_class("hold-hint");
        });
    }

    /// What the F1 reveal labels (reveal.rs): the field, the microphone,
    /// and the row the three buttons sit in.
    pub fn reveal_targets(&self) -> (gtk::Widget, gtk::Widget, gtk::Widget) {
        let primary: gtk::Widget = self.composer.primary.clone().upcast();
        (
            self.composer.entry.clone().upcast(),
            self.composer.mic.clone().upcast(),
            primary.parent().unwrap_or(primary),
        )
    }

    /// TASTE_PROBE_CHECK only: a draft in the box.
    pub fn seed_for_probe(&self, text: &str) {
        self.composer.set_text(text);
    }

    /// `TASTE_PROBE_TYPE` only: type `text` into the box one character at a
    /// time, waiting between them, then run `done`.
    ///
    /// Typing is not the same act as setting the text, and this bug lives in
    /// the difference: the slash-command completion, the markdown restyle
    /// debounce, the buttons' per-keystroke tooltips and the completion
    /// popup's own frame clock all do their work in the gaps between
    /// keystrokes. A burst inserted in one go turns the main loop once and
    /// exercises none of them.
    pub fn type_for_probe(
        &self,
        text: &str,
        per_char: std::time::Duration,
        done: impl Fn() + 'static,
    ) {
        let entry = self.composer.entry.clone();
        let text = text.to_string();
        self.focus();
        // `TASTE_PROBE_PASTE`: the text pasted through the clipboard in one
        // go, as Ctrl+V does — one insertion of the whole, which typing a
        // character at a time is not.
        if std::env::var_os("TASTE_PROBE_PASTE").is_some() {
            // Into an empty field, as a message is most often pasted.
            // Steps separated by U+0001 paste one after another, each over
            // the whole of what is there, as select-all and paste does — a
            // long paste replaced by a short one is a state one paste does
            // not reach.
            entry.buffer().set_text("");
            let steps: Vec<String> = text.split('\u{1}').map(str::to_string).collect();
            glib::spawn_future_local(async move {
                for step in steps {
                    let buffer = entry.buffer();
                    let (start, end) = buffer.bounds();
                    buffer.select_range(&start, &end);
                    entry.clipboard().set_text(&step);
                    glib::timeout_future(std::time::Duration::from_millis(300)).await;
                    entry.emit_paste_clipboard();
                    glib::timeout_future(std::time::Duration::from_millis(400)).await;
                }
                done();
            });
            return;
        }
        glib::spawn_future_local(async move {
            for ch in text.chars() {
                let buffer = entry.buffer();
                let mut end = buffer.end_iter();
                // A backspace in the text deletes the last character, so a
                // probe can type past a wrap and back again — the shrink is
                // a state typing alone never reaches.
                if ch == '\u{8}' {
                    let mut start = end;
                    if start.backward_char() {
                        buffer.delete(&mut start, &mut end);
                    }
                } else {
                    buffer.insert(&mut end, &ch.to_string());
                }
                glib::timeout_future(per_char).await;
            }
            done();
        });
    }
}

/// Put the keyboard in a widget, even when it was hidden a moment ago or
/// something else is about to take the focus back.
///
/// Two things break a bare `grab_focus`, and the backlog's ghost row hit
/// both (David, 2026-09-08: "clicking the ghost backlog item doesn't
/// reliably set focus in dispatch"). A widget only holds focus once it is
/// MAPPED, and a box unfolded in this same tick is mapped in the next
/// layout pass — so the grab that follows `set_visible(true)` silently
/// does nothing. And `GtkListBox` focuses the row it just activated
/// *after* the activation handler returns, which takes the focus straight
/// back out of wherever the handler put it.
///
/// So: grab now, grab again on the next idle — after the list has
/// finished with the click — and once more the first time the widget is
/// mapped, whichever of those comes first.
fn focus_when_ready(widget: &impl IsA<gtk::Widget>) {
    let widget = widget.as_ref().clone();
    widget.grab_focus();
    {
        let widget = widget.clone();
        glib::idle_add_local_once(move || {
            if !widget.has_focus() {
                widget.grab_focus();
            }
        });
    }
    if !widget.is_mapped() {
        // One shot: the box is unfolded once and stays that way, and a
        // handler left connected would steal the focus on every later map.
        let slot: Rc<RefCell<Option<glib::SignalHandlerId>>> = Rc::new(RefCell::new(None));
        let taken = slot.clone();
        let id = widget.connect_map(move |widget| {
            widget.grab_focus();
            if let Some(id) = taken.borrow_mut().take() {
                widget.disconnect(id);
            }
        });
        *slot.borrow_mut() = Some(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALM: Surroundings = Surroundings {
        staged: 3,
        commit_blocked: false,
        chat: ChatReadiness::Ready,
    };

    fn env(slug: &str) -> EnvironmentId {
        EnvironmentId::parse(slug).unwrap()
    }

    #[test]
    fn a_drafted_brief_fills_the_empty_box_of_its_environment() {
        let mut drafts = Handoffs::default();
        // Start aims the panes there first, then drafts.
        assert_eq!(drafts.aim(&env("i-0007"), ""), BoxEdit::Leave);
        assert_eq!(
            drafts.offer(&env("i-0007"), "You are working issue i-0007", ""),
            BoxEdit::Fill("You are working issue i-0007".into())
        );
        assert_eq!(drafts.in_box(), Some(&env("i-0007")));
        // Sent to the chat: the issue is handed over, once.
        assert_eq!(drafts.sent(Destination::Chat), Some(env("i-0007")));
        assert_eq!(drafts.in_box(), None);
        assert_eq!(drafts.sent(Destination::Chat), None);
    }

    #[test]
    fn a_drafted_brief_never_overwrites_the_users_words() {
        let mut drafts = Handoffs::default();
        drafts.aim(&env("i-0007"), "my own words");
        assert_eq!(
            drafts.offer(&env("i-0007"), "brief", "my own words"),
            BoxEdit::Leave
        );
        assert_eq!(drafts.in_box(), None);
        // It waits for the next arrival with the box empty.
        drafts.aim(&env("primary"), "my own words");
        assert_eq!(
            drafts.aim(&env("i-0007"), ""),
            BoxEdit::Fill("brief".into())
        );
    }

    #[test]
    fn a_drafted_brief_follows_its_environment_and_ends_when_emptied() {
        let mut drafts = Handoffs::default();
        drafts.aim(&env("i-0007"), "");
        drafts.offer(&env("i-0007"), "brief", "");
        // Away, edited: parked as the user left it, and the box emptied.
        assert_eq!(drafts.aim(&env("primary"), "brief, edited"), BoxEdit::Clear);
        assert_eq!(drafts.in_box(), None);
        // Back: there again.
        assert_eq!(
            drafts.aim(&env("i-0007"), ""),
            BoxEdit::Fill("brief, edited".into())
        );
        // Deleted: gone, and the environment is the user's.
        drafts.edited("   ");
        assert_eq!(drafts.in_box(), None);
        drafts.aim(&env("primary"), "");
        assert_eq!(drafts.aim(&env("i-0007"), ""), BoxEdit::Leave);
    }

    #[test]
    fn a_message_of_the_users_own_ends_the_parked_brief() {
        let mut drafts = Handoffs::default();
        drafts.aim(&env("i-0007"), "my own words");
        drafts.offer(&env("i-0007"), "brief", "my own words");
        // The user's words go to that environment's chat: they work there.
        assert_eq!(drafts.sent(Destination::Chat), None);
        drafts.aim(&env("primary"), "");
        assert_eq!(drafts.aim(&env("i-0007"), ""), BoxEdit::Leave);
        // A brief sent elsewhere is used up, and hands nothing over.
        drafts.offer(&env("i-0007"), "brief", "");
        assert_eq!(drafts.in_box(), Some(&env("i-0007")));
        assert_eq!(drafts.sent(Destination::Backlog), None);
        assert_eq!(drafts.in_box(), None);
    }

    #[test]
    fn a_destroyed_environments_brief_goes_with_it() {
        let mut drafts = Handoffs::default();
        drafts.aim(&env("i-0007"), "");
        drafts.offer(&env("i-0007"), "brief", "");
        assert_eq!(drafts.forget(&env("i-0007")), BoxEdit::Clear);
        assert_eq!(drafts.in_box(), None);
        assert_eq!(handoff_caption(&env("i-0007")), "i-0007's brief, unsent");
    }

    #[test]
    fn a_destination_refuses_a_draft_it_cannot_take_and_says_why() {
        let words = Draft {
            has_text: true,
            attachments: 0,
        };
        let picture = Draft {
            has_text: true,
            attachments: 1,
        };
        let nothing = Draft {
            has_text: false,
            attachments: 0,
        };
        for destination in Destination::ORDER {
            assert_eq!(
                availability(destination, words, CALM),
                Ok(()),
                "{destination:?}"
            );
        }
        assert!(availability(Destination::Commit, picture, CALM).is_err());
        assert_eq!(availability(Destination::Backlog, picture, CALM), Ok(()));
        assert_eq!(availability(Destination::Chat, picture, CALM), Ok(()));
        assert!(availability(Destination::Chat, nothing, CALM).is_err());
        assert!(availability(Destination::Backlog, nothing, CALM).is_err());
        assert_eq!(
            availability(
                Destination::Commit,
                words,
                Surroundings { staged: 0, ..CALM }
            ),
            Err("nothing is staged")
        );
        assert!(availability(
            Destination::Commit,
            words,
            Surroundings {
                commit_blocked: true,
                ..CALM
            }
        )
        .is_err());
        for chat in [ChatReadiness::Gone, ChatReadiness::NoCredential] {
            assert!(
                availability(Destination::Chat, words, Surroundings { chat, ..CALM }).is_err(),
                "{chat:?}"
            );
            // The other two destinations do not need a chat.
            assert_eq!(
                availability(Destination::Backlog, words, Surroundings { chat, ..CALM }),
                Ok(())
            );
        }
    }

    #[test]
    fn the_pill_names_the_destination_and_counts_the_staged_files() {
        assert_eq!(verb(Destination::Chat), "Send to Chat");
        assert_eq!(verb(Destination::Backlog), "Backlog");
        assert_eq!(verb(Destination::Commit), "Commit");
        assert_eq!(staged_words(1), "1 staged file");
        assert_eq!(staged_words(3), "3 staged files");
        assert_eq!(staged_words(0), "nothing staged");
    }

    #[test]
    fn a_release_with_nothing_down_is_idle() {
        let hold = Hold::new();
        assert_eq!(hold.release(), Release::Idle);
        assert!(!hold.is_down());
    }

    #[test]
    fn keys_and_buttons_read_in_the_same_order() {
        assert_eq!(
            Destination::ORDER.map(Destination::key),
            ["Enter", "F5", "F6"]
        );
        assert_eq!(Destination::ORDER.map(Destination::button), ["A", "B", "Y"]);
        assert_eq!(
            Destination::ORDER.map(Destination::held),
            [false, true, true]
        );
    }

    /// The rule that is invisible until it destroys somebody's sentence:
    /// while an input method is composing, Enter COMMITS the composition.
    /// Sending there truncates the message mid-word, unrecoverably, and it
    /// happens on ordinary typing for every CJK user.
    #[test]
    fn enter_belongs_to_the_input_method_mid_preedit() {
        assert!(enter_sends(false, false));
        // Shift is a new line either way.
        assert!(!enter_sends(true, false));
        // Preedit claims Enter even with Shift up.
        assert!(!enter_sends(false, true));
    }
}

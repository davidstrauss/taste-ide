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
//! Nothing here knows how a chat sends, how an issue is filed or how a
//! commit is written: the window hands in three hooks.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::composer::{Attachment, Composer};
use taste_core::{ControllerButton, Event, Workspace};

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
            Destination::Backlog => "view-list-ordered-symbolic",
            Destination::Commit => "object-select-symbolic",
        }
    }

    /// The key that sends to it — held, for the two that are not the chat
    /// ([`Destination::held`]) — and the controller button that does.
    pub fn key(self) -> &'static str {
        match self {
            Destination::Chat => "Enter",
            Destination::Backlog => "F5",
            Destination::Commit => "F6",
        }
    }

    /// Whether the key and the button must be held to send here.
    pub fn held(self) -> bool {
        self != Destination::Chat
    }

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

/// What the window is doing, in the terms a destination judges by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Surroundings {
    pub staged: usize,
    /// The Staged view has files unchecked: a commit takes the whole
    /// index, so there is no committing a selection.
    pub commit_blocked: bool,
    pub has_chat: bool,
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
            if !surroundings.has_chat {
                return Err("no chat is selected — pick an environment in the backlog");
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

const PLACEHOLDER: &str = "A message for the agent · an issue, title first · a commit message";

/// A destination's button face: the words, and for Send to Chat — the
/// one the user asked for as "the glyph+text equivalent" — the glyph too.
/// The two hold buttons are words alone, which is what lets all three read
/// in full at the chat column's floor (chat_column.rs); should the column
/// be narrower still, the words yield before the row does.
fn button_content(destination: Destination) -> (gtk::Box, gtk::Label) {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    content.set_halign(gtk::Align::Center);
    if destination == Destination::Chat {
        content.append(&gtk::Image::from_icon_name(destination.icon()));
    }
    let label = gtk::Label::builder()
        .label(verb(destination))
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    content.append(&label);
    (content, label)
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

pub struct Compose {
    /// The whole section: header, then the folding body.
    pub widget: gtk::Box,
    body: gtk::Box,
    composer: Rc<Composer>,
    /// One button per destination, each sending straight there; the
    /// label is the verb, which counts the staged files for Commit.
    buttons: Vec<(Destination, gtk::Button, gtk::Label)>,
    staged: Cell<usize>,
    commit_blocked: Cell<bool>,
    chat_available: RefCell<Option<Box<dyn Fn() -> bool>>>,
    on_chat: RefCell<Option<ChatHook>>,
    on_backlog: RefCell<Option<BacklogHook>>,
    on_commit: RefCell<Option<CommitHook>>,
    /// The chat's Escape — deny the permission card, else stop the turn —
    /// answering whether it had something to do.
    on_escape: RefCell<Option<Box<dyn Fn() -> bool>>>,
    /// The last text sent, for Up in an empty box.
    last_sent: RefCell<Option<String>>,
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
        if let Some(title) = header.last_child() {
            title.set_hexpand(true);
            // The title yields first at the chat column's floor.
            if let Ok(label) = title.downcast::<gtk::Label>() {
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            }
        }
        // The three buttons, in the composer's action row: To backlog and
        // Commit between the microphone and the pill, Send to Chat the
        // pill itself, rightmost.
        let mut buttons: Vec<(Destination, gtk::Button, gtk::Label)> = Vec::new();
        let mut extras = Vec::new();
        for destination in [Destination::Backlog, Destination::Commit] {
            let (content, label) = button_content(destination);
            let button = gtk::Button::builder().child(&content).build();
            extras.push(button.clone());
            buttons.push((destination, button, label));
        }
        let composer = Composer::new(workspace, verb(Destination::Chat), &extras);
        {
            let (content, label) = button_content(Destination::Chat);
            composer.primary.set_child(Some(&content));
            buttons.push((Destination::Chat, composer.primary.clone(), label));
        }
        composer.set_placeholder(PLACEHOLDER);
        composer.widget.set_margin_start(12);
        composer.widget.set_margin_end(12);
        composer.widget.set_margin_top(6);
        composer.widget.set_margin_bottom(4);
        // The keys and the buttons, written where they apply (David: "Pick
        // one out and document it (and the controller inputs below) in the
        // universal composer panel").
        let hint = gtk::Label::builder()
            .label(
                "Ctrl+D focuses · hold to talk · twice clears · Enter sends to chat\n\
                 Hold F5 to file the issue · hold F6 to commit · hold F1 for every key\n\
                 Controller: X focus, talk, clear · A chat · hold B backlog · hold Y commit",
            )
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .css_classes(["caption", "dim-label"])
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(8)
            .build();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&composer.widget);
        body.append(&hint);
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
            on_escape: RefCell::new(None),
            last_sent: RefCell::new(None),
            provider: RefCell::new(None),
            events: workspace.events.clone(),
            x_hold: Rc::new(Hold::new()),
            b_hold: Rc::new(Hold::new()),
            y_hold: Rc::new(Hold::new()),
        });

        for (destination, button, _) in &compose.buttons {
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
                    compose.sync();
                }
            });
        }
        {
            let events = workspace.events.clone();
            composer.set_on_notice(move |text| events.publish(Event::Toast(text)));
        }
        // Enter sends to the chat; Shift+Enter is a new line;
        // Escape is the chat's (deny the card, stop the turn) when the chat
        // has something for it; Up in an empty box brings the last text
        // back.
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
                    Key::Return | Key::KP_Enter if !shift => {
                        compose.dispatch(Destination::Chat);
                        glib::Propagation::Stop
                    }
                    Key::Escape => {
                        let handled = compose
                            .on_escape
                            .borrow()
                            .as_ref()
                            .is_some_and(|escape| escape());
                        if handled {
                            glib::Propagation::Stop
                        } else {
                            glib::Propagation::Proceed
                        }
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

    pub fn set_on_escape(&self, hook: impl Fn() -> bool + 'static) {
        *self.on_escape.borrow_mut() = Some(Box::new(hook));
    }

    /// Whether there is a chat to send to right now.
    pub fn set_chat_available(&self, probe: impl Fn() -> bool + 'static) {
        *self.chat_available.borrow_mut() = Some(Box::new(probe));
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

    /// The keyboard's way in (F4, or X on a controller). Unfolds the section
    /// if it was folded: a box you cannot see is not focused.
    pub fn focus(&self) {
        self.body.set_visible(true);
        self.composer.entry.grab_focus();
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
            has_chat: self
                .chat_available
                .borrow()
                .as_ref()
                .is_none_or(|probe| probe()),
        }
    }

    /// Each destination's button says whether the draft can go there and,
    /// when it cannot, why — disabled, with the reason as its tooltip.
    fn sync(&self) {
        let draft = self.draft();
        let surroundings = self.surroundings();
        let staged = self.staged.get();
        for (destination, button, label) in &self.buttons {
            let verdict = availability(*destination, draft, surroundings);
            let words = verb(*destination);
            label.set_label(words);
            let mut tip = format!(
                "{words}{} — {}{}, or {} on a controller",
                match destination {
                    Destination::Commit => format!(" {}", staged_words(staged)),
                    _ => String::new(),
                },
                if destination.held() { "hold " } else { "" },
                destination.key(),
                destination.button()
            );
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
                self.composer.clear();
                self.composer.entry.grab_focus();
                self.sync();
                true
            }
            Err(why) => {
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
        let Some((_, button, _)) = self.buttons.iter().find(|(d, _, _)| *d == destination) else {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALM: Surroundings = Surroundings {
        staged: 3,
        commit_blocked: false,
        has_chat: true,
    };

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
        assert!(availability(
            Destination::Chat,
            words,
            Surroundings {
                has_chat: false,
                ..CALM
            }
        )
        .is_err());
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
}

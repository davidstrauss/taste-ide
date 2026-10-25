//! **The transcript's rows, drawn.**
//!
//! A row of the transcript's model (`super::model`) as widgets: built when
//! the list brings the row on screen, and dropped when it leaves. Every
//! builder here is a function of the row's data and nothing else, so a
//! row drawn again is the row it was; and each is cheap, because a step's
//! content — a diff, a command's output — is built only when the step is
//! opened onto it.

use std::rc::Rc;

use adw::prelude::*;
use agent_client_protocol::schema::v1::{ContentBlock, PlanEntryStatus, ToolCallContent};

use super::model::{
    Answer, Delivery, Dot, Kind, PlanCard, Rail, Row, RowId, Step, Task, Thought, User,
};
use super::*;

/// A row's widgets, and what a change to its row can patch in place
/// rather than build again.
pub(super) struct RowView {
    pub widget: gtk::Widget,
    /// The row as it was built, which a change is told apart from.
    shown: Row,
    /// Its line's segments above and below its dot, for a row on the rail.
    pub(super) lines: Option<super::rail::Rail>,
    /// An answer's document, which every change to the answer — its text
    /// growing, its end, its being shown whole — is drawn into in place.
    answer: Option<Rc<AnswerView>>,
}

/// An answer's drawing: its Markdown, drawn as it streams without what is
/// drawn moving (`markdown_view::Incremental`), and under it the line that
/// opens the whole of a long one.
pub(super) struct AnswerView {
    body: gtk::Box,
    doc: crate::markdown_view::Incremental,
    more: gtk::Box,
    /// What the more line says now: the lines it hides, and whether the
    /// answer has ended, when the document it opens is the whole of it.
    more_for: Cell<Option<(usize, bool)>>,
    /// Text waiting to be drawn, and whether a frame's drawing is asked
    /// for: a stream is hundreds of updates, each a few words, and they are
    /// drawn once a frame.
    pending: RefCell<Option<String>>,
    queued: Cell<bool>,
    /// Drawn at least once: a row built, or bound as the list scrolls to
    /// it, shows its text at once; only the updates after wait a frame.
    drawn: Cell<bool>,
}

/// A row's piece of the rail, as [`ChatPane::on_rail`] built it.
struct Railed {
    row: gtk::Box,
    rail: super::rail::Rail,
}

impl ChatPane {
    /// `row`'s widgets.
    pub(super) fn build_row(&self, row: &Row) -> RowView {
        let mut answer_view = None;
        let (inner, railed): (gtk::Widget, Option<Railed>) = match &row.kind {
            Kind::User(user) if user.within_turn => {
                (self.on_rail_line(&self.user_widget(row.id, user)), None)
            }
            Kind::User(user) => (self.user_widget(row.id, user).upcast(), None),
            Kind::Answer(answer) => {
                let (railed, view) = self.answer_widget(row, answer);
                answer_view = Some(view);
                (railed.row.clone().upcast(), Some(railed))
            }
            Kind::Thought(thought) => self.railed(self.thought_widget(row, thought)),
            Kind::Step(step) => self.railed(self.step_widget(row, step)),
            Kind::Task(task) => self.railed(self.task_widget(row, task)),
            Kind::Plan(plan) => self.railed(self.plan_widget(row, plan)),
            Kind::Note(text) => self.railed(self.note_widget(row, text)),
            Kind::Marker { prompt, through } => (self.marker_widget(prompt, *through), None),
        };
        // The row's own box, which wears the blue while its document is
        // the tab in front, and a search hit's light.
        let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        outer.add_css_class("transcript-row");
        if row.lit {
            outer.add_css_class("doc-open");
        }
        outer.append(&inner);
        RowView {
            widget: outer.upcast(),
            shown: row.clone(),
            lines: railed.map(|railed| railed.rail),
            answer: answer_view,
        }
    }

    fn railed(&self, railed: Railed) -> (gtk::Widget, Option<Railed>) {
        (railed.row.clone().upcast(), Some(railed))
    }

    /// Bring `view` up to `row` in place, when what changed is its line,
    /// its light, or a streaming answer's text — the changes a row sees
    /// most, and none of which moves anything but itself: whether it could.
    pub(super) fn patch_row(&self, view: &mut RowView, row: &Row) -> bool {
        let same = view.shown.dot == row.dot
            && match (&view.shown.kind, &row.kind, &view.answer) {
                // Whatever changed of an answer — its text, its end, its
                // being shown whole — is drawn into the same widgets: an
                // answer that ended used to be built again, which moved it.
                (Kind::Answer(was), Kind::Answer(is), Some(answer)) => {
                    if was != is {
                        self.fill_answer(answer, row.id, is);
                    }
                    true
                }
                (was, is, _) => was == is,
            };
        if !same {
            return false;
        }
        if view.shown.rail != row.rail {
            match (&view.lines, row.rail) {
                (Some(lines), Some(rail))
                    if view.shown.rail.map(|r| r.after_prompt) == Some(rail.after_prompt) =>
                {
                    lines.set_lines(rail.top, rail.bottom);
                }
                _ => return false,
            }
        }
        if view.shown.lit != row.lit {
            if row.lit {
                view.widget.add_css_class("doc-open");
            } else {
                view.widget.remove_css_class("doc-open");
            }
        }
        view.shown = row.clone();
        true
    }

    /// `content` beside its piece of the rail — the connected bullets the
    /// agent's turn is read by (David, 2026-09-07). The line runs up from
    /// the dot when `rail.top`, down from it when `rail.bottom`. Hands back
    /// the row, and its rail for a step to paint (`rail.rs`).
    fn on_rail(&self, content: &impl IsA<gtk::Widget>, rail: Rail, dot_kind: Dot) -> Railed {
        // Under a prompt the step carries the gap itself, so the line can
        // cross it: the content moves down by the gap, and the line runs
        // up across it to the box.
        let gap = if rail.after_prompt {
            PROMPT_GAP
        } else {
            STEP_GAP
        };
        let drawn = super::rail::Rail::new(content, gap);
        drawn.set_lines(rail.top, rail.bottom);
        drawn.set_mark(match dot_kind {
            Dot::Plain => super::rail::Mark::Plain,
            Dot::Note => super::rail::Mark::Hollow,
            Dot::Tone(tone) => super::rail::Mark::Tone(tone),
        });
        let row_box = gtk::Box::new(gtk::Orientation::Horizontal, RAIL_GAP);
        row_box.set_margin_start(ROW_OWN_SIDE);
        row_box.set_margin_end(ROW_OWN_SIDE);
        row_box.append(&drawn);
        content.set_hexpand(true);
        content.set_margin_top(gap);
        content.add_css_class("step-content");
        row_box.append(content);
        Railed {
            row: row_box,
            rail: drawn,
        }
    }

    /// A message the user sent, as its card: full width above the turn it
    /// began (David, 2026-09-07: "My prompts show up as full-width in
    /// boxes").
    fn user_widget(&self, id: RowId, user: &User) -> gtk::Box {
        let card = self.prompt_card(&user.text, &user.attachments, id);
        // Where it has got to, under the words: why it has not gone yet,
        // how long it waited, or that it never went.
        let (label, tooltip): (String, Option<String>) = match &user.delivery {
            Delivery::Delivered | Delivery::Sent => return card,
            Delivery::Queued => ("queued — sends when the current turn ends".into(), None),
            Delivery::Asking { told: false } => (
                "queued — asking the agent to end its turn after its current step".into(),
                None,
            ),
            Delivery::Asking { told: true } => (
                "queued — the agent has been asked to end its turn".into(),
                None,
            ),
            Delivery::Held(label) => (label.clone(), None),
            Delivery::Waited(note) => (taste_core::queued::badge(note), Some(note.clone())),
            Delivery::Refused(why) => (
                format!("refused — {}", single_line(why, 200)),
                Some(why.clone()),
            ),
            Delivery::NotSent(why) => (
                format!("not sent — {}", single_line(why, 200)),
                Some(why.clone()),
            ),
            Delivery::Lost => (
                "not delivered — the agent stopped before it took this".into(),
                None,
            ),
            Delivery::Undelivered => (
                "not delivered — the IDE closed before the agent took this".into(),
                None,
            ),
        };
        let badge = gtk::Label::builder()
            .label(&label)
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .css_classes(["dim-label", "caption"])
            .margin_top(CARD_INSET)
            .margin_bottom(CARD_INSET)
            .margin_start(CARD_INSET)
            .margin_end(CARD_INSET)
            .build();
        badge.set_tooltip_text(tooltip.as_deref());
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        line.append(&badge);
        if matches!(user.delivery, Delivery::Refused(_)) {
            card.add_css_class("refused");
            badge.remove_css_class("dim-label");
            badge.add_css_class("error");
        }
        // A queued message's two controls at the badge's end. Fast-forward:
        // a click asks the agent, at its next use of the IDE's tools, to
        // finish its step and end the turn (`taste_core::nudge`), which
        // sends the message without stopping anything; held two seconds,
        // it stops the turn now, drops the other queued messages, and
        // sends this one next. Stop: the message is dropped before the
        // agent takes it (David, 2026-10-24).
        if let (Delivery::Queued | Delivery::Asking { .. }, Some(prompt)) =
            (&user.delivery, &user.prompt)
        {
            let forward = crate::holdbutton::HoldButton::new(
                "media-seek-forward-symbolic",
                "Ask the agent to finish the step it is on and take this next. Hold for two \
                 seconds to stop it now and send only this, dropping the other queued messages",
            );
            forward.widget.add_css_class("flat");
            forward.widget.set_valign(gtk::Align::Center);
            {
                let weak = self.weak_self.borrow().clone();
                let prompt = prompt.clone();
                forward.set_on_click(move || {
                    if let Some(pane) = weak.upgrade() {
                        pane.ask_for_turn(&prompt);
                    }
                });
            }
            {
                let weak = self.weak_self.borrow().clone();
                let prompt = prompt.clone();
                forward.set_on_confirm(move || {
                    if let Some(pane) = weak.upgrade() {
                        pane.expedite_prompt(&prompt);
                    }
                });
            }
            let stop = gtk::Button::builder()
                .icon_name("media-playback-stop-symbolic")
                .css_classes(["flat"])
                .valign(gtk::Align::Center)
                .margin_end(CARD_INSET)
                .tooltip_text("Cancel this message: it is not sent")
                .build();
            {
                let weak = self.weak_self.borrow().clone();
                let prompt = prompt.clone();
                stop.connect_clicked(move |button| {
                    button.set_sensitive(false);
                    if let Some(pane) = weak.upgrade() {
                        pane.withdraw_prompt(&prompt);
                    }
                });
            }
            line.append(&forward.widget);
            line.append(&stop);
            // Held by the pane: a hold button keeps itself only weakly.
            self.keep_card_hold(prompt, forward);
        }
        card.append(&line);
        card
    }

    /// `content` in the timeline's column with the line running past it
    /// beside the content: a prompt waiting inside a turn, which goes on
    /// below it (`User::within_turn`).
    fn on_rail_line(&self, content: &impl IsA<gtk::Widget>) -> gtk::Widget {
        let rail = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .width_request(RAIL_WIDTH)
            .hexpand(false)
            .build();
        rail.append(
            &gtk::Box::builder()
                .css_classes(["rail-line"])
                // On the rail's own pixel column, `(RAIL_WIDTH - 1) / 2`,
                // where the step rail draws its line (rail.rs): centred,
                // a 1px line in the 16px column fell between two pixels
                // and smeared across both, half a pixel off the steps'.
                .halign(gtk::Align::Start)
                .margin_start((RAIL_WIDTH - 1) / 2)
                .vexpand(true)
                .build(),
        );
        let row = gtk::Box::new(gtk::Orientation::Horizontal, RAIL_GAP);
        row.set_margin_start(ROW_OWN_SIDE);
        row.append(&rail);
        // The row carries the inset, as a step's does, so the card's edge
        // is the steps' text's.
        content.set_margin_start(0);
        content.set_hexpand(true);
        row.append(content);
        row.upcast()
    }

    /// Where a prompt was typed, which the agent took further down: a
    /// marker that jumps to it. Between two steps the line runs on through
    /// it, beside the rail as a step's content stands.
    fn marker_widget(&self, prompt: &str, through: bool) -> gtk::Widget {
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
            .hexpand(true)
            .margin_top(4)
            .margin_bottom(4)
            .build();
        {
            let weak = self.weak_self.borrow().clone();
            let prompt = prompt.to_string();
            marker.connect_clicked(move |_| {
                if let Some(pane) = weak.upgrade() {
                    pane.scroll_to_prompt(&prompt);
                }
            });
        }
        // Vertical, so the line is given the column's width to be centred
        // in, as a step's is; never expanding, or the column takes the
        // row's spare width and the line leaves the rail.
        let rail = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .width_request(RAIL_WIDTH)
            .hexpand(false)
            .build();
        rail.append(
            &gtk::Box::builder()
                .css_classes(["rail-line"])
                // On the rail's own pixel column, `(RAIL_WIDTH - 1) / 2`,
                // where the step rail draws its line (rail.rs): centred,
                // a 1px line in the 16px column fell between two pixels
                // and smeared across both, half a pixel off the steps'.
                .halign(gtk::Align::Start)
                .margin_start((RAIL_WIDTH - 1) / 2)
                .vexpand(true)
                .visible(through)
                .build(),
        );
        let row = gtk::Box::new(gtk::Orientation::Horizontal, RAIL_GAP);
        row.set_margin_start(ROW_OWN_SIDE);
        row.set_margin_end(ROW_OWN_SIDE);
        row.append(&rail);
        row.append(&marker);
        row.upcast()
    }

    /// The card a prompt is drawn as: the words, clipped long and opened
    /// whole in the editor on a click, a copy button, and the attachments.
    pub(super) fn prompt_card(
        &self,
        text: &str,
        attachments: &[(String, ContentBlock)],
        row: RowId,
    ) -> gtk::Box {
        // No spacing: every row carries the inset itself.
        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.add_css_class("card");
        card.set_margin_top(PROMPT_TOP);
        // No margin below: the first step under the box carries the gap
        // (`PROMPT_GAP`) so the rail's line can run from the box to its dot.
        card.set_margin_bottom(0);
        card.set_margin_start(ROW_OWN_SIDE);
        card.set_margin_end(ROW_OWN_SIDE);
        // Long prompts are clipped in the TEXT, not by the label's line
        // limit, which caps lines per paragraph; the box is the way to the
        // rest (David, 2026-09-07).
        let (head, hidden) = clip_prompt(text);
        if !text.is_empty() {
            let label = gtk::Label::builder()
                .use_markup(true)
                .label(crate::issue_pill::pillify(&head, &self.issues))
                .attributes(&no_hyphens())
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .max_width_chars(40)
                .xalign(0.0)
                .hexpand(true)
                .selectable(hidden == 0)
                .focusable(false)
                .margin_top(CARD_INSET)
                .margin_bottom(if hidden > 0 { 2 } else { CARD_INSET })
                .margin_start(CARD_INSET)
                .margin_end(CARD_INSET)
                .build();
            crate::issue_pill::install_tooltips(&label, self.issues.clone());
            {
                let events = self.workspace.events.clone();
                label.connect_activate_link(move |_, url| {
                    match url.strip_prefix(crate::issue_pill::SCHEME) {
                        Some(id) => {
                            events.publish(taste_core::Event::RevealIssueRequested(id.to_string()));
                            glib::Propagation::Stop
                        }
                        None => glib::Propagation::Proceed,
                    }
                });
            }
            // At the card's right edge, centred on the first line of text
            // (David, 2026-09-16).
            let copy = gtk::Button::builder()
                .icon_name("edit-copy-symbolic")
                .tooltip_text("Copy prompt")
                .css_classes(["flat", "circular"])
                .halign(gtk::Align::End)
                .valign(gtk::Align::Start)
                .margin_top(4)
                .margin_end(4)
                .build();
            let prompt = text.to_string();
            copy.connect_clicked(move |button| {
                button.clipboard().set_text(&prompt);
            });
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            line.set_hexpand(true);
            let text_frame = crate::issue_pill::PillText::wrap(&label);
            text_frame.set_hexpand(true);
            text_frame.set_halign(gtk::Align::Fill);
            line.append(&text_frame);
            line.append(&copy);
            card.append(&line);
            if hidden > 0 {
                let more = gtk::Label::builder()
                    .label(format!(
                        "… {hidden} more line{} · click to open in the editor",
                        if hidden == 1 { "" } else { "s" }
                    ))
                    .xalign(0.0)
                    .css_classes(["caption", "dim-label"])
                    .margin_start(CARD_INSET)
                    .margin_end(CARD_INSET)
                    .margin_bottom(CARD_INSET - 2)
                    .build();
                card.append(&more);
                let key = self.row_doc(row, "prompt");
                let open = self.opener(
                    &key,
                    Document::Text {
                        title: stamp_now(),
                        body: text.to_string(),
                        markdown: false,
                        role: crate::chatdoc::TextRole::Prompt,
                    },
                );
                let click = gtk::GestureClick::new();
                click.connect_released(move |_, _, _, _| open());
                card.add_controller(click);
                card.set_cursor_from_name(Some("pointer"));
                card.set_tooltip_text(Some("Open the whole prompt in the editor"));
                card.add_css_class("clipped-prompt");
            }
        }
        // Images get an openable thumbnail; everything else stays a name on
        // the paperclip row.
        let mut thumbnails: Vec<(String, gtk::gdk::Texture)> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for (label, block) in attachments {
            match block {
                ContentBlock::Image(image) => match decode_image(image) {
                    Some(texture) => thumbnails.push((label.clone(), texture)),
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
                .build()
                .full_text_on_hover();
            attached.append(&label);
            card.append(&attached);
        }
        card
    }

    /// An answer: its Markdown, drawn as it streams and as it ends alike —
    /// clipped long, the whole of it one click away, and whole when the
    /// turn's suggested replies choose from it.
    fn answer_widget(&self, row: &Row, answer: &Answer) -> (Railed, Rc<AnswerView>) {
        let rail = row.rail.unwrap_or_default();
        let events = self.workspace.events.clone();
        let on_link: Rc<dyn Fn(&str)> =
            Rc::new(
                move |url: &str| match url.strip_prefix(crate::issue_pill::SCHEME) {
                    Some(id) => {
                        events.publish(taste_core::Event::RevealIssueRequested(id.to_string()))
                    }
                    None => events.publish(taste_core::Event::OpenUrlRequested(url.to_string())),
                },
            );
        let doc = crate::markdown_view::Incremental::new(
            on_link,
            Some(self.issues.clone()),
            self.document_base(),
        );
        // As a response's prose is set: flush to the column, its blocks a
        // little closer than a document's.
        doc.root.set_margin_top(0);
        doc.root.set_margin_bottom(0);
        doc.root.set_margin_start(0);
        doc.root.set_margin_end(0);
        doc.root.set_spacing(6);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 2);
        body.set_hexpand(true);
        body.append(&doc.root);
        let more = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&more);
        let view = Rc::new(AnswerView {
            body: body.clone(),
            doc,
            more,
            more_for: Cell::new(None),
            pending: RefCell::new(None),
            queued: Cell::new(false),
            drawn: Cell::new(false),
        });
        self.fill_answer(&view, row.id, answer);
        (self.on_rail(&body, rail, row.dot), view)
    }

    /// Draw `answer` into its view: the head of a long one — the same head
    /// as it streams as once it has ended, `clip_prose` deciding it from
    /// the first lines alone — and the line that opens the rest. Streaming,
    /// the text is drawn at the next frame, once however many updates
    /// arrived; ended, at once.
    fn fill_answer(&self, view: &Rc<AnswerView>, id: super::model::RowId, answer: &Answer) {
        let (head, hidden) = if answer.whole {
            (answer.text.clone(), 0)
        } else {
            crate::chatdoc::clip_prose(&answer.text, PROSE_CLIP_LINES, PROSE_CLIP_CHARS)
        };
        if answer.done || !view.drawn.replace(true) {
            view.pending.borrow_mut().take();
            view.doc.set_text(&head);
        } else {
            *view.pending.borrow_mut() = Some(head);
            if !view.queued.replace(true) {
                let weak = Rc::downgrade(view);
                view.body.add_tick_callback(move |_, _| {
                    if let Some(view) = weak.upgrade() {
                        view.queued.set(false);
                        if let Some(text) = view.pending.borrow_mut().take() {
                            view.doc.set_text(&text);
                        }
                    }
                    glib::ControlFlow::Break
                });
            }
        }
        // The more line, said anew when what it hides changes, and once
        // more when the answer ends, so the document it opens is the whole.
        let wanted = (hidden > 0).then_some((hidden, answer.done));
        if view.more_for.replace(wanted) != wanted {
            while let Some(child) = view.more.first_child() {
                view.more.remove(&child);
            }
            if hidden > 0 {
                let key = self.row_doc(id, "response");
                view.more.append(&self.more_button(
                    &key,
                    hidden,
                    "Open the whole response in the editor",
                    Document::Text {
                        title: stamp_now(),
                        body: answer.text.clone(),
                        markdown: true,
                        role: crate::chatdoc::TextRole::Response,
                    },
                ));
            }
        }
    }

    /// Where a response's paths point, and where its images are: the
    /// environment's checkout.
    pub(super) fn document_base(&self) -> Option<crate::markdown_view::DocumentBase> {
        self.environments.get(&self.environment).map(|supervisor| {
            let root = supervisor.checkout().path().to_path_buf();
            let events = self.workspace.events.clone();
            crate::markdown_view::DocumentBase {
                files: supervisor.files(),
                dir: root.clone(),
                root,
                open_file: Rc::new(move |path, at| {
                    events.publish(taste_core::Event::OpenFileRequested { path, at });
                }),
            }
        })
    }

    /// A thought: an aside, folded under a dim header that says how long it
    /// took and what it opened with.
    fn thought_widget(&self, row: &Row, thought: &Thought) -> Railed {
        let view = thought_body(&thought.text);
        let header = gtk::Label::builder()
            .label(thought.header.as_deref().unwrap_or("Thinking…"))
            .css_classes(["dim-label", "caption"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(40)
            .xalign(0.0)
            .build()
            .full_text_on_hover();
        let expander = gtk::Expander::builder()
            .label_widget(&header)
            .child(&view)
            .build();
        self.on_rail(&expander, row.rail.unwrap_or_default(), row.dot)
    }

    /// A background task's end, as a step: what it was, how it ended on its
    /// dot, and its report one click away.
    fn task_widget(&self, row: &Row, task: &Task) -> Railed {
        let label = gtk::Label::builder()
            .label(&task.title)
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .attributes(&no_hyphens())
            .css_classes(["tool-title"])
            .build();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 2);
        body.append(&label);
        if let Some(report) = &task.report {
            let key = self.row_doc(row.id, "task");
            let lines = report.lines().count();
            body.append(&self.open_line(
                &key,
                &format!(
                    "report · {lines} line{} · open in the editor",
                    if lines == 1 { "" } else { "s" }
                ),
                "Open the task's report in the editor",
                Document::Text {
                    title: stamp_now(),
                    body: report.clone(),
                    markdown: true,
                    role: crate::chatdoc::TextRole::Response,
                },
            ));
        }
        self.on_rail(&body, row.rail.unwrap_or_default(), row.dot)
    }

    /// The turn's plan, as a checklist on the rail.
    fn plan_widget(&self, row: &Row, plan: &PlanCard) -> Railed {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 2);
        card.set_widget_name("plan-card");
        for (entry, status) in &plan.entries {
            let icon = gtk::Image::from_icon_name(match status {
                PlanEntryStatus::Completed => "object-select-symbolic",
                PlanEntryStatus::InProgress => "content-loading-symbolic",
                _ => "radio-symbolic",
            });
            icon.set_valign(gtk::Align::Start);
            icon.set_margin_top(3);
            let label = gtk::Label::builder()
                .label(entry)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .max_width_chars(40)
                .xalign(0.0)
                .hexpand(true)
                .build();
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            line.append(&icon);
            line.append(&label);
            card.append(&line);
        }
        self.on_rail(&card, row.rail.unwrap_or_default(), row.dot)
    }

    /// A note: one dim line, an aside between two steps — and a note too
    /// long for its line wears a disclosure, since an ellipsis in the
    /// middle of a reason is half an error.
    fn note_widget(&self, row: &Row, text: &str) -> Railed {
        let label = gtk::Label::builder()
            .label(text)
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(40)
            .lines(1)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .margin_top(2)
            .margin_bottom(2)
            .build()
            .full_text_on_hover();
        let content: gtk::Widget = if text.lines().count() <= 1 && text.chars().count() <= 80 {
            label.upcast()
        } else {
            let disclose = gtk::ToggleButton::builder()
                .icon_name("pan-down-symbolic")
                .css_classes(["flat", "circular", "note-disclose", "dim-label"])
                .valign(gtk::Align::Start)
                .tooltip_text("Read the whole note")
                .build();
            let line = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(4)
                .build();
            line.append(&label);
            line.append(&disclose);
            let toggled = label.clone();
            disclose.connect_toggled(move |button| {
                let open = button.is_active();
                toggled.set_ellipsize(if open {
                    gtk::pango::EllipsizeMode::None
                } else {
                    gtk::pango::EllipsizeMode::End
                });
                toggled.set_lines(if open { -1 } else { 1 });
                button.set_icon_name(if open {
                    "pan-up-symbolic"
                } else {
                    "pan-down-symbolic"
                });
                button.set_tooltip_text(Some(if open {
                    "Fold this note back to one line"
                } else {
                    "Read the whole note"
                }));
            });
            line.upcast()
        };
        self.on_rail(&content, row.rail.unwrap_or_default(), row.dot)
    }

    /// A step's title, as shown: an act's headline, or the call worded in
    /// English, or the adapter's own words (`step_title`).
    pub(super) fn step_shown_title(step: &Step) -> String {
        match step.act {
            Some(kind) => act_headline(kind, step.input.as_ref(), step.act_output.as_ref()),
            None => step_title(&step.title, step.input.as_ref(), step.output.as_ref())
                .unwrap_or_else(|| single_line(&step.title, 200)),
        }
    }

    /// One tool call on the timeline, Claude Code's shape: the dot says how
    /// it went, the title what it was, a dim line under it what came of it;
    /// opened, what it did.
    fn step_widget(&self, row: &Row, step: &Step) -> Railed {
        let shown = Self::step_shown_title(step);
        let title_label = gtk::Label::builder()
            .xalign(0.0)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["tool-title"])
            .use_markup(true)
            .build()
            .full_text_on_hover();
        title_label.set_markup(&self.pillify_headline(&shown));
        // The whole sentence on hover; the raw name never (David,
        // 2026-10-09).
        title_label.set_tooltip_text(Some(&match step.act {
            Some(_) => shown.clone(),
            None => step_title(&step.title, step.input.as_ref(), step.output.as_ref())
                .unwrap_or_else(|| step.title.clone()),
        }));
        crate::issue_pill::install_tooltips(&title_label, self.issues.clone());
        {
            let events = self.workspace.events.clone();
            title_label.connect_activate_link(move |_, url| {
                match url.strip_prefix(crate::issue_pill::SCHEME) {
                    Some(id) => {
                        events.publish(taste_core::Event::RevealIssueRequested(id.to_string()));
                        glib::Propagation::Stop
                    }
                    None => glib::Propagation::Proceed,
                }
            });
        }
        // Only what shows is built: a row is styled, measured, and laid
        // out for every widget in it, hidden or not, each time it comes on
        // screen.
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        if let Some(kind) = step.act {
            title_label.add_css_class("act-title");
            let (icon, tone) = act_icon(kind, step.input.as_ref(), step.act_output.as_ref());
            let act_glyph = gtk::Image::from_icon_name(icon);
            act_glyph.set_valign(gtk::Align::Center);
            act_glyph.add_css_class("act-icon");
            if let Some(tone) = tone {
                act_glyph.add_css_class(tone);
            }
            header.append(&act_glyph);
        }
        // A command's step in the face its IN/OUT block is in.
        let command = is_command_call(step.kind, &step.title);
        let command_title = command
            && match mcp_tool_name(&step.title).as_deref() {
                Some("ide_exec") => command_input_line("ide_exec", step.input.as_ref()).is_some(),
                Some(_) => false,
                None => true,
            };
        if command_title {
            title_label.add_css_class("monospace");
        }
        header.append(&crate::issue_pill::PillText::wrap(&title_label));
        // Right-hand end of the header: who said yes, not how it went.
        if let Some((icon, tooltip)) = &step.permission {
            let permission = gtk::Image::from_icon_name(icon);
            permission.set_valign(gtk::Align::Center);
            permission.add_css_class("dim-label");
            permission.set_tooltip_text(Some(tooltip));
            header.append(&permission);
        }
        let has_content = step_has_content(step);
        let killable = step.running
            && step.kind == ToolKind::Execute
            && running_shell(&self.workspace.shells, &self.environment, &step.title)
                .is_some_and(|entry| entry.killable);
        // The disclosure, at the END of the line, only once there is
        // something to open onto; it shares its slot with the Kill.
        if has_content && !killable {
            let arrow = gtk::Image::from_icon_name(if step.expanded {
                "pan-down-symbolic"
            } else {
                "pan-end-symbolic"
            });
            arrow.set_valign(gtk::Align::Center);
            arrow.add_css_class("dim-label");
            header.append(&arrow);
        }
        let head = gtk::Box::new(gtk::Orientation::Vertical, 0);
        head.append(&header);
        if let Some(text) = self.step_summary(step).filter(|_| !step.expanded) {
            let summary = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["caption", "dim-label", "step-summary"])
                .label(text)
                .build()
                .full_text_on_hover();
            if command {
                summary.add_css_class("monospace");
            }
            head.append(&summary);
        }
        let toggle = gtk::Button::builder()
            .child(&head)
            .css_classes(["flat", "step-toggle"])
            .can_target(has_content)
            .can_focus(has_content)
            .hexpand(true)
            .build();
        {
            let weak = self.weak_self.borrow().clone();
            let id = row.id;
            let open = !step.expanded;
            toggle.connect_clicked(move |_| {
                if let Some(pane) = weak.upgrade() {
                    pane.set_step_expanded(id, open);
                }
            });
        }
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        if killable {
            let kill = gtk::Button::builder()
                .icon_name("media-playback-stop-symbolic")
                .css_classes(["flat", "circular", "step-kill"])
                .valign(gtk::Align::Start)
                .tooltip_text("Stop this command. The output stays, and the agent is told it died.")
                .build();
            let shells = self.workspace.shells.clone();
            let env = self.environment.clone();
            let command = step.title.clone();
            kill.connect_clicked(move |button| {
                button.set_sensitive(false);
                if let Some(entry) = running_shell(&shells, &env, &command) {
                    shells.kill(entry.id);
                }
            });
            let head_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            head_row.append(&toggle);
            head_row.append(&kill);
            body.append(&head_row);
        } else {
            body.append(&toggle);
        }
        if step.expanded && has_content {
            let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
            content.set_margin_top(2);
            content.set_margin_bottom(4);
            self.step_content(step, &content);
            body.append(&content);
        }
        let railed = self.on_rail(&body, row.rail.unwrap_or_default(), row.dot);
        // The traffic light: green finished, red failed, the question's
        // amber while one is open, the spinner while it runs.
        let rail = &railed.rail;
        rail.set_mark(match (step.waiting, step.tone) {
            (true, _) => super::rail::Mark::Tone("wait"),
            (false, "cut") => super::rail::Mark::Cut,
            (false, tone) => super::rail::Mark::Tone(tone),
        });
        rail.set_running(step.running);
        if step.tone == "cut" {
            rail.set_tooltip_text(Some("Never reported finishing: its turn ended first"));
        } else if step.rebuild_took_the_agent {
            rail.set_tooltip_text(Some(
                "The rebuild started, and stopped this agent's container as a rebuild does — \
                 the call's connection went with it. The agent comes back when the rebuild \
                 finishes.",
            ));
        }
        railed
    }

    /// The step's one dim line while it is closed: the last line the
    /// command printed, the size of the edit, the fields or lines of the
    /// result.
    fn step_summary(&self, step: &Step) -> Option<String> {
        let content = step.content.as_ref()?;
        if is_command_call(step.kind, &step.title) {
            return crate::chatdoc::digest(&step_command_output(step, content));
        }
        let mut summary = None;
        for item in content {
            match item {
                ToolCallContent::Diff(diff) => {
                    let (added, removed) = crate::chatdoc::edit_counts(&edit_from(diff));
                    summary = Some(format!("+{added} −{removed}"));
                }
                ToolCallContent::Content(block) => {
                    if let Some(fields) = content_text(&block.content).and_then(|t| json_fields(&t))
                    {
                        summary.get_or_insert_with(|| {
                            fields
                                .iter()
                                .find(|(k, _)| k == "Status" || k == "State")
                                .map(|(_, v)| v.clone())
                                .unwrap_or_else(|| match fields.len() {
                                    1 => "1 field".to_string(),
                                    n => format!("{n} fields"),
                                })
                        });
                    } else if let Some(text) = content_text(&block.content) {
                        summary.get_or_insert_with(|| {
                            let lines = text.lines().count();
                            format!("{lines} line{}", if lines == 1 { "" } else { "s" })
                        });
                    }
                }
                _ => {}
            }
        }
        summary
    }

    /// What a step opens onto, built into `into`: a command's IN/OUT
    /// block, an edit's diff, a result's fields or text — each openable
    /// whole in the editor.
    fn step_content(&self, step: &Step, into: &gtk::Box) {
        let Some(content) = step.content.as_ref() else {
            return;
        };
        if is_command_call(step.kind, &step.title) {
            // Claude Code's IN/OUT: IN is what was asked, OUT everything
            // printed (David, 2026-09-07; i-0016, i-0017).
            let command = mcp_tool_name(&step.title)
                .filter(|name| is_ide_command_tool(name))
                .and_then(|name| command_input_line(&name, step.input.as_ref()))
                .unwrap_or_else(|| step.title.clone());
            let output = step_command_output(step, content);
            let key = format!("{}/command", step.call);
            let open = self.opener(
                &key,
                Document::Command {
                    command: command.clone(),
                    output: output.clone(),
                },
            );
            into.append(&crate::chatdoc::command_block(
                &command,
                &output,
                Some(OUTPUT_CLIP_LINES),
                Some(open),
            ));
            return;
        }
        let title = single_line(&Self::step_shown_title(step), 60);
        for (index, item) in content.iter().enumerate() {
            match item {
                ToolCallContent::Diff(diff) => {
                    let edit = edit_from(diff);
                    let view = crate::chatdoc::diff_view(
                        &edit,
                        Some(DIFF_CLIP_LINES),
                        crate::chatdoc::Layout::Auto,
                    );
                    let key = format!("{}/edit-{index}", step.call);
                    let open = self.opener(&key, Document::Edit(edit.clone()));
                    into.append(&crate::chatdoc::diff_header(
                        &edit,
                        view.added,
                        view.removed,
                        Some(open),
                    ));
                    into.append(&view.widget);
                }
                ToolCallContent::Content(block) => {
                    let Some(text) = content_text(&block.content) else {
                        continue;
                    };
                    let key = format!("{}/result-{index}", step.call);
                    if let Some(fields) = json_fields(&text) {
                        into.append(&fields_grid(&fields));
                        into.append(&self.more_button(
                            &key,
                            0,
                            "Open the whole result in the editor",
                            Document::Text {
                                title: title.clone(),
                                body: text,
                                markdown: false,
                                role: crate::chatdoc::TextRole::Result,
                            },
                        ));
                    } else {
                        let (head, hidden) = crate::chatdoc::clip_lines(&text, OUTPUT_CLIP_LINES);
                        into.append(
                            &gtk::Label::builder()
                                .label(head)
                                .attributes(&no_hyphens())
                                .wrap(true)
                                .wrap_mode(gtk::pango::WrapMode::WordChar)
                                .max_width_chars(40)
                                .xalign(0.0)
                                .selectable(true)
                                .focusable(false)
                                .css_classes(["caption"])
                                .build(),
                        );
                        if hidden > 0 {
                            into.append(&self.more_button(
                                &key,
                                hidden,
                                "Open the whole result in the editor",
                                Document::Text {
                                    title: title.clone(),
                                    body: text,
                                    markdown: false,
                                    role: crate::chatdoc::TextRole::Result,
                                },
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// A thought's text, as it shows opened: a label, not a text view. A
/// `GtkTextView` reports the height of its last layout, and a thought's has
/// never been laid out when its expander first opens — so it answered at
/// its narrowest, wrapped a few characters a line, and the row stood a
/// screen of blank under the text until it was closed and opened again,
/// by which time the view had been laid out at its real width (David,
/// 2026-10-24: "expanding some boxes in chat creates a huge, vertical
/// blank area"). A label measures height for width honestly, as the OUT
/// block learned (`chatdoc::ansi_label`).
fn thought_body(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .attributes(&no_hyphens())
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        // Its natural width is a line's, not the whole thought's: the
        // column decides how wide it is.
        .max_width_chars(40)
        .xalign(0.0)
        .hexpand(true)
        .selectable(true)
        .focusable(false)
        .css_classes(["dim-label"])
        .build()
}

/// What a command's step printed: its content's text, the adapter's
/// terminal capture when the content is only the terminal, and for the
/// IDE's own command tools what the command printed rather than the JSON
/// the agent was answered in.
fn step_command_output(step: &Step, content: &[ToolCallContent]) -> String {
    let mut output = content
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Content(block) => content_text(&block.content),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if output.trim().is_empty() {
        if let Some(text) = &step.terminal {
            output = text.clone();
        }
    }
    mcp_tool_name(&step.title)
        .filter(|name| is_ide_command_tool(name))
        .and_then(|_| command_console_text(&output))
        .unwrap_or(output)
}

/// Whether a step has anything to open onto.
fn step_has_content(step: &Step) -> bool {
    let Some(content) = step.content.as_ref() else {
        return false;
    };
    if is_command_call(step.kind, &step.title) {
        return true;
    }
    content.iter().any(|item| match item {
        ToolCallContent::Diff(_) => true,
        ToolCallContent::Content(block) => content_text(&block.content).is_some(),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use gtk::prelude::*;

    /// A thought's body is as tall as its text at the width it is given,
    /// before it has ever been laid out: at a chat column's width, a
    /// fraction of what it needs a few characters wide. A text view gave
    /// the same tall answer at any width until it had been allocated.
    #[test]
    fn a_thought_measures_its_height_at_the_width_it_is_given() {
        crate::gtk_test::on_gtk_thread("thought height: no display — skipped", || {
            let text = "The row stood a screen of blank under the text. ".repeat(40);
            let body = super::thought_body(&text);
            let (narrow, _, _, _) = body.measure(gtk::Orientation::Vertical, 60);
            let (column, _, _, _) = body.measure(gtk::Orientation::Vertical, 360);
            assert!(column > 0);
            assert!(
                column * 4 < narrow,
                "{column}px at 360 wide against {narrow}px at 60: the width is not honoured"
            );
        });
    }
}

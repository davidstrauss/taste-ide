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
    lines: Option<(gtk::Box, gtk::Box)>,
    /// A streaming answer's buffer: a stream is hundreds of updates, each a
    /// few words.
    stream: Option<gtk::TextBuffer>,
}

/// A row's piece of the rail, as [`ChatPane::on_rail`] built it.
struct Railed {
    row: gtk::Box,
    dot: gtk::Box,
    spinner: gtk::Spinner,
    top: gtk::Box,
    bottom: gtk::Box,
}

impl ChatPane {
    /// `row`'s widgets.
    pub(super) fn build_row(&self, row: &Row) -> RowView {
        let mut stream = None;
        let (inner, railed): (gtk::Widget, Option<Railed>) = match &row.kind {
            Kind::User(user) => (self.user_widget(row.id, user).upcast(), None),
            Kind::Answer(answer) => {
                let (railed, buffer) = self.answer_widget(row, answer);
                stream = buffer;
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
            lines: railed.map(|railed| (railed.top, railed.bottom)),
            stream,
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
            && match (&view.shown.kind, &row.kind, &view.stream) {
                (Kind::Answer(was), Kind::Answer(is), Some(buffer)) if !is.done => {
                    let grew = is.text.len() >= was.text.len()
                        && is.text.is_char_boundary(was.text.len())
                        && is.text.starts_with(was.text.as_str());
                    if grew && is.text.len() > was.text.len() {
                        let mut end = buffer.end_iter();
                        buffer.insert(&mut end, &is.text[was.text.len()..]);
                    }
                    grew
                }
                (was, is, _) => was == is,
            };
        if !same {
            return false;
        }
        if view.shown.rail != row.rail {
            match (&view.lines, row.rail) {
                (Some((top, bottom)), Some(rail))
                    if view.shown.rail.map(|r| r.after_prompt) == Some(rail.after_prompt) =>
                {
                    top.set_visible(rail.top);
                    bottom.set_visible(rail.bottom);
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
    /// the row, and the dot and its spinner for a step to paint.
    fn on_rail(&self, content: &impl IsA<gtk::Widget>, rail: Rail, dot_kind: Dot) -> Railed {
        // Under a prompt the step carries the gap itself, so the line can
        // cross it: the dot slot and the content move down by the gap and
        // the top segment grows by it.
        let gap = if rail.after_prompt {
            PROMPT_GAP
        } else {
            STEP_GAP
        };
        let top = gtk::Box::builder()
            .css_classes(["rail-line"])
            .halign(gtk::Align::Center)
            .height_request(RAIL_LINE / 2 + gap)
            .visible(rail.top)
            .build();
        let bottom = gtk::Box::builder()
            .css_classes(["rail-line"])
            .halign(gtk::Align::Center)
            .vexpand(true)
            .visible(rail.bottom)
            .build();
        // The break in the line for the spinner: zero-height whenever the
        // dot is what shows (David, 2026-09-16: "The vertical timeline line
        // should not be visible inside spinners").
        let hole = gtk::Box::builder()
            .halign(gtk::Align::Center)
            .height_request(0)
            .build();
        let line = gtk::Box::new(gtk::Orientation::Vertical, 0);
        line.append(&top);
        line.append(&hole);
        line.append(&bottom);
        let dot = gtk::Box::builder()
            .css_classes(["rail-dot"])
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();
        match dot_kind {
            Dot::Plain => {}
            Dot::Note => dot.add_css_class("note"),
            Dot::Tone(tone) => dot.add_css_class(tone),
        }
        let spinner = gtk::Spinner::builder()
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .visible(false)
            .build();
        spinner.set_size_request(RAIL_SPINNER, RAIL_SPINNER);
        spinner
            .bind_property("visible", &hole, "height-request")
            .transform_to(|_, visible: bool| {
                Some(
                    if visible {
                        RAIL_SPINNER + 2 * RAIL_HOLE_PAD
                    } else {
                        0
                    }
                    .to_value(),
                )
            })
            .sync_create()
            .build();
        {
            let top = top.clone();
            hole.connect_height_request_notify(move |hole| {
                if let Some(rail) = top.parent().and_then(|line| line.parent()) {
                    fit_top_segment(&rail, &top, hole);
                }
            });
        }
        // One text line tall, at the top: the dot is centred on the step's
        // first line, whatever the step grows to below it.
        let slot = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .height_request(RAIL_LINE)
            .valign(gtk::Align::Start)
            .margin_top(gap)
            .build();
        slot.append(&dot);
        slot.append(&spinner);
        // The rail never sizes its row: the line and the dot's slot are laid
        // over the height the content decides.
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&gtk::Box::new(gtk::Orientation::Vertical, 0)));
        overlay.add_overlay(&line);
        overlay.add_overlay(&slot);
        overlay.set_size_request(RAIL_WIDTH, -1);
        let row_box = gtk::Box::new(gtk::Orientation::Horizontal, RAIL_GAP);
        row_box.set_margin_start(ROW_OWN_SIDE);
        row_box.set_margin_end(ROW_OWN_SIDE);
        row_box.append(&overlay);
        content.set_hexpand(true);
        content.set_margin_top(gap);
        content.add_css_class("step-content");
        row_box.append(content);
        row_box.connect_map(centre_dot_after_layout);
        Railed {
            row: row_box,
            dot,
            spinner,
            top,
            bottom,
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
        // Ask for a Turn at the end of a queued message's badge: the agent
        // is asked, at its next use of the IDE's tools, to finish its step
        // and end the turn (`taste_core::nudge`), which sends the message
        // without stopping anything.
        if let (Delivery::Queued, Some(prompt)) = (&user.delivery, &user.prompt) {
            let button = gtk::Button::builder()
                .label("Ask for a Turn")
                .css_classes(["flat", "caption"])
                .valign(gtk::Align::Center)
                .margin_end(CARD_INSET)
                .tooltip_text(
                    "Ask the agent to finish the step it is on and end its turn, so this \
                     message goes",
                )
                .build();
            let weak = self.weak_self.borrow().clone();
            let prompt = prompt.clone();
            button.connect_clicked(move |button| {
                button.set_sensitive(false);
                if let Some(pane) = weak.upgrade() {
                    pane.ask_for_turn(&prompt);
                }
            });
            line.append(&button);
        }
        card.append(&line);
        card
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
                .halign(gtk::Align::Center)
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

    /// An answer: its text as it streams, rendered as Markdown once it has
    /// ended — clipped long, the whole of it one click away, and whole when
    /// the turn's suggested replies choose from it.
    fn answer_widget(&self, row: &Row, answer: &Answer) -> (Railed, Option<gtk::TextBuffer>) {
        let rail = row.rail.unwrap_or_default();
        if !answer.done {
            let view = gtk::TextView::builder()
                .editable(false)
                .cursor_visible(false)
                .wrap_mode(gtk::WrapMode::WordChar)
                .build();
            let buffer = view.buffer();
            buffer.set_text(&answer.text);
            return (self.on_rail(&view, rail, row.dot), Some(buffer));
        }
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
        let (head, hidden) = if answer.whole {
            (answer.text.clone(), 0)
        } else {
            crate::chatdoc::clip_prose(&answer.text, PROSE_CLIP_LINES, PROSE_CLIP_CHARS)
        };
        let body = gtk::Box::new(gtk::Orientation::Vertical, 2);
        body.set_hexpand(true);
        body.append(&response_prose(
            &head,
            on_link,
            Some(self.issues.clone()),
            self.document_base(),
        ));
        if hidden > 0 {
            let key = self.row_doc(row.id, "response");
            body.append(&self.more_button(
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
        (self.on_rail(&body, rail, row.dot), None)
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
                open_file: Rc::new(move |path, line| {
                    events.publish(taste_core::Event::OpenFileRequested { path, line });
                }),
            }
        })
    }

    /// A thought: an aside, folded under a dim header that says how long it
    /// took and what it opened with.
    fn thought_widget(&self, row: &Row, thought: &Thought) -> Railed {
        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .css_classes(["dim-label"])
            .build();
        view.buffer().set_text(&thought.text);
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
        let act_glyph = gtk::Image::new();
        act_glyph.set_valign(gtk::Align::Center);
        act_glyph.add_css_class("act-icon");
        act_glyph.set_visible(step.act.is_some());
        if let Some(kind) = step.act {
            title_label.add_css_class("act-title");
            let (icon, tone) = act_icon(kind, step.input.as_ref(), step.act_output.as_ref());
            act_glyph.set_icon_name(Some(icon));
            if let Some(tone) = tone {
                act_glyph.add_css_class(tone);
            }
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
        let summary_text = self.step_summary(step);
        let summary = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption", "dim-label", "step-summary"])
            .label(summary_text.as_deref().unwrap_or_default())
            .visible(summary_text.is_some() && !step.expanded)
            .build()
            .full_text_on_hover();
        if command {
            summary.add_css_class("monospace");
        }
        // Right-hand end of the header: who said yes, not how it went.
        let permission = gtk::Image::new();
        permission.set_valign(gtk::Align::Center);
        permission.add_css_class("dim-label");
        permission.set_visible(step.permission.is_some());
        if let Some((icon, tooltip)) = &step.permission {
            permission.set_icon_name(Some(icon));
            permission.set_tooltip_text(Some(tooltip));
        }
        let has_content = step_has_content(step);
        let killable = step.running
            && step.kind == ToolKind::Execute
            && running_shell(&self.workspace.shells, &self.environment, &step.title)
                .is_some_and(|entry| entry.killable);
        // The disclosure, at the END of the line, only once there is
        // something to open onto; it shares its slot with the Kill.
        let arrow = gtk::Image::from_icon_name(if step.expanded {
            "pan-down-symbolic"
        } else {
            "pan-end-symbolic"
        });
        arrow.set_valign(gtk::Align::Center);
        arrow.add_css_class("dim-label");
        arrow.set_visible(has_content && !killable);
        let kill = gtk::Button::builder()
            .icon_name("media-playback-stop-symbolic")
            .css_classes(["flat", "circular", "step-kill"])
            .valign(gtk::Align::Start)
            .visible(killable)
            .tooltip_text("Stop this command. The output stays, and the agent is told it died.")
            .build();
        {
            let shells = self.workspace.shells.clone();
            let env = self.environment.clone();
            let command = step.title.clone();
            kill.connect_clicked(move |button| {
                button.set_sensitive(false);
                if let Some(entry) = running_shell(&shells, &env, &command) {
                    shells.kill(entry.id);
                }
            });
        }
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.append(&act_glyph);
        header.append(&crate::issue_pill::PillText::wrap(&title_label));
        header.append(&permission);
        header.append(&arrow);
        let head = gtk::Box::new(gtk::Orientation::Vertical, 0);
        head.append(&header);
        head.append(&summary);
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
        let head_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        head_row.append(&toggle);
        head_row.append(&kill);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&head_row);
        if step.expanded && has_content {
            let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
            content.set_margin_top(2);
            content.set_margin_bottom(4);
            self.step_content(step, &content);
            body.append(&content);
        }
        let railed = self.on_rail(&body, row.rail.unwrap_or_default(), row.dot);
        let (dot, spinner) = (&railed.dot, &railed.spinner);
        // The traffic light: green finished, red failed, the question's
        // amber while one is open, the spinner while it runs.
        dot.add_css_class(if step.waiting { "wait" } else { step.tone });
        spinner.set_visible(step.running);
        dot.set_visible(!step.running);
        if step.running {
            spinner.start();
        }
        if step.tone == "cut" {
            dot.set_tooltip_text(Some("Never reported finishing: its turn ended first"));
        } else if step.rebuild_took_the_agent {
            dot.set_tooltip_text(Some(
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

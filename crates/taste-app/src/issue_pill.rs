//! Issue references in chat prose, as pills.
//!
//! An agent writing "filed i-0042 for the flicker" names a thing the
//! backlog holds, and the reader should not have to go and look it up:
//! the reference is drawn as a pill carrying the issue's title, it does
//! not wrap inside itself, its tooltip has the rest of the facts, and a
//! click selects the issue — or its environment, once it has one — in the
//! backlog (David, 2026-09-16: "formatted as text pills that do not wrap
//! (within the pill), show the actual issue title, truncate to a max
//! length if the title is really long, allow using the tooltip to see
//! details, and select/focus the issue/env in the backlog on click").
//!
//! # Where this lives in the rendering
//!
//! Transcript prose is a `GtkLabel` with Pango markup
//! (`markdown_view`), so a pill is markup too: a link whose `href` is the
//! [`SCHEME`] plus the id, wrapping a span with a background. That is what
//! makes the three behaviours cheap. The click is the label's
//! `activate-link`; the tooltip is `query-tooltip` asking the label which
//! link is under the pointer (`current_uri`); and not wrapping inside the
//! pill is the text itself — no-break spaces and a no-break hyphen — so
//! the pill moves to the next line whole, as a word does. What Pango
//! markup cannot do is round the corners, so the shape is not markup:
//! the label sits in a [`PillText`], a one-child widget that finds each
//! pill's span in the label's own layout and paints a capsule under it
//! before the text is drawn — the shape the search badges have
//! (`.hit-badge`: fully rounded, 6px of side padding), in the accent's
//! tint on both themes (David, 2026-09-16: "some margin from the text to
//! the pill boundary … rounded on the corners … the same rendering style
//! as the search result counts. By style, I don't mean color, just
//! shape"). The span keeps a background attribute at an invisible alpha:
//! it is how the painter finds the pills, not what draws them, and the
//! padding is no-break spaces inside the span, so the text beside a pill
//! never sits on its capsule.
//!
//! The facts come from [`IssueIndex`], one per window, filled from the
//! same read of `refs/taste/issues` that fills the backlog. A reference
//! to an id the index does not have is still a pill, read as a reference
//! to a **deleted** issue: ids are random and never reused, so an id the
//! ref no longer holds was almost certainly one it once did (David,
//! 2026-09-16: "It's reasonable to assume that a reference to an issue ID
//! that no longer exists" was deleted). Such a pill is drawn struck
//! through and dimmer, its tooltip and its click both say so, and a click
//! does nothing else — there is no row to select ("Ensure there's a
//! fallback for references to deleted issues").

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use gtk::subclass::prelude::*;
use gtk::{gdk, graphene, gsk, pango};

/// The href scheme a pill carries. `markdown_view` routes it to the
/// caller's link handler, and the chat turns it into
/// `Event::RevealIssueRequested`.
pub const SCHEME: &str = "taste-issue:";

/// The longest title a pill shows before it is cut with an ellipsis.
///
/// Sized to the narrowest line a pill has to fit on whole: a prompt card's
/// text beside its Copy button, about 44 characters at the chat's default
/// width. A pill is one unbreakable word, and a word wider than the line
/// is broken mid-word by the label's fallback wrap — a capsule cut in two
/// — so the id, the separator, this many characters, and the ellipsis
/// stay under that line (measured at 36: "half-typed f" / "oll…").
const MAX_TITLE_CHARS: usize = 30;

/// What a pill and its tooltip say about one issue. Everything a reader
/// wants at a glance and nothing that needs the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueFacts {
    pub id: String,
    pub title: String,
    pub state: String,
    pub started_by: Option<String>,
    pub agent: Option<String>,
}

impl IssueFacts {
    pub fn from_issue(issue: &taste_git::Issue) -> Self {
        Self {
            id: issue.id.clone(),
            title: issue.title.clone(),
            state: issue.state().as_str().to_string(),
            started_by: issue.started_by.clone(),
            agent: issue.agent.clone(),
        }
    }
}

/// The issues a window knows, by id, for the pills to read. Shared by
/// every chat pane and refilled whenever the backlog is.
#[derive(Default)]
pub struct IssueIndex {
    facts: RefCell<HashMap<String, IssueFacts>>,
}

pub type SharedIssueIndex = Rc<IssueIndex>;

/// What the index has to say about one id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    Known(IssueFacts),
    /// Not on the backlog: deleted, as far as anyone can tell.
    Deleted,
}

impl Lookup {
    pub fn facts(&self) -> Option<&IssueFacts> {
        match self {
            Lookup::Known(facts) => Some(facts),
            _ => None,
        }
    }
}

impl IssueIndex {
    pub fn set(&self, issues: &[taste_git::Issue]) {
        *self.facts.borrow_mut() = issues
            .iter()
            .map(|issue| (issue.id.clone(), IssueFacts::from_issue(issue)))
            .collect();
    }

    pub fn get(&self, id: &str) -> Option<IssueFacts> {
        self.facts.borrow().get(id).cloned()
    }

    pub fn lookup(&self, id: &str) -> Lookup {
        match self.get(id) {
            Some(facts) => Lookup::Known(facts),
            None => Lookup::Deleted,
        }
    }
}

/// The sentence for an id with no row: a toast on click, and the
/// tooltip's first line. Shared with the backlog, so the pill and the
/// toast a click on it raises agree.
pub fn missing_note(id: &str) -> String {
    format!("{id} is no longer on the backlog — deleted, as far as anyone can tell")
}

/// Every issue id in `text`, as byte ranges: `i-` and the run of
/// `[a-z0-9]` after it, standing alone — not inside a longer word, a path,
/// or a longer token — and shaped like an id (`taste_git::is_issue_id`:
/// six of `[a-z0-9]`, or the sequence era's four or more digits).
pub fn find_refs(text: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == b'i' && bytes[i + 1] == b'-' {
            let before_ok = i == 0 || !is_word_byte(bytes[i - 1]);
            let mut j = i + 2;
            while j < bytes.len() && (bytes[j].is_ascii_lowercase() || bytes[j].is_ascii_digit()) {
                j += 1;
            }
            let after_ok = j == bytes.len() || !is_word_byte(bytes[j]);
            if before_ok && after_ok && taste_git::is_issue_id(&text[i..j]) {
                out.push(i..j);
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'/'
}

/// The title as the pill shows it: cut at [`MAX_TITLE_CHARS`] with an
/// ellipsis, and made unbreakable — spaces to no-break spaces — so the
/// pill never wraps inside itself.
pub fn pill_title(title: &str) -> String {
    let trimmed = title.trim();
    let mut shown: String = trimmed.chars().take(MAX_TITLE_CHARS).collect();
    if trimmed.chars().count() > MAX_TITLE_CHARS {
        shown = shown.trim_end().to_string();
        shown.push('…');
    }
    unbreakable(&shown)
}

/// Spaces and hyphens that hold: a pill is one word to the line breaker.
fn unbreakable(text: &str) -> String {
    text.replace(' ', "\u{00A0}").replace('-', "\u{2011}")
}

/// The pill's markup for one reference, given what the index knows.
///
/// Known: the id and the title. Deleted: the id alone, struck through and
/// dimmer — still a link, so a click can say what happened to it, but not
/// dressed as something the backlog holds.
pub fn pill_markup(id: &str, lookup: &Lookup) -> String {
    let mut text = unbreakable(id);
    if let Some(facts) = lookup.facts() {
        text.push_str("\u{00A0}·\u{00A0}");
        text.push_str(&pill_title(&facts.title));
    }
    let deleted = *lookup == Lookup::Deleted;
    // The background is the marker [`PillText`] paints by, at an alpha
    // Pango draws as nothing (1 of 65535): the capsule is painted under
    // the span, not by it. The no-break spaces each side are the pill's
    // side padding, and being inside the span they are inside the
    // capsule — and unbreakable, so the padding cannot end up on the line
    // before. The link's underline is turned off so the pill is the
    // affordance rather than a hyperlink wearing a box.
    format!(
        "<a href=\"{SCHEME}{}\"><span background=\"{PILL_MARKER}\" background_alpha=\"1\" \
         underline=\"none\"{}>{PILL_PAD}{}{PILL_PAD}</span></a>",
        glib::markup_escape_text(id),
        if deleted {
            " strikethrough=\"true\" alpha=\"60%\""
        } else {
            ""
        },
        glib::markup_escape_text(&text)
    )
}

/// The accent, as the span's background: what [`PillText`] looks for in
/// the layout, and the hue it paints.
const PILL_MARKER: &str = "#3584e4";

/// The pill's side padding, in the text: a no-break space and a narrow
/// one, about 6px at the body size, matching the search badge's padding.
/// Both no-break, so a line never breaks between a pill and its padding.
const PILL_PAD: &str = "\u{00A0}\u{202F}";

/// The marker's channels as Pango reports them (16-bit).
fn is_marker(color: pango::Color) -> bool {
    (color.red(), color.green(), color.blue()) == (0x3535, 0x8484, 0xe4e4)
}

/// The capsule's fill: the accent, tinted the way the old box was —
/// readable on both themes — and paler for a deleted issue.
fn pill_fill(deleted: bool) -> gdk::RGBA {
    gdk::RGBA::new(
        0x35 as f32 / 255.0,
        0x84 as f32 / 255.0,
        0xe4 as f32 / 255.0,
        if deleted { 0.10 } else { 0.22 },
    )
}

/// Where the pills in `label` are, in the label's own coordinates: one
/// rectangle per line a pill's span touches, each the line's logical
/// height and the span's width on that line. Read off the layout the
/// label actually drew, so it is right at any width and after any wrap.
/// `true` marks a deleted issue's pill (its span is struck through).
fn pill_rects(label: &gtk::Label) -> Vec<(graphene::Rect, bool)> {
    let layout = label.layout();
    let Some(attrs) = layout.attributes() else {
        return Vec::new();
    };
    let attributes = attrs.attributes();
    let struck: Vec<(u32, u32)> = attributes
        .iter()
        .filter(|attr| attr.type_() == pango::AttrType::Strikethrough)
        .map(|attr| (attr.start_index(), attr.end_index()))
        .collect();
    let (dx, dy) = label.layout_offsets();
    let mut rects = Vec::new();
    for attr in &attributes {
        if attr.type_() != pango::AttrType::Background {
            continue;
        }
        let Some(color) = attr.downcast_ref::<pango::AttrColor>() else {
            continue;
        };
        if !is_marker(color.color()) {
            continue;
        }
        let (start, end) = (attr.start_index() as i32, attr.end_index() as i32);
        let deleted = struck
            .iter()
            .any(|(s, e)| *s as i32 <= start && end <= *e as i32);
        let mut lines = layout.iter();
        loop {
            if let Some(line) = lines.line_readonly() {
                let line_start = line.start_index();
                let line_end = line_start + line.length();
                let from = start.max(line_start);
                let to = end.min(line_end);
                if from < to {
                    let (_, logical) = lines.line_extents();
                    let x0 = logical.x() + line.index_to_x(from, false);
                    let x1 = if to >= line_end {
                        logical.x() + logical.width()
                    } else {
                        logical.x() + line.index_to_x(to, false)
                    };
                    let (left, right) = (x0.min(x1), x0.max(x1));
                    let scale = pango::SCALE as f32;
                    rects.push((
                        graphene::Rect::new(
                            dx as f32 + left as f32 / scale,
                            dy as f32 + logical.y() as f32 / scale,
                            (right - left) as f32 / scale,
                            logical.height() as f32 / scale,
                        ),
                        deleted,
                    ));
                }
            }
            if !lines.next_line() {
                break;
            }
        }
    }
    rects
}

mod imp {
    use super::*;

    /// See [`super::PillText`].
    #[derive(Default)]
    pub struct PillText;

    #[glib::object_subclass]
    impl ObjectSubclass for PillText {
        const NAME: &'static str = "TastePillText";
        type Type = super::PillText;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            // One child, the label, at the widget's full size: the label
            // measures, and this widget is exactly as big as it is.
            klass.set_layout_manager_type::<gtk::BinLayout>();
        }
    }

    impl ObjectImpl for PillText {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for PillText {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            if let Some(label) = widget.first_child().and_downcast::<gtk::Label>() {
                if let Some(bounds) = label.compute_bounds(widget.upcast_ref::<gtk::Widget>()) {
                    for (rect, deleted) in pill_rects(&label) {
                        let rect = rect.offset_r(bounds.x(), bounds.y());
                        let capsule = gsk::RoundedRect::from_rect(rect, rect.height() / 2.0);
                        snapshot.push_rounded_clip(&capsule);
                        snapshot.append_color(&pill_fill(deleted), &rect);
                        snapshot.pop();
                    }
                }
            }
            // The label, over the capsules.
            self.parent_snapshot(snapshot);
        }
    }
}

glib::wrapper! {
    /// A label's frame that paints its issue pills.
    ///
    /// `GtkLabel` is final, so the capsules cannot be drawn by the label
    /// itself; this holds the label as its one child, measures as it
    /// does, and paints a capsule under each pill span — found in the
    /// label's own layout by the marker background [`pill_markup`] sets —
    /// before the label draws its text over them. Everything else stays
    /// the label's: selection, the link click, the tooltip. The label is
    /// [`PillText::label`], for the callers that wire those.
    pub struct PillText(ObjectSubclass<imp::PillText>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl PillText {
    /// Frame `label`, which must not have a parent yet.
    pub fn wrap(label: &gtk::Label) -> Self {
        let this: Self = glib::Object::new();
        label.set_parent(&this);
        this
    }
}

/// `text`, escaped for markup, with every issue reference replaced by its
/// pill. The one entry point `markdown_view` needs for a run of plain
/// text.
pub fn pillify(text: &str, index: &IssueIndex) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    let mut last = 0;
    for range in find_refs(text) {
        out.push_str(&glib::markup_escape_text(&text[last..range.start]));
        let id = &text[range.clone()];
        out.push_str(&pill_markup(id, &index.lookup(id)));
        last = range.end;
    }
    out.push_str(&glib::markup_escape_text(&text[last..]));
    out
}

/// `text` with every " · {title}" that follows an id whose pill will carry
/// that same title removed. An act's headline spells the title out
/// ("Filed i-0042 · The flicker"), which read twice once the id became a
/// pill saying the same thing. A deleted issue's pill has no title, so its
/// sentence keeps it.
pub fn without_repeated_titles(text: &str, index: &IssueIndex) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for range in find_refs(text) {
        if range.start < last {
            continue;
        }
        out.push_str(&text[last..range.end]);
        last = range.end;
        let id = &text[range];
        if let Some(facts) = index.get(id) {
            let rest = &text[last..];
            for sep in [" · ", " \u{2014} ", ": "] {
                if let Some(after) = rest.strip_prefix(sep) {
                    let title = facts.title.trim();
                    if !title.is_empty() && after.starts_with(title) {
                        last += sep.len() + title.len();
                    }
                    break;
                }
            }
        }
    }
    out.push_str(&text[last..]);
    out
}

/// The tooltip for one pill: the whole title, the state, and who is on
/// it — or that the issue is gone, and that a click can do no more than
/// say so.
pub fn tooltip(id: &str, lookup: &Lookup) -> String {
    let Some(facts) = lookup.facts() else {
        return format!(
            "{}\nNothing to select: the backlog has no row for it.",
            missing_note(id)
        );
    };
    let mut lines = vec![format!("{} — {}", facts.id, facts.title.trim())];
    let mut who = vec![facts.state.clone()];
    if let Some(by) = &facts.started_by {
        who.push(format!("started by {by}"));
    }
    if let Some(agent) = &facts.agent {
        who.push(agent.clone());
    }
    lines.push(who.join(" · "));
    lines.push("Click to select it in the backlog".to_string());
    lines.join("\n")
}

/// Give a label's pills their tooltips: GTK tells us which link the pointer
/// is over (`current_uri`), and the tooltip answers for that one.
pub fn install_tooltips(label: &gtk::Label, index: SharedIssueIndex) {
    label.set_has_tooltip(true);
    label.connect_query_tooltip(move |label, _x, _y, _keyboard, tip| {
        let Some(uri) = label.current_uri() else {
            return false;
        };
        let Some(id) = uri.strip_prefix(SCHEME) else {
            return false;
        };
        tip.set_text(Some(&tooltip(id, &index.lookup(id))));
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_the_sentence_repeats_after_the_id_is_left_to_the_pill() {
        let index = IssueIndex::default();
        index.facts.borrow_mut().insert(
            "i-0042".into(),
            facts("i-0042", "The composer loses a half-typed follow-up"),
        );
        assert_eq!(
            without_repeated_titles(
                "Filed i-0042 · The composer loses a half-typed follow-up",
                &index
            ),
            "Filed i-0042"
        );
        // Another sentence after the id is not a title, and stays.
        assert_eq!(
            without_repeated_titles("Completed i-0042 · merged", &index),
            "Completed i-0042 · merged"
        );
        // A deleted issue's pill carries no title, so the sentence keeps it.
        assert_eq!(
            without_repeated_titles("Filed i-0099 · Gone now", &index),
            "Filed i-0099 · Gone now"
        );
    }

    fn facts(id: &str, title: &str) -> IssueFacts {
        IssueFacts {
            id: id.into(),
            title: title.into(),
            state: "active".into(),
            started_by: Some("primary".into()),
            agent: Some("claude-code".into()),
        }
    }

    /// Ids stand alone — six of `[a-z0-9]`, or the sequence era's digits —
    /// and not part of a longer word, path, or token.
    #[test]
    fn refs_are_ids_standing_alone() {
        let text = "filed i-k7m2qx and i-0007, see i-12345; not i-12, not i-mean, not x-i-0001, \
                    not agents/i-0003, not i-k7m2qxz";
        let found: Vec<&str> = find_refs(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(found, vec!["i-k7m2qx", "i-0007", "i-12345"]);
        assert!(find_refs("i-0042").len() == 1);
        assert!(find_refs("(i-0042)").len() == 1);
        assert!(find_refs("i-0042.").len() == 1);
        assert!(find_refs("i-0042x").is_empty());
    }

    /// A long title is cut with an ellipsis, and nothing in a pill can
    /// break: the spaces and hyphens are the no-break kind.
    #[test]
    fn a_pill_is_one_unbreakable_word_with_a_cut_title() {
        let long = "The Dirty filter jumps back to the top every time git status refreshes";
        let shown = pill_title(long);
        assert!(shown.ends_with('…'));
        assert!(shown.chars().count() <= MAX_TITLE_CHARS + 1);
        assert!(!shown.contains(' '), "{shown:?}");
        assert!(shown.contains('\u{00A0}'));
        assert_eq!(pill_title("short"), "short");
        assert_eq!(pill_title("a-b c"), "a\u{2011}b\u{00A0}c");
    }

    /// The markup is a link on the pill's scheme around a tinted span; the
    /// text inside is escaped and unbreakable, an unknown id is a pill of
    /// its id alone, and a deleted one is that pill struck through and
    /// dimmer — still a link, so the click can say what happened.
    #[test]
    fn the_markup_is_a_link_around_a_tinted_span() {
        let known = pill_markup(
            "i-0042",
            &Lookup::Known(facts("i-0042", "Fix <the> flicker")),
        );
        assert!(known.starts_with("<a href=\"taste-issue:i-0042\">"));
        assert!(known.contains("background=\"#3584e4\" background_alpha=\"1\""));
        assert!(!known.contains("strikethrough"));
        assert!(known.contains("underline=\"none\""));
        assert!(!known.contains("strikethrough"));
        assert!(known.contains("Fix\u{00A0}&lt;the&gt;\u{00A0}flicker"));
        assert!(known.contains("i\u{2011}0042\u{00A0}·\u{00A0}"));
        let deleted = pill_markup("i-0042", &Lookup::Deleted);
        assert!(deleted.starts_with("<a href=\"taste-issue:i-0042\">"));
        assert!(deleted.contains("strikethrough=\"true\""));
        assert!(deleted.contains("strikethrough=\"true\""));
        assert!(deleted.contains(">\u{00A0}\u{202F}i\u{2011}0042\u{00A0}\u{202F}<"));
    }

    /// An absent id is a deleted issue: the index says so, and so does the
    /// sentence the backlog's toast and the pill's tooltip share.
    #[test]
    fn an_absent_id_is_read_as_a_deleted_issue() {
        let index = IssueIndex::default();
        assert_eq!(index.lookup("i-0007"), Lookup::Deleted);
        assert_eq!(
            missing_note("i-0007"),
            "i-0007 is no longer on the backlog — deleted, as far as anyone can tell"
        );
    }

    #[test]
    fn pillify_escapes_the_prose_and_pills_the_refs() {
        let index = IssueIndex::default();
        assert_eq!(pillify("a < b & c", &index), "a &lt; b &amp; c");
        let out = pillify("see i-0042 & i-0043.", &index);
        assert!(out.starts_with("see <a href=\"taste-issue:i-0042\">"));
        assert!(out.contains("</a> &amp; <a href=\"taste-issue:i-0043\">"));
        assert!(out.ends_with("</a>."));
    }

    /// The sentence a coordinator actually wrote, with the id in bold and
    /// an en dash after it: the parser must hand the scanner "i-0010" as
    /// one run, and the scanner must take it.
    #[test]
    fn an_id_inside_bold_is_still_a_reference() {
        use pulldown_cmark::{Event, Parser};
        let prose = "The backlog has one queued issue: **i-0010** – “Publish is one word”. \
                     That is the next item to start.";
        let runs: Vec<String> = Parser::new(prose)
            .filter_map(|event| match event {
                Event::Text(text) => Some(text.to_string()),
                _ => None,
            })
            .collect();
        let with_ref: Vec<&String> = runs
            .iter()
            .filter(|run| !find_refs(run).is_empty())
            .collect();
        assert_eq!(with_ref, vec![&"i-0010".to_string()], "{runs:?}");
        let index = IssueIndex::default();
        assert!(pillify("i-0010", &index).starts_with("<a href=\"taste-issue:i-0010\">"));
    }

    #[test]
    fn the_tooltip_carries_the_whole_title_and_who_is_on_it() {
        let tip = tooltip(
            "i-0042",
            &Lookup::Known(facts("i-0042", "A long title that the pill cut")),
        );
        assert!(tip.starts_with("i-0042 — A long title that the pill cut\n"));
        assert!(tip.contains("active · started by primary · claude-code"));
        assert!(tip.ends_with("Click to select it in the backlog"));
        let gone = tooltip("i-0007", &Lookup::Deleted);
        assert!(gone.starts_with("i-0007 is no longer on the backlog"));
        assert!(gone.contains("Nothing to select"));
    }
}

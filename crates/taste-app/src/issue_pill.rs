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
//! markup cannot do is round the corners: the pill is a tinted box with
//! a hair of padding on each side, in the same tint on both themes.
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

/// The href scheme a pill carries. `markdown_view` routes it to the
/// caller's link handler, and the chat turns it into
/// `Event::RevealIssueRequested`.
pub const SCHEME: &str = "taste-issue:";

/// The longest title a pill shows before it is cut with an ellipsis.
const MAX_TITLE_CHARS: usize = 36;

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
    // A thin space each side stands in for padding, which markup has no
    // word for. The tint is the accent at low alpha, readable on both
    // themes, and the link's underline is turned off so the pill is the
    // affordance rather than a hyperlink wearing a box.
    format!(
        "<a href=\"{SCHEME}{}\"><span background=\"#3584e4\" background_alpha=\"{}\" \
         underline=\"none\"{}>\u{2009}{}\u{2009}</span></a>",
        glib::markup_escape_text(id),
        if deleted { "10%" } else { "22%" },
        if deleted {
            " strikethrough=\"true\" alpha=\"60%\""
        } else {
            ""
        },
        glib::markup_escape_text(&text)
    )
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
        assert!(known.contains("background_alpha=\"22%\""));
        assert!(known.contains("underline=\"none\""));
        assert!(!known.contains("strikethrough"));
        assert!(known.contains("Fix\u{00A0}&lt;the&gt;\u{00A0}flicker"));
        assert!(known.contains("i\u{2011}0042\u{00A0}·\u{00A0}"));
        let deleted = pill_markup("i-0042", &Lookup::Deleted);
        assert!(deleted.starts_with("<a href=\"taste-issue:i-0042\">"));
        assert!(deleted.contains("strikethrough=\"true\""));
        assert!(deleted.contains("background_alpha=\"10%\""));
        assert!(deleted.contains(">\u{2009}i\u{2011}0042\u{2009}<"));
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

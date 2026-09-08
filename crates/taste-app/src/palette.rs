//! Every colour the app states itself, in one place, each with why.
//!
//! Almost everything on screen is coloured by libadwaita and says so in
//! the stylesheet by name — `@accent_color` for the thing that is chosen or
//! live, `@success_color` / `@warning_color` / `@error_color` for the
//! traffic light an environment's state is, a red washed into the window
//! for "the panes are aimed away from somebody else's checkout" — burgundy
//! on dark, a very light red on light. Those are the theme's
//! and are not restated here. What IS here is the handful of colours the
//! theme does not supply and Rust code has to hand to a widget as a value:
//! the terminal's palette, the search's hue, and the highlight a hit wears,
//! the greys and washes a diff is drawn in. One place, so a colour is
//! picked from the set that already exists rather than invented at the call site (David,
//! 2026-09-06: "pick from the existing palette, which we should centralize
//! and annotate").

use gtk::prelude::*;

/// GNOME Console's ANSI palette, for a terminal's sixteen colours and
/// everything drawn to match one. Legible on both the light and the dark
/// terminal background below — these are the terminal's own choices, not
/// the theme's. Index 0 is the black, 3 the yellow, 11 the bright yellow.
pub const ANSI_TERMINAL: [&str; 16] = [
    "#241f31", "#c01c28", "#2ec27e", "#f5c211", "#1e78e4", "#9841bb", "#0ab9dc", "#c0bfbc",
    "#5e5c64", "#ed333b", "#57e389", "#f8e45c", "#51a1ff", "#c061cb", "#4fd2fd", "#f6f5f4",
];

/// GNOME Console's ANSI palette as *text* colours on the theme's own
/// background — a tool card's output in the chat, coloured the way the same
/// bytes are coloured in the console tab, but readable on a card rather
/// than on the terminal's black or white.
pub const ANSI_TEXT: [&str; 16] = [
    "#171421", "#c01c28", "#26a269", "#a2734c", "#12488b", "#a347ba", "#2aa1b3", "#d0cfcc",
    "#5e5c64", "#f66151", "#33d17a", "#e9ad0c", "#2a7bde", "#c061cb", "#33c7de", "#ffffff",
];

/// A terminal's foreground and background in the dark scheme.
pub const TERMINAL_DARK: (&str, &str) = ("#d0cfcc", "#1d1b20");
/// ...and in the light one.
pub const TERMINAL_LIGHT: (&str, &str) = ("#171421", "#ffffff");

/// What the panels that are neither the terminal nor the editor set their
/// text in, under the dark scheme: the terminal's own foreground.
///
/// The theme's dark foreground is pure white, and white on the window's
/// grey is starker than anything else in the window — the console beside
/// it has been drawing its text in this off-white all along, because it is
/// GNOME Console's, and the editor's source view has a scheme of its own
/// (David, 2026-09-08: "make the colors of the non-terminal, non-editor
/// panels leverage the same color palettes so the text is less stark. In
/// VS Code, the files and chat are more off-white text on dark gray than
/// white-on-gray"). So the file tree, the chat, and Dispatch borrow it,
/// and the whole window is one family.
///
/// Dark only. The light scheme's foreground is already a soft near-black
/// rather than pure black, so there is nothing to take the edge off.
pub const PANEL_FG_DARK: &str = TERMINAL_DARK.0;

/// What the file listing's names are drawn in under the dark scheme: the
/// colour the editor draws a plain identifier in, measured off the frame
/// (`#c0bfbc`, which the terminal palette already carries as its base
/// white).
///
/// A step softer than the panels' own foreground — 8.6:1 against the
/// tree's ground where `PANEL_FG_DARK` is 10.2:1 — because a file listing
/// is a long column of names read by shape, and at full strength it shouts
/// beside the editor it names (David, 2026-09-08: "the files listing
/// should use a smaller typeface, slightly lower contrast (maybe same
/// color as normal characters in the editor panel), and wider leading").
pub const TREE_FG_DARK: &str = ANSI_TERMINAL[7];

/// The search's own hue: **purple** (`#9141ac`, libadwaita's purple accent
/// and the GNOME palette's `purple_3`), which is now the search's alone —
/// blue is the accent and reads as "chosen", green and amber are two
/// thirds of the traffic light an environment's state is, and the red
/// family took over "you are looking at somebody else's checkout"
/// (`main.rs::theme_conditional_css`) when the search took purple (David,
/// 2026-09-08: "drop the teal theme for search. Instead, use the purple
/// one that you've been using for the read only environments").
///
/// It was teal. Nothing about the shades below changed in *meaning* — but
/// every NUMBER did, because purple is far darker than teal and an alpha
/// or a mix percentage is only ever a way of asking for a lightness step.
/// Each one below was re-measured against the step the teal it replaces
/// produced, which is the same method the away wash documents.
///
/// Everything the search draws is this one hue in a few shades, so
/// a count, a listing and a lit hit are seen to be one thing, and nothing
/// else has to be dimmed for them to stand out (David, 2026-09-06: "use
/// color to emphasize the results listings, counts, and highlights … the
/// same color theme for all of them, with a few shade variants"). The
/// shades, quietest to loudest, and who wears them:
///   · the WASH — the hue mixed into the window background, under a
///     results listing (`.results-panel`; `main.rs::search_css`), and,
///     stronger, as the search box's own fill — the box wears the hue
///     with no query typed, because the colour is seen to flow from it;
///   · the TINT — the hue at a fifth or a quarter, behind a count badge
///     and behind a transcript row a hit was activated on (`.hit-badge`,
///     `.search-hit`);
///   · the INK — the hue as text and glyphs on the window background: a
///     badge's count, the progress rules (`search_ink`);
///   · the FILL — the hue solid under a contrasting foreground: the one
///     hit that is selected, in a buffer, a terminal or a log, and a tab's
///     count badge (`hit_background` / `hit_foreground`).
pub const SEARCH_FILL: &str = "#9141ac";

/// The search hue as ink on the window background: the palette's lightest
/// purple on the dark scheme and its darkest on the light one, which is
/// what "this hue, legible as text here" comes to (measured: 6.6:1 on the
/// dark window, 6.5:1 on the light).
pub fn search_ink(dark: bool) -> &'static str {
    if dark {
        "#dc8add"
    } else {
        "#813d9c"
    }
}

/// What a search hit wears when it is the one selected, in a buffer, a
/// terminal or a log — and what a tab's count badge is drawn in: the
/// search hue solid, paired the way the terminal palette pairs its brights
/// and bases: the light purple under the ANSI black on a dark scheme
/// (6.6:1), the base purple under white on a light one (5.9:1).
pub fn hit_background(dark: bool) -> &'static str {
    if dark {
        "#dc8add"
    } else {
        SEARCH_FILL
    }
}
pub fn hit_foreground(dark: bool) -> &'static str {
    if dark {
        ANSI_TERMINAL[0]
    } else {
        "#ffffff"
    }
}

/// The grey a diff's meta lines and a ghost suggestion are drawn in: quiet
/// beside code in either scheme.
pub const MUTED: &str = "#888888";

/// The wash behind an added line in a diff: the ANSI green at 18%, so the
/// syntax colours read through it.
pub const DIFF_ADDED_WASH: &str = "rgba(46,194,126,0.18)";
/// The wash behind a removed line: the ANSI red at 18%.
pub const DIFF_REMOVED_WASH: &str = "rgba(192,28,40,0.18)";
/// The stronger wash on the words that changed within a changed line, over
/// the line's own: what VS Code's diff draws, so the eye lands on the word
/// rather than reading two lines to find it.
///
/// **Scheme-aware, and that is the whole point.** A wash over a LIGHT
/// ground darkens it, and text stays readable however strong it gets; over
/// a dark ground it lightens, and it climbs toward the text. At 0.42 on
/// the dark source view the marked words sat on rgb(34,97,69) — 4.6:1
/// against the body's off-white, and 1.9:1 against a comment's grey, which
/// is not a highlight but an erasure (David, 2026-09-08, of a diff with
/// comment lines in it: "this is too low-contrast"). At 0.25 the same
/// words sit on rgb(33,69,55): 6.7:1, near the 7.8:1 the line's own wash
/// keeps, so the word is marked by SATURATION rather than by lightness and
/// the syntax colours still read through — which is what the line wash
/// promises two paragraphs up.
pub fn diff_added_strong(dark: bool) -> &'static str {
    if dark {
        "rgba(46,194,126,0.25)"
    } else {
        "rgba(46,194,126,0.42)"
    }
}

pub fn diff_removed_strong(dark: bool) -> &'static str {
    if dark {
        "rgba(192,28,40,0.30)"
    } else {
        "rgba(192,28,40,0.42)"
    }
}
/// The wash behind the blank a side-by-side diff shows opposite a line the
/// other side has and it does not: a grey, so it reads as "nothing here"
/// rather than as an empty line of the file.
pub const DIFF_PAD_WASH: &str = "rgba(128,128,128,0.10)";

/// A palette entry as GDK wants it.
pub fn rgba(color: &str) -> gtk::gdk::RGBA {
    gtk::gdk::RGBA::parse(color).expect("a palette colour parses")
}

const HIT_TAG: &str = "taste-search-hit";

/// Put the hit colours on one range of a buffer, and nowhere else in it:
/// the previous hit's tag comes off first. The tag is made on first use
/// and recoloured every time, so a scheme flip between two hits is drawn
/// right. Added after any syntax tags, so it draws over them.
pub fn highlight_range(buffer: &gtk::TextBuffer, start: &gtk::TextIter, end: &gtk::TextIter) {
    let table = buffer.tag_table();
    let tag = match table.lookup(HIT_TAG) {
        Some(tag) => tag,
        None => {
            let tag = gtk::TextTag::new(Some(HIT_TAG));
            table.add(&tag);
            tag
        }
    };
    let dark = adw::StyleManager::default().is_dark();
    tag.set_background(Some(hit_background(dark)));
    tag.set_foreground(Some(hit_foreground(dark)));
    buffer.remove_tag(&tag, &buffer.start_iter(), &buffer.end_iter());
    buffer.apply_tag(&tag, start, end);
}

/// Take the hit colours off a buffer entirely — the query cleared.
pub fn clear_highlight(buffer: &gtk::TextBuffer) {
    if let Some(tag) = buffer.tag_table().lookup(HIT_TAG) {
        buffer.remove_tag(&tag, &buffer.start_iter(), &buffer.end_iter());
    }
}

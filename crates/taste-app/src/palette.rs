//! Every colour the app states itself, in one place, each with why.
//!
//! Almost everything on screen is coloured by libadwaita and says so in
//! the stylesheet by name — `@accent_color` for the thing that is chosen or
//! live, `@success_color` / `@warning_color` / `@error_color` for the
//! traffic light an environment's state is, `@purple_3` washed into the
//! window for "the panes are aimed away from home". Those are the theme's
//! and are not restated here. What IS here is the handful of colours the
//! theme does not supply and Rust code has to hand to a widget as a value:
//! the terminal's palette, the highlight a search hit wears, the greys and
//! washes a diff is drawn in. One place, so a colour is picked from the
//! set that already exists rather than invented at the call site (David,
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

/// What a search hit wears when it is the one selected, in a buffer or a
/// terminal: the ANSI yellow of the terminal palette (bright on dark, base
/// on light) under the ANSI black. The highest-contrast pair the palette
/// has that does not already mean something — red, green and amber are the
/// traffic light, blue is the accent — and the colour every editor's find
/// has taught the eye to read as "here".
pub fn hit_background(dark: bool) -> &'static str {
    if dark {
        ANSI_TERMINAL[11]
    } else {
        ANSI_TERMINAL[3]
    }
}
pub const HIT_FOREGROUND: &str = ANSI_TERMINAL[0];

/// The match-count badge drawn as a picture — for a tab, whose icon slot
/// is the only place a tab can carry one (`search::badge_texture`). The
/// stylesheet's badge is the theme's accent; this is the terminal
/// palette's blue, bright on dark, under white — the same idea in the one
/// place CSS cannot reach.
pub fn badge_background(dark: bool) -> &'static str {
    if dark {
        ANSI_TERMINAL[12]
    } else {
        ANSI_TERMINAL[4]
    }
}
pub const BADGE_FOREGROUND: &str = ANSI_TERMINAL[15];

/// The grey a diff's meta lines and a ghost suggestion are drawn in: quiet
/// beside code in either scheme.
pub const MUTED: &str = "#888888";

/// The wash behind an added line in a diff: the ANSI green at 18%, so the
/// syntax colours read through it.
pub const DIFF_ADDED_WASH: &str = "rgba(46,194,126,0.18)";
/// The wash behind a removed line: the ANSI red at 18%.
pub const DIFF_REMOVED_WASH: &str = "rgba(192,28,40,0.18)";

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
    tag.set_foreground(Some(HIT_FOREGROUND));
    buffer.remove_tag(&tag, &buffer.start_iter(), &buffer.end_iter());
    buffer.apply_tag(&tag, start, end);
}

/// Take the hit colours off a buffer entirely — the query cleared.
pub fn clear_highlight(buffer: &gtk::TextBuffer) {
    if let Some(tag) = buffer.tag_table().lookup(HIT_TAG) {
        buffer.remove_tag(&tag, &buffer.start_iter(), &buffer.end_iter());
    }
}

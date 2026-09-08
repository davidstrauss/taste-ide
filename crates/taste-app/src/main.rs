//! taste-ide: opinionated AI-supported IDE.
//!
//! One binary, two modes:
//! - default: the libadwaita application
//! - `--mcp-bridge <socket>`: stdio↔socket bridge, registered as an MCP
//!   stdio server in every agent session so agents can reach the IDE.

// Hook types in this crate are spelled out where they are stored, so the
// reader sees the shape without chasing an alias; clippy would rather
// have the alias. The judgement is ours.
#![allow(clippy::type_complexity)]

mod backlog;
mod chat;
mod chat_column;
mod chatdoc;
mod chats;
mod command_completion;
mod compose;
mod composer;
mod console;
mod controller;
mod coordinator;
mod devcontainer_ui;
mod editor;
mod env_channel;
mod environments;
mod filetree;
mod fleet;
mod gadget;
mod gauge;
mod hover;
mod inset;
mod intervention;
mod logview;
#[allow(dead_code)] // kept for the style_ranges perf harness
mod markdown;
mod markdown_view;
mod notify;
mod orchestration;
mod pages_menu;
mod palette;
mod portview;
mod rest;
mod results;
mod reveal;
mod runtime;
mod search;
mod semantic;
mod sparkline;
mod tabfamily;
mod textline;
mod ui_probe;
mod voice;
mod window;

use adw::prelude::*;
use gtk::glib;

/// GNOME convention: development builds run under a .Devel identity with
/// a badged icon, so the shell can tell the IDE-under-test apart from the
/// IDE doing the developing.
pub const APP_ID: &str = if cfg!(debug_assertions) {
    "net.davidstrauss.Taste.Devel"
} else {
    "net.davidstrauss.Taste"
};

/// The host URL-opener channel (dir + token), captured at startup and
/// stripped from the environment so no child — agents included — can
/// drive the host browser without the app's confirm flows.
static HOST_OPEN: std::sync::OnceLock<Option<(std::path::PathBuf, String)>> =
    std::sync::OnceLock::new();

pub(crate) fn host_open_channel() -> Option<(std::path::PathBuf, String)> {
    HOST_OPEN.get().cloned().flatten()
}

fn main() -> glib::ExitCode {
    let channel = match (
        std::env::var("TASTE_HOST_OPEN_DIR"),
        std::env::var("TASTE_HOST_OPEN_TOKEN"),
    ) {
        (Ok(dir), Ok(token)) => Some((std::path::PathBuf::from(dir), token)),
        _ => None,
    };
    let _ = HOST_OPEN.set(channel);
    std::env::remove_var("TASTE_HOST_OPEN_DIR");
    std::env::remove_var("TASTE_HOST_OPEN_TOKEN");

    // Self-hosting: nothing in here may hold a handle to a container
    // runtime. A runtime socket reachable from inside the IDE's container
    // is host root by another name — `run -v /:/host` needs no exploit —
    // and every child (the agent, terminals, the repo's own build and
    // tests) would inherit it. The bootstrap forwards no socket; stripping
    // the handles as well means a stray one in the launch environment
    // cannot quietly re-open that door. Outside a container these are the
    // user's own settings and are left alone.
    if std::path::Path::new("/run/.containerenv").exists()
        || std::path::Path::new("/.dockerenv").exists()
    {
        for handle in ["CONTAINER_HOST", "DOCKER_HOST", "CONTAINER_CONNECTION"] {
            std::env::remove_var(handle);
        }
    }

    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 && args[1] == "--mcp-bridge" {
        let socket = std::path::PathBuf::from(&args[2]);
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        return match rt.block_on(taste_mcp_bridge(&socket)) {
            Ok(()) => glib::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("mcp bridge failed: {e:#}");
                glib::ExitCode::FAILURE
            }
        };
    }

    // Logs go two places: stderr for the developer at a terminal, and the
    // taste_core::app_log ring buffer the MCP server serves as ide_app_log
    // — the agent's answer to "did GTK complain about what I just did".
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "taste=info,warn".into());
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer())
            .with(AppLogLayer)
            .init();
    }
    // GLib's structured log (GTK CSS parse errors, missing icons,
    // unparented-widget warnings) is mirrored the same way, then handed to
    // the default writer so stderr behaves exactly as before.
    glib::log_set_writer_func(|level, fields| {
        use glib::LogLevel;
        if matches!(
            level,
            LogLevel::Error | LogLevel::Critical | LogLevel::Warning | LogLevel::Message
        ) {
            let field = |key: &str| {
                fields
                    .iter()
                    .find(|f| f.key() == key)
                    .and_then(|f| f.value_str())
            };
            taste_core::app_log::push(
                &format!("{level:?}").to_uppercase(),
                field("GLIB_DOMAIN").unwrap_or("GLib"),
                field("MESSAGE").unwrap_or_default(),
            );
        }
        glib::log_writer_default(level, fields)
    });

    // The workspace folder comes from the command line (GNOME Files' "Open
    // With", `taste-ide <dir>`); with no argument, a folder chooser (whose
    // Recent list is the desktop's own) picks one.
    let root_arg: Option<std::path::PathBuf> = args
        .get(1)
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_dir())
        .and_then(|p| p.canonicalize().ok());

    // NON_UNIQUE: each `taste-ide <folder>` is its own process/window —
    // otherwise a second workspace would just re-activate the first.
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        // Icon: installed into hicolor normally; in-repo data/ covers dev runs.
        if let Some(display) = gtk::gdk::Display::default() {
            let dev_icons = std::path::Path::new("data/icons");
            if dev_icons.is_dir() {
                gtk::IconTheme::for_display(&display).add_search_path(dev_icons);
            }
        }
        gtk::Window::set_default_icon_name(APP_ID);
        // App-level styling: the chat prompt entry (transparent TextView in
        // an entry-shaped container, matching GNOME chat apps).
        // The composer wears the same treatment a selected tab gets, and
        // for the same reason: libadwaita styles `tabbar tab:selected` as a
        // 10% currentColor overlay, which resolves to a lighter grey on a
        // dark background and a darker one on a light background without
        // hard-coding either. Its border stays, transparent, purely to hold
        // the geometry still — :focus-within colours it in, so a border is
        // now a focus signal rather than decoration. Horizontal padding is
        // zero on purpose: the TextView's own margins are the single place
        // the composer's inset is stated.
        if let Some(display) = gtk::gdk::Display::default() {
            let css = gtk::CssProvider::new();
            // ---- One radius scale for the chat column. ----
            //
            // Which step a thing gets follows from WHAT IT IS, not from
            // which widget happened to build it:
            //
            //   · 12px — SURFACES. Anything that is a box holding content:
            //     the composer field, the transcript's bubbles, the
            //     permission card, the pinned prompt. 12px is Adwaita's own
            //     `.card`, which the bubbles already wear, so the composer
            //     joins them instead of arguing with them.
            //   · pill — ACTIONS and CHIPS. Free-standing, one-gesture
            //     objects: +, Stop, Send, a permission card's answers, an
            //     attachment chip. Small and discrete, and a pill is what
            //     says so.
            //   · 6px — NESTED inside a surface: a tool's terminal output,
            //     the command on a permission card. Deliberately smaller,
            //     because a concentric inner corner inside a 12px card with
            //     12px of padding is *not* a fourth opinion about roundness.
            //
            // Peers agree; only nesting changes the step. Anything added to
            // this region picks one of the three rather than a fourth.
            css.load_from_string(
                ".prompt-entry { \
                   background-color: color-mix(in srgb, currentColor 10%, \
                   transparent); \
                   border: 1px solid transparent; border-radius: 12px; \
                   padding: 0; min-height: 44px; }\n\
                 .prompt-entry textview, .prompt-entry textview > text { \
                   background: transparent; }\n\
                 .prompt-entry entry.flat-entry { background: transparent; \
                   border: none; box-shadow: none; outline: none; \
                   min-height: 32px; }\n\
                 /* The platform's focus ring, not a hand-drawn one: \
                    `outline` is what Adwaita draws focus with, so this \
                    tracks the theme's ring width, colour and corner \
                    radius — and follows a high-contrast or custom-accent \
                    setting that a hard-coded border cannot see. Inset by \
                    its own width so the ring lands inside the card \
                    instead of over the widget beside it. */\n\
                 .prompt-entry:focus-within { \
                   outline: 2px solid @accent_color; \
                   outline-offset: -2px; }\n\
                 /* The same treatment, at the NESTED step of the scale, \
                    for a composer whose fields sit INSIDE a card rather \
                    than being one: same wash, same platform focus ring, \
                    6px because 12px inside a 12px card is a second \
                    opinion about roundness. Two inputs asking for two \
                    halves of one thing are peers, and peers agree — the \
                    backlog composer's title read as the theme's entry \
                    while its body read as a slab, which is two widgets \
                    that happen to be adjacent rather than one form. */\n\
                 .composer-field { background-color: color-mix(in srgb, \
                   currentColor 10%, transparent); \
                   border: 1px solid transparent; border-radius: 6px; }\n\
                 .composer-field:focus-within { \
                   outline: 2px solid @accent_color; \
                   outline-offset: -2px; }\n\
                 /* The field is the surface; whatever draws text inside \
                    it brings none of its own. */\n\
                 entry.composer-field { border: none; box-shadow: none; }\n\
                 .composer-field > text, .composer-field textview, \
                 .composer-field textview > text { \
                   background: transparent; }\n\
                 /* A note too long for one line carries a disclosure to \
                    open it (chat.rs). Sized to the caption beside it, so \
                    an aside does not grow a button's worth of chrome. */\n\
                 button.note-disclose { min-width: 20px; min-height: 20px; \
                   padding: 0; }\n\
                 /* 12 at the sides: the console's column. The environment \
                    tab's header and log stand 12 in, and a terminal tab \
                    beside them is the same region — text that jumped four \
                    pixels between two tabs of one strip was a seam. */\n\
                 vte-terminal { padding: 4px 12px; }\n\
                 /* A review tab's comparison line. Quiet enough to be a \
                    label rather than a banner — it states a fact that is \
                    true for as long as the tab exists, and a banner's \
                    weight would claim something happened. */\n\
                 .review-bar { padding: 4px 10px; background-color: \
                   color-mix(in srgb, currentColor 5%, transparent); }\n\
                 .taste-banner { padding: 6px 12px; background-color: \
                   color-mix(in srgb, var(--banner-color) 30%, \
                   var(--window-bg-color)); }\n\
                 /* GtkSourceMap paints its slider BENEATH the text layer; \
                    this GSV build leaves the map's text background opaque, \
                    which hides the slider entirely (verified by pixel \
                    probe). Transparent text lets it show through. */\n\
                 textview.GtkSourceMap text { background: transparent; }\n\
                 textview.GtkSourceMap > slider { \
                   background-color: alpha(@accent_bg_color, 0.25); \
                   border-radius: 2px; }\n\
                 textview.GtkSourceMap > slider:hover { \
                   background-color: alpha(@accent_bg_color, 0.4); }\n\
                 .composer-action, .composer-action > button, \
                 button.composer-action, button.composer-action.circular { \
                   min-width: 26px; min-height: 26px; padding: 2px; \
                   margin: 0; }\n\
                 /* The ACTION step of the scale. A MenuButton paints its \
                    background on an inner `button` node, so the class \
                    has to reach both or the + stays square while its \
                    neighbours round. */\n\
                 .pill-action, .pill-action > button, \
                 .composer-action, .composer-action > button { \
                   border-radius: 9999px; }\n\
                 /* Find-in-project's index progress: a hairline along the \
                    entry's bottom edge, with no text of its own (the \
                    count goes in the placeholder — filetree.rs). A stock \
                    trough is tall enough to look like a second widget \
                    stacked in the entry; this is a rule that happens to \
                    move. */\n\
                 progressbar.index-bar > trough, \
                 progressbar.index-bar > trough > progress { \
                   min-height: 3px; border-radius: 9999px; }\n\
                 progressbar.index-bar > trough { \
                   background-color: transparent; }\n\
                 progressbar.index-bar > trough > progress { \
                   background-color: @accent_color; }\n\
                 /* The pinned prompt floats OVER the transcript, so it \
                    needs a surface of its own: Adwaita's .card colour is \
                    a translucent overlay and the scrolling text reads \
                    straight through it. Popover colours are the theme's \
                    opaque floating-surface tokens. */\n\
                 .pinned-prompt { background-color: @popover_bg_color; \
                   box-shadow: 0 1px 4px rgba(0, 0, 0, 0.35); }\n\
                 /* ...and the band it floats IN (chat.rs). The plate is \
                    the transcript's own background, so the row the float \
                    covers is HIDDEN rather than cut in half, and the plate \
                    is invisible as an object; the hem is that plate's \
                    bottom edge stated as a fade rather than as an edge, so \
                    a card scrolling up under the float dissolves into the \
                    background instead of being sliced. Opaque for the \
                    first third, or the plate would end in a line anyway \
                    and only the gradient's tail would be soft. \
                    `alpha(bg, 0)` rather than `transparent`, which is \
                    transparent BLACK and drags a grey bloom through the \
                    middle of the gradient on a light theme. */\n\
                 .pinned-plate { background-color: @window_bg_color; }\n\
                 .pinned-hem { background-image: linear-gradient(to bottom, \
                   @window_bg_color 0%, @window_bg_color 30%, \
                   alpha(@window_bg_color, 0) 100%); }\n\
                 /* A tool call's terminal output, set off from the prose \
                    around it. currentColor at a few percent rather than a \
                    fixed grey, so it darkens on light and lightens on dark \
                    without hard-coding either. */\n\
                 .terminal-output { background-color: color-mix(in srgb, \
                   currentColor 7%, transparent); border-radius: 6px; }\n\
                 .terminal-output textview, \
                 .terminal-output textview > text { \
                   background: transparent; }\n\
                 /* An agent's proposed edit. The NESTED step, same as the \
                    output above it: a GtkSourceView brings its own opaque \
                    background, so without this the one block in the \
                    transcript a reader is asked to JUDGE was also the one \
                    with square corners. */\n\
                 .diff-block { border-radius: 6px; }\n\
                 /* An attachment chip: a discrete object, so it gets a \
                    pill. Same currentColor wash as the composer, so the \
                    chips above the prompt and the prompt itself read as \
                    one surface. */\n\
                 button.attachment-chip { border-radius: 9999px; \
                   padding: 2px 8px; min-height: 24px; \
                   background-color: color-mix(in srgb, currentColor 8%, \
                   transparent); }\n\
                 /* The chip's stamp: a picture the size of the words \
                    beside it, with the chip's own corner. */\n\
                 image.attachment-stamp { border-radius: 3px; }\n\
                 button.attachment-chip:hover { \
                   background-color: color-mix(in srgb, currentColor 15%, \
                   transparent); }\n\
                 /* A tool card's header is a button, and Adwaita bolds \
                    button labels. A tool title is a fact, not a heading. */\n\
                 label.tool-title { font-weight: normal; }\n\
                 /* A coordinator's act (chat.rs::act_kind) — filing, \
                    starting, completing, declining, moving, prompting — is \
                    the step the user should see at a glance among the reads \
                    and shells: a typed glyph in the accent, a headline that \
                    keeps the weight a tool title gives up. */\n\
                 label.act-title { font-weight: 600; }\n\
                 /* The transcript is a timeline (chat.rs::append_step): \
                    the agent's steps on a rail of connected dots, Claude \
                    Code's shape. The rows carry no padding of their own so \
                    the rail's line runs unbroken from one step into the \
                    next. */\n\
                 list.transcript > row { padding: 0; margin: 0; \
                   min-height: 0; }\n\
                 /* The transcript reads at a chat's scale, not a document's \
                    (David, 2026-09-07, beside Claude Code's: the scale and \
                    spacing are still much better there): body text a step down \
                    from the window's, code a step under that. The pinned \
                    prompt floats outside the list and follows it. */\n\
                 list.transcript, .pinned-prompt { font-size: 0.92em; }\n\
                 list.transcript label.monospace, \
                 list.transcript textview.diff-side { font-size: 0.9em; }\n\
                 .rail-line { min-width: 1px; \
                   background-color: alpha(currentColor, 0.22); }\n\
                 .rail-dot { min-width: 7px; min-height: 7px; \
                   border-radius: 9999px; \
                   background-color: alpha(currentColor, 0.45); }\n\
                 /* The traffic light the environment rows already speak, \
                    for a call: finished, failed, waiting on you — and the \
                    accent while it runs (the spinner stands in for the dot \
                    then). Prose keeps the neutral dot. */\n\
                 .rail-dot.ok { background-color: @success_color; }\n\
                 .rail-dot.fail { background-color: @error_color; }\n\
                 .rail-dot.wait { background-color: @warning_color; }\n\
                 .rail-dot.live { background-color: @accent_color; }\n\
                 /* An aside — a note, a thought, the plan — is a hollow \
                    dot. */\n\
                 .rail-dot.note { background-color: transparent; \
                   border: 2px solid alpha(currentColor, 0.35); \
                   min-width: 3px; min-height: 3px; }\n\
                 /* THE BLUE. libadwaita's own selected row is the accent at a \
                    quarter, and that is what this window means by it: the \
                    item is open in the editor's strip — or would open there \
                    on a click. One colour, one meaning, wherever the item is \
                    listed (David, 2026-09-07: the blue is the convention for \
                    selectable, or opened in the editor panel area): the file \
                    tree's rows have it from the theme; the \
                    Ports and Logs rows sit in a .navigation-sidebar, whose \
                    selection the theme paints grey, so they take it here; a \
                    transcript step whose document is the tab in front takes \
                    it (chat.rs::highlight_document); and the tab in front \
                    takes it too, to close the loop from the other end \
                    (David: make the active editor tab that same blue to \
                    emphasize the link). */\n\
                 .section-list > row:selected, \
                 list.transcript > row.doc-open > .card, \
                 list.transcript > row.doc-open .step-content, \
                 tabbar.editor-strip tab:selected { background-color: \
                   color-mix(in srgb, var(--accent-bg-color) 25%, \
                   transparent); }\n\
                 .section-list > row:selected:hover, \
                 tabbar.editor-strip tab:selected:hover { background-color: \
                   color-mix(in srgb, var(--accent-bg-color) 32%, \
                   transparent); }\n\
                 /* On the item's own boundary — the prompt's card, the step's \
                    text column — never the row, whose edge with the rail is \
                    nobody's. */\n\
                 list.transcript > row.doc-open .step-content { border-radius: 8px; }\n\
                 /* The F1 reveal (reveal.rs): libadwaita's popover, arrow and \
                    all, in the accent so the map reads as a layer over the \
                    window rather than as more window. */\n\
                 .reveal-bubble { background-color: @accent_bg_color; \
                   color: @accent_fg_color; padding: 6px 12px; \
                   border-radius: 10px; font-weight: bold; \
                   box-shadow: 0 2px 8px rgba(0, 0, 0, 0.35); }\n\
                 .keycap { min-width: 10px; min-height: 20px; padding: 0 6px 2px 6px; \
                   margin: 0 1px; border-radius: 5px; font-size: 0.85em; \
                   font-weight: bold; color: @window_fg_color; \
                   background-color: @window_bg_color; \
                   box-shadow: inset 0 -2px @shade_color, \
                     0 0 0 1px alpha(@window_fg_color, 0.12); }\n\
                 .reveal-bubble .pad { border: 2px solid @accent_fg_color; \
                   border-radius: 999px; min-width: 14px; min-height: 16px; \
                   padding: 0 4px; font-size: 0.8em; margin: 0 1px; }\n\
                 .reveal-bubble .pad-wide { border-radius: 6px; }\n\
                 .reveal-effect { font-weight: normal; }\n\
                 .reveal-pointer { color: @accent_bg_color; font-size: 0.8em; \
                   margin-top: -3px; margin-bottom: -3px; }\n\
                 .pill-action.hold-hint { box-shadow: 0 0 0 2px @accent_bg_color; }\n\
                 /* The minutes left on the search's meaning button while the \
                    index builds (search.rs): a grey pill, the hit badge's \
                    shape without its hue — this is a wait, not a result. */\n\
                 .index-pill { border-radius: 9999px; padding: 0 5px; \
                   min-height: 14px; font-size: 0.75em; font-weight: bold; \
                   background-color: alpha(currentColor, 0.15); }\n\
                 /* A clipped prompt's box is the click that opens the whole \
                    prompt in the editor; it says so on hover. */\n\
                 .clipped-prompt:hover { background-color: color-mix(in srgb, \
                   currentColor 12%, var(--card-bg-color)); }\n\
                 /* A step's header is a button so the step opens on a \
                    click and from the keyboard; the button's chrome is not \
                    wanted, and its text has to stand on the step column. */\n\
                 button.step-toggle { padding: 0; min-height: 0; \
                   border-radius: 6px; }\n\
                 /* The N-more-lines line that opens the whole thing, and \
                    the prompt's opener: a caption that happens to be a \
                    button. Padding \
                    small enough that its text stands on the column above. */\n\
                 button.open-more { padding: 0 4px; min-height: 0; \
                   margin-left: -4px; font-size: 0.8182em; \
                   font-weight: normal; }\n\
                 button.open-whole { min-width: 20px; min-height: 20px; \
                   padding: 0; margin: 0; }\n\
                 /* IN / OUT tags and a hunk head, top-aligned to the first \
                    line of what they tag. */\n\
                 label.io-tag { margin-top: 3px; }\n\
                 label.hunk-head { margin-top: 2px; }\n\
                 /* A diff column's line numbers: the view beside them, its \
                    numbers dimmed by a tag; a touch quieter still. */\n\
                 textview.diff-gutter { opacity: 0.85; }\n\
                 .diff-added { color: @success_color; }\n\
                 .diff-removed { color: @error_color; }\n\
                 image.act-icon { color: @accent_color; }\n\
                 image.act-icon.success { color: @success_color; }\n\
                 image.act-icon.error { color: @error_color; }\n\
                 /* The floating jump (inset.rs): an OSD pill inside the \
                    scrolling area, on the edge it points at — the chat's \
                    jump to latest and to the open item, the backlog's back \
                    to top, a log's jump to latest. Sized for a caption and \
                    a 16px glyph. */\n\
                 button.inset-jump { min-height: 26px; padding: 0 10px 0 8px; }\n\
                 /* The permission card. `.card` gives it the theme's own \
                    surface and radius; the padding is the HIG's 12px step, \
                    and the accent wash over that surface is what separates \
                    a question waiting on the user from the tool cards it \
                    sits under. Mixed rather than fixed, so it lands as a \
                    tint on dark and on light alike. */\n\
                 .permission-card { padding: 12px; background-color: \
                   color-mix(in srgb, @accent_bg_color 8%, \
                   var(--card-bg-color)); }\n\
                 /* The glyph types the ask — terminal, pencil, trash — and \
                    the accent is what makes it read as the card's subject \
                    rather than decoration beside the title. */\n\
                 .permission-icon { color: @accent_color; margin-top: 1px; }\n\
                 /* The command itself, on the tool cards' own output wash. \
                    Padding inside the wash, so the box hugs one line of \
                    monospace instead of floating around it. */\n\
                 .permission-code { padding: 6px 8px; }\n\
                 /* The backlog (backlog.rs): a full-bleed band of rows at \
                    the bottom of the file-tree pane — the user's own \
                    checkout first, then the issues, the ones with an \
                    environment at the top. Full-bleed so its rows keep \
                    the pane's edges rather than sitting in it. */\n\
                 /* The list is a switcher, not a document: tighter than \
                    .navigation-sidebar's default so seven rows fit where \
                    a file tree also has to live. */\n\
                 .backlog-panel .backlog-list > row { min-height: 40px; \
                   padding: 0; margin: 0 4px; border-radius: 6px; }\n\
                 /* The Logs and Ports sections' rows (filetree.rs) share \
                    the backlog's column and wear its exact row geometry, \
                    so a dot in one list sits over a dot in the other. */\n\
                 .section-list > row { min-height: 40px; \
                   padding: 0; margin: 0 4px; border-radius: 6px; }\n\
                 /* The search box (search.rs) in the title bar: its rule of \
                    progress is the gauges' drawing, in the search hue \
                    (search_css below) because it is the search's. */\n\
                 levelbar.search-rule trough { min-height: 3px; \
                   border-radius: 2px; background: transparent; \
                   border: none; }\n\
                 levelbar.search-rule block { min-height: 3px; \
                   border-radius: 2px; border: none; }\n\
                 /* The filter views' checkboxes (filetree.rs, .change-list): \
                    12px where stock is 14, in the same 26px prefix box, so \
                    the centre the column shares does not move (David, \
                    2026-09-07: \"could they be a bit narrower?\"). */\n\
                 .change-list row checkbutton check { min-width: 12px; \
                   min-height: 12px; -gtk-icon-size: 12px; }\n\
                 /* A row the query did not match, kept for reachability \
                    or by the ghost toggle. */\n\
                 .search-dim { opacity: 0.45; }\n\
                 /* The match-count badge (search.rs::hit_badge): one pill \
                    for every flank row that has hits. Its colours are the \
                    search hue's, stated with the rest of that palette in \
                    search_css below. */\n\
                 .hit-badge { font-weight: bold; \
                   border-radius: 9999px; padding: 0 6px; min-height: 16px; }\n\
                 /* A results listing (results.rs) at the foot of a pane. */\n\
                 .results-panel .results-list > row { min-height: 26px; \
                   padding: 0; margin: 0 4px; border-radius: 6px; }\n\
                 /* The placeholder in an empty composer field. */\n\
                 .composer-placeholder { opacity: 0.55; }\n\
                 /* The microphone while it listens, and the level beside \
                    it: accent, because it is the one thing on the row \
                    that is happening right now. */\n\
                 button.composer-mic.recording { background-color: \
                   @accent_bg_color; color: @accent_fg_color; }\n\
                 levelbar.voice-level trough { min-height: 4px; \
                   border-radius: 2px; }\n\
                 levelbar.voice-level block { min-height: 4px; \
                   border-radius: 2px; background-color: @accent_color; \
                   border: none; }\n\
                 /* The header's + is an action among a list of work, \
                    and must not shout over it. */\n\
                 button.backlog-new { min-width: 20px; min-height: 20px; \
                   padding: 0; }\n\
                 /* Adwaita's spinner is sized for a dialog. Beside an \
                    8px status dot and a 14px sparkline it reads as the \
                    loudest thing on the row, which inverts the row's own \
                    hierarchy: a turn being in flight is the least \
                    actionable of the three facts. */\n\
                 .backlog-panel spinner, .env-work spinner { min-width: 12px; \
                   min-height: 12px; opacity: 0.7; }\n\
                 .env-dot { min-width: 8px; min-height: 8px; \
                   border-radius: 9999px; }\n\
                 /* Traffic lights (fleet.rs → Light). Green up, amber \
                    wanting the user, red for a fault — and grey for OFF: \
                    stopped or never configured is not a failure, and a \
                    fleet of finished work must not read as one. \
                    `unknown` is the absence of a status, drawn as a ring \
                    rather than a second grey dot, so it cannot be mistaken \
                    for off. */\n\
                 .env-dot.green { background-color: @success_color; }\n\
                 .env-dot.amber { background-color: @warning_color; }\n\
                 .env-dot.red { background-color: @error_color; }\n\
                 .env-dot.off { background-color: \
                   color-mix(in srgb, currentColor 40%, transparent); }\n\
                 .env-dot.unknown { background-color: transparent; \
                   box-shadow: inset 0 0 0 1px \
                   color-mix(in srgb, currentColor 35%, transparent); }\n\
                 /* Every circle on a row is 8px — the traffic light's \
                    size. Two badges that almost match read as a mistake, \
                    and colour already carries which is which. */\n\
                 .env-unpublished { min-width: 8px; min-height: 8px; \
                   border-radius: 9999px; \
                   background-color: @accent_color; }\n\
                 /* Waiting on the user: the chat's speech bubble \
                    (backlog.rs), in amber, the one hue this UI reserves \
                    for \"you are the blocker\". */\n\
                 .env-attention { color: @warning_color; }\n\
                 /* The usage gauge (gauge.rs): the panel header's \
                    subscription window and the chat header's context \
                    window, one drawing. A level bar at Adwaita's default \
                    height would be a slab beside a caption; at 4px it is \
                    a rule with a filled part, which is all it needs to \
                    be. The colour is the traffic light the rows already \
                    speak — green with room, amber past three fifths, red \
                    when nearly gone — stated here rather than left to the \
                    level bar's stock offsets, whose palette (green at \
                    FULL) says the opposite of running out. The class is \
                    set by gauge::set, so the thresholds live in one \
                    place. */\n\
                 levelbar.usage-gauge trough { min-height: 4px; \
                   border-radius: 2px; background-color: \
                   color-mix(in srgb, currentColor 12%, transparent); \
                   border: none; }\n\
                 levelbar.usage-gauge block { min-height: 4px; \
                   border-radius: 2px; border: none; }\n\
                 levelbar.usage-gauge block.empty { \
                   background-color: transparent; }\n\
                 levelbar.usage-gauge.ok block.filled { \
                   background-color: @success_color; }\n\
                 levelbar.usage-gauge.warn block.filled { \
                   background-color: @warning_color; }\n\
                 levelbar.usage-gauge.spent block.filled { \
                   background-color: @error_color; }\n\
                 /* An hour-old reading is still true about the past and \
                    nothing about now. Faded rather than hidden: the \
                    level that was last seen is worth keeping on screen, \
                    and the tooltip says how old it is. */\n\
                 levelbar.usage-gauge.stale { opacity: 0.45; }\n\
                 /* Flagged for review (fleet.rs → ReviewState). Neither \
                    of the row's existing marks would do: amber is \"you \
                    are the blocker\" and this environment is not blocked, \
                    and the accent dot already means \"work only this \
                    checkout has\". A rail on the leading edge marks the \
                    row without moving it, without competing with the \
                    selection, and without borrowing a hue that means \
                    something else here. Order stays stable — a row that \
                    jumped to the top when an agent finished would move \
                    the list under the pointer. */\n\
                 /* A background gradient rather than an inset shadow. An \
                    inset shadow follows the row's 6px radius all the way \
                    round, so a 2px rail on a 26px row curled in at both \
                    ends and read as a stray parenthesis beside the name. \
                    A background image is clipped by the same radius, but \
                    a 14px rail centred in a 26px row never reaches a \
                    corner to be bent by one — so it draws as the straight \
                    rule it is meant to be. */\n\
                 .backlog-panel .backlog-list > row.review-flagged { \
                   background-image: linear-gradient(@accent_color, \
                   @accent_color); \
                   background-size: 2px 14px; \
                   background-position: left center; \
                   background-repeat: no-repeat; }\n\
                 /* Settled: merged or rejected. The user has ruled, so \
                    the row is history — dimmed, and the glyph says which \
                    way it went. */\n\
                 .backlog-panel .backlog-list > row.review-settled label { \
                   opacity: 0.6; }\n\
                 .env-review { color: @accent_color; }\n\
                 /* A transcript row a search hit was activated on, lit for \
                    a moment so the eye lands (chat.rs); its tint is the \
                    search hue's (search_css below). */\n\
                 .search-hit { border-radius: 8px; }\n\
                 /* The sidebar style pads its lists 6px top and bottom, which \
                    put a section's first row 10px under its header where the \
                    files tree sits 5 (David, 2026-09-08: too much space below \
                    panel title headers, except the files one). The header's own \
                    margin is the gap. */\n\
                 list.section-list, list.backlog-list { padding-top: 0; padding-bottom: 0; }\n\
                 /* A row is reordered by dragging it or by its own menu, \
                    so it carries no action chrome at all — the flank's \
                    narrowest pane spends its width on titles. What is \
                    left is the drop indicator: a line in the accent \
                    colour on the edge the row would land against, drawn \
                    with box-shadow so it takes no space and cannot \
                    shift the list under a drag in flight. */\n\
                 .backlog-list > row.drop-above { \
                   box-shadow: inset 0 2px 0 0 @accent_color; }\n\
                 .backlog-list > row.drop-below { \
                   box-shadow: inset 0 -2px 0 0 @accent_color; }\n\
                 /* The row being carried stays visible in place, dimmed: \
                    a gap where it was would move every other row while \
                    the pointer is trying to aim between two of them. */\n\
                 .backlog-list > row.dragging { opacity: 0.35; }\n\
                 /* Asking to delete is not a state to be subtle about, \
                    and it is the row's own shape while it is asking. */\n\
                 .backlog-confirm button { min-width: 20px; \
                   min-height: 20px; padding: 0; }\n\
                 .backlog-composer { padding: 8px; }",
            );
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            theme_conditional_css(&display);
        }
        match &root_arg {
            Some(root) => open_workspace(app, root.clone()),
            None => {
                let hold = app.hold(); // keep the app alive while choosing
                let app = app.clone();
                gtk::FileDialog::builder()
                    .title("Open a project folder")
                    .build()
                    .select_folder(
                        gtk::Window::NONE,
                        gtk::gio::Cancellable::NONE,
                        move |result| {
                            let _hold = hold;
                            if let Ok(folder) = result {
                                if let Some(path) = folder.path() {
                                    open_workspace(&app, path);
                                }
                            }
                        },
                    );
            }
        }
    });
    app.run_with_args::<&str>(&[])
}

/// The rules whose VALUE has to differ between light and dark, in a
/// provider reloaded whenever the style manager flips.
///
/// GTK CSS has no media queries, so a mix stated once is a mix judged in
/// one theme and inherited by the other. `.env-panel.away` was exactly
/// that: 17% of `@purple_3` is a wash on a dark window and a lilac stripe
/// on a light one, because the two backgrounds are not equidistant from
/// the tint — the same recipe moves the surface 4.6 L\* on dark and 9.5 on
/// light. What the eye reads as "how strong is this tint" is that
/// lightness step, not the recipe that produced it, so the two
/// percentages are picked to land on the same step (measured: +4.6 L\*
/// dark, −5.0 light) rather than to be the same number. That is the only
/// sense in which they are the same colour.
///
/// One provider for the whole display rather than a `.dark`/`.light` class
/// on a widget: nothing about the answer is per-window, and a class has to
/// be put on every window that ever exists and taken off again.
///
/// The search's palette rides in the same provider (`search_css`): its
/// ink is a different shade per scheme, and its values are `palette.rs`
/// constants the literal sheet above cannot name.
fn theme_conditional_css(display: &gtk::gdk::Display) {
    // Not home: this panel is another environment's checkout and read-only.
    // A tint the corner of an eye can catch, mixed into the window
    // background in both themes so the theme's own foreground stays
    // legible on it.
    //
    // The RED family, since the search took purple (David, 2026-09-08:
    // "for read only environments, let's go with something like burgundy
    // for dark mode and a very light red for light mode"). A different
    // member per scheme, which is what this provider is for: dark wants a
    // deep red to read as burgundy, and light wants a bright one thinned
    // to almost nothing. One colour at two percentages gave a burgundy and
    // a muddy warm grey.
    //
    // The percentages are measured, not chosen: the step the purple used
    // to take was +4.6 L* on dark and −5.0 on light, and these land on it
    // (`red_5` at 27% → +4.6; `red_3` at 8% → −5.0). What the eye reads as
    // "how strong is this tint" is the lightness step, not the recipe.
    //
    // The one thing to keep an eye on: red is also two thirds of a failed
    // state, and a failed environment's dot now sits on a red-family
    // panel. The wash is desaturated and dark where the alert red is
    // neither, and a dot is a glyph while this is a ground — but they are
    // in one family now, where purple was in none.
    const AWAY_DARK: &str = ".backlog-panel.away { background-color: \
                        color-mix(in srgb, @red_5 27%, @window_bg_color); }";
    const AWAY_LIGHT: &str = ".backlog-panel.away { background-color: \
                         color-mix(in srgb, @red_3 8%, @window_bg_color); }";
    let provider = gtk::CssProvider::new();
    // Above the sheet beside it, so a rule may be stated in both places
    // and the theme-conditional one is the one that lands.
    gtk::style_context_add_provider_for_display(
        display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    // The panels' softer text (`palette::PANEL_FG_DARK`). Set on each pane's
    // root and inherited, so anything that states no colour of its own
    // takes it and anything that does — an accent, a traffic light, a
    // markdown span — still wins.
    let panels = format!(
        ".panel-text {{ color: {}; }}\n",
        crate::palette::PANEL_FG_DARK
    );
    let apply = move |style: &adw::StyleManager, provider: &gtk::CssProvider| {
        let dark = style.is_dark();
        let away = if dark { AWAY_DARK } else { AWAY_LIGHT };
        let panels = if dark { panels.as_str() } else { "" };
        provider.load_from_string(&format!("{away}\n{panels}{}", search_css(dark)));
    };
    let style = adw::StyleManager::default();
    apply(&style, &provider);
    style.connect_dark_notify(move |style| apply(style, &provider));
}

/// The search's colours (SEARCH.md): one hue, `palette::SEARCH_FILL`, in
/// the shades `palette.rs` names — the wash under a results listing, the
/// tint behind a count badge and a lit transcript row, the ink the count
/// and the progress rules are drawn in. The wash is 11% in both schemes:
/// measured the way the away wash was, that is +4.6 L* on the dark window
/// and −4.9 on the light, the same step the purple takes, so only the ink
/// changes with the scheme. Solid fills (the selected hit, a tab's badge)
/// are handed to widgets by `palette::hit_background`.
///
/// The search box itself wears the hue always, query or none — the field
/// is the hue's source, and the colour is seen to flow from it to the
/// answers (David, 2026-09-06: "Make the search box always have the
/// search color scheme, as if saying, 'Typing here makes this color flow
/// to other parts of the IDE in the form of results'"). Its fill is the
/// hue at the strength that makes it as much a field as a stock entry is
/// (a stock entry is currentColor at 10%, +9.3 L* on the dark header bar
/// and −8.9 on the light; teal at 26% and 20% land on the same steps),
/// and its glyphs and focus ring are the ink.
fn search_css(dark: bool) -> String {
    let fill = crate::palette::SEARCH_FILL;
    let ink = crate::palette::search_ink(dark);
    let bg = crate::palette::hit_background(dark);
    let fg = crate::palette::hit_foreground(dark);
    // Every alpha and mix below is a way of asking for a LIGHTNESS STEP,
    // and purple is far darker than the teal it replaced, so each one was
    // re-measured against the step its teal produced rather than carried
    // over. Left as it was, the box's fill went from a tint to a slab on
    // dark and to nothing on light.
    let field = if dark { "0.42" } else { "0.16" };
    let listening = if dark { "0.80" } else { "0.43" };
    let wash = if dark { "17%" } else { "9%" };
    let hit = if dark { "0.38" } else { "0.20" };
    let badge = if dark { "0.30" } else { "0.16" };
    format!(
        ".search-box entry.search {{ background-color: alpha({fill}, {field}); }}\n\
         .search-box entry.search image {{ color: {ink}; }}\n\
         .search-box entry.search:focus-within {{ outline-color: {ink}; }}\n\
         /* Ctrl+F (or Start) held: the box is listening, and says so in \
            the hue's solid shade until the words replace the query. */\n\
         .search-box entry.search.listening {{ background-color: alpha({fill}, {listening}); \
           outline: 2px solid {ink}; outline-offset: -2px; }}\n\
         .results-panel {{ background-color: \
           color-mix(in srgb, {fill} {wash}, @window_bg_color); }}\n\
         .hit-badge {{ background-color: alpha({fill}, {badge}); color: {ink}; }}\n\
         .search-summary, .tab-key {{ color: {ink}; }}\n\
         /* The Tab strip (search.rs): the stop the search is on wears the \
            hue solid, as the one selected hit does; a stop with nothing \
            fades but keeps its place. An empty section's banner title \
            wears the same solid while the stop is on it. */\n\
         .tab-stop.tab-stop-current {{ background-color: {bg}; color: {fg}; }}\n\
         .tab-stop.tab-stop-empty {{ opacity: 0.55; }}\n\
         .tab-stop.tab-stop-empty.tab-stop-current {{ opacity: 1; }}\n\
         .results-title {{ border-radius: 6px; padding: 1px 6px; margin-left: -6px; }}\n\
         .results-current {{ background-color: {bg}; color: {fg}; }}\n\
         .search-hit {{ background-color: alpha({fill}, {hit}); }}\n\
         levelbar.search-rule block {{ background-color: {ink}; }}"
    )
}

fn open_workspace(app: &adw::Application, root: std::path::PathBuf) {
    // Recent folders are the desktop's recents — no custom list. Proper URI
    // escaping matters (spaces, unicode) so the entry stays clickable.
    // Probe instances (TASTE_PROBE_CHECK) leave no footprint, recents
    // included.
    if std::env::var("TASTE_PROBE_CHECK").is_err() {
        if let Ok(uri) = glib::filename_to_uri(&root, None) {
            gtk::RecentManager::default().add_item(&uri);
        }
    }
    // The auth proxy comes up here, before anything can ask about it.
    //
    // It has to be started from the runtime, and almost every *reader* of
    // it is on this thread instead — the console's spend and quota gauges,
    // the channel's hosting probe, a chat composing a spawn. Starting it at
    // the one place that owns the runtime, once per workspace, is what
    // keeps those readers pure reads.
    taste_acp::authproxy::start(runtime::runtime().handle());
    let window = window::build_window(app, root);
    window.present();
}

async fn taste_mcp_bridge(socket: &std::path::Path) -> anyhow::Result<()> {
    taste_mcp::stdio_bridge(socket).await
}

/// Mirrors every tracing event into `taste_core::app_log`, next to the
/// GLib messages, so `ide_app_log` is the one place an agent looks.
struct AppLogLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for AppLogLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        use std::fmt::Write;
        struct Collector(String);
        impl tracing::field::Visit for Collector {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    let _ = write!(self.0, "{value:?}");
                } else {
                    let _ = write!(self.0, " {}={:?}", field.name(), value);
                }
            }
        }
        let mut collector = Collector(String::new());
        event.record(&mut collector);
        taste_core::app_log::push(
            event.metadata().level().as_str(),
            event.metadata().target(),
            &collector.0,
        );
    }
}

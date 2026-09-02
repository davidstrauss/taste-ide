//! taste-ide: opinionated AI-supported IDE.
//!
//! One binary, two modes:
//! - default: the libadwaita application
//! - `--mcp-bridge <socket>`: stdio↔socket bridge, registered as an MCP
//!   stdio server in every agent session so agents can reach the IDE.

mod backlog;
mod chat;
mod chat_column;
mod chats;
mod command_completion;
mod composer;
mod console;
mod devcontainer_ui;
mod editor;
mod env_channel;
mod environments;
mod envstrip;
mod filetree;
mod fleet;
mod gadget;
#[allow(dead_code)] // kept for the style_ranges perf harness
mod markdown;
mod markdown_view;
mod notify;
mod orchestration;
mod runtime;
mod services;
mod sparkline;
mod ui_probe;
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
                 vte-terminal { padding: 4px 8px; }\n\
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
                 button.attachment-chip:hover { \
                   background-color: color-mix(in srgb, currentColor 15%, \
                   transparent); }\n\
                 /* A tool card's header is a button, and Adwaita bolds \
                    button labels. A tool title is a fact, not a heading. */\n\
                 label.tool-title { font-weight: normal; }\n\
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
                 /* The environment panel (envstrip.rs): a full-bleed band \
                    of rows at the bottom of the file-tree pane, one per \
                    environment, always visible. Full-bleed so its rows \
                    keep the pane's edges rather than sitting in it. */\n\
                 .env-panel-header { padding: 4px 12px 2px 8px; }\n\
                 /* The list is a switcher, not a document: tighter than \
                    .navigation-sidebar's default so six rows fit where a \
                    file tree also has to live. */\n\
                 .env-panel .env-list > row { min-height: 26px; \
                   padding: 0; margin: 0 4px; border-radius: 6px; }\n\
                 /* Not home. A tint the corner of an eye can catch, in a \
                    hue nothing else here uses — accent would read as a \
                    selection, and the state dots already own \
                    green/amber/red. Mixed into the window background, so \
                    it lands light on a light theme and dark on a dark \
                    one, and the theme's own foreground stays legible on \
                    it. */\n\
                 .env-panel.away { background-color: color-mix(in srgb, \
                   @purple_3 17%, @window_bg_color); }\n\
                 /* The header's + is an action among a list of places, \
                    and must not shout over them. */\n\
                 button.env-new { min-width: 22px; min-height: 22px; \
                   padding: 0; }\n\
                 /* Adwaita's spinner is sized for a dialog. Beside an \
                    8px status dot and a 14px sparkline it reads as the \
                    loudest thing on the row, which inverts the row's own \
                    hierarchy: a turn being in flight is the least \
                    actionable of the three facts. */\n\
                 .env-panel spinner { min-width: 12px; min-height: 12px; \
                   opacity: 0.7; }\n\
                 .env-dot { min-width: 8px; min-height: 8px; \
                   border-radius: 9999px; background-color: \
                   color-mix(in srgb, currentColor 35%, transparent); }\n\
                 /* Traffic lights (fleet.rs → Light). `unknown` keeps the \
                    faint currentColor above: the absence of a status must \
                    not look like one. */\n\
                 .env-dot.green { background-color: @success_color; }\n\
                 .env-dot.amber { background-color: @warning_color; }\n\
                 .env-dot.red { background-color: @error_color; }\n\
                 /* Every circle on a row is 8px — the traffic light's \
                    size. Two badges that almost match read as a mistake, \
                    and colour already carries which is which. */\n\
                 .env-unpublished { min-width: 8px; min-height: 8px; \
                   border-radius: 9999px; \
                   background-color: @accent_color; }\n\
                 /* Waiting on the user. Amber, the one hue this UI \
                    reserves for \"you are the blocker\". */\n\
                 .env-attention { min-width: 8px; min-height: 8px; \
                   border-radius: 9999px; \
                   background-color: @warning_color; }\n\
                 /* The subscription gauge in the panel header. A level \
                    bar at Adwaita's default height would be a slab beside \
                    a caption; at 4px it is a rule with a filled part, \
                    which is all it needs to be. The colour is stated \
                    rather than left to the level bar's own offsets, so \
                    the thresholds match the ones the chat pane's \
                    utilization tab tints its tab with. */\n\
                 .env-quota levelbar trough { min-height: 4px; \
                   border-radius: 2px; }\n\
                 .env-quota levelbar block { min-height: 4px; \
                   border-radius: 2px; }\n\
                 .env-quota levelbar block.filled { \
                   background-color: @accent_color; }\n\
                 .env-quota.warn levelbar block.filled { \
                   background-color: @warning_color; }\n\
                 .env-quota.spent levelbar block.filled { \
                   background-color: @error_color; }\n\
                 /* An hour-old reading is still true about the past and \
                    nothing about now. Faded rather than hidden: the \
                    number that was last seen is worth keeping on screen, \
                    and the tooltip says how old it is. */\n\
                 .env-quota.stale { opacity: 0.45; }\n\
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
                 .env-panel .env-list > row.review-flagged { \
                   background-image: linear-gradient(@accent_color, \
                   @accent_color); \
                   background-size: 2px 14px; \
                   background-position: left center; \
                   background-repeat: no-repeat; }\n\
                 /* Settled: merged or rejected. The user has ruled, so \
                    the row is history — dimmed, and the glyph says which \
                    way it went. */\n\
                 .env-panel .env-list > row.review-settled label { \
                   opacity: 0.6; }\n\
                 .env-review { color: @accent_color; }\n\
                 /* The backlog (backlog.rs): the environment panel's \
                    sibling, below it, folded away when it is not the \
                    question. Same header metrics as the panel above, so \
                    the two read as one stack rather than as two \
                    unrelated things that happen to be adjacent. */\n\
                 button.backlog-disclose { min-width: 18px; \
                   min-height: 18px; padding: 0; }\n\
                 .backlog-panel .backlog-list > row { min-height: 26px; \
                   padding: 0; margin: 0 4px; border-radius: 6px; }\n\
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

//! taste-ide: opinionated AI-supported IDE.
//!
//! One binary, two modes:
//! - default: the libadwaita application
//! - `--mcp-bridge <socket>`: stdio↔socket bridge, registered as an MCP
//!   stdio server in every agent session so agents can reach the IDE.

mod chat;
mod chat_tabs;
mod command_completion;
mod composer;
mod console;
mod devcontainer_ui;
mod editor;
mod env_channel;
mod filetree;
#[allow(dead_code)] // kept for the style_ranges perf harness
mod markdown;
mod markdown_view;
mod runtime;
mod services;
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
            css.load_from_string(
                ".prompt-entry { \
                   background-color: color-mix(in srgb, currentColor 10%, \
                   transparent); \
                   border: 1px solid transparent; border-radius: 6px; \
                   padding: 0; min-height: 44px; }\n\
                 .prompt-entry textview, .prompt-entry textview > text { \
                   background: transparent; }\n\
                 .prompt-entry entry.flat-entry { background: transparent; \
                   border: none; box-shadow: none; outline: none; \
                   min-height: 32px; }\n\
                 .prompt-entry:focus-within { \
                   border-color: @accent_bg_color; }\n\
                 vte-terminal { padding: 4px 8px; }\n\
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
                 /* The pinned prompt floats OVER the transcript, so it \
                    needs a surface of its own: Adwaita's .card colour is \
                    a translucent overlay and the scrolling text reads \
                    straight through it. Popover colours are the theme's \
                    opaque floating-surface tokens. */\n\
                 .pinned-prompt { background-color: @popover_bg_color; \
                   box-shadow: 0 1px 4px rgba(0, 0, 0, 0.35); }",
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

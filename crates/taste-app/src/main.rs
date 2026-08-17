//! taste-ide: opinionated AI-supported IDE.
//!
//! One binary, two modes:
//! - default: the libadwaita application
//! - `--mcp-bridge <socket>`: stdio↔socket bridge, registered as an MCP
//!   stdio server in every agent session so agents can reach the IDE.

mod chat;
mod composer;
mod console;
mod devcontainer_ui;
mod editor;
mod filetree;
#[allow(dead_code)] // kept for the style_ranges perf harness
mod markdown;
mod markdown_view;
mod runtime;
mod services;
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

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "taste=info,warn".into()),
        )
        .init();

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
        if let Some(display) = gtk::gdk::Display::default() {
            let css = gtk::CssProvider::new();
            css.load_from_string(
                ".prompt-entry { background-color: @view_bg_color; \
                   border: 1px solid @borders; border-radius: 6px; \
                   padding: 0 4px; min-height: 34px; }\n\
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
                   margin: 0; }",
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
    if let Ok(uri) = glib::filename_to_uri(&root, None) {
        gtk::RecentManager::default().add_item(&uri);
    }
    let window = window::build_window(app, root);
    window.present();
}

async fn taste_mcp_bridge(socket: &std::path::Path) -> anyhow::Result<()> {
    taste_mcp::stdio_bridge(socket).await
}

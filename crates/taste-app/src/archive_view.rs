//! A web archive in a viewer tab: an MHTML, WARC, or WACZ file unpacked
//! by the confined `taste-archive` helper (its readers and its HTML
//! rewriter never run in the IDE), and its pages drawn by WebKit, whose
//! web process WebKitGTK always sandboxes (David, 2026-10-26: "support for
//! viewing HTML and various web archive formats that bundle resources").
//!
//! The helper rewrites every URL an archived page or stylesheet names to
//! the resource's path in the archive, `/https/example.com/a.css`, so the
//! page draws from what was captured and never from the live web. What it
//! cannot rewrite, a script's own requests, meets the same policy the HTML
//! preview's answers carry: this archive's origin and inline content, and
//! nothing on the network. A link clicked to a page the archive does not
//! hold opens the live page in the user's browser.
//!
//! On `taste-archive://<id>/<path>`, answered only for the view that asks,
//! as the HTML preview's scheme is.

use std::cell::Cell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use webkit6::prelude::*;

const SCHEME: &str = "taste-archive";

/// How long an archive may take to unpack.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// The most the helper's index line may be.
const MAX_INDEX: u64 = 64 * 1024 * 1024;

/// The format word the helper takes for a file of this name, when it is an
/// archive: `.mht` and `.mhtml`, `.warc` and `.warc.gz`, `.wacz`.
pub fn format_of(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    if name.ends_with(".mht") || name.ends_with(".mhtml") {
        Some("mhtml")
    } else if name.ends_with(".warc") || name.ends_with(".warc.gz") {
        Some("warc")
    } else if name.ends_with(".wacz") {
        Some("wacz")
    } else {
        None
    }
}

/// A page of the archive a reader may open.
#[derive(Debug, Clone)]
pub struct ArchivedPage {
    pub url: String,
    pub path: String,
    pub title: Option<String>,
}

/// An archive unpacked: its pages, and every resource by its path, with
/// its media type.
pub struct Unpacked {
    pub pages: Vec<ArchivedPage>,
    resources: HashMap<String, (glib::Bytes, String)>,
}

/// How a resource's path is looked up: its fragment dropped and its
/// escapes undone, so the helper's spelling and WebKit's agree.
fn key(path: &str) -> String {
    crate::markdown_view::percent_decode(path.split('#').next().unwrap_or(path))
}

impl Unpacked {
    /// The resource at `path`, or the nearest capture of it: without its
    /// query, under the other of http and https, or with or without a
    /// trailing slash, as a page asking again for what it captured spells
    /// it.
    fn find(&self, path: &str) -> Option<&(glib::Bytes, String)> {
        let path = key(path);
        let mut tries = vec![path.clone()];
        if let Some((bare, _)) = path.split_once('?') {
            tries.push(bare.to_string());
        }
        for tried in tries.clone() {
            if let Some(rest) = tried.strip_prefix("/https/") {
                tries.push(format!("/http/{rest}"));
            } else if let Some(rest) = tried.strip_prefix("/http/") {
                tries.push(format!("/https/{rest}"));
            }
        }
        for tried in tries.clone() {
            match tried.strip_suffix('/') {
                Some(bare) => tries.push(bare.to_string()),
                None => tries.push(format!("{tried}/")),
            }
        }
        tries.iter().find_map(|tried| self.resources.get(tried))
    }
}

impl Unpacked {
    /// The paths it serves, sorted: what a probe reports.
    #[doc(hidden)]
    pub fn paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self.resources.keys().cloned().collect();
        paths.sort();
        paths
    }
}

/// Where the helper is: beside this executable, or wherever
/// `TASTE_ARCHIVE_BIN` names.
fn helper_path() -> Result<PathBuf, String> {
    if let Some(named) = std::env::var_os("TASTE_ARCHIVE_BIN") {
        return Ok(PathBuf::from(named));
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("taste-archive")))
        .filter(|path| path.is_file())
        .ok_or_else(|| "the archive helper (taste-archive) is not installed beside the IDE".into())
}

/// Unpack the archive `bytes`, a file named `name`, in the confined
/// helper. Blocking: run on the blocking pool.
pub fn unpack(name: &str, bytes: glib::Bytes) -> Result<Unpacked, String> {
    let format = format_of(name).ok_or_else(|| format!("{name} is not a web archive"))?;
    let program = helper_path()?;
    let mut policy = taste_confine::Policy::for_program(&program).map_err(|e| format!("{e:#}"))?;
    // An archive and what it unpacks to, both held at once.
    policy.memory = 8 << 30;
    let mut command = Command::new(&program);
    command
        .arg(format)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    taste_confine::confine(&mut command, &policy).map_err(|e| format!("{e:#}"))?;
    let mut child = command
        .spawn()
        .map_err(|e| format!("starting {}: {e}", program.display()))?;
    let mut stdin = child.stdin.take().ok_or("the helper has no stdin")?;
    let stdout = child.stdout.take().ok_or("the helper has no stdout")?;
    // Killed at the deadline, so the reads below end at the pipe's close;
    // waited on only after, so its pid cannot be another's by then.
    let pid = child.id() as libc::pid_t;
    let (done, finished) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if finished.recv_timeout(DEADLINE).is_err() {
            // SAFETY: a signal to our own unreaped child.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    });
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&bytes);
    });
    let read = read_reply(stdout);
    let _ = done.send(());
    let _ = writer.join();
    let _ = child.kill();
    let _ = child.wait();
    read
}

/// The helper's reply: its index line, checked, then each body in turn.
fn read_reply(stdout: std::process::ChildStdout) -> Result<Unpacked, String> {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    (&mut reader)
        .take(MAX_INDEX)
        .read_until(b'\n', &mut line)
        .map_err(|e| format!("reading the archive helper: {e}"))?;
    let index: serde_json::Value = serde_json::from_slice(&line)
        .map_err(|_| "the archive helper said something unreadable".to_string())?;
    if let Some(why) = index.get("error").and_then(serde_json::Value::as_str) {
        return Err(why.to_string());
    }
    let text = |value: &serde_json::Value, name: &str| {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let pages: Vec<ArchivedPage> = index["pages"]
        .as_array()
        .map(|pages| {
            pages
                .iter()
                .filter_map(|page| {
                    Some(ArchivedPage {
                        url: text(page, "url")?,
                        path: text(page, "path").filter(|p| p.starts_with('/'))?,
                        title: text(page, "title"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let mut resources = HashMap::new();
    let mut total: u64 = 0;
    for entry in index["resources"].as_array().into_iter().flatten() {
        let (Some(path), Some(mime), Some(len)) = (
            text(entry, "path"),
            text(entry, "mime"),
            entry.get("len").and_then(serde_json::Value::as_u64),
        ) else {
            return Err("the archive helper's index is malformed".into());
        };
        total += len;
        if total > 3 << 30 {
            return Err("the archive unpacks to more than the viewer holds".into());
        }
        let mut body = vec![0; len as usize];
        reader
            .read_exact(&mut body)
            .map_err(|_| "the archive helper stopped partway through".to_string())?;
        resources
            .entry(key(&path))
            .or_insert((glib::Bytes::from_owned(body), mime));
    }
    if pages.is_empty() {
        return Err("the archive holds no page to show".into());
    }
    Ok(Unpacked { pages, resources })
}

/// What one archive's scheme serves, and the view that may ask.
struct Site {
    unpacked: Rc<Unpacked>,
    view: glib::WeakRef<webkit6::WebView>,
}

thread_local! {
    static SITES: std::cell::RefCell<HashMap<u64, Site>> =
        std::cell::RefCell::new(HashMap::new());
    static NEXT_SITE: Cell<u64> = const { Cell::new(1) };
}

/// One archive's view: the page in front, and, for an archive of several
/// pages, the choice of page for the tab bar.
pub struct ArchiveView {
    pub view: webkit6::WebView,
    pub tools: Option<gtk::Widget>,
    id: u64,
}

impl Drop for ArchiveView {
    fn drop(&mut self) {
        SITES.with(|sites| sites.borrow_mut().remove(&self.id));
    }
}

/// The page's address on the scheme.
fn address(id: u64, path: &str) -> String {
    format!("{SCHEME}://{id}{path}")
}

/// What an archived path was captured from: `/https/example.com/a` is
/// `https://example.com/a`.
fn original_url(path: &str) -> Option<String> {
    let rest = path.strip_prefix('/')?;
    let (scheme, rest) = rest.split_once('/')?;
    matches!(scheme, "http" | "https").then(|| format!("{scheme}://{rest}"))
}

impl ArchiveView {
    pub fn new(unpacked: Unpacked) -> Rc<Self> {
        register_scheme();
        let id = NEXT_SITE.with(|next| next.replace(next.get() + 1));
        let unpacked = Rc::new(unpacked);
        let session = webkit6::NetworkSession::new_ephemeral();
        let view = webkit6::WebView::builder()
            .network_session(&session)
            .vexpand(true)
            .hexpand(true)
            .build();
        SITES.with(|sites| {
            sites.borrow_mut().insert(
                id,
                Site {
                    unpacked: unpacked.clone(),
                    view: view.downgrade(),
                },
            )
        });
        let prefix = format!("{SCHEME}://{id}");
        let held = unpacked.clone();
        view.connect_decide_policy(move |view, decision, kind| {
            let action = match kind {
                webkit6::PolicyDecisionType::NavigationAction
                | webkit6::PolicyDecisionType::NewWindowAction => decision
                    .downcast_ref::<webkit6::NavigationPolicyDecision>()
                    .and_then(|d| d.navigation_action()),
                _ => return false,
            };
            let Some(action) = action else {
                return false;
            };
            let Some(uri) = action.request().and_then(|r| r.uri()) else {
                return false;
            };
            let clicked = action.is_user_gesture()
                && action.navigation_type() == webkit6::NavigationType::LinkClicked;
            let browse = |uri: &str| {
                let window = view.root().and_downcast::<gtk::Window>();
                gtk::UriLauncher::new(uri).launch(
                    window.as_ref(),
                    None::<&gtk::gio::Cancellable>,
                    |_| {},
                );
            };
            if let Some(path) = uri.strip_prefix(&prefix) {
                if held.find(path).is_some() {
                    // A link meant for a new window opens here: this view
                    // is the archive's one window.
                    if kind == webkit6::PolicyDecisionType::NewWindowAction {
                        decision.ignore();
                        view.load_uri(&uri);
                        return true;
                    }
                    return false;
                }
                // Not in the archive: the live page, when it was clicked.
                decision.ignore();
                if clicked {
                    if let Some(original) = original_url(path) {
                        browse(&original);
                    }
                }
                return true;
            }
            if uri.starts_with("about:") || uri.starts_with("data:") || uri.starts_with("blob:") {
                return false;
            }
            decision.ignore();
            if clicked && (uri.starts_with("https://") || uri.starts_with("http://")) {
                browse(&uri);
            }
            true
        });
        let tools = (unpacked.pages.len() > 1).then(|| page_chooser(id, &unpacked.pages, &view));
        view.load_uri(&address(id, &unpacked.pages[0].path));
        Rc::new(Self { view, tools, id })
    }
}

/// The archive's pages as a drop-down for the tab bar: each by its title,
/// or its address when it has none, the address in the tooltip.
fn page_chooser(id: u64, pages: &[ArchivedPage], view: &webkit6::WebView) -> gtk::Widget {
    let names: Vec<String> = pages
        .iter()
        .map(|page| page.title.clone().unwrap_or_else(|| page.url.clone()))
        .collect();
    let model = gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>());
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(40)
            .build();
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            item.set_child(Some(&label));
        }
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let text = item
            .item()
            .and_downcast::<gtk::StringObject>()
            .map(|s| s.string().to_string())
            .unwrap_or_default();
        if let Some(label) = item.child().and_downcast::<gtk::Label>() {
            label.set_label(&text);
        }
    });
    let chooser = gtk::DropDown::builder()
        .model(&model)
        .factory(&factory)
        .tooltip_text(format!("The archive's {} pages", pages.len()))
        .focus_on_click(false)
        .build();
    let paths: Vec<String> = pages.iter().map(|page| page.path.clone()).collect();
    let view = view.downgrade();
    chooser.connect_selected_notify(move |chooser| {
        let (Some(view), Some(path)) = (view.upgrade(), paths.get(chooser.selected() as usize))
        else {
            return;
        };
        view.load_uri(&address(id, path));
        view.grab_focus();
    });
    chooser.upcast()
}

/// The policy every answer carries: this archive's origin, inline content,
/// and `data:` and `blob:` URLs, and nothing on the network.
fn policy(id: u64) -> String {
    format!(
        "default-src {SCHEME}://{id} data: blob: 'unsafe-inline' 'unsafe-eval'; \
         form-action 'none'"
    )
}

fn register_scheme() {
    thread_local! {
        static REGISTERED: Cell<bool> = const { Cell::new(false) };
    }
    if REGISTERED.with(|done| done.replace(true)) {
        return;
    }
    let Some(context) = webkit6::WebContext::default() else {
        return;
    };
    if let Some(security) = context.security_manager() {
        security.register_uri_scheme_as_secure(SCHEME);
        security.register_uri_scheme_as_cors_enabled(SCHEME);
    }
    context.register_uri_scheme(SCHEME, |request| {
        let uri = request.uri().map(|u| u.to_string()).unwrap_or_default();
        let asker = request.web_view();
        let found = answer(&uri, asker.as_ref());
        match found {
            Ok((id, bytes, mime)) => {
                let stream = gtk::gio::MemoryInputStream::from_bytes(&bytes);
                let response = webkit6::URISchemeResponse::new(&stream, bytes.len() as i64);
                response.set_content_type(&mime);
                let headers =
                    webkit6::soup::MessageHeaders::new(webkit6::soup::MessageHeadersType::Response);
                headers.append("Content-Security-Policy", &policy(id));
                response.set_http_headers(headers);
                request.finish_with_response(&response);
            }
            Err(why) => {
                let mut error = glib::Error::new(gtk::gio::IOErrorEnum::NotFound, &why);
                request.finish_error(&mut error);
            }
        }
    });
}

/// The resource `uri` names, for the view `asker`.
fn answer(
    uri: &str,
    asker: Option<&webkit6::WebView>,
) -> Result<(u64, glib::Bytes, String), String> {
    let rest = uri
        .strip_prefix(&format!("{SCHEME}://"))
        .ok_or_else(|| format!("{uri} is not an archive's"))?;
    let (id, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let id: u64 = id.parse().map_err(|_| format!("{uri} names no archive"))?;
    SITES.with(|sites| {
        let sites = sites.borrow();
        let site = sites
            .get(&id)
            .ok_or_else(|| format!("{uri}: that archive is closed"))?;
        if site.view.upgrade().as_ref() != asker {
            return Err(format!("{uri} is another archive's"));
        }
        let (bytes, mime) = site
            .unpacked
            .find(path)
            .ok_or_else(|| format!("{uri} was not captured"))?;
        Ok((id, bytes.clone(), mime.clone()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unpacked(paths: &[&str]) -> Unpacked {
        Unpacked {
            pages: Vec::new(),
            resources: paths
                .iter()
                .map(|p| {
                    (
                        key(p),
                        (glib::Bytes::from_static(b"x"), "text/plain".into()),
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn an_archive_is_known_by_its_name() {
        assert_eq!(format_of("Saved Page.MHTML"), Some("mhtml"));
        assert_eq!(format_of("crawl.warc.gz"), Some("warc"));
        assert_eq!(format_of("session.wacz"), Some("wacz"));
        assert_eq!(format_of("page.html"), None);
    }

    #[test]
    fn a_path_finds_its_capture_however_the_page_spells_it() {
        let archive = unpacked(&[
            "/https/example.com/a%20b.css",
            "/https/example.com/docs/",
            "/http/plain.org/x",
        ]);
        assert!(archive.find("/https/example.com/a b.css#top").is_some());
        assert!(archive.find("/https/example.com/a%20b.css?v=3").is_some());
        assert!(archive.find("/https/example.com/docs").is_some());
        assert!(archive.find("/https/plain.org/x").is_some());
        assert!(archive.find("/https/example.com/missing.js").is_none());
    }

    #[test]
    fn an_archived_path_names_the_page_it_was_captured_from() {
        assert_eq!(
            original_url("/https/example.com/a?b=1").as_deref(),
            Some("https://example.com/a?b=1")
        );
        assert_eq!(original_url("/cid/frame-1"), None);
    }
}

//! An HTML file's preview: the page as a browser draws it, in a WebKit
//! view, whose web process WebKitGTK always sandboxes, so the markup, the
//! stylesheets, and the scripts are parsed and run there and never in the
//! IDE's process (David, 2026-10-26: "I would definitely like support for
//! viewing HTML"). The document is the tab's buffer, unsaved edits and
//! all; what it links to (stylesheets, scripts, pictures, fonts, frames)
//! is read from beside it in its own checkout, through the files service,
//! as the Markdown preview's pictures are.
//!
//! Nothing reaches the network. Every answer carries a
//! Content-Security-Policy allowing this preview's own origin and inline
//! content only, so a page a repository or an agent wrote cannot call
//! out, and a link clicked to the web opens in the user's browser. A link
//! to another file of the checkout opens that file's tab, as a path link
//! in a Markdown preview does.
//!
//! On `taste-html://<id>/<path>`, the path from the checkout's root. A
//! request is answered only for the view it was made for: one preview
//! cannot read through another's id, which may name another environment's
//! checkout.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use webkit6::prelude::*;

use crate::markdown_view::DocumentBase;

const HTML_SCHEME: &str = "taste-html";

/// The largest file of a page's that is read for it.
const MAX_RESOURCE_BYTES: u64 = 64 * 1024 * 1024;

/// How long the typing pauses before the page is drawn again: a reload is
/// a whole page's layout and scripts, which twelve times a second would be.
const RELOAD_PAUSE: std::time::Duration = std::time::Duration::from_millis(400);

/// What a preview serves: its checkout, its document's path in it and its
/// text, and the one view that may ask.
struct Site {
    base: DocumentBase,
    doc: PathBuf,
    text: String,
    view: glib::WeakRef<webkit6::WebView>,
}

thread_local! {
    static SITES: RefCell<HashMap<u64, Site>> = RefCell::new(HashMap::new());
    static NEXT_SITE: Cell<u64> = const { Cell::new(1) };
}

/// One HTML file's preview face.
pub struct HtmlPreview {
    pub view: webkit6::WebView,
    id: u64,
    /// The document's address on the scheme.
    uri: String,
    loaded: Cell<bool>,
    /// Which edit the next reload is for: one overtaken while it waited
    /// is dropped.
    generation: Rc<Cell<u64>>,
}

impl HtmlPreview {
    /// A preview of `doc`, a file of `base`'s checkout, which opens a
    /// clicked link to the web with `open_url`.
    pub fn new(base: DocumentBase, doc: &Path, open_url: Rc<dyn Fn(&str)>) -> Rc<Self> {
        register_scheme();
        let id = NEXT_SITE.with(|next| next.replace(next.get() + 1));
        let rel = doc
            .strip_prefix(&base.root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| PathBuf::from(doc.file_name().unwrap_or_default()));
        let uri = format!("{HTML_SCHEME}://{id}/{}", escape_path(&rel));
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
                    base: base.clone(),
                    doc: rel.clone(),
                    text: String::new(),
                    view: view.downgrade(),
                },
            )
        });
        let prefix = format!("{HTML_SCHEME}://{id}/");
        view.connect_decide_policy(move |_, decision, kind| {
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
            if let Some(rest) = uri.strip_prefix(&prefix) {
                let (path, fragment) = match rest.split_once('#') {
                    Some((path, fragment)) => (path, Some(fragment)),
                    None => (rest, None),
                };
                let path = path.split('?').next().unwrap_or(path);
                let target = site_path(path);
                // A click to another file of the checkout opens its tab;
                // this page stays this file's. Anything else on the scheme
                // (a frame, the page's own anchors, a script's move)
                // happens here.
                if clicked && target.as_ref().is_some_and(|t| *t != rel) {
                    decision.ignore();
                    if let Some(target) = target {
                        let at = fragment.and_then(taste_core::anchor::Anchor::from_fragment);
                        (base.open_file)(base.root.join(target), at);
                    }
                    return true;
                }
                return false;
            }
            if uri.starts_with("about:") || uri.starts_with("data:") || uri.starts_with("blob:") {
                return false;
            }
            // Anywhere else: the user's browser has it when they clicked
            // it, and this view goes nowhere either way.
            decision.ignore();
            if clicked && (uri.starts_with("https://") || uri.starts_with("http://")) {
                open_url(&uri);
            }
            true
        });
        Rc::new(Self {
            view,
            id,
            uri,
            loaded: Cell::new(false),
            generation: Rc::new(Cell::new(0)),
        })
    }

    /// Show `text` as the document: at once the first time, and after a
    /// pause in the typing when it changes.
    pub fn show(self: &Rc<Self>, text: String) {
        let changed = SITES.with(|sites| {
            let mut sites = sites.borrow_mut();
            let Some(site) = sites.get_mut(&self.id) else {
                return false;
            };
            let changed = site.text != text;
            site.text = text;
            changed
        });
        if !self.loaded.replace(true) {
            self.view.load_uri(&self.uri);
            return;
        }
        if !changed {
            return;
        }
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(RELOAD_PAUSE, move || {
            let Some(preview) = weak.upgrade() else {
                return;
            };
            if preview.generation.get() != generation {
                return;
            }
            // A reload keeps the reader's place in the page; one that has
            // moved off the document (a frame does not) is put back on it.
            let on_document = preview
                .view
                .uri()
                .is_some_and(|uri| uri.split('#').next() == Some(preview.uri.as_str()));
            if on_document {
                preview.view.reload();
            } else {
                preview.view.load_uri(&preview.uri);
            }
        });
    }
}

impl Drop for HtmlPreview {
    fn drop(&mut self) {
        SITES.with(|sites| sites.borrow_mut().remove(&self.id));
    }
}

/// Each segment of `rel`, escaped for a URI path.
fn escape_path(rel: &Path) -> String {
    rel.components()
        .map(|c| glib::Uri::escape_string(&c.as_os_str().to_string_lossy(), None, false))
        .collect::<Vec<_>>()
        .join("/")
}

/// The file a request's path names, relative to the checkout's root:
/// `None` for one that would climb out of it. A folder is its
/// `index.html`.
fn site_path(path: &str) -> Option<PathBuf> {
    let mut rel = PathBuf::new();
    for segment in path.split('/') {
        let segment = crate::markdown_view::percent_decode(segment);
        match segment.as_str() {
            "" | "." => {}
            ".." => return None,
            _ if segment.contains(['/', '\0']) => return None,
            _ => rel.push(segment),
        }
    }
    if path.is_empty() || path.ends_with('/') {
        rel.push("index.html");
    }
    Some(rel)
}

/// The policy every answer carries: this preview's origin, the page's
/// inline styles and scripts, and `data:` and `blob:` URLs, and nothing
/// else, which leaves out the network.
fn policy(id: u64) -> String {
    format!(
        "default-src {HTML_SCHEME}://{id} data: blob: 'unsafe-inline' 'unsafe-eval'; \
         form-action 'none'"
    )
}

/// The media type a file's name calls for.
fn mime_of(path: &Path) -> String {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let (content_type, _) = gtk::gio::functions::content_type_guess(Some(name), None::<&[u8]>);
    gtk::gio::functions::content_type_get_mime_type(&content_type)
        .map(|mime| mime.to_string())
        .unwrap_or_else(|| "application/octet-stream".into())
}

/// What a request gets: bytes to hand over now, a file to read first, or
/// nothing.
enum Answer {
    Now(glib::Bytes, String),
    Read(taste_core::files::Files, PathBuf, String),
    Missing(String),
}

/// The answer for `uri`, asked by `asker`.
fn answer(uri: &str, asker: Option<&webkit6::WebView>) -> (u64, Answer) {
    let Some(rest) = uri.strip_prefix(&format!("{HTML_SCHEME}://")) else {
        return (0, Answer::Missing(format!("{uri} is not a preview's")));
    };
    let (id, path) = rest.split_once('/').unwrap_or((rest, ""));
    let path = path.split(['?', '#']).next().unwrap_or("");
    let Ok(id) = id.parse::<u64>() else {
        return (0, Answer::Missing(format!("{uri} names no preview")));
    };
    let answer = SITES.with(|sites| {
        let sites = sites.borrow();
        let Some(site) = sites.get(&id) else {
            return Answer::Missing(format!("{uri}: that preview is closed"));
        };
        if site.view.upgrade().as_ref() != asker {
            return Answer::Missing(format!("{uri} is another preview's"));
        }
        let Some(rel) = site_path(path) else {
            return Answer::Missing(format!("{uri} climbs out of the checkout"));
        };
        if rel == site.doc {
            // The buffer, marked as UTF-8 by its byte order mark, which an
            // HTML parser takes over any default.
            let xhtml = rel
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("xhtml"));
            let mime = if xhtml {
                "application/xhtml+xml"
            } else {
                "text/html"
            };
            let bytes = format!("\u{feff}{}", site.text).into_bytes();
            return Answer::Now(glib::Bytes::from_owned(bytes), mime.into());
        }
        let file = site.base.root.join(&rel);
        let mime = mime_of(&file);
        Answer::Read(site.base.files.clone(), file, mime)
    });
    (id, answer)
}

/// Finish `request` with `bytes`, under this preview's policy.
fn respond(request: &webkit6::URISchemeRequest, id: u64, bytes: &glib::Bytes, mime: &str) {
    let stream = gtk::gio::MemoryInputStream::from_bytes(bytes);
    let response = webkit6::URISchemeResponse::new(&stream, bytes.len() as i64);
    response.set_content_type(mime);
    let headers = webkit6::soup::MessageHeaders::new(webkit6::soup::MessageHeadersType::Response);
    headers.append("Content-Security-Policy", &policy(id));
    response.set_http_headers(headers);
    request.finish_with_response(&response);
}

fn refuse(request: &webkit6::URISchemeRequest, why: &str) {
    let mut error = glib::Error::new(gtk::gio::IOErrorEnum::NotFound, why);
    request.finish_error(&mut error);
}

/// Register the scheme on the default web context, once: secure and
/// CORS-enabled, as a page's module scripts need.
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
        security.register_uri_scheme_as_secure(HTML_SCHEME);
        security.register_uri_scheme_as_cors_enabled(HTML_SCHEME);
    }
    context.register_uri_scheme(HTML_SCHEME, |request| {
        let uri = request.uri().map(|u| u.to_string()).unwrap_or_default();
        let asker = request.web_view();
        match answer(&uri, asker.as_ref()) {
            (id, Answer::Now(bytes, mime)) => respond(request, id, &bytes, &mime),
            (_, Answer::Missing(why)) => refuse(request, &why),
            (id, Answer::Read(files, path, mime)) => {
                let request = request.clone();
                glib::spawn_future_local(async move {
                    let read = crate::runtime::runtime()
                        .spawn_blocking(move || -> Result<Vec<u8>, String> {
                            let size = files.stat(&path).map_err(|e| e.to_string())?.size;
                            if size > MAX_RESOURCE_BYTES {
                                return Err(format!("{} is too large to serve", path.display()));
                            }
                            files.read(&path).map_err(|e| e.to_string())
                        })
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()));
                    match read {
                        Ok(bytes) => respond(&request, id, &glib::Bytes::from_owned(bytes), &mime),
                        Err(why) => refuse(&request, &why),
                    }
                });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_path_stays_in_the_checkout() {
        assert_eq!(
            site_path("docs/a%20b.css"),
            Some(PathBuf::from("docs/a b.css"))
        );
        assert_eq!(site_path("./x/./y.js"), Some(PathBuf::from("x/y.js")));
        assert_eq!(site_path("docs/"), Some(PathBuf::from("docs/index.html")));
        assert_eq!(site_path(""), Some(PathBuf::from("index.html")));
        assert_eq!(site_path("../etc/passwd"), None);
        assert_eq!(site_path("a/%2e%2e/%2e%2e/etc"), None);
        assert_eq!(site_path("a%2F..%2Fb"), None);
    }

    #[test]
    fn a_path_is_escaped_a_segment_at_a_time() {
        assert_eq!(
            escape_path(Path::new("docs/a b/c#d.html")),
            "docs/a%20b/c%23d.html"
        );
    }

    #[test]
    fn the_policy_names_only_this_preview_and_inline_content() {
        let policy = policy(7);
        assert!(policy.starts_with("default-src taste-html://7 data: blob:"));
        assert!(!policy.contains("http"));
    }
}

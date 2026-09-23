//! A forwarded port as a document: the page a Ports row in the file tree
//! opens in the editor's strip.
//!
//! David, 2026-09-06: "When I click on a 'port,' it opens a tab up (in
//! the main editor area) with information about the port and what's
//! behind it. In the 'editor' view for the port, provide me ways to
//! interact with the service. Let's start with a REST client (that
//! supports schemas and JSON formatting) and a web browser (whatever GTK
//! can easily embed). The choice of these should be the same menu that
//! controls edit/preview for Markdown."
//!
//! So: a header that says what the port is — its number and label from
//! `portsAttributes`, the loopback address it is published on, whether
//! anything is listening, and what (the process in the container, the
//! server header, the content type) — over one of two faces chosen from
//! the editor's display-mode menu: **Browser** (WebKitGTK, an ephemeral
//! session — nothing a dev server sets outlives the tab) and **REST**
//! (`rest.rs`). The facts are gathered off the main thread by the window
//! and handed in through [`PortPage::set_facts`]; this page does no IO of
//! its own beyond what its faces do.
//!
//! The ports are the devcontainer's `forwardPorts`, which the supervisor
//! publishes on 127.0.0.1 and nowhere else — the devcontainer spec has no
//! notion of a service, and a forwarded port is the one running-thing it
//! does describe.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::hover::FullTextOnHover;
use adw::prelude::*;
use taste_devcontainer::config::PortSpec;
use webkit6::prelude::*;

/// The glyph every port surface wears: the tree section, the rows, the tab.
pub const PORT_ICON: &str = "network-server-symbolic";

/// Where a forwarded port stands, asked of the container rather than of
/// the host.
///
/// Rootless podman publishes a port by holding it open on the host for as
/// long as the container runs: pasta (or the rootlessport helper) binds
/// `127.0.0.1:host` the moment `podman run` returns, accepts every
/// connection, and resets the ones it cannot deliver inside. So a connect
/// to the published port answers "this container is up", never "this
/// service is up", and the only place the real question can be asked is
/// inside the container — which is how a tab came to say `listening` over
/// a page reading "Error receiving data: Connection reset by peer", with
/// nothing on that port inside at all (David, 2026-09-16).
///
/// Three states, because the middle one is the common mistake and is
/// invisible from the host: a dev server on the container's OWN loopback
/// — `php artisan serve`, `rails server` and `python -m http.server` all
/// default there — resets exactly like a port nothing holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortState {
    /// Something listens on an address the published port reaches.
    Listening,
    /// Something listens, but on `127.0.0.1` inside the container, which
    /// the published port cannot reach.
    Loopback,
    /// The port is published and nothing inside holds it.
    Nothing,
}

impl PortState {
    /// Whether a request to the published address can get an answer.
    pub fn reachable(self) -> bool {
        self == PortState::Listening
    }

    /// The dot beside the port, in the tree and in the tab's header.
    /// Amber for the loopback case: something IS running, which is not a
    /// fault, and it is not reachable, which is not health either.
    pub fn dot(self) -> &'static str {
        match self {
            PortState::Listening => "green",
            PortState::Loopback => "amber",
            PortState::Nothing => "off",
        }
    }

    /// The short word a tree row wears in front of its address. The
    /// header says the same thing at length, because it has the room and
    /// the row does not.
    pub fn word(self) -> &'static str {
        match self {
            PortState::Listening => "listening",
            PortState::Loopback => "loopback only",
            PortState::Nothing => "nothing listening",
        }
    }
}

/// What the window found out about a port, off the main thread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortFacts {
    /// What the container's own listener table says. `None` until probed.
    pub state: Option<PortState>,
    /// `users:(("node",pid=123,…))` from `ss` inside the container, made
    /// readable: `node (pid 123)`.
    pub process: Option<String>,
    /// The `Server:` header, when an HTTP probe got one.
    pub server: Option<String>,
    /// The content type of `/`, when an HTTP probe got one.
    pub content_type: Option<String>,
}

impl PortFacts {
    /// One line: state, then what is behind it.
    pub fn sentence(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        parts.push(match self.state {
            Some(PortState::Listening) => "listening".into(),
            // The one state that has to carry its own fix: from the host
            // it is indistinguishable from nothing at all.
            Some(PortState::Loopback) => {
                "listening on the container's loopback — bind 0.0.0.0 to reach it from here".into()
            }
            Some(PortState::Nothing) => "nothing listening".into(),
            None => "not probed yet".into(),
        });
        if let Some(process) = &self.process {
            parts.push(process.clone());
        }
        if let Some(server) = &self.server {
            parts.push(server.clone());
        }
        if let Some(kind) = &self.content_type {
            parts.push(kind.split(';').next().unwrap_or(kind).trim().to_string());
        }
        parts.join(" · ")
    }

    /// Which face a fresh tab opens on: an API answers in JSON and wants
    /// the REST client; anything else is a page and wants the browser.
    pub fn default_face(&self) -> PortFace {
        match &self.content_type {
            Some(kind) if kind.contains("json") => PortFace::Rest,
            _ => PortFace::Browser,
        }
    }
}

/// The two ways to interact with the service, chosen from the editor's
/// display-mode menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortFace {
    Browser,
    Rest,
}

impl PortFace {
    pub fn label(self) -> &'static str {
        match self {
            PortFace::Browser => "Browser",
            PortFace::Rest => "REST",
        }
    }
    pub fn icon(self) -> &'static str {
        match self {
            PortFace::Browser => "web-browser-symbolic",
            PortFace::Rest => "network-transmit-receive-symbolic",
        }
    }
    fn stack_name(self) -> &'static str {
        match self {
            PortFace::Browser => "browser",
            PortFace::Rest => "rest",
        }
    }
}

pub struct PortPage {
    pub widget: gtk::Widget,
    pub port: u16,
    pub base_url: String,
    facts_label: gtk::Label,
    state_dot: gtk::Box,
    stack: gtk::Stack,
    face: Cell<PortFace>,
    /// The browser face, made on first show: a WebKit web process is not
    /// free, and a tab opened for its REST face may never want one.
    browser_holder: gtk::Box,
    browser: RefCell<Option<webkit6::WebView>>,
    /// The load that failed and is showing the IDE's own error page: its
    /// uri and the reason, kept so a theme change redraws the page in the
    /// new theme, and cleared by the next navigation the user makes.
    failed_page: RefCell<Option<(String, String)>>,
    /// Set for the one load that is the IDE's own error page, so the
    /// `Started` it raises is not read as the user navigating away.
    alternate_loading: Cell<bool>,
    address: gtk::Entry,
    back: gtk::Button,
    forward: gtk::Button,
    pub rest: Rc<crate::rest::RestClient>,
    facts: RefCell<PortFacts>,
}

impl PortPage {
    pub fn new(environment: &str, spec: PortSpec, facts: PortFacts) -> Rc<Self> {
        let base_url = spec.url();

        // The header: what this is, and what is behind it.
        let heading = gtk::Label::builder()
            .label(format!("Port {}", spec.title()))
            .css_classes(["heading"])
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .hexpand(true)
            .build()
            .full_text_on_hover();
        let open_button = gtk::Button::builder()
            .icon_name("adw-external-link-symbolic")
            .css_classes(["flat"])
            .tooltip_text(format!("Open {base_url} in your browser"))
            .build();
        let copy_button = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .css_classes(["flat"])
            .tooltip_text("Copy the address")
            .build();
        let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        title_row.append(&gtk::Image::from_icon_name(PORT_ICON));
        title_row.append(&heading);
        title_row.append(&copy_button);
        title_row.append(&open_button);

        let state_dot = gtk::Box::builder()
            .css_classes(["env-dot", "off"])
            .valign(gtk::Align::Center)
            .build();
        let address_label = gtk::Label::builder()
            .label(format!("{base_url} · {environment}"))
            .css_classes(["caption", "dim-label"])
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .selectable(true)
            .build()
            .full_text_on_hover();
        let facts_label = gtk::Label::builder()
            .label(facts.sentence())
            .css_classes(["caption", "dim-label"])
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .hexpand(true)
            .build()
            .full_text_on_hover();
        let facts_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        facts_row.append(&state_dot);
        facts_row.append(&address_label);
        facts_row.append(
            &gtk::Label::builder()
                .label("·")
                .css_classes(["dim-label"])
                .build(),
        );
        facts_row.append(&facts_label);

        let header = gtk::Box::new(gtk::Orientation::Vertical, 2);
        header.set_margin_top(8);
        header.set_margin_bottom(6);
        header.set_margin_start(10);
        header.set_margin_end(10);
        header.append(&title_row);
        header.append(&facts_row);

        // Browser face: a small toolbar over the view.
        let back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .css_classes(["flat"])
            .tooltip_text("Back")
            .sensitive(false)
            .build();
        let forward = gtk::Button::builder()
            .icon_name("go-next-symbolic")
            .css_classes(["flat"])
            .tooltip_text("Forward")
            .sensitive(false)
            .build();
        let reload = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .css_classes(["flat"])
            .tooltip_text("Reload")
            .build();
        let address = gtk::Entry::builder()
            .text(&base_url)
            .hexpand(true)
            .width_chars(8)
            .input_purpose(gtk::InputPurpose::Url)
            .build();
        let browser_bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        browser_bar.set_margin_start(6);
        browser_bar.set_margin_end(6);
        browser_bar.set_margin_top(4);
        browser_bar.set_margin_bottom(4);
        browser_bar.append(&back);
        browser_bar.append(&forward);
        browser_bar.append(&reload);
        browser_bar.append(&address);
        let browser_holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        browser_holder.set_vexpand(true);
        let browser_face = gtk::Box::new(gtk::Orientation::Vertical, 0);
        browser_face.append(&browser_bar);
        browser_face.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        browser_face.append(&browser_holder);

        let rest = crate::rest::RestClient::new(&base_url);

        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        stack.set_transition_type(gtk::StackTransitionType::None);
        stack.add_named(&browser_face, Some(PortFace::Browser.stack_name()));
        stack.add_named(&rest.widget, Some(PortFace::Rest.stack_name()));

        let inner = gtk::Box::new(gtk::Orientation::Vertical, 0);
        inner.append(&header);
        inner.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        inner.append(&stack);
        // Its width is the editor's to give, never this page's to demand:
        // see `chat_column`. A request line and a browser toolbar are
        // exactly the kind of row that would otherwise set the pane's
        // minimum.
        let widget = crate::chat_column::ChatColumn::with_widths(&inner, 200, 720);

        let page = Rc::new(Self {
            widget: widget.upcast(),
            port: spec.port,
            base_url: base_url.clone(),
            facts_label,
            state_dot,
            stack,
            face: Cell::new(PortFace::Rest),
            browser_holder,
            browser: RefCell::new(None),
            failed_page: RefCell::new(None),
            alternate_loading: Cell::new(false),
            address,
            back,
            forward,
            rest,
            facts: RefCell::new(PortFacts::default()),
        });
        page.set_facts(&facts);
        page.set_face(facts.default_face());

        {
            let url = base_url.clone();
            open_button.connect_clicked(move |button| {
                let launcher = gtk::UriLauncher::new(&url);
                let window = button.root().and_downcast::<gtk::Window>();
                launcher.launch(window.as_ref(), gtk::gio::Cancellable::NONE, |result| {
                    if let Err(e) = result {
                        tracing::warn!("opening a port externally: {e}");
                    }
                });
            });
        }
        {
            let url = base_url.clone();
            copy_button.connect_clicked(move |button| {
                button.clipboard().set_text(&url);
            });
        }
        {
            let weak = Rc::downgrade(&page);
            page.address.connect_activate(move |entry| {
                if let Some(page) = weak.upgrade() {
                    page.load(&entry.text());
                }
            });
        }
        {
            let weak = Rc::downgrade(&page);
            reload.connect_clicked(move |_| {
                if let Some(page) = weak.upgrade() {
                    if let Some(view) = page.browser.borrow().as_ref() {
                        view.reload();
                    }
                }
            });
        }
        {
            let weak = Rc::downgrade(&page);
            page.back.connect_clicked(move |_| {
                if let Some(page) = weak.upgrade() {
                    if let Some(view) = page.browser.borrow().as_ref() {
                        view.go_back();
                    }
                }
            });
        }
        {
            let weak = Rc::downgrade(&page);
            page.forward.connect_clicked(move |_| {
                if let Some(page) = weak.upgrade() {
                    if let Some(view) = page.browser.borrow().as_ref() {
                        view.go_forward();
                    }
                }
            });
        }
        page
    }

    pub fn face(&self) -> PortFace {
        self.face.get()
    }

    /// Show one face. The browser is built on its first showing.
    pub fn set_face(self: &Rc<Self>, face: PortFace) {
        self.face.set(face);
        if face == PortFace::Browser {
            self.ensure_browser();
        }
        self.stack.set_visible_child_name(face.stack_name());
    }

    fn ensure_browser(self: &Rc<Self>) {
        if self.browser.borrow().is_some() {
            return;
        }
        // Ephemeral: cookies, storage and cache live as long as this tab
        // and no longer. A dev server's session is not something to keep.
        let session = webkit6::NetworkSession::new_ephemeral();
        // No downloads. The page is the project's — the agent's server —
        // and WebKit's default for an attachment is to save it into the
        // user's Downloads folder: a write into the home directory from the
        // far side of the boundary (review, 2026-09-23). A file the server
        // offers is had from the checkout instead.
        session.connect_download_started(|_, download| download.cancel());
        let view = webkit6::WebView::builder()
            .network_session(&session)
            .vexpand(true)
            .hexpand(true)
            .build();
        if let Some(settings) = webkit6::prelude::WebViewExt::settings(&view) {
            settings.set_enable_developer_extras(true);
        }
        // The theme reaches the page two ways. WebKit reads the dark
        // preference libadwaita keeps on GtkSettings, so a page asking
        // `prefers-color-scheme` gets the answer; and the view's own
        // background — what shows before a page paints, and behind one that
        // paints none — is the pane's rather than WebKit's white (David,
        // 2026-09-16: "we should send the necessary dark/light cue into the
        // browser context").
        webkit6::prelude::WebViewExt::set_background_color(
            &view,
            &gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0),
        );
        // A load that fails shows the IDE's own page, in the IDE's theme,
        // instead of WebKit's stock white one ("error pages we control
        // should match dark/light mode").
        {
            let weak = Rc::downgrade(self);
            webkit6::prelude::WebViewExt::connect_load_failed(&view, move |view, _, uri, error| {
                let Some(page) = weak.upgrade() else {
                    return false;
                };
                let reason = error.message().to_string();
                *page.failed_page.borrow_mut() = Some((uri.to_string(), reason.clone()));
                page.alternate_loading.set(true);
                // The view is the one passed in rather than the page's,
                // which is not stored until the first load has been asked
                // for — and the first load is the one that fails when the
                // service is not up.
                view.load_alternate_html(
                    &error_page(
                        uri,
                        &reason,
                        adw::StyleManager::default().is_dark(),
                        page.port,
                        &page.facts.borrow(),
                    ),
                    uri,
                    None,
                );
                true
            });
        }
        {
            let weak = Rc::downgrade(self);
            webkit6::prelude::WebViewExt::connect_load_changed(&view, move |_, event| {
                let Some(page) = weak.upgrade() else { return };
                if event == webkit6::LoadEvent::Started && !page.alternate_loading.replace(false) {
                    page.failed_page.borrow_mut().take();
                }
            });
        }
        {
            let weak = Rc::downgrade(self);
            adw::StyleManager::default().connect_dark_notify(move |_| {
                let Some(page) = weak.upgrade() else { return };
                page.redraw_failed_page();
            });
        }
        {
            let weak = Rc::downgrade(self);
            webkit6::prelude::WebViewExt::connect_uri_notify(&view, move |view| {
                let Some(page) = weak.upgrade() else { return };
                if let Some(uri) = webkit6::prelude::WebViewExt::uri(view) {
                    page.address.set_text(&uri);
                }
                page.back.set_sensitive(view.can_go_back());
                page.forward.set_sensitive(view.can_go_forward());
            });
        }
        self.guard_then_load(&view);
        self.browser_holder.append(&view);
        *self.browser.borrow_mut() = Some(view);
    }

    /// Load the port's page once the network guard is in place, and not
    /// before. The page is the project's — the agent's own server — and
    /// WebKit fetches for it with this host's network: WebKit's own sandbox
    /// confines the page's process to no files, but what it fetches is the
    /// network process's, which runs as the IDE does. Unguarded, its
    /// scripts reach this machine's loopback services and the LAN the VM's
    /// own network is kept from (`provision::PRIVATE_NETWORKS`). The guard
    /// is a content blocker ([`network_guard_rules`]), which applies to
    /// documents, subresources, fetch, XHR, and WebSockets alike; a guard
    /// that will not compile loads nothing.
    fn guard_then_load(self: &Rc<Self>, view: &webkit6::WebView) {
        let Some(port) = self
            .base_url
            .rsplit(':')
            .next()
            .and_then(|p| p.parse::<u16>().ok())
        else {
            self.refuse_load(view, "the port's address names no port to guard");
            return;
        };
        let store = webkit6::UserContentFilterStore::new(
            &gtk::glib::user_cache_dir()
                .join("taste-ide/content-filters")
                .to_string_lossy(),
        );
        let rules = gtk::glib::Bytes::from_owned(network_guard_rules(port).into_bytes());
        let weak = Rc::downgrade(self);
        let view = view.clone();
        store.save(
            &format!("port-{port}"),
            &rules,
            None::<&gtk::gio::Cancellable>,
            move |filter| {
                let Some(page) = weak.upgrade() else { return };
                let manager = webkit6::prelude::WebViewExt::user_content_manager(&view);
                match (filter, manager) {
                    (Ok(filter), Some(manager)) => {
                        manager.add_filter(&filter);
                        view.load_uri(&page.base_url);
                    }
                    (Ok(_), None) => {
                        page.refuse_load(&view, "the browser has no content manager to guard it")
                    }
                    (Err(e), _) => page.refuse_load(
                        &view,
                        &format!("the browser's network guard did not compile: {e}"),
                    ),
                }
            },
        );
    }

    fn refuse_load(&self, view: &webkit6::WebView, reason: &str) {
        view.load_alternate_html(
            &error_page(
                &self.base_url,
                reason,
                adw::StyleManager::default().is_dark(),
                self.port,
                &self.facts.borrow(),
            ),
            &self.base_url,
            None,
        );
    }

    fn load(self: &Rc<Self>, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let url = if text.contains("://") {
            text.to_string()
        } else if text.starts_with('/') {
            format!("{}{text}", self.base_url)
        } else {
            format!("http://{text}")
        };
        self.ensure_browser();
        if let Some(view) = self.browser.borrow().as_ref() {
            view.load_uri(&url);
        }
    }

    /// New facts from the window's probe: the header line and the dot.
    pub fn set_facts(&self, facts: &PortFacts) {
        if *self.facts.borrow() == *facts {
            return;
        }
        self.facts_label.set_label(&facts.sentence());
        for class in ["green", "amber", "off", "unknown"] {
            self.state_dot.remove_css_class(class);
        }
        // A ring rather than a grey dot before the first answer: not
        // probed yet is the absence of a state, and drawing it as `off`
        // would say "nothing is listening" a second before the container
        // says otherwise.
        self.state_dot.add_css_class(match facts.state {
            Some(state) => state.dot(),
            None => "unknown",
        });
        self.state_dot.set_tooltip_text(Some(match facts.state {
            Some(PortState::Listening) => "Something is listening on this port",
            Some(PortState::Loopback) => {
                "Something is listening inside the container, but on the container's own \
                 loopback: the forwarded port cannot reach it"
            }
            Some(PortState::Nothing) => "Nothing is listening on this port",
            None => "Not probed yet",
        }));
        *self.facts.borrow_mut() = facts.clone();
        // An error page already up was drawn from what was known then, and
        // what explains it usually lands a moment later.
        self.redraw_failed_page();
    }

    /// Redraw the IDE's error page, if one is showing, from what is known
    /// now: a new theme, or a probe that has since said why the load
    /// failed.
    fn redraw_failed_page(&self) {
        let failed = self.failed_page.borrow().clone();
        let (Some((uri, reason)), Some(view)) = (failed, self.browser.borrow().clone()) else {
            return;
        };
        self.alternate_loading.set(true);
        view.load_alternate_html(
            &error_page(
                &uri,
                &reason,
                adw::StyleManager::default().is_dark(),
                self.port,
                &self.facts.borrow(),
            ),
            &uri,
            None,
        );
    }

    /// TASTE_PROBE_CHECK only: the REST face at work, with facts.
    #[doc(hidden)]
    pub fn seed_for_probe(self: &Rc<Self>) {
        self.set_facts(&PortFacts {
            state: Some(PortState::Listening),
            process: Some("node (pid 4127)".into()),
            server: Some("Express".into()),
            content_type: Some("application/json".into()),
        });
        self.set_face(PortFace::Rest);
        self.rest.seed_for_probe();
    }

    /// TASTE_PROBE_CHECK only: pose a port in one of the two states that
    /// need a container to reach — `TASTE_PROBE_PORT=loopback|nothing`.
    ///
    /// The tab goes to its browser face, where the load then fails for
    /// real (a probe run has nothing behind the port) and the IDE's error
    /// page draws what these facts say about why. That page and the
    /// loopback header are otherwise reachable only from a checkout with
    /// a container running a server bound the wrong way.
    #[doc(hidden)]
    pub fn seed_state_for_probe(self: &Rc<Self>, state: PortState) {
        self.set_facts(&PortFacts {
            state: Some(state),
            process: match state {
                PortState::Nothing => None,
                _ => Some("php (pid 51)".into()),
            },
            ..PortFacts::default()
        });
        self.set_face(PortFace::Browser);
    }
}

/// The IDE's own page for a load that failed, in the IDE's theme: the
/// address that did not answer, WebKit's reason, and a link that tries
/// again. `color-scheme` tells the engine which palette the page is in, so
/// its form controls and scrollbars agree with the colours chosen here,
/// which are the terminal's — the one pair this window already keeps for
/// text on a plain ground in each theme.
/// The port browser's network guard, as WebKit content-blocker rules:
/// every load to a loopback, private, shared, or link-local address, an
/// IPv6 literal, a single-label host, or a `.local`-style name is blocked,
/// and then the port's own origin on 127.0.0.1 is let back through — any
/// scheme, so the dev server's own WebSocket (hot reload) still connects.
/// Public addresses are left alone: egress is a project's already
/// (ENVIRONMENTS → "Isolation: the standard, and what meets it"), and a
/// page's CDN fonts and scripts are how dev servers look. What a name
/// resolves to cannot be judged from a URL, so a public name pointing at a
/// private address is not caught here.
///
/// The rule syntax has no alternation, which is why each range is a rule
/// of its own.
fn network_guard_rules(port: u16) -> String {
    const BLOCKED: &[&str] = &[
        r"^[a-z]+://127\.",
        r"^[a-z]+://0\.",
        r"^[a-z]+://10\.",
        r"^[a-z]+://192\.168\.",
        r"^[a-z]+://169\.254\.",
        r"^[a-z]+://172\.1[6-9]\.",
        r"^[a-z]+://172\.2[0-9]\.",
        r"^[a-z]+://172\.3[01]\.",
        r"^[a-z]+://100\.6[4-9]\.",
        r"^[a-z]+://100\.[7-9][0-9]\.",
        r"^[a-z]+://100\.1[01][0-9]\.",
        r"^[a-z]+://100\.12[0-7]\.",
        r"^[a-z]+://\[",
        // A host with no dot: `localhost`, an intranet name, a router.
        r"^[a-z]+://[a-z0-9-]+[:/]",
        r"^[a-z]+://[a-z0-9.-]*\.local[:/]",
        r"^[a-z]+://[a-z0-9.-]*\.lan[:/]",
        r"^[a-z]+://[a-z0-9.-]*\.internal[:/]",
        r"^[a-z]+://[a-z0-9.-]*\.home\.arpa[:/]",
        r"^[a-z]+://[a-z0-9.-]*\.localhost[:/]",
    ];
    let mut rules: Vec<serde_json::Value> = BLOCKED
        .iter()
        .map(|filter| {
            serde_json::json!({
                "trigger": { "url-filter": filter },
                "action": { "type": "block" },
            })
        })
        .collect();
    rules.push(serde_json::json!({
        "trigger": { "url-filter": format!(r"^[a-z]+://127\.0\.0\.1:{port}/") },
        "action": { "type": "ignore-previous-rules" },
    }));
    serde_json::Value::Array(rules).to_string()
}

fn error_page(uri: &str, reason: &str, dark: bool, port: u16, facts: &PortFacts) -> String {
    let (fg, bg) = if dark {
        crate::palette::TERMINAL_DARK
    } else {
        crate::palette::TERMINAL_LIGHT
    };
    let scheme = if dark { "dark" } else { "light" };
    let uri = gtk::glib::markup_escape_text(uri);
    let reason = gtk::glib::markup_escape_text(reason);
    // WebKit's reason for a published port is always the same sentence
    // about a reset, and it names the wrong end: the host accepted and
    // reset because podman's forwarder could not reach the container.
    // This is the surface the user is looking at when that happens, so it
    // is where the two causes get named (see [`PortState`]).
    let holder = facts
        .process
        .as_deref()
        .map(|process| gtk::glib::markup_escape_text(process).to_string())
        .unwrap_or_else(|| "Something".into());
    let advice = match facts.state {
        Some(PortState::Loopback) => format!(
            "<p>{holder} is listening on port {port} inside the container, but on the \
             container's own <code>127.0.0.1</code> — an address the forwarded port cannot \
             reach, so the connection is accepted and then reset. Start it on \
             <code>0.0.0.0</code> instead.</p>"
        ),
        Some(PortState::Nothing) => format!(
            "<p>Nothing is listening on port {port} inside the container. The address \
             answers at all because podman holds the forwarded port open for as long as \
             the container runs, and resets what it cannot deliver.</p>"
        ),
        _ => "<p>The dot on the port's row says when something is listening.</p>".to_string(),
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"color-scheme\" content=\"{scheme}\">\
         <style>\
         html,body{{margin:0;background:{bg};color:{fg};font:15px/1.5 system-ui,sans-serif}}\
         main{{max-width:36em;margin:15vh auto 0;padding:0 24px}}\
         h1{{font-size:1.2em;font-weight:600;margin:0 0 .5em}}\
         p{{margin:0 0 .75em;opacity:.8}}\
         code{{font-family:monospace;opacity:1}}\
         a{{color:inherit}}\
         </style></head><body><main>\
         <h1>Nothing answers at <code>{uri}</code></h1>\
         <p>{reason}</p>\
         {advice}\
         <p><a href=\"{uri}\">Try again</a>.</p>\
         </main></body></html>"
    )
}

/// Blocking: does the PUBLISHED port accept a connection?
///
/// A weak question, and only the fallback for when there is no container
/// to ask: podman's forwarder accepts for the container's whole life
/// whether or not anything is behind it ([`PortState`]). [`states`] asks
/// the real one.
pub fn is_listening(port: u16) -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(300)).is_ok()
}

/// One listener inside the container: where it is bound, and the socket
/// inode that names the process holding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub state: PortState,
    /// The `inode` column verbatim — the token `/proc/<pid>/fd` links to
    /// as `socket:[…]`, which is how the holder gets named without `ss`.
    pub inode: Option<String>,
}

/// `/proc/net/tcp` and `/proc/net/tcp6`, concatenated, into the ports
/// something is listening on inside the container.
///
/// The kernel prints each address as its 32-bit words in the HOST's byte
/// order, so 127.0.0.1 comes out `0100007F` and the 127/8 test is the
/// word's last byte rather than its first. A port that appears twice — a
/// v4 and a v6 listener, or one socket per address — keeps the reachable
/// answer, because one reachable listener is enough.
pub fn listeners_from_proc(table: &str) -> HashMap<u16, Listener> {
    let mut found: HashMap<u16, Listener> = HashMap::new();
    for line in table.lines() {
        let mut fields = line.split_whitespace();
        // sl, local_address, rem_address, st — the header line's `st` is
        // the literal word, which is not `0A`, so it falls out here.
        let (Some(_), Some(local), Some(_), Some(st)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if st != "0A" {
            continue;
        }
        let Some((address, port)) = local.split_once(':') else {
            continue;
        };
        let Ok(port) = u16::from_str_radix(port, 16) else {
            continue;
        };
        let state = if is_loopback_hex(address) {
            PortState::Loopback
        } else {
            PortState::Listening
        };
        // tx:rx, tr:tm->when, retrnsmt, uid, timeout, then the inode.
        let inode = fields.nth(5).map(str::to_string);
        let listener = Listener { state, inode };
        match found.get(&port) {
            Some(held) if held.state.reachable() => {}
            _ => {
                found.insert(port, listener);
            }
        }
    }
    found
}

/// Is this the container's own loopback, in `/proc/net/tcp`'s spelling?
///
/// `0100007F` is 127.0.0.1 with its word byte-swapped, so every 127/8
/// address ends in `7F`; `::1` is three zero words and a byte-swapped one;
/// and a v4-mapped listener carries that same v4 word after `FFFF0000`.
fn is_loopback_hex(address: &str) -> bool {
    let address = address.to_ascii_uppercase();
    match address.len() {
        8 => address.ends_with("7F"),
        32 => {
            address == "00000000000000000000000001000000"
                || (address.starts_with("0000000000000000FFFF0000") && address.ends_with("7F"))
        }
        _ => false,
    }
}

/// The container's listener table, read over exec. `None` when there is
/// nowhere to ask or the read failed — which is NOT the same answer as
/// "nothing is listening", and must never be drawn as one.
pub async fn container_listeners(exec: &taste_core::ExecContext) -> Option<HashMap<u16, Listener>> {
    if !exec.has_exec_target() {
        return None;
    }
    // `/proc/net/*` rather than `ss`: this is the one question that must
    // not depend on the image carrying iproute2, and plenty do not — the
    // Fedora base this project's own environments are built from has
    // neither `ss` nor `netstat`, which is how the header came to say
    // `listening` with nothing after it.
    let resolved = exec.resolve(
        "sh",
        &["-c", "cat /proc/net/tcp /proc/net/tcp6 2>/dev/null"],
        false,
    );
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(&resolved.program)
            .args(&resolved.args)
            .stdin(std::process::Stdio::null())
            .output()
    })
    .await;
    let Ok(Ok(output)) = output else { return None };
    let table = String::from_utf8_lossy(&output.stdout);
    // An empty read is a failed one: the table always carries its header.
    if table.trim().is_empty() {
        return None;
    }
    Some(listeners_from_proc(&table))
}

/// What the file tree asks every few seconds for a whole environment: ONE
/// look inside the container for every port it forwards, rather than a
/// connect each. `ports` is `(container port, host port)`.
pub async fn states(
    exec: Option<taste_core::ExecContext>,
    ports: Vec<(u16, u16)>,
) -> Vec<(u16, PortState)> {
    if let Some(listeners) = match &exec {
        Some(exec) => container_listeners(exec).await,
        None => None,
    } {
        return ports
            .into_iter()
            .map(|(port, _)| {
                let state = listeners
                    .get(&port)
                    .map_or(PortState::Nothing, |listener| listener.state);
                (port, state)
            })
            .collect();
    }
    // Nothing to ask: the published port is all there is to go on, and it
    // is the weaker question — see [`is_listening`].
    tokio::task::spawn_blocking(move || {
        ports
            .into_iter()
            .map(|(port, host)| {
                let state = if is_listening(host) {
                    PortState::Listening
                } else {
                    PortState::Nothing
                };
                (port, state)
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// The deeper look a port tab takes: the container's listener table,
/// then the name of the process holding the port, then one GET of `/` for
/// the server header and the content type. Runs on the tokio runtime;
/// every blocking step is on the blocking pool. `exec` is the
/// environment's context, or `None` when there is no container to ask.
pub async fn probe(exec: Option<taste_core::ExecContext>, spec: PortSpec) -> PortFacts {
    let port = spec.port;
    let exec = exec.filter(|exec| exec.has_exec_target());
    let listener = match &exec {
        Some(exec) => container_listeners(exec)
            .await
            .map(|found| found.get(&port).cloned()),
        None => None,
    };
    let state = match &listener {
        Some(Some(listener)) => listener.state,
        Some(None) => PortState::Nothing,
        // Nothing to ask: the published port, weakly — see
        // [`is_listening`].
        None => {
            let host = spec.host;
            let up = tokio::task::spawn_blocking(move || is_listening(host))
                .await
                .unwrap_or(false);
            if up {
                PortState::Listening
            } else {
                PortState::Nothing
            }
        }
    };
    let mut facts = PortFacts {
        state: Some(state),
        ..PortFacts::default()
    };
    if state == PortState::Nothing {
        return facts;
    }
    if let Some(exec) = &exec {
        // `ss` names the process when the image has iproute2 and the
        // listener belongs to the asking user, which is the dev server's
        // user in the common case; anything else reads as no process,
        // never as an error. When the image has no `ss` at all, the socket
        // inode from the table above names it instead: whichever
        // `/proc/<pid>/fd` links to that socket is the holder. The
        // fallback SPEAKS ss's format, so one parser reads both.
        let inode = listener.flatten().and_then(|listener| listener.inode);
        let command = match &inode {
            Some(inode) => format!(
                "ss -ltnpH 'sport = :{port}' 2>/dev/null | grep . || \
                 for fd in /proc/[0-9]*/fd/*; do \
                 [ \"$(readlink \"$fd\" 2>/dev/null)\" = 'socket:[{inode}]' ] || continue; \
                 pid=${{fd#/proc/}}; pid=${{pid%%/*}}; \
                 printf 'users:((\"%s\",pid=%s,fd=0))' \
                 \"$(cat /proc/$pid/comm 2>/dev/null)\" \"$pid\"; break; done"
            ),
            None => format!("ss -ltnpH 'sport = :{port}' 2>/dev/null"),
        };
        let resolved = exec.resolve("sh", &["-c", &command], false);
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(&resolved.program)
                .args(&resolved.args)
                .stdin(std::process::Stdio::null())
                .output()
        })
        .await;
        if let Ok(Ok(output)) = output {
            facts.process = process_from_ss(&String::from_utf8_lossy(&output.stdout));
        }
    }
    // A port that accepts and resets has no answer to read: the GET would
    // spend its three seconds to learn what the table already said. The
    // process above came first for exactly that reason — a loopback
    // listener is the case the user most needs named.
    if !state.reachable() {
        return facts;
    }
    let root = format!("{}/", spec.url());
    if let Ok(Ok(response)) = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        crate::rest::send("GET".into(), root, Vec::new(), None),
    )
    .await
    {
        facts.server = response
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("server"))
            .map(|(_, v)| v.clone());
        facts.content_type = response.content_type().map(str::to_string);
    }
    facts
}

/// `ss -ltnpH 'sport = :3000'` output into `node (pid 123)`, when the
/// line names a process.
pub fn process_from_ss(output: &str) -> Option<String> {
    let start = output.find("users:((")? + "users:((".len();
    let rest = &output[start..];
    let end = rest.find("))")?;
    let inner = &rest[..end];
    // `"node",pid=123,fd=20` — possibly several, comma-separated groups.
    let first = inner.split("),(").next()?;
    let mut name = None;
    let mut pid = None;
    for field in first.split(',') {
        let field = field.trim();
        if let Some(value) = field.strip_prefix("pid=") {
            pid = Some(value.to_string());
        } else if field.starts_with('"') {
            name = Some(field.trim_matches('"').to_string());
        }
    }
    let name = name?;
    Some(match pid {
        Some(pid) => format!("{name} (pid {pid})"),
        None => name,
    })
}

#[cfg(test)]
mod tests {
    /// The page names the theme it is drawn in, and what it quotes is
    /// escaped: a reason with a `<` in it is text, never markup.
    #[test]
    fn the_error_page_follows_the_theme_and_escapes_what_it_quotes() {
        let facts = super::PortFacts::default();
        let dark = super::error_page(
            "http://127.0.0.1:8000/",
            "Could not connect",
            true,
            8000,
            &facts,
        );
        assert!(dark.contains("content=\"dark\""), "{dark}");
        assert!(dark.contains(crate::palette::TERMINAL_DARK.1), "{dark}");
        let light = super::error_page(
            "http://127.0.0.1:8000/",
            "a <b> reason",
            false,
            8000,
            &facts,
        );
        assert!(light.contains("content=\"light\""), "{light}");
        assert!(
            light.contains("a &lt;b&gt; reason") && !light.contains("<b>"),
            "{light}"
        );
    }

    use super::*;

    #[test]
    fn the_network_guard_blocks_the_host_and_lets_the_port_through() {
        let rules: serde_json::Value = serde_json::from_str(&network_guard_rules(3000)).unwrap();
        let rules = rules.as_array().unwrap();
        let last = rules.last().unwrap();
        assert_eq!(last["action"]["type"], "ignore-previous-rules");
        assert_eq!(
            last["trigger"]["url-filter"],
            r"^[a-z]+://127\.0\.0\.1:3000/"
        );
        let blocked: Vec<&str> = rules[..rules.len() - 1]
            .iter()
            .map(|rule| rule["trigger"]["url-filter"].as_str().unwrap())
            .collect();
        for filter in &blocked {
            assert!(
                !filter.contains('|'),
                "no alternation in this syntax: {filter}"
            );
        }
        assert!(blocked.contains(&r"^[a-z]+://127\."));
        assert!(blocked.contains(&r"^[a-z]+://192\.168\."));
    }

    #[test]
    fn ss_output_names_the_process() {
        let line =
            r#"LISTEN 0 511 *:3000 *:* users:(("node",pid=4127,fd=20),("node",pid=4127,fd=19))"#;
        assert_eq!(process_from_ss(line).as_deref(), Some("node (pid 4127)"));
        assert_eq!(process_from_ss("LISTEN 0 511 *:3000 *:*"), None);
        assert_eq!(process_from_ss(""), None);
    }

    #[test]
    fn facts_read_as_one_line_and_pick_a_face() {
        let facts = PortFacts {
            state: Some(PortState::Listening),
            process: Some("node (pid 1)".into()),
            server: None,
            content_type: Some("text/html; charset=utf-8".into()),
        };
        assert_eq!(facts.sentence(), "listening · node (pid 1) · text/html");
        assert_eq!(facts.default_face(), PortFace::Browser);
        let api = PortFacts {
            content_type: Some("application/json".into()),
            ..PortFacts::default()
        };
        assert_eq!(api.default_face(), PortFace::Rest);
        assert_eq!(PortFacts::default().sentence(), "not probed yet");
    }

    /// The header carries the fix for the one state that cannot be seen
    /// from the host, and the tree's word for it stays short.
    #[test]
    fn a_loopback_listener_says_how_to_reach_it() {
        let facts = PortFacts {
            state: Some(PortState::Loopback),
            process: Some("php (pid 51)".into()),
            ..PortFacts::default()
        };
        assert_eq!(
            facts.sentence(),
            "listening on the container's loopback — bind 0.0.0.0 to reach it from here · \
             php (pid 51)"
        );
        assert_eq!(PortState::Loopback.word(), "loopback only");
        assert_eq!(PortState::Loopback.dot(), "amber");
        assert!(!PortState::Loopback.reachable());
    }

    /// The table as the kernel prints it: listeners only, addresses in the
    /// host's byte order, and the inode that names the holder.
    #[test]
    fn the_proc_table_names_listeners_and_where_they_are_bound() {
        let table = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F40 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 782251 1 x
   1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 782252 1 x
   2: 0100007F:9931 0100007F:B314 01 00000000:00000000 00:00000000 00000000  1000        0 801525 1 x
   0: 00000000000000000000000000000000:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 782253 1 x
   1: 00000000000000000000000001000000:1F41 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 782254 1 x
";
        let found = listeners_from_proc(table);
        // 127.0.0.1:8000 — the case that resets from the host.
        assert_eq!(found[&8000].state, PortState::Loopback);
        assert_eq!(found[&8000].inode.as_deref(), Some("782251"));
        // 0.0.0.0:8080 and [::]:3000 — reachable.
        assert_eq!(found[&8080].state, PortState::Listening);
        assert_eq!(found[&3000].state, PortState::Listening);
        // [::1]:8001 — loopback, the v6 spelling of the same mistake.
        assert_eq!(found[&8001].state, PortState::Loopback);
        // An ESTABLISHED connection is not a listener, and the header is
        // not a row.
        assert!(!found.contains_key(&39217));
        assert_eq!(found.len(), 4);
    }

    /// One reachable listener is enough, whichever order the table lists
    /// the two in.
    #[test]
    fn a_reachable_listener_wins_over_a_loopback_one_on_the_same_port() {
        let row = |address: &str, inode: &str| {
            format!(
                "   0: {address}:1F40 00000000:0000 0A 00000000:00000000 00:00000000 \
                 00000000  1000        0 {inode} 1 x\n"
            )
        };
        let loopback_first = format!("{}{}", row("0100007F", "1"), row("00000000", "2"));
        let any_first = format!("{}{}", row("00000000", "2"), row("0100007F", "1"));
        for table in [loopback_first, any_first] {
            assert_eq!(
                listeners_from_proc(&table)[&8000].state,
                PortState::Listening
            );
        }
    }

    /// v4-mapped and plain v6 loopback, the two spellings `/proc/net/tcp6`
    /// uses, against addresses that only look like them.
    #[test]
    fn loopback_is_read_out_of_the_kernels_byte_order() {
        assert!(is_loopback_hex("0100007F")); // 127.0.0.1
        assert!(is_loopback_hex("0200007F")); // 127.0.0.2
        assert!(is_loopback_hex("0000000000000000FFFF00000100007F")); // ::ffff:127.0.0.1
        assert!(is_loopback_hex("00000000000000000000000001000000")); // ::1
        assert!(!is_loopback_hex("00000000")); // 0.0.0.0
        assert!(!is_loopback_hex("0F00A8C0")); // 192.168.0.15
        assert!(!is_loopback_hex("0000000000000000FFFF00000F00A8C0")); // ::ffff:192.168.0.15
        assert!(!is_loopback_hex("00000000000000000000000000000000")); // ::
    }

    /// The page explains the reset rather than repeating WebKit's word for
    /// it: the two causes, each in its own terms.
    #[test]
    fn the_error_page_names_the_cause_the_probe_found() {
        let page = |state, process: Option<&str>| {
            error_page(
                "http://127.0.0.1:8000/",
                "Error receiving data: Connection reset by peer",
                true,
                8000,
                &PortFacts {
                    state: Some(state),
                    process: process.map(str::to_string),
                    ..PortFacts::default()
                },
            )
        };
        let loopback = page(PortState::Loopback, Some("php (pid 51)"));
        assert!(
            loopback.contains("php (pid 51) is listening on port 8000"),
            "{loopback}"
        );
        assert!(loopback.contains("<code>0.0.0.0</code>"), "{loopback}");
        let nothing = page(PortState::Nothing, None);
        assert!(
            nothing.contains("Nothing is listening on port 8000 inside the container"),
            "{nothing}"
        );
        assert!(
            nothing.contains("podman holds the forwarded port open"),
            "{nothing}"
        );
    }
}

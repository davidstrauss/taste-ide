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
use std::rc::Rc;

use crate::hover::FullTextOnHover;
use adw::prelude::*;
use taste_devcontainer::config::PortSpec;
use webkit6::prelude::*;

/// The glyph every port surface wears: the tree section, the rows, the tab.
pub const PORT_ICON: &str = "network-server-symbolic";

/// What the window found out about a port, off the main thread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortFacts {
    /// A TCP connect to 127.0.0.1:port succeeded. `None` until probed.
    pub listening: Option<bool>,
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
        parts.push(match self.listening {
            Some(true) => "listening".into(),
            Some(false) => "nothing listening".into(),
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
        let view = webkit6::WebView::builder()
            .network_session(&session)
            .vexpand(true)
            .hexpand(true)
            .build();
        if let Some(settings) = webkit6::prelude::WebViewExt::settings(&view) {
            settings.set_enable_developer_extras(true);
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
        view.load_uri(&self.base_url);
        self.browser_holder.append(&view);
        *self.browser.borrow_mut() = Some(view);
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
        for class in ["green", "amber", "off"] {
            self.state_dot.remove_css_class(class);
        }
        self.state_dot.add_css_class(match facts.listening {
            Some(true) => "green",
            Some(false) => "off",
            None => "amber",
        });
        self.state_dot.set_tooltip_text(Some(match facts.listening {
            Some(true) => "Something is listening on this port",
            Some(false) => "Nothing is listening on this port",
            None => "Not probed yet",
        }));
        *self.facts.borrow_mut() = facts.clone();
    }

    /// TASTE_PROBE_CHECK only: the REST face at work, with facts.
    #[doc(hidden)]
    pub fn seed_for_probe(self: &Rc<Self>) {
        self.set_facts(&PortFacts {
            listening: Some(true),
            process: Some("node (pid 4127)".into()),
            server: Some("Express".into()),
            content_type: Some("application/json".into()),
        });
        self.set_face(PortFace::Rest);
        self.rest.seed_for_probe();
    }
}

/// Blocking: does anything answer on 127.0.0.1:port? A connect, not a
/// request — cheap enough to ask every few seconds for every listed port.
pub fn is_listening(port: u16) -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(300)).is_ok()
}

/// The deeper look a port tab takes: the connect, then one GET of `/` for
/// the server header and the content type, then `ss` inside the container
/// for the process that holds the port. Runs on the tokio runtime; every
/// blocking step is on the blocking pool. `exec` is the environment's
/// context, or `None` when there is no container to ask.
pub async fn probe(exec: Option<taste_core::ExecContext>, spec: PortSpec) -> PortFacts {
    let port = spec.port;
    let listening = tokio::task::spawn_blocking(move || is_listening(port))
        .await
        .unwrap_or(false);
    let mut facts = PortFacts {
        listening: Some(listening),
        ..PortFacts::default()
    };
    if !listening {
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
    if let Some(exec) = exec.filter(|exec| exec.has_exec_target()) {
        // `ss` names processes the asking user owns, which is the dev
        // server's user in the common case; anything else reads as no
        // process, never as an error.
        let command = format!("ss -ltnpH 'sport = :{port}' 2>/dev/null");
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
    use super::*;

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
            listening: Some(true),
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
}

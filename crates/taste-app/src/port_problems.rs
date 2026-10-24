//! What went wrong loading a port's page: the files it asked for that did
//! not load, the script errors nothing caught, and an error status on the
//! page itself, counted on a button at the end of the browser's address
//! bar and listed in its popover (David, 2026-10-24: "I don't necessarily
//! want the full debug tooling … but I do want to know something went
//! wrong"). A Laravel page whose styles were refused drew unstyled with
//! nothing to say so, which read as the browser's fault.
//!
//! The page tells nobody; a script the IDE adds does. It runs in a world
//! of its own ([`WORLD`]), which shares the page's document and events but
//! none of its globals, so the page can neither see it nor write its list.
//! It keeps what it saw in that world and pings a message handler; the
//! ping says only "look", and each tab then reads its own view's list,
//! because a tab opened from another shares that one's content manager and
//! a message cannot say which view sent it.
//!
//! Where the reason is the IDE's own — the network guard, a port not
//! forwarded — or WebKit's — 0.0.0.0, which it refuses outright — the row
//! says so. Everything else is the Web Inspector's to explain, and the
//! popover opens it — as does the toggle beside the count, which shows and
//! closes the inspector whether or not anything went wrong. Prompt Agent
//! hands the list to the environment's agent, as a failed task's output is
//! handed over (`tasks::repair_prompt`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use webkit6::prelude::*;

/// The script world the watcher runs in, apart from the page's — and
/// where the IDE asks anything else of a page (`PortPage::fit_canvas`).
pub const WORLD: &str = "taste-ide";
/// The message handler it pings when it has seen something.
const HANDLER: &str = "tasteProblems";
/// How many it keeps: a page with a thousand broken images has one
/// problem worth reading, and the list stays short enough to read.
const KEPT: usize = 100;

/// Installed at the document's start, in [`WORLD`]: a capturing `error`
/// listener hears every element whose load failed (those events do not
/// bubble) and every uncaught exception (those reach the window), and
/// `unhandledrejection` the promises nobody caught.
const WATCHER: &str = r#"(() => {
  const found = [];
  window.__tasteFound = found;
  const note = (kind, text) => {
    if (found.length >= KEPT || found.some((p) => p.kind === kind && p.text === text)) return;
    found.push({ kind, text });
    try { window.webkit.messageHandlers.HANDLER.postMessage(''); } catch (e) {}
  };
  addEventListener('error', (event) => {
    const target = event.target;
    if (target && target !== window && target.nodeType === 1) {
      const url = target.currentSrc || target.src || target.href;
      note('resource', typeof url === 'string' && url ? url : '<' + target.localName + '>');
    } else if (event.message) {
      const at = event.filename ? ' (' + event.filename + ':' + event.lineno + ')' : '';
      note('script', event.message + at);
    }
  }, true);
  addEventListener('unhandledrejection', (event) => {
    let reason = '';
    try { reason = String((event.reason && event.reason.message) || event.reason || ''); } catch (e) {}
    note('script', 'Unhandled promise rejection' + (reason ? ': ' + reason : ''));
  });
})();"#;

/// What the IDE reads back from [`WORLD`].
const READ: &str = "JSON.stringify(window.__tasteFound || [])";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// A file the page asked for that did not load: its address.
    Resource,
    /// A script error nothing caught: its message.
    Script,
    /// The page itself answered with an error status.
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Problem {
    kind: Kind,
    text: String,
}

/// Where Prompt Agent sends what it composed: the prompt, and the list as
/// its attachment's file name and text.
pub type PromptHook = Rc<dyn Fn(String, (String, String))>;

/// The browser's problem count and the list behind it, one per port page.
pub struct Problems {
    pub button: gtk::MenuButton,
    /// The Web Inspector, shown and closed: after the count on the bar.
    pub inspect: gtk::ToggleButton,
    /// Set while the toggle is being told what the inspector did, so the
    /// telling is not taken for a press.
    following_inspector: Cell<bool>,
    count: gtk::Label,
    rows: gtk::Box,
    popover: gtk::Popover,
    /// The port the page's server listens on, in its environment.
    port: u16,
    on_prompt: RefCell<Option<PromptHook>>,
    prompt_agent: gtk::Button,
    /// The host ports the environment forwards, for saying why a file on
    /// one of this machine's ports was not let through.
    forwarded: Vec<u16>,
    /// What the watcher reported, and the page's own status when it was an
    /// error; both cleared when a page starts loading.
    found: RefCell<Vec<Problem>>,
    status: Cell<Option<u32>>,
    /// A read already asked for: a page whose fifty images fail pings
    /// fifty times, and one read answers them all.
    reading: Cell<bool>,
    view: RefCell<Option<glib::WeakRef<webkit6::WebView>>>,
}

impl Problems {
    pub fn new(port: u16, forwarded: Vec<u16>) -> Rc<Self> {
        let count = gtk::Label::new(None);
        let face = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        face.append(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        face.append(&count);
        let rows = gtk::Box::new(gtk::Orientation::Vertical, 10);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(320)
            .child(&rows)
            .build();
        let inspect = gtk::Button::builder().label("Open Web Inspector").build();
        let prompt_agent = gtk::Button::builder()
            .label("Prompt Agent")
            .tooltip_text("Hand this list to the environment's agent to fix")
            // Shown once there is an agent to hand it to.
            .visible(false)
            .build();
        let actions = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .halign(gtk::Align::End)
            .build();
        actions.append(&inspect);
        actions.append(&prompt_agent);
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .width_request(340)
            .build();
        content.append(
            &gtk::Label::builder()
                .label("Problems loading this page")
                .css_classes(["heading"])
                .xalign(0.0)
                .build(),
        );
        content.append(&scroller);
        content.append(&actions);
        let popover = gtk::Popover::builder().child(&content).build();
        // A probe target of its own: `window.port-problems`.
        popover.set_widget_name("port-problems");
        let button = gtk::MenuButton::builder()
            .child(&face)
            .popover(&popover)
            .css_classes(["flat", "warning"])
            .visible(false)
            .build();
        let inspector_toggle = gtk::ToggleButton::builder()
            .icon_name("taste-inspect-symbolic")
            .css_classes(["flat"])
            .tooltip_text("Web Inspector")
            .build();
        let problems = Rc::new(Self {
            button,
            inspect: inspector_toggle,
            following_inspector: Cell::new(false),
            count,
            rows,
            popover: popover.clone(),
            port,
            on_prompt: RefCell::new(None),
            prompt_agent: prompt_agent.clone(),
            forwarded,
            found: RefCell::new(Vec::new()),
            status: Cell::new(None),
            reading: Cell::new(false),
            view: RefCell::new(None),
        });
        {
            let weak = Rc::downgrade(&problems);
            inspect.connect_clicked(move |_| {
                let Some(problems) = weak.upgrade() else {
                    return;
                };
                problems.popover.popdown();
                problems.inspect.set_active(true);
            });
        }
        {
            let weak = Rc::downgrade(&problems);
            problems.inspect.connect_toggled(move |toggle| {
                let Some(problems) = weak.upgrade() else {
                    return;
                };
                if problems.following_inspector.get() {
                    return;
                }
                let view = problems.view.borrow().as_ref().and_then(|v| v.upgrade());
                if let Some(inspector) = view.and_then(|view| view.inspector()) {
                    if toggle.is_active() {
                        inspector.show();
                    } else {
                        inspector.close();
                    }
                }
            });
        }
        {
            let weak = Rc::downgrade(&problems);
            prompt_agent.connect_clicked(move |_| {
                let Some(problems) = weak.upgrade() else {
                    return;
                };
                problems.popover.popdown();
                problems.prompt();
            });
        }
        problems
    }

    /// Where Prompt Agent hands the list. Until it is set the button is not
    /// shown: a list with nobody to give it to offers nothing.
    pub fn set_on_prompt(&self, hook: PromptHook) {
        *self.on_prompt.borrow_mut() = Some(hook);
        self.prompt_agent.set_visible(true);
    }

    /// The toggle says what the inspector is doing, however it was opened
    /// or closed — from here, from the list, or from the page's own
    /// Inspect Element.
    fn follow_inspector(&self, open: bool) {
        self.following_inspector.set(true);
        self.inspect.set_active(open);
        self.following_inspector.set(false);
    }

    /// Prompt Agent: the list as it stands, with the page it is about.
    fn prompt(&self) {
        let Some(hook) = self.on_prompt.borrow().clone() else {
            return;
        };
        let url = self
            .view
            .borrow()
            .as_ref()
            .and_then(|v| v.upgrade())
            .and_then(|view| view.uri())
            .map(|uri| uri.to_string())
            .unwrap_or_default();
        let (prompt, attachment) = repair_prompt(self.port, &url, &self.all());
        hook(prompt, attachment);
    }

    /// Give a new content manager the watcher and its handler. Once per
    /// manager: a tab opened from another shares that one's.
    pub fn install(manager: &webkit6::UserContentManager) {
        let source = WATCHER
            .replace("KEPT", &KEPT.to_string())
            .replace("HANDLER", HANDLER);
        manager.add_script(&webkit6::UserScript::for_world(
            &source,
            webkit6::UserContentInjectedFrames::TopFrame,
            webkit6::UserScriptInjectionTime::Start,
            WORLD,
            &[],
            &[],
        ));
        manager.register_script_message_handler(HANDLER, Some(WORLD));
    }

    /// Follow `view`: cleared as each page starts, its status read when it
    /// finishes, and its list read whenever the watcher pings.
    pub fn watch(self: &Rc<Self>, view: &webkit6::WebView) {
        *self.view.borrow_mut() = Some(view.downgrade());
        if let Some(inspector) = view.inspector() {
            // Opened, docked or in a window of its own; and closed.
            let weak = Rc::downgrade(self);
            inspector.connect_attach(move |_| {
                if let Some(problems) = weak.upgrade() {
                    problems.follow_inspector(true);
                }
                false
            });
            let weak = Rc::downgrade(self);
            inspector.connect_open_window(move |_| {
                if let Some(problems) = weak.upgrade() {
                    problems.follow_inspector(true);
                }
                false
            });
            let weak = Rc::downgrade(self);
            inspector.connect_closed(move |_| {
                if let Some(problems) = weak.upgrade() {
                    problems.follow_inspector(false);
                }
            });
        }
        {
            let weak = Rc::downgrade(self);
            view.connect_load_changed(move |view, event| {
                let Some(problems) = weak.upgrade() else {
                    return;
                };
                match event {
                    webkit6::LoadEvent::Started => {
                        problems.found.borrow_mut().clear();
                        problems.status.set(None);
                        problems.show();
                    }
                    webkit6::LoadEvent::Finished => {
                        let status = view
                            .main_resource()
                            .and_then(|resource| resource.response())
                            .map(|response| response.status_code())
                            .filter(|status| *status >= 400);
                        problems.status.set(status);
                        problems.read();
                    }
                    _ => {}
                }
            });
        }
        if let Some(manager) = view.user_content_manager() {
            let weak = Rc::downgrade(self);
            manager.connect_script_message_received(Some(HANDLER), move |_, _| {
                if let Some(problems) = weak.upgrade() {
                    problems.read();
                }
            });
        }
    }

    /// Read the watcher's list from this page's own view.
    fn read(self: &Rc<Self>) {
        if self.reading.replace(true) {
            return;
        }
        let Some(view) = self.view.borrow().as_ref().and_then(|v| v.upgrade()) else {
            self.reading.set(false);
            return;
        };
        let weak = Rc::downgrade(self);
        view.evaluate_javascript(
            READ,
            Some(WORLD),
            None,
            gtk::gio::Cancellable::NONE,
            move |result| {
                let Some(problems) = weak.upgrade() else {
                    return;
                };
                problems.reading.set(false);
                // A page that is gone, or a frame the read did not reach,
                // leaves the list as it was.
                if let Ok(value) = result {
                    *problems.found.borrow_mut() = parse(&value.to_str());
                    problems.show();
                }
            },
        );
    }

    /// The count on the button, and the rows behind it.
    /// Every problem, the page's status first, each with what is known of
    /// why — what the list shows and what Prompt Agent hands over.
    fn all(&self) -> Vec<(Problem, String)> {
        let mut all: Vec<Problem> = Vec::new();
        if let Some(status) = self.status.get() {
            all.push(Problem {
                kind: Kind::Status,
                text: format!("The page answered HTTP {status}"),
            });
        }
        all.extend(self.found.borrow().iter().cloned());
        all.into_iter()
            .map(|problem| {
                let detail = match problem.kind {
                    Kind::Resource => why(&problem.text, &self.forwarded).unwrap_or_else(|| {
                        "Did not load — the Web Inspector's Network tab says why".into()
                    }),
                    Kind::Script => "A script error nothing caught".into(),
                    Kind::Status => "The server's own answer, drawn as it sent it".into(),
                };
                (problem, detail)
            })
            .collect()
    }

    fn show(&self) {
        let all = self.all();
        self.button.set_visible(!all.is_empty());
        self.count.set_label(&all.len().to_string());
        self.button.set_tooltip_text(Some(&match all.len() {
            1 => "A problem loading this page".to_string(),
            n => format!("{n} problems loading this page"),
        }));
        while let Some(row) = self.rows.first_child() {
            self.rows.remove(&row);
        }
        for (problem, detail) in &all {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
            row.append(
                &gtk::Label::builder()
                    .label(&problem.text)
                    .xalign(0.0)
                    .wrap(true)
                    .wrap_mode(gtk::pango::WrapMode::WordChar)
                    .max_width_chars(48)
                    .lines(3)
                    .ellipsize(gtk::pango::EllipsizeMode::Middle)
                    .selectable(true)
                    // Selectable for copying, but not where the keyboard
                    // lands when the list opens: a focused label selects
                    // itself whole, and the button is what the list is for.
                    .focusable(false)
                    .build(),
            );
            row.append(
                &gtk::Label::builder()
                    .label(detail)
                    .css_classes(["caption", "dim-label"])
                    .xalign(0.0)
                    .wrap(true)
                    .max_width_chars(48)
                    .build(),
            );
            self.rows.append(&row);
        }
    }

    /// TASTE_PROBE_CHECK only: the list open, once the page has had time to
    /// fail at what it fails at.
    #[doc(hidden)]
    pub fn open_for_probe(&self) {
        self.button.popup();
    }
}

/// What Prompt Agent sends: the page and its problems, the list attached
/// with what the IDE knows of each one's cause.
fn repair_prompt(
    port: u16,
    url: &str,
    problems: &[(Problem, String)],
) -> (String, (String, String)) {
    let file = format!("port-{port}-problems.txt");
    let list = problems
        .iter()
        .map(|(problem, why)| {
            let kind = match problem.kind {
                Kind::Resource => "did not load",
                Kind::Script => "script error",
                Kind::Status => "page status",
            };
            format!("- {kind}: {}\n  why: {why}", problem.text)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let page = if url.is_empty() {
        format!("The page served on port {port}")
    } else {
        format!("The page {url}, served on port {port},")
    };
    let n = problems.len();
    let prompt = format!(
        "{page} had {n} problem{} loading in the IDE's browser.\n\n\
         WHAT IS TRUE\n\
         The server listens on port {port} in this environment. The problems are attached as \
         {file}: files that did not load, script errors nothing caught, and the page's own \
         error status, each with what the IDE knows of its cause.\n\n\
         HOW TO WORK\n\
         1. Read the attached list before changing anything.\n\
         2. Where a cause is named (0.0.0.0, a port not forwarded, a host this browser cannot \
         reach), fix the project's configuration: the address the server gives out, or \
         forwardPorts in .devcontainer/devcontainer.json, then call devcontainer_reload.\n\
         3. Otherwise reproduce it here: request the failing addresses with curl through \
         ide_exec, and read the server's output with devcontainer_logs. You cannot see the \
         browser's console or network log; ide_screenshot of the editor shows the page while \
         its tab is in front.\n\
         4. Fix the cause in the project, request each failing address again, and check it \
         now loads.\n\
         5. If the fix needs something only the user can give, stop and say exactly what.\n\
         6. Finish with one short paragraph: the cause, and what you changed.",
        if n == 1 { "" } else { "s" },
    );
    (prompt, (file, list))
}

/// The watcher's list, as JSON; anything else is no list.
fn parse(json: &str) -> Vec<Problem> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let kind = match item["kind"].as_str()? {
                "resource" => Kind::Resource,
                "script" => Kind::Script,
                _ => return None,
            };
            Some(Problem {
                kind,
                text: item["text"].as_str()?.to_string(),
            })
        })
        .take(KEPT)
        .collect()
}

/// Why a file at `url` did not load, where that is the IDE's doing or
/// WebKit's rather than the server's: `None` when it is not known here.
fn why(url: &str, forwarded: &[u16]) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (host, after) = v6.split_once(']')?;
            (format!("[{host}]"), after.strip_prefix(':'))
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), Some(port)),
            None => (authority.to_string(), None),
        },
    };
    let host = host.to_ascii_lowercase();
    let port = match port {
        Some(port) => port.parse::<u16>().ok()?,
        None if scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("wss") => 443,
        None => 80,
    };
    if host == "0.0.0.0" {
        return Some(
            "This browser refuses 0.0.0.0. Have the server name localhost instead — Vite takes \
             it from server.hmr.host"
                .into(),
        );
    }
    let loopback = host == "localhost" || host == "[::1]" || host.starts_with("127.");
    if loopback {
        return (!forwarded.contains(&port)).then(|| {
            format!("Port {port} is not forwarded from this environment, so this browser does not reach it")
        });
    }
    let octets: Vec<u8> = host.split('.').filter_map(|o| o.parse().ok()).collect();
    let private = match octets[..] {
        [10, ..] | [192, 168, ..] | [169, 254, ..] | [0, ..] => true,
        [172, b, ..] => (16..=31).contains(&b),
        [100, b, ..] => (64..=127).contains(&b),
        _ => false,
    };
    let local_name = !host.contains('.')
        || [".local", ".lan", ".internal", ".home.arpa", ".localhost"]
            .iter()
            .any(|suffix| host.ends_with(suffix));
    (private || local_name || host.starts_with('[')).then(|| {
        "This browser reaches the environment's ports, not this machine's network or the LAN".into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prompt Agent names the page and its port, and attaches every
    /// problem with its cause, under a name that says what it is.
    #[test]
    fn the_prompt_attaches_every_problem_with_its_cause() {
        let problems = vec![
            (
                Problem {
                    kind: Kind::Status,
                    text: "The page answered HTTP 500".into(),
                },
                "The server's own answer, drawn as it sent it".into(),
            ),
            (
                Problem {
                    kind: Kind::Resource,
                    text: "http://0.0.0.0:5173/resources/sass/app.scss".into(),
                },
                "This browser refuses 0.0.0.0.".into(),
            ),
        ];
        let (prompt, (file, list)) = repair_prompt(8000, "http://localhost:41234/", &problems);
        assert!(prompt
            .starts_with("The page http://localhost:41234/, served on port 8000, had 2 problems"));
        assert!(prompt.contains(&file));
        assert_eq!(file, "port-8000-problems.txt");
        assert!(list.contains("- page status: The page answered HTTP 500"));
        assert!(list.contains("- did not load: http://0.0.0.0:5173/resources/sass/app.scss\n  why: This browser refuses 0.0.0.0."));
    }

    /// The reason a row gives is the one the IDE or WebKit is behind, and
    /// none at all for what only the server could say.
    #[test]
    fn a_file_that_did_not_load_says_why_where_the_ide_knows() {
        let forwarded = [5173, 8000];
        assert!(
            why("http://0.0.0.0:5173/resources/sass/app.scss", &forwarded)
                .unwrap()
                .contains("refuses 0.0.0.0")
        );
        assert_eq!(
            why("http://localhost:6000/e.css", &forwarded).as_deref(),
            Some("Port 6000 is not forwarded from this environment, so this browser does not reach it")
        );
        assert!(why("http://127.0.0.1/x.png", &forwarded)
            .unwrap()
            .contains("Port 80 "));
        assert_eq!(why("http://localhost:5173/@vite/client", &forwarded), None);
        assert_eq!(why("http://[::1]:8000/app.js", &forwarded), None);
        for blocked in [
            "http://192.168.1.4/x.js",
            "https://printer/x.js",
            "http://nas.local:5000/x",
            "http://[fe80::1]/x",
            "http://172.20.0.3/x",
        ] {
            assert!(
                why(blocked, &forwarded)
                    .unwrap()
                    .contains("not this machine's network"),
                "{blocked}"
            );
        }
        assert_eq!(
            why("https://cdn.jsdelivr.net/npm/flatpickr.css", &forwarded),
            None
        );
        assert_eq!(why("data:image/png;base64,xyz", &forwarded), None);
    }

    #[test]
    fn the_watchers_list_reads_back_and_nothing_else_does() {
        let found = parse(
            r#"[{"kind":"resource","text":"http://0.0.0.0:5173/a.css"},
                {"kind":"script","text":"x is not defined"},
                {"kind":"other","text":"ignored"},
                {"kind":"script"}]"#,
        );
        assert_eq!(
            found,
            [
                Problem {
                    kind: Kind::Resource,
                    text: "http://0.0.0.0:5173/a.css".into()
                },
                Problem {
                    kind: Kind::Script,
                    text: "x is not defined".into()
                },
            ]
        );
        assert!(parse("undefined").is_empty());
        assert!(parse("{}").is_empty());
    }
}

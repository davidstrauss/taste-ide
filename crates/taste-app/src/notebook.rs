//! A Jupyter notebook's face in the editor: its cells drawn, its code run.
//!
//! The `.ipynb` file is the page's buffer, as every file is, and its Edit
//! face is that JSON. This is the other face — the notebook as a reader
//! sees it — drawn into the same preview holder a Markdown file's preview
//! uses, so it is double-buffered the same way and has the same overview
//! strip beside it (`crate::preview_map`; David, 2026-10-11: "I want
//! minimap support for Jupyter, too").
//!
//! **Running it is the environment's business.** Code cells run in a
//! kernel inside the notebook's environment's container (`crate::kernel`),
//! on the container's own Jupyter; a kernel that cannot start puts a
//! banner over the cells whose button hands what it said to that
//! environment's agent to fix in place (David: "If a Jupyter notebook
//! preview fails to start, give me a button in a banner for the editor to
//! prompt the agent to fix what the container needs in place"). The kernel
//! starts on the first Run, not on opening: looking at a notebook runs
//! nothing.
//!
//! **Outputs land in the file.** A cell's outputs and execution count are
//! written back into the buffer when its run finishes — the edit lands in
//! the undo stack, and saving keeps it, as Jupyter's own save would — and
//! a code cell's source, edited here, is written back the same way. Each
//! write is checked against the cell it meant: a cell whose source has
//! changed since it was run keeps its outputs on screen only.
//!
//! What is drawn of an output: PNG and JPEG pictures, Markdown, and text
//! (ANSI colour read, as the logs read it); HTML and SVG are named rather
//! than drawn, since drawing them means a renderer for content the project
//! chose, and a DataFrame's HTML has its text beside it, which is shown.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::glib;
use serde::Serialize;
use serde_json::Value;
use sourceview5::prelude::*;

use crate::kernel::{Event, Kernel};
use crate::markdown_view::{DocumentBase, Readiness};

/// The most lines of one text output drawn; the rest stay in the file.
const MAX_OUTPUT_LINES: usize = 400;
/// The most of one text output drawn, in bytes, whatever its lines.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// How long a code cell's typing settles before it is written back.
const SOURCE_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);
/// The most often a running cell's outputs are redrawn. A cell printing in
/// a loop sends thousands of messages a second, and redrawing on each one
/// is the main thread's whole time (20,000 flushed prints took 26.9s).
const REDRAW_EVERY: std::time::Duration = std::time::Duration::from_millis(50);

/// Where the kernel stands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Not started: nothing has been run.
    Off,
    Starting,
    Idle,
    Busy,
    Restarting,
    /// It did not start: what it said, and the report for the agent.
    Failed {
        message: String,
        report: String,
    },
    /// It stopped after starting.
    Stopped {
        report: String,
    },
}

/// One run of one cell, until its reply AND its idle: outputs can arrive
/// after the reply (a large final figure often does), and the protocol's
/// word for "nothing more" is the request's idle.
struct Run {
    index: usize,
    /// The cell's nbformat id, when it has one: what its outputs are
    /// written back to, whatever has moved it since.
    cell_id: Option<String>,
    code: String,
    outputs: Vec<Value>,
    count: Option<i64>,
    /// `clear_output` with `wait`: clear at the next output, not now.
    clear_on_next: bool,
    reply: Option<Value>,
    idle: bool,
}

/// A change to the notebook's JSON, queued and applied off the main thread
/// with the others queued beside it; it says whether it changed anything.
/// A `Fn`, so a batch the buffer changed under can be applied again.
type Edit = std::sync::Arc<dyn Fn(&mut Value) -> bool + Send + Sync>;

/// One drawn code cell's live parts.
struct CellParts {
    prompt: gtk::Label,
    outputs: gtk::Box,
    source: sourceview5::Buffer,
    view: sourceview5::View,
    /// Its nbformat id, when it has one.
    id: Option<String>,
    /// The source the file holds for it, as last drawn or written: a write
    /// of an edit lands only on a cell that still holds this.
    filed: Rc<RefCell<String>>,
    /// A source edit waiting for the typing to settle.
    pending: Rc<RefCell<Option<glib::SourceId>>>,
    /// The outputs drawn now, for an `update_display_data` after its run.
    shown: Rc<RefCell<Vec<Value>>>,
}

/// The parts of the current drawing that a kernel event changes.
struct Drawing {
    status: gtk::Label,
    run_all: gtk::Button,
    interrupt: gtk::Button,
    restart: gtk::Button,
    banner: KernelBar,
    cells: HashMap<usize, CellParts>,
    base: DocumentBase,
    on_link: Rc<dyn Fn(&str)>,
}

/// A notebook page's session: its kernel, the runs in flight, and the
/// drawing on screen. One per open notebook; dropped with its page, which
/// ends the kernel.
pub struct Session {
    path: PathBuf,
    /// The environment the notebook is in, whose container its kernel runs
    /// in.
    env: taste_core::environment::EnvironmentId,
    /// Where the kernel runs: that environment's container; `None` when
    /// the environment is gone, and the notebook is only looked at.
    exec: Option<taste_core::ExecContext>,
    /// Whether the notebook is the user's own, whose cells are edited and
    /// whose outputs are written into the file. Another environment's is
    /// run in its own container and its outputs drawn, and its file is left
    /// as it is: watching is reading (David, 2026-10-11: "I would want to
    /// be able to run kernels in other environments, actually").
    writable: bool,
    buffer: sourceview5::Buffer,
    /// Set while the session writes the buffer, so its own edit is not
    /// read back as the file changing under it.
    pub writing: Cell<bool>,
    kernel: RefCell<Option<Kernel>>,
    state: RefCell<State>,
    kernel_name: RefCell<String>,
    runs: RefCell<HashMap<String, Run>>,
    next_run: Cell<u64>,
    drawing: RefCell<Option<Drawing>>,
    on_prompt_agent: Rc<dyn Fn(String, (String, String))>,
    /// The kernel's widget models, and their views (`crate::widgets`).
    widgets: Rc<crate::widgets::Manager>,
    /// Cells whose outputs changed since they were last drawn, and whether
    /// a redraw of them is due.
    stale: RefCell<std::collections::BTreeSet<usize>>,
    redraw_due: Cell<bool>,
    /// The latest rendering asked for: an older one that finishes after it
    /// does not take the drawing over.
    generation: Cell<u64>,
    /// Edits to the file waiting to be applied, and whether a batch is
    /// being applied now.
    edits: RefCell<Vec<Edit>>,
    editing: Cell<bool>,
    /// How many times the buffer has changed: a batch computed off the main
    /// thread is applied only over the text it was computed from.
    changes: Rc<Cell<u64>>,
    /// Where each display id's output is: its cell and its place among the
    /// cell's outputs (`update_display_data`).
    displays: RefCell<HashMap<String, (usize, usize)>>,
    /// The session itself, weakly, for the work it schedules.
    this: RefCell<Weak<Session>>,
}

impl Session {
    /// A session for the notebook at `path`, whose bytes are `buffer`.
    /// `exec` is its environment's, or `None` for a notebook that is only
    /// looked at; `on_prompt_agent` hands a prompt and its attachment to
    /// that environment's agent.
    pub fn new(
        path: PathBuf,
        env: taste_core::environment::EnvironmentId,
        exec: Option<taste_core::ExecContext>,
        writable: bool,
        buffer: sourceview5::Buffer,
        on_prompt_agent: Rc<dyn Fn(String, (String, String))>,
    ) -> Rc<Self> {
        let state = match std::env::var("TASTE_PROBE_NOTEBOOK").as_deref() {
            // A kernel that did not start, which otherwise needs a
            // container without Jupyter.
            Ok("failed") => State::Failed {
                // With an apostrophe in it: the old banner read its title as
                // markup and showed this one as `&apos;`.
                message: "the kernel's bridge exited before the kernel started: no Python in \
                          this environment can import jupyter_client (tried .venv/bin/python, \
                          venv/bin/python, python3, and python)"
                    .into(),
                report: String::new(),
            },
            _ => State::Off,
        };
        let session = Rc::new(Self {
            path,
            env,
            exec,
            writable,
            buffer,
            writing: Cell::new(false),
            kernel: RefCell::new(None),
            state: RefCell::new(state),
            kernel_name: RefCell::new(String::new()),
            runs: RefCell::new(HashMap::new()),
            next_run: Cell::new(0),
            drawing: RefCell::new(None),
            on_prompt_agent,
            widgets: crate::widgets::Manager::new(),
            stale: RefCell::new(std::collections::BTreeSet::new()),
            redraw_due: Cell::new(false),
            generation: Cell::new(0),
            edits: RefCell::new(Vec::new()),
            editing: Cell::new(false),
            changes: Rc::new(Cell::new(0)),
            displays: RefCell::new(HashMap::new()),
            this: RefCell::new(Weak::new()),
        });
        *session.this.borrow_mut() = Rc::downgrade(&session);
        {
            let changes = session.changes.clone();
            session
                .buffer
                .connect_changed(move |_| changes.set(changes.get() + 1));
        }
        // A widget's change reaches its model through the kernel's bridge.
        let weak = Rc::downgrade(&session);
        session
            .widgets
            .set_sender(Rc::new(move |comm_id, data, buffers| {
                if let Some(session) = weak.upgrade() {
                    if let Some(kernel) = session.kernel.borrow().as_ref() {
                        kernel.comm_msg(comm_id, data, &buffers);
                    }
                }
            }));
        session
    }

    /// Whether its cells can be run: there is a container to run them in.
    fn runnable(&self) -> bool {
        self.exec.is_some()
    }

    /// Whether its cells are edited and its outputs written into the file.
    fn editable(&self) -> bool {
        self.writable && self.runnable()
    }

    /// The environment it runs in.
    pub fn env(&self) -> &taste_core::environment::EnvironmentId {
        &self.env
    }

    /// The notebook's file name, for the Tasks entry.
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// How its kernel stands, and what it said when it did not start.
    pub fn kernel_status(&self) -> (crate::kernels::KernelState, Option<String>) {
        use crate::kernels::KernelState as K;
        match &*self.state.borrow() {
            State::Off | State::Stopped { .. } => (K::Off, None),
            State::Starting | State::Restarting => (K::Starting, None),
            State::Idle => (K::Idle, None),
            State::Busy => (K::Busy, None),
            State::Failed { message, .. } => (K::Failed, Some(message.clone())),
        }
    }

    /// Start its kernel, afresh after a failure or a stop; nothing when it
    /// is running.
    pub fn start_kernel(self: &Rc<Self>) {
        if matches!(
            *self.state.borrow(),
            State::Failed { .. } | State::Stopped { .. }
        ) {
            self.kernel.borrow_mut().take();
            self.set_state(State::Off);
        }
        self.ensure_kernel();
    }

    /// Stop its kernel: the bridge's input ended, which shuts it down in
    /// the container, and the runs in flight given up.
    pub fn stop_kernel(&self) {
        if self.kernel.borrow_mut().take().is_none()
            && !matches!(*self.state.borrow(), State::Failed { .. })
        {
            return;
        }
        self.abandon_runs();
        self.set_state(State::Off);
    }

    /// Draw `text` (the notebook's JSON). Returned at once, empty, with
    /// what it still has to do — the file parsed off this thread, its
    /// pictures decoded — for the editor's double buffer to wait on.
    pub fn render(
        self: &Rc<Self>,
        text: String,
        base: DocumentBase,
        on_link: Rc<dyn Fn(&str)>,
    ) -> (gtk::Widget, Readiness) {
        let readiness = Readiness::default();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let hold = readiness.hold();
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let weak = Rc::downgrade(self);
        let root_weak = root.downgrade();
        let ready = readiness.clone();
        glib::spawn_future_local(async move {
            let _hold = hold;
            let parsed = crate::runtime::runtime()
                .spawn_blocking(move || serde_json::from_str::<Value>(&text))
                .await;
            let (Some(session), Some(root)) = (weak.upgrade(), root_weak.upgrade()) else {
                return;
            };
            match parsed {
                Ok(Ok(notebook)) => {
                    session.build(&root, &notebook, base, on_link, &ready, generation)
                }
                Ok(Err(e)) => root.append(&notice(&format!(
                    "This notebook's JSON could not be read: {e}. The Edit face shows it as it \
                     is."
                ))),
                Err(e) => root.append(&notice(&format!("Reading the notebook failed: {e}"))),
            }
        });
        (root.upcast(), readiness)
    }

    fn build(
        self: &Rc<Self>,
        root: &gtk::Box,
        notebook: &Value,
        base: DocumentBase,
        on_link: Rc<dyn Fn(&str)>,
        ready: &Readiness,
        generation: u64,
    ) {
        let major = notebook["nbformat"].as_u64().unwrap_or(0);
        if major != 4 {
            root.append(&notice(&format!(
                "This is an nbformat {major} notebook, and only nbformat 4 is drawn; the Edit \
                 face shows it as it is."
            )));
            return;
        }
        let metadata = &notebook["metadata"];
        *self.kernel_name.borrow_mut() = metadata["kernelspec"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let language = metadata["language_info"]["name"]
            .as_str()
            .or_else(|| metadata["kernelspec"]["language"].as_str())
            .unwrap_or("python")
            .to_lowercase();
        let display = metadata["kernelspec"]["display_name"]
            .as_str()
            .unwrap_or("Kernel")
            .to_string();
        // A notebook saved with its widgets' state draws them from it until
        // a kernel opens their models.
        self.widgets
            .load_saved(&metadata["widgets"]["application/vnd.jupyter.widget-state+json"]["state"]);
        {
            let (base, on_link) = (base.clone(), on_link.clone());
            let widgets = Rc::downgrade(&self.widgets);
            self.widgets.set_output_drawer(Rc::new(move |output| {
                let widgets = widgets.upgrade();
                output_widget(output, &base, &on_link, None, widgets.as_ref())
            }));
        }

        // The kernel's bar: which kernel, and the three things done to it.
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.add_css_class("notebook-bar");
        // On the cells' own inset, so the bar's words start where theirs do.
        bar.set_margin_start(16);
        bar.set_margin_end(10);
        bar.set_margin_top(4);
        // The kernel it runs on, by name; how it stands is the Tasks
        // entry's to say, and the bar above's when it is not running.
        let status = gtk::Label::builder()
            .label(&display)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption", "dim-label"])
            .build();
        status.set_tooltip_text(Some(&display));
        let button = |icon: &str, tip: &str| {
            gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(tip)
                .css_classes(["flat"])
                .valign(gtk::Align::Center)
                .build()
        };
        let run_all = button("media-playback-start-symbolic", "Run All");
        let interrupt = button("media-playback-stop-symbolic", "Interrupt the kernel");
        let restart = button("view-refresh-symbolic", "Restart the kernel");
        bar.append(&status);
        bar.append(&run_all);
        bar.append(&interrupt);
        bar.append(&restart);
        let banner = KernelBar::new();

        let column = gtk::Box::new(gtk::Orientation::Vertical, 14);
        column.set_margin_top(16);
        column.set_margin_bottom(16);
        column.set_margin_start(16);
        column.set_margin_end(16);
        let mut cells = HashMap::new();
        let empty = Vec::new();
        let cell_list = notebook["cells"].as_array().unwrap_or(&empty);
        for (index, cell) in cell_list.iter().enumerate() {
            let source = joined(&cell["source"]);
            match cell["cell_type"].as_str() {
                Some("markdown") => {
                    let drawn = crate::markdown_view::render_in(
                        &source,
                        on_link.clone(),
                        None,
                        Some(base.clone()),
                    );
                    for side in [
                        gtk::PositionType::Top,
                        gtk::PositionType::Bottom,
                        gtk::PositionType::Left,
                        gtk::PositionType::Right,
                    ] {
                        set_margin(&drawn, side, 0);
                    }
                    column.append(&drawn);
                }
                Some("code") => {
                    let parts = self.code_cell(index, &source, &language, cell);
                    column.append(&parts.0);
                    let parts = parts.1;
                    // A run in flight keeps what it has said so far, across
                    // a redraw; otherwise the file's outputs.
                    let live = self
                        .runs
                        .borrow()
                        .values()
                        .find(|run| run.index == index)
                        .map(|run| run.outputs.clone());
                    let outputs = live
                        .clone()
                        .unwrap_or_else(|| cell["outputs"].as_array().cloned().unwrap_or_default());
                    for output in &outputs {
                        if let Some(widget) =
                            output_widget(output, &base, &on_link, Some(ready), Some(&self.widgets))
                        {
                            parts.outputs.append(&widget);
                        }
                    }
                    parts
                        .outputs
                        .set_visible(parts.outputs.first_child().is_some());
                    *parts.shown.borrow_mut() = outputs.clone();
                    parts.prompt.set_label(&if live.is_some() {
                        "[*]".to_string()
                    } else {
                        prompt_text(cell["execution_count"].as_i64())
                    });
                    cells.insert(index, parts);
                }
                _ => {
                    column.append(
                        &gtk::Label::builder()
                            .label(&source)
                            .xalign(0.0)
                            .wrap(true)
                            .wrap_mode(gtk::pango::WrapMode::WordChar)
                            .selectable(true)
                            .css_classes(["monospace", "dim-label"])
                            .build(),
                    );
                }
            }
        }
        if cell_list.is_empty() {
            column.append(&notice("This notebook has no cells."));
        }
        root.append(&banner.widget);
        root.append(&bar);
        root.append(&column);

        {
            let weak = Rc::downgrade(self);
            run_all.connect_clicked(move |_| {
                if let Some(session) = weak.upgrade() {
                    session.run_all();
                }
            });
        }
        {
            let weak = Rc::downgrade(self);
            interrupt.connect_clicked(move |_| {
                if let Some(session) = weak.upgrade() {
                    if let Some(kernel) = session.kernel.borrow().as_ref() {
                        kernel.interrupt();
                    }
                }
            });
        }
        {
            let weak = Rc::downgrade(self);
            restart.connect_clicked(move |_| {
                if let Some(session) = weak.upgrade() {
                    session.restart();
                }
            });
        }
        {
            let weak = Rc::downgrade(self);
            banner.prompt.connect_clicked(move |_| {
                if let Some(session) = weak.upgrade() {
                    session.prompt_agent();
                }
            });
        }
        {
            let weak = Rc::downgrade(self);
            banner.start.connect_clicked(move |_| {
                if let Some(session) = weak.upgrade() {
                    session.start_kernel();
                }
            });
        }
        // Overtaken by a newer rendering while this one was parsed: it is
        // drawn, and dropped by the editor, and it is not the one runs draw
        // into.
        if generation != self.generation.get() {
            return;
        }
        self.displays.borrow_mut().clear();
        *self.drawing.borrow_mut() = Some(Drawing {
            status,
            run_all,
            interrupt,
            restart,
            banner,
            cells,
            base,
            on_link,
        });
        self.show_state();
    }

    /// A code cell: its run button and prompt in the gutter, its source
    /// editable beside them, and its outputs under the source.
    fn code_cell(
        self: &Rc<Self>,
        index: usize,
        source: &str,
        language: &str,
        cell: &Value,
    ) -> (gtk::Widget, CellParts) {
        let buffer = sourceview5::Buffer::new(None);
        buffer.set_text(source.strip_suffix('\n').unwrap_or(source));
        if let Some(lang) = sourceview5::LanguageManager::default().language(language) {
            buffer.set_language(Some(&lang));
        }
        crate::editor::apply_scheme_for_style(&buffer);
        buffer.set_highlight_matching_brackets(false);
        let view = sourceview5::View::builder()
            .buffer(&buffer)
            .monospace(true)
            .editable(self.editable())
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(10)
            .right_margin(10)
            .hexpand(true)
            .build();
        let frame = gtk::Frame::builder().child(&view).build();
        frame.add_css_class("view");

        let run = gtk::Button::builder()
            .icon_name("media-playback-start-symbolic")
            .tooltip_text("Run this cell (Shift+Enter)")
            .css_classes(["flat", "circular"])
            .halign(gtk::Align::Center)
            .sensitive(self.runnable())
            .build();
        let prompt = gtk::Label::builder()
            .css_classes(["caption", "dim-label", "monospace"])
            .halign(gtk::Align::Center)
            .build();
        let gutter = gtk::Box::new(gtk::Orientation::Vertical, 2);
        gutter.set_width_request(48);
        gutter.append(&run);
        gutter.append(&prompt);

        let outputs = gtk::Box::new(gtk::Orientation::Vertical, 6);
        outputs.set_margin_top(6);
        // Where the source's text starts: the frame's border and the view's
        // left margin, so an output's first character stands under the
        // code's.
        outputs.set_margin_start(11);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.set_hexpand(true);
        body.append(&frame);
        body.append(&outputs);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.append(&gutter);
        row.append(&body);

        {
            let weak = Rc::downgrade(self);
            run.connect_clicked(move |_| {
                if let Some(session) = weak.upgrade() {
                    session.run_cell(index);
                }
            });
        }
        // Shift+Enter runs the cell and moves to the next; Ctrl+Enter runs
        // it in place — Jupyter's two keys.
        {
            let keys = gtk::EventControllerKey::new();
            keys.set_propagation_phase(gtk::PropagationPhase::Capture);
            let weak = Rc::downgrade(self);
            keys.connect_key_pressed(move |_, key, _, modifiers| {
                let enter = matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter);
                let shift = modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK);
                let ctrl = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK);
                if !enter || !(shift || ctrl) {
                    return glib::Propagation::Proceed;
                }
                let Some(session) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                session.run_cell(index);
                if shift {
                    session.focus_after(index);
                }
                glib::Propagation::Stop
            });
            view.add_controller(keys);
        }
        // Edited here, written back once the typing settles — or at once,
        // when the cell is run before it has (`run_cell`).
        let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        if self.editable() {
            let pending = pending.clone();
            let weak: Weak<Session> = Rc::downgrade(self);
            buffer.connect_changed(move |_| {
                if let Some(id) = pending.borrow_mut().take() {
                    id.remove();
                }
                let weak = weak.clone();
                let slot = pending.clone();
                let id = glib::timeout_add_local_once(SOURCE_SETTLE, move || {
                    slot.borrow_mut().take();
                    if let Some(session) = weak.upgrade() {
                        session.write_source(index);
                    }
                });
                *pending.borrow_mut() = Some(id);
            });
        }
        (
            row.upcast(),
            CellParts {
                prompt,
                outputs,
                source: buffer,
                view,
                id: cell["id"].as_str().map(str::to_string),
                filed: Rc::new(RefCell::new(joined(&cell["source"]))),
                pending,
                shown: Rc::new(RefCell::new(Vec::new())),
            },
        )
    }

    /// Put the cursor in the next code cell after `index`, if there is one.
    fn focus_after(&self, index: usize) {
        let drawing = self.drawing.borrow();
        let Some(drawing) = drawing.as_ref() else {
            return;
        };
        let mut later: Vec<&usize> = drawing.cells.keys().filter(|i| **i > index).collect();
        later.sort();
        if let Some(next) = later.first().and_then(|i| drawing.cells.get(i)) {
            next.view.grab_focus();
        }
    }

    /// Start the kernel if it is not running. `false` when it cannot be.
    fn ensure_kernel(self: &Rc<Self>) -> bool {
        if self.kernel.borrow().is_some() {
            return true;
        }
        let Some(exec) = self.exec.clone() else {
            return false;
        };
        let cwd = self
            .path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let weak = Rc::downgrade(self);
        let name = self.kernel_name.borrow().clone();
        match Kernel::start(&exec, &name, &cwd, move |event| {
            if let Some(session) = weak.upgrade() {
                session.on_event(event);
            }
        }) {
            Ok(kernel) => {
                *self.kernel.borrow_mut() = Some(kernel);
                self.set_state(State::Starting);
                true
            }
            Err(e) => {
                self.set_state(State::Failed {
                    message: format!("{e:#}"),
                    report: format!("{e:#}"),
                });
                false
            }
        }
    }

    /// Run code cell `index` as its source stands on screen.
    pub fn run_cell(self: &Rc<Self>, index: usize) {
        // An edit still settling is filed first, so the outputs written at
        // the end of this run land beside the code that made them.
        self.write_source(index);
        let (code, cell_id) = {
            let drawing = self.drawing.borrow();
            let Some(parts) = drawing.as_ref().and_then(|d| d.cells.get(&index)) else {
                return;
            };
            (buffer_text(&parts.source), parts.id.clone())
        };
        if !self.ensure_kernel() {
            return;
        }
        let token = format!("run-{}", self.next_run.get());
        self.next_run.set(self.next_run.get() + 1);
        self.runs.borrow_mut().retain(|_, run| run.index != index);
        self.runs.borrow_mut().insert(
            token.clone(),
            Run {
                index,
                cell_id,
                code: code.clone(),
                outputs: Vec::new(),
                count: None,
                clear_on_next: false,
                reply: None,
                idle: false,
            },
        );
        if let Some(parts) = self
            .drawing
            .borrow()
            .as_ref()
            .and_then(|d| d.cells.get(&index))
        {
            parts.prompt.set_label("[*]");
            clear(&parts.outputs);
        }
        if let Some(kernel) = self.kernel.borrow().as_ref() {
            kernel.execute(&token, &code);
        }
    }

    /// `TASTE_PROBE_NOTEBOOK=drag`: run every cell, wait for the kernel to
    /// settle, then drag the first slider the outputs hold from where it
    /// is to its end at sixty changes a second — as a pointer drag sets it
    /// — and print what the widgets did: the changes made, the updates the
    /// throttle let through, the echoes set aside, and where the slider's
    /// model and the Output widgets ended up. The real path end to end, in
    /// a container that has Jupyter, which is the environment when the IDE
    /// runs inside one.
    pub fn probe_drag(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let pause = |ms| glib::timeout_future(std::time::Duration::from_millis(ms));
            // The drawing first, then the run.
            for _ in 0..100 {
                if weak.upgrade().is_some_and(|s| s.drawing.borrow().is_some()) {
                    break;
                }
                pause(100).await;
            }
            let Some(session) = weak.upgrade() else {
                return;
            };
            session.run_all();
            drop(session);
            let started = std::time::Instant::now();
            loop {
                pause(250).await;
                let Some(session) = weak.upgrade() else {
                    return;
                };
                let state = session.state.borrow().clone();
                if let State::Failed { message, .. } = state {
                    println!("probe notebook: the kernel did not start: {message}");
                    return;
                }
                if session.runs.borrow().is_empty() && state == State::Idle {
                    break;
                }
                if started.elapsed() > std::time::Duration::from_secs(90) {
                    println!("probe notebook: the run did not finish in 90s ({state:?})");
                    return;
                }
            }
            println!(
                "probe notebook: ran in {:.1}s",
                started.elapsed().as_secs_f64()
            );
            let Some(session) = weak.upgrade() else {
                return;
            };
            let scale = {
                let drawing = session.drawing.borrow();
                let mut cells: Vec<_> = drawing
                    .as_ref()
                    .map(|d| {
                        d.cells
                            .iter()
                            .map(|(i, p)| (*i, p.outputs.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                cells.sort_by_key(|(i, _)| *i);
                cells
                    .into_iter()
                    .find_map(|(_, outputs)| first_scale(outputs.upcast_ref()))
            };
            let Some(scale) = scale else {
                println!("probe notebook: no slider in the outputs");
                return;
            };
            let sent_before = session.widgets.updates_sent.get();
            let adjustment = scale.adjustment();
            let (from, to) = (adjustment.value(), adjustment.upper());
            let steps = 60;
            drop(session);
            for step in 1..=steps {
                scale.set_value(from + (to - from) * step as f64 / steps as f64);
                pause(16).await;
            }
            // In view for the shot: the preview scrolled to the slider.
            if let Some(scroller) = scale
                .ancestor(gtk::ScrolledWindow::static_type())
                .and_downcast::<gtk::ScrolledWindow>()
            {
                if let Some(content) = scroller.child() {
                    if let Some(point) =
                        scale.compute_point(&content, &gtk::graphene::Point::new(0.0, 0.0))
                    {
                        scroller
                            .vadjustment()
                            .set_value((point.y() as f64 - 60.0).max(0.0));
                    }
                }
            }
            // The throttle's last send, the kernel's redraw, and its echo.
            pause(4000).await;
            let Some(session) = weak.upgrade() else {
                return;
            };
            let widgets = &session.widgets;
            let sliders = widgets.models_named("IntSliderModel");
            let outputs = widgets.models_named("OutputModel");
            println!(
                "probe notebook: dragged {from} -> {to} in {steps} changes; {} updates sent; \
                 {} echoes set aside",
                widgets.updates_sent.get() - sent_before,
                widgets.echoes_set_aside.get()
            );
            for slider in &sliders {
                println!(
                    "probe notebook: slider model value {}; on screen {}",
                    crate::widgets::Manager::state_of(slider, "value"),
                    scale.value()
                );
            }
            for output in &outputs {
                let kinds: Vec<String> = crate::widgets::Manager::state_of(output, "outputs")
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|o| {
                        let data: Vec<&str> = o["data"]
                            .as_object()
                            .map(|d| d.keys().map(String::as_str).collect())
                            .unwrap_or_default();
                        format!("{}{:?}", o["output_type"].as_str().unwrap_or("?"), data)
                    })
                    .collect();
                println!("probe notebook: an Output widget holds {kinds:?}");
            }
        });
    }

    fn run_all(self: &Rc<Self>) {
        let mut indices: Vec<usize> = self
            .drawing
            .borrow()
            .as_ref()
            .map(|d| d.cells.keys().copied().collect())
            .unwrap_or_default();
        indices.sort();
        for index in indices {
            self.run_cell(index);
        }
    }

    fn restart(self: &Rc<Self>) {
        // A kernel that never started, or stopped, is started afresh.
        if matches!(
            *self.state.borrow(),
            State::Failed { .. } | State::Stopped { .. }
        ) {
            self.kernel.borrow_mut().take();
            self.set_state(State::Off);
            self.ensure_kernel();
            return;
        }
        if let Some(kernel) = self.kernel.borrow().as_ref() {
            kernel.restart();
        }
        self.abandon_runs();
    }

    /// Every run in flight given up: their prompts back to unrun.
    fn abandon_runs(&self) {
        let runs: Vec<Run> = self.runs.borrow_mut().drain().map(|(_, r)| r).collect();
        let drawing = self.drawing.borrow();
        for run in runs {
            if let Some(parts) = drawing.as_ref().and_then(|d| d.cells.get(&run.index)) {
                parts.prompt.set_label(&prompt_text(None));
            }
        }
    }

    fn on_event(self: &Rc<Self>, event: Event) {
        match event {
            Event::Ready { display_name } => {
                if let Some(drawing) = self.drawing.borrow().as_ref() {
                    drawing.status.set_label(&display_name);
                    drawing.status.set_tooltip_text(Some(&display_name));
                }
                self.set_state(State::Idle);
            }
            Event::Failed { message, report } => {
                self.kernel.borrow_mut().take();
                self.abandon_runs();
                self.set_state(State::Failed { message, report });
            }
            Event::State { state, cell, .. } => match state.as_str() {
                "busy" => self.set_state(State::Busy),
                "idle" => {
                    self.set_state(State::Idle);
                    if let Some(token) = cell {
                        self.on_idle(&token);
                    }
                }
                "restarting" => self.set_state(State::Restarting),
                "dead" => {
                    // Gone: the next Run starts another, rather than sending
                    // to one that will never answer.
                    self.kernel.borrow_mut().take();
                    self.abandon_runs();
                    self.set_state(State::Stopped {
                        report: "The kernel died.".into(),
                    });
                }
                _ => {}
            },
            Event::Error { message } => {
                tracing::warn!("notebook kernel: {message}");
                if let Some(drawing) = self.drawing.borrow().as_ref() {
                    drawing.status.set_tooltip_text(Some(&message));
                }
            }
            Event::Output {
                cell,
                parent,
                kind,
                content,
            } => {
                // A display updated by its id, wherever it was drawn.
                if kind == "update_display_data" {
                    self.update_display(&content);
                    return;
                }
                // An Output widget capturing this request takes its output
                // first — a callback's, or a cell's own under `with out:`.
                if self.widgets.capture(&parent, &kind, &content) {
                    return;
                }
                if let Some(cell) = cell {
                    self.on_output(&cell, &kind, content);
                }
            }
            Event::Comm {
                kind,
                content,
                buffers,
            } => self.widgets.on_comm(&kind, &content, buffers),
            Event::Reply { cell, content } => self.on_reply(&cell, &content),
            Event::Exited { report } => {
                self.kernel.borrow_mut().take();
                self.abandon_runs();
                let state = self.state.borrow().clone();
                match state {
                    State::Failed { .. } => {}
                    State::Starting => self.set_state(State::Failed {
                        message: "the kernel's bridge exited before the kernel started".into(),
                        report,
                    }),
                    _ => self.set_state(State::Stopped { report }),
                }
            }
        }
    }

    fn on_output(self: &Rc<Self>, token: &str, kind: &str, content: Value) {
        let index = {
            let mut runs = self.runs.borrow_mut();
            let Some(run) = runs.get_mut(token) else {
                return;
            };
            match kind {
                "execute_input" => {
                    run.count = content["execution_count"].as_i64();
                    return;
                }
                "clear_output" => {
                    if content["wait"].as_bool().unwrap_or(false) {
                        run.clear_on_next = true;
                        return;
                    }
                    run.outputs.clear();
                }
                _ => {
                    let display = content["transient"]["display_id"]
                        .as_str()
                        .map(str::to_string);
                    let Some(output) = nbformat_output(kind, content) else {
                        return;
                    };
                    if run.clear_on_next {
                        run.clear_on_next = false;
                        run.outputs.clear();
                    }
                    push_output(&mut run.outputs, output);
                    if let Some(display) = display {
                        self.displays
                            .borrow_mut()
                            .insert(display, (run.index, run.outputs.len() - 1));
                    }
                }
            }
            run.index
        };
        self.stale.borrow_mut().insert(index);
        if self.redraw_due.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(REDRAW_EVERY, move || {
            if let Some(session) = weak.upgrade() {
                session.redraw_stale();
            }
        });
    }

    /// Redraw the cells whose running outputs changed since they were last
    /// drawn, from what their runs hold now.
    fn redraw_stale(&self) {
        self.redraw_due.set(false);
        let stale = std::mem::take(&mut *self.stale.borrow_mut());
        for index in stale {
            let outputs = self
                .runs
                .borrow()
                .values()
                .find(|run| run.index == index)
                .map(|run| run.outputs.clone());
            if let Some(outputs) = outputs {
                self.draw_outputs(index, &outputs);
            }
        }
    }

    /// Cell `index`'s outputs drawn afresh as `outputs`.
    fn draw_outputs(&self, index: usize, outputs: &[Value]) {
        let drawing = self.drawing.borrow();
        let Some(drawing) = drawing.as_ref() else {
            return;
        };
        let Some(parts) = drawing.cells.get(&index) else {
            return;
        };
        clear(&parts.outputs);
        for output in outputs {
            if let Some(widget) = output_widget(
                output,
                &drawing.base,
                &drawing.on_link,
                None,
                Some(&self.widgets),
            ) {
                parts.outputs.append(&widget);
            }
        }
        parts
            .outputs
            .set_visible(parts.outputs.first_child().is_some());
        *parts.shown.borrow_mut() = outputs.to_vec();
    }

    /// A run's reply. The run is over once its idle has come too — or at
    /// once, for one the kernel aborted (a cell queued behind an error),
    /// which may have no idle of its own; and a reply whose idle has not
    /// come a second later is taken as the end all the same.
    fn on_reply(self: &Rc<Self>, token: &str, content: &Value) {
        let done = {
            let mut runs = self.runs.borrow_mut();
            let Some(run) = runs.get_mut(token) else {
                return;
            };
            run.reply = Some(content.clone());
            run.idle || content["status"].as_str() == Some("aborted")
        };
        if done {
            self.finish_run(token);
            return;
        }
        let (weak, token) = (Rc::downgrade(self), token.to_string());
        glib::timeout_add_local_once(std::time::Duration::from_secs(1), move || {
            if let Some(session) = weak.upgrade() {
                session.finish_run(&token);
            }
        });
    }

    fn on_idle(&self, token: &str) {
        let done = {
            let mut runs = self.runs.borrow_mut();
            let Some(run) = runs.get_mut(token) else {
                return;
            };
            run.idle = true;
            run.reply.is_some()
        };
        if done {
            self.finish_run(token);
        }
    }

    /// A run over: its last outputs drawn, its count shown, and both
    /// written into the file.
    fn finish_run(&self, token: &str) {
        let Some(run) = self.runs.borrow_mut().remove(token) else {
            return;
        };
        let content = run.reply.clone().unwrap_or(Value::Null);
        // Its last outputs drawn now, whatever the redraw timer has yet to
        // do: the run is over and out of the list the timer reads.
        self.stale.borrow_mut().remove(&run.index);
        self.draw_outputs(run.index, &run.outputs);
        let aborted = content["status"].as_str() == Some("aborted");
        let count = content["execution_count"].as_i64().or(run.count);
        if let Some(parts) = self
            .drawing
            .borrow()
            .as_ref()
            .and_then(|d| d.cells.get(&run.index))
        {
            parts
                .prompt
                .set_label(&prompt_text(if aborted { None } else { count }));
        }
        if !aborted {
            self.write_outputs(&run, count);
        }
    }

    /// `update_display_data`: the output a display id names replaced where
    /// it is — in a run still going, or among a finished cell's outputs and
    /// in the file.
    fn update_display(&self, content: &Value) {
        let Some(display) = content["transient"]["display_id"].as_str() else {
            return;
        };
        let Some((index, place)) = self.displays.borrow().get(display).copied() else {
            return;
        };
        let Some(output) = nbformat_output("display_data", content.clone()) else {
            return;
        };
        let running = {
            let mut runs = self.runs.borrow_mut();
            match runs.values_mut().find(|run| run.index == index) {
                Some(run) => {
                    if let Some(slot) = run.outputs.get_mut(place) {
                        *slot = output.clone();
                    }
                    Some(run.outputs.clone())
                }
                None => None,
            }
        };
        if let Some(outputs) = running {
            self.draw_outputs(index, &outputs);
            return;
        }
        let (id, outputs) = {
            let drawing = self.drawing.borrow();
            let Some(parts) = drawing.as_ref().and_then(|d| d.cells.get(&index)) else {
                return;
            };
            let mut shown = parts.shown.borrow_mut();
            let Some(slot) = shown.get_mut(place) else {
                return;
            };
            *slot = output.clone();
            (parts.id.clone(), shown.clone())
        };
        self.draw_outputs(index, &outputs);
        self.queue_edit(std::sync::Arc::new(move |notebook: &mut Value| {
            let Some(cell) = cell_mut(notebook, id.as_deref(), index) else {
                return false;
            };
            match cell.get_mut("outputs").and_then(Value::as_array_mut) {
                Some(outputs) if place < outputs.len() => {
                    outputs[place] = output.clone();
                    true
                }
                _ => false,
            }
        }));
    }

    fn set_state(&self, state: State) {
        // A kernel that is not running holds no widget models: their views
        // stay, unusable, until a run opens new ones.
        if matches!(
            state,
            State::Failed { .. } | State::Stopped { .. } | State::Restarting
        ) {
            self.widgets.disconnect();
        }
        *self.state.borrow_mut() = state;
        self.show_state();
        crate::kernels::kernels().changed();
    }

    fn show_state(&self) {
        let drawing = self.drawing.borrow();
        let Some(drawing) = drawing.as_ref() else {
            return;
        };
        let state = self.state.borrow();
        let runnable = self.runnable();
        // The bar's sentence, and which of its answers it offers: Start
        // Kernel, and Prompt Agent.
        let (said, start, prompt): (Option<String>, bool, bool) = match &*state {
            _ if !runnable => (
                Some(
                    "This notebook's environment is gone, so its cells cannot run; it is shown \
                     as it was saved."
                        .to_string(),
                ),
                false,
                false,
            ),
            State::Idle | State::Busy => (None, false, false),
            State::Off => (
                Some(format!(
                    "The kernel is not running. Cells run in {} once it starts.",
                    self.where_it_runs()
                )),
                true,
                false,
            ),
            State::Starting | State::Restarting => (
                Some(format!(
                    "The kernel is starting in {}.",
                    self.where_it_runs()
                )),
                false,
                false,
            ),
            State::Failed { message, .. } => (
                Some(format!("The kernel did not start: {message}")),
                true,
                true,
            ),
            State::Stopped { .. } => (Some("The kernel stopped.".to_string()), true, true),
        };
        let running = matches!(*state, State::Busy | State::Starting | State::Restarting);
        drawing.run_all.set_sensitive(runnable);
        drawing.interrupt.set_sensitive(runnable && running);
        drawing
            .restart
            .set_sensitive(runnable && !matches!(*state, State::Off));
        drawing.banner.start.set_visible(start);
        drawing.banner.prompt.set_visible(prompt);
        match said {
            Some(said) => {
                drawing.banner.title.set_label(&said);
                drawing.banner.widget.set_reveal_child(true);
            }
            None => drawing.banner.widget.set_reveal_child(false),
        }
    }

    /// Where its kernel runs, in the bar's words: this environment's
    /// container, or which other one's.
    fn where_it_runs(&self) -> String {
        if self.writable {
            "this environment's container".to_string()
        } else {
            format!("{}'s container", self.env)
        }
    }

    /// Hand what the kernel said to the environment's agent, to fix in the
    /// container.
    fn prompt_agent(&self) {
        let (message, report) = match &*self.state.borrow() {
            State::Failed { message, report } => (message.clone(), report.clone()),
            State::Stopped { report } => ("the kernel stopped".to_string(), report.clone()),
            _ => return,
        };
        let name = self.kernel_name.borrow().clone();
        let (prompt, attachment) = repair_prompt(&self.path, &name, &message, &report);
        (self.on_prompt_agent)(prompt, attachment);
    }

    /// Write `code`'s outputs into cell `index` of the buffer, when that
    /// cell's source is still `code`.
    /// A finished run's outputs and count, into its cell in the file — the
    /// cell its id names, or the one at its index — when that cell's source
    /// is still the code that ran.
    fn write_outputs(&self, run: &Run, count: Option<i64>) {
        // Another environment's notebook is run and its outputs drawn; its
        // file is not written.
        if !self.editable() {
            return;
        }
        let (id, index, code) = (run.cell_id.clone(), run.index, run.code.clone());
        let outputs = Value::Array(run.outputs.clone());
        self.queue_edit(std::sync::Arc::new(move |notebook: &mut Value| {
            let Some(cell) = cell_mut(notebook, id.as_deref(), index) else {
                return false;
            };
            let source = joined(cell.get("source").unwrap_or(&Value::Null));
            if source.trim_end_matches('\n') != code.trim_end_matches('\n') {
                return false;
            }
            cell.insert("outputs".into(), outputs.clone());
            cell.insert(
                "execution_count".into(),
                count.map_or(Value::Null, Value::from),
            );
            true
        }));
    }

    /// Cell `index`'s source as it stands on screen, into the file — onto
    /// the cell that still holds the source last filed for it, so an edit
    /// never lands on a cell something else moved into its place.
    fn write_source(&self, index: usize) {
        if !self.editable() {
            return;
        }
        let (id, source, filed) = {
            let drawing = self.drawing.borrow();
            let Some(parts) = drawing.as_ref().and_then(|d| d.cells.get(&index)) else {
                return;
            };
            if let Some(timer) = parts.pending.borrow_mut().take() {
                timer.remove();
            }
            let source = buffer_text(&parts.source);
            let before = parts.filed.replace(source.clone());
            (parts.id.clone(), source, before)
        };
        if source == filed {
            return;
        }
        self.queue_edit(std::sync::Arc::new(move |notebook: &mut Value| {
            let Some(cell) = cell_mut(notebook, id.as_deref(), index) else {
                return false;
            };
            let now = joined(cell.get("source").unwrap_or(&Value::Null));
            if now.trim_end_matches('\n') != filed.trim_end_matches('\n') {
                return false;
            }
            cell.insert("source".into(), source_lines(&source));
            true
        }));
    }

    /// Queue a change to the file. Changes are applied in batches off the
    /// main thread — parsing and writing a notebook of figures is tens of
    /// milliseconds — and the buffer is changed by the difference only, in
    /// one user action, so the Edit face keeps its place and one undo takes
    /// the batch back.
    fn queue_edit(&self, edit: Edit) {
        self.edits.borrow_mut().push(edit);
        if !self.editing.replace(true) {
            glib::idle_add_local_once({
                let weak = self.self_weak();
                move || {
                    if let Some(session) = weak.upgrade() {
                        session.apply_edits(0);
                    }
                }
            });
        }
    }

    fn self_weak(&self) -> Weak<Session> {
        self.this.borrow().clone()
    }

    fn apply_edits(self: &Rc<Self>, attempt: u32) {
        let edits: Vec<Edit> = std::mem::take(&mut *self.edits.borrow_mut());
        if edits.is_empty() {
            self.editing.set(false);
            return;
        }
        let text = buffer_text(&self.buffer);
        let changes = self.changes.get();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let batch = edits.clone();
            let splice = crate::runtime::runtime()
                .spawn_blocking(move || -> Option<Splice> {
                    let mut notebook: Value = serde_json::from_str(&text).ok()?;
                    let mut changed = false;
                    for edit in &batch {
                        changed |= edit(&mut notebook);
                    }
                    if !changed {
                        return None;
                    }
                    Some(splice_between(&text, &nbformat_json(&notebook)?))
                })
                .await
                .ok()
                .flatten();
            let Some(session) = weak.upgrade() else {
                return;
            };
            if session.changes.get() != changes {
                // The buffer changed while the batch was computed: computed
                // again over what it holds now, a few times at most.
                if attempt < 5 {
                    let mut queued = session.edits.borrow_mut();
                    let later = std::mem::take(&mut *queued);
                    *queued = edits;
                    queued.extend(later);
                } else {
                    tracing::warn!("notebook: outputs not written: the file kept changing");
                }
                session.apply_edits(attempt + 1);
                return;
            }
            if let Some(splice) = splice {
                session.writing.set(true);
                let buffer = &session.buffer;
                buffer.begin_user_action();
                let mut start = buffer.iter_at_offset(splice.start as i32);
                let mut end = buffer.iter_at_offset(splice.end as i32);
                buffer.delete(&mut start, &mut end);
                let mut at = buffer.iter_at_offset(splice.start as i32);
                buffer.insert(&mut at, &splice.text);
                buffer.end_user_action();
                session.writing.set(false);
            }
            session.apply_edits(0);
        });
    }
}

/// The part of a text that changed: characters `start..end` of the old
/// text replaced by `text`.
struct Splice {
    start: usize,
    end: usize,
    text: String,
}

/// The smallest replacement that turns `before` into `after`: what they
/// share at the start and at the end is kept.
fn splice_between(before: &str, after: &str) -> Splice {
    let prefix = before
        .char_indices()
        .zip(after.chars())
        .find(|((_, a), b)| a != b)
        .map_or(before.len().min(after.len()), |((i, _), _)| i);
    let prefix = {
        let mut p = prefix.min(after.len());
        while !after.is_char_boundary(p) || !before.is_char_boundary(p) {
            p -= 1;
        }
        p
    };
    let (rest_before, rest_after) = (&before[prefix..], &after[prefix..]);
    let mut suffix = 0;
    for (a, b) in rest_before.chars().rev().zip(rest_after.chars().rev()) {
        if a != b {
            break;
        }
        suffix += a.len_utf8();
    }
    let suffix = suffix.min(rest_before.len()).min(rest_after.len());
    let start = before[..prefix].chars().count();
    let removed = rest_before[..rest_before.len() - suffix].chars().count();
    Splice {
        start,
        end: start + removed,
        text: rest_after[..rest_after.len() - suffix].to_string(),
    }
}

/// The cell an edit is for: the one with nbformat id `id` when it has one,
/// otherwise the one at `index` — `None` when the notebook is not the shape
/// a notebook has, rather than a panic on a file something rewrote.
fn cell_mut<'a>(
    notebook: &'a mut Value,
    id: Option<&str>,
    index: usize,
) -> Option<&'a mut serde_json::Map<String, Value>> {
    let cells = notebook.as_object_mut()?.get_mut("cells")?.as_array_mut()?;
    let cell = match id {
        Some(id) => cells
            .iter_mut()
            .find(|cell| cell.get("id").and_then(Value::as_str) == Some(id))?,
        None => cells.get_mut(index)?,
    };
    cell.as_object_mut()
}

/// The first slider under `widget`, depth first.
fn first_scale(widget: &gtk::Widget) -> Option<gtk::Scale> {
    if let Some(scale) = widget.downcast_ref::<gtk::Scale>() {
        return Some(scale.clone());
    }
    let mut child = widget.first_child();
    while let Some(next) = child {
        if let Some(found) = first_scale(&next) {
            return Some(found);
        }
        child = next.next_sibling();
    }
    None
}

/// The text of a buffer, whole.
fn buffer_text(buffer: &impl IsA<gtk::TextBuffer>) -> String {
    let buffer = buffer.as_ref();
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), true)
        .to_string()
}

fn clear(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn set_margin(widget: &gtk::Widget, side: gtk::PositionType, value: i32) {
    match side {
        gtk::PositionType::Top => widget.set_margin_top(value),
        gtk::PositionType::Bottom => widget.set_margin_bottom(value),
        gtk::PositionType::Left => widget.set_margin_start(value),
        _ => widget.set_margin_end(value),
    }
}

fn notice(text: &str) -> gtk::Widget {
    gtk::Label::builder()
        .label(text)
        .wrap(true)
        .xalign(0.0)
        .margin_top(16)
        .margin_start(16)
        .margin_end(16)
        .css_classes(["dim-label"])
        .build()
        .upcast()
}

fn prompt_text(count: Option<i64>) -> String {
    match count {
        Some(n) => format!("[{n}]"),
        None => "[ ]".to_string(),
    }
}

/// A multiline string as nbformat stores it: a string, or a list of
/// strings to be joined.
pub fn joined(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

/// A cell's source as Jupyter writes it: one string per line, each
/// keeping its newline but the last.
fn source_lines(source: &str) -> Value {
    Value::Array(
        source
            .split_inclusive('\n')
            .map(|line| Value::from(line.to_string()))
            .collect(),
    )
}

/// The notebook as Jupyter writes it: one-space indentation and a closing
/// newline, so a run's change to the file is the outputs and nothing else.
fn nbformat_json(notebook: &Value) -> Option<String> {
    let mut out = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(
        &mut out,
        serde_json::ser::PrettyFormatter::with_indent(b" "),
    );
    notebook.serialize(&mut serializer).ok()?;
    let mut text = String::from_utf8(out).ok()?;
    text.push('\n');
    Some(text)
}

/// A kernel's iopub message as the output the file keeps; `None` for one
/// that is not an output.
pub(crate) fn nbformat_output(kind: &str, content: Value) -> Option<Value> {
    let mut output = serde_json::Map::new();
    output.insert("output_type".into(), Value::from(kind));
    match kind {
        "stream" => {
            output.insert("name".into(), content["name"].clone());
            output.insert("text".into(), content["text"].clone());
        }
        "display_data" | "execute_result" => {
            output.insert("data".into(), content["data"].clone());
            output.insert(
                "metadata".into(),
                if content["metadata"].is_object() {
                    content["metadata"].clone()
                } else {
                    Value::Object(Default::default())
                },
            );
            if kind == "execute_result" {
                output.insert("execution_count".into(), content["execution_count"].clone());
            }
        }
        "error" => {
            output.insert("ename".into(), content["ename"].clone());
            output.insert("evalue".into(), content["evalue"].clone());
            output.insert("traceback".into(), content["traceback"].clone());
        }
        _ => return None,
    }
    Some(Value::Object(output))
}

/// Add an output to a run's list: a stream continues the last output when
/// that is a stream of the same name, as Jupyter keeps them.
pub(crate) fn push_output(outputs: &mut Vec<Value>, output: Value) {
    if output["output_type"] == "stream" {
        if let Some(last) = outputs.last_mut() {
            if last["output_type"] == "stream" && last["name"] == output["name"] {
                // In place: a stream of twenty thousand prints copied its
                // whole text on every one of them.
                if !last["text"].is_string() {
                    last["text"] = Value::from(joined(&last["text"]));
                }
                if let Value::String(text) = &mut last["text"] {
                    let from = text.rfind('\n').map_or(0, |i| i + 1);
                    text.push_str(&joined(&output["text"]));
                    collapse_overwrites(text, from);
                }
                return;
            }
        }
    }
    let mut output = output;
    if output["output_type"] == "stream" {
        let mut text = joined(&output["text"]);
        collapse_overwrites(&mut text, 0);
        output["text"] = Value::from(text);
    }
    outputs.push(output);
}

/// A carriage return's overwrite applied to `text` from byte `from` on, as
/// a terminal — and JupyterLab — keeps it: each line keeps what follows its
/// last lone `\r`. A progress bar redrawing itself thousands of times is
/// then one line in the file, not thousands run together.
fn collapse_overwrites(text: &mut String, from: usize) {
    let tail = &text[from..];
    if !tail.contains('\r') {
        return;
    }
    let collapsed = tail
        .replace("\r\n", "\n")
        .split('\n')
        .map(|line| line.rsplit('\r').next().unwrap_or(line).to_string())
        .collect::<Vec<_>>()
        .join("\n");
    text.truncate(from);
    text.push_str(&collapsed);
}

/// Text with a terminal's escapes as Pango markup: SGR colour and weight
/// read (`crate::ansi`), a carriage return's overwrite honoured (a
/// progress bar keeps its last state), everything else dropped.
fn ansi_markup(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            let line = line.rsplit('\r').next().unwrap_or(line);
            crate::ansi::spans(line)
                .into_iter()
                .map(|(run, style)| {
                    let escaped = glib::markup_escape_text(&run);
                    if style.is_plain() {
                        return escaped.to_string();
                    }
                    let mut attributes = String::new();
                    if let Some(colour) = style.fg {
                        let (r, g, b) = colour.rgb();
                        attributes.push_str(&format!(" foreground=\"#{r:02x}{g:02x}{b:02x}\""));
                    }
                    if style.bold {
                        attributes.push_str(" weight=\"bold\"");
                    }
                    if style.dim {
                        attributes.push_str(" alpha=\"60%\"");
                    }
                    if style.italic {
                        attributes.push_str(" style=\"italic\"");
                    }
                    if style.underline {
                        attributes.push_str(" underline=\"single\"");
                    }
                    format!("<span{attributes}>{escaped}</span>")
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Text as an output shows it: monospace, coloured, selectable, folded to
/// the column, and cut at [`MAX_OUTPUT_LINES`] with what was cut said.
fn text_output(text: &str, error: bool) -> gtk::Widget {
    let text = text.trim_end_matches('\n');
    let total = text.lines().count();
    // The last lines: a long output is a log, and its latest lines are the
    // ones being watched.
    let shown: &str = if total > MAX_OUTPUT_LINES {
        let mut cut = text.len();
        for _ in 0..MAX_OUTPUT_LINES {
            cut = text[..cut].rfind('\n').unwrap_or(0);
        }
        text[cut..].trim_start_matches('\n')
    } else {
        text
    };
    // And by size: one line can be megabytes, and laying it out is the
    // main thread's.
    let shown = if shown.len() > MAX_OUTPUT_BYTES {
        let mut cut = shown.len() - MAX_OUTPUT_BYTES;
        while !shown.is_char_boundary(cut) {
            cut += 1;
        }
        &shown[cut..]
    } else {
        shown
    };
    let label = gtk::Label::builder()
        .use_markup(true)
        .label(ansi_markup(shown))
        .xalign(0.0)
        .selectable(true)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .css_classes(["monospace", "notebook-output"])
        .build();
    if error {
        label.add_css_class("notebook-error");
    }
    if total <= MAX_OUTPUT_LINES && shown.len() == text.len() {
        return label.upcast();
    }
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.append(
        &gtk::Label::builder()
            .label(if total > MAX_OUTPUT_LINES {
                format!("{} earlier lines, in the file", total - MAX_OUTPUT_LINES)
            } else {
                "The start of this output is in the file".to_string()
            })
            .xalign(0.0)
            .css_classes(["caption", "dim-label"])
            .build(),
    );
    column.append(&label);
    column.upcast()
}

/// One output, drawn; `None` for an output with nothing to draw.
fn output_widget(
    output: &Value,
    base: &DocumentBase,
    on_link: &Rc<dyn Fn(&str)>,
    ready: Option<&Readiness>,
    widgets: Option<&Rc<crate::widgets::Manager>>,
) -> Option<gtk::Widget> {
    match output["output_type"].as_str()? {
        "stream" => {
            let text = joined(&output["text"]);
            (!text.is_empty())
                .then(|| text_output(&text, output["name"].as_str() == Some("stderr")))
        }
        "error" => {
            let traceback = output["traceback"]
                .as_array()
                .map(|lines| {
                    lines
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| {
                    format!(
                        "{}: {}",
                        output["ename"].as_str().unwrap_or("Error"),
                        output["evalue"].as_str().unwrap_or_default()
                    )
                });
            Some(text_output(&traceback, true))
        }
        "display_data" | "execute_result" => {
            let data = &output["data"];
            // An interactive widget: its live view, while its model is
            // known; otherwise the text the kernel sent beside it.
            if let (Some(view), Some(widgets)) = (
                data.get("application/vnd.jupyter.widget-view+json"),
                widgets,
            ) {
                if let Some(drawn) = view["model_id"].as_str().and_then(|id| widgets.view(id)) {
                    return Some(drawn);
                }
            }
            // matplotlib's and Seaborn's figures: a PNG by default, with
            // the size to show it at beside it when it was drawn at twice
            // that (`figure_format = 'retina'`); an SVG when the notebook
            // asked for one, drawn by the confined helper Mermaid uses.
            // Each drawn by the confined helper, never decoded here.
            for mime in ["image/png", "image/jpeg"] {
                if data.get(mime).is_some() {
                    let width = output["metadata"][mime]["width"].as_f64();
                    let encoded = joined(&data[mime]);
                    let figure = if mime == "image/png" {
                        crate::mermaid::Figure::EncodedPng(encoded)
                    } else {
                        crate::mermaid::Figure::EncodedJpeg(encoded)
                    };
                    return Some(crate::mermaid::figure(
                        figure,
                        width,
                        ready.map(Readiness::hold),
                    ));
                }
            }
            if data.get("image/svg+xml").is_some() {
                return Some(crate::mermaid::figure(
                    crate::mermaid::Figure::Svg(joined(&data["image/svg+xml"])),
                    None,
                    ready.map(Readiness::hold),
                ));
            }
            if data.get("text/markdown").is_some() {
                let drawn = crate::markdown_view::render_in(
                    &joined(&data["text/markdown"]),
                    on_link.clone(),
                    None,
                    Some(base.clone()),
                );
                drawn.set_margin_top(0);
                drawn.set_margin_bottom(0);
                drawn.set_margin_start(0);
                drawn.set_margin_end(0);
                return Some(drawn);
            }
            if data.get("text/plain").is_some() {
                return Some(text_output(&joined(&data["text/plain"]), false));
            }
            let kind = data
                .as_object()
                .and_then(|d| d.keys().next().cloned())
                .unwrap_or_else(|| "unknown".into());
            Some(notice(&format!(
                "An output of type {kind}, which is not drawn here; it is in the file."
            )))
        }
        _ => None,
    }
}

/// The bar at the top of a notebook whose kernel is not running: that it is
/// not, or what it said when it would not start, Start Kernel, and — for a
/// failure — Prompt Agent; gone while the kernel runs as it should (David,
/// 2026-10-11: "Show a warning banner at the top of a Jupyter document that
/// warns if it isn't running and offers a button to start it. But, have the
/// banner go away if it's running properly"). How the kernels stand lives in
/// the environment's Tasks entry (`crate::kernels`). Drawn as every tab's
/// bar is (`viewer::ChangedBar`, `.changed-bar`), in the warning colour: the
/// sentence at the start, wrapping, and the answers at the end — not
/// `AdwBanner`'s centred title over a wide button, which stood apart from
/// every other document's ("ugly and inconsistent with other banners for
/// other document types"). The text is the kernel's words as they are,
/// selectable to copy.
struct KernelBar {
    widget: gtk::Revealer,
    title: gtk::Label,
    start: gtk::Button,
    prompt: gtk::Button,
}

impl KernelBar {
    fn new() -> Self {
        let title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .selectable(true)
            .build();
        let prompt = gtk::Button::builder()
            .label("Prompt Agent")
            .tooltip_text("Ask this environment's agent to make the kernel start")
            .valign(gtk::Align::Center)
            .build();
        let start = gtk::Button::builder()
            .label("Start Kernel")
            .tooltip_text("Start this notebook's kernel in its environment")
            .css_classes(["suggested-action"])
            .valign(gtk::Align::Center)
            .build();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.add_css_class("changed-bar");
        row.add_css_class("notebook-kernel");
        row.append(&title);
        row.append(&prompt);
        row.append(&start);
        let widget = gtk::Revealer::builder()
            .child(&row)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(false)
            .build();
        Self {
            widget,
            title,
            start,
            prompt,
        }
    }
}

/// The prompt Prompt Agent sends when the kernel would not start, and what
/// it said as the attachment — written to the house rule on agent-facing
/// text: the evidence rides along, and the steps name the tools.
pub fn repair_prompt(
    path: &std::path::Path,
    kernel: &str,
    message: &str,
    report: &str,
) -> (String, (String, String)) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "the notebook".into());
    let kernel = if kernel.is_empty() {
        "the default Python kernel".to_string()
    } else {
        format!("the kernel `{kernel}`")
    };
    let prompt = format!(
        "The notebook `{name}` cannot run: its kernel did not start in this environment.\n\n\
         WHAT IS TRUE\n\
         The IDE runs a notebook's kernel inside this environment's container, with the \
         container's own Jupyter: the first of .venv/bin/python, venv/bin/python, python3, and \
         python that can import jupyter_client, then {kernel}, which the notebook names. It \
         reported: {message}. Its full report is attached as notebook-kernel.log.\n\n\
         HOW TO WORK\n\
         1. Read the attached report.\n\
         2. Make the container provide what is missing: ipykernel (which brings \
         jupyter_client) and {kernel}, and the packages the notebook imports. Install them the \
         way this project already manages packages (its .venv, requirements, pyproject, or the \
         image in .devcontainer/); after changing .devcontainer/, call devcontainer_reload.\n\
         3. Check with ide_exec: `python3 -c \"import jupyter_client\"` and `jupyter \
         kernelspec list`.\n\
         4. Finish with one short paragraph: what was missing, and what you changed."
    );
    let attachment = if report.trim().is_empty() {
        message.to_string()
    } else {
        report.to_string()
    };
    (prompt, ("notebook-kernel.log".to_string(), attachment))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multiline_string_reads_either_way_nbformat_writes_it() {
        assert_eq!(joined(&serde_json::json!("a\nb")), "a\nb");
        assert_eq!(joined(&serde_json::json!(["a\n", "b"])), "a\nb");
        assert_eq!(joined(&Value::Null), "");
        assert_eq!(
            source_lines("x = 1\ny = 2"),
            serde_json::json!(["x = 1\n", "y = 2"])
        );
    }

    /// Streams of one name run together, as Jupyter keeps them; another
    /// name, or anything between, starts a new one.
    #[test]
    fn streams_continue_and_other_outputs_do_not() {
        let stream = |name: &str, text: &str| {
            nbformat_output("stream", serde_json::json!({"name": name, "text": text})).unwrap()
        };
        let mut outputs = Vec::new();
        push_output(&mut outputs, stream("stdout", "a\n"));
        push_output(&mut outputs, stream("stdout", "b\n"));
        push_output(&mut outputs, stream("stderr", "c\n"));
        assert_eq!(outputs.len(), 2);
        assert_eq!(joined(&outputs[0]["text"]), "a\nb\n");
        let result = nbformat_output(
            "execute_result",
            serde_json::json!({"data": {"text/plain": "2"}, "metadata": {}, "execution_count": 1}),
        )
        .unwrap();
        assert_eq!(result["output_type"], "execute_result");
        assert_eq!(result["execution_count"], 1);
        assert!(nbformat_output("status", Value::Null).is_none());
    }

    /// A progress bar redrawn by carriage returns, arriving in pieces, is
    /// kept as its last state — one line, as a terminal shows it — and the
    /// lines before it are untouched.
    #[test]
    fn a_progress_bar_is_kept_as_its_last_state() {
        let stream = |text: &str| {
            nbformat_output(
                "stream",
                serde_json::json!({"name": "stderr", "text": text}),
            )
            .unwrap()
        };
        let mut outputs = Vec::new();
        push_output(&mut outputs, stream("loading\n  0%"));
        for pct in 1..=100 {
            push_output(&mut outputs, stream(&format!("\r{pct:3}%")));
        }
        push_output(&mut outputs, stream("\ndone\r\n"));
        assert_eq!(joined(&outputs[0]["text"]), "loading\n100%\ndone\n");
    }

    /// The buffer is changed by the difference only, in characters, so a
    /// write-back is one small edit and multibyte text survives it.
    #[test]
    fn a_write_back_changes_only_what_differs() {
        let splice = splice_between(
            "{ \"a\": \"é1\", \"z\": 0 }",
            "{ \"a\": \"é22\", \"z\": 0 }",
        );
        assert_eq!(
            (splice.start, splice.end, splice.text.as_str()),
            (9, 10, "22")
        );
        let same = splice_between("abc", "abc");
        assert_eq!((same.start, same.end, same.text.as_str()), (3, 3, ""));
        let grown = splice_between("ab", "abab");
        assert_eq!((grown.start, grown.end, grown.text.as_str()), (2, 2, "ab"));
    }

    /// A cell is found by its id wherever it has moved, by its index when
    /// it has none, and a file that is not a notebook's shape is refused
    /// rather than a panic.
    #[test]
    fn a_cell_is_found_by_id_and_a_strange_file_is_refused() {
        let mut notebook = serde_json::json!({"cells": [
            {"id": "new", "source": "inserted above"},
            {"id": "mine", "source": "x = 1"},
        ]});
        let cell = cell_mut(&mut notebook, Some("mine"), 0).unwrap();
        assert_eq!(cell["source"], "x = 1");
        assert!(cell_mut(&mut notebook, None, 1).is_some());
        assert!(cell_mut(&mut notebook, Some("gone"), 0).is_none());
        assert!(cell_mut(&mut serde_json::json!([1, 2]), None, 0).is_none());
        assert!(cell_mut(&mut serde_json::json!({"cells": ["a string"]}), None, 0).is_none());
    }

    /// Written back the way Jupyter writes: one-space indentation, a
    /// closing newline, and the same keys in the same order.
    #[test]
    fn the_file_is_written_as_jupyter_writes_it() {
        let text = "{\n \"cells\": [],\n \"metadata\": {},\n \"nbformat\": 4,\n \"nbformat_minor\": 5\n}\n";
        let notebook: Value = serde_json::from_str(text).unwrap();
        assert_eq!(nbformat_json(&notebook).unwrap(), text);
    }

    #[test]
    fn terminal_colour_becomes_markup_and_a_carriage_return_overwrites() {
        let markup = ansi_markup("\u{1b}[31mError\u{1b}[39m: <bad>");
        assert!(markup.contains("<span foreground="), "{markup}");
        assert!(markup.contains("&lt;bad&gt;"));
        assert_eq!(ansi_markup("10%\r50%\r100%"), "100%");
    }

    #[test]
    fn the_repair_prompt_carries_what_the_kernel_said() {
        let (prompt, (file, log)) = repair_prompt(
            std::path::Path::new("/w/analysis.ipynb"),
            "ir",
            "no kernel named 'ir' in this environment",
            "no kernel named 'ir'\nKernels this environment has: python3",
        );
        assert!(prompt.contains("analysis.ipynb"));
        assert!(prompt.contains("the kernel `ir`"));
        assert!(prompt.contains("devcontainer_reload"));
        assert_eq!(file, "notebook-kernel.log");
        assert!(log.contains("python3"));
    }
}

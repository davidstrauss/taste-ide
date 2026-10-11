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
/// How long a code cell's typing settles before it is written back.
const SOURCE_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

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

/// One run of one cell, until its reply.
struct Run {
    index: usize,
    code: String,
    outputs: Vec<Value>,
    count: Option<i64>,
    /// `clear_output` with `wait`: clear at the next output, not now.
    clear_on_next: bool,
}

/// One drawn code cell's live parts.
struct CellParts {
    prompt: gtk::Label,
    outputs: gtk::Box,
    source: sourceview5::Buffer,
    view: sourceview5::View,
}

/// The parts of the current drawing that a kernel event changes.
struct Drawing {
    status: gtk::Label,
    run_all: gtk::Button,
    interrupt: gtk::Button,
    restart: gtk::Button,
    banner: adw::Banner,
    cells: HashMap<usize, CellParts>,
    base: DocumentBase,
    on_link: Rc<dyn Fn(&str)>,
}

/// A notebook page's session: its kernel, the runs in flight, and the
/// drawing on screen. One per open notebook; dropped with its page, which
/// ends the kernel.
pub struct Session {
    path: PathBuf,
    /// Where the kernel runs; `None` when this notebook is another
    /// environment's, which is looked at and neither edited nor run.
    exec: Option<taste_core::ExecContext>,
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
}

impl Session {
    /// A session for the notebook at `path`, whose bytes are `buffer`.
    /// `exec` is its environment's, or `None` for a notebook that is only
    /// looked at; `on_prompt_agent` hands a prompt and its attachment to
    /// that environment's agent.
    pub fn new(
        path: PathBuf,
        exec: Option<taste_core::ExecContext>,
        buffer: sourceview5::Buffer,
        on_prompt_agent: Rc<dyn Fn(String, (String, String))>,
    ) -> Rc<Self> {
        let state = match std::env::var("TASTE_PROBE_NOTEBOOK").as_deref() {
            // A kernel that did not start, which otherwise needs a
            // container without Jupyter.
            Ok("failed") => State::Failed {
                message: "no Python in this environment can import jupyter_client (tried \
                          .venv/bin/python, venv/bin/python, python3, and python)"
                    .into(),
                report: String::new(),
            },
            _ => State::Off,
        };
        let session = Rc::new(Self {
            path,
            exec,
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
        });
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

    fn editable(&self) -> bool {
        self.exec.is_some()
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
                Ok(Ok(notebook)) => session.build(&root, &notebook, base, on_link, &ready),
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

        // The kernel's bar: what it is and where it stands, and the three
        // things done to it.
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.add_css_class("notebook-bar");
        // On the cells' own inset, so the bar's words start where theirs do.
        bar.set_margin_start(16);
        bar.set_margin_end(10);
        bar.set_margin_top(4);
        let status = gtk::Label::builder()
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
        let banner = adw::Banner::builder().button_label("Prompt Agent").build();

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
        root.append(&bar);
        root.append(&banner);
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
            banner.connect_button_clicked(move |_| {
                if let Some(session) = weak.upgrade() {
                    session.prompt_agent();
                }
            });
        }
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
        _cell: &Value,
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
            .sensitive(self.editable())
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
        // Edited here, written back once the typing settles.
        if self.editable() {
            let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
            let weak: Weak<Session> = Rc::downgrade(self);
            buffer.connect_changed(move |buffer| {
                if let Some(id) = pending.borrow_mut().take() {
                    id.remove();
                }
                let weak = weak.clone();
                let buffer = buffer.clone();
                let slot = pending.clone();
                let id = glib::timeout_add_local_once(SOURCE_SETTLE, move || {
                    slot.borrow_mut().take();
                    if let Some(session) = weak.upgrade() {
                        session.write_source(index, &buffer_text(&buffer));
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
        let code = {
            let drawing = self.drawing.borrow();
            let Some(parts) = drawing.as_ref().and_then(|d| d.cells.get(&index)) else {
                return;
            };
            buffer_text(&parts.source)
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
                code: code.clone(),
                outputs: Vec::new(),
                count: None,
                clear_on_next: false,
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
                    drawing.status.set_tooltip_text(Some(&display_name));
                }
                self.set_state(State::Idle);
            }
            Event::Failed { message, report } => {
                self.kernel.borrow_mut().take();
                self.abandon_runs();
                self.set_state(State::Failed { message, report });
            }
            Event::State(state) => match state.as_str() {
                "busy" => self.set_state(State::Busy),
                "idle" => self.set_state(State::Idle),
                "restarting" => self.set_state(State::Restarting),
                "dead" => self.set_state(State::Stopped {
                    report: "The kernel died.".into(),
                }),
                _ => {}
            },
            Event::Output {
                cell,
                parent,
                kind,
                content,
            } => {
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

    fn on_output(&self, token: &str, kind: &str, content: Value) {
        let mut runs = self.runs.borrow_mut();
        let Some(run) = runs.get_mut(token) else {
            return;
        };
        let drawing = self.drawing.borrow();
        let parts = drawing.as_ref().and_then(|d| d.cells.get(&run.index));
        if kind == "execute_input" {
            run.count = content["execution_count"].as_i64();
            return;
        }
        if kind == "clear_output" {
            if content["wait"].as_bool().unwrap_or(false) {
                run.clear_on_next = true;
            } else {
                run.outputs.clear();
                if let Some(parts) = parts {
                    clear(&parts.outputs);
                }
            }
            return;
        }
        let Some(output) = nbformat_output(kind, content) else {
            return;
        };
        if run.clear_on_next {
            run.clear_on_next = false;
            run.outputs.clear();
            if let Some(parts) = parts {
                clear(&parts.outputs);
            }
        }
        push_output(&mut run.outputs, output.clone());
        if let (Some(parts), Some(drawing)) = (parts, drawing.as_ref()) {
            // A stream continues the last one of its name, so redraw that
            // one rather than add a line of its own.
            clear(&parts.outputs);
            for output in &run.outputs {
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
            parts.outputs.set_visible(true);
        }
    }

    fn on_reply(&self, token: &str, content: &Value) {
        let Some(run) = self.runs.borrow_mut().remove(token) else {
            return;
        };
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
            self.write_outputs(run.index, &run.code, &run.outputs, count);
        }
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
    }

    fn show_state(&self) {
        let drawing = self.drawing.borrow();
        let Some(drawing) = drawing.as_ref() else {
            return;
        };
        let state = self.state.borrow();
        let editable = self.editable();
        let (label, banner) = match &*state {
            _ if !editable => (
                "Another environment's notebook — read here, run in its own environment"
                    .to_string(),
                None,
            ),
            State::Off => ("Kernel not started — Run starts it".to_string(), None),
            State::Starting => ("Kernel starting in this environment".to_string(), None),
            State::Idle => ("Kernel idle".to_string(), None),
            State::Busy => ("Kernel busy".to_string(), None),
            State::Restarting => ("Kernel restarting".to_string(), None),
            State::Failed { message, .. } => (
                "Kernel not running".to_string(),
                Some(format!("The kernel did not start: {message}")),
            ),
            State::Stopped { .. } => (
                "Kernel stopped".to_string(),
                Some("The kernel stopped. Restart starts it again.".to_string()),
            ),
        };
        drawing.status.set_label(&label);
        let running = matches!(*state, State::Busy | State::Starting | State::Restarting);
        drawing.run_all.set_sensitive(editable);
        drawing.interrupt.set_sensitive(editable && running);
        drawing
            .restart
            .set_sensitive(editable && !matches!(*state, State::Off));
        match banner {
            Some(title) => {
                drawing.banner.set_title(&glib::markup_escape_text(&title));
                drawing.banner.set_revealed(true);
            }
            None => drawing.banner.set_revealed(false),
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
    fn write_outputs(&self, index: usize, code: &str, outputs: &[Value], count: Option<i64>) {
        self.edit_cell(index, |cell| {
            if joined(&cell["source"]).trim_end_matches('\n') != code.trim_end_matches('\n') {
                return false;
            }
            cell["outputs"] = Value::Array(outputs.to_vec());
            cell["execution_count"] = count.map_or(Value::Null, Value::from);
            true
        });
    }

    fn write_source(&self, index: usize, source: &str) {
        self.edit_cell(index, |cell| {
            if joined(&cell["source"]) == source {
                return false;
            }
            cell["source"] = source_lines(source);
            true
        });
    }

    /// Change one cell in the buffer's JSON, through `change`, which says
    /// whether it changed anything. The buffer is rewritten in one user
    /// action, so one undo takes it back.
    fn edit_cell(&self, index: usize, change: impl FnOnce(&mut Value) -> bool) {
        let text = buffer_text(&self.buffer);
        let Ok(mut notebook) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        let Some(cell) = notebook["cells"].get_mut(index) else {
            return;
        };
        if !change(cell) {
            return;
        }
        let Some(written) = nbformat_json(&notebook) else {
            return;
        };
        self.writing.set(true);
        self.buffer.begin_user_action();
        self.buffer.set_text(&written);
        self.buffer.end_user_action();
        self.writing.set(false);
    }
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
                let text = joined(&last["text"]) + &joined(&output["text"]);
                last["text"] = Value::from(text);
                return;
            }
        }
    }
    outputs.push(output);
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
    let total = text.lines().count();
    let shown: String = if total > MAX_OUTPUT_LINES {
        text.lines()
            .take(MAX_OUTPUT_LINES)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text.trim_end_matches('\n').to_string()
    };
    let label = gtk::Label::builder()
        .use_markup(true)
        .label(ansi_markup(&shown))
        .xalign(0.0)
        .selectable(true)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .css_classes(["monospace", "notebook-output"])
        .build();
    if error {
        label.add_css_class("notebook-error");
    }
    if total <= MAX_OUTPUT_LINES {
        return label.upcast();
    }
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.append(&label);
    column.append(
        &gtk::Label::builder()
            .label(format!(
                "{} more lines, in the file",
                total - MAX_OUTPUT_LINES
            ))
            .xalign(0.0)
            .css_classes(["caption", "dim-label"])
            .build(),
    );
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
            for mime in ["image/png", "image/jpeg"] {
                if data.get(mime).is_some() {
                    let width = output["metadata"][mime]["width"].as_f64();
                    return Some(picture(
                        &joined(&data[mime]),
                        width,
                        ready.map(Readiness::hold),
                    ));
                }
            }
            if data.get("image/svg+xml").is_some() {
                return Some(crate::mermaid::svg_figure(
                    &joined(&data["image/svg+xml"]),
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

/// A base64 picture, decoded off this thread and shown scaled down to the
/// column, never up — and never wider than `width`, the size its output
/// says to show it at, when it says one: a retina figure is drawn at twice
/// the size it stands for.
fn picture(
    base64: &str,
    width: Option<f64>,
    hold: Option<crate::markdown_view::Hold>,
) -> gtk::Widget {
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::ScaleDown)
        .can_shrink(true)
        .halign(gtk::Align::Start)
        .build();
    let shown: gtk::Widget = match width.filter(|w| w.is_finite() && *w >= 1.0) {
        Some(width) => adw::Clamp::builder()
            .maximum_size(width.ceil() as i32)
            .tightening_threshold(width.ceil() as i32)
            .halign(gtk::Align::Start)
            .child(&picture)
            .build()
            .upcast(),
        None => picture.clone().upcast(),
    };
    let encoded: String = base64.chars().filter(|c| !c.is_whitespace()).collect();
    let weak = picture.downgrade();
    glib::spawn_future_local(async move {
        let _hold = hold;
        let decoded = crate::runtime::runtime()
            .spawn_blocking(move || -> Result<gtk::gdk::Texture, String> {
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(encoded.as_bytes())
                    .map_err(|e| format!("its base64 does not decode: {e}"))?;
                gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes))
                    .map_err(|e| format!("it is not a picture this build decodes: {e}"))
            })
            .await;
        let Some(picture) = weak.upgrade() else {
            return;
        };
        match decoded {
            Ok(Ok(texture)) => picture.set_paintable(Some(&texture)),
            Ok(Err(why)) => {
                tracing::info!("a notebook picture was not shown: {why}");
                picture.set_tooltip_text(Some(&format!("Not shown: {why}")));
            }
            Err(e) => tracing::info!("a notebook picture was not shown: {e}"),
        }
    });
    shown
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

//! Jupyter's interactive widgets (ipywidgets), drawn as GTK widgets.
//!
//! A notebook's widgets are models in the kernel, each on a comm, and a
//! frontend that keeps a copy of each model's state and draws views of it
//! (David, 2026-10-11: "I want interactive widgets to work in the
//! preview"). This is that frontend, in this process's own toolkit rather
//! than a web page: ipywidgets' views are JavaScript a project supplies,
//! and the editor draws project content without a web engine
//! (ARCHITECTURE → "Center: editor"). So the core widgets are drawn
//! natively — sliders, number and text fields, checkboxes and toggles,
//! dropdowns, radio buttons and selections, buttons, labels, progress
//! bars, Play, images, colours, the boxes, tabs, and accordions that lay
//! them out, and the Output widget that `interact` draws into — and a
//! widget of another library (its views are its own JavaScript) shows the
//! text the kernel sent beside it.
//!
//! The protocol is ipywidgets' own, version 2 (`jupyter.widget` comms;
//! the kernel's `jupyter_client` relays it, `crate::kernel`): a model's
//! state arrives on `comm_open`, changes as `update`; a change made here is
//! sent as an `update` and comes back as an `echo_update`, which is ignored
//! for a key this side has changed since, so a slider being dragged does
//! not jump back; a button's click is a `custom` event. An Output widget
//! takes every output whose parent is the request its `msg_id` names,
//! which is how a callback's prints and plots land inside it. `jslink`s
//! are kept here, as the protocol has them kept by the frontend.
//!
//! A notebook saved with its widgets' state has its views drawn from that
//! state when no kernel is running, unusable until a cell is run.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::glib;
use serde_json::{Map, Value};

/// How often a dragged slider tells the kernel where it is: often enough
/// for a plot under `interact` to follow, never per pixel.
const THROTTLE: std::time::Duration = std::time::Duration::from_millis(60);
/// How long a widget that does not update continuously waits for the
/// change to settle before it is sent.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

/// The prefix a model reference carries in another model's state.
const MODEL_REF: &str = "IPY_MODEL_";

type Listener = Box<dyn Fn(&Rc<Model>, &[String]) -> bool>;

/// One widget model: its state as the kernel last said or this side last
/// set, and the views drawn of it.
pub struct Model {
    pub id: String,
    pub name: String,
    state: RefCell<Map<String, Value>>,
    /// Binary state, by its top-level key (an Image's `value`).
    buffers: RefCell<HashMap<String, Vec<u8>>>,
    listeners: RefCell<Vec<Listener>>,
    /// Keys changed here whose echo has not come back yet.
    pending: RefCell<HashMap<String, u32>>,
    /// Whether it is connected to a running kernel: a closed model, or
    /// one read from a saved notebook, is drawn and not used.
    live: Cell<bool>,
    /// An Output widget's `clear_output(wait=True)`: clear at the next
    /// output rather than now.
    clear_on_next: Cell<bool>,
}

impl Model {
    fn get(&self, key: &str) -> Value {
        self.state.borrow().get(key).cloned().unwrap_or(Value::Null)
    }

    fn f64(&self, key: &str) -> f64 {
        self.get(key).as_f64().unwrap_or(0.0)
    }

    fn str(&self, key: &str) -> String {
        self.get(key).as_str().unwrap_or_default().to_string()
    }

    fn bool(&self, key: &str) -> bool {
        self.get(key).as_bool().unwrap_or(false)
    }

    fn labels(&self) -> Vec<String> {
        self.get("_options_labels")
            .as_array()
            .map(|labels| {
                labels
                    .iter()
                    .map(|l| l.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Call `listener` on every change from here on; it says `false` once
    /// its view is gone, and is dropped.
    fn listen(&self, listener: impl Fn(&Rc<Model>, &[String]) -> bool + 'static) {
        self.listeners.borrow_mut().push(Box::new(listener));
    }
}

/// The models of one notebook's kernel, and how to reach them.
pub struct Manager {
    models: RefCell<HashMap<String, Rc<Model>>>,
    /// How a message reaches a model's comm in the kernel.
    send: RefCell<Option<Rc<dyn Fn(&str, Value, Vec<Vec<u8>>)>>>,
    /// How an output is drawn, for an Output widget's outputs: the
    /// notebook's own drawing of one.
    draw_output: RefCell<Option<Rc<dyn Fn(&Value) -> Option<gtk::Widget>>>>,
}

impl Manager {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            models: RefCell::new(HashMap::new()),
            send: RefCell::new(None),
            draw_output: RefCell::new(None),
        })
    }

    pub fn set_sender(&self, send: Rc<dyn Fn(&str, Value, Vec<Vec<u8>>)>) {
        *self.send.borrow_mut() = Some(send);
    }

    pub fn set_output_drawer(&self, draw: Rc<dyn Fn(&Value) -> Option<gtk::Widget>>) {
        *self.draw_output.borrow_mut() = Some(draw);
    }

    pub fn model(&self, id: &str) -> Option<Rc<Model>> {
        self.models.borrow().get(id).cloned()
    }

    /// The kernel went: every model kept for what it shows, none usable.
    pub fn disconnect(&self) {
        let models: Vec<Rc<Model>> = self.models.borrow().values().cloned().collect();
        for model in models {
            if model.live.replace(false) {
                notify(&model, &["_live".to_string()]);
            }
        }
    }

    /// A saved notebook's widget state
    /// (`metadata.widgets["application/vnd.jupyter.widget-state+json"].state`),
    /// for drawing its widgets when no kernel is running. A model the
    /// kernel has already opened is not replaced.
    pub fn load_saved(&self, state: &Value) {
        let Some(saved) = state.as_object() else {
            return;
        };
        let mut models = self.models.borrow_mut();
        for (id, entry) in saved {
            if models.contains_key(id) {
                continue;
            }
            let Some(name) = entry["model_name"].as_str() else {
                continue;
            };
            let state = entry["state"].as_object().cloned().unwrap_or_default();
            models.insert(
                id.clone(),
                new_model(id, name, state, HashMap::new(), false),
            );
        }
    }

    /// A comm message from the kernel.
    pub fn on_comm(self: &Rc<Self>, kind: &str, content: &Value, buffers: Vec<Vec<u8>>) {
        let id = content["comm_id"].as_str().unwrap_or_default().to_string();
        let data = &content["data"];
        match kind {
            "comm_open" => {
                if content["target_name"].as_str() != Some("jupyter.widget") {
                    return;
                }
                let (state, binary) = with_buffers(&data["state"], &data["buffer_paths"], buffers);
                let name = state
                    .get("_model_name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let model = new_model(&id, &name, state, binary, true);
                self.models.borrow_mut().insert(id, model);
            }
            "comm_msg" => {
                let Some(model) = self.model(&id) else {
                    return;
                };
                match data["method"].as_str() {
                    Some("update") | Some("echo_update") => {
                        let echo = data["method"].as_str() == Some("echo_update");
                        let (patch, binary) =
                            with_buffers(&data["state"], &data["buffer_paths"], buffers);
                        let mut changed = Vec::new();
                        for (key, value) in patch {
                            if echo {
                                let mut pending = model.pending.borrow_mut();
                                if let Some(count) = pending.get_mut(&key) {
                                    *count -= 1;
                                    if *count == 0 {
                                        pending.remove(&key);
                                    }
                                    continue;
                                }
                            }
                            model.state.borrow_mut().insert(key.clone(), value);
                            changed.push(key);
                        }
                        for (key, bytes) in binary {
                            model.buffers.borrow_mut().insert(key.clone(), bytes);
                            if !changed.contains(&key) {
                                changed.push(key);
                            }
                        }
                        if !changed.is_empty() {
                            self.changed(&model, &changed);
                        }
                    }
                    _ => {}
                }
            }
            "comm_close" => {
                if let Some(model) = self.model(&id) {
                    model.live.set(false);
                    notify(&model, &["_live".to_string()]);
                }
            }
            _ => {}
        }
    }

    /// Set a key of a model here, and tell the kernel.
    fn set(self: &Rc<Self>, model: &Rc<Model>, key: &str, value: Value) {
        if !model.live.get() || model.get(key) == value {
            return;
        }
        model
            .state
            .borrow_mut()
            .insert(key.to_string(), value.clone());
        *model
            .pending
            .borrow_mut()
            .entry(key.to_string())
            .or_insert(0) += 1;
        let mut patch = Map::new();
        patch.insert(key.to_string(), value);
        self.send_to(
            model,
            serde_json::json!({"method": "update", "state": patch, "buffer_paths": []}),
        );
        self.changed(model, &[key.to_string()]);
    }

    /// A `custom` message — a button's click, a text field's submit.
    fn custom(&self, model: &Model, content: Value) {
        if model.live.get() {
            self.send_to(
                model,
                serde_json::json!({"method": "custom", "content": content}),
            );
        }
    }

    fn send_to(&self, model: &Model, data: Value) {
        if let Some(send) = self.send.borrow().as_ref() {
            send(&model.id, data, Vec::new());
        }
    }

    /// A model changed: its views told, and the links that start at it
    /// followed.
    fn changed(self: &Rc<Self>, model: &Rc<Model>, keys: &[String]) {
        notify(model, keys);
        let links: Vec<Rc<Model>> = self
            .models
            .borrow()
            .values()
            .filter(|m| {
                m.live.get() && matches!(m.name.as_str(), "LinkModel" | "DirectionalLinkModel")
            })
            .cloned()
            .collect();
        for link in links {
            let ends = |key: &str| -> Option<(String, String)> {
                let end = link.get(key);
                let id = end.get(0)?.as_str()?.strip_prefix(MODEL_REF)?.to_string();
                Some((id, end.get(1)?.as_str()?.to_string()))
            };
            let (Some(source), Some(target)) = (ends("source"), ends("target")) else {
                continue;
            };
            let both_ways = link.name == "LinkModel";
            for (from, to) in [(&source, &target), (&target, &source)] {
                let followed = *from == source || both_ways;
                if followed && from.0 == model.id && keys.contains(&from.1) {
                    if let Some(other) = self.model(&to.0) {
                        self.set(&other, &to.1, model.get(&from.1));
                    }
                }
            }
        }
    }

    /// An output whose parent is `parent`, for the Output widget capturing
    /// that request's output, if one is; `false` when none takes it.
    pub fn capture(self: &Rc<Self>, parent: &str, kind: &str, content: &Value) -> bool {
        if parent.is_empty() {
            return false;
        }
        let output_model = self
            .models
            .borrow()
            .values()
            .find(|m| m.live.get() && m.name == "OutputModel" && m.str("msg_id") == parent)
            .cloned();
        let Some(model) = output_model else {
            return false;
        };
        if kind == "clear_output" {
            if content["wait"].as_bool().unwrap_or(false) {
                model.clear_on_next.set(true);
            } else {
                model
                    .state
                    .borrow_mut()
                    .insert("outputs".into(), Value::Array(Vec::new()));
                notify(&model, &["outputs".to_string()]);
            }
            return true;
        }
        let Some(output) = crate::notebook::nbformat_output(kind, content.clone()) else {
            // `execute_input` and the like: the request's, not shown.
            return true;
        };
        let mut outputs: Vec<Value> = if model.clear_on_next.replace(false) {
            Vec::new()
        } else {
            model.get("outputs").as_array().cloned().unwrap_or_default()
        };
        crate::notebook::push_output(&mut outputs, output);
        model
            .state
            .borrow_mut()
            .insert("outputs".into(), Value::Array(outputs));
        notify(&model, &["outputs".to_string()]);
        true
    }

    /// A view of model `id`; `None` when there is no such model or it is
    /// not one drawn here.
    pub fn view(self: &Rc<Self>, id: &str) -> Option<gtk::Widget> {
        let model = self.model(id)?;
        let view = build_view(self, &model)?;
        // Usable while the kernel holds the model; drawn, and said to be
        // waiting for one, when it does not.
        let set_live = |view: &gtk::Widget, model: &Model| {
            view.set_sensitive(model.live.get());
            view.set_tooltip_text(if model.live.get() {
                None
            } else {
                Some("Run the notebook's cells to use this widget")
            });
        };
        set_live(&view, &model);
        let weak = view.downgrade();
        model.listen(move |model, keys| {
            let Some(view) = weak.upgrade() else {
                return false;
            };
            if keys.iter().any(|k| k == "_live") {
                set_live(&view, model);
            }
            true
        });
        Some(view)
    }
}

fn new_model(
    id: &str,
    name: &str,
    state: Map<String, Value>,
    buffers: HashMap<String, Vec<u8>>,
    live: bool,
) -> Rc<Model> {
    Rc::new(Model {
        id: id.to_string(),
        name: name.to_string(),
        state: RefCell::new(state),
        buffers: RefCell::new(buffers),
        listeners: RefCell::new(Vec::new()),
        pending: RefCell::new(HashMap::new()),
        live: Cell::new(live),
        clear_on_next: Cell::new(false),
    })
}

/// A state and its binary buffers, as the protocol sends them: each buffer
/// belongs at a path in the state. A path one key long — the only kind the
/// core widgets use — is kept by that key; deeper ones are dropped.
fn with_buffers(
    state: &Value,
    paths: &Value,
    buffers: Vec<Vec<u8>>,
) -> (Map<String, Value>, HashMap<String, Vec<u8>>) {
    let state = state.as_object().cloned().unwrap_or_default();
    let mut binary = HashMap::new();
    for (path, bytes) in paths.as_array().into_iter().flatten().zip(buffers) {
        if let Some([key]) = path.as_array().map(Vec::as_slice) {
            if let Some(key) = key.as_str() {
                binary.insert(key.to_string(), bytes);
            }
        }
    }
    (state, binary)
}

/// Tell a model's views that `keys` changed. Listeners are taken out while
/// they run, so one that sets another key — a link, a view's own echo —
/// does not find them borrowed; ones added meanwhile are kept.
fn notify(model: &Rc<Model>, keys: &[String]) {
    let listeners = std::mem::take(&mut *model.listeners.borrow_mut());
    let kept: Vec<Listener> = listeners
        .into_iter()
        .filter(|listener| listener(model, keys))
        .collect();
    let mut slot = model.listeners.borrow_mut();
    let added = std::mem::take(&mut *slot);
    *slot = kept;
    slot.extend(added);
}

/// Run `apply` on a view's widget whenever one of `keys` changes, with
/// `guard` set, so the widget's own change signal knows the change is the
/// model's and does not send it back.
fn bind<W: IsA<gtk::Widget>>(
    model: &Rc<Model>,
    widget: &W,
    guard: &Rc<Cell<bool>>,
    keys: &'static [&'static str],
    apply: impl Fn(&W, &Model) + 'static,
) {
    let weak = widget.downgrade();
    let guard = guard.clone();
    if let Some(widget) = weak.upgrade() {
        guard.set(true);
        apply(&widget, model);
        guard.set(false);
    }
    model.listen(move |model, changed| {
        let Some(widget) = weak.upgrade() else {
            return false;
        };
        if changed.iter().any(|k| keys.contains(&k.as_str())) {
            guard.set(true);
            apply(&widget, model);
            guard.set(false);
        }
        true
    });
}

/// Sends a widget's value at most every [`THROTTLE`] while it changes —
/// or once it has settled for [`SETTLE`], for a widget that does not
/// update continuously — the last value always sent.
struct Sender {
    manager: Weak<Manager>,
    model: Rc<Model>,
    key: &'static str,
    latest: RefCell<Option<Value>>,
    timer: RefCell<Option<glib::SourceId>>,
}

impl Sender {
    fn new(manager: &Rc<Manager>, model: &Rc<Model>, key: &'static str) -> Rc<Self> {
        Rc::new(Self {
            manager: Rc::downgrade(manager),
            model: model.clone(),
            key,
            latest: RefCell::new(None),
            timer: RefCell::new(None),
        })
    }

    fn push(self: &Rc<Self>, value: Value) {
        *self.latest.borrow_mut() = Some(value);
        let continuous = self
            .model
            .get("continuous_update")
            .as_bool()
            .unwrap_or(true);
        if continuous && self.timer.borrow().is_some() {
            return;
        }
        if let Some(id) = self.timer.borrow_mut().take() {
            id.remove();
        }
        let this = self.clone();
        let id =
            glib::timeout_add_local_once(if continuous { THROTTLE } else { SETTLE }, move || {
                this.timer.borrow_mut().take();
                if let (Some(manager), Some(value)) =
                    (this.manager.upgrade(), this.latest.borrow_mut().take())
                {
                    manager.set(&this.model, this.key, value);
                }
            });
        *self.timer.borrow_mut() = Some(id);
    }
}

/// A view by the model's kind; `None` for one not drawn here.
fn build_view(manager: &Rc<Manager>, model: &Rc<Model>) -> Option<gtk::Widget> {
    let view = match model.name.as_str() {
        "IntSliderModel" | "FloatSliderModel" | "FloatLogSliderModel" => slider(manager, model),
        "IntRangeSliderModel" | "FloatRangeSliderModel" => range_slider(manager, model),
        "SelectionSliderModel" => selection_slider(manager, model),
        "IntTextModel" | "FloatTextModel" | "BoundedIntTextModel" | "BoundedFloatTextModel" => {
            number(manager, model)
        }
        "TextModel" | "PasswordModel" | "ComboboxModel" => text(manager, model),
        "TextareaModel" => textarea(manager, model),
        "CheckboxModel" => checkbox(manager, model),
        "ToggleButtonModel" => toggle(manager, model),
        "DropdownModel" => dropdown(manager, model),
        "RadioButtonsModel" => radios(manager, model),
        "ToggleButtonsModel" => toggles(manager, model),
        "SelectModel" | "SelectMultipleModel" => select(manager, model),
        "ButtonModel" => button(manager, model),
        "LabelModel" | "HTMLModel" | "HTMLMathModel" => label(model),
        "IntProgressModel" | "FloatProgressModel" => progress(model),
        "BoxModel" | "HBoxModel" | "VBoxModel" | "GridBoxModel" => boxed(manager, model),
        "TabModel" | "AccordionModel" | "StackModel" => tabs(manager, model),
        "OutputModel" => output(manager, model),
        "ImageModel" => image(model),
        "PlayModel" => play(manager, model),
        "ColorPickerModel" => colour(manager, model),
        "ValidModel" => valid(model),
        _ => return None,
    };
    Some(view)
}

/// A control with its description before it, as ipywidgets lays one out.
fn described(model: &Rc<Model>, control: &impl IsA<gtk::Widget>) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let description = gtk::Label::builder()
        .xalign(1.0)
        .width_chars(10)
        .max_width_chars(24)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let guard = Rc::new(Cell::new(false));
    bind(
        model,
        &description,
        &guard,
        &["description"],
        |label, model| {
            let text = plain_text(&model.str("description"));
            label.set_label(&text);
            label.set_visible(!text.is_empty());
        },
    );
    row.append(&description);
    row.append(control);
    row.upcast()
}

fn readout() -> gtk::Label {
    gtk::Label::builder()
        .css_classes(["monospace", "numeric"])
        .width_chars(6)
        .xalign(0.0)
        .build()
}

fn slider(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    let log = model.name == "FloatLogSliderModel";
    let int = model.name == "IntSliderModel";
    let vertical = model.str("orientation") == "vertical";
    let scale = gtk::Scale::builder()
        .orientation(if vertical {
            gtk::Orientation::Vertical
        } else {
            gtk::Orientation::Horizontal
        })
        .inverted(vertical)
        .draw_value(false)
        .hexpand(!vertical)
        .width_request(if vertical { -1 } else { 240 })
        .height_request(if vertical { 160 } else { -1 })
        .build();
    let shown = readout();
    let guard = Rc::new(Cell::new(false));
    bind(
        model,
        &scale,
        &guard,
        &["min", "max", "step", "value", "base"],
        move |scale, model| {
            let base = model.get("base").as_f64().unwrap_or(10.0);
            let (min, max) = (model.f64("min"), model.f64("max"));
            let step = model
                .get("step")
                .as_f64()
                .filter(|s| *s > 0.0)
                .unwrap_or(1.0);
            scale.set_range(min, max.max(min));
            scale.set_increments(step, step * 10.0);
            scale.set_digits(if int { 0 } else { 6 });
            let value = model.f64("value");
            scale.set_value(if log { value.log(base) } else { value });
        },
    );
    bind(
        model,
        &shown,
        &guard,
        &["value", "readout", "readout_format"],
        move |label, model| {
            label.set_visible(model.get("readout").as_bool().unwrap_or(true));
            label.set_label(&number_text(
                model.f64("value"),
                &model.str("readout_format"),
                int,
            ));
        },
    );
    let sender = Sender::new(manager, model, "value");
    {
        let guard = guard.clone();
        let model = model.clone();
        scale.connect_value_changed(move |scale| {
            if guard.get() {
                return;
            }
            let position = scale.value();
            let base = model.get("base").as_f64().unwrap_or(10.0);
            let value = if log {
                Value::from(base.powf(position))
            } else if int {
                Value::from(position.round() as i64)
            } else {
                Value::from(position)
            };
            sender.push(value);
        });
    }
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.append(&scale);
    row.append(&shown);
    described(model, &row)
}

fn range_slider(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    let int = model.name == "IntRangeSliderModel";
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let shown = readout();
    let guard = Rc::new(Cell::new(false));
    let ends: Vec<gtk::Scale> = (0..2)
        .map(|_| {
            gtk::Scale::builder()
                .draw_value(false)
                .hexpand(true)
                .width_request(240)
                .build()
        })
        .collect();
    for (end, scale) in ends.iter().enumerate() {
        column.append(scale);
        bind(
            model,
            scale,
            &guard,
            &["min", "max", "step", "value"],
            move |scale, model| {
                let step = model
                    .get("step")
                    .as_f64()
                    .filter(|s| *s > 0.0)
                    .unwrap_or(1.0);
                scale.set_range(model.f64("min"), model.f64("max").max(model.f64("min")));
                scale.set_increments(step, step * 10.0);
                scale.set_digits(if int { 0 } else { 6 });
                if let Some(value) = model.get("value").get(end).and_then(Value::as_f64) {
                    scale.set_value(value);
                }
            },
        );
    }
    bind(
        model,
        &shown,
        &guard,
        &["value", "readout_format"],
        move |label, model| {
            let value = model.get("value");
            let at = |i: usize| value.get(i).and_then(Value::as_f64).unwrap_or(0.0);
            let format = model.str("readout_format");
            label.set_label(&format!(
                "{} – {}",
                number_text(at(0), &format, int),
                number_text(at(1), &format, int)
            ));
        },
    );
    let sender = Sender::new(manager, model, "value");
    for scale in &ends {
        let (guard, ends, sender) = (guard.clone(), ends.clone(), sender.clone());
        scale.connect_value_changed(move |_| {
            if guard.get() {
                return;
            }
            let mut pair = [ends[0].value(), ends[1].value()];
            pair.sort_by(f64::total_cmp);
            sender.push(if int {
                serde_json::json!([pair[0].round() as i64, pair[1].round() as i64])
            } else {
                serde_json::json!(pair)
            });
        });
    }
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.append(&column);
    row.append(&shown);
    described(model, &row)
}

fn selection_slider(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    let scale = gtk::Scale::builder()
        .draw_value(false)
        .hexpand(true)
        .width_request(240)
        .build();
    let shown = readout();
    let guard = Rc::new(Cell::new(false));
    bind(
        model,
        &scale,
        &guard,
        &["_options_labels", "index"],
        |scale, model| {
            let count = model.labels().len().max(1) as f64;
            scale.set_range(0.0, count - 1.0);
            scale.set_increments(1.0, 1.0);
            scale.set_digits(0);
            scale.set_value(model.f64("index"));
        },
    );
    bind(
        model,
        &shown,
        &guard,
        &["_options_labels", "index"],
        |label, model| {
            let index = model.get("index").as_u64().unwrap_or(0) as usize;
            label.set_label(model.labels().get(index).map(String::as_str).unwrap_or(""));
        },
    );
    let sender = Sender::new(manager, model, "index");
    {
        let guard = guard.clone();
        scale.connect_value_changed(move |scale| {
            if !guard.get() {
                sender.push(Value::from(scale.value().round() as i64));
            }
        });
    }
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.append(&scale);
    row.append(&shown);
    described(model, &row)
}

fn number(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    let int = model.name.contains("Int");
    let bounded = model.name.starts_with("Bounded");
    let spin = gtk::SpinButton::builder().width_chars(10).build();
    let guard = Rc::new(Cell::new(false));
    bind(
        model,
        &spin,
        &guard,
        &["min", "max", "step", "value"],
        move |spin, model| {
            let (min, max) = if bounded {
                (model.f64("min"), model.f64("max"))
            } else {
                (f64::MIN, f64::MAX)
            };
            let step = model
                .get("step")
                .as_f64()
                .filter(|s| *s > 0.0)
                .unwrap_or(if int { 1.0 } else { 0.1 });
            spin.set_range(min, max.max(min));
            spin.set_increments(step, step * 10.0);
            spin.set_digits(if int { 0 } else { 4 });
            spin.set_value(model.f64("value"));
        },
    );
    let sender = Sender::new(manager, model, "value");
    {
        let guard = guard.clone();
        spin.connect_value_changed(move |spin| {
            if !guard.get() {
                sender.push(if int {
                    Value::from(spin.value().round() as i64)
                } else {
                    Value::from(spin.value())
                });
            }
        });
    }
    described(model, &spin)
}

fn text(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    let entry: gtk::Entry = if model.name == "PasswordModel" {
        gtk::Entry::builder().visibility(false).build()
    } else {
        gtk::Entry::new()
    };
    entry.set_width_chars(24);
    let guard = Rc::new(Cell::new(false));
    bind(
        model,
        &entry,
        &guard,
        &["value", "placeholder"],
        |entry, model| {
            if entry.text() != model.str("value") {
                entry.set_text(&model.str("value"));
            }
            entry.set_placeholder_text(Some(&model.str("placeholder")));
        },
    );
    let sender = Sender::new(manager, model, "value");
    {
        let guard = guard.clone();
        entry.connect_changed(move |entry| {
            if !guard.get() {
                sender.push(Value::from(entry.text().to_string()));
            }
        });
    }
    {
        let (manager, model) = (Rc::downgrade(manager), model.clone());
        entry.connect_activate(move |_| {
            if let Some(manager) = manager.upgrade() {
                manager.custom(&model, serde_json::json!({"event": "submit"}));
            }
        });
    }
    described(model, &entry)
}

fn textarea(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    let view = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(6)
        .bottom_margin(6)
        .left_margin(6)
        .right_margin(6)
        .hexpand(true)
        .height_request(80)
        .build();
    let frame = gtk::Frame::builder().child(&view).hexpand(true).build();
    let guard = Rc::new(Cell::new(false));
    bind(model, &view, &guard, &["value"], |view, model| {
        let buffer = view.buffer();
        let now = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
        if now != model.str("value") {
            buffer.set_text(&model.str("value"));
        }
    });
    let sender = Sender::new(manager, model, "value");
    {
        let guard = guard.clone();
        view.buffer().connect_changed(move |buffer| {
            if !guard.get() {
                sender.push(Value::from(
                    buffer
                        .text(&buffer.start_iter(), &buffer.end_iter(), false)
                        .to_string(),
                ));
            }
        });
    }
    described(model, &frame)
}

fn checkbox(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    let check = gtk::CheckButton::new();
    let guard = Rc::new(Cell::new(false));
    bind(
        model,
        &check,
        &guard,
        &["value", "description"],
        |check, model| {
            check.set_active(model.bool("value"));
            check.set_label(Some(&plain_text(&model.str("description"))));
        },
    );
    let (manager, model2) = (Rc::downgrade(manager), model.clone());
    check.connect_toggled(move |check| {
        if !guard.get() {
            if let Some(manager) = manager.upgrade() {
                manager.set(&model2, "value", Value::from(check.is_active()));
            }
        }
    });
    // Indented by default to where the other controls start, past the
    // description column, as ipywidgets lays a checkbox out.
    if !model.get("indent").as_bool().unwrap_or(true) {
        return check.upcast();
    }
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.append(&gtk::Label::builder().width_chars(10).build());
    row.append(&check);
    row.upcast()
}

fn toggle(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    let button = gtk::ToggleButton::new();
    button.set_halign(gtk::Align::Start);
    let guard = Rc::new(Cell::new(false));
    bind(
        model,
        &button,
        &guard,
        &["value", "description", "button_style"],
        |button, model| {
            button.set_active(model.bool("value"));
            button.set_label(&plain_text(&model.str("description")));
            style_button(button, &model.str("button_style"));
        },
    );
    let (manager, model2) = (Rc::downgrade(manager), model.clone());
    button.connect_toggled(move |button| {
        if !guard.get() {
            if let Some(manager) = manager.upgrade() {
                manager.set(&model2, "value", Value::from(button.is_active()));
            }
        }
    });
    button.upcast()
}

fn dropdown(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    let list = gtk::StringList::new(&[]);
    let drop = gtk::DropDown::builder().model(&list).build();
    let guard = Rc::new(Cell::new(false));
    bind(
        model,
        &drop,
        &guard,
        &["_options_labels", "index"],
        move |drop, model| {
            let labels = model.labels();
            let list = drop.model().and_downcast::<gtk::StringList>().unwrap();
            let now: Vec<String> = (0..list.n_items())
                .filter_map(|i| list.string(i).map(|s| s.to_string()))
                .collect();
            if now != labels {
                let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                list.splice(0, list.n_items(), &refs);
            }
            drop.set_selected(
                model
                    .get("index")
                    .as_u64()
                    .map_or(gtk::INVALID_LIST_POSITION, |i| i as u32),
            );
        },
    );
    let (manager, model2) = (Rc::downgrade(manager), model.clone());
    drop.connect_selected_notify(move |drop| {
        if guard.get() {
            return;
        }
        if let Some(manager) = manager.upgrade() {
            let selected = drop.selected();
            manager.set(
                &model2,
                "index",
                if selected == gtk::INVALID_LIST_POSITION {
                    Value::Null
                } else {
                    Value::from(selected)
                },
            );
        }
    });
    described(model, &drop)
}

/// A one-of-several choice drawn as a group of buttons that rebuilds when
/// its options change: radio buttons down, or toggle buttons across.
fn choice_group(manager: &Rc<Manager>, model: &Rc<Model>, across: bool) -> gtk::Widget {
    let holder = gtk::Box::new(
        if across {
            gtk::Orientation::Horizontal
        } else {
            gtk::Orientation::Vertical
        },
        if across { 0 } else { 4 },
    );
    if across {
        holder.add_css_class("linked");
    }
    let guard = Rc::new(Cell::new(false));
    let built: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let manager_weak = Rc::downgrade(manager);
    let model_for = model.clone();
    let guard_for = guard.clone();
    bind(
        model,
        &holder,
        &guard,
        &["_options_labels", "index"],
        move |holder, model| {
            let labels = model.labels();
            if *built.borrow() != labels {
                while let Some(child) = holder.first_child() {
                    holder.remove(&child);
                }
                let mut first: Option<gtk::CheckButton> = None;
                let mut first_toggle: Option<gtk::ToggleButton> = None;
                for (index, text) in labels.iter().enumerate() {
                    let set = {
                        let (manager, model, guard) =
                            (manager_weak.clone(), model_for.clone(), guard_for.clone());
                        move |active: bool| {
                            if guard.get() || !active {
                                return;
                            }
                            if let Some(manager) = manager.upgrade() {
                                manager.set(&model, "index", Value::from(index));
                            }
                        }
                    };
                    if across {
                        let button = gtk::ToggleButton::with_label(text);
                        if let Some(first) = &first_toggle {
                            button.set_group(Some(first));
                        } else {
                            first_toggle = Some(button.clone());
                        }
                        button.connect_toggled(move |b| set(b.is_active()));
                        holder.append(&button);
                    } else {
                        let button = gtk::CheckButton::with_label(text);
                        if let Some(first) = &first {
                            button.set_group(Some(first));
                        } else {
                            first = Some(button.clone());
                        }
                        button.connect_toggled(move |b| set(b.is_active()));
                        holder.append(&button);
                    }
                }
                *built.borrow_mut() = labels;
            }
            let selected = model.get("index").as_u64().map(|i| i as usize);
            let mut child = holder.first_child();
            let mut index = 0;
            while let Some(widget) = child {
                let on = selected == Some(index);
                if let Some(check) = widget.downcast_ref::<gtk::CheckButton>() {
                    check.set_active(on);
                } else if let Some(toggle) = widget.downcast_ref::<gtk::ToggleButton>() {
                    toggle.set_active(on);
                }
                child = widget.next_sibling();
                index += 1;
            }
        },
    );
    described(model, &holder)
}

fn radios(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    choice_group(manager, model, false)
}

fn toggles(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    choice_group(manager, model, true)
}

fn select(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    let multiple = model.name == "SelectMultipleModel";
    let list = gtk::ListBox::builder()
        .selection_mode(if multiple {
            gtk::SelectionMode::Multiple
        } else {
            gtk::SelectionMode::Single
        })
        .css_classes(["boxed-list"])
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(120)
        .width_request(220)
        .build();
    let guard = Rc::new(Cell::new(false));
    let built: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    bind(
        model,
        &list,
        &guard,
        &["_options_labels", "index"],
        move |list, model| {
            let labels = model.labels();
            if *built.borrow() != labels {
                list.remove_all();
                for text in &labels {
                    list.append(
                        &gtk::Label::builder()
                            .label(text)
                            .xalign(0.0)
                            .margin_start(8)
                            .margin_end(8)
                            .margin_top(4)
                            .margin_bottom(4)
                            .build(),
                    );
                }
                *built.borrow_mut() = labels;
            }
            list.unselect_all();
            let chosen: Vec<i64> = match model.get("index") {
                Value::Array(indices) => indices.iter().filter_map(Value::as_i64).collect(),
                Value::Number(n) => n.as_i64().into_iter().collect(),
                _ => Vec::new(),
            };
            for index in chosen {
                if let Some(row) = list.row_at_index(index as i32) {
                    list.select_row(Some(&row));
                }
            }
        },
    );
    let (manager, model2) = (Rc::downgrade(manager), model.clone());
    list.connect_selected_rows_changed(move |list| {
        if guard.get() {
            return;
        }
        let Some(manager) = manager.upgrade() else {
            return;
        };
        let rows: Vec<i64> = list
            .selected_rows()
            .iter()
            .map(|row| row.index() as i64)
            .collect();
        let value = if multiple {
            Value::from(rows)
        } else {
            rows.first().map_or(Value::Null, |i| Value::from(*i))
        };
        manager.set(&model2, "index", value);
    });
    described(model, &scroller)
}

fn style_button(button: &impl IsA<gtk::Widget>, style: &str) {
    for class in [
        "suggested-action",
        "destructive-action",
        "success",
        "warning",
    ] {
        button.remove_css_class(class);
    }
    match style {
        "primary" | "info" => button.add_css_class("suggested-action"),
        "danger" => button.add_css_class("destructive-action"),
        "success" => button.add_css_class("success"),
        "warning" => button.add_css_class("warning"),
        _ => {}
    }
}

fn button(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    let button = gtk::Button::new();
    button.set_halign(gtk::Align::Start);
    let guard = Rc::new(Cell::new(false));
    bind(
        model,
        &button,
        &guard,
        &["description", "button_style", "tooltip"],
        |button, model| {
            let text = plain_text(&model.str("description"));
            button.set_label(if text.is_empty() { "Button" } else { &text });
            style_button(button, &model.str("button_style"));
        },
    );
    let (manager, model2) = (Rc::downgrade(manager), model.clone());
    button.connect_clicked(move |_| {
        if let Some(manager) = manager.upgrade() {
            manager.custom(&model2, serde_json::json!({"event": "click"}));
        }
    });
    button.upcast()
}

fn label(model: &Rc<Model>) -> gtk::Widget {
    let label = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .selectable(true)
        .build();
    let guard = Rc::new(Cell::new(false));
    let html = model.name != "LabelModel";
    bind(model, &label, &guard, &["value"], move |label, model| {
        let value = model.str("value");
        label.set_label(&if html { plain_text(&value) } else { value });
    });
    described(model, &label)
}

fn progress(model: &Rc<Model>) -> gtk::Widget {
    let bar = gtk::ProgressBar::builder()
        .hexpand(true)
        .width_request(240)
        .valign(gtk::Align::Center)
        .build();
    let guard = Rc::new(Cell::new(false));
    bind(
        model,
        &bar,
        &guard,
        &["value", "min", "max", "bar_style"],
        |bar, model| {
            let (min, max) = (model.f64("min"), model.f64("max"));
            let span = (max - min).max(f64::EPSILON);
            bar.set_fraction(((model.f64("value") - min) / span).clamp(0.0, 1.0));
            for class in ["success", "warning", "error"] {
                bar.remove_css_class(class);
            }
            match model.str("bar_style").as_str() {
                "success" => bar.add_css_class("success"),
                "warning" => bar.add_css_class("warning"),
                "danger" => bar.add_css_class("error"),
                _ => {}
            }
        },
    );
    described(model, &bar)
}

/// The views of a model's children, from its `children` references.
fn children(manager: &Rc<Manager>, model: &Model) -> Vec<gtk::Widget> {
    model
        .get("children")
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|r| r.strip_prefix(MODEL_REF))
        .map(|id| {
            manager.view(id).unwrap_or_else(|| {
                gtk::Label::builder()
                    .label("A widget not drawn here")
                    .css_classes(["dim-label"])
                    .build()
                    .upcast()
            })
        })
        .collect()
}

fn boxed(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    let across = model.name == "HBoxModel";
    let holder = gtk::Box::new(
        if across {
            gtk::Orientation::Horizontal
        } else {
            gtk::Orientation::Vertical
        },
        if across { 12 } else { 6 },
    );
    let guard = Rc::new(Cell::new(false));
    let manager = Rc::downgrade(manager);
    bind(
        model,
        &holder,
        &guard,
        &["children"],
        move |holder, model| {
            let Some(manager) = manager.upgrade() else {
                return;
            };
            while let Some(child) = holder.first_child() {
                holder.remove(&child);
            }
            for child in children(&manager, model) {
                holder.append(&child);
            }
        },
    );
    holder.upcast()
}

fn tabs(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    let kind = model.name.clone();
    let holder = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let guard = Rc::new(Cell::new(false));
    let manager_weak = Rc::downgrade(manager);
    let model_for = model.clone();
    let guard_for = guard.clone();
    bind(
        model,
        &holder,
        &guard,
        &["children", "titles", "selected_index"],
        move |holder, model| {
            let Some(manager) = manager_weak.upgrade() else {
                return;
            };
            while let Some(child) = holder.first_child() {
                holder.remove(&child);
            }
            let titles = model.get("titles");
            let title = |i: usize| {
                titles
                    .get(i)
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{}", i + 1))
            };
            let selected = model.get("selected_index").as_u64().map(|i| i as usize);
            let views = children(&manager, model);
            let select = {
                let (manager, model, guard) = (
                    Rc::downgrade(&manager),
                    model_for.clone(),
                    guard_for.clone(),
                );
                move |index: Option<usize>| {
                    if guard.get() {
                        return;
                    }
                    if let Some(manager) = manager.upgrade() {
                        manager.set(
                            &model,
                            "selected_index",
                            index.map_or(Value::Null, Value::from),
                        );
                    }
                }
            };
            match kind.as_str() {
                "TabModel" => {
                    let notebook = gtk::Notebook::new();
                    for (i, view) in views.into_iter().enumerate() {
                        notebook.append_page(&view, Some(&gtk::Label::new(Some(&title(i)))));
                    }
                    if let Some(i) = selected {
                        notebook.set_current_page(Some(i as u32));
                    }
                    notebook.connect_switch_page(move |_, _, page| select(Some(page as usize)));
                    holder.append(&notebook);
                }
                "AccordionModel" => {
                    for (i, view) in views.into_iter().enumerate() {
                        let expander = gtk::Expander::builder()
                            .label(title(i))
                            .child(&view)
                            .expanded(selected == Some(i))
                            .build();
                        let select = select.clone();
                        expander.connect_expanded_notify(move |e| {
                            select(e.is_expanded().then_some(i));
                        });
                        holder.append(&expander);
                    }
                }
                _ => {
                    if let Some(view) = selected.and_then(|i| views.into_iter().nth(i)) {
                        holder.append(&view);
                    }
                }
            }
        },
    );
    holder.upcast()
}

fn output(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    let holder = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let guard = Rc::new(Cell::new(false));
    let manager = Rc::downgrade(manager);
    bind(
        model,
        &holder,
        &guard,
        &["outputs"],
        move |holder, model| {
            while let Some(child) = holder.first_child() {
                holder.remove(&child);
            }
            let Some(manager) = manager.upgrade() else {
                return;
            };
            let Some(draw) = manager.draw_output.borrow().clone() else {
                return;
            };
            for output in model.get("outputs").as_array().into_iter().flatten() {
                if let Some(widget) = draw(output) {
                    holder.append(&widget);
                }
            }
        },
    );
    holder.upcast()
}

fn image(model: &Rc<Model>) -> gtk::Widget {
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::ScaleDown)
        .can_shrink(true)
        .halign(gtk::Align::Start)
        .build();
    let guard = Rc::new(Cell::new(false));
    bind(
        model,
        &picture,
        &guard,
        &["value", "width"],
        |picture, model| {
            if let Some(bytes) = model.buffers.borrow().get("value") {
                match gtk::gdk::Texture::from_bytes(&glib::Bytes::from(bytes.as_slice())) {
                    Ok(texture) => picture.set_paintable(Some(&texture)),
                    Err(e) => tracing::info!("an Image widget's picture was not decoded: {e}"),
                }
            }
            if let Ok(width) = model.str("width").trim_end_matches("px").parse::<i32>() {
                picture.set_width_request(width.min(1200));
            }
        },
    );
    picture.upcast()
}

fn play(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("linked");
    let start = gtk::ToggleButton::builder()
        .icon_name("media-playback-start-symbolic")
        .tooltip_text("Play")
        .build();
    let stop = gtk::Button::builder()
        .icon_name("media-playback-stop-symbolic")
        .tooltip_text("Stop")
        .build();
    let shown = readout();
    let timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let guard = Rc::new(Cell::new(false));
    bind(model, &shown, &guard, &["value"], |label, model| {
        label.set_label(&format!("{}", model.f64("value") as i64));
    });
    let tick = {
        let (manager, model, start) = (Rc::downgrade(manager), model.clone(), start.clone());
        move || -> glib::ControlFlow {
            let Some(manager) = manager.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let step = model.get("step").as_f64().unwrap_or(1.0);
            let next = model.f64("value") + step;
            if next > model.f64("max") {
                if model.bool("repeat") {
                    manager.set(&model, "value", Value::from(model.f64("min") as i64));
                    return glib::ControlFlow::Continue;
                }
                start.set_active(false);
                return glib::ControlFlow::Break;
            }
            manager.set(&model, "value", Value::from(next as i64));
            glib::ControlFlow::Continue
        }
    };
    {
        let (timer, model) = (timer.clone(), model.clone());
        let tick = Rc::new(tick);
        start.connect_toggled(move |start| {
            if let Some(id) = timer.borrow_mut().take() {
                id.remove();
            }
            if start.is_active() {
                let interval = model.get("interval").as_u64().unwrap_or(100).max(16);
                let tick = tick.clone();
                let slot = timer.clone();
                *timer.borrow_mut() = Some(glib::timeout_add_local(
                    std::time::Duration::from_millis(interval),
                    move || {
                        let flow = tick();
                        if flow == glib::ControlFlow::Break {
                            slot.borrow_mut().take();
                        }
                        flow
                    },
                ));
            }
        });
    }
    {
        let (manager, model, start) = (Rc::downgrade(manager), model.clone(), start.clone());
        stop.connect_clicked(move |_| {
            start.set_active(false);
            if let Some(manager) = manager.upgrade() {
                manager.set(&model, "value", Value::from(model.f64("min") as i64));
            }
        });
    }
    row.append(&start);
    row.append(&stop);
    let outer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    outer.append(&row);
    outer.append(&shown);
    described(model, &outer)
}

fn colour(manager: &Rc<Manager>, model: &Rc<Model>) -> gtk::Widget {
    let button = gtk::ColorDialogButton::new(Some(gtk::ColorDialog::new()));
    button.set_halign(gtk::Align::Start);
    let guard = Rc::new(Cell::new(false));
    bind(model, &button, &guard, &["value"], |button, model| {
        if let Ok(rgba) = gtk::gdk::RGBA::parse(model.str("value").as_str()) {
            button.set_rgba(&rgba);
        }
    });
    let (manager, model2) = (Rc::downgrade(manager), model.clone());
    button.connect_rgba_notify(move |button| {
        if guard.get() {
            return;
        }
        let rgba = button.rgba();
        let hex = format!(
            "#{:02x}{:02x}{:02x}",
            (rgba.red() * 255.0).round() as u8,
            (rgba.green() * 255.0).round() as u8,
            (rgba.blue() * 255.0).round() as u8
        );
        if let Some(manager) = manager.upgrade() {
            manager.set(&model2, "value", Value::from(hex));
        }
    });
    described(model, &button)
}

fn valid(model: &Rc<Model>) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let icon = gtk::Image::new();
    let text = gtk::Label::new(None);
    row.append(&icon);
    row.append(&text);
    let guard = Rc::new(Cell::new(false));
    let text_for = text.clone();
    bind(
        model,
        &icon,
        &guard,
        &["value", "readout"],
        move |icon, model| {
            let ok = model.bool("value");
            icon.set_icon_name(Some(if ok {
                "object-select-symbolic"
            } else {
                "window-close-symbolic"
            }));
            icon.set_css_classes(if ok { &["success"] } else { &["error"] });
            let readout = model.str("readout");
            text_for.set_label(if ok { "" } else { &readout });
        },
    );
    described(model, &row)
}

/// Text from a description or an HTML widget's value: tags dropped, the
/// common entities read, and a `<br>` or a block's end kept as a line.
/// What a description's HTML or LaTeX looks like set as text, rather than
/// a renderer for markup the project chose.
pub fn plain_text(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('>') else {
            out.push_str(&rest[open..]);
            rest = "";
            break;
        };
        let tag = rest[open + 1..open + close].trim().to_ascii_lowercase();
        if tag.starts_with("br")
            || tag.starts_with("/p")
            || tag.starts_with("/div")
            || tag.starts_with("/li")
        {
            out.push('\n');
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .trim()
        .to_string()
}

/// A number as a Python format spec would set it, for the specs ipywidgets
/// uses: `d`, `.Nf`, `.N%`, `.Ne`, and the default (`d` for an integer
/// widget, `.2f` for a float one).
pub fn number_text(value: f64, spec: &str, int: bool) -> String {
    let spec = spec.trim();
    let digits = |s: &str| {
        s.trim_start_matches('.')
            .trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '%')
            .parse::<usize>()
            .ok()
    };
    if spec.ends_with('%') {
        return format!("{:.*}%", digits(spec).unwrap_or(0), value * 100.0);
    }
    if spec.ends_with('e') {
        return format!("{:.*e}", digits(spec).unwrap_or(6), value);
    }
    if spec.ends_with('f') {
        return format!("{:.*}", digits(spec).unwrap_or(6), value);
    }
    if spec.ends_with('d') || (spec.is_empty() && int) {
        return format!("{}", value.round() as i64);
    }
    format!("{value:.2}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(manager: &Rc<Manager>, id: &str, state: Value) {
        manager.on_comm(
            "comm_open",
            &serde_json::json!({"comm_id": id, "target_name": "jupyter.widget",
                "data": {"state": state, "buffer_paths": []}}),
            Vec::new(),
        );
    }

    fn update(manager: &Rc<Manager>, id: &str, method: &str, state: Value) {
        manager.on_comm(
            "comm_msg",
            &serde_json::json!({"comm_id": id, "data": {"method": method, "state": state, "buffer_paths": []}}),
            Vec::new(),
        );
    }

    /// A change made here goes to the kernel as an update, and its echo
    /// does not undo a later one: a dragged slider does not jump back.
    #[test]
    fn a_change_here_is_sent_and_its_echo_does_not_undo_a_later_one() {
        let manager = Manager::new();
        let sent: Rc<RefCell<Vec<Value>>> = Rc::default();
        {
            let sent = sent.clone();
            manager.set_sender(Rc::new(move |_, data, _| sent.borrow_mut().push(data)));
        }
        open(
            &manager,
            "s",
            serde_json::json!({"_model_name": "IntSliderModel", "value": 3}),
        );
        let model = manager.model("s").unwrap();
        manager.set(&model, "value", Value::from(5));
        manager.set(&model, "value", Value::from(6));
        assert_eq!(sent.borrow().len(), 2);
        assert_eq!(sent.borrow()[0]["method"], "update");
        assert_eq!(sent.borrow()[1]["state"]["value"], 6);
        // The first echo arrives after the second change was made.
        update(
            &manager,
            "s",
            "echo_update",
            serde_json::json!({"value": 5}),
        );
        assert_eq!(model.get("value"), 6);
        update(
            &manager,
            "s",
            "echo_update",
            serde_json::json!({"value": 6}),
        );
        // And a change the kernel makes itself always lands.
        update(&manager, "s", "update", serde_json::json!({"value": 9}));
        assert_eq!(model.get("value"), 9);
    }

    /// An Output widget takes the output whose parent its msg_id names,
    /// and clear_output with wait clears at the next output.
    #[test]
    fn an_output_widget_captures_by_parent() {
        let manager = Manager::new();
        open(
            &manager,
            "o",
            serde_json::json!({"_model_name": "OutputModel", "msg_id": "", "outputs": []}),
        );
        let stream = serde_json::json!({"name": "stdout", "text": "a\n"});
        assert!(!manager.capture("req-1", "stream", &stream));
        update(
            &manager,
            "o",
            "update",
            serde_json::json!({"msg_id": "req-1"}),
        );
        assert!(manager.capture("req-1", "stream", &stream));
        assert!(manager.capture(
            "req-1",
            "stream",
            &serde_json::json!({"name": "stdout", "text": "b\n"})
        ));
        let model = manager.model("o").unwrap();
        assert_eq!(
            model.get("outputs").as_array().unwrap().len(),
            1,
            "one stream"
        );
        assert!(manager.capture("req-1", "clear_output", &serde_json::json!({"wait": true})));
        assert_eq!(model.get("outputs").as_array().unwrap().len(), 1, "not yet");
        assert!(manager.capture(
            "req-1",
            "stream",
            &serde_json::json!({"name": "stdout", "text": "c\n"})
        ));
        let outputs = model.get("outputs");
        assert_eq!(crate::notebook::joined(&outputs[0]["text"]), "c\n");
    }

    /// A jslink keeps two models' keys together, from either end.
    #[test]
    fn a_link_follows_its_source_both_ways() {
        let manager = Manager::new();
        manager.set_sender(Rc::new(|_, _, _| {}));
        open(
            &manager,
            "a",
            serde_json::json!({"_model_name": "IntSliderModel", "value": 1}),
        );
        open(
            &manager,
            "b",
            serde_json::json!({"_model_name": "IntTextModel", "value": 1}),
        );
        open(
            &manager,
            "l",
            serde_json::json!({"_model_name": "LinkModel",
            "source": ["IPY_MODEL_a", "value"], "target": ["IPY_MODEL_b", "value"]}),
        );
        let (a, b) = (manager.model("a").unwrap(), manager.model("b").unwrap());
        manager.set(&a, "value", Value::from(4));
        assert_eq!(b.get("value"), 4);
        manager.set(&b, "value", Value::from(7));
        assert_eq!(a.get("value"), 7);
    }

    /// A saved notebook's widgets are drawn from its state, not usable.
    #[test]
    fn saved_state_is_drawn_but_not_live() {
        let manager = Manager::new();
        manager.load_saved(
            &serde_json::json!({"x": {"model_name": "IntSliderModel", "state": {"value": 2}}}),
        );
        let model = manager.model("x").unwrap();
        assert!(!model.live.get());
        manager.set(&model, "value", Value::from(5));
        assert_eq!(
            model.get("value"),
            2,
            "nothing changes a model with no kernel"
        );
    }

    #[test]
    fn numbers_and_descriptions_read_as_ipywidgets_writes_them() {
        assert_eq!(number_text(3.0, "d", true), "3");
        assert_eq!(number_text(1.23456, ".2f", false), "1.23");
        assert_eq!(number_text(0.25, ".1%", false), "25.0%");
        assert_eq!(number_text(2.5, "", false), "2.50");
        assert_eq!(number_text(7.0, "", true), "7");
        assert_eq!(plain_text("<b>Size</b>&nbsp;&amp; shape"), "Size & shape");
        assert_eq!(plain_text("one<br>two"), "one\ntwo");
    }

    #[test]
    fn a_binary_state_is_kept_by_its_key() {
        let (state, binary) = with_buffers(
            &serde_json::json!({"format": "png"}),
            &serde_json::json!([["value"]]),
            vec![vec![1, 2, 3]],
        );
        assert_eq!(state["format"], "png");
        assert_eq!(binary["value"], vec![1, 2, 3]);
    }
}

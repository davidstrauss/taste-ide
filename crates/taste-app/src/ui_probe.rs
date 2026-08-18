//! The GTK side of `taste_core::ui_probe`: answers screenshots and
//! geometry dumps for agents.
//!
//! Agents debugging UI work used to need the user's eyes for both "what
//! does it look like" and "where did that pixel come from". These two
//! answers close that loop: [`UiRequest::Screenshot`] renders a pane
//! exactly as the compositor sees it (the `map_probe` technique:
//! WidgetPaintable → Snapshot → render_texture), and
//! [`UiRequest::Geometry`] dumps the widget subtree *as computed* —
//! allocations, margins, scroll positions — where configured-vs-rendered
//! bugs are visible analytically instead of by trial and error.
//!
//! Targets are pane names from the window's registry, optionally dotted
//! with a descendant's widget name: `chat`, `editor`, `chat.composer`.

use adw::prelude::*;
use gtk::glib;
use serde_json::{json, Value};
use taste_core::ui_probe::{UiReply, UiRequest};
use taste_core::Workspace;

/// Screenshot ceiling: the MCP bridge reads 4 MiB lines, and base64 costs
/// 4/3 — a 4K window PNG can blow through that. Anything larger renders
/// scaled; UI structure survives, and the agent reads it, not a human.
const MAX_RENDER_DIM: f64 = 2048.0;

/// Geometry dump bounds: deep enough for any real pane, small enough that
/// a runaway ListView can't turn one tool call into megabytes.
const MAX_DEPTH: usize = 12;
const MAX_NODES: usize = 800;

/// The editor's live-buffer lookup (the ACP fs/read_text_file path).
pub type BufferLookup = std::rc::Rc<dyn Fn(&std::path::Path) -> Option<String>>;

/// The editor's write path (the ACP fs/write_text_file path). Applies the
/// text and saves, through the user's own buffer when they have one open.
pub type BufferWriter = std::rc::Rc<dyn Fn(&std::path::Path, &str) -> Result<(), String>>;

/// Start answering probe requests on the main thread. `registry` maps the
/// stable pane names to their root widgets.
pub fn attach(
    workspace: &Workspace,
    registry: Vec<(&'static str, gtk::Widget)>,
    buffer_text: BufferLookup,
    buffer_write: BufferWriter,
) {
    let requests = workspace.ui.requests();
    glib::spawn_future_local(async move {
        while let Ok((request, reply)) = requests.recv().await {
            let response = match &request {
                UiRequest::Screenshot { target } => screenshot(&registry, target),
                UiRequest::Geometry { target } => geometry(&registry, target),
                UiRequest::BufferText { path } => Ok(UiReply::BufferText(buffer_text(path))),
                UiRequest::BufferWrite { path, content } => {
                    Ok(UiReply::BufferWrite(buffer_write(path, content)))
                }
            };
            let _ = reply.send(response.unwrap_or_else(UiReply::Error)).await;
        }
    });
}

/// `"chat"` or `"chat.composer"` → the widget, or a self-explaining error
/// (the valid pane names; how to discover descendant names).
fn resolve(registry: &[(&'static str, gtk::Widget)], target: &str) -> Result<gtk::Widget, String> {
    let (pane, descendant) = match target.split_once('.') {
        Some((pane, rest)) => (pane, Some(rest)),
        None => (target, None),
    };
    let Some((_, root)) = registry.iter().find(|(name, _)| *name == pane) else {
        let names: Vec<&str> = registry.iter().map(|(name, _)| *name).collect();
        return Err(format!(
            "unknown target '{target}': panes are {names:?}, optionally dotted with a \
             widget name from an ide_widget_geometry dump (e.g. \"chat.composer\")"
        ));
    };
    let Some(name) = descendant else {
        return Ok(root.clone());
    };
    // Breadth-first: the shallowest widget wearing the name wins.
    let mut queue = std::collections::VecDeque::from([root.clone()]);
    while let Some(widget) = queue.pop_front() {
        if widget.widget_name() == name {
            return Ok(widget);
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            child = current.next_sibling();
            queue.push_back(current);
        }
    }
    Err(format!(
        "no widget named '{name}' under '{pane}' — run ide_widget_geometry on '{pane}' \
         and use a \"name\" from the dump"
    ))
}

fn screenshot(registry: &[(&'static str, gtk::Widget)], target: &str) -> Result<UiReply, String> {
    let widget = resolve(registry, target)?;
    if !widget.is_mapped() {
        return Err(format!("'{target}' is not on screen (unmapped)"));
    }
    // Render the whole WINDOW and crop to the target. Rendering the target
    // subtree alone produces a lie for any widget with a translucent
    // background (the composer's grey wash is white at 10% alpha — cropped
    // standalone it reads as a white box): what's on screen is the
    // COMPOSITE, so the composite is what a screenshot must show.
    let root: gtk::Widget = widget
        .root()
        .ok_or_else(|| format!("'{target}' is not in a window"))?
        .upcast();
    let bounds = widget
        .compute_bounds(&root)
        .ok_or_else(|| format!("'{target}' has no computed bounds yet"))?;
    let (width, height) = (root.width(), root.height());
    if width == 0 || height == 0 || bounds.width() <= 0.0 || bounds.height() <= 0.0 {
        return Err(format!("'{target}' has no allocation yet"));
    }
    let scale = (MAX_RENDER_DIM / f64::from(bounds.width().max(bounds.height()))).min(1.0);
    let paintable = gtk::WidgetPaintable::new(Some(&root));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(
        &snapshot,
        f64::from(width) * scale,
        f64::from(height) * scale,
    );
    // WidgetPaintable serves the widget's last DRAWN frame; a widget that
    // has never been through a frame cycle (headless display, first
    // milliseconds of a window) yields an empty snapshot.
    let node = snapshot.to_node().ok_or_else(|| {
        format!("'{target}' has not been drawn yet — no frame has rendered it; retry shortly")
    })?;
    let renderer = root
        .native()
        .and_then(|native| native.renderer())
        .ok_or_else(|| format!("'{target}' has no renderer (window not realized)"))?;
    let viewport = gtk::graphene::Rect::new(
        bounds.x() * scale as f32,
        bounds.y() * scale as f32,
        bounds.width() * scale as f32,
        bounds.height() * scale as f32,
    );
    let texture = renderer.render_texture(&node, Some(&viewport));
    Ok(UiReply::Screenshot {
        png: texture.save_to_png_bytes().to_vec(),
        width: texture.width(),
        height: texture.height(),
    })
}

fn geometry(registry: &[(&'static str, gtk::Widget)], target: &str) -> Result<UiReply, String> {
    let widget = resolve(registry, target)?;
    let mut budget = MAX_NODES;
    let tree = dump(&widget, &widget, 0, &mut budget);
    Ok(UiReply::Geometry(json!({
        "target": target,
        "note": "bounds are computed x/y/w/h relative to the target's own origin; \
                 any \"name\" here works as a dotted target (e.g. \"chat.composer\")",
        "tree": tree,
    })))
}

/// One widget as JSON, children included. Only facts that earn their bytes:
/// defaults (Fill alignment, zero margins, no scroll) are omitted, so what
/// remains is exactly the widget's deviations — the things worth reading.
fn dump(widget: &gtk::Widget, root: &gtk::Widget, depth: usize, budget: &mut usize) -> Value {
    if *budget == 0 {
        return json!({"truncated": "node budget exhausted"});
    }
    *budget -= 1;
    let mut node = serde_json::Map::new();
    node.insert("type".into(), widget.type_().name().into());
    let name = widget.widget_name();
    if !name.is_empty() && name != widget.type_().name() {
        node.insert("name".into(), name.as_str().into());
    }
    node.insert("css_name".into(), widget.css_name().as_str().into());
    let classes = widget.css_classes();
    if !classes.is_empty() {
        node.insert(
            "css_classes".into(),
            classes
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .into(),
        );
    }
    if !widget.is_visible() {
        node.insert("visible".into(), false.into());
    } else if !widget.is_mapped() {
        node.insert("mapped".into(), false.into());
    }
    match widget.compute_bounds(root) {
        Some(bounds) => {
            node.insert(
                "bounds".into(),
                json!({
                    "x": round1(bounds.x()), "y": round1(bounds.y()),
                    "w": round1(bounds.width()), "h": round1(bounds.height()),
                }),
            );
        }
        None => {
            node.insert("bounds".into(), Value::Null);
        }
    }
    let margins = (
        widget.margin_top(),
        widget.margin_bottom(),
        widget.margin_start(),
        widget.margin_end(),
    );
    if margins != (0, 0, 0, 0) {
        node.insert(
            "margins".into(),
            json!({
                "top": margins.0, "bottom": margins.1,
                "start": margins.2, "end": margins.3,
            }),
        );
    }
    if widget.halign() != gtk::Align::Fill {
        node.insert("halign".into(), format!("{:?}", widget.halign()).into());
    }
    if widget.valign() != gtk::Align::Fill {
        node.insert("valign".into(), format!("{:?}", widget.valign()).into());
    }
    if widget.hexpands() {
        node.insert("hexpand".into(), true.into());
    }
    if widget.vexpands() {
        node.insert("vexpand".into(), true.into());
    }
    // The two families whose *internal* offsets keep causing rendered-vs-
    // configured bugs: scrollers (a scrolled-away margin is invisible in
    // source) and text views (the composer states its inset as margins).
    if let Some(scroller) = widget.downcast_ref::<gtk::ScrolledWindow>() {
        node.insert(
            "scroll".into(),
            json!({
                "h": adjustment_json(&scroller.hadjustment()),
                "v": adjustment_json(&scroller.vadjustment()),
            }),
        );
    }
    if let Some(text_view) = widget.downcast_ref::<gtk::TextView>() {
        node.insert(
            "text_margins".into(),
            json!({
                "top": text_view.top_margin(), "bottom": text_view.bottom_margin(),
                "left": text_view.left_margin(), "right": text_view.right_margin(),
            }),
        );
    }
    if depth < MAX_DEPTH {
        let mut children = Vec::new();
        let mut child = widget.first_child();
        while let Some(current) = child {
            child = current.next_sibling();
            children.push(dump(&current, root, depth + 1, budget));
        }
        if !children.is_empty() {
            node.insert("children".into(), children.into());
        }
    } else if widget.first_child().is_some() {
        node.insert("children".into(), "truncated: depth limit".into());
    }
    Value::Object(node)
}

fn adjustment_json(adjustment: &gtk::Adjustment) -> Value {
    json!({
        "value": round1(adjustment.value() as f32),
        "upper": round1(adjustment.upper() as f32),
        "page": round1(adjustment.page_size() as f32),
    })
}

fn round1(v: f32) -> f64 {
    (f64::from(v) * 10.0).round() / 10.0
}

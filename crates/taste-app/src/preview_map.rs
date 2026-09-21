//! The markdown preview's overview strip: the rendered document, scaled
//! to a narrow column beside it, with the visible part framed and a
//! click or a drag jumping there — the same bar the source view has
//! (GtkSourceView's map), for the face that is widgets rather than text
//! (David, 2026-09-21: "I want markdown previews to have the same sort of
//! 'zoomed out' bar on the side as code").
//!
//! The picture is a [`gtk::WidgetPaintable`] of the preview's content box,
//! which GTK re-snapshots as the content repaints, so an image landing or
//! a theme flip shows up here without a hook. It is scaled to fit the
//! strip's width and height both, so the whole document is always in the
//! strip: a long document is a thin one, which is what an overview is for.
//! The frame is drawn over it from the scroller's own adjustment, and a
//! press anywhere puts that point of the document at the middle of the
//! view, as the source map's slider does.

use adw::prelude::*;

/// The strip's width. GtkSourceMap's is its one-pixel font times the
/// right margin, about this; the two stand side by side across the
/// display-mode switch and should not jump.
const WIDTH: i32 = 96;

/// How much of the foreground the frame around the visible part takes,
/// and its fill.
const FRAME_ALPHA: f64 = 0.55;
const FILL_ALPHA: f64 = 0.08;

/// Build the strip for `content`, the widget the scroller scrolls.
pub fn preview_map(content: &gtk::Widget, scroller: &gtk::ScrolledWindow) -> gtk::Widget {
    let paintable = gtk::WidgetPaintable::new(Some(content));
    let picture = gtk::Picture::builder()
        .paintable(&paintable)
        .content_fit(gtk::ContentFit::Contain)
        .can_shrink(true)
        .halign(gtk::Align::Fill)
        .valign(gtk::Align::Fill)
        .build();
    picture.set_size_request(WIDTH, -1);
    let frame = gtk::DrawingArea::builder()
        .can_target(false)
        .hexpand(true)
        .vexpand(true)
        .build();
    let overlay = gtk::Overlay::builder().child(&picture).build();
    overlay.add_overlay(&frame);
    overlay.set_size_request(WIDTH, -1);
    overlay.add_css_class("preview-map");

    // Where the document's picture sits inside the strip: `Contain`
    // centres it, so the drawn extent and its offset follow from the
    // paintable's intrinsic size and the strip's allocation.
    let extent = {
        let paintable = paintable.clone();
        move |width: i32, height: i32| -> Option<(f64, f64, f64, f64)> {
            let (iw, ih) = (
                f64::from(paintable.intrinsic_width()),
                f64::from(paintable.intrinsic_height()),
            );
            if iw <= 0.0 || ih <= 0.0 || width <= 0 || height <= 0 {
                return None;
            }
            let scale = (f64::from(width) / iw).min(f64::from(height) / ih);
            let (dw, dh) = (iw * scale, ih * scale);
            let x = (f64::from(width) - dw) / 2.0;
            let y = (f64::from(height) - dh) / 2.0;
            Some((x, y, dw, dh))
        }
    };

    let adjustment = scroller.vadjustment();
    {
        let adjustment = adjustment.clone();
        let extent = extent.clone();
        frame.set_draw_func(move |area, cr, width, height| {
            let Some((x, y, dw, dh)) = extent(width, height) else {
                return;
            };
            let upper = adjustment.upper();
            if upper <= 0.0 {
                return;
            }
            let top = y + dh * (adjustment.value() / upper);
            let visible = dh * (adjustment.page_size() / upper).min(1.0);
            let colour = area.color();
            let ink = |alpha: f64| {
                cr.set_source_rgba(
                    f64::from(colour.red()),
                    f64::from(colour.green()),
                    f64::from(colour.blue()),
                    alpha * f64::from(colour.alpha()),
                );
            };
            ink(FILL_ALPHA);
            cr.rectangle(x, top, dw, visible);
            let _ = cr.fill();
            ink(FRAME_ALPHA);
            cr.set_line_width(1.0);
            cr.rectangle(x + 0.5, top + 0.5, dw - 1.0, (visible - 1.0).max(1.0));
            let _ = cr.stroke();
        });
    }
    {
        let frame = frame.clone();
        adjustment.connect_value_changed(move |_| frame.queue_draw());
    }
    {
        let frame = frame.clone();
        adjustment.connect_changed(move |_| frame.queue_draw());
    }

    // A press puts that point of the document at the middle of the view;
    // a drag keeps doing so.
    let jump = {
        let adjustment = adjustment.clone();
        let overlay = overlay.clone();
        std::rc::Rc::new(move |y: f64| {
            let Some((_, top, _, dh)) = extent(overlay.width(), overlay.height()) else {
                return;
            };
            if dh <= 0.0 {
                return;
            }
            let fraction = ((y - top) / dh).clamp(0.0, 1.0);
            let upper = adjustment.upper();
            let page = adjustment.page_size();
            let value = (fraction * upper - page / 2.0).clamp(0.0, (upper - page).max(0.0));
            adjustment.set_value(value);
        })
    };
    let drag = gtk::GestureDrag::new();
    drag.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let jump = jump.clone();
        drag.connect_drag_begin(move |gesture, _x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            jump(y);
        });
    }
    {
        let jump = jump.clone();
        drag.connect_drag_update(move |gesture, _dx, dy| {
            if let Some((_, y0)) = gesture.start_point() {
                jump(y0 + dy);
            }
        });
    }
    overlay.add_controller(drag);
    overlay.upcast()
}

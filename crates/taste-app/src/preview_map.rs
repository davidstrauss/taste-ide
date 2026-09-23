//! The markdown preview's overview strip: the rendered document, scaled
//! to a narrow column beside it, with the visible part marked and a
//! click or a drag jumping there — the same bar the source view has
//! (GtkSourceView's map), for the face that is widgets rather than text
//! (David, 2026-09-21: "I want markdown previews to have the same sort of
//! 'zoomed out' bar on the side as code").
//!
//! It is drawn the way the source map is drawn: the document scaled to
//! the strip's WIDTH and anchored at the top, a document taller than the
//! strip scrolling within it in step with the view, and the visible part
//! a translucent accent block — the same width, the same alignment, the
//! same slider (David, 2026-09-22). The strip is a widget of its own
//! with its own `snapshot`, because painting a paintable scaled and
//! offset is something no closure adapter offers: a `GtkPicture` fits or
//! centres, and a `GtkDrawingArea` paints with cairo, which a
//! `GtkWidgetPaintable` cannot be drawn into.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

/// The strip's width when the source map's cannot be read: GtkSourceMap
/// at its one-pixel font is about this.
pub const WIDTH: i32 = 96;

/// The slider's share of the accent colour, as the source map's CSS has
/// it (`textview.GtkSourceMap > slider`).
const SLIDER_ALPHA: f32 = 0.25;

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    pub struct PreviewMap {
        pub(super) paintable: RefCell<Option<gtk::WidgetPaintable>>,
        pub(super) adjustment: RefCell<Option<gtk::Adjustment>>,
        pub(super) width: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PreviewMap {
        const NAME: &'static str = "TastePreviewMap";
        type Type = super::PreviewMap;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for PreviewMap {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().set_overflow(gtk::Overflow::Hidden);
            self.obj().add_css_class("preview-map");
            self.obj().set_vexpand(true);
        }
    }

    impl WidgetImpl for PreviewMap {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            match orientation {
                gtk::Orientation::Horizontal => (self.width.get(), self.width.get(), -1, -1),
                _ => (0, 0, -1, -1),
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let Some(geometry) = obj.geometry() else {
                return;
            };
            let Some(paintable) = self.paintable.borrow().clone() else {
                return;
            };
            // The document, scaled to the strip and scrolled within it.
            snapshot.save();
            snapshot.translate(&gtk::graphene::Point::new(0.0, -geometry.offset as f32));
            snapshot.scale(geometry.scale as f32, geometry.scale as f32);
            paintable.snapshot(
                snapshot.upcast_ref::<gtk::gdk::Snapshot>(),
                f64::from(paintable.intrinsic_width()),
                f64::from(paintable.intrinsic_height()),
            );
            snapshot.restore();
            // The visible part, as the source map's slider: the accent,
            // translucent, over what is on screen.
            if let Some((top, height)) = obj.slider(&geometry) {
                let colour = obj.color();
                let rgba = gtk::gdk::RGBA::new(
                    colour.red(),
                    colour.green(),
                    colour.blue(),
                    colour.alpha() * SLIDER_ALPHA,
                );
                snapshot.append_color(
                    &rgba,
                    &gtk::graphene::Rect::new(0.0, top as f32, obj.width() as f32, height as f32),
                );
            }
        }
    }
}

glib::wrapper! {
    pub struct PreviewMap(ObjectSubclass<imp::PreviewMap>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

/// How the document sits in the strip: the scale that makes it the
/// strip's width, its drawn height, and how far it is scrolled up so the
/// visible part stays in the strip.
#[derive(Clone, Copy)]
struct Geometry {
    scale: f64,
    drawn_height: f64,
    offset: f64,
}

impl PreviewMap {
    /// The strip for `content`, the widget `scroller` scrolls, `width`
    /// wide — the source map's width, so the two faces' strips match.
    pub fn new(content: &gtk::Widget, scroller: &gtk::ScrolledWindow, width: i32) -> Self {
        let this: Self = glib::Object::new();
        let imp = this.imp();
        imp.width.set(width.max(1));
        let paintable = gtk::WidgetPaintable::new(Some(content));
        {
            let weak = this.downgrade();
            paintable.connect_invalidate_contents(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.queue_draw();
                }
            });
            let weak = this.downgrade();
            paintable.connect_invalidate_size(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.queue_draw();
                }
            });
        }
        *imp.paintable.borrow_mut() = Some(paintable);
        let adjustment = scroller.vadjustment();
        {
            let weak = this.downgrade();
            adjustment.connect_value_changed(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.queue_draw();
                }
            });
            let weak = this.downgrade();
            adjustment.connect_changed(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.queue_draw();
                }
            });
        }
        *imp.adjustment.borrow_mut() = Some(adjustment);

        // A press puts that point of the document at the middle of the
        // view; a drag keeps doing so, as the source map's slider does.
        let drag = gtk::GestureDrag::new();
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let weak = this.downgrade();
            drag.connect_drag_begin(move |gesture, _x, y| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                if let Some(this) = weak.upgrade() {
                    this.jump(y);
                }
            });
            let weak = this.downgrade();
            drag.connect_drag_update(move |gesture, _dx, dy| {
                if let (Some(this), Some((_, y0))) = (weak.upgrade(), gesture.start_point()) {
                    this.jump(y0 + dy);
                }
            });
        }
        this.add_controller(drag);

        // The wheel scrolls the document, as it does over the source map
        // (David, 2026-09-23: "Scrolling over the document minimap works
        // for code but not markdown preview"): a wheel notch by the step a
        // scrolled window takes for one — the page's height to the two
        // thirds — and a touchpad by its own pixels.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        {
            let weak = this.downgrade();
            scroll.connect_scroll(move |controller, _dx, dy| {
                let Some(this) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                let Some(adjustment) = this.imp().adjustment.borrow().clone() else {
                    return glib::Propagation::Proceed;
                };
                let step = match controller.unit() {
                    gtk::gdk::ScrollUnit::Wheel => adjustment.page_size().powf(2.0 / 3.0),
                    _ => 1.0,
                };
                let top = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
                adjustment
                    .set_value((adjustment.value() + dy * step).clamp(adjustment.lower(), top));
                glib::Propagation::Stop
            });
        }
        this.add_controller(scroll);
        this
    }

    fn geometry(&self) -> Option<Geometry> {
        let imp = self.imp();
        let paintable = imp.paintable.borrow().clone()?;
        let adjustment = imp.adjustment.borrow().clone()?;
        let (iw, ih) = (
            f64::from(paintable.intrinsic_width()),
            f64::from(paintable.intrinsic_height()),
        );
        let (w, h) = (f64::from(self.width()), f64::from(self.height()));
        if iw <= 0.0 || ih <= 0.0 || w <= 0.0 || h <= 0.0 {
            return None;
        }
        let scale = w / iw;
        let drawn_height = ih * scale;
        // A document taller than the strip scrolls within it in step
        // with the view: at the top of the document the strip shows the
        // top, at the bottom the bottom.
        let (value, upper, page) = (
            adjustment.value(),
            adjustment.upper(),
            adjustment.page_size(),
        );
        let offset = if drawn_height > h && upper > page {
            (drawn_height - h) * (value / (upper - page)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        Some(Geometry {
            scale,
            drawn_height,
            offset,
        })
    }

    /// The slider: where the visible part of the document falls in the
    /// strip, as (top, height).
    fn slider(&self, geometry: &Geometry) -> Option<(f64, f64)> {
        let adjustment = self.imp().adjustment.borrow().clone()?;
        let upper = adjustment.upper();
        if upper <= 0.0 {
            return None;
        }
        let top = geometry.drawn_height * (adjustment.value() / upper) - geometry.offset;
        let height = (geometry.drawn_height * (adjustment.page_size() / upper).min(1.0)).max(2.0);
        Some((top, height))
    }

    fn jump(&self, y: f64) {
        let Some(geometry) = self.geometry() else {
            return;
        };
        let Some(adjustment) = self.imp().adjustment.borrow().clone() else {
            return;
        };
        let fraction = ((y + geometry.offset) / geometry.drawn_height).clamp(0.0, 1.0);
        let upper = adjustment.upper();
        let page = adjustment.page_size();
        let value = (fraction * upper - page / 2.0).clamp(0.0, (upper - page).max(0.0));
        adjustment.set_value(value);
    }
}

/// Build the strip for `content`, the widget the scroller scrolls, as
/// wide as `width`.
pub fn preview_map(
    content: &gtk::Widget,
    scroller: &gtk::ScrolledWindow,
    width: i32,
) -> gtk::Widget {
    PreviewMap::new(content, scroller, width).upcast()
}

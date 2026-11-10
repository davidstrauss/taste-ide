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
//!
//! A Markdown document's preview is a text view (`crate::doc_text`), which
//! draws only the lines on screen, so painting it would paint a screenful.
//! Its strip is given a GHOST instead ([`PreviewMap::with_ghost`]): a
//! second view of the same document that the strip itself holds, lays out
//! at the preview's width and its whole height, and never draws but
//! through the strip. Its owner says when it has changed
//! ([`PreviewMap::refresh`]), since nothing else draws it and so nothing
//! else would know.
//!
//! Either way, the document is drawn once it has been still for
//! `CONTENTS_EVERY`, into a picture at the strip's scale, and the strip
//! paints that picture: the ghost, being the strip's own child, asks it to
//! draw again at every step of a selection's drag, and painting the whole
//! document's text that often made a frame a tenth of a second long every
//! twenty or so.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

/// The strip's width when the source map's cannot be read: GtkSourceMap
/// at its one-pixel font is about this.
pub const WIDTH: i32 = 96;

/// The slider's share of the accent colour, as the source map's CSS has
/// it (`textview.GtkSourceMap > slider`).
const SLIDER_ALPHA: f32 = 0.25;

/// How long the document's contents must be still before the strip draws
/// them again.
const CONTENTS_EVERY: std::time::Duration = std::time::Duration::from_millis(150);

/// The tallest picture of the document the strip keeps, in device pixels.
const MAX_TEXTURE: f64 = 8192.0;

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    #[derive(Default)]
    pub struct PreviewMap {
        pub(super) paintable: RefCell<Option<gtk::WidgetPaintable>>,
        pub(super) adjustment: RefCell<Option<gtk::Adjustment>>,
        pub(super) width: Cell<i32>,
        /// The document as last drawn, at the strip's scale, as a picture:
        /// painting the document's text at that scale every frame cost a
        /// frame a tenth of a second long every twenty or so while a
        /// selection was dragged, and a picture costs nothing to paint.
        pub(super) drawn: RefCell<Option<gtk::gdk::Texture>>,
        /// The next drawing, waiting for the changes to stop.
        pub(super) redraw: RefCell<Option<glib::SourceId>>,
        /// The ghost this strip holds and paints, and how wide to lay it out.
        pub(super) ghost: RefCell<Option<(gtk::Widget, Rc<dyn Fn() -> i32>)>>,
        /// The ghost's size and the strip's width when last laid out, which
        /// changing is a reason to draw again.
        pub(super) laid_out: Cell<(i32, i32, i32)>,
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

        fn dispose(&self) {
            if let Some(waiting) = self.redraw.take() {
                waiting.remove();
            }
            if let Some((ghost, _)) = self.ghost.take() {
                ghost.unparent();
            }
        }
    }

    impl WidgetImpl for PreviewMap {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            match orientation {
                gtk::Orientation::Horizontal => (self.width.get(), self.width.get(), -1, -1),
                _ => (0, 0, -1, -1),
            }
        }

        fn size_allocate(&self, width: i32, _height: i32, _baseline: i32) {
            // The ghost at the preview's width and as tall as it asks: its
            // whole document. The strip's own size is not its business.
            let mut laid_out = (0, 0, width);
            if let Some((ghost, width_of)) = self.ghost.borrow().as_ref() {
                let ghost_width = width_of().max(ghost.measure(gtk::Orientation::Horizontal, -1).0);
                let (_, height, _, _) = ghost.measure(gtk::Orientation::Vertical, ghost_width);
                ghost.allocate(ghost_width, height, -1, None);
                laid_out = (ghost_width, height, width);
            }
            if self.laid_out.replace(laid_out) != laid_out {
                self.obj().refresh();
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let Some(geometry) = obj.geometry() else {
                return;
            };
            match self.drawn.borrow().as_ref() {
                // The document, scaled to the strip and scrolled within it.
                Some(texture) => snapshot.append_texture(
                    texture,
                    &gtk::graphene::Rect::new(
                        0.0,
                        -geometry.offset as f32,
                        obj.width() as f32,
                        geometry.drawn_height as f32,
                    ),
                ),
                None => obj.refresh(),
            }
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

/// How the document sits in the strip: its height drawn at the strip's
/// width, and how far it is scrolled up so the visible part stays in the
/// strip.
#[derive(Clone, Copy)]
struct Geometry {
    drawn_height: f64,
    offset: f64,
}

impl PreviewMap {
    /// The strip for `content`, the widget `scroller` scrolls, `width`
    /// wide — the source map's width, so the two faces' strips match.
    pub fn new(content: &gtk::Widget, scroller: &gtk::ScrolledWindow, width: i32) -> Self {
        Self::build(content, scroller, width)
    }

    /// The strip for a text view's document: `ghost`, a second view of it
    /// that the strip holds, lays out `width_of()` wide (the preview's
    /// width), and paints, scrolled with `scroller`.
    pub fn with_ghost(
        ghost: &gtk::Widget,
        width_of: std::rc::Rc<dyn Fn() -> i32>,
        scroller: &gtk::ScrolledWindow,
        width: i32,
    ) -> Self {
        let this = Self::build(ghost, scroller, width);
        ghost.set_parent(&this);
        *this.imp().ghost.borrow_mut() = Some((ghost.clone(), width_of));
        this
    }

    fn build(content: &gtk::Widget, scroller: &gtk::ScrolledWindow, width: i32) -> Self {
        let this: Self = glib::Object::new();
        let imp = this.imp();
        imp.width.set(width.max(1));
        let paintable = gtk::WidgetPaintable::new(Some(content));
        {
            // Drawing the strip draws the whole document again. A change of
            // contents — a selection growing under a drag changes them
            // every frame — is drawn once it has stopped for
            // `CONTENTS_EVERY`, never every frame, which had the strip
            // drawing the document twice a frame for as long as the drag
            // lasted (2026-10-13). Scrolling still moves the slider at once.
            // A ghost's contents are followed by its owner instead
            // ([`PreviewMap::refresh`]): nothing draws it but this, so its
            // paintable is never told.
            let weak = this.downgrade();
            paintable.connect_invalidate_contents(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.refresh();
                }
            });
            let weak = this.downgrade();
            paintable.connect_invalidate_size(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.refresh();
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

    /// Draw the document again once it has stopped changing for
    /// `CONTENTS_EVERY`: what it shows changed, or where it is laid out.
    pub fn refresh(&self) {
        let imp = self.imp();
        if let Some(waiting) = imp.redraw.take() {
            waiting.remove();
        }
        let weak = self.downgrade();
        let id = glib::timeout_add_local_once(CONTENTS_EVERY, move || {
            if let Some(this) = weak.upgrade() {
                this.imp().redraw.take();
                this.draw_document();
            }
        });
        imp.redraw.replace(Some(id));
    }

    /// The document drawn at the strip's scale into a picture: the ghost
    /// as its parent draws a child (a paintable shows only what a frame
    /// has drawn of a widget, and nothing but this draws the ghost), or the
    /// content through its paintable. Left as it was when the strip has no
    /// window to draw with yet.
    fn draw_document(&self) {
        let imp = self.imp();
        let Some((width, height)) = self.document_size() else {
            return;
        };
        let strip = f64::from(self.width());
        let Some(renderer) = self.native().and_then(|native| native.renderer()) else {
            return;
        };
        if width <= 0.0 || height <= 0.0 || strip <= 0.0 {
            return;
        }
        let document = gtk::Snapshot::new();
        match imp.ghost.borrow().as_ref() {
            Some((ghost, _)) => self.snapshot_child(ghost, &document),
            None => {
                let Some(paintable) = imp.paintable.borrow().clone() else {
                    return;
                };
                paintable.snapshot(document.upcast_ref::<gtk::gdk::Snapshot>(), width, height);
            }
        }
        let Some(node) = document.to_node() else {
            return;
        };
        // In device pixels, and no taller than a texture may be: a long
        // document is squeezed rather than cut off, at a scale where its
        // text is texture rather than words anyway.
        let factor = f64::from(self.scale_factor());
        let across = strip / width * factor;
        let pixels_wide = (strip * factor).ceil();
        let tall = height * across;
        let down = across * (MAX_TEXTURE / tall).min(1.0);
        let pixels_tall = (height * down).ceil().max(1.0);
        let scaled = gtk::Snapshot::new();
        scaled.scale(across as f32, down as f32);
        scaled.append_node(&node);
        let Some(scaled) = scaled.to_node() else {
            return;
        };
        let texture = renderer.render_texture(
            &scaled,
            Some(&gtk::graphene::Rect::new(
                0.0,
                0.0,
                pixels_wide as f32,
                pixels_tall as f32,
            )),
        );
        imp.drawn.replace(Some(texture));
        self.queue_draw();
    }

    /// The document's size as it is laid out: the ghost's, or what the
    /// content's paintable says.
    fn document_size(&self) -> Option<(f64, f64)> {
        let imp = self.imp();
        if let Some((ghost, _)) = imp.ghost.borrow().as_ref() {
            return Some((f64::from(ghost.width()), f64::from(ghost.height())));
        }
        let paintable = imp.paintable.borrow().clone()?;
        Some((
            f64::from(paintable.intrinsic_width()),
            f64::from(paintable.intrinsic_height()),
        ))
    }

    fn geometry(&self) -> Option<Geometry> {
        let imp = self.imp();
        let adjustment = imp.adjustment.borrow().clone()?;
        let (iw, ih) = self.document_size()?;
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

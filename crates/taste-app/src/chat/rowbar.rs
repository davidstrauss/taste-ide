//! **The transcript's scrollbar, counted in rows.**
//!
//! The transcript is a `ListView`, and a list view knows the height of only
//! the rows it has built: the rest it estimates from those, and its extent
//! is that estimate. A conversation's rows run from a one-line step to an
//! answer a screen tall, so the estimate swings with whichever rows are
//! built, and a scrollbar drawn from the extent swings with it. Dragged,
//! the slider lands somewhere the pointer is not, the next motion sends the
//! view somewhere else, the rows built there change the estimate again —
//! the slider jittering under the pointer until it is unusable (David,
//! 2026-10-19: "jitters around to the point of not being usable"). The
//! probe measured it on a real conversation: the slider 69px off the
//! pointer, the extent changing 138 times in a 180-frame drag.
//!
//! So the bar is not drawn from the extent. Its adjustment counts the
//! list's items: the value is the item at the top of the view and how far
//! into it the view starts, the page is how many items the view shows, the
//! upper is how many there are. None of that changes when a row is
//! measured, so the slider stays under the pointer, and dragging it takes
//! the view to that item ([`RowBar::drive`]). Between drags the bar follows
//! the view, once a frame. The wheel, the touchpad, and the keys still
//! scroll the list's own adjustment in pixels — the native bar is only
//! hidden (`PolicyType::External`).
//!
//! It is drawn as the native one is: Adwaita's `overlay-indicator`, with
//! `hovering` and `dragging`, shown while the view moves or the pointer is
//! over it and faded out after; and a plain bar beside the transcript when
//! the desktop asks for scrollbars always shown (`gtk-overlay-scrolling`).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

/// How long the bar stays after the view last moved or the pointer left
/// it, as the native indicator does.
const LINGER: std::time::Duration = std::time::Duration::from_millis(1200);

/// How near the transcript's right edge the pointer widens the bar.
const HOVER_REACH: f64 = 24.0;

/// The most frames a jump the reader let go of is followed for before it
/// is taken as settled, whatever the list is still measuring.
const SETTLE_FRAMES: u32 = 120;

pub struct RowBar {
    /// What goes where the scroller went: the scroller with the bar over
    /// it, or beside it.
    pub widget: gtk::Widget,
    bar: gtk::Scrollbar,
    rows: gtk::Adjustment,
    scroller: gtk::ScrolledWindow,
    list: gtk::ListView,
    /// The list's bound items, by address: the only ones with a place on
    /// screen to measure.
    items: RefCell<HashMap<usize, glib::WeakRef<gtk::ListItem>>>,
    /// The bar is being set from the view, not moved by the reader.
    syncing: Cell<bool>,
    sync_queued: Cell<bool>,
    /// Where the reader put the slider, in items, while the view is
    /// being taken there.
    target: Cell<Option<f64>>,
    driving: Cell<bool>,
    dragging: Cell<bool>,
    hovering: Cell<bool>,
    overlay: bool,
    hide: RefCell<Option<glib::SourceId>>,
    /// The reader moved the view: marks it as theirs (`reader_scrolled`)
    /// and lets go of the end.
    on_reader: Rc<dyn Fn()>,
    /// To the end, following it (`ChatPane::scroll_to_end`).
    to_end: Rc<dyn Fn()>,
}

impl RowBar {
    pub fn new(
        scroller: &gtk::ScrolledWindow,
        list: &gtk::ListView,
        on_reader: Rc<dyn Fn()>,
        to_end: Rc<dyn Fn()>,
    ) -> Rc<Self> {
        scroller.set_vscrollbar_policy(gtk::PolicyType::External);
        let rows = gtk::Adjustment::new(0.0, 0.0, 1.0, 1.0, 1.0, 1.0);
        let bar = gtk::Scrollbar::new(gtk::Orientation::Vertical, Some(&rows));
        bar.add_css_class("row-scrollbar");
        let overlay = gtk::Settings::default().is_none_or(|s| s.is_gtk_overlay_scrolling());
        let widget: gtk::Widget = if overlay {
            bar.add_css_class("overlay-indicator");
            bar.set_halign(gtk::Align::End);
            let frame = gtk::Overlay::new();
            frame.set_child(Some(scroller));
            frame.add_overlay(&bar);
            frame.upcast()
        } else {
            let frame = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            scroller.set_hexpand(true);
            frame.append(scroller);
            frame.append(&bar);
            frame.upcast()
        };
        let this = Rc::new(Self {
            widget,
            bar,
            rows,
            scroller: scroller.clone(),
            list: list.clone(),
            items: RefCell::new(HashMap::new()),
            syncing: Cell::new(false),
            sync_queued: Cell::new(false),
            target: Cell::new(None),
            driving: Cell::new(false),
            dragging: Cell::new(false),
            hovering: Cell::new(false),
            overlay,
            hide: RefCell::new(None),
            on_reader,
            to_end,
        });

        let pixels = scroller.vadjustment();
        let weak = Rc::downgrade(&this);
        pixels.connect_value_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.queue_sync();
                this.show();
            }
        });
        let weak = Rc::downgrade(&this);
        pixels.connect_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.queue_sync();
            }
        });
        if let Some(model) = list.model() {
            let weak = Rc::downgrade(&this);
            model.connect_items_changed(move |_, _, _, _| {
                if let Some(this) = weak.upgrade() {
                    this.queue_sync();
                }
            });
        }
        let weak = Rc::downgrade(&this);
        this.rows.connect_value_changed(move |rows| {
            let Some(this) = weak.upgrade() else { return };
            if this.syncing.get() {
                return;
            }
            // The reader moved the slider: a drag, a click in the trough,
            // the wheel over the bar.
            this.target.set(Some(rows.value()));
            (this.on_reader)();
            this.show();
            this.drive();
        });

        let drag = gtk::GestureDrag::new();
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&this);
        drag.connect_drag_begin(move |_, _, _| {
            if let Some(this) = weak.upgrade() {
                this.dragging.set(true);
                this.bar.add_css_class("dragging");
                this.sync_hover();
                (this.on_reader)();
            }
        });
        let weak = Rc::downgrade(&this);
        drag.connect_drag_update(move |_, _, _| {
            if let Some(this) = weak.upgrade() {
                (this.on_reader)();
            }
        });
        let weak = Rc::downgrade(&this);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(this) = weak.upgrade() {
                this.dragging.set(false);
                this.bar.remove_css_class("dragging");
                this.sync_hover();
                this.show();
                // A press at the bottom that moved nothing let go of the end
                // all the same (`on_reader`), and the next line would have
                // been announced rather than followed (review, 2026-10-20).
                if this.target.get().is_none() && super::at_bottom(&this.scroller.vadjustment()) {
                    (this.to_end)();
                }
            }
        });
        this.bar.add_controller(drag);

        // The pointer moving over the transcript shows the bar, and near
        // its edge widens it to be grabbed, as the native one does.
        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(&this);
        motion.connect_motion(move |_, x, _| {
            if let Some(this) = weak.upgrade() {
                let width = f64::from(this.widget.width());
                this.hovering.set(x >= width - HOVER_REACH);
                this.sync_hover();
                this.show();
            }
        });
        let weak = Rc::downgrade(&this);
        motion.connect_leave(move |_| {
            if let Some(this) = weak.upgrade() {
                this.hovering.set(false);
                this.sync_hover();
                this.show();
            }
        });
        this.widget.add_controller(motion);
        this.queue_sync();
        this
    }

    /// The list bound `item`: one more to measure.
    pub fn bound(&self, item: &gtk::ListItem) {
        self.items
            .borrow_mut()
            .insert(item.as_ptr() as usize, item.downgrade());
    }

    pub fn unbound(&self, item: &gtk::ListItem) {
        self.items.borrow_mut().remove(&(item.as_ptr() as usize));
    }

    /// The bar's adjustment, for the drag probe to move as GtkRange does.
    pub fn rows(&self) -> &gtk::Adjustment {
        &self.rows
    }

    /// TASTE_PROBE_CHECK only: a drag begun or ended, as the gesture says
    /// it, the slider drawn held.
    pub fn probe_drag(&self, on: bool) {
        self.dragging.set(on);
        if on {
            self.bar.add_css_class("dragging");
        } else {
            self.bar.remove_css_class("dragging");
        }
        self.sync_hover();
    }

    /// TASTE_PROBE_CHECK only: the bar as drawn — shown, its size and
    /// place against the transcript, its opacity, and its classes.
    pub fn probe_state(&self) -> String {
        let bounds = self.bar.compute_bounds(&self.scroller);
        format!(
            "bar visible {} mapped {} {:?} opacity {:.2} classes {:?}",
            self.bar.is_visible(),
            self.bar.is_mapped(),
            bounds.map(|b| (b.x(), b.y(), b.width(), b.height())),
            self.bar.opacity(),
            self.bar.css_classes()
        )
    }

    /// Each bound item on screen, with its place in the list: its
    /// position, and its top and height against the view.
    fn on_screen(&self) -> Vec<(u32, f64, f64)> {
        let height = f64::from(self.scroller.height());
        let mut items = self.items.borrow_mut();
        items.retain(|_, item| item.upgrade().is_some());
        items
            .values()
            .filter_map(|item| {
                let item = item.upgrade()?;
                let child = item.child()?;
                if !child.is_mapped() {
                    return None;
                }
                let bounds = child.compute_bounds(&self.scroller)?;
                let (y, h) = (f64::from(bounds.y()), f64::from(bounds.height()));
                (h > 0.0 && y + h > 0.0 && y < height).then_some((item.position(), y, h))
            })
            .collect()
    }

    /// The view, in items: the item at its top and how far into it, and how
    /// many items it shows.
    fn view_in_rows(&self) -> Option<(f64, f64)> {
        let height = f64::from(self.scroller.height());
        let on = self.on_screen();
        let (top_at, top_y, top_h) = on.iter().copied().min_by_key(|(p, _, _)| *p)?;
        let (end_at, end_y, end_h) = on.iter().copied().max_by_key(|(p, _, _)| *p)?;
        let top = f64::from(top_at) + (-top_y / top_h).clamp(0.0, 1.0);
        let end = f64::from(end_at) + ((height - end_y) / end_h).clamp(0.0, 1.0);
        Some((top, (end - top).max(0.05)))
    }

    /// Set the bar from the view on the next frame, once however many
    /// times it is asked. Not while the reader holds the slider: it is
    /// theirs until they let go and the view has arrived.
    fn queue_sync(self: &Rc<Self>) {
        if self.sync_queued.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        self.scroller.add_tick_callback(move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.sync_queued.set(false);
                this.sync();
            }
            glib::ControlFlow::Break
        });
    }

    fn sync(&self) {
        let pixels = self.scroller.vadjustment();
        let scrolls = pixels.upper() > pixels.page_size() + 1.0;
        self.bar.set_visible(scrolls);
        if self.target.get().is_some() || self.dragging.get() || !scrolls {
            return;
        }
        let count = self.list.model().map_or(0, |m| m.n_items());
        let Some((top, page)) = self.view_in_rows() else {
            return;
        };
        let upper = f64::from(count).max(page);
        // At the end, the slider is at the end: the last item's share of
        // the view is not a whole item, and a slider a hair short of the
        // bottom reads as more below.
        let value = if super::at_bottom(&pixels) {
            upper - page
        } else if pixels.value() <= 0.0 {
            0.0
        } else {
            top.min(upper - page)
        };
        self.syncing.set(true);
        self.rows.configure(value, 0.0, upper, 1.0, page, page);
        self.syncing.set(false);
    }

    /// Take the view to where the reader put the slider, on the frame
    /// clock: the item it names brought into the list, then moved until
    /// its top sits where the slider says, for as long as the slider is
    /// held and until the view has held still two frames after.
    fn drive(self: &Rc<Self>) {
        if self.driving.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        let (still, after) = (Cell::new(0u32), Cell::new(0u32));
        let asked = Cell::new(None::<u32>);
        self.scroller.add_tick_callback(move |scroller, _| {
            let Some(this) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let Some(target) = this.target.get() else {
                this.driving.set(false);
                return glib::ControlFlow::Break;
            };
            let done = |this: &Self| {
                this.target.set(None);
                this.driving.set(false);
                this.sync();
                glib::ControlFlow::Break
            };
            if !this.dragging.get() {
                after.set(after.get() + 1);
                if after.get() > SETTLE_FRAMES {
                    return done(&this);
                }
            } else {
                after.set(0);
            }
            (this.on_reader)();
            let count = this.list.model().map_or(0, |m| m.n_items());
            if count == 0 {
                return done(&this);
            }
            // The slider at the end of its travel is the end of the
            // conversation, followed from there.
            if target >= this.rows.upper() - this.rows.page_size() - 0.001 {
                (this.to_end)();
                if this.dragging.get() {
                    return glib::ControlFlow::Continue;
                }
                return done(&this);
            }
            let index = (target.floor() as u32).min(count - 1);
            let into = target - f64::from(index);
            let pixels = scroller.vadjustment();
            let found = this.on_screen_or_bound(index);
            let Some((y, h)) = found else {
                // Not built yet: the list brings it in, and the next frame
                // places it.
                if asked.replace(Some(index)) != Some(index) || still.get() > 2 {
                    this.list.scroll_to(index, gtk::ListScrollFlags::NONE, None);
                    still.set(0);
                }
                still.set(still.get() + 1);
                return glib::ControlFlow::Continue;
            };
            let by = y + into * h;
            if by.abs() >= 1.0 {
                pixels.set_value((pixels.value() + by).clamp(
                    pixels.lower(),
                    (pixels.upper() - pixels.page_size()).max(pixels.lower()),
                ));
                still.set(0);
                return glib::ControlFlow::Continue;
            }
            still.set(still.get() + 1);
            if !this.dragging.get() && still.get() >= 2 {
                return done(&this);
            }
            glib::ControlFlow::Continue
        });
    }

    /// The top and height of the item at `position` against the view,
    /// when the list has it bound and laid out — on screen or just off it.
    fn on_screen_or_bound(&self, position: u32) -> Option<(f64, f64)> {
        let items = self.items.borrow();
        items.values().find_map(|item| {
            let item = item.upgrade()?;
            if item.position() != position {
                return None;
            }
            let child = item.child()?;
            if !child.is_mapped() {
                return None;
            }
            let bounds = child.compute_bounds(&self.scroller)?;
            (bounds.height() > 0.0).then(|| (f64::from(bounds.y()), f64::from(bounds.height())))
        })
    }

    /// The wide, grabbable slider while the pointer is near it or it is
    /// held — Adwaita widens an indicator on `hovering` alone, and the
    /// native bar keeps it through a drag.
    fn sync_hover(&self) {
        if self.hovering.get() || self.dragging.get() {
            self.bar.add_css_class("hovering");
        } else {
            self.bar.remove_css_class("hovering");
        }
    }

    /// Bring the indicator up, and let it fade once nothing holds it.
    fn show(self: &Rc<Self>) {
        if !self.overlay {
            return;
        }
        self.bar.add_css_class("shown");
        if let Some(id) = self.hide.borrow_mut().take() {
            id.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(LINGER, move || {
            let Some(this) = weak.upgrade() else { return };
            this.hide.borrow_mut().take();
            if this.hovering.get() || this.dragging.get() || this.target.get().is_some() {
                this.show();
            } else {
                this.bar.remove_css_class("shown");
            }
        });
        *self.hide.borrow_mut() = Some(id);
    }
}

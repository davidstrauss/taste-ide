//! A file tree row's sync status, left of its git letter or its policy mark
//! (David, 2026-10-17: "The straight arrows show policy; the circle/progress
//! ones show status"): two arrows chasing round a circle for a file that
//! syncs by policy and is in step; and for any file on its way between the
//! folder and Personal's checkout, a ring
//! that fills as it moves, with an arrow in it saying which way — up to
//! Personal, down to the folder — and beside it, dim, how much is left and
//! how long (David, 2026-10-17: "change the circular arrows to a progress
//! circle with an arrow in the middle that's up or down, depending on the
//! direction of transfer … show low-contrast text … 'X MiB, Y s
//! remaining'"). The words show only while the whole row fits with them,
//! name and all; otherwise they go, and the ring's tooltip has them (David:
//! "You can drop the remaining size/time text entirely if space is tight.
//! Just show it in the tooltip for the circle"). What it is told comes from
//! `taste_devcontainer::transfers` through `taste_core::Event::SyncFiles`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use taste_core::SyncFile;

/// The ring's box: the hold button's.
const RING: i32 = crate::holdbutton::RING_SIZE;

/// One row's mark. Cheap to clone: the widgets are shared.
#[derive(Clone)]
pub struct SyncMark {
    pub widget: gtk::Box,
    rest: gtk::Image,
    ring: gtk::DrawingArea,
    text: gtk::Label,
    drawn: Rc<RefCell<Option<SyncFile>>>,
    /// The row the words must fit in beside everything else on it.
    row: Rc<RefCell<glib::WeakRef<gtk::Widget>>>,
}

impl SyncMark {
    pub fn new() -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        widget.set_valign(gtk::Align::Center);
        widget.set_visible(false);
        // Left-aligned and against the ring: the words start where the
        // ring's column leaves off, whatever their length.
        let text = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["caption", "dim-label", "numeric"])
            .visible(false)
            .build();
        let ring = gtk::DrawingArea::builder()
            .content_width(RING)
            .content_height(RING)
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        let rest = gtk::Image::from_icon_name("taste-sync-symbolic");
        rest.set_pixel_size(RING);
        rest.add_css_class("dim-label");
        rest.set_visible(false);
        widget.append(&text);
        widget.append(&ring);
        widget.append(&rest);
        let drawn: Rc<RefCell<Option<SyncFile>>> = Rc::default();
        let row: Rc<RefCell<glib::WeakRef<gtk::Widget>>> = Rc::default();
        {
            let drawn = drawn.clone();
            let (row, text) = (row.clone(), text.clone());
            ring.set_draw_func(move |area, cr, width, height| {
                let _doing = crate::stall::doing(|| "drawing a file's sync ring".into());
                if let Some(file) = drawn.borrow().as_ref() {
                    draw(area, cr, width, height, file);
                }
                // A row resized since the words were placed: settled after
                // this frame, never during it.
                if let Some(row) = row.borrow().upgrade() {
                    if words_fit(&row, &text) != Some(text.is_visible()) {
                        let (row, text) = (row.downgrade(), text.clone());
                        glib::idle_add_local_once(move || {
                            if let Some(row) = row.upgrade() {
                                place_words(&row, &text);
                            }
                        });
                    }
                }
            });
        }
        Self {
            widget,
            rest,
            ring,
            text,
            drawn,
            row,
        }
    }

    /// The row whose width decides whether the words fit.
    pub fn set_row(&self, row: &impl IsA<gtk::Widget>) {
        *self.row.borrow_mut() = row.upcast_ref::<gtk::Widget>().downgrade();
    }

    /// Show `moving` when the file is on its way, else the chasing arrows
    /// with `at_rest` as their tooltip when it is given (the file syncs by
    /// policy, and is in step), else nothing.
    pub fn show(&self, moving: Option<&SyncFile>, at_rest: Option<&str>) {
        match moving {
            Some(file) => {
                let unchanged = self.drawn.borrow().as_ref() == Some(file);
                if !unchanged {
                    *self.drawn.borrow_mut() = Some(file.clone());
                    self.ring.queue_draw();
                    let (words, tooltip) = words(file);
                    self.text.set_label(&words);
                    // The ring says it all, whether or not the words fit.
                    self.ring
                        .set_tooltip_text(Some(&format!("{words}\n{tooltip}")));
                }
                self.ring.set_visible(true);
                self.rest.set_visible(false);
                self.widget.set_tooltip_text(None);
                self.widget.set_visible(true);
                match self.row.borrow().upgrade() {
                    Some(row) => place_words(&row, &self.text),
                    None => self.text.set_visible(true),
                }
            }
            None => {
                self.drawn.borrow_mut().take();
                self.text.set_visible(false);
                self.ring.set_visible(false);
                self.rest.set_visible(at_rest.is_some());
                self.widget.set_tooltip_text(at_rest);
                self.widget.set_visible(at_rest.is_some());
            }
        }
    }
}

/// Whether the row has room for the words beside everything else on it,
/// its name whole included: its natural width with the words in, whether
/// they are showing or not — so showing them cannot change the answer —
/// against what it has. `None` before the row has a width.
fn words_fit(row: &gtk::Widget, text: &gtk::Label) -> Option<bool> {
    let width = row.width();
    if width <= 0 {
        return None;
    }
    let (_, natural, _, _) = row.measure(gtk::Orientation::Horizontal, -1);
    let needed = if text.is_visible() {
        natural
    } else {
        // The words' own width and the gap before the ring.
        natural + text.layout().pixel_size().0 + 4
    };
    Some(needed <= width)
}

/// The words shown when they fit and dropped when they do not; shown until
/// the row has a width to say.
fn place_words(row: &gtk::Widget, text: &gtk::Label) {
    let fits = words_fit(row, text).unwrap_or(true);
    if text.is_visible() != fits {
        text.set_visible(fits);
    }
}

/// `340 KiB`, `4.2 MiB`, `85 MiB`, `1.3 GiB`.
pub fn size_words(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KIB * KIB {
        format!("{:.0} KiB", (b / KIB).max(1.0))
    } else if b < 10.0 * KIB * KIB {
        format!("{:.1} MiB", b / (KIB * KIB))
    } else if b < KIB * KIB * KIB {
        format!("{:.0} MiB", b / (KIB * KIB))
    } else {
        format!("{:.1} GiB", b / (KIB * KIB * KIB))
    }
}

/// `8 s`, `3 min`.
fn time_words(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds} s")
    } else {
        format!("{} min", seconds.div_ceil(60))
    }
}

/// The dim words beside the ring, and the row's tooltip.
fn words(file: &SyncFile) -> (String, String) {
    let way = if file.up {
        "Going to Personal"
    } else {
        "Coming to your folder"
    };
    if file.fraction.is_none() {
        return (
            "waiting".to_string(),
            format!("{way}: waiting for the sync to begin"),
        );
    }
    let remaining = file
        .seconds_left
        .map(|s| format!("{} remaining", time_words(s)));
    // What is left of it and how long, whichever way it goes. Coming down,
    // both are estimates against the fetch it rides in.
    let words = match (file.left, remaining) {
        (Some(left), Some(remaining)) => format!("{}, {remaining}", size_words(left)),
        (Some(left), None) => format!("{} left", size_words(left)),
        (None, Some(remaining)) => remaining,
        (None, None) => format!("{:.0}%", file.fraction.unwrap_or(0.0) * 100.0),
    };
    let tooltip = if file.shared {
        format!(
            "{way}, in one transfer with the other files coming down. Git reports only how \
             far through the whole transfer it is, so what is left of this file is \
             estimated from that, and the time is the transfer's."
        )
    } else {
        way.to_string()
    };
    (words, tooltip)
}

/// The hold button's ring (`crate::holdbutton::draw_ring`), filled as far
/// as the file has come, with the arrow in the middle, up or down: drawn in
/// the ring's colour, and cut out of the wedge where the wedge has reached
/// it, so it reads at every fraction.
fn draw(
    area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
    file: &SyncFile,
) {
    let progress = file.fraction.unwrap_or(0.0);
    crate::holdbutton::draw_ring(area, cr, width, height, progress);
    let (cx, cy) = (f64::from(width) / 2.0, f64::from(height) / 2.0);
    let reach = (cx.min(cy) - 1.0) * 0.5;
    let (from, to) = if file.up {
        (cy + reach, cy - reach)
    } else {
        (cy - reach, cy + reach)
    };
    let head = reach * 0.75;
    let back = if file.up { head } else { -head };
    let arrow = || {
        cr.new_path();
        cr.move_to(cx, from);
        cr.line_to(cx, to);
        cr.move_to(cx - head, to + back);
        cr.line_to(cx, to);
        cr.line_to(cx + head, to + back);
    };
    cr.set_line_width(1.5);
    cr.set_line_cap(gtk::cairo::LineCap::Round);
    cr.set_line_join(gtk::cairo::LineJoin::Round);
    let color = area.color();
    // Outside the wedge: drawn.
    cr.save().ok();
    cr.new_path();
    cr.rectangle(0.0, 0.0, f64::from(width), f64::from(height));
    crate::holdbutton::ring_wedge(cr, width, height, progress);
    cr.set_fill_rule(gtk::cairo::FillRule::EvenOdd);
    cr.clip();
    cr.set_source_rgba(
        f64::from(color.red()),
        f64::from(color.green()),
        f64::from(color.blue()),
        f64::from(color.alpha()),
    );
    arrow();
    let _ = cr.stroke();
    cr.restore().ok();
    // Inside it: cut out.
    if progress > 0.0 {
        cr.save().ok();
        cr.new_path();
        crate::holdbutton::ring_wedge(cr, width, height, progress);
        cr.clip();
        cr.set_operator(gtk::cairo::Operator::Clear);
        arrow();
        let _ = cr.stroke();
        cr.restore().ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(up: bool, fraction: Option<f64>, left: Option<u64>, seconds: Option<u64>) -> SyncFile {
        SyncFile {
            up,
            fraction,
            left,
            moved: 0,
            seconds_left: seconds,
            shared: false,
        }
    }

    /// The words beside the ring: how much is left and how long, as the
    /// request put it, and what can be said when one of them is not known.
    #[test]
    fn the_words_say_what_is_left_and_how_long() {
        assert_eq!(
            words(&file(true, Some(0.4), Some(12 << 20), Some(8))).0,
            "12 MiB, 8 s remaining"
        );
        assert_eq!(
            words(&file(true, Some(0.1), Some(3 << 29), Some(200))).0,
            "1.5 GiB, 4 min remaining"
        );
        assert_eq!(
            words(&file(true, Some(0.0), Some(4096), None)).0,
            "4 KiB left"
        );
        assert_eq!(words(&file(false, None, None, None)).0, "waiting");
        // Coming down: what is left of it and how long too, never what
        // has come in.
        let shared = SyncFile {
            moved: 5 << 20,
            shared: true,
            ..file(false, Some(0.5), Some(5 << 20), Some(3))
        };
        assert_eq!(words(&shared).0, "5.0 MiB, 3 s remaining");
        assert!(words(&shared).1.contains("estimated"));
    }
}

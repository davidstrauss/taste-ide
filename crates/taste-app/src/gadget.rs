//! Gadget mode: below a breakpoint, the window IS the monitor.
//!
//! ENVIRONMENTS.md → "Gadget mode: the window is the monitor". Supervising
//! a busy fleet should not require keeping a full IDE focused. Shrink the
//! window into a corner and the four panes give way to the two panels that
//! were already answering the question — the environments, and the backlog
//! under them. Stretch it back and it is the IDE again, with nothing
//! rearranged: one window, and the layout commitment intact.
//!
//! **The card is gone, and that is the point.** This used to be a bespoke
//! render of [`taste_fleetlink::Snapshot`]: its own list of environments,
//! its own state glyphs, its own spend bars, its own review count. It drew
//! the same facts as the environment panel in the file-tree flank and drew
//! them differently — a second widget tree to keep in agreement with the
//! first, and the one that went stale was always whichever the developer
//! was not looking at. So gadget mode shows *the panel itself*, moved here.
//!
//! Four things follow from that, and each is why it is worth doing:
//!
//! - **Reparented, never duplicated.** The widgets are taken out of the
//!   file-tree pane and put in here, exactly as the editor stows a tab set
//!   when the selection moves. Crossing the breakpoint is two
//!   `remove`/`append` pairs — O(panes), no rebuild, no filesystem, no git
//!   — and the panel keeps its scroll position, its filter text, its
//!   sparkline history and its selection because the widgets are never
//!   taken apart.
//! - **Everything the card had, the panel already has.** The traffic
//!   lights, the live sparklines, the waiting-on-you marks, the review
//!   rails, the subscription gauge in its header. The gauge rides along
//!   because it is a child of the panel's own header, not of the window's.
//! - **It is a control panel now, and honestly so.** The old card's rows
//!   clicked through and did nothing else, on the argument that acting
//!   needs room to say what would be lost. The backlog's row actions are
//!   the exception that proves it: reordering a queue costs nothing and
//!   undoes cleanly, which is exactly why it is safe to do from a corner
//!   of the screen. Destroy still lives in the console, where there is
//!   room to enumerate what it would take.
//! - **Not a second window.** Unchanged: Wayland grants apps no
//!   keep-above, and panes never float. The window we already have gets
//!   small.
//!
//! What is left in this module is the breakpoint's two numbers and the
//! container the panels are moved into.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

/// The width, in scalable pixels, at or below which the panes give way.
///
/// Picked to be **unreachable by accident**. The four panes have real
/// minimum widths (`TASTE_MEASURE_MIN=1` prints them) and stop being
/// useful long before they stop fitting, so the number could be much
/// larger — but it also has to sit below every width the desktop hands out
/// on its own. GNOME's keyboard tiling gives a window half the workspace,
/// and the narrowest display anyone runs this on is 1280 logical pixels
/// wide, so a tiled IDE is 640sp; quarter-tiling on a 1280 display is
/// 640×360. 520sp is clear of all of them.
///
/// The consequence is the intended one: gadget mode is entered by
/// dragging a corner, on purpose, and never by tiling an IDE to the side
/// of a browser. In `sp` rather than px so it means the same thing on a
/// HiDPI display.
pub const GADGET_MAX_WIDTH_SP: f64 = 520.0;

/// The width at or below which the flank collapses and the chat becomes a
/// pinned tab in the editor — the middle rung of the responsive ladder.
///
/// Above [`GADGET_MAX_WIDTH_SP`] by a wide margin, because these two are
/// answers to different questions: gadget mode is "I am not editing", and
/// this is "I am editing in half a screen". 960sp is where the four panes
/// stop being four useful panes — the tree, an editor wide enough to read
/// code in, and a chat column at its own minimum do not fit under it — and
/// it is deliberately ABOVE the widths GNOME's tiling hands out, unlike
/// the gadget breakpoint, because being tiled beside a browser is exactly
/// when consolidating helps.
pub const CONSOLIDATED_MAX_WIDTH_SP: f64 = 960.0;

/// The size the window returns to when a row is clicked through, if it is
/// smaller than that. Not the IDE's default size — a user who had it at
/// 1100×700 before shrinking it should not be handed 1440×900 — but the
/// floor that makes four panes legible again.
pub const RESTORED_WIDTH: i32 = 1100;
pub const RESTORED_HEIGHT: i32 = 720;

/// Where gadget mode puts the environment panel and the backlog while the
/// window is too small for panes.
///
/// A container and nothing else: it renders no facts, holds no snapshot and
/// asks nobody anything, because the widgets it is handed are already the
/// ones the IDE draws those facts with.
pub struct Gadget {
    pub widget: gtk::Box,
    /// Where the panels go. Inside a scroller, because two panels plus a
    /// backlog of twenty is taller than a window somebody has dragged into
    /// a corner.
    slot: gtk::Box,
    /// What is currently borrowed, in the order it was taken — which is
    /// the order it goes back in.
    held: RefCell<Vec<gtk::Widget>>,
}

impl Gadget {
    pub fn new() -> Rc<Self> {
        let slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
        slot.set_vexpand(true);

        let scroller = gtk::ScrolledWindow::builder()
            .child(&slot)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        // A probe target of its own, and the name the window's registry
        // knows it by.
        widget.set_widget_name("gadget");
        widget.append(&scroller);

        // Deliberately no header of its own. The window's own header bar
        // already names the workspace and says "fleet monitor" below the
        // breakpoint, two centimetres above this; and the subscription
        // gauge is a child of the environment panel's header, so it
        // arrives with the panel rather than being drawn twice.
        Rc::new(Self {
            widget,
            slot,
            held: RefCell::new(Vec::new()),
        })
    }

    /// Take the panels in. Called when the breakpoint applies.
    ///
    /// The caller has already unparented them; this only re-homes them, in
    /// the order given, which is the order they sat in the flank.
    pub fn adopt(&self, panels: Vec<gtk::Widget>) {
        for panel in &panels {
            self.slot.append(panel);
        }
        *self.held.borrow_mut() = panels;
    }

    /// Hand the panels back. Called when the breakpoint unapplies, and it
    /// must be exactly the inverse — "stretch back to the IDE, nothing
    /// rearranged" is the commitment, and a panel left behind here would
    /// be the file-tree pane silently losing its bottom half.
    pub fn release(&self) -> Vec<gtk::Widget> {
        let panels = std::mem::take(&mut *self.held.borrow_mut());
        for panel in &panels {
            self.slot.remove(panel);
        }
        panels
    }

    /// Whether the panels are here right now. The guard against a double
    /// apply — AdwBreakpoint fires `apply` on the breakpoint being added
    /// as well as on the window crossing it.
    pub fn holding(&self) -> bool {
        !self.held.borrow().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The breakpoint has to be unreachable by tiling, or an IDE snapped
    /// to half a screen turns into a monitor and the user loses their
    /// panes to a gesture they meant as "put this beside my browser".
    #[test]
    fn the_breakpoint_is_below_every_size_the_desktop_hands_out() {
        // Half and quarter tiling on the narrowest display this targets,
        // and on the common ones.
        for display_width in [1280.0, 1366.0, 1440.0, 1920.0, 2560.0] {
            assert!(
                GADGET_MAX_WIDTH_SP < display_width / 2.0,
                "half-tiling a {display_width}px display would trip the gadget"
            );
        }
        // Quarter-tiling is half-tiling's width, so the loop above
        // already covers it. What is left to check is that the size a
        // click-through restores actually clears the breakpoint — a
        // restore that landed the user back in the card would be a loop.
        assert!(f64::from(RESTORED_WIDTH) > GADGET_MAX_WIDTH_SP * 2.0);
    }

    /// The ladder has three rungs and they have to stay in order, with the
    /// restored size landing on the top one. Consolidating is for a window
    /// somebody is still editing in; the gadget is for one they are not.
    #[test]
    fn the_responsive_ladder_is_ordered_and_restores_to_the_top_of_it() {
        // Both of these are decidable from the constants alone, so they
        // are decided at compile time: a ladder whose rungs crossed over
        // should not build, let alone reach a test run.
        const {
            assert!(
                GADGET_MAX_WIDTH_SP < CONSOLIDATED_MAX_WIDTH_SP,
                "the gadget must be the narrower of the two, or it swallows the middle rung"
            );
            // Half-tiling is where consolidating earns its place, so the
            // middle breakpoint is deliberately ABOVE the widths tiling
            // hands out — the opposite of the gadget's rule, and for the
            // opposite reason.
            assert!(CONSOLIDATED_MAX_WIDTH_SP >= 1920.0 / 2.0);
        }
        // A click-through restores to a width with four panes in it, which
        // means clearing BOTH rungs and not just the lower one.
        assert!(f64::from(RESTORED_WIDTH) > CONSOLIDATED_MAX_WIDTH_SP);
    }
}

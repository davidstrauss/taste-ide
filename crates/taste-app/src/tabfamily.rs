//! One strip, three families, and the rule that keeps them apart.
//!
//! ENVIRONMENTS.md → "The responsive ladder". **No nested tab sets**: every
//! leaf view is a first-class tab in its region's one strip, and below
//! `CONSOLIDATED_MAX_WIDTH_SP` the window has exactly one strip — the
//! editor's — with the chat pane's views and the console's tabs grafted
//! onto its end.
//!
//! Grafted tabs are *guests* in that strip. They arrive as a family, they
//! stay together, and they stay trailing: a file dragged past them would
//! otherwise interleave documents with panes and leave the user with a
//! strip whose order says nothing. That rule is the whole of this module,
//! written as data so it can be tested without a display.

/// Which set of tabs a page belongs to.
///
/// The order of the variants IS the order of the families in the strip:
/// documents first, because the strip is the document strip and the rest
/// are visiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Family {
    /// The user's open files (and review diffs) — the strip's own.
    Document,
    /// The chat pane's three views: the conversation, its utilization, and
    /// the agent's settings.
    Chat,
    /// The console pane's tabs: the environment's sections, Services, and
    /// every terminal.
    Console,
}

impl Family {
    /// Where this family sits, low first.
    pub fn rank(self) -> usize {
        match self {
            Family::Document => 0,
            Family::Chat => 1,
            Family::Console => 2,
        }
    }
}

/// A rung of the responsive ladder, named by what it does to the panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    /// Four panes, four columns.
    Full,
    /// One strip: the chat column and the console pane give up being panes
    /// and become tabs.
    Consolidated,
    /// No panes at all — the window IS the monitor.
    Gadget,
}

impl Rung {
    /// Which rung a window of this width (in sp) is on. Widest first, the
    /// same order the breakpoints are added in, because at 400sp both
    /// conditions match and the narrower one has to win.
    pub fn of_width(width_sp: f64) -> Rung {
        if width_sp <= crate::gadget::GADGET_MAX_WIDTH_SP {
            Rung::Gadget
        } else if width_sp <= crate::gadget::CONSOLIDATED_MAX_WIDTH_SP {
            Rung::Consolidated
        } else {
            Rung::Full
        }
    }
}

/// What the document strip carries at a rung.
///
/// The composition of the one strip, as a fact rather than as the sum of
/// two breakpoint callbacks — which is what makes it checkable.
pub fn strip_families(rung: Rung) -> &'static [Family] {
    match rung {
        Rung::Full => &[Family::Document],
        Rung::Consolidated => &[Family::Document, Family::Chat, Family::Console],
        // The panes are not on screen at all down here, so the strip is
        // not either. It keeps its documents — nothing is torn down — but
        // nothing is grafted onto it.
        Rung::Gadget => &[Family::Document],
    }
}

/// The order these pages should sit in, as indices into the given slice.
///
/// Stable within a family: the user's own drags decide the order of their
/// files, and of the terminals among themselves. What is not theirs to
/// decide is whether a file may sit between two panes.
pub fn family_order(families: &[Family]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..families.len()).collect();
    order.sort_by_key(|&index| (families[index].rank(), index));
    order
}

/// Is the strip already in family order? The guard's fast path — a drag
/// that changed nothing structural must not provoke a reorder storm.
pub fn is_settled(families: &[Family]) -> bool {
    families.windows(2).all(|pair| pair[0] <= pair[1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use Family::{Chat, Console, Document};

    #[test]
    fn a_strip_in_order_is_left_alone() {
        let strip = [Document, Document, Chat, Chat, Chat, Console, Console];
        assert!(is_settled(&strip));
        assert_eq!(family_order(&strip), vec![0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn a_file_dragged_past_the_panes_is_put_back_in_front_of_them() {
        // [chat] [file] [log]: the user dropped a document between two
        // grafted families.
        let strip = [Chat, Document, Console];
        assert!(!is_settled(&strip));
        assert_eq!(family_order(&strip), vec![1, 0, 2]);
    }

    #[test]
    fn the_families_keep_their_own_internal_order() {
        // Terminals reordered among themselves, and files among
        // themselves, survive untouched: only the family boundary moves.
        let strip = [Console, Console, Document, Document, Chat];
        assert_eq!(family_order(&strip), vec![2, 3, 4, 0, 1]);
    }

    #[test]
    fn an_empty_or_single_strip_is_settled() {
        assert!(is_settled(&[]));
        assert!(is_settled(&[Console]));
        assert_eq!(family_order(&[]), Vec::<usize>::new());
    }

    #[test]
    fn ordering_is_idempotent() {
        let strip = [Console, Chat, Document, Console, Document];
        let once: Vec<Family> = family_order(&strip).iter().map(|&i| strip[i]).collect();
        assert!(is_settled(&once));
        assert_eq!(family_order(&once), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn only_the_consolidated_rung_grafts_anything() {
        assert_eq!(strip_families(Rung::Full), &[Document]);
        assert_eq!(
            strip_families(Rung::Consolidated),
            &[Document, Chat, Console]
        );
        assert_eq!(strip_families(Rung::Gadget), &[Document]);
    }

    #[test]
    fn the_rungs_are_read_widest_first() {
        use crate::gadget::{CONSOLIDATED_MAX_WIDTH_SP, GADGET_MAX_WIDTH_SP};
        assert_eq!(Rung::of_width(1440.0), Rung::Full);
        assert_eq!(
            Rung::of_width(CONSOLIDATED_MAX_WIDTH_SP),
            Rung::Consolidated
        );
        assert_eq!(Rung::of_width(900.0), Rung::Consolidated);
        // Both breakpoint conditions match down here; the narrower rung
        // wins, exactly as libadwaita's last-match rule makes it.
        assert_eq!(Rung::of_width(GADGET_MAX_WIDTH_SP), Rung::Gadget);
        assert_eq!(Rung::of_width(400.0), Rung::Gadget);
    }
}

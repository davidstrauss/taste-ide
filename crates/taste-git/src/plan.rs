//! The sync's decision, as a pure function (docs/SYNC.md → "The decision").
//!
//! Three working copies in — the archive, what both sides last agreed on;
//! the folder; and the checkout, as a pass observed them — and per path one
//! of: nothing, a write to the side that did not change it, an agreement for
//! the archive, or a conflict. No I/O: what reads the working copies and
//! what carries out the writes are `crate::mirror` and
//! `taste_devcontainer::peer`, and this is the part of the sync that can be
//! checked against its specification over every case of a small universe,
//! which `tests` does.
//!
//! Generic over the entry, which the sync makes a blob and its mode and the
//! tests a small number, since only equality is ever asked of one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// One side of the sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Folder,
    Checkout,
}

/// The user's answer to a conflict: the side to keep, for the paths they
/// were asked about and no others (docs/SYNC.md, P2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolve {
    pub keep: Side,
    pub paths: BTreeSet<PathBuf>,
}

/// One path written on one side: `entry` the new one (`None`, removed), and
/// `expect` what the side held there when the pass observed it, which is
/// what the write checks for before it lands (P1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Write<E> {
    pub path: PathBuf,
    pub entry: Option<E>,
    pub expect: Option<E>,
}

/// What a pass does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan<E> {
    pub to_folder: Vec<Write<E>>,
    pub to_checkout: Vec<Write<E>>,
    /// Paths both sides changed to the same entry: nothing to write, and
    /// the archive takes the entry.
    pub agreed: Vec<(PathBuf, Option<E>)>,
    /// Paths out of scope that the archive still records: it lets them go,
    /// so one coming back into scope is decided afresh (P7).
    pub forget: Vec<PathBuf>,
    /// Paths written on neither side until the user answers.
    pub conflicts: Vec<PathBuf>,
}

impl<E> Default for Plan<E> {
    fn default() -> Self {
        Plan {
            to_folder: Vec::new(),
            to_checkout: Vec::new(),
            agreed: Vec::new(),
            forget: Vec::new(),
            conflicts: Vec::new(),
        }
    }
}

impl<E> Plan<E> {
    /// Whether the pass has anything to write or record.
    pub fn is_empty(&self) -> bool {
        self.to_folder.is_empty()
            && self.to_checkout.is_empty()
            && self.agreed.is_empty()
            && self.forget.is_empty()
    }
}

/// The three working copies a pass decides from.
#[derive(Debug, Clone, Copy)]
pub struct Observed<'a, E> {
    pub archive: &'a BTreeMap<PathBuf, E>,
    pub folder: &'a BTreeMap<PathBuf, E>,
    pub checkout: &'a BTreeMap<PathBuf, E>,
    /// Paths the folder's working copy leaves out that are on its disk all
    /// the same — ignored there, whatever the rules the pass reads say. Out
    /// of the folder's scope: unchanged as far as the sync knows, and never
    /// written (2026-10-04: a Drupal install, ignored in the folder and not
    /// yet in the checkout, was deleted from the checkout file by file).
    pub folder_unseen: &'a BTreeSet<PathBuf>,
}

/// Decide every path (docs/SYNC.md → "The decision"). `in_scope` is the
/// rules both sides share: a path it rejects is never written, and the
/// archive forgets it. `resolve` is the user's answer to a conflict, if
/// they gave one.
pub fn plan<E: Clone + Eq>(
    observed: &Observed<'_, E>,
    in_scope: impl Fn(&Path) -> bool,
    resolve: Option<&Resolve>,
) -> Plan<E> {
    let Observed {
        archive,
        folder,
        checkout,
        folder_unseen,
    } = *observed;
    let paths: BTreeSet<&PathBuf> = archive
        .keys()
        .chain(folder.keys())
        .chain(checkout.keys())
        .collect();
    let mut plan = Plan::default();
    for path in paths {
        if !in_scope(path) {
            if archive.contains_key(path) {
                plan.forget.push(path.clone());
            }
            continue;
        }
        let a = archive.get(path);
        let unseen = folder_unseen.contains(path);
        let f = if unseen { a } else { folder.get(path) };
        let c = checkout.get(path);
        let write = |entry: Option<&E>, expect: Option<&E>| Write {
            path: path.clone(),
            entry: entry.cloned(),
            expect: expect.cloned(),
        };
        match (f != a, c != a) {
            (false, false) => {}
            (true, false) => plan.to_checkout.push(write(f, c)),
            // Out of the folder's scope: left as it is there.
            (false, true) if unseen => {}
            (false, true) => plan.to_folder.push(write(c, f)),
            (true, true) if f == c => plan.agreed.push((path.clone(), f.cloned())),
            (true, true) => match resolve.filter(|r| r.paths.contains(path)) {
                Some(r) if r.keep == Side::Folder && !unseen => plan.to_checkout.push(write(f, c)),
                Some(r) if r.keep == Side::Checkout && !unseen => plan.to_folder.push(write(c, f)),
                _ => plan.conflicts.push(path.clone()),
            },
        }
    }
    // A file and a directory never meet at one path (P12): a write that
    // would leave a path and something beneath it both on a side is not
    // made, and its path is a conflict, until nothing is left that would.
    loop {
        let folder_after = after(folder.keys().chain(folder_unseen.iter()), &plan.to_folder);
        let checkout_after = after(checkout.keys(), &plan.to_checkout);
        let clashing: BTreeSet<PathBuf> = clashes(&folder_after, &plan.to_folder)
            .into_iter()
            .chain(clashes(&checkout_after, &plan.to_checkout))
            .collect();
        if clashing.is_empty() {
            break;
        }
        plan.to_folder.retain(|w| !clashing.contains(&w.path));
        plan.to_checkout.retain(|w| !clashing.contains(&w.path));
        plan.conflicts.extend(clashing);
    }
    plan.conflicts.sort();
    plan.conflicts.dedup();
    plan
}

/// The paths a side holds once `writes` have landed on `present`.
fn after<'a, E>(
    present: impl Iterator<Item = &'a PathBuf>,
    writes: &[Write<E>],
) -> BTreeSet<PathBuf> {
    let mut paths: BTreeSet<PathBuf> = present.cloned().collect();
    for write in writes {
        if write.entry.is_some() {
            paths.insert(write.path.clone());
        } else {
            paths.remove(&write.path);
        }
    }
    paths
}

/// The written paths, among `writes`, that leave a path and one beneath it
/// both in `paths`.
fn clashes<E>(paths: &BTreeSet<PathBuf>, writes: &[Write<E>]) -> Vec<PathBuf> {
    let written: BTreeSet<&PathBuf> = writes
        .iter()
        .filter(|w| w.entry.is_some())
        .map(|w| &w.path)
        .collect();
    let mut out = Vec::new();
    for path in paths {
        let mut ancestor = path.parent();
        while let Some(dir) = ancestor.filter(|d| !d.as_os_str().is_empty()) {
            if paths.contains(dir) {
                // Whichever of the two this pass wrote is the one not made.
                if written.contains(path) {
                    out.push(path.clone());
                }
                if written.contains(&dir.to_path_buf()) {
                    out.push(dir.to_path_buf());
                }
            }
            ancestor = dir.parent();
        }
    }
    out
}

/// A working copy after `plan`'s writes to `side` landed — those in
/// `landed`, by their index in that side's list — for what records them.
pub fn applied<E: Clone>(
    copy: &BTreeMap<PathBuf, E>,
    writes: &[Write<E>],
    landed: impl Fn(usize) -> bool,
) -> BTreeMap<PathBuf, E> {
    let mut copy = copy.clone();
    for (i, write) in writes.iter().enumerate() {
        if !landed(i) {
            continue;
        }
        match &write.entry {
            Some(entry) => {
                copy.insert(write.path.clone(), entry.clone());
            }
            None => {
                copy.remove(&write.path);
            }
        }
    }
    copy
}

/// The archive after a pass: each write that landed, each agreement, and
/// each path let go (P10: a write that did not land records nothing).
pub fn archived<E: Clone>(
    archive: &BTreeMap<PathBuf, E>,
    plan: &Plan<E>,
    landed_folder: impl Fn(usize) -> bool,
    landed_checkout: impl Fn(usize) -> bool,
) -> BTreeMap<PathBuf, E> {
    let mut next = applied(archive, &plan.to_folder, landed_folder);
    next = applied(&next, &plan.to_checkout, landed_checkout);
    for (path, entry) in &plan.agreed {
        match entry {
            Some(entry) => {
                next.insert(path.clone(), entry.clone());
            }
            None => {
                next.remove(path);
            }
        }
    }
    for path in &plan.forget {
        next.remove(path);
    }
    next
}

#[cfg(test)]
mod tests {
    //! The planner against docs/SYNC.md, over every case of a small
    //! universe: three paths — `a`, `a/b` beneath it (so a file and a
    //! directory can meet), and `c` beside them — each absent or one of two
    //! contents in each of the archive, the folder, and the checkout; every
    //! set of unseen paths the folder could have; `c` in scope and out of
    //! it; and every subset of each plan's writes landing, for a pass
    //! stopped partway or a write that found its side changed.

    use super::*;

    type Copy = BTreeMap<PathBuf, u8>;

    const PATHS: [&str; 3] = ["a", "a/b", "c"];

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// Every working copy of the universe a disk could hold: `a` and `a/b`
    /// never both.
    fn copies() -> Vec<Copy> {
        let mut out = Vec::new();
        for a in 0..3u8 {
            for ab in 0..3u8 {
                if a != 0 && ab != 0 {
                    continue;
                }
                for c in 0..3u8 {
                    let mut copy = Copy::new();
                    for (path, v) in PATHS.iter().zip([a, ab, c]) {
                        if v != 0 {
                            copy.insert(p(path), v);
                        }
                    }
                    out.push(copy);
                }
            }
        }
        out
    }

    fn well_formed(paths: &BTreeSet<PathBuf>) -> bool {
        !(paths.contains(&p("a")) && paths.contains(&p("a/b")))
    }

    /// The sets of paths the folder could have on its disk unobserved: ones
    /// its copy lacks, that the disk could hold beside what it has.
    fn unseen_sets(archive: &Copy, folder: &Copy, checkout: &Copy) -> Vec<BTreeSet<PathBuf>> {
        let candidates: Vec<PathBuf> = PATHS
            .iter()
            .map(|s| p(s))
            .filter(|path| !folder.contains_key(path))
            .filter(|path| archive.contains_key(path) || checkout.contains_key(path))
            .collect();
        let mut out = Vec::new();
        for mask in 0..(1u32 << candidates.len()) {
            let set: BTreeSet<PathBuf> = candidates
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, path)| path.clone())
                .collect();
            let disk: BTreeSet<PathBuf> =
                folder.keys().cloned().chain(set.iter().cloned()).collect();
            if well_formed(&disk) {
                out.push(set);
            }
        }
        out
    }

    struct Case<'a> {
        archive: &'a Copy,
        folder: &'a Copy,
        checkout: &'a Copy,
        unseen: &'a BTreeSet<PathBuf>,
        scope_c: bool,
    }

    impl Case<'_> {
        fn in_scope(&self) -> impl Fn(&Path) -> bool + '_ {
            move |path: &Path| self.scope_c || path != Path::new("c")
        }

        fn plan(&self, resolve: Option<&Resolve>) -> Plan<u8> {
            plan(
                &Observed {
                    archive: self.archive,
                    folder: self.folder,
                    checkout: self.checkout,
                    folder_unseen: self.unseen,
                },
                self.in_scope(),
                resolve,
            )
        }

        fn describe(&self) -> String {
            format!(
                "A={:?} F={:?} C={:?} unseen={:?} c in scope={}",
                self.archive, self.folder, self.checkout, self.unseen, self.scope_c
            )
        }
    }

    /// The folder as the pass treats it: its copy, with each unseen path at
    /// the archive's entry.
    fn folder_as_decided(case: &Case) -> Copy {
        let mut folder = case.folder.clone();
        for path in case.unseen {
            if let Some(e) = case.archive.get(path) {
                folder.insert(path.clone(), *e);
            }
        }
        folder
    }

    /// Every property of docs/SYNC.md a plan can be held to on its own.
    fn check(case: &Case, plan: &Plan<u8>, resolve: Option<&Resolve>) {
        let why = case.describe();
        let in_scope = case.in_scope();
        let resolved = |path: &PathBuf| resolve.is_some_and(|r| r.paths.contains(path));
        let decided_folder = folder_as_decided(case);

        // P7: nothing out of scope is written, and nothing unseen in the
        // folder is written there or sent from there.
        for w in plan.to_folder.iter().chain(&plan.to_checkout) {
            assert!(
                in_scope(&w.path),
                "P7: wrote {:?} out of scope: {why}",
                w.path
            );
        }
        for w in &plan.to_folder {
            assert!(
                !case.unseen.contains(&w.path),
                "P7: wrote unseen {:?}: {why}",
                w.path
            );
        }
        for w in &plan.to_checkout {
            assert!(
                !case.unseen.contains(&w.path),
                "P7: sent unseen {:?}: {why}",
                w.path
            );
        }

        // P1: a write lands only where its side has not changed the path
        // since the archive — unless the user answered for that path — and
        // it expects exactly what the side holds.
        for w in &plan.to_folder {
            assert_eq!(
                w.expect.as_ref(),
                case.folder.get(&w.path),
                "P1 expect: {why}"
            );
            if !resolved(&w.path) {
                assert_eq!(
                    decided_folder.get(&w.path),
                    case.archive.get(&w.path),
                    "P1: overwrote the folder's change at {:?}: {why}",
                    w.path
                );
            }
        }
        for w in &plan.to_checkout {
            assert_eq!(
                w.expect.as_ref(),
                case.checkout.get(&w.path),
                "P1 expect: {why}"
            );
            if !resolved(&w.path) {
                assert_eq!(
                    case.checkout.get(&w.path),
                    case.archive.get(&w.path),
                    "P1: overwrote the checkout's change at {:?}: {why}",
                    w.path
                );
            }
        }

        // P2: a path both changed, differently, is written on neither side
        // unless answered; and is a conflict when unanswered.
        for path in PATHS.iter().map(|s| p(s)) {
            if !in_scope(&path) || case.unseen.contains(&path) {
                continue;
            }
            let (a, f, c) = (
                case.archive.get(&path),
                case.folder.get(&path),
                case.checkout.get(&path),
            );
            if f != a && c != a && f != c && !resolved(&path) {
                assert!(
                    plan.conflicts.contains(&path),
                    "P2: {path:?} both changed, not a conflict: {why}"
                );
                assert!(
                    !plan
                        .to_folder
                        .iter()
                        .chain(&plan.to_checkout)
                        .any(|w| w.path == path),
                    "P2: wrote {path:?}, a conflict: {why}"
                );
            }
        }

        // P3 (one pass): a path changed on exactly one side is written to
        // the other, or held back as a conflict by a clash of kinds.
        for path in PATHS.iter().map(|s| p(s)) {
            if !in_scope(&path) || case.unseen.contains(&path) {
                continue;
            }
            let (a, f, c) = (
                case.archive.get(&path),
                case.folder.get(&path),
                case.checkout.get(&path),
            );
            let to_checkout = plan.to_checkout.iter().any(|w| w.path == path);
            let to_folder = plan.to_folder.iter().any(|w| w.path == path);
            let held = plan.conflicts.contains(&path);
            if f != a && c == a {
                assert!(to_checkout || held, "P3: {path:?} not sent: {why}");
            }
            if c != a && f == a {
                assert!(to_folder || held, "P3: {path:?} not received: {why}");
            }
        }

        // P12: neither side ends holding a path and one beneath it.
        let folder_paths: BTreeSet<PathBuf> = applied(case.folder, &plan.to_folder, |_| true)
            .into_keys()
            .chain(case.unseen.iter().cloned())
            .collect();
        let checkout_paths: BTreeSet<PathBuf> = applied(case.checkout, &plan.to_checkout, |_| true)
            .into_keys()
            .collect();
        assert!(well_formed(&folder_paths), "P12 in the folder: {why}");
        assert!(well_formed(&checkout_paths), "P12 in the checkout: {why}");
    }

    /// A pass's every write landing, and the archive recording it: the
    /// sides agree on every path in scope that is not a conflict (P4),
    /// nothing removed on one side and unchanged on the other comes back
    /// (P5), and a second pass has nothing to do (P4, P6).
    fn check_settled(case: &Case, plan: &Plan<u8>) {
        let why = case.describe();
        let in_scope = case.in_scope();
        let folder = applied(case.folder, &plan.to_folder, |_| true);
        let checkout = applied(case.checkout, &plan.to_checkout, |_| true);
        let archive = archived(case.archive, plan, |_| true, |_| true);
        for path in PATHS.iter().map(|s| p(s)) {
            if !in_scope(&path) || case.unseen.contains(&path) || plan.conflicts.contains(&path) {
                continue;
            }
            assert_eq!(folder.get(&path), checkout.get(&path), "P4 {path:?}: {why}");
            assert_eq!(
                archive.get(&path),
                folder.get(&path),
                "P4 archive {path:?}: {why}"
            );
            // P5.
            let (a, f, c) = (
                case.archive.get(&path),
                case.folder.get(&path),
                case.checkout.get(&path),
            );
            if a.is_some() && ((f.is_none() && c == a) || (c.is_none() && f == a)) {
                assert!(
                    !folder.contains_key(&path) && !checkout.contains_key(&path),
                    "P5: {path:?} came back: {why}"
                );
            }
        }
        let again = plan_of(&archive, &folder, &checkout, case);
        assert!(
            again.to_folder.is_empty() && again.to_checkout.is_empty() && again.agreed.is_empty(),
            "P4: a settled pass would write again {again:?}: {why}"
        );
        assert_eq!(
            again.conflicts, plan.conflicts,
            "P4: conflicts moved: {why}"
        );
    }

    fn plan_of(archive: &Copy, folder: &Copy, checkout: &Copy, case: &Case) -> Plan<u8> {
        let next = Case {
            archive,
            folder,
            checkout,
            unseen: case.unseen,
            scope_c: case.scope_c,
        };
        next.plan(None)
    }

    /// P10: a pass that stopped with only some of its writes landed — and
    /// the archive recording either nothing (stopped before it) or only
    /// those (it finished, and the rest found their sides changed) — leaves
    /// the next pass overwriting nothing, and settling.
    fn check_partial(case: &Case, plan: &Plan<u8>) {
        let why = case.describe();
        let n_f = plan.to_folder.len();
        let n_c = plan.to_checkout.len();
        for mask in 0..(1u32 << (n_f + n_c)) {
            let lf = |i: usize| mask & (1 << i) != 0;
            let lc = |i: usize| mask & (1 << (n_f + i)) != 0;
            let folder = applied(case.folder, &plan.to_folder, lf);
            let checkout = applied(case.checkout, &plan.to_checkout, lc);
            for archive in [case.archive.clone(), archived(case.archive, plan, lf, lc)] {
                let next = Case {
                    archive: &archive,
                    folder: &folder,
                    checkout: &checkout,
                    unseen: case.unseen,
                    scope_c: case.scope_c,
                };
                let again = next.plan(None);
                check(&next, &again, None);
                // What landed is not undone: a path the first pass wrote
                // on a side is not written back on that side by the next.
                for (i, w) in plan.to_folder.iter().enumerate() {
                    if lf(i) {
                        assert!(
                            !again.to_folder.iter().any(|x| x.path == w.path),
                            "P10: rewrote {:?} in the folder: {why} landed {mask:b}",
                            w.path
                        );
                    }
                }
                for (i, w) in plan.to_checkout.iter().enumerate() {
                    if lc(i) {
                        assert!(
                            !again.to_checkout.iter().any(|x| x.path == w.path),
                            "P10: rewrote {:?} in the checkout: {why} landed {mask:b}",
                            w.path
                        );
                    }
                }
                check_settled(&next, &again);
                // And against what each side actually did, not what an
                // archive says: a path only one side changed ends with that
                // side's entry on both, however the first pass was cut
                // short — so an archive that records a write which never
                // landed, and reads the stale side as the one that changed,
                // is caught.
                let folder_end = applied(&folder, &again.to_folder, |_| true);
                let checkout_end = applied(&checkout, &again.to_checkout, |_| true);
                for path in PATHS.iter().map(|s| p(s)) {
                    if !case.in_scope()(&path)
                        || case.unseen.contains(&path)
                        || plan.conflicts.contains(&path)
                        || again.conflicts.contains(&path)
                    {
                        continue;
                    }
                    let (a, f, c) = (
                        case.archive.get(&path),
                        case.folder.get(&path),
                        case.checkout.get(&path),
                    );
                    let want = match (f != a, c != a) {
                        (true, false) => f,
                        (false, true) => c,
                        (false, false) => a,
                        (true, true) if f == c => f,
                        (true, true) => continue,
                    };
                    assert_eq!(
                        folder_end.get(&path),
                        want,
                        "P1/P3: the folder ends wrong at {path:?}: {why} landed {mask:b}"
                    );
                    assert_eq!(
                        checkout_end.get(&path),
                        want,
                        "P1/P3: the checkout ends wrong at {path:?}: {why} landed {mask:b}"
                    );
                }
            }
        }
    }

    #[test]
    fn every_case_of_the_small_universe_holds_the_specification() {
        let copies = copies();
        let mut cases = 0usize;
        let mut conflicts = 0usize;
        for archive in &copies {
            for folder in &copies {
                for checkout in &copies {
                    for unseen in unseen_sets(archive, folder, checkout) {
                        for scope_c in [true, false] {
                            let case = Case {
                                archive,
                                folder,
                                checkout,
                                unseen: &unseen,
                                scope_c,
                            };
                            let plan = case.plan(None);
                            check(&case, &plan, None);
                            check_settled(&case, &plan);
                            check_partial(&case, &plan);
                            cases += 1;
                            if plan.conflicts.is_empty() {
                                continue;
                            }
                            conflicts += 1;
                            // Answered, either way: the answer writes the
                            // paths asked about, and only those, and then
                            // the sides settle with no conflict left.
                            for keep in [Side::Folder, Side::Checkout] {
                                let resolve = Resolve {
                                    keep,
                                    paths: plan.conflicts.iter().cloned().collect(),
                                };
                                let answered = case.plan(Some(&resolve));
                                check(&case, &answered, Some(&resolve));
                                for w in answered.to_folder.iter().chain(&answered.to_checkout) {
                                    let was_conflict = plan.conflicts.contains(&w.path);
                                    let was_write = plan
                                        .to_folder
                                        .iter()
                                        .chain(&plan.to_checkout)
                                        .any(|x| x == w);
                                    assert!(
                                        was_conflict || was_write,
                                        "P2: the answer wrote {:?}, not asked about: {}",
                                        w.path,
                                        case.describe()
                                    );
                                }
                                check_settled(&case, &answered);
                            }
                        }
                    }
                }
            }
        }
        // The universe is what it says: every combination, conflicts among
        // them.
        assert!(cases > 10_000, "{cases} cases");
        assert!(conflicts > 1_000, "{conflicts} with a conflict");
    }

    /// The rename that came back (f0ae3a2), as the planner sees it: sent,
    /// renamed before the two agreed, observed with the checkout holding
    /// the old name.
    #[test]
    fn a_rename_during_a_send_is_a_removal_and_a_new_file() {
        let archive = Copy::new();
        let folder: Copy = [(p("amex.qfx"), 1)].into();
        let checkout: Copy = [(p("activity.qfx"), 1)].into();
        // The send of activity.qfx landed and was recorded per path.
        let archive_after: Copy = [(p("activity.qfx"), 1)].into();
        let _ = archive;
        let plan = plan(
            &Observed {
                archive: &archive_after,
                folder: &folder,
                checkout: &checkout,
                folder_unseen: &BTreeSet::new(),
            },
            |_| true,
            None,
        );
        assert_eq!(
            plan.to_checkout,
            vec![
                Write {
                    path: p("activity.qfx"),
                    entry: None,
                    expect: Some(1)
                },
                Write {
                    path: p("amex.qfx"),
                    entry: Some(1),
                    expect: None
                },
            ]
        );
        assert!(plan.to_folder.is_empty());
    }
}

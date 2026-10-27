//! The tracked files git converts between their bytes and their blobs, for
//! the folder's snapshot (`snapshot::worktree_tree`).
//!
//! A snapshot is HEAD's tree with what `git status` shows changed laid
//! over it. Status undoes git's own conversions before it compares — an
//! `ident` file's `$Id: <blob> $` back to `$Id$`, a `text` file's CRLF
//! back to LF, a `working-tree-encoding` file back to UTF-8 — so it calls
//! such a file unchanged, and the snapshot took HEAD's blob for it. The
//! checkout's snapshot, in the VM, hashes the bytes on disk. The two never
//! agreed on those files, so every pass of the folder's sync planned a
//! write for each and missed it: php-src's seven `ident` files, on every
//! pass (David, 2026-10-26: "The $Id$ issue is probably related to the
//! gitattributes work earlier today").
//!
//! So the snapshot is also given each converted file whose size on disk
//! is not its blob's — an expansion, a line ending, and an encoding each
//! change a file's length — and hashes its bytes as it does a changed
//! file's. Which files are converted is worked out once per working tree
//! and kept: not at all when no attributes file names a converting
//! attribute, which is most repositories; else by asking libgit2 about
//! every file once, and after that, when HEAD moves, about only the paths
//! the tree's diff names. Any attributes file that changes starts it over.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use git2::{AttrCheckFlags, AttrValue, Oid, Repository, Tree};

/// The attributes that make a file's bytes differ from its blob.
const CONVERTING: [&str; 5] = ["ident", "text", "eol", "crlf", "working-tree-encoding"];

/// When an attributes file was last seen: its modification time and size,
/// or `None` while it is absent.
type Stamp = Option<(i64, u32, u64)>;

fn stamp(path: &Path) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.mtime(), meta.mtime_nsec() as u32, meta.len()))
}

fn is_rules(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name == ".gitattributes")
}

/// One working tree's converted files, as of `tree`.
struct Known {
    /// The attributes files that applied, each as it was seen.
    rules: Vec<(PathBuf, Stamp)>,
    /// The converting attributes those files name; none, and nothing is
    /// asked of libgit2 at all.
    named: Vec<&'static str>,
    tree: Oid,
    /// Each converted file, by its path from the root, with its blob's size.
    files: HashMap<PathBuf, u64>,
}

fn known() -> &'static Mutex<HashMap<PathBuf, Known>> {
    static KNOWN: OnceLock<Mutex<HashMap<PathBuf, Known>>> = OnceLock::new();
    KNOWN.get_or_init(Default::default)
}

/// Whether `path` is converted, by the attributes `named`.
fn converts(repo: &Repository, path: &Path, named: &[&'static str]) -> bool {
    named.iter().any(|name| {
        let value = repo
            .get_attr(path, name, AttrCheckFlags::FILE_THEN_INDEX)
            .ok()
            .flatten();
        matches!(
            AttrValue::from_string(value),
            AttrValue::True | AttrValue::String(_) | AttrValue::Bytes(_)
        )
    })
}

fn blob_size(repo: &Repository, id: Oid) -> Option<u64> {
    repo.odb()
        .ok()?
        .read_header(id)
        .ok()
        .map(|(size, _)| size as u64)
}

/// Worked out from nothing, against `head`.
fn whole(repo: &Repository, workdir: &Path, head: &Tree) -> Result<Known> {
    let mut rules: Vec<PathBuf> = Vec::new();
    let mut blobs: Vec<(PathBuf, Oid)> = Vec::new();
    head.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
        if entry.kind() == Some(git2::ObjectType::Blob) {
            let path = Path::new(dir).join(entry.name().unwrap_or_default());
            if is_rules(&path) {
                rules.push(workdir.join(&path));
            }
            blobs.push((path, entry.id()));
        }
        git2::TreeWalkResult::Ok
    })?;
    rules.push(repo.path().join("info/attributes"));
    if let Ok(global) = repo
        .config()
        .and_then(|c| c.get_path("core.attributesFile"))
    {
        rules.push(global);
    }
    let text: String = rules
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect();
    // A macro could set any of them under another name, so one means every
    // file is asked about every converting attribute.
    let named: Vec<&'static str> = if text.contains("[attr]") {
        CONVERTING.to_vec()
    } else {
        CONVERTING
            .iter()
            .copied()
            .filter(|name| text.contains(name))
            .collect()
    };
    let mut files = HashMap::new();
    if !named.is_empty() {
        for (path, id) in blobs {
            if converts(repo, &path, &named) {
                if let Some(size) = blob_size(repo, id) {
                    files.insert(path, size);
                }
            }
        }
    }
    Ok(Known {
        rules: rules
            .into_iter()
            .map(|path| {
                let seen = stamp(&path);
                (path, seen)
            })
            .collect(),
        named,
        tree: head.id(),
        files,
    })
}

/// The converted files whose size on disk is not their blob's, as paths
/// from the root: what the snapshot hashes besides what status names.
/// `listed` is what status named, which tells of an attributes file not
/// yet committed.
pub(crate) fn changed(
    repo: &Repository,
    workdir: &Path,
    head: &Tree,
    listed: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let mut all = known().lock().unwrap_or_else(|e| e.into_inner());
    let current = all.get_mut(workdir).and_then(|known| {
        let unseen_rules = listed
            .iter()
            .any(|p| is_rules(p) && !known.rules.iter().any(|(r, _)| *r == workdir.join(p)));
        let rules_hold =
            !unseen_rules && known.rules.iter().all(|(path, seen)| stamp(path) == *seen);
        if !rules_hold {
            return None;
        }
        if known.tree == head.id() {
            return Some(());
        }
        // HEAD moved: only what the commits between touched is asked about.
        let old = repo.find_tree(known.tree).ok()?;
        let diff = repo.diff_tree_to_tree(Some(&old), Some(head), None).ok()?;
        for delta in diff.deltas() {
            let touched = [delta.old_file().path(), delta.new_file().path()];
            if touched.iter().flatten().any(|p| is_rules(p)) {
                return None;
            }
            if let Some(path) = delta.old_file().path() {
                known.files.remove(path);
            }
            let new = delta.new_file();
            if let (Some(path), true) = (new.path(), new.exists()) {
                if !known.named.is_empty() && converts(repo, path, &known.named) {
                    if let Some(size) = blob_size(repo, new.id()) {
                        known.files.insert(path.to_path_buf(), size);
                    }
                }
            }
        }
        known.tree = head.id();
        Some(())
    });
    if current.is_none() {
        all.insert(workdir.to_path_buf(), whole(repo, workdir, head)?);
    }
    let known = &all[workdir];
    Ok(known
        .files
        .iter()
        .filter(|(path, size)| {
            std::fs::symlink_metadata(workdir.join(path))
                .is_ok_and(|meta| meta.is_file() && meta.len() != **size)
        })
        .map(|(path, _)| path.clone())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
    }

    /// An `ident` file checked out expanded is changed by size; a file no
    /// attribute converts is not asked about; and a file a later commit
    /// adds under the same rule is found by the diff, not a new walk.
    #[test]
    fn an_expanded_ident_file_is_found_and_a_commit_adds_to_the_set() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        std::fs::write(root.join(".gitattributes"), "*.c ident\n").unwrap();
        std::fs::write(root.join("cdb.c"), "/* $Id$ */\n").unwrap();
        std::fs::write(root.join("plain.txt"), "nothing\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::remove_file(root.join("cdb.c")).unwrap();
        git(root, &["checkout", "--", "cdb.c"]);
        assert!(std::fs::read_to_string(root.join("cdb.c"))
            .unwrap()
            .contains("$Id: "));

        let repo = Repository::open(root).unwrap();
        fn head(repo: &Repository) -> Tree<'_> {
            repo.head().unwrap().peel_to_tree().unwrap()
        }
        let found = changed(&repo, root, &head(&repo), &[]).unwrap();
        assert_eq!(found, vec![PathBuf::from("cdb.c")]);

        std::fs::write(root.join("more.c"), "/* $Id$ */ int x;\n").unwrap();
        git(root, &["add", "more.c"]);
        git(root, &["commit", "-qm", "more"]);
        std::fs::remove_file(root.join("more.c")).unwrap();
        git(root, &["checkout", "--", "more.c"]);
        let mut found = changed(&repo, root, &head(&repo), &[]).unwrap();
        found.sort();
        assert_eq!(found, vec![PathBuf::from("cdb.c"), PathBuf::from("more.c")]);
    }

    /// A repository whose attributes name nothing that converts asks
    /// libgit2 about no file.
    #[test]
    fn attributes_that_convert_nothing_cost_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        std::fs::write(root.join(".gitattributes"), "*.c diff=cpp\n").unwrap();
        std::fs::write(root.join("a.c"), "int a;\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "init"]);
        let repo = Repository::open(root).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        assert!(changed(&repo, root, &tree, &[]).unwrap().is_empty());
        let all = known().lock().unwrap();
        assert!(all[root].named.is_empty());
    }
}

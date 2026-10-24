//! Rebasing the local issue queue onto the remote's.
//!
//! When a fetch finds that both this machine and the remote moved the
//! issues ref, the remote's history is taken as it is and every commit
//! only this machine has is replayed on top of it, oldest first, keeping
//! its message and author. The result is ahead of the remote and nothing
//! else, so the user's next push is an ordinary fast-forward.
//!
//! Git's own rebase would stop at the first conflict and ask; this one has
//! nobody to ask, and does not need to, because the queue is made of
//! independent files and the few places two machines can touch the same
//! one each have an answer:
//!
//! - **Unrelated histories are the common case**, not an error: a machine
//!   that filed an issue before it ever fetched started a queue of its
//!   own. With no common ancestor every local file is an addition, and
//!   issue ids are random, so the two queues simply sit side by side.
//! - **One issue edited on both sides** is merged field by field against
//!   the version this machine started from: whatever this machine changed
//!   wins, everything else is the remote's, and `updated` is the later of
//!   the two.
//! - **A comment or an attachment numbered the same on both sides** is
//!   renumbered past the remote's — they are numbered only to keep
//!   concurrent writers on disjoint paths — and the body's references to a
//!   renumbered attachment follow it.
//! - **A deletion wins.** An issue this machine deleted goes, with whatever
//!   the remote added to it since; this machine's changes to an issue the
//!   remote deleted are dropped and counted, since writing them would put
//!   back pieces of an issue without the issue.
//! - **The order file** keeps this machine's arrangement of the issues it
//!   knew, and places the issues it did not know where the remote put them.

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use git2::Oid;

use super::{issue_path_parts, parse_order, render_order, Issue, CAS_ATTEMPTS, ISSUES_ORDER_PATH};
use crate::GitWorkspace;

/// What a replay produced: the new tip, how many local commits it carried
/// over, and how many of their changes to an issue it had to drop (one per
/// commit and issue, however many of the issue's files it touched).
struct Replayed {
    tip: Oid,
    replayed: usize,
    dropped: usize,
}

impl GitWorkspace {
    /// Rebase [`super::ISSUES_REF`] onto `remote`, compare-and-swapping
    /// against the local tip the replay was built from. A write that lands
    /// on the ref meanwhile (an agent filing an issue) costs a retry, never
    /// that write.
    pub(super) fn rebase_issues(&self, remote: Oid) -> Result<(usize, usize)> {
        let mut last: Option<anyhow::Error> = None;
        for _ in 0..CAS_ATTEMPTS {
            // A fresh handle per attempt, for the reason
            // `issue_transaction` gives.
            let git = GitWorkspace::discover(&self.workdir)
                .context("this checkout is no longer a git repository")?;
            let local = git
                .read_ref(super::ISSUES_REF)?
                .context("the issues ref is gone")?;
            let done = git.replay_issues(local, remote)?;
            match git.swap_ref(
                super::ISSUES_REF,
                Some(local),
                done.tip,
                "issues: rebased onto the remote",
            ) {
                Ok(()) => return Ok((done.replayed, done.dropped)),
                Err(e) => last = Some(e),
            }
        }
        Err(last
            .unwrap_or_else(|| anyhow!("the issues ref could not be written"))
            .context("the issues ref kept moving under the rebase — nothing was changed"))
    }

    /// Replay the commits `local` has and `remote` does not onto `remote`,
    /// writing the new commits but moving no ref.
    fn replay_issues(&self, local: Oid, remote: Oid) -> Result<Replayed> {
        let repo = &self.repo;
        let committer = self.ref_signature()?;
        let mut walk = repo.revwalk()?;
        walk.push(local)?;
        walk.hide(remote)?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;
        let ours: Vec<Oid> = walk.collect::<Result<_, _>>()?;

        let mut tip = repo.find_commit(remote)?;
        let mut replay = Replay {
            repo,
            files: flatten(&tip.tree()?)?,
            renamed: BTreeMap::new(),
            pending: Vec::new(),
            dropped: 0,
        };
        let mut replayed = 0;
        for oid in ours {
            let commit = repo.find_commit(oid)?;
            let before = match commit.parent(0) {
                Ok(parent) => Some(parent.tree()?),
                Err(_) => None,
            };
            let after = commit.tree()?;
            replay.apply(before.as_ref(), &after)?;
            let tree = replay.write(&tip.tree()?)?;
            if tree.id() == tip.tree_id() {
                // Nothing left of it on this side, as `git rebase` drops a
                // commit that became empty.
                continue;
            }
            let message = commit.message().unwrap_or("issues: (replayed)");
            let id = repo.commit(None, &commit.author(), &committer, message, &tree, &[&tip])?;
            tip = repo.find_commit(id)?;
            replayed += 1;
        }
        Ok(Replayed {
            tip: tip.id(),
            replayed,
            dropped: replay.dropped,
        })
    }
}

/// The tree being built, as a path → blob map for the questions the rules
/// ask, and the changes since it was last written.
struct Replay<'r> {
    repo: &'r git2::Repository,
    files: BTreeMap<String, Oid>,
    /// A local comment's or attachment's path → where it was renumbered
    /// to, so a later local commit touching it follows it there.
    renamed: BTreeMap<String, String>,
    pending: Vec<(String, Option<Oid>)>,
    dropped: usize,
}

impl<'r> Replay<'r> {
    fn put(&mut self, path: &str, blob: Oid) {
        if self.files.get(path) != Some(&blob) {
            self.files.insert(path.to_string(), blob);
            self.pending.push((path.to_string(), Some(blob)));
        }
    }

    fn remove(&mut self, path: &str) {
        if self.files.remove(path).is_some() {
            self.pending.push((path.to_string(), None));
        }
    }

    /// One local commit's changes, from the tree it was made on (`None`
    /// for a root commit) to its own.
    fn apply(&mut self, before: Option<&git2::Tree<'_>>, after: &git2::Tree<'_>) -> Result<()> {
        let diff = self.repo.diff_tree_to_tree(before, Some(after), None)?;
        let mut by_issue: BTreeMap<String, Vec<(String, Option<Oid>)>> = BTreeMap::new();
        let mut order_changed = false;
        for delta in diff.deltas() {
            let file = match delta.status() {
                git2::Delta::Deleted => delta.old_file(),
                _ => delta.new_file(),
            };
            let Some(path) = file.path().and_then(|p| p.to_str()) else {
                continue;
            };
            let blob = (delta.status() != git2::Delta::Deleted).then(|| delta.new_file().id());
            if path == ISSUES_ORDER_PATH {
                order_changed = true;
            } else if let Some((id, _)) = issue_path_parts(path) {
                by_issue
                    .entry(id.to_string())
                    .or_default()
                    .push((path.to_string(), blob));
            } else {
                match blob {
                    Some(blob) => self.put(path, blob),
                    None => self.remove(path),
                }
            }
        }
        let base = |path: &str| blob_at(before, path);
        for (id, changes) in by_issue {
            self.apply_issue(&id, changes, &base)?;
        }
        if order_changed {
            let read = |blob: Option<Oid>| -> Result<Vec<String>> {
                Ok(match blob {
                    Some(blob) => parse_order(&String::from_utf8_lossy(
                        self.repo.find_blob(blob)?.content(),
                    )),
                    None => Vec::new(),
                })
            };
            let merged = merge_order(
                &read(base(ISSUES_ORDER_PATH))?,
                &read(blob_at(Some(after), ISSUES_ORDER_PATH))?,
                &read(self.files.get(ISSUES_ORDER_PATH).copied())?,
            );
            let blob = self.repo.blob(render_order(&merged).as_bytes())?;
            self.put(ISSUES_ORDER_PATH, blob);
        }
        Ok(())
    }

    fn apply_issue(
        &mut self,
        id: &str,
        changes: Vec<(String, Option<Oid>)>,
        base: &dyn Fn(&str) -> Option<Oid>,
    ) -> Result<()> {
        let issue_md = Issue::path(id);
        let prefix = format!("issues/{id}/");
        if changes
            .iter()
            .any(|(path, blob)| *path == issue_md && blob.is_none())
        {
            let gone: Vec<String> = self
                .files
                .range(prefix.clone()..)
                .take_while(|(path, _)| path.starts_with(&prefix))
                .map(|(path, _)| path.clone())
                .collect();
            for path in gone {
                self.remove(&path);
            }
            return Ok(());
        }
        if base(&issue_md).is_some() && !self.files.contains_key(&issue_md) {
            self.dropped += 1;
            return Ok(());
        }
        // Numbered files before the issue itself, so a renumbered
        // attachment's new name is known when the body is written.
        let (body, rest): (Vec<_>, Vec<_>) =
            changes.into_iter().partition(|(path, _)| *path == issue_md);
        for (path, blob) in rest {
            if let Some(moved) = self.renamed.get(&path).cloned() {
                match blob {
                    Some(blob) => self.put(&moved, blob),
                    None => self.remove(&moved),
                }
                continue;
            }
            let Some(blob) = blob else {
                self.remove(&path);
                continue;
            };
            let target = self.renumbered(&prefix, &path, blob, base);
            if target != path {
                self.renamed.insert(path, target.clone());
            }
            self.put(&target, blob);
        }
        for (_, blob) in body {
            let Some(ours) = blob else { continue };
            let merged = self.merge_issue(id, base(&issue_md), ours)?;
            let blob = self.repo.blob(merged.as_bytes())?;
            self.put(&issue_md, blob);
        }
        Ok(())
    }

    /// Where a local comment or attachment goes: its own path, unless that
    /// is a NEW file whose number the remote has already used for
    /// something else, in which case the next number free on the remote.
    fn renumbered(
        &self,
        prefix: &str,
        path: &str,
        blob: Oid,
        base: &dyn Fn(&str) -> Option<Oid>,
    ) -> String {
        let Some((dir, seq, name)) = numbered(prefix, path) else {
            return path.to_string();
        };
        if base(path).is_some() || self.files.get(path) == Some(&blob) {
            return path.to_string();
        }
        let dir_prefix = format!("{prefix}{dir}/");
        let taken: Vec<u32> = self
            .files
            .range(dir_prefix.clone()..)
            .take_while(|(other, _)| other.starts_with(&dir_prefix))
            .filter_map(|(other, _)| numbered(prefix, other).map(|(_, seq, _)| seq))
            .collect();
        if !taken.contains(&seq) {
            return path.to_string();
        }
        let next = taken.iter().max().copied().unwrap_or(0) + 1;
        match name {
            Some(name) => format!("{dir_prefix}{next:04}-{name}"),
            None => format!("{dir_prefix}{next:04}.md"),
        }
    }

    /// The issue's text: this machine's when the remote did not touch it
    /// since, otherwise the two merged field by field against `base` —
    /// with the body's references following any renumbered attachment.
    fn merge_issue(&self, id: &str, base: Option<Oid>, ours: Oid) -> Result<String> {
        let read = |blob: Oid| -> Result<String> {
            Ok(String::from_utf8_lossy(self.repo.find_blob(blob)?.content()).into_owned())
        };
        let theirs = self.files.get(&Issue::path(id)).copied();
        let mut text = read(ours)?;
        if let (Some(base), Some(theirs)) = (base, theirs) {
            if theirs != base && theirs != ours {
                let parsed = (
                    Issue::parse(id, &read(base)?),
                    Issue::parse(id, &text),
                    Issue::parse(id, &read(theirs)?),
                );
                if let (Ok(base), Ok(ours), Ok(theirs)) = parsed {
                    text = merge_fields(&base, ours, theirs).render();
                }
            }
        }
        let attachments = format!("issues/{id}/attachments/");
        for (from, to) in &self.renamed {
            if let (Some(from), Some(to)) = (
                from.strip_prefix(&attachments),
                to.strip_prefix(&attachments),
            ) {
                text = text.replace(
                    &format!("](attachments/{from})"),
                    &format!("](attachments/{to})"),
                );
            }
        }
        Ok(text)
    }

    /// Write the changes since the last call onto `parent`'s tree.
    fn write(&mut self, parent: &git2::Tree<'r>) -> Result<git2::Tree<'r>> {
        let mut builder = git2::build::TreeUpdateBuilder::new();
        for (path, blob) in self.pending.drain(..) {
            match blob {
                Some(blob) => builder.upsert(&path, blob, git2::FileMode::Blob),
                None => builder.remove(&path),
            };
        }
        let id = builder
            .create_updated(self.repo, parent)
            .context("building a replayed issues tree")?;
        Ok(self.repo.find_tree(id)?)
    }
}

/// Every blob in `tree`, by path.
fn flatten(tree: &git2::Tree<'_>) -> Result<BTreeMap<String, Oid>> {
    let mut files = BTreeMap::new();
    tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
        if entry.kind() == Some(git2::ObjectType::Blob) {
            if let Some(name) = entry.name() {
                files.insert(format!("{dir}{name}"), entry.id());
            }
        }
        git2::TreeWalkResult::Ok
    })?;
    Ok(files)
}

fn blob_at(tree: Option<&git2::Tree<'_>>, path: &str) -> Option<Oid> {
    tree?
        .get_path(std::path::Path::new(path))
        .ok()
        .map(|entry| entry.id())
}

/// `comments/0003.md` → `("comments", 3, None)`;
/// `attachments/0002-shot.png` → `("attachments", 2, Some("shot.png"))`.
fn numbered<'p>(prefix: &str, path: &'p str) -> Option<(&'p str, u32, Option<&'p str>)> {
    let rest = path.strip_prefix(prefix)?;
    if let Some(file) = rest.strip_prefix("comments/") {
        let seq = file.strip_suffix(".md")?.parse().ok()?;
        return Some(("comments", seq, None));
    }
    let file = rest.strip_prefix("attachments/")?;
    let (seq, name) = file.split_once('-')?;
    Some(("attachments", seq.parse().ok()?, Some(name)))
}

/// Each field as this machine left it if it changed it from `base`, and as
/// the remote has it otherwise.
fn merge_fields(base: &Issue, ours: Issue, theirs: Issue) -> Issue {
    macro_rules! pick {
        ($field:ident) => {
            if ours.$field != base.$field {
                ours.$field
            } else {
                theirs.$field
            }
        };
    }
    Issue {
        id: ours.id.clone(),
        updated: ours.updated.max(theirs.updated),
        title: pick!(title),
        resolution: pick!(resolution),
        reporter: pick!(reporter),
        started_by: pick!(started_by),
        agent: pick!(agent),
        model: pick!(model),
        created: pick!(created),
        labels: pick!(labels),
        links: pick!(links),
        body: pick!(body),
        comments: Vec::new(),
        attachments: Vec::new(),
    }
}

/// This machine's order (`ours`, changed from `base`) laid over the
/// remote's (`theirs`): every id this machine listed stays where it put
/// it, an id the remote added goes after the id it follows there, and an
/// id the remote dropped from a list this machine also had is dropped.
fn merge_order(base: &[String], ours: &[String], theirs: &[String]) -> Vec<String> {
    let mut out: Vec<String> = ours
        .iter()
        .filter(|id| theirs.contains(id) || !base.contains(id))
        .cloned()
        .collect();
    for (at, id) in theirs.iter().enumerate() {
        if base.contains(id) || out.contains(id) {
            continue;
        }
        let after = theirs[..at]
            .iter()
            .rev()
            .find_map(|prev| out.iter().position(|o| o == prev));
        out.insert(after.map_or(0, |p| p + 1), id.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_order_keeps_ours_and_places_theirs_where_they_put_it() {
        // Ours moved c to the top; the remote added x after a and dropped b.
        let merged = merge_order(
            &ids(&["a", "b", "c"]),
            &ids(&["c", "a", "b"]),
            &ids(&["a", "x", "c"]),
        );
        assert_eq!(merged, ids(&["c", "a", "x"]));
        // Unrelated: nothing in common, theirs goes in after ours' place
        // for its predecessor, or first.
        assert_eq!(
            merge_order(&[], &ids(&["l"]), &ids(&["r1", "r2"])),
            ids(&["r1", "r2", "l"])
        );
    }
}

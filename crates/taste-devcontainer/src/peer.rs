//! **The peer and its checkout, kept in step over ssh.**
//!
//! An environment whose checkout is in a VM has two repositories: the
//! checkout over there, and its **peer** on this host holding the refs
//! (docs/ENVIRONMENTS.md → "The topology: remote by default"). They are
//! related by git's own transport — push and fetch over the VM's ssh
//! forward — and never by a file-level sync: divergence is then an
//! ordinary branch relationship the review surfaces already draw.
//!
//! # The `git` CLI, on purpose
//!
//! libgit2 here is built without ssh (`Cargo.toml`), and stays so: the
//! user's own push already goes through the `git` CLI so that ssh, its
//! keys, and its prompts are the system's. This module runs the same CLI
//! with the **workspace's** identity and `known_hosts`
//! (`crate::keys`), never the user's — the VM is the project's, and so is
//! the trust. `receive-pack` and `upload-pack` run in the VM, which is the
//! untrusted side; this host runs only its own client.
//!
//! # No credential enters the VM
//!
//! The push carries refs. Nothing here ever runs `git push` FROM the VM to
//! anywhere: the user's remote is reached from the peer, on this host,
//! with the user's keys, which is the "no push, ever" the standard makes
//! (CLAUDE.md → the boundary is the host).

use std::path::Path;

use anyhow::{bail, Context, Result};
use taste_core::podman::host_argv;

use taste_core::files::Files;

use crate::keys::Keys;
use crate::provision::{Vm, GUEST_USER};

/// The refspecs a checkout's peer tracks: the branches, the tags, and the
/// IDE's own refs — snapshots among them.
pub const PEER_REFSPECS: [&str; 3] = [
    "+refs/heads/*:refs/heads/*",
    "+refs/tags/*:refs/tags/*",
    "+refs/taste/*:refs/taste/*",
];

/// The `ssh://` URL of a repository at `path` in `vm`.
pub fn guest_url(vm: &Vm, path: &Path) -> String {
    format!(
        "ssh://{GUEST_USER}@127.0.0.1:{}{}",
        vm.ssh_port,
        path.display()
    )
}

/// Run `git -C <peer> <args>` on this host with the workspace's ssh
/// identity, and nothing that could ask a question.
fn git(peer: &Path, keys: &Keys, args: &[String]) -> Result<String> {
    let mut argv = vec!["-C".to_string(), peer.display().to_string()];
    argv.extend(args.iter().cloned());
    let (program, argv) = host_argv(taste_core::podman::sandboxed(), "git", argv);
    let output = std::process::Command::new(&program)
        .args(&argv)
        .env("GIT_SSH_COMMAND", keys.git_ssh_command())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.first().map(String::as_str).unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Push the peer's refs into the checkout at `path` in `vm`.
pub fn push_to_guest(
    peer: &Path,
    vm: &Vm,
    keys: &Keys,
    path: &Path,
    refspecs: &[&str],
) -> Result<()> {
    let mut args = vec!["push".to_string(), "--quiet".into(), guest_url(vm, path)];
    args.extend(refspecs.iter().map(|s| s.to_string()));
    git(peer, keys, &args)
        .with_context(|| format!("pushing to {} in {}", path.display(), vm.domain))?;
    Ok(())
}

/// Fetch the checkout's refs at `path` in `vm` into the peer. The peer's
/// HEAD is parked on an unborn branch (`taste_git::strip_worktree`), so
/// every branch may be updated.
pub fn fetch_from_guest(
    peer: &Path,
    vm: &Vm,
    keys: &Keys,
    path: &Path,
    refspecs: &[&str],
) -> Result<()> {
    let mut args = vec![
        "fetch".to_string(),
        "--quiet".into(),
        "--update-head-ok".into(),
        guest_url(vm, path),
    ];
    args.extend(refspecs.iter().map(|s| s.to_string()));
    git(peer, keys, &args)
        .with_context(|| format!("fetching from {} in {}", path.display(), vm.domain))?;
    Ok(())
}

/// The namespace a peer keeps its checkout's branches under while they
/// are compared with its own: `refs/taste/vm/<branch>`.
pub const VM_BRANCH_NAMESPACE: &str = "refs/taste/vm/";

/// The refspecs that bring a checkout's state home to a peer that is the
/// user's own folder: branches into the comparison namespace (the folder
/// has branches of its own), everything else in place — except the
/// comparison namespace itself, which is the peer's and never the
/// checkout's. Without that exclusion a checkout seeded from a peer that
/// had already synced once carried `refs/taste/vm/*` of its own, and the
/// fetch refused to put both a branch and that ref on one name
/// (2026-09-21: "Cannot fetch both refs/heads/X and refs/taste/vm/X").
pub const PRIMARY_SYNC_REFSPECS: [&str; 5] = [
    "+refs/heads/*:refs/taste/vm/*",
    "+refs/tags/*:refs/tags/*",
    "+refs/taste/*:refs/taste/*",
    "^refs/taste/vm/*",
    "^refs/taste/peer/*",
];

/// Where the folder's branch lands in the checkout while the checkout
/// fast-forwards to it (`push_ahead_into_checkout`); deleted after.
const PEER_STAGING: &str = "refs/taste/peer";

/// The refspecs that seed a checkout in the VM from the user's folder:
/// branches, tags, the IDE's refs (never the peer's own comparison
/// namespace), and the remote-tracking refs the sync flow rebases onto
/// over there.
pub const PRIMARY_SEED_REFSPECS: [&str; 5] = [
    "+refs/heads/*:refs/heads/*",
    "+refs/tags/*:refs/tags/*",
    "+refs/taste/*:refs/taste/*",
    "^refs/taste/vm/*",
    "+refs/remotes/*:refs/remotes/*",
];

/// The peer's remote-tracking refs, for a checkout about to rebase onto
/// one: fetched on this host with the user's keys, then pushed over.
pub const REMOTES_REFSPEC: &str = "+refs/remotes/*:refs/remotes/*";

/// What syncing the primary's peer with its checkout did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerSync {
    pub branch: Option<String>,
    /// The folder's working tree was fast-forwarded to the checkout's
    /// branch.
    pub fast_forwarded: bool,
    /// The folder's branch was ahead of the checkout's and was pushed in.
    pub pushed: bool,
    /// Commits the folder has that the checkout does not, still.
    pub host_ahead: usize,
    /// Commits the checkout has that the folder's working tree does not,
    /// still — because the folder was dirty, or on another branch.
    pub host_behind: usize,
    /// Why the folder was left where it was, when it was.
    pub note: Option<String>,
}

/// Bring the user's folder — the primary's peer — up to date with the
/// checkout in the VM, and the checkout up to date with the folder.
///
/// The checkout is the working copy of record; the folder is a peer
/// (docs/ENVIRONMENTS.md → "The topology"). Its branches are fetched into
/// `refs/taste/vm/` and compared: a branch that is not checked out is
/// simply moved to what the checkout has; the checked-out one is
/// **fast-forwarded when the folder is clean** and left alone with a note
/// otherwise (David, 2026-09-20: fast-forward it when clean). A folder
/// whose branch is AHEAD — the user committed here with another tool — is
/// pushed into the checkout, which `receive.denyCurrentBranch=updateInstead`
/// lets land when the checkout's tree is clean. Two sides that both moved
/// are an ordinary divergence: named, and left for the person.
pub fn sync_primary_peer(
    peer: &Path,
    vm: &Vm,
    keys: &Keys,
    files: &Files,
    path: &Path,
) -> Result<PeerSync> {
    fetch_from_guest(peer, vm, keys, path, &PRIMARY_SYNC_REFSPECS)?;
    let git = taste_git::GitWorkspace::discover(peer)
        .with_context(|| format!("{} is not a git working tree", peer.display()))?;
    let mut sync = PeerSync {
        branch: git.branch_name(),
        ..PeerSync::default()
    };
    let clean = git.status().map(|s| s.is_empty()).unwrap_or(false);
    for (name, oid) in git.refs_under(VM_BRANCH_NAMESPACE)? {
        let branch = &name[VM_BRANCH_NAMESPACE.len()..];
        let local = format!("refs/heads/{branch}");
        let mine = git.read_ref(&local)?;
        if Some(branch) != sync.branch.as_deref() {
            // Not checked out here: the checkout's word is final.
            if mine != Some(oid) {
                git.set_ref(&local, oid)?;
            }
            continue;
        }
        if mine == Some(oid) {
            continue;
        }
        let (ahead, behind) = git.ahead_behind(&local, &name)?;
        match (ahead, behind) {
            (0, behind) if behind > 0 => {
                if clean {
                    git.fast_forward_branch(&name)
                        .with_context(|| format!("fast-forwarding {branch}"))?;
                    sync.fast_forwarded = true;
                } else {
                    sync.host_behind = behind;
                    sync.note = Some(format!(
                        "this folder has uncommitted changes, so it was not fast-forwarded to \
                         the {behind} commit(s) the checkout in the VM has"
                    ));
                }
            }
            (ahead, 0) if ahead > 0 => {
                match push_ahead_into_checkout(peer, vm, keys, files, path, branch) {
                    Ok(None) => sync.pushed = true,
                    // Taken, but its uncommitted work did not come along:
                    // said as what happened, not as a refusal.
                    Ok(Some(kept)) => {
                        sync.pushed = true;
                        sync.note = Some(format!(
                            "the checkout in the VM took this folder's {ahead} commit(s); {kept}"
                        ));
                    }
                    Err(e) => {
                        sync.host_ahead = ahead;
                        sync.note = Some(format!(
                            "this folder is {ahead} commit(s) ahead of the checkout in the VM, \
                             which would not take them: {e:#}"
                        ));
                    }
                }
            }
            (ahead, behind) => {
                sync.host_ahead = ahead;
                sync.host_behind = behind;
                sync.note = Some(format!(
                    "this folder and the checkout in the VM have diverged on {branch}: {ahead} \
                     commit(s) here, {behind} there"
                ));
            }
        }
    }
    Ok(sync)
}

/// Bring the checkout in the VM up to the folder's `branch`, which is
/// ahead of it: the user committed here with another tool.
///
/// A plain push into the checked-out branch is what
/// `receive.denyCurrentBranch=updateInstead` allows only when the
/// checkout's working tree is clean, and a checkout in use is seldom
/// clean — so the first launch after nine host-side commits refused with
/// "would not take them" (2026-09-21). Instead the branch is pushed to a
/// staging ref and the checkout fast-forwards to it ITSELF, its
/// uncommitted work stashed around the move and put back after, the way
/// `git pull --autostash` does. A stash that does not apply over the new
/// commits is kept — and the half-applied tree it would have left is put
/// back to the new commits, clean, so the checkout is not found strewn
/// with conflict markers nobody asked for (2026-09-21: a Conflicts row
/// and a stash both, from one sync); `Ok(Some(..))` says so, for the
/// note. A tree already carrying such leftovers from an earlier sync is
/// cleaned the same way first; a conflict of the user's own — unmerged
/// paths with no such stash, or a merge in progress — is refused by name,
/// with what to do. A branch that is not a fast-forward is left alone, an
/// error.
fn push_ahead_into_checkout(
    peer: &Path,
    vm: &Vm,
    keys: &Keys,
    files: &Files,
    path: &Path,
    branch: &str,
) -> Result<Option<String>> {
    let staging = format!("{PEER_STAGING}/{branch}");
    push_to_guest(
        peer,
        vm,
        keys,
        path,
        &[&format!("+refs/heads/{branch}:{staging}")],
    )?;
    let script = format!(
        r#"set -e
staging='{staging}'
branch='{branch}'
cleanup() {{ git update-ref -d "$staging" 2>/dev/null || true; }}
if ! git merge-base --is-ancestor "refs/heads/$branch" "$staging"; then
  cleanup; echo "the checkout's $branch is not behind the folder's" >&2; exit 2
fi
if [ "$(git symbolic-ref --short -q HEAD)" != "$branch" ]; then
  git update-ref "refs/heads/$branch" "$staging"; cleanup; exit 0
fi
if git ls-files -u | grep -q .; then
  if git stash list | grep -q taste-ide-sync && ! git rev-parse -q --verify MERGE_HEAD >/dev/null 2>&1; then
    # The leftovers of a taste-ide-sync stash that did not apply, from
    # before the sync learned to leave the tree clean: the stash holds the
    # changes, so the tree goes back to its commit.
    git reset -q --hard
  else
    cleanup
    echo "the checkout has unresolved conflicts in $(git ls-files -u | awk '{{print $4}}' | sort -u | tr '\n' ' ')- resolve or discard them in the file tree, and the folder's commits follow" >&2
    exit 5
  fi
fi
stashed=0
if [ -n "$(git status --porcelain)" ]; then
  git stash push --include-untracked -q -m taste-ide-sync && stashed=1
fi
if ! git merge --ff-only -q "$staging"; then
  [ "$stashed" = 1 ] && git stash pop -q || true
  cleanup; echo "the checkout would not fast-forward to the folder's $branch" >&2; exit 3
fi
cleanup
if [ "$stashed" = 1 ] && ! git stash pop -q 2>/dev/null; then
  git reset -q --hard
  git clean -fdq
  echo "its uncommitted changes do not apply over them and are kept in its stash (taste-ide-sync), for the file tree's Unstash" >&2
  exit 4
fi
"#
    );
    let out = files
        .exec(path, &["sh".into(), "-c".into(), script])
        .with_context(|| format!("fast-forwarding {branch} in VM {}", vm.domain))?;
    match out.status {
        0 => Ok(None),
        4 => Ok(Some(out.stderr_utf8().trim().to_string())),
        _ => bail!("{}", out.stderr_utf8().trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guest_url_names_core_the_forward_and_the_absolute_path() {
        let vm = Vm {
            domain: "taste-a".into(),
            ssh_port: 40022,
            workspace_root: "/work/proj".into(),
            state: crate::provision::DomainState::Running,
        };
        assert_eq!(
            guest_url(&vm, Path::new("/var/home/core/taste/799f/i-0001")),
            "ssh://core@127.0.0.1:40022/var/home/core/taste/799f/i-0001"
        );
    }

    /// The primary's refspecs keep the comparison namespace on the peer's
    /// side only: it is neither seeded into the checkout nor fetched back
    /// as an IDE ref, or a branch and its mirror would land on one name.
    #[test]
    fn the_comparison_namespace_never_crosses() {
        assert!(PRIMARY_SEED_REFSPECS.contains(&"^refs/taste/vm/*"));
        assert!(PRIMARY_SYNC_REFSPECS.contains(&"^refs/taste/vm/*"));
        assert!(PRIMARY_SYNC_REFSPECS.contains(&"+refs/heads/*:refs/taste/vm/*"));
    }

    /// The refspecs are forced and cover branches, tags, and the IDE's own
    /// refs — which is where the snapshots are.
    #[test]
    fn the_peer_tracks_branches_tags_and_the_ides_refs() {
        assert!(PEER_REFSPECS.iter().all(|r| r.starts_with('+')));
        assert!(PEER_REFSPECS.iter().any(|r| r.contains("refs/taste/*")));
        assert!(PEER_REFSPECS.iter().any(|r| r.contains("refs/heads/*")));
    }
}

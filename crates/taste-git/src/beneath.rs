//! Writing paths a VM chose into a folder on this host.
//!
//! The folder the user opened is on the host side of the boundary
//! (docs/ENVIRONMENTS.md → "Isolation"), and every path the mirror, a first
//! commit's adoption, or an ignored-file copy writes there was named by a
//! tree or a listing the checkout in the VM produced. A tree is data, and
//! data does not get to choose where on this host it is written. So every
//! such write comes through here, and three things hold for each:
//!
//! - **The path stays in the folder**: every component is a plain name,
//!   never `..`, a root, or a repository's metadata directory in any of
//!   the spellings git's own `verify_path` refuses.
//! - **No link carries it out.** A directory on the way that is a symlink
//!   is refused, whoever put it there — the mirror writes the checkout's
//!   links into the folder with whatever target they have, so a link
//!   written in one pass is the next pass's way out unless every write
//!   looks.
//! - **Nothing opened for writing follows a link.** The file is written
//!   beside its place and renamed over it, and that staging name is
//!   created fresh (`O_CREAT|O_EXCL`, which never follows a link) after
//!   whatever held it is removed. Written with a plain `std::fs::write`, a
//!   link the checkout planted at `.name.taste-mirror` sent the bytes of
//!   `name` wherever it pointed — `~/.bashrc`, an `authorized_keys`, a user
//!   unit — which is a process on this machine run from a VM's files.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The suffix of the name a file is staged under before it is renamed
/// into place. A path that ends in it is refused rather than written: it
/// is this module's own scratch, and a tree that carries one is either a
/// stray from a pass that died or an attempt on the staging name.
pub const STAGING_SUFFIX: &str = ".taste-mirror";

/// Whether a path component names a repository's metadata directory, the
/// way git's `verify_path` sees it: case, and the trailing dots, spaces,
/// and 8.3 name some filesystems fold into `.git`, too.
pub(crate) fn is_git_dir_name(name: &str) -> bool {
    let folded = name.trim_end_matches(['.', ' ']).to_ascii_lowercase();
    folded == ".git" || folded == "git~1"
}

/// `rel` inside `root`, refused when it would reach outside it, into
/// `.git`, onto a staging name, or through a directory that is a link.
///
/// `.git` in ANY component: a tree carrying `sub/.git/config` would plant
/// a repository's config in the folder, and git on this host reads it —
/// and runs what `core.fsmonitor` or an alias names — in that directory.
pub fn checked(root: &Path, rel: &Path) -> Result<PathBuf> {
    let ok = rel.components().next().is_some()
        && rel.components().all(|c| match c {
            Component::Normal(name) => !is_git_dir_name(&name.to_string_lossy()),
            _ => false,
        });
    if !ok {
        bail!(
            "refusing to write {} outside the working tree",
            rel.display()
        );
    }
    if rel
        .file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(STAGING_SUFFIX))
    {
        bail!(
            "refusing to write {}: the name is the mirror's own staging name",
            rel.display()
        );
    }
    let mut at = root.to_path_buf();
    for part in rel.parent().into_iter().flat_map(Path::components) {
        at.push(part);
        if std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!(
                "refusing to write {} through the link {}",
                rel.display(),
                at.display()
            );
        }
    }
    Ok(root.join(rel))
}

/// Whether any part of `rel` that exists under `root`, its last
/// component included, is a link: the test a path a VM named has to pass
/// before this host treats what is there as its own — a submodule's
/// clone, which the sync fetches from and mirrors into. A part that does
/// not exist ends the walk, since nothing below it can be a link yet.
pub fn passes_a_link(root: &Path, rel: &Path) -> bool {
    let mut at = root.to_path_buf();
    for part in rel.components() {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

/// Make `rel`'s missing parent directories one at a time, each checked to
/// be a real directory once it exists — so a link that appears on the way
/// is refused rather than walked through, as `create_dir_all` would.
fn make_parents(root: &Path, rel: &Path) -> Result<()> {
    let mut at = root.to_path_buf();
    for part in rel.parent().into_iter().flat_map(Path::components) {
        at.push(part);
        match std::fs::create_dir(&at) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("making {}", at.display())),
        }
        let meta =
            std::fs::symlink_metadata(&at).with_context(|| format!("making {}", at.display()))?;
        if !meta.file_type().is_dir() {
            bail!(
                "refusing to write {}: {} is not a directory",
                rel.display(),
                at.display()
            );
        }
    }
    Ok(())
}

/// Clear a link or a file at `absolute`, so what is made there next is
/// made fresh. A directory is left, and the write after fails on it.
fn clear(absolute: &Path) -> Result<()> {
    match std::fs::symlink_metadata(absolute) {
        Ok(meta) if !meta.file_type().is_dir() => std::fs::remove_file(absolute)
            .with_context(|| format!("removing {}", absolute.display())),
        _ => Ok(()),
    }
}

/// Write `content` at `rel` inside `root`, its executable bits set or
/// cleared by `executable` and its other bits kept from the file it
/// replaces (or the umask's, for a new one).
///
/// Written beside and renamed over, so a file is never seen half-written
/// — by the user's editor, a build, or a close that stops the pass
/// partway.
pub fn write_file(root: &Path, rel: &Path, content: &[u8], executable: bool) -> Result<()> {
    let absolute = checked(root, rel)?;
    make_parents(root, rel)?;
    // The file it replaces, for its bits — only a real file's, never a
    // link's target.
    let kept = std::fs::symlink_metadata(&absolute)
        .ok()
        .filter(|m| m.file_type().is_file())
        .map(|m| m.permissions().mode());
    let name = absolute.file_name().unwrap_or_default().to_string_lossy();
    let staging = absolute.with_file_name(format!(".{name}{STAGING_SUFFIX}"));
    clear(&staging)?;
    let written = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)?;
        file.write_all(content)?;
        let bits = kept.unwrap_or(file.metadata()?.permissions().mode());
        let bits = if executable {
            bits | 0o111
        } else {
            bits & !0o111
        };
        // Through the descriptor: the path could be anything by now.
        file.set_permissions(std::fs::Permissions::from_mode(bits & 0o7777 & !0o7000))?;
        Ok(())
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&staging);
        return Err(e).with_context(|| format!("writing {}", absolute.display()));
    }
    if let Err(e) = std::fs::rename(&staging, &absolute) {
        let _ = std::fs::remove_file(&staging);
        return Err(e).with_context(|| format!("writing {}", absolute.display()));
    }
    Ok(())
}

/// Make `rel` inside `root` a link to `target`, replacing a file or link
/// there. The target is the checkout's to choose, and is never followed
/// here; what keeps it from being followed later is that every write
/// looks for links on its way ([`checked`]).
pub fn write_link(root: &Path, rel: &Path, target: &std::ffi::OsStr) -> Result<()> {
    let absolute = checked(root, rel)?;
    make_parents(root, rel)?;
    clear(&absolute)?;
    std::os::unix::fs::symlink(target, &absolute)
        .with_context(|| format!("linking {}", absolute.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_path_outside_the_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for bad in [
            "../escape",
            "/etc/passwd",
            ".git/config",
            "sub/.git/config",
            "sub/.GIT/config",
            "sub/.git./config",
            "sub/git~1/config",
            ".foo.taste-mirror",
            "",
        ] {
            assert!(checked(root, Path::new(bad)).is_err(), "{bad:?}");
        }
        assert!(checked(root, Path::new("ok/file")).is_ok());
        assert!(checked(root, Path::new("ok/.gitignore")).is_ok());
    }

    /// The staging name, planted as a link to a file outside the folder,
    /// must not carry the write there: `.name.taste-mirror` sorts before
    /// `name`, so a tree can carry both and have the link written first.
    #[test]
    fn a_link_at_the_staging_name_is_not_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("bashrc");
        fs::write(&victim, "the user's own\n").unwrap();
        let root = dir.path();
        std::os::unix::fs::symlink(&victim, root.join(".notes.taste-mirror")).unwrap();
        write_file(root, Path::new("notes"), b"from the VM\n", false).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "the user's own\n");
        assert_eq!(
            fs::read_to_string(root.join("notes")).unwrap(),
            "from the VM\n"
        );
        assert!(fs::symlink_metadata(root.join("notes"))
            .unwrap()
            .file_type()
            .is_file());
        assert!(fs::symlink_metadata(root.join(".notes.taste-mirror")).is_err());
    }

    #[test]
    fn a_link_on_the_way_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::os::unix::fs::symlink(outside.path(), root.join("evil")).unwrap();
        assert!(write_file(root, Path::new("evil/authorized_keys"), b"key", false).is_err());
        assert!(write_link(root, Path::new("evil/x"), "y".as_ref()).is_err());
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[test]
    fn a_link_at_the_place_itself_is_replaced_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("target");
        fs::write(&victim, "kept\n").unwrap();
        let root = dir.path();
        std::os::unix::fs::symlink(&victim, root.join("name")).unwrap();
        write_file(root, Path::new("name"), b"new\n", true).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "kept\n");
        let meta = fs::symlink_metadata(root.join("name")).unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!(meta.permissions().mode() & 0o111, 0o111);
    }

    #[test]
    fn executable_bits_follow_the_tree_and_nothing_else_is_granted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("f"), "x").unwrap();
        fs::set_permissions(root.join("f"), fs::Permissions::from_mode(0o4755)).unwrap();
        write_file(root, Path::new("f"), b"y", false).unwrap();
        let mode = fs::metadata(root.join("f")).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0, "executable bits cleared");
        assert_eq!(mode & 0o7000, 0, "no setuid, setgid, or sticky bit");
        write_file(root, Path::new("deep/new/file"), b"z", true).unwrap();
        assert_eq!(
            fs::metadata(root.join("deep/new/file"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0o111
        );
    }

    #[test]
    fn a_path_through_a_link_is_seen_whichever_part_is_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("libs/real")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("libs/linked")).unwrap();
        std::os::unix::fs::symlink(root.join("libs"), root.join("alias")).unwrap();
        assert!(!passes_a_link(root, Path::new("libs/real")));
        assert!(!passes_a_link(root, Path::new("libs/missing/deeper")));
        assert!(passes_a_link(root, Path::new("libs/linked")));
        assert!(passes_a_link(root, Path::new("alias/real")));
    }
}

//! **Files dropped on the tree**: a row of its own moved, or copied with
//! Ctrl held, into the folder it was dropped on; and files from outside —
//! a file manager, a browser's download — copied in (David, 2026-10-26:
//! "implement dropping in from outside and Ctrl-drag to copy").
//!
//! A drag out of the tree carries [`TreeDrag`] beside the file list it
//! offers other applications (`file_drag`), and a drop that finds it is the
//! tree's own: a move or a copy inside the checkout, through the files
//! service, with nothing fetched out of a VM. A drop that finds only a file
//! list came from outside and is imported. To other applications the tree
//! offers a copy and nothing else, so a file manager can never move a file
//! out of the user's folder by a drag.
//!
//! The rules a drop keeps, here so they are tested apart from GTK: a move
//! never takes a folder into itself, a copy never replaces anything (it
//! takes a free name, "name (copy).ext"), and an import skips symbolic
//! links, so a drop cannot pull in whatever a link points at.

use std::path::{Path, PathBuf};

use gtk::glib;
use taste_core::files::Files;

/// The mark a drag out of the tree carries: the drop is the tree's own,
/// and what it moves is what the tree recorded when the drag began.
#[derive(Clone, Debug, Default, glib::Boxed)]
#[boxed_type(name = "TasteTreeDrag")]
pub struct TreeDrag;

/// The most one file brought in from outside may be: it is held in memory
/// on its way into the checkout (`Files::write`).
pub const MOST_FILE: u64 = 1 << 30;
/// The most one drop brings in.
pub const MOST_DROP: u64 = 4 << 30;

/// Whether moving or copying `from` into `dir` would put a folder inside
/// itself.
pub fn into_itself(from: &Path, dir: &Path) -> bool {
    dir.starts_with(from)
}

/// The name a copy of `name` takes in a folder where `taken` says which
/// names are already used: the name itself when it is free, then
/// "name (copy).ext", "name (copy 2).ext", and on.
pub fn free_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    let path = Path::new(name);
    let (stem, extension) = match (path.file_stem(), path.extension()) {
        (Some(stem), Some(extension)) => (
            stem.to_string_lossy().into_owned(),
            format!(".{}", extension.to_string_lossy()),
        ),
        _ => (name.to_string(), String::new()),
    };
    (1..)
        .map(|n| match n {
            1 => format!("{stem} (copy){extension}"),
            n => format!("{stem} (copy {n}){extension}"),
        })
        .find(|candidate| !taken(candidate))
        .expect("an unbounded count finds a free name")
}

/// What a copy or an import did, for the line that says so.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Copied {
    pub files: usize,
    pub skipped_links: usize,
}

impl Copied {
    /// "Copied 3 files" and what was passed over, said once.
    pub fn summary(&self, into: &str) -> String {
        let files = match self.files {
            1 => "1 file".to_string(),
            n => format!("{n} files"),
        };
        match self.skipped_links {
            0 => format!("Copied {files} into {into}"),
            1 => format!("Copied {files} into {into}; a symbolic link was skipped"),
            n => format!("Copied {files} into {into}; {n} symbolic links were skipped"),
        }
    }
}

/// Copy `from` to `to` inside the checkout, beside the files, as `cp -a`
/// copies: modes kept, a link kept a link, a folder whole. Blocking.
pub fn copy_within(files: &Files, from: &Path, to: &Path) -> std::io::Result<()> {
    let cwd = to.parent().unwrap_or(to);
    let output = files.exec(
        cwd,
        &[
            "cp".into(),
            "-a".into(),
            "--no-target-directory".into(),
            "--".into(),
            from.display().to_string(),
            to.display().to_string(),
        ],
    )?;
    if output.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(
            output.stderr_utf8().trim().to_string(),
        ))
    }
}

/// Copy `source`, a file or a folder on this host that the user dropped,
/// into the checkout at `to`: its files written through the files
/// service, an executable one made executable again there, its symbolic
/// links skipped. Blocking.
pub fn import(files: &Files, source: &Path, to: &Path) -> std::io::Result<Copied> {
    let mut copied = Copied::default();
    let mut budget = MOST_DROP;
    import_into(files, source, to, &mut copied, &mut budget)?;
    Ok(copied)
}

fn import_into(
    files: &Files,
    source: &Path,
    to: &Path,
    copied: &mut Copied,
    budget: &mut u64,
) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(source)?;
    if meta.file_type().is_symlink() {
        copied.skipped_links += 1;
        return Ok(());
    }
    if meta.is_dir() {
        files.mkdir_all(to)?;
        let mut entries: Vec<_> = std::fs::read_dir(source)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            import_into(
                files,
                &entry.path(),
                &to.join(entry.file_name()),
                copied,
                budget,
            )?;
        }
        return Ok(());
    }
    if !meta.is_file() {
        return Ok(());
    }
    if meta.len() > MOST_FILE {
        return Err(std::io::Error::other(format!(
            "{} is larger than a drop brings in ({} MiB)",
            source.display(),
            MOST_FILE >> 20
        )));
    }
    *budget = budget.checked_sub(meta.len()).ok_or_else(|| {
        std::io::Error::other(format!(
            "the drop is larger than one brings in ({} GiB)",
            MOST_DROP >> 30
        ))
    })?;
    let bytes = std::fs::read(source)?;
    files.write(to, &bytes)?;
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o111 != 0 {
        let cwd = to.parent().unwrap_or(to);
        let output = files.exec(
            cwd,
            &[
                "chmod".into(),
                "+x".into(),
                "--".into(),
                to.display().to_string(),
            ],
        )?;
        if !output.success() {
            return Err(std::io::Error::other(
                output.stderr_utf8().trim().to_string(),
            ));
        }
    }
    copied.files += 1;
    Ok(())
}

/// The name a folder is called by in a line about it: its own, or the
/// project's for the checkout's root.
pub fn folder_name(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string())
}

/// Every path the moved `from` stood at, mapped to where it stands now: a
/// file to its new path, and anything under a moved folder to its place
/// under the new one.
pub fn moved_to(path: &Path, from: &Path, to: &Path) -> Option<PathBuf> {
    path.strip_prefix(from).ok().map(|rest| {
        if rest.as_os_str().is_empty() {
            to.to_path_buf()
        } else {
            to.join(rest)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_is_never_moved_into_itself() {
        assert!(into_itself(Path::new("/c/src"), Path::new("/c/src")));
        assert!(into_itself(Path::new("/c/src"), Path::new("/c/src/app")));
        assert!(!into_itself(Path::new("/c/src"), Path::new("/c/srcs")));
        assert!(!into_itself(Path::new("/c/src/app"), Path::new("/c/src")));
    }

    #[test]
    fn a_copy_takes_the_first_free_name() {
        let taken = |names: &'static [&'static str]| move |n: &str| names.contains(&n);
        assert_eq!(free_name("a.rs", taken(&[])), "a.rs");
        assert_eq!(free_name("a.rs", taken(&["a.rs"])), "a (copy).rs");
        assert_eq!(
            free_name("a.rs", taken(&["a.rs", "a (copy).rs"])),
            "a (copy 2).rs"
        );
        assert_eq!(free_name("docs", taken(&["docs"])), "docs (copy)");
        assert_eq!(free_name(".env", taken(&[".env"])), ".env (copy)");
    }

    #[test]
    fn what_moved_with_a_folder_moves_under_its_new_place() {
        let (from, to) = (Path::new("/c/a"), Path::new("/c/b/a"));
        assert_eq!(moved_to(Path::new("/c/a"), from, to).as_deref(), Some(to));
        assert_eq!(
            moved_to(Path::new("/c/a/x/y.rs"), from, to).as_deref(),
            Some(Path::new("/c/b/a/x/y.rs"))
        );
        assert_eq!(moved_to(Path::new("/c/ab.rs"), from, to), None);
    }

    /// An import from this host: a folder's files written, an executable
    /// one executable again, a symbolic link skipped, and a copy within
    /// the checkout keeping what `cp -a` keeps.
    #[test]
    fn an_import_writes_files_and_skips_links_and_a_copy_keeps_modes() {
        use std::os::unix::fs::PermissionsExt;
        let outside = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let dropped = outside.path().join("kit");
        std::fs::create_dir_all(dropped.join("bin")).unwrap();
        std::fs::write(dropped.join("README"), "read me").unwrap();
        std::fs::write(dropped.join("bin/run"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            dropped.join("bin/run"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dropped.join("passwd")).unwrap();
        let files = Files::Local;
        let copied = import(&files, &dropped, &checkout.path().join("kit")).unwrap();
        assert_eq!(
            copied,
            Copied {
                files: 2,
                skipped_links: 1
            }
        );
        let run = checkout.path().join("kit/bin/run");
        assert_eq!(std::fs::read_to_string(&run).unwrap(), "#!/bin/sh\n");
        assert_ne!(
            std::fs::metadata(&run).unwrap().permissions().mode() & 0o111,
            0
        );
        assert!(!checkout.path().join("kit/passwd").exists());

        let copy = checkout.path().join("kit (copy)");
        copy_within(&files, &checkout.path().join("kit"), &copy).unwrap();
        assert_ne!(
            std::fs::metadata(copy.join("bin/run"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        assert_eq!(
            copied.summary("src"),
            "Copied 2 files into src; a symbolic link was skipped"
        );
    }
}

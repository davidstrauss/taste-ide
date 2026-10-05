//! A submodule's change, as its parent's status reports it: the submodule
//! is at a commit the parent does not record. Its row in the Dirty list
//! has no text of its own to diff, so what opens is the change itself —
//! the commits between the one the parent records and the one the
//! submodule is at, and each file that differs between the two — read
//! from the submodule's own repository (David, 2026-10-05: "I should be
//! able to open the diff, even if it has to compute against the
//! submodule's .git instead of parent").
//!
//! Two git calls through the files service, wherever the checkout is: one
//! for the two commits, the log between them, and the files that differ;
//! one `git cat-file --batch` for every text file's two sides. The edits
//! the submodule's working tree holds on top are its own files' rows in
//! the Dirty list, and are not repeated here.

use std::path::{Path, PathBuf};

use taste_core::files::Files;

use crate::chatdoc::{Document, Edit};

/// The files drawn at most; the rest are named as not drawn.
const MAX_FILES: usize = 100;

/// A side larger than this is named rather than drawn.
const MAX_BYTES: usize = 1 << 20;

/// Run in the submodule, with its path as `$1`: the commit the parent
/// records for it (its index, else its HEAD), the commit it is at, the
/// commits the submodule has that the parent's does not and the other
/// way, and the files that differ, each record NUL-terminated.
const META_SCRIPT: &str = r#"set -e
top=$(git rev-parse --show-superproject-working-tree)
[ -n "$top" ] || { echo "not a submodule" >&2; exit 3; }
rel=${1#"$top"/}
want=$(git -C "$top" ls-files -s -- "$rel" | awk '$1=="160000"{print $2; exit}')
[ -n "$want" ] || want=$(git -C "$top" rev-parse -q --verify "HEAD:$rel" || true)
have=$(git rev-parse HEAD)
printf '%s\0%s\0' "$want" "$have"
[ -n "$want" ] || exit 0
git log --format='%h %s' --max-count=200 "$want..$have" || true
printf '\0'
git log --format='%h %s' --max-count=200 "$have..$want" || true
printf '\0'
git diff --numstat --no-renames -z "$want" "$have" || true
"#;

/// Run in the submodule: `$1` and `$2` the two commits, then the paths;
/// each path's two sides, in that order, as `git cat-file --batch` prints
/// them.
const CONTENT_SCRIPT: &str = r#"want="$1"; have="$2"; shift 2
for f in "$@"; do printf '%s:%s\n%s:%s\n' "$want" "$f" "$have" "$f"; done | git cat-file --batch
"#;

/// What the first call found.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Meta {
    want: String,
    have: String,
    ahead: Vec<String>,
    behind: Vec<String>,
    /// Each file that differs, and whether git called it binary.
    files: Vec<(String, bool)>,
}

fn parse_meta(out: &[u8]) -> Option<Meta> {
    let text = String::from_utf8_lossy(out);
    let mut fields = text.split('\0');
    let want = fields.next()?.trim().to_string();
    let have = fields.next()?.trim().to_string();
    let lines = |field: Option<&str>| -> Vec<String> {
        field
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    };
    let ahead = lines(fields.next());
    let behind = lines(fields.next());
    // `--numstat -z`: "added\tdeleted\tpath", each NUL-terminated; a
    // binary file counts "-" for both.
    let files = fields
        .filter(|record| !record.trim().is_empty())
        .filter_map(|record| {
            let mut parts = record.trim_start_matches('\n').splitn(3, '\t');
            let added = parts.next()?;
            let _deleted = parts.next()?;
            let path = parts.next()?;
            Some((path.to_string(), added == "-"))
        })
        .collect();
    Some(Meta {
        want,
        have,
        ahead,
        behind,
        files,
    })
}

/// `git cat-file --batch`'s answer, `count` objects of it: each one's
/// bytes, or `None` where it is missing — a file one side does not have.
fn parse_batch(out: &[u8], count: usize) -> Vec<Option<Vec<u8>>> {
    let mut objects = Vec::with_capacity(count);
    let mut at = 0;
    while objects.len() < count && at < out.len() {
        let Some(end) = out[at..].iter().position(|&b| b == b'\n') else {
            break;
        };
        let header = String::from_utf8_lossy(&out[at..at + end]).to_string();
        at += end + 1;
        if header.ends_with(" missing") || header.ends_with(" ambiguous") {
            objects.push(None);
            continue;
        }
        let size = header
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse::<usize>().ok());
        let Some(size) = size.filter(|size| at + size <= out.len()) else {
            break;
        };
        objects.push(Some(out[at..at + size].to_vec()));
        // The object, then the newline the batch format ends it with.
        at += size + 1;
    }
    objects.resize(count, None);
    objects
}

fn short(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

/// The change at the submodule `path`, as a document to open — with the
/// two commits it is between, for the tab's key — or why there is none.
pub fn load(files: &Files, path: &Path) -> Result<(Document, String, String), String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let meta = files
        .exec(
            path,
            &[
                "sh".into(),
                "-c".into(),
                META_SCRIPT.into(),
                "taste-submodule".into(),
                path.display().to_string(),
            ],
        )
        .map_err(|e| e.to_string())?;
    if !meta.success() {
        return Err(meta.stderr_utf8().trim().to_string());
    }
    let meta = parse_meta(&meta.stdout).ok_or("git said nothing about it")?;
    if meta.want.is_empty() {
        return Err(format!("its parent records no commit for {name}"));
    }
    let mut summary = format!(
        "{name} is at {}; its parent records {}.",
        short(&meta.have),
        short(&meta.want)
    );
    let commits = |n: usize| if n == 1 { "commit" } else { "commits" };
    if !meta.ahead.is_empty() {
        summary.push_str(&format!(
            "\n\n{} {} since:\n{}",
            meta.ahead.len(),
            commits(meta.ahead.len()),
            meta.ahead.join("\n")
        ));
    }
    if !meta.behind.is_empty() {
        summary.push_str(&format!(
            "\n\n{} {} of the recorded one it does not have:\n{}",
            meta.behind.len(),
            commits(meta.behind.len()),
            meta.behind.join("\n")
        ));
    }
    let mut skipped: Vec<String> = Vec::new();
    let mut text_files: Vec<String> = Vec::new();
    for (file, binary) in &meta.files {
        if *binary {
            skipped.push(format!("{file} (binary)"));
        } else if text_files.len() >= MAX_FILES {
            skipped.push(file.clone());
        } else {
            text_files.push(file.clone());
        }
    }
    let mut edits = Vec::new();
    if !text_files.is_empty() {
        let mut argv: Vec<String> = vec![
            "sh".into(),
            "-c".into(),
            CONTENT_SCRIPT.into(),
            "taste-submodule".into(),
            meta.want.clone(),
            meta.have.clone(),
        ];
        argv.extend(text_files.iter().cloned());
        let out = files.exec(path, &argv).map_err(|e| e.to_string())?;
        let objects = parse_batch(&out.stdout, text_files.len() * 2);
        for (index, file) in text_files.iter().enumerate() {
            let side = |object: &Option<Vec<u8>>| -> Option<String> {
                match object {
                    None => Some(String::new()),
                    Some(bytes) if bytes.len() > MAX_BYTES => None,
                    Some(bytes) => String::from_utf8(bytes.clone()).ok(),
                }
            };
            match (side(&objects[index * 2]), side(&objects[index * 2 + 1])) {
                (Some(old), Some(new)) => edits.push(Edit {
                    path: PathBuf::from(&name).join(file),
                    old,
                    new,
                }),
                _ => skipped.push(format!("{file} (too large, or not text)")),
            }
        }
    }
    if meta.files.is_empty() {
        summary.push_str("\n\nNo file differs between the two.");
    }
    Ok((
        Document::Changes {
            title: format!("{name} {}…{}", short(&meta.want), short(&meta.have)),
            summary,
            edits,
            skipped,
        },
        meta.want,
        meta.have,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_call_reads_back() {
        let out = b"aaaaaaaa\0bbbbbbbb\0bbbbbbb two\nccccccc one\n\0\0\n3\t1\tlib.typ\x001\t0\tdocs/a b.md\x00-\t-\tlogo.png\x00";
        let meta = parse_meta(out).unwrap();
        assert_eq!(meta.want, "aaaaaaaa");
        assert_eq!(meta.have, "bbbbbbbb");
        assert_eq!(meta.ahead, ["bbbbbbb two", "ccccccc one"]);
        assert!(meta.behind.is_empty());
        assert_eq!(
            meta.files,
            [
                ("lib.typ".to_string(), false),
                ("docs/a b.md".to_string(), false),
                ("logo.png".to_string(), true)
            ]
        );
    }

    #[test]
    fn the_batch_reads_back_with_its_missing_objects() {
        let out = b"1111 blob 4\nold\n\n2222 blob 3\nnew\naaaa:gone.txt missing\n3333 blob 0\n\n";
        let objects = parse_batch(out, 3);
        assert_eq!(objects[0].as_deref(), Some(&b"old\n"[..]));
        assert_eq!(objects[1].as_deref(), Some(&b"new"[..]));
        assert_eq!(objects[2], None);
        let objects = parse_batch(out, 4);
        assert_eq!(objects[3].as_deref(), Some(&b""[..]));
    }

    /// The whole of it against a real parent and submodule, on this
    /// machine's git.
    #[test]
    fn a_submodule_ahead_of_its_parent_opens_as_its_change() {
        let dir = std::env::temp_dir().join(format!("taste-subdiff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(cwd)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        git(&sub, &["init", "-q", "-b", "main"]);
        std::fs::write(sub.join("lib.typ"), "one\n").unwrap();
        git(&sub, &["add", "-A"]);
        git(&sub, &["commit", "-q", "-m", "first"]);
        let parent = dir.join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        git(&parent, &["init", "-q", "-b", "main"]);
        git(
            &parent,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub.to_str().unwrap(),
                "tpl",
            ],
        );
        git(&parent, &["commit", "-q", "-m", "add"]);
        let tpl = parent.join("tpl");
        std::fs::write(tpl.join("lib.typ"), "one\ntwo\n").unwrap();
        std::fs::write(tpl.join("new.typ"), "new\n").unwrap();
        git(&tpl, &["add", "-A"]);
        git(&tpl, &["commit", "-q", "-m", "second"]);

        let (doc, want, have) = load(&Files::Local, &tpl).unwrap();
        assert_ne!(want, have);
        match doc {
            Document::Changes { summary, edits, .. } => {
                assert!(summary.contains("1 commit since"), "{summary}");
                assert!(summary.contains("second"), "{summary}");
                let lib = edits.iter().find(|e| e.path.ends_with("lib.typ")).unwrap();
                assert_eq!(
                    (lib.old.as_str(), lib.new.as_str()),
                    ("one\n", "one\ntwo\n")
                );
                let new = edits.iter().find(|e| e.path.ends_with("new.typ")).unwrap();
                assert_eq!((new.old.as_str(), new.new.as_str()), ("", "new\n"));
            }
            other => panic!("not a change: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

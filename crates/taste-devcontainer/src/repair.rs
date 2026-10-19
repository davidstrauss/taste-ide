//! **A damaged repository repaired from its other copy.**
//!
//! Every checkout in a VM has a second copy of its history: its peer, the
//! folder the user opened (`crate::peer`). So an object one of them can no
//! longer read is almost always one the other still has. On 2026-10-19 a
//! page of Personal's pack went bad in the VM's memory — the bytes on its
//! disk were sound — and every sync after it failed in `git fetch` with
//! "inflate: data stream error", raw, five times over in the title bar's
//! history, until it was repaired by hand: the damaged objects copied in
//! from the folder, the pack written again (David: "But why did it occur,
//! and how can we prevent these issues?"). This is that repair, done by
//! the sync itself when git says an object is damaged:
//!
//! 1. `git fsck` each copy, and name the objects and packs it cannot read.
//! 2. Move a damaged pack aside, and unpack what is still readable in it
//!    (`git unpack-objects -r`): git will not read past a damaged object
//!    in a pack to a sound copy elsewhere, so the pack has to go.
//! 3. Copy each object still missing from the copy that has it, checked
//!    against its id by `git hash-object` on the way in — the id is the
//!    object's hash, so what lands is exactly what was lost or nothing.
//! 4. Repack, check again, and drop the damaged pack.
//!
//! An object neither copy can read is said, and left: that one is lost,
//! and the person is told which, rather than the sync failing on it
//! forever in git's words.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};
use taste_core::files::Files;

/// How many times a repair copies what is named and packs again.
const REPAIR_ROUNDS: usize = 32;

/// Whether git's error — a fetch's, a push's, a snapshot's — says a
/// repository's objects are damaged, rather than anything about the
/// network, the disk's room, or the refs.
pub fn damaged(error: &str) -> bool {
    const SIGNS: [&str; 12] = [
        "inflate:",
        "unpack-objects failed",
        "index-pack failed",
        "pack checksum mismatch",
        "CRC mismatch",
        "is corrupt",
        "object corrupt",
        "corrupt loose object",
        "bad object",
        "did not receive expected object",
        "unable to read",
        "cannot unpack",
    ];
    SIGNS.iter().any(|sign| error.contains(sign))
}

/// The ids of the objects `git fsck`'s output names as damaged or missing.
pub fn findings(fsck: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for line in fsck.lines() {
        let lower = line.to_ascii_lowercase();
        let about_damage = [
            "error",
            "missing",
            "corrupt",
            "broken link",
            "cannot",
            "could not",
        ]
        .iter()
        .any(|word| lower.contains(word));
        if !about_damage {
            continue;
        }
        for word in line.split(|c: char| !c.is_ascii_hexdigit()) {
            if matches!(word.len(), 40 | 64) {
                found.insert(word.to_ascii_lowercase());
            }
        }
    }
    found
}

/// What a repair came to.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Repair {
    /// Objects Personal's checkout had damaged, copied in from the folder.
    pub checkout: Vec<String>,
    /// Objects the folder had damaged, copied in from the checkout.
    pub folder: Vec<String>,
    /// Damaged in one copy and not readable in the other: lost.
    pub lost: Vec<String>,
}

impl Repair {
    pub fn repaired_any(&self) -> bool {
        !self.checkout.is_empty() || !self.folder.is_empty()
    }

    /// The repair in a sentence, for the title bar's history.
    pub fn words(&self) -> String {
        let objects = |n: usize| {
            if n == 1 {
                "1 damaged object".to_string()
            } else {
                format!("{n} damaged objects")
            }
        };
        let mut parts = Vec::new();
        if !self.checkout.is_empty() {
            parts.push(format!(
                "Repaired {} in Personal's repository from your folder",
                objects(self.checkout.len())
            ));
        }
        if !self.folder.is_empty() {
            parts.push(format!(
                "Repaired {} in your folder's repository from Personal",
                objects(self.folder.len())
            ));
        }
        if !self.lost.is_empty() {
            parts.push(format!(
                "{} could not be read in either copy and {} lost: {}",
                objects(self.lost.len()),
                if self.lost.len() == 1 { "is" } else { "are" },
                short_ids(&self.lost)
            ));
        }
        parts.join(". ")
    }
}

fn short_ids(ids: &[String]) -> String {
    let shown: Vec<&str> = ids
        .iter()
        .take(3)
        .map(|id| &id[..id.len().min(10)])
        .collect();
    let more = ids.len().saturating_sub(shown.len());
    if more > 0 {
        format!("{} and {more} more", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

/// One copy of the history, as the repair reads and writes it.
trait Replica {
    /// `git` with `args` in it: whether it succeeded, its stdout, and its
    /// stderr.
    fn git(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<(bool, Vec<u8>, String)>;

    /// `script` in a shell in it: whether it succeeded, and what it said.
    fn sh(&self, script: &str) -> Result<(bool, String)>;
}

/// The folder, on this host.
struct Folder<'a>(&'a Path);

impl Replica for Folder<'_> {
    fn git(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<(bool, Vec<u8>, String)> {
        let mut child = std::process::Command::new("git")
            .arg("-C")
            .arg(self.0)
            .args(args)
            .stdin(if stdin.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("running git")?;
        if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(bytes).context("writing to git")?;
        }
        let output = child.wait_with_output().context("waiting for git")?;
        Ok((
            output.status.success(),
            output.stdout,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }

    fn sh(&self, script: &str) -> Result<(bool, String)> {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(self.0)
            .stdin(std::process::Stdio::null())
            .output()
            .context("running sh")?;
        let mut said = String::from_utf8_lossy(&output.stdout).into_owned();
        said.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok((output.status.success(), said))
    }
}

/// Personal's checkout, in its VM, through the files service.
struct Checkout<'a> {
    files: &'a Files,
    path: &'a Path,
}

impl Replica for Checkout<'_> {
    fn git(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<(bool, Vec<u8>, String)> {
        // The files service runs a command with no stdin: what would be
        // piped in is written to a file of the repository's own first.
        let staged = match stdin {
            Some(bytes) => {
                let at = self.path.join(".git").join("taste-repair-object");
                self.files
                    .write(&at, bytes)
                    .context("writing the object into Personal's repository")?;
                Some(at)
            }
            None => None,
        };
        let mut argv: Vec<String> = std::iter::once("git".to_string())
            .chain(args.iter().map(|a| a.to_string()))
            .collect();
        if let Some(at) = &staged {
            // `hash-object --stdin` becomes `hash-object <file>`.
            argv.retain(|a| a != "--stdin");
            argv.push(at.display().to_string());
        }
        let output = self
            .files
            .exec(self.path, &argv)
            .context("running git in Personal's checkout")?;
        if let Some(at) = &staged {
            let _ = self.files.remove(at, false);
        }
        Ok((
            output.success(),
            output.stdout.clone(),
            output.stderr_utf8(),
        ))
    }

    fn sh(&self, script: &str) -> Result<(bool, String)> {
        let argv = ["sh".to_string(), "-c".to_string(), script.to_string()];
        let output = self
            .files
            .exec(self.path, &argv)
            .context("running sh in Personal's checkout")?;
        let mut said = String::from_utf8_lossy(&output.stdout).into_owned();
        said.push_str(&output.stderr_utf8());
        Ok((output.success(), said))
    }
}

/// What `copy`'s own `git fsck` names: the objects it cannot read or
/// finds missing, and the packs it finds damaged.
fn damaged_in(copy: &dyn Replica) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let (_, stdout, stderr) = copy.git(&["fsck", "--no-dangling", "--no-progress"], None)?;
    let mut said = String::from_utf8_lossy(&stdout).into_owned();
    said.push('\n');
    said.push_str(&stderr);
    Ok((findings(&said), damaged_packs(&said)))
}

/// The packs git's output names as damaged: the ones a checksum or an
/// object in them failed.
pub fn damaged_packs(said: &str) -> BTreeSet<String> {
    let mut packs = BTreeSet::new();
    for line in said.lines() {
        let lower = line.to_ascii_lowercase();
        if !["mismatch", "corrupt", "cannot unpack", "inflate"]
            .iter()
            .any(|word| lower.contains(word))
        {
            continue;
        }
        for word in line.split(|c: char| c.is_whitespace() || c == '(' || c == ')') {
            if word.ends_with(".pack") && word.contains("objects/pack/pack-") {
                packs.insert(word.to_string());
            }
        }
    }
    packs
}

/// Where a repair keeps the damaged packs it moved aside, until it is done.
const QUARANTINE: &str = "taste-damaged";

/// Move `packs` out of `copy`'s object store, and unpack what is still
/// readable in them back into it.
fn quarantine(copy: &dyn Replica, packs: &BTreeSet<String>) -> Result<()> {
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    let list: Vec<String> = packs.iter().map(|p| quote(p)).collect();
    let script = format!(
        r#"set -e
dir=$(git rev-parse --git-dir)
q="$dir/{QUARANTINE}"
mkdir -p "$q"
for pack in {packs}; do
  base="${{pack%.pack}}"
  for ext in pack idx rev bitmap; do
    if [ -e "$base.$ext" ]; then mv -f "$base.$ext" "$q/"; fi
  done
  name=$(basename "$base")
  git unpack-objects -r -q < "$q/$name.pack" || true
done
rm -f "$dir/objects/pack/multi-pack-index""#,
        packs = list.join(" ")
    );
    let (moved, said) = copy.sh(&script)?;
    if !moved {
        bail!("moving the damaged packs aside: {}", said.trim());
    }
    Ok(())
}

/// The damaged packs moved aside, removed once the repair is done.
fn drop_quarantine(copy: &dyn Replica) {
    let _ = copy.sh(&format!(
        r#"rm -rf "$(git rev-parse --git-dir)/{QUARANTINE}""#
    ));
}

/// Copy each of `ids` from `from` into `into`; the ones copied, and the
/// ones `from` could not supply.
fn copy_objects(
    ids: &BTreeSet<String>,
    from: &dyn Replica,
    into: &dyn Replica,
) -> Result<(Vec<String>, Vec<String>)> {
    let (mut copied, mut lost) = (Vec::new(), Vec::new());
    for id in ids {
        let (known, kind, _) = from.git(&["cat-file", "-t", id], None)?;
        let kind = String::from_utf8_lossy(&kind).trim().to_string();
        if !known || kind.is_empty() {
            lost.push(id.clone());
            continue;
        }
        let (read, bytes, _) = from.git(&["cat-file", &kind, id], None)?;
        if !read {
            lost.push(id.clone());
            continue;
        }
        let (wrote, landed, why) =
            into.git(&["hash-object", "-t", &kind, "-w", "--stdin"], Some(&bytes))?;
        let landed = String::from_utf8_lossy(&landed).trim().to_ascii_lowercase();
        if !wrote || landed != *id {
            bail!("writing object {id} back: {}", why.trim());
        }
        copied.push(id.clone());
    }
    Ok((copied, lost))
}

/// Write `copy`'s packs again, once nothing in them is damaged.
fn repack(copy: &dyn Replica) -> Result<()> {
    let (packed, _, why) = copy.git(&["repack", "-a", "-d", "-q"], None)?;
    if !packed {
        bail!("repacking: {}", why.trim());
    }
    // The commit graph names commits by where they were; written again
    // from what is there now.
    let _ = copy.git(&["commit-graph", "write", "--reachable"], None);
    Ok(())
}

/// Find the objects either copy cannot read and repair each from the other:
/// Personal's checkout at `path` (through `files`) and the folder `peer`.
pub fn repair_pair(peer: &Path, files: &Files, path: &Path) -> Result<Repair> {
    let folder = Folder(peer);
    let checkout = Checkout { files, path };
    let mut repair = Repair::default();
    let mut lost: BTreeSet<String> = BTreeSet::new();
    for (into, from, record) in [
        (&checkout as &dyn Replica, &folder as &dyn Replica, true),
        (&folder as &dyn Replica, &checkout as &dyn Replica, false),
    ] {
        let (mut damaged, mut packs) = damaged_in(into)?;
        if damaged.is_empty() && packs.is_empty() {
            continue;
        }
        let mut copied: Vec<String> = Vec::new();
        // Each round: a damaged pack named goes aside, with what is still
        // readable in it unpacked; what is named missing or unreadable is
        // copied in. fsck does not always name every damaged object at
        // once, so it is asked again until it names nothing new.
        for _round in 0..REPAIR_ROUNDS {
            if !packs.is_empty() {
                quarantine(into, &packs)?;
            } else {
                damaged.retain(|id| !copied.contains(id) && !lost.contains(id));
                if damaged.is_empty() {
                    break;
                }
                let (more, missing) = copy_objects(&damaged, from, into)?;
                let progress = !more.is_empty();
                lost.extend(missing);
                copied.extend(more);
                if !progress {
                    break;
                }
            }
            (damaged, packs) = damaged_in(into)?;
        }
        repack(into)?;
        drop_quarantine(into);
        // What is still unreadable now is lost, whatever the other copy
        // said.
        let (still, _) = damaged_in(into)?;
        lost.extend(still);
        if record {
            repair.checkout = copied;
        } else {
            repair.folder = copied;
        }
    }
    repair.lost = lost.into_iter().collect();
    Ok(repair)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What git said on 2026-10-19 is damage; a refused connection is not.
    #[test]
    fn damage_is_told_from_other_failures() {
        assert!(damaged(
            "git fetch: remote: Total 95 (delta 43)\nerror: inflate: data stream error \
             (incorrect data check)\nfatal: unpack-objects failed"
        ));
        assert!(!damaged(
            "ssh: connect to host 127.0.0.1 port 33053: Connection refused"
        ));
        assert!(!damaged(
            "fetching needs 2.0 GiB and this disk has 1.0 GiB free"
        ));
    }

    /// The ids fsck names on its error lines, and not the ones it merely
    /// mentions.
    #[test]
    fn fsck_findings_are_the_damaged_ids() {
        let fsck = "\
error: .git/objects/pack/pack-32659e545be0388bf7a0ec86f775ebda7fa11fb6.pack pack checksum mismatch
error: index CRC mismatch for object f3e3ca6ebb1fcd63a4af7ee7906fe2f729fc8227 from pack
error: Could not read ac92a86f9edf357d9bee7912b98bb812a5686a44
dangling blob 1111111111111111111111111111111111111111
missing tree 2222222222222222222222222222222222222222";
        let found = findings(fsck);
        assert!(found.contains("f3e3ca6ebb1fcd63a4af7ee7906fe2f729fc8227"));
        assert!(found.contains("ac92a86f9edf357d9bee7912b98bb812a5686a44"));
        assert!(found.contains("2222222222222222222222222222222222222222"));
        assert!(!found.contains("1111111111111111111111111111111111111111"));
        let packs = damaged_packs(
            "error: .git/objects/pack/pack-32659e545be0388bf7a0ec86f775ebda7fa11fb6.pack pack \
             checksum mismatch\nfatal: packed object 82bd3d4bc655e0195df3e24dd9419f7f1784c73a \
             (stored in .git/objects/pack/pack-c0dcc2e45cdb830487c4eb5bac24c1710b582621.pack) \
             is corrupt\nnothing about .git/objects/pack/pack-feed.pack here",
        );
        assert_eq!(
            packs.into_iter().collect::<Vec<_>>(),
            vec![
                ".git/objects/pack/pack-32659e545be0388bf7a0ec86f775ebda7fa11fb6.pack".to_string(),
                ".git/objects/pack/pack-c0dcc2e45cdb830487c4eb5bac24c1710b582621.pack".to_string(),
            ]
        );
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository with a damaged object, repaired from a clone that has
    /// it: the object is readable again, fsck is clean, and the repair says
    /// which side it mended.
    #[test]
    fn a_damaged_object_is_copied_back_from_the_other_copy() {
        let base = tempfile::tempdir().unwrap();
        let folder = base.path().join("folder");
        let checkout = base.path().join("checkout");
        std::fs::create_dir_all(&folder).unwrap();
        git(&folder, &["init", "-q"]);
        git(&folder, &["config", "user.email", "t@example.org"]);
        git(&folder, &["config", "user.name", "T"]);
        std::fs::write(
            folder.join("a.txt"),
            "the file that will be damaged\n".repeat(50),
        )
        .unwrap();
        git(&folder, &["add", "a.txt"]);
        git(&folder, &["commit", "-q", "-m", "one"]);
        let blob = git(&folder, &["rev-parse", "HEAD:a.txt"]);
        git(
            base.path(),
            &[
                "clone",
                "-q",
                "--no-local",
                folder.to_str().unwrap(),
                "checkout",
            ],
        );
        // Damage the checkout's copy: every pack's bytes past its header
        // garbled, as a bad page would read.
        let packs = checkout.join(".git/objects/pack");
        for entry in std::fs::read_dir(&packs).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "pack") {
                let mut bytes = std::fs::read(&path).unwrap();
                for byte in bytes.iter_mut().skip(40).take(64) {
                    *byte ^= 0x5a;
                }
                let mut perms = std::fs::metadata(&path).unwrap().permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                std::fs::set_permissions(&path, perms).unwrap();
                std::fs::write(&path, bytes).unwrap();
            }
        }
        let (ids, packs) = damaged_in(&Folder(&checkout)).unwrap();
        assert!(!ids.is_empty() || !packs.is_empty());
        let repair = repair_pair(&folder, &Files::Local, &checkout).unwrap();
        assert!(repair.repaired_any(), "{repair:?}");
        assert!(repair.lost.is_empty(), "{repair:?}");
        let (ids, packs) = damaged_in(&Folder(&checkout)).unwrap();
        assert!(ids.is_empty() && packs.is_empty(), "{ids:?} {packs:?}");
        assert_eq!(git(&checkout, &["cat-file", "-t", &blob]), "blob");
        assert!(repair.words().contains("Repaired"), "{}", repair.words());
    }
}

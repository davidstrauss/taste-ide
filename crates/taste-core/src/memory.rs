//! The project's memory: notes an agent keeps for itself across sessions,
//! kept by the IDE on this machine.
//!
//! An agent's own memory lived in its home — the environment's volume, in
//! the VM — so it was one environment's, went with the VM, and was out of
//! the user's reach (David, 2026-10-05: "The agent in the VM writes to
//! memory that seems to only be in the VM. Can we direct that to an MCP
//! command that the IDE processes as a file namespaced to the project but
//! stored locally?"). Here it is the project's: one directory beside the
//! workspace's other state, every environment of the project reading and
//! writing the same notes through the IDE's `memory_*` tools, and the index
//! of them handed to each agent as its session starts.
//!
//! One note to a file, `<name>.md`: a header with its name and a one-line
//! description — the index is made of those — and the note below it.

use std::path::{Path, PathBuf};

/// A note as the index shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteEntry {
    pub name: String,
    pub description: String,
}

/// The index handed to an agent at most this long; the rest is a count,
/// and `memory_list` has it all.
const INDEX_MAX_BYTES: usize = 8 * 1024;

/// The project's notes.
#[derive(Debug, Clone)]
pub struct Memory {
    dir: PathBuf,
}

impl Memory {
    /// The notes of the workspace at `root`, in its state directory.
    pub fn for_workspace(root: &Path) -> Self {
        Self::at(crate::state::workspace_state_dir(root).join("memory"))
    }

    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A name a note may have: lowercase words joined by hyphens, which is
    /// also its file's name, so nothing an agent passes can name a path.
    pub fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !name.starts_with('-')
            && !name.ends_with('-')
    }

    fn file(&self, name: &str) -> Result<PathBuf, String> {
        if !Self::valid_name(name) {
            return Err(format!(
                "{name:?} is not a note name: use lowercase words joined by hyphens, \
                 e.g. typst-build-steps"
            ));
        }
        Ok(self.dir.join(format!("{name}.md")))
    }

    /// Every note, by name.
    pub fn list(&self) -> Vec<NoteEntry> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut notes: Vec<NoteEntry> = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.strip_suffix(".md")?.to_string();
                if !Self::valid_name(&name) {
                    return None;
                }
                let text = std::fs::read_to_string(entry.path()).ok()?;
                let (description, _) = parse(&text);
                Some(NoteEntry { name, description })
            })
            .collect();
        notes.sort_by(|a, b| a.name.cmp(&b.name));
        notes
    }

    /// One note: its description and its text.
    pub fn read(&self, name: &str) -> Result<(String, String), String> {
        let file = self.file(name)?;
        let text = std::fs::read_to_string(&file).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                format!("no note named {name}; memory_list has the names")
            }
            _ => format!("reading {name}: {e}"),
        })?;
        Ok(parse(&text))
    }

    /// Write a note, replacing one of the same name. True when it is new.
    pub fn save(&self, name: &str, description: &str, body: &str) -> Result<bool, String> {
        let file = self.file(name)?;
        let description = description.split_whitespace().collect::<Vec<_>>().join(" ");
        if description.is_empty() {
            return Err("a note needs a description: one line saying what it is about".into());
        }
        if body.trim().is_empty() {
            return Err("a note needs a body: what to remember".into());
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("making the memory: {e}"))?;
        let fresh = !file.exists();
        let text = format!(
            "---\nname: {name}\ndescription: {description}\n---\n\n{}\n",
            body.trim_end()
        );
        let part = file.with_extension("md.part");
        std::fs::write(&part, text).map_err(|e| format!("writing {name}: {e}"))?;
        std::fs::rename(&part, &file).map_err(|e| format!("writing {name}: {e}"))?;
        Ok(fresh)
    }

    /// Remove a note. False when there was none.
    pub fn delete(&self, name: &str) -> Result<bool, String> {
        let file = self.file(name)?;
        match std::fs::remove_file(&file) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(format!("removing {name}: {e}")),
        }
    }

    /// The index as an agent is handed it: one line a note, `name — what
    /// it is about`, up to [`INDEX_MAX_BYTES`]. Empty when there are none.
    pub fn index(&self) -> String {
        let notes = self.list();
        let mut out = String::new();
        for (shown, note) in notes.iter().enumerate() {
            let line = format!("- {} — {}\n", note.name, note.description);
            if out.len() + line.len() > INDEX_MAX_BYTES {
                out.push_str(&format!(
                    "- …and {} more; memory_list has them all\n",
                    notes.len() - shown
                ));
                break;
            }
            out.push_str(&line);
        }
        out
    }
}

/// A note file's description and body. A file without the header is all
/// body, described by its first line.
fn parse(text: &str) -> (String, String) {
    if let Some(rest) = text.strip_prefix("---\n") {
        if let Some((head, body)) = rest.split_once("\n---\n") {
            let description = head
                .lines()
                .find_map(|line| line.strip_prefix("description:"))
                .map(|d| d.trim().to_string())
                .unwrap_or_default();
            return (description, body.trim().to_string());
        }
    }
    let description = text.lines().next().unwrap_or_default().trim().to_string();
    (description, text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> (tempfile::TempDir, Memory) {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::at(dir.path().join("memory"));
        (dir, memory)
    }

    #[test]
    fn a_note_saved_is_listed_read_replaced_and_removed() {
        let (_dir, memory) = memory();
        assert!(memory.list().is_empty());
        assert_eq!(memory.index(), "");
        assert!(memory
            .save("typst-build", "How the slides build", "Run `task build`.")
            .unwrap());
        assert_eq!(
            memory.list(),
            [NoteEntry {
                name: "typst-build".into(),
                description: "How the slides build".into()
            }]
        );
        assert_eq!(
            memory.read("typst-build").unwrap(),
            ("How the slides build".into(), "Run `task build`.".into())
        );
        assert!(!memory
            .save("typst-build", "How the slides\n build", "Run `task pdf`.")
            .unwrap());
        assert_eq!(memory.read("typst-build").unwrap().1, "Run `task pdf`.");
        assert_eq!(memory.index(), "- typst-build — How the slides build\n");
        assert!(memory.delete("typst-build").unwrap());
        assert!(!memory.delete("typst-build").unwrap());
    }

    #[test]
    fn a_name_is_never_a_path() {
        let (_dir, memory) = memory();
        for name in ["../x", "a/b", "", "Upper", "-x", "x.md"] {
            assert!(memory.save(name, "d", "b").is_err(), "{name}");
        }
        assert!(memory.read("nope").unwrap_err().contains("memory_list"));
    }
}

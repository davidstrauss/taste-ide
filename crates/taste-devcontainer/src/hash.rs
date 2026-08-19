//! Content hashing for devcontainer-config drift detection.
//!
//! The hash covers the config file, every file it references (currently the
//! Containerfile), AND the mounts the IDE adds on its own account. The
//! supervisor records the hash the running container was created from; a
//! re-hash mismatch raises the persistent "pending changes" state in the UI
//! and over MCP.
//!
//! Including the IDE mounts is what makes an IDE upgrade self-correcting.
//! Without them, changing what the supervisor mounts leaves every existing
//! container silently stale — same config, same hash, no banner, no drift
//! flag — until someone happens to reload. Hashing the computed list rather
//! than a version constant means it cannot be forgotten: change the mounts
//! and the hash moves by itself.

use std::path::Path;

use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::DevcontainerConfig;

/// Hash the configuration's defining inputs. Missing referenced files hash
/// as absent (rather than erroring) so a half-edited config still produces a
/// stable, comparable value.
pub fn config_hash(config: &DevcontainerConfig, ide_mounts: &[String]) -> Result<String> {
    let mut hasher = Sha256::new();
    for path in config.hash_inputs() {
        hash_file(&mut hasher, &path);
    }
    // Domain-separated from the file bytes above, so a Containerfile whose
    // contents happen to look like a mount spec cannot collide with one.
    hasher.update([2u8]);
    for mount in ide_mounts {
        hasher.update((mount.len() as u64).to_le_bytes());
        hasher.update(mount.as_bytes());
    }
    Ok(hex(&hasher.finalize()))
}

fn hash_file(hasher: &mut Sha256, path: &Path) {
    hasher.update(path.to_string_lossy().as_bytes());
    match std::fs::read(path) {
        Ok(bytes) => {
            hasher.update([1u8]);
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
        }
        Err(_) => hasher.update([0u8]),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(dir: &Path, containerfile: &str) -> DevcontainerConfig {
        let dc = dir.join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();
        std::fs::write(
            dc.join("devcontainer.json"),
            r#"{"build": {"dockerfile": "Containerfile"}}"#,
        )
        .unwrap();
        std::fs::write(dc.join("Containerfile"), containerfile).unwrap();
        DevcontainerConfig::discover(dir).unwrap().unwrap()
    }

    #[test]
    fn hash_changes_when_referenced_containerfile_changes() {
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(dir.path(), "FROM a\n");
        let h1 = config_hash(&config, &[]).unwrap();

        std::fs::write(dir.path().join(".devcontainer/Containerfile"), "FROM b\n").unwrap();
        let h2 = config_hash(&config, &[]).unwrap();
        assert_ne!(h1, h2);

        std::fs::write(dir.path().join(".devcontainer/Containerfile"), "FROM a\n").unwrap();
        assert_eq!(h1, config_hash(&config, &[]).unwrap());
    }

    /// An IDE upgrade that changes what gets mounted must invalidate
    /// containers built by the old one. Otherwise the config is identical,
    /// the hash matches, and a stale container runs on with no drift flag
    /// and nothing on screen to suggest a reload — which is exactly how the
    /// MCP socket mount sat broken until someone looked.
    #[test]
    fn hash_changes_when_the_ide_mount_set_changes() {
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(dir.path(), "FROM a\n");
        let before = config_hash(&config, &["-v".into(), "/w:/w:Z".into()]).unwrap();
        let after = config_hash(
            &config,
            &[
                "-v".into(),
                "/w:/w:Z".into(),
                "-v".into(),
                "/sock:/sock:z".into(),
            ],
        )
        .unwrap();
        assert_ne!(before, after, "a new mount must count as drift");

        // And a changed FLAG on an existing mount, which is the subtler
        // case: same paths, different labelling, different behaviour.
        let relabelled = config_hash(&config, &["-v".into(), "/w:/w:z".into()]).unwrap();
        assert_ne!(before, relabelled);
    }
}

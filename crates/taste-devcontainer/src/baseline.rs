//! The baseline environment: the IDE's own container definition.
//!
//! Safe mode used to mean "no container". It does not any more. When an
//! environment's *project* config is absent, unbuilt, or broken, the
//! environment runs this instead — the same topology as container mode
//! (container up, agent relocated inside it, channel, shells), differing in
//! exactly one thing: **who wrote the config**. The project's
//! `.devcontainer/` is one authority; this is the other, and it is the IDE's.
//!
//! Why that is worth a whole module rather than an `if` in the supervisor:
//!
//! - **`NoConfig` stops being a dead state.** A repo with no devcontainer
//!   used to be a workspace where nothing could run. Now it is a workspace
//!   with a modest container, which is what makes "one environment is always
//!   usable" true rather than aspirational.
//! - **"No exec in safe mode" was derived from absence**, not chosen. The
//!   only target would have been the host, and the host is the line this
//!   whole design defends. A baseline container is not the host, so the
//!   principle is untouched while the repair loop gains real tools — an
//!   agent fixing a broken build can now *run* things to find out why.
//! - **The write wall does not move.** The baseline mounts the environment's
//!   checkout **read-only**, and IDE-mediated writes stay bounded by
//!   `taste_core::policy::write_allowed`'s safe-mode scope. Reads go native,
//!   which is the one mode where a read-only bind was always the right
//!   answer: the agent needs to read the repo to repair its config, and it
//!   needs to write nothing but the config.
//!
//! The definition is **bundled, not fetched**: the bytes are compiled into
//! the IDE binary and written out at first need. The rung that exists to
//! always work cannot depend on a registry being reachable, and the base
//! image is pinned by digest so it cannot move under the user.
//!
//! It is deliberately **not configurable**. One fixed definition, in-tree —
//! a per-project knob here would be the project defining the fallback that
//! exists for when the project's own definition is what broke.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::DevcontainerConfig;

/// The baseline `devcontainer.json`, compiled in.
const BASELINE_DEVCONTAINER_JSON: &str =
    include_str!("../../../data/baseline-environment/devcontainer.json");

/// The baseline `Containerfile`, compiled in.
const BASELINE_CONTAINERFILE: &str =
    include_str!("../../../data/baseline-environment/Containerfile");

/// Where the baseline definition is written out.
///
/// **Fixed, and shared by every workspace on the machine — deliberately.**
/// `config_hash` includes the config file's own path, so a per-workspace
/// staging directory would give each workspace a different build hash for
/// byte-identical bytes, and therefore its own copy of the image. One path
/// means one `taste-img-<hash>`, built once and reused by every environment
/// that ever falls back to it.
pub fn baseline_dir() -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            Path::new(&home).join(".local/state")
        })
        .join("taste-ide")
        .join("baseline")
}

/// Write the bundled definition out and parse it.
///
/// Rewritten every time rather than only when missing: the bytes are the
/// IDE's, so an upgrade that changes them must take effect without anyone
/// remembering to clear a cache. Writing identical bytes is cheap, and the
/// build hash moves by itself when they differ — which correctly makes the
/// old baseline image stale rather than silently reusing it.
pub fn ensure_baseline_config() -> Result<DevcontainerConfig> {
    ensure_baseline_config_in(&baseline_dir())
}

/// [`ensure_baseline_config`] against an explicit directory. Split out so
/// the tests need no process-global `XDG_STATE_HOME`, which two tests
/// setting at once would make racy.
pub fn ensure_baseline_config_in(dir: &Path) -> Result<DevcontainerConfig> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating baseline directory {}", dir.display()))?;

    let containerfile = dir.join("Containerfile");
    write_if_changed(&containerfile, BASELINE_CONTAINERFILE)?;
    let json = dir.join("devcontainer.json");
    write_if_changed(&json, BASELINE_DEVCONTAINER_JSON)?;

    let config = DevcontainerConfig::load(&json)
        .with_context(|| format!("parsing the baseline config at {}", json.display()))?;
    config
        .validate()
        .context("the bundled baseline config is not a usable devcontainer")?;
    // The baseline is the IDE's own, so this can never fail on repo input —
    // but it is checked anyway. The validator is what "a container the IDE
    // starts is confined" means, and a definition exempt from it would be a
    // second, unexamined standard.
    crate::security::validate_security(&config, dir)
        .context("the bundled baseline config does not pass the security validator")?;
    Ok(config)
}

/// Avoid rewriting bytes that already match, so the mtime (and the file
/// watcher) stay quiet on the common path.
fn write_if_changed(path: &Path, contents: &str) -> Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == contents) {
        return Ok(());
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The definition compiled into the binary must actually be a
    /// devcontainer the supervisor can drive — parsed, valid, and confined.
    /// A typo here breaks the rung that exists so nothing else can break.
    #[test]
    fn the_bundled_definition_parses_validates_and_is_confined() {
        let dir = tempfile::tempdir().unwrap();
        let config = ensure_baseline_config_in(dir.path()).unwrap();

        assert_eq!(config.name.as_deref(), Some("baseline"));
        assert!(
            config.dockerfile_path().is_some_and(|p| p.is_file()),
            "the Containerfile must be written out beside the config"
        );
        config.validate().unwrap();
        crate::security::validate_security(&config, dir.path()).unwrap();
    }

    /// The baseline exists to host an agent, and the conventions for that
    /// are checked at runtime by the hosting probe — but node and the agent
    /// home are decided *here*, in the image, and a baseline missing either
    /// would refuse relocation on every host at once.
    #[test]
    fn the_image_carries_what_hosting_an_agent_requires() {
        assert!(
            BASELINE_CONTAINERFILE.contains("nodejs"),
            "every ACP adapter, the MCP bridge and the auth forwarder are node programs"
        );
        assert!(
            BASELINE_CONTAINERFILE.contains("git"),
            "reading history is how an agent understands a repo it may not write"
        );
        // The home the supervisor mounts the agent's volume at must be the
        // home the image gives its user, or the agent's history lands
        // somewhere the next container will not look.
        assert!(
            BASELINE_CONTAINERFILE.contains(taste_core::policy::AGENT_HOME_IN_DEVCONTAINER),
            "the baseline user's home must be {}",
            taste_core::policy::AGENT_HOME_IN_DEVCONTAINER
        );
    }

    /// The rung that is supposed to always work must not move under the
    /// user because an upstream tag was republished.
    #[test]
    fn the_base_image_is_pinned_by_digest() {
        let from = BASELINE_CONTAINERFILE
            .lines()
            .find(|l| l.starts_with("FROM "))
            .expect("a Containerfile has a FROM");
        assert!(
            from.contains("@sha256:"),
            "the baseline base image must be pinned by digest, got {from}"
        );
    }

    /// The baseline declares no lifecycle hooks, and that is load-bearing:
    /// `devcontainer_reload`'s consent prompt names the commands a config
    /// will run, and the answer for the IDE's own fallback must be "none".
    #[test]
    fn the_baseline_runs_no_lifecycle_commands() {
        let dir = tempfile::tempdir().unwrap();
        let config = ensure_baseline_config_in(dir.path()).unwrap();
        assert!(config.on_create_command.is_none());
        assert!(config.post_create_command.is_none());
        assert!(config.post_start_command.is_none());
    }
}

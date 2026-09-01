//! Environment identity, and the one place every derived resource name is
//! computed.
//!
//! A **workspace** is an open folder. An **environment** is one supervised
//! world inside it: a checkout (the main one, or a clone), a devcontainer
//! built from that checkout's config, a mode, and zero or one bound chat.
//! The workspace always has the [`EnvironmentId::primary`] environment;
//! everything else is created on demand.
//!
//! Every podman-visible string — container name, image tag, volumes, MCP
//! socket, build staging directory — is derived here and *only* here.
//! Scattering `format!("taste-{hash}")` around the codebase is how the
//! single-environment scheme ended up with a machine-global agent-home
//! volume and an image tag that could not be shared; one module owning the
//! names is what makes the environment dimension impossible to forget.
//!
//! The primary environment is not a special case in any of these functions:
//! it is the environment whose slug happens to be `primary`. Old-scheme
//! names (no environment dimension at all) are *also* derived here, solely
//! so the startup sweep can recognise and remove them — see
//! [`legacy_container_name`].

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// The reserved slug for the environment backing the main checkout.
pub const PRIMARY: &str = "primary";

/// Upper bound on a slug. Environment ids are concatenated into container
/// and volume names, which podman keeps to a sane length, and into
/// directory names; short is also how they stay readable in the fleet view.
pub const MAX_ID_LEN: usize = 24;

/// Container/image label: which workspace a podman resource belongs to.
/// Reconciliation enumerates by these labels rather than by exact name, so
/// a resource whose name we would not have guessed is still ours.
pub const LABEL_WORKSPACE: &str = "taste.workspace";
/// Container/image label: which environment of that workspace.
pub const LABEL_ENV: &str = "taste.env";
/// Container label: the config hash the container was created from.
pub const LABEL_CONFIG_HASH: &str = "taste.config-hash";

/// A short, stable environment slug.
///
/// Validation is not cosmetic: the value lands verbatim in container names,
/// volume names, socket filenames and directory paths, so it is restricted
/// to lowercase alphanumerics and interior dashes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EnvironmentId(String);

impl EnvironmentId {
    /// The environment backing the main checkout. Always exists.
    pub fn primary() -> Self {
        Self(PRIMARY.to_string())
    }

    pub fn is_primary(&self) -> bool {
        self.0 == PRIMARY
    }

    /// Parse and validate a slug: 1..=[`MAX_ID_LEN`] characters, lowercase
    /// alphanumerics and single interior dashes, starting and ending
    /// alphanumeric.
    pub fn parse(raw: impl AsRef<str>) -> Result<Self> {
        let raw = raw.as_ref();
        if raw.is_empty() {
            bail!("environment id is empty");
        }
        if raw.len() > MAX_ID_LEN {
            bail!("environment id {raw:?} is longer than {MAX_ID_LEN} characters");
        }
        if !raw
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            bail!("environment id {raw:?} must be lowercase letters, digits and dashes");
        }
        let first = raw.chars().next().unwrap();
        let last = raw.chars().next_back().unwrap();
        if first == '-' || last == '-' {
            bail!("environment id {raw:?} must start and end with a letter or digit");
        }
        if raw.contains("--") {
            bail!("environment id {raw:?} must not contain consecutive dashes");
        }
        Ok(Self(raw.to_string()))
    }

    /// Best-effort slug from free text (a chat title, a task summary).
    /// Fails only when nothing usable survives.
    pub fn slugify(text: &str) -> Result<Self> {
        let mut slug = String::new();
        for c in text.chars() {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                slug.push(c);
            } else if !slug.ends_with('-') && !slug.is_empty() {
                slug.push('-');
            }
        }
        while slug.ends_with('-') {
            slug.pop();
        }
        slug.truncate(MAX_ID_LEN);
        while slug.ends_with('-') {
            slug.pop();
        }
        Self::parse(slug)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EnvironmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for EnvironmentId {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self> {
        Self::parse(value)
    }
}

impl From<EnvironmentId> for String {
    fn from(value: EnvironmentId) -> Self {
        value.0
    }
}

/// The workspace's stable key: 6 bytes of SHA-256 over the root path, hex.
///
/// Twelve hex characters, the same width the single-environment scheme
/// used, so a machine's existing container names stay recognisable to the
/// legacy sweep.
pub fn workspace_key(workspace_root: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(workspace_root.to_string_lossy().as_bytes());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// `taste-<workspace-key>-<env>` — the container for one environment.
pub fn env_container_name(workspace_root: &Path, env: &EnvironmentId) -> String {
    format!("taste-{}-{env}", workspace_key(workspace_root))
}

/// The agent's own home volume, per environment.
///
/// The single-environment scheme used one machine-global `taste-agent-home`
/// for every workspace on the machine. That was already wrong (two projects
/// shared one agent history); with N environments it would also mean N
/// agents writing one home concurrently.
pub fn env_home_volume(workspace_root: &Path, env: &EnvironmentId) -> String {
    format!("taste-env-{}-{env}-home", workspace_key(workspace_root))
}

/// A volume the repo's own devcontainer.json declared, namespaced to this
/// environment.
///
/// devcontainer.json names volumes with a verbatim string. Two environments
/// of the same workspace run the *same* config, so without a prefix they
/// would silently share every declared cache — one agent's `cargo build`
/// clobbering another's. Sharing on purpose is a later, explicit feature;
/// the default is separation.
pub fn env_config_volume(workspace_root: &Path, env: &EnvironmentId, declared: &str) -> String {
    let sanitized: String = declared
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // `-cfg-` keeps repo-declared volumes out of the IDE's own namespace: a
    // config that declares a volume literally called `home` must not
    // collide with `env_home_volume`.
    format!(
        "taste-env-{}-{env}-cfg-{sanitized}",
        workspace_key(workspace_root)
    )
}

/// The image tag for a given build hash.
///
/// Deliberately *not* keyed by workspace or environment: environments whose
/// devcontainer config hashes identically share one image. N environments
/// must not mean N copies of a multi-gigabyte image. The workspace tie
/// needed for cleanup rides on the [`LABEL_WORKSPACE`] label instead of the
/// name.
pub fn env_image_tag(build_hash: &str) -> String {
    let short: String = build_hash.chars().take(12).collect();
    format!("taste-img-{short}")
}

/// The MCP socket for one environment. Per-environment because the socket
/// *is* the caller's identity — an agent that connects here is that
/// environment, and `taste-mcp` routes every environment-facing tool on
/// which socket the connection arrived on.
pub fn env_socket_path(workspace_root: &Path, env: &EnvironmentId) -> PathBuf {
    crate::mcp::socket_path(&env_container_name(workspace_root, env))
}

/// Where an environment's channel endpoints live **inside** its container.
///
/// Not a host path and never mounted from one: the sockets under here are
/// bound by the channel helper the IDE `podman exec`s into the container,
/// and dialled by the agent running beside it. Both ends are the container's
/// own processes, which is the whole point — an SELinux-enforcing host
/// refuses a confined container `connectto` on a socket the unconfined IDE
/// bound, and permits it on one the container bound itself.
///
/// `/tmp` because it is the one directory every image guarantees is
/// writable by whoever `podman exec` runs as. The environment id is in the
/// name for a human reading `ls`, not for identity: nothing on the IDE side
/// ever parses it back.
pub fn container_channel_dir(env: &EnvironmentId) -> PathBuf {
    PathBuf::from(format!("/tmp/taste-ide-{env}"))
}

/// The environment channel's MCP endpoint, inside the container. The
/// relocated agent's stdio bridge connects here instead of to a mounted
/// host socket.
pub fn container_mcp_socket(env: &EnvironmentId) -> PathBuf {
    container_channel_dir(env).join("mcp.sock")
}

/// The environment channel's auth endpoint, inside the container. The
/// relocated agent's auth forwarder connects here and republishes it as the
/// loopback `ANTHROPIC_BASE_URL` the adapter expects.
pub fn container_auth_socket(env: &EnvironmentId) -> PathBuf {
    container_channel_dir(env).join("auth.sock")
}

/// Build-context staging directory name for one environment. Per
/// environment: two environments can build concurrently, and staging is
/// destructive (it wipes the directory first).
pub fn env_staging_name(workspace_root: &Path, env: &EnvironmentId) -> String {
    env_container_name(workspace_root, env)
}

/// `$XDG_STATE_HOME/taste-ide/environments` — where non-primary clones live.
pub fn environments_base() -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            Path::new(&home).join(".local/state")
        })
        .join("taste-ide")
        .join("environments")
}

/// The directory holding one environment's IDE-owned state (its clone, and
/// whatever later phases put beside it).
pub fn env_dir(workspace_root: &Path, env: &EnvironmentId) -> PathBuf {
    environments_base()
        .join(workspace_key(workspace_root))
        .join(env.as_str())
}

/// Where an environment's checkout lives.
///
/// The primary environment *is* the main checkout — no clone, no copy. Every
/// other environment gets `<env_dir>/repo`, a git clone the IDE owns.
pub fn env_repo_root(workspace_root: &Path, env: &EnvironmentId) -> PathBuf {
    if env.is_primary() {
        workspace_root.to_path_buf()
    } else {
        env_dir(workspace_root, env).join("repo")
    }
}

/// The container name the *single-environment* scheme used for this
/// workspace. Nothing creates this any more; the startup sweep looks for it
/// so a machine upgrading into multi-environment does not leave an
/// unmanaged container holding this workspace's forwarded ports.
pub fn legacy_container_name(workspace_root: &Path) -> String {
    format!("taste-{}", workspace_key(workspace_root))
}

/// The image tag the single-environment scheme used for this workspace.
pub fn legacy_image_tag(workspace_root: &Path) -> String {
    format!("{}-image", legacy_container_name(workspace_root))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(s: &str) -> EnvironmentId {
        EnvironmentId::parse(s).unwrap()
    }

    #[test]
    fn primary_is_an_ordinary_id_with_a_reserved_slug() {
        assert!(EnvironmentId::primary().is_primary());
        assert_eq!(EnvironmentId::primary(), env(PRIMARY));
        assert!(!env("review").is_primary());
    }

    #[test]
    fn ids_are_validated_because_they_become_container_names() {
        for good in ["primary", "a", "fix-42", "z9", &"a".repeat(MAX_ID_LEN)] {
            assert!(EnvironmentId::parse(good).is_ok(), "{good} rejected");
        }
        for bad in [
            "",
            "-lead",
            "trail-",
            "Upper",
            "has_underscore",
            "double--dash",
            "with space",
            "sl/ash",
            "a.b",
            &"a".repeat(MAX_ID_LEN + 1),
        ] {
            assert!(EnvironmentId::parse(bad).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn slugify_survives_human_text() {
        assert_eq!(
            EnvironmentId::slugify("Fix the Login Bug!")
                .unwrap()
                .as_str(),
            "fix-the-login-bug"
        );
        assert_eq!(EnvironmentId::slugify("  ---  ").ok(), None);
        assert!(
            EnvironmentId::slugify(&"very long task title ".repeat(10))
                .unwrap()
                .as_str()
                .len()
                <= MAX_ID_LEN
        );
    }

    #[test]
    fn names_are_stable_per_workspace_and_environment() {
        let root = Path::new("/work/project");
        assert_eq!(
            env_container_name(root, &env("primary")),
            env_container_name(root, &env("primary"))
        );
        assert_ne!(
            env_container_name(root, &env("primary")),
            env_container_name(root, &env("review"))
        );
        assert_ne!(
            env_container_name(root, &env("primary")),
            env_container_name(Path::new("/work/other"), &env("primary"))
        );
        assert!(env_container_name(root, &env("primary")).starts_with("taste-"));
        assert!(env_container_name(root, &env("primary")).ends_with("-primary"));
    }

    #[test]
    fn every_derived_name_carries_the_environment_dimension() {
        let root = Path::new("/work/project");
        let (a, b) = (env("primary"), env("review"));
        assert_ne!(env_home_volume(root, &a), env_home_volume(root, &b));
        assert_ne!(
            env_config_volume(root, &a, "cargo"),
            env_config_volume(root, &b, "cargo")
        );
        assert_ne!(env_socket_path(root, &a), env_socket_path(root, &b));
        assert_ne!(env_staging_name(root, &a), env_staging_name(root, &b));
        assert_ne!(env_repo_root(root, &a), env_repo_root(root, &b));
    }

    /// The one name that must NOT be per-environment: two environments with
    /// identical config share one image rather than two multi-gigabyte
    /// copies.
    #[test]
    fn image_tags_are_keyed_by_build_hash_alone() {
        assert_eq!(env_image_tag("abcdef0123456789"), "taste-img-abcdef012345");
        assert_ne!(env_image_tag("abcdef0123456789"), env_image_tag("ffff"));
    }

    #[test]
    fn declared_volumes_cannot_collide_with_the_ide_namespace() {
        let root = Path::new("/work/project");
        let e = env("primary");
        assert_ne!(
            env_config_volume(root, &e, "home"),
            env_home_volume(root, &e)
        );
        // Anything podman would choke on is flattened, not passed through.
        assert_eq!(
            env_config_volume(root, &e, "we/ird name"),
            format!("taste-env-{}-primary-cfg-we_ird_name", workspace_key(root))
        );
    }

    #[test]
    fn primary_repo_root_is_the_main_checkout_not_a_clone() {
        let root = Path::new("/work/project");
        assert_eq!(env_repo_root(root, &EnvironmentId::primary()), root);
        assert!(env_repo_root(root, &env("review")).ends_with("review/repo"));
    }

    /// The legacy names are derived here so the sweep and the new scheme
    /// cannot disagree about what "old" looks like.
    #[test]
    fn legacy_names_are_distinguishable_from_new_ones() {
        let root = Path::new("/work/project");
        let legacy = legacy_container_name(root);
        assert_eq!(legacy, format!("taste-{}", workspace_key(root)));
        assert_ne!(legacy, env_container_name(root, &EnvironmentId::primary()));
        assert_eq!(legacy_image_tag(root), format!("{legacy}-image"));
    }
}

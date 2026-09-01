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

/// How many environments beyond the primary an ORCHESTRATOR may have in
/// flight at once.
///
/// Every one of these is a clone of the repository, a container, an agent
/// process and a share of the user's subscription; six of them is already
/// more than a laptop enjoys, and an orchestrator that fans out to twenty
/// has not planned, it has thrashed. Soft in the precise sense that it
/// bounds the *tool*, not the user: the fleet view's own "New Environment"
/// and a chat's own "Give This Chat Its Own Environment" are a person's
/// decision about their own machine and stay unbounded. The tool refuses
/// by naming the cap and what to do about it (destroy something finished,
/// or wait for a chat to end its turn).
pub const MAX_ORCHESTRATED_ENVIRONMENTS: usize = 6;

/// Container/image label: which workspace a podman resource belongs to.
/// Reconciliation enumerates by these labels rather than by exact name, so
/// a resource whose name we would not have guessed is still ours.
pub const LABEL_WORKSPACE: &str = "taste.workspace";
/// Container/image label: which environment of that workspace.
pub const LABEL_ENV: &str = "taste.env";
/// Container label: the config hash the container was created from.
pub const LABEL_CONFIG_HASH: &str = "taste.config-hash";
/// Container label: whose config built it — `project` or `baseline`.
///
/// Adoption reads this. A container left running by a previous IDE run is
/// the one case where the supervisor cannot know which rung of the ladder
/// produced it, and guessing from the config now on disk would be wrong in
/// exactly the interesting case: a baseline container still running beside
/// a project config the user has since repaired but not yet applied. The
/// container's own claim settles it.
pub const LABEL_AUTHORITY: &str = "taste.authority";

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

/// The generation tag mixed into [`workspace_key`]'s hash input.
///
/// Domain separation, and it earns its keep twice. It makes the key of one
/// generation share no prefix with the key of another, which is what lets
/// [`previous_generation_key`] name the old generation to the sweep without
/// any chance of matching a current name; and it means a future re-keying
/// is a one-character change here rather than a new hash construction.
///
/// Bump this — and only this — when the derivation changes. Alpha rules
/// (ARCHITECTURE → Compatibility posture): the old names are swept and
/// reported, never migrated.
const KEY_GENERATION: &str = "2";

/// How many bytes of the digest the key carries. Eight — sixty-four bits.
///
/// The previous generation took six. Forty-eight bits is already far more
/// than a person's folder count needs, so this is not a fix for a collision
/// anyone would ever see; it is that the key is the ONLY thing separating N
/// concurrent IDE windows' containers, volumes and sockets on one machine,
/// and four more hex characters in a name nobody types is the cheapest
/// insurance in the codebase.
const KEY_BYTES: usize = 8;

/// The workspace's stable key: [`KEY_BYTES`] of SHA-256 over the
/// **canonicalized** root path, hex.
///
/// Canonicalized because the key is an identity, and `/home/me/work/proj`
/// reached through a symlink must be the same workspace as the path the
/// symlink resolves to — otherwise one folder opened two ways is two sets
/// of containers fighting over one checkout. `canonicalize` needs the path
/// to exist; a root that does not (a test fixture, a folder deleted out
/// from under an open window) falls back to the path as given, which is
/// stable for as long as it is all we have.
pub fn workspace_key(workspace_root: &Path) -> String {
    key_over(KEY_GENERATION, KEY_BYTES, &settled(workspace_root))
}

/// The path this key is actually taken over.
///
/// `canonicalize` is the real answer and needs the path to exist, which it
/// does for every workspace the IDE opens — `main.rs` canonicalizes the
/// folder before anything else sees it. When it does not (a test fixture, a
/// folder deleted out from under an open window) the components are still
/// walked, which is not symlink resolution but does settle the spellings
/// that cost nothing to settle: a trailing slash, an interior `.`, a
/// doubled separator. Without it `/work/p` and `/work/p/` are two
/// workspaces, and the only thing standing between them and two sets of
/// containers is that nobody happened to type the slash.
fn settled(path: &Path) -> PathBuf {
    path.canonicalize()
        .unwrap_or_else(|_| path.components().collect())
}

/// The key the **previous** naming generation computed for this workspace:
/// six bytes over the path as handed in, un-canonicalized and
/// un-domain-separated.
///
/// Exists for one reason — [`crate::environment::legacy_container_name`] and
/// the startup sweep, so a rename reports what it orphaned instead of
/// leaking it. Nothing creates a name from this.
///
/// It cannot recover names from a run that opened this folder through a
/// symlink: that generation hashed whatever string it was given, and the
/// canonical path is all this build has. Those names are swept when the
/// user next opens the folder by the same route, and are otherwise the
/// documented cost of the re-key.
pub fn previous_generation_key(workspace_root: &Path) -> String {
    key_over("", 6, &settled(workspace_root))
}

fn key_over(generation: &str, bytes: usize, path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    if !generation.is_empty() {
        hasher.update(generation.as_bytes());
        hasher.update(b":");
    }
    hasher.update(path.to_string_lossy().as_bytes());
    hasher
        .finalize()
        .iter()
        .take(bytes)
        .map(|b| format!("{b:02x}"))
        .collect()
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
/// Deliberately *not* keyed by workspace or environment. The tag is
/// **content-addressed**: the hash is over the devcontainer config's own
/// bytes, so anything that hashes the same is the same image, and N
/// environments must not mean N copies of a multi-gigabyte image.
///
/// The sharing reaches further than one workspace, and that is correct
/// rather than tolerated. N IDE windows are open at once by design, and two
/// projects with byte-identical devcontainer configs genuinely want one
/// image. Nothing anywhere looks an image up by workspace — the Resources
/// view asks for `reference=<this environment's tag>` — so there is no
/// ownership claim here to be wrong. An image does carry
/// [`LABEL_WORKSPACE`], but only whichever workspace built it last, which
/// is precisely why nothing may read it: a shared name cannot express sole
/// ownership, and a label that tried would be a fact that quietly changes
/// under a rebuild.
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

/// The **discovery directory**: every open window's fleet socket, and
/// nothing else.
///
/// A directory rather than a glob over the runtime directory, because
/// enumeration is the point. N windows are open at once by design (each
/// `taste-ide <folder>` is its own process — see `main.rs`, NON_UNIQUE), so
/// a client that wants *all* of them — the in-tree GNOME Shell extension
/// aggregating every project the user has open — reads this directory and
/// dials each entry. No pattern to get subtly wrong, and no chance of
/// sweeping up an environment's MCP socket, which lives in the runtime
/// directory beside it.
///
/// Created 0700 by whoever binds first. That matters only in the `/tmp`
/// fallback, where a private directory is strictly better than private
/// files in a shared one.
pub fn fleet_dir() -> PathBuf {
    crate::mcp::runtime_socket("taste-ide").join("fleet")
}

/// The fleet service socket for a workspace: `<fleet-dir>/<key>.socket`.
///
/// Per WORKSPACE, not per environment, and that is the whole difference
/// from [`env_socket_path`]. The MCP socket *is* the caller's identity —
/// which socket an agent connects on is which environment it is — so
/// there is one per environment. The fleet service answers the opposite
/// question: what is this window supervising, all of it at once. One
/// window, one open folder, one socket.
///
/// The name is derived here with every other podman- and socket-visible
/// string, so the GNOME Shell extension that eventually reads it and the
/// IDE that binds it cannot disagree. The key is not meant to be reversed:
/// a client reads [`fleet_dir`], dials each entry and asks it who it is
/// (`org.varlink.service.GetInfo`, then `List`, whose `workspaceRoot`
/// field names the folder in full).
///
/// Two windows on the same folder derive the same path, which is
/// deliberate and is exactly why the bind refuses when a live service
/// already answers there — one folder has one supervisor. See
/// `crate::instance`.
pub fn fleet_socket_path(workspace_root: &Path) -> PathBuf {
    fleet_dir().join(format!("{}.socket", workspace_key(workspace_root)))
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

/// The stem every name of the **previous generation** started with:
/// `taste-<previous-generation-key>`.
///
/// It is the single-environment scheme's whole container name, and it is
/// the prefix of every multi-environment name from before the re-key
/// (`taste-<prev>-<env>`). Nothing creates either any more; the startup
/// sweep looks for both so a machine crossing a generation does not leave
/// unmanaged containers holding this workspace's forwarded ports.
///
/// Because [`KEY_GENERATION`] domain-separates the current key, this stem
/// can never be a prefix of a current name — which is what makes matching
/// the old generation *by name* safe.
pub fn legacy_container_name(workspace_root: &Path) -> String {
    format!("taste-{}", previous_generation_key(workspace_root))
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

    /// The fleet socket is the one socket keyed by workspace alone: it
    /// answers for the window, not for one environment in it.
    #[test]
    fn the_fleet_socket_is_per_workspace_not_per_environment() {
        let root = Path::new("/work/project");
        let socket = fleet_socket_path(root);
        let name = socket.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(name, format!("{}.socket", workspace_key(root)));
        assert_ne!(name, ".socket", "the key is actually in there");
        assert_ne!(
            fleet_socket_path(root),
            fleet_socket_path(Path::new("/work/other")),
            "two open windows are two sockets"
        );
        // It must never collide with an environment's MCP socket. That is
        // now structural rather than a naming argument: the MCP sockets are
        // in the runtime directory, the fleet sockets in a directory of
        // their own.
        for env in [EnvironmentId::primary(), env("review")] {
            assert_ne!(fleet_socket_path(root), env_socket_path(root, &env));
            assert_ne!(
                env_socket_path(root, &env).parent(),
                Some(fleet_dir().as_path())
            );
        }
        // Enumerable: every fleet socket is a direct child of one directory,
        // so a shell extension reads a directory rather than matching a
        // pattern.
        assert_eq!(socket.parent(), Some(fleet_dir().as_path()));
    }

    /// Every window's fleet socket lands in one directory, and two windows
    /// are two entries in it — that directory listing *is* the discovery
    /// mechanism.
    #[test]
    fn fleet_sockets_are_enumerable_from_one_directory() {
        let roots = [
            Path::new("/work/project"),
            Path::new("/work/other"),
            Path::new("/elsewhere/project"),
        ];
        let mut names: Vec<String> = roots
            .iter()
            .map(|root| {
                let socket = fleet_socket_path(root);
                assert_eq!(socket.parent(), Some(fleet_dir().as_path()));
                socket.file_name().unwrap().to_string_lossy().to_string()
            })
            .collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), roots.len(), "three windows, three entries");
    }

    /// The key is an identity, so a folder reached through a symlink is the
    /// same workspace as the folder itself. Two keys for one checkout would
    /// be two sets of containers fighting over it.
    #[test]
    fn the_key_follows_symlinks_because_it_is_an_identity() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(workspace_key(&real), workspace_key(&link));
        assert_eq!(
            env_container_name(&real, &EnvironmentId::primary()),
            env_container_name(&link, &EnvironmentId::primary()),
        );
        assert_eq!(fleet_socket_path(&real), fleet_socket_path(&link));
        // ...and a trailing-slash or `.` spelling of the same folder, which
        // is what an "Open With" hand-off can produce.
        assert_eq!(workspace_key(&real), workspace_key(&real.join(".")));

        // The same settling applies when the folder does NOT exist and
        // canonicalize has nothing to resolve — the spellings that cost
        // nothing to settle are settled anyway.
        let gone = Path::new("/work/deleted-project");
        assert_eq!(
            workspace_key(gone),
            workspace_key(Path::new("/work/deleted-project/"))
        );
        assert_eq!(
            workspace_key(gone),
            workspace_key(Path::new("/work/./deleted-project"))
        );

        // A different real folder is still a different workspace.
        let other = dir.path().join("other");
        std::fs::create_dir(&other).unwrap();
        assert_ne!(workspace_key(&real), workspace_key(&other));
    }

    /// Distinct paths give distinct keys, including ones that differ only
    /// where a truncation would hide it.
    #[test]
    fn distinct_paths_give_distinct_keys() {
        let paths = [
            "/work/project",
            "/work/project2",
            "/work/Project",
            "/work/other/project",
            "/",
            "/work",
        ];
        let mut keys: Vec<String> = paths.iter().map(|p| workspace_key(Path::new(p))).collect();
        assert!(keys.iter().all(|k| k.len() == KEY_BYTES * 2));
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), paths.len());
    }

    /// The re-key has to be a clean break: the sweep matches the previous
    /// generation BY NAME, so a current name that happened to start with the
    /// old stem would be swept as stale. Domain separation is what rules
    /// that out.
    #[test]
    fn the_previous_generation_key_is_no_prefix_of_the_current_one() {
        for path in ["/work/project", "/work/other", "/", "/a/b/c/d"] {
            let root = Path::new(path);
            let current = workspace_key(root);
            let previous = previous_generation_key(root);
            assert_eq!(previous.len(), 12, "the old generation's width");
            assert_ne!(current, previous);
            assert!(
                !current.starts_with(&previous),
                "{current} starts with the old stem {previous}"
            );
            // ...which is the property the sweep actually leans on.
            let stem = format!("{}-", legacy_container_name(root));
            assert!(!env_container_name(root, &EnvironmentId::primary()).starts_with(&stem));
        }
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
        assert_eq!(legacy, format!("taste-{}", previous_generation_key(root)));
        assert_ne!(legacy, env_container_name(root, &EnvironmentId::primary()));
        assert_eq!(legacy_image_tag(root), format!("{legacy}-image"));
    }
}

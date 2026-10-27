//! **The agent's runtime, as a container sees it**: the IDE's own node,
//! Task, and agent packages, installed once per VM and mounted read-only
//! into every container on it (`taste_devcontainer::agentnode`). Here, and
//! not there, because the ACP crate composes the agent's command from these
//! names and cannot depend on the container crate.

/// Where an environment's container has the runtime.
pub const IN_CONTAINER: &str = "/opt/taste-agent";

/// The directory an npm package spec, `<name>@<version>`, is installed
/// under in the VM's runtime directory's `agents/`: the spec with its
/// scope's `@` gone and each `/` and the version's `@` a `-` — one token,
/// the version in it, so a bump installs beside the old one. `None` for a spec with no
/// version, which is never installed: a version is what makes it safe to
/// share.
pub fn agent_key(package: &str) -> Option<String> {
    let bare = package.strip_prefix('@').unwrap_or(package);
    let (name, version) = bare.rsplit_once('@')?;
    let key = format!("{}-{version}", name.replace('/', "-"));
    (!name.is_empty()
        && !version.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
    .then_some(key)
}

/// Where a package installed once for the VM is run from, inside a
/// container: a link to its command, which the VM's install makes once
/// it is whole (`taste_devcontainer::agentnode`). Whether it is there is
/// the container's to find (`taste_acp::relocate`), and an agent whose
/// package is not falls back to `npx`.
pub fn preinstalled_run(package: &str) -> Option<String> {
    agent_key(package).map(|key| format!("{IN_CONTAINER}/agents/{key}/run"))
}

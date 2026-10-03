//! The one-time setup the user runs, and what it grants.
//!
//! The setup is `build-aux/gcp-setup.sh`, checked in, documented there,
//! and runnable by hand. The IDE hands out that same file with the
//! workspace's three values filled in ([`cloud_shell_script`]), embedded
//! at build time, so the copy a user pastes and the copy reviewed in the
//! tree cannot differ. The constants below are the IDE's half of the same
//! facts — the preflight asks `testIamPermissions` for [`PERMISSIONS`] —
//! and a test reads the script and holds the two equal.
//!
//! The IDE never holds the user's own Google credentials, not even to set
//! itself up: the script runs where gcloud is signed in as the user, and
//! what it grants is a role of exactly the calls the IDE makes, to the
//! federated identity whose key is in this machine's TPM.

use anyhow::{bail, Result};

use crate::resources::Workspace;

/// The Workload Identity pool, shared by every workspace in a project.
pub const POOL: &str = "taste-ide";
/// The custom role, shared the same way.
pub const ROLE: &str = "tasteIde";

/// The APIs the IDE calls.
pub const SERVICES: &[&str] = &[
    "cloudbilling.googleapis.com",
    "cloudresourcemanager.googleapis.com",
    "compute.googleapis.com",
    "dns.googleapis.com",
    "iam.googleapis.com",
    "iamcredentials.googleapis.com",
    "sts.googleapis.com",
];

/// Exactly what the IDE calls: the model's networks, rules, DNS policy,
/// disks, and instances, the operations it waits on, and the quota and
/// machine facts the preflight reads. `testIamPermissions` asks for this
/// same list, so a role that has drifted is named before anything fails.
pub const PERMISSIONS: &[&str] = &[
    "compute.disks.create",
    "compute.disks.delete",
    "compute.disks.get",
    "compute.disks.list",
    "compute.disks.setLabels",
    "compute.disks.update",
    "compute.disks.use",
    "compute.disks.useReadOnly",
    "compute.firewalls.create",
    "compute.firewalls.delete",
    "compute.firewalls.get",
    "compute.firewalls.list",
    "compute.firewalls.update",
    "compute.globalOperations.get",
    "compute.instances.create",
    "compute.instances.delete",
    "compute.instances.get",
    "compute.instances.getGuestAttributes",
    "compute.instances.list",
    "compute.instances.setLabels",
    "compute.instances.setMetadata",
    "compute.instances.start",
    "compute.instances.stop",
    "compute.machineTypes.get",
    "compute.networks.create",
    "compute.networks.delete",
    "compute.networks.get",
    "compute.networks.list",
    "compute.networks.updatePolicy",
    "compute.networks.use",
    "compute.projects.get",
    "compute.regionOperations.get",
    "compute.regions.get",
    "compute.subnetworks.create",
    "compute.subnetworks.delete",
    "compute.subnetworks.get",
    "compute.subnetworks.list",
    "compute.subnetworks.use",
    "compute.subnetworks.useExternalIp",
    "compute.zoneOperations.get",
    "compute.zones.get",
    "dns.networks.bindPrivateDNSPolicy",
    "dns.policies.create",
    "dns.policies.delete",
    "dns.policies.get",
    "dns.policies.list",
    "dns.policies.update",
    "resourcemanager.projects.get",
];

/// The subject the Google leaf's certificate carries, mapped to
/// `google.subject` and nothing else.
pub fn subject(ws: &Workspace) -> String {
    format!("taste-{}", ws.id())
}

/// This workspace's provider in the pool.
pub fn provider(ws: &Workspace) -> String {
    format!("taste-{}", ws.id())
}

/// Where a federated identity is, which is what the token exchange names
/// as its audience. It needs the project's number, which Cloud Shell
/// prints at the end of the setup, because nothing can be asked of the
/// project before the identity exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Federation {
    pub project_number: u64,
    pub provider: String,
}

impl Federation {
    pub fn new(ws: &Workspace, project_number: u64) -> Self {
        Self {
            project_number,
            provider: provider(ws),
        }
    }

    /// The `audience` of the token exchange.
    pub fn audience(&self) -> String {
        format!(
            "//iam.googleapis.com/projects/{}/locations/global/workloadIdentityPools/{POOL}/providers/{}",
            self.project_number, self.provider
        )
    }
}

/// GCP project ids: 6–30 characters, a lowercase letter first, then
/// lowercase letters, digits, and hyphens, not ending in a hyphen. Checked
/// here because the id is pasted into a shell script.
pub fn valid_project_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    (6..=30).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && !id.ends_with('-')
}

/// The setup script, as checked in.
pub const SCRIPT: &str = include_str!("../../../build-aux/gcp-setup.sh");

/// The line that ends the pasted block; nothing in [`SCRIPT`] is that line.
const END: &str = "TASTE_SETUP";

/// [`SCRIPT`] with this workspace's values filled in, wrapped to be pasted
/// into Cloud Shell: run as `bash -c`, so its `set -e` ends a child shell
/// rather than the user's session and gcloud keeps the terminal as its
/// input. `ca_pem` is the workspace's CA certificate, which is public.
pub fn cloud_shell_script(ws: &Workspace, project: &str, ca_pem: &str) -> Result<String> {
    if !valid_project_id(project) {
        bail!("{project:?} is not a GCP project id");
    }
    let ca_pem = ca_pem.trim();
    let lines: Vec<&str> = ca_pem.lines().collect();
    let body_ok = lines.len() > 2
        && lines[1..lines.len() - 1].iter().all(|line| {
            !line.is_empty()
                && line
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
        });
    if lines.first() != Some(&"-----BEGIN CERTIFICATE-----")
        || lines.last() != Some(&"-----END CERTIFICATE-----")
        || !body_ok
    {
        bail!("the trust anchor must be exactly one PEM certificate");
    }
    let (shebang, rest) = SCRIPT
        .split_once('\n')
        .expect("the script has a first line");
    debug_assert!(!SCRIPT.lines().any(|line| line == END));
    Ok(format!(
        "bash -c \"$(cat <<'{END}'\n{shebang}\n\
         TASTE_PROJECT={project}\n\
         TASTE_WORKSPACE={id}\n\
         TASTE_CA_PEM='{ca_pem}'\n\
         {rest}{END}\n)\"\n",
        id = ws.id(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CA: &str =
        "-----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIU\nabc+/=\n-----END CERTIFICATE-----\n";

    fn ws() -> Workspace {
        Workspace::new("0a1b2c3d").unwrap()
    }

    /// The words of a bash array assignment `NAME=( … )` in the script.
    fn script_array(name: &str) -> Vec<String> {
        let start = SCRIPT
            .find(&format!("\n{name}=(\n"))
            .unwrap_or_else(|| panic!("{name} in the script"));
        let body = &SCRIPT[start..];
        let body = &body[body.find('(').unwrap() + 1..body.find(')').unwrap()];
        body.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn the_script_and_the_ide_agree() {
        assert_eq!(script_array("PERMISSIONS"), PERMISSIONS);
        assert_eq!(script_array("SERVICES"), SERVICES);
        assert!(SCRIPT.contains(&format!("\nPOOL={POOL}\n")));
        assert!(SCRIPT.contains(&format!("\nROLE={ROLE}\n")));
    }

    #[test]
    fn permissions_are_sorted_unique_and_well_formed() {
        let mut sorted = PERMISSIONS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, PERMISSIONS);
        for permission in PERMISSIONS {
            let parts: Vec<_> = permission.split('.').collect();
            assert_eq!(parts.len(), 3, "{permission}");
            assert!(parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric())));
        }
    }

    #[test]
    fn nothing_granted_touches_billing_or_iam() {
        for permission in PERMISSIONS {
            assert!(
                !permission.starts_with("billing.")
                    && !permission.starts_with("iam.")
                    && !permission.contains("serviceAccount")
                    && !permission.contains("setIamPolicy"),
                "{permission}"
            );
        }
    }

    #[test]
    fn what_goes_into_the_shell_is_checked() {
        assert!(cloud_shell_script(&ws(), "my-project-1; rm -rf ~", CA).is_err());
        assert!(cloud_shell_script(&ws(), "My-Project", CA).is_err());
        assert!(cloud_shell_script(&ws(), "my-project-1", "not a pem").is_err());
        let two = format!("{CA}{CA}");
        assert!(cloud_shell_script(&ws(), "my-project-1", &two).is_err());
        let quoted = CA.replace("abc", "a'b");
        assert!(cloud_shell_script(&ws(), "my-project-1", &quoted).is_err());
        // A body line spelling the pasted block's terminator never gets in.
        let ended = CA.replace("abc", "TASTE_SETUP");
        assert!(cloud_shell_script(&ws(), "my-project-1", &ended).is_err());
    }

    #[test]
    fn the_audience_names_the_pool_and_this_provider() {
        assert_eq!(
            Federation::new(&ws(), 123456789).audience(),
            "//iam.googleapis.com/projects/123456789/locations/global/workloadIdentityPools/taste-ide/providers/taste-0a1b2c3d"
        );
    }

    /// Run what the IDE hands out, against a gcloud that records its
    /// arguments, as a project where nothing exists yet.
    #[test]
    fn the_pasted_script_creates_the_provider_and_grants_the_role() {
        let Ok(bash) = std::process::Command::new("bash").arg("--version").output() else {
            eprintln!("no bash; skipping");
            return;
        };
        assert!(bash.status.success());
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls");
        let stub = dir.path().join("gcloud");
        std::fs::write(
            &stub,
            r#"#!/usr/bin/env bash
echo "$*" >> "$GCLOUD_LOG"
case "$*" in
  "projects describe"*) echo 123456789 ;;
  *" describe "*) exit 1 ;;
  *create-x509*)
    for arg in "$@"; do
      [[ $arg == --trust-store-config-path=* ]] && cp "${arg#*=}" "$GCLOUD_LOG.trust"
    done ;;
esac
exit 0
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let script = cloud_shell_script(&ws(), "my-project-1", CA).unwrap();
        let path = format!(
            "{}:{}",
            dir.path().display(),
            std::env::var("PATH").unwrap()
        );
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(&script)
            .env("PATH", path)
            .env("GCLOUD_LOG", &log)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("project number: 123456789"));

        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(calls.contains("iam roles create tasteIde --project=my-project-1"));
        assert!(calls.contains("iam workload-identity-pools create taste-ide"));
        assert!(calls.contains("providers create-x509 taste-0a1b2c3d"));
        assert!(calls.contains("--attribute-condition=assertion.subject.dn.cn == 'taste-0a1b2c3d'"));
        assert!(calls.contains(
            "--member=principal://iam.googleapis.com/projects/123456789/locations/global/workloadIdentityPools/taste-ide/subject/taste-0a1b2c3d"
        ));
        let trust = std::fs::read_to_string(dir.path().join("calls.trust")).unwrap();
        assert_eq!(
            trust,
            "trustStore:\n  trustAnchors:\n  - pemCertificate: |\n      -----BEGIN CERTIFICATE-----\n      MIIBszCCAVmgAwIBAgIU\n      abc+/=\n      -----END CERTIFICATE-----\n"
        );
    }
}

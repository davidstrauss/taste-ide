//! The one-time setup the user runs, and what it grants.
//!
//! The IDE never holds the user's own Google credentials, not even to set
//! itself up. It writes out a short script for Cloud Shell, where the user
//! is already signed in, and the script does the three things only a
//! project owner can: enable the APIs, create a Workload Identity
//! Federation provider whose only trust anchor is this workspace's CA
//! (whose key is in the TPM), and grant the identity that CA vouches for a
//! custom role holding exactly [`PERMISSIONS`]. Every step is idempotent,
//! so running it again — after a re-enrollment, or a permission this list
//! gained — repairs rather than fails.
//!
//! The role is least privilege in the literal sense: the calls the IDE
//! makes and nothing else. In particular nothing in it touches billing,
//! IAM, or any service account, so the identity can spend money only by
//! running machines, which the cap meters, and cannot widen its own reach.

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

/// The Cloud Shell script that sets this workspace up in `project`,
/// trusting `ca_pem` — this workspace's CA certificate, which is public.
pub fn cloud_shell_script(ws: &Workspace, project: &str, ca_pem: &str) -> Result<String> {
    if !valid_project_id(project) {
        bail!("{project:?} is not a GCP project id");
    }
    let ca_pem = ca_pem.trim();
    if !ca_pem.starts_with("-----BEGIN CERTIFICATE-----")
        || !ca_pem.ends_with("-----END CERTIFICATE-----")
        || ca_pem.matches("-----BEGIN").count() != 1
    {
        bail!("the trust anchor must be exactly one PEM certificate");
    }
    if !ca_pem.lines().all(|line| {
        line.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+/=- ".contains(&b))
    }) {
        bail!("the trust anchor holds characters a PEM certificate does not");
    }
    let id = ws.id();
    let subject = subject(ws);
    let provider = provider(ws);
    let services = SERVICES.join(" ");
    let permissions = PERMISSIONS.join(",");
    let anchor: String = ca_pem
        .lines()
        .map(|line| format!("      {line}\n"))
        .collect();
    Ok(format!(
        r#"#!/usr/bin/env bash
# taste-ide: let workspace {id} run its model in project {project}.
# Run once in Cloud Shell, signed in as yourself. It is safe to run again.
set -euo pipefail
PROJECT={project}

gcloud services enable {services} --project="$PROJECT"
PROJECT_NUMBER=$(gcloud projects describe "$PROJECT" --format='value(projectNumber)')

# The role: exactly the calls the IDE makes. Nothing in it touches
# billing, IAM, or a service account.
if gcloud iam roles describe {ROLE} --project="$PROJECT" >/dev/null 2>&1; then
  gcloud iam roles update {ROLE} --project="$PROJECT" --quiet \
    --permissions={permissions}
else
  gcloud iam roles create {ROLE} --project="$PROJECT" --stage=GA \
    --title="taste-ide" \
    --description="What the taste-ide model route calls" \
    --permissions={permissions}
fi

# The pool every workspace in this project shares.
gcloud iam workload-identity-pools describe {POOL} --location=global \
    --project="$PROJECT" >/dev/null 2>&1 \
  || gcloud iam workload-identity-pools create {POOL} --location=global \
    --project="$PROJECT" --display-name="taste-ide"

# This workspace's CA, whose key is in the TPM of the machine that made
# it, as the provider's only trust anchor.
TRUST=$(mktemp)
cat > "$TRUST" <<'ANCHOR'
trustStore:
  trustAnchors:
  - pemCertificate: |
{anchor}ANCHOR
if gcloud iam workload-identity-pools providers describe {provider} \
    --location=global --workload-identity-pool={POOL} \
    --project="$PROJECT" >/dev/null 2>&1; then
  gcloud iam workload-identity-pools providers update-x509 {provider} \
    --location=global --workload-identity-pool={POOL} --project="$PROJECT" \
    --trust-store-config-path="$TRUST"
else
  gcloud iam workload-identity-pools providers create-x509 {provider} \
    --location=global --workload-identity-pool={POOL} --project="$PROJECT" \
    --trust-store-config-path="$TRUST" \
    --attribute-mapping="google.subject=assertion.subject.dn.cn" \
    --attribute-condition="assertion.subject.dn.cn == '{subject}'"
fi
rm -f "$TRUST"

gcloud projects add-iam-policy-binding "$PROJECT" --condition=None --quiet \
  --role="projects/$PROJECT/roles/{ROLE}" \
  --member="principal://iam.googleapis.com/projects/$PROJECT_NUMBER/locations/global/workloadIdentityPools/{POOL}/subject/{subject}" \
  >/dev/null

echo
echo "Done. Give the IDE this project number: $PROJECT_NUMBER"
"#
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
    fn the_script_trusts_this_ca_alone_and_names_this_subject() {
        let script = cloud_shell_script(&ws(), "my-project-1", CA).unwrap();
        assert!(script.contains("      -----BEGIN CERTIFICATE-----\n      MIIBszCCAVmgAwIBAgIU\n"));
        assert!(script.contains("assertion.subject.dn.cn == 'taste-0a1b2c3d'"));
        assert!(script.contains("/subject/taste-0a1b2c3d\""));
        assert!(script.contains("PROJECT=my-project-1\n"));
        assert_eq!(script.matches("BEGIN CERTIFICATE").count(), 1);
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
    }

    #[test]
    fn no_anchor_line_can_end_the_heredoc() {
        // The terminator only counts at column 0, and every anchor line is
        // indented, so even a body line that spells it stays inside.
        let spelled = CA.replace("abc", "abc\nANCHOR");
        let script = cloud_shell_script(&ws(), "my-project-1", &spelled).unwrap();
        assert_eq!(script.lines().filter(|l| *l == "ANCHOR").count(), 1);
    }

    #[test]
    fn the_audience_names_the_pool_and_this_provider() {
        assert_eq!(
            Federation::new(&ws(), 123456789).audience(),
            "//iam.googleapis.com/projects/123456789/locations/global/workloadIdentityPools/taste-ide/providers/taste-0a1b2c3d"
        );
    }
}

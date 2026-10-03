//! The one-time setup that grants the IDE its role, and what it grants.
//!
//! The setup is `build-aux/gcp-setup.sh`, checked in, documented there,
//! and runnable by hand or in Cloud Shell. The IDE runs that same file,
//! embedded at build time, with its own pinned gcloud signed in to the
//! project's configuration ([`crate::gcloud::Gcloud::setup`]), so what a
//! person reviews in the tree is what runs. The constants below are the
//! IDE's half of the same facts — the preflight asks `testIamPermissions`
//! for [`PERMISSIONS`] — and a test reads the script and holds the two
//! equal.
//!
//! The setup runs as the user. What it grants is a custom role of exactly
//! the calls the IDE makes, held by a keyless service account the user may
//! act as, so the IDE's calls carry that role's reach and no more of the
//! user's.

/// The custom role, one per project.
pub const ROLE: &str = "tasteIde";
/// The service account's id, one per project.
pub const ACCOUNT: &str = "taste-ide";

/// The service account the IDE's calls run as in `project`.
pub fn service_account(project: &str) -> String {
    format!("{ACCOUNT}@{project}.iam.gserviceaccount.com")
}

/// The APIs the IDE calls.
pub const SERVICES: &[&str] = &[
    "cloudbilling.googleapis.com",
    "cloudresourcemanager.googleapis.com",
    "compute.googleapis.com",
    "dns.googleapis.com",
    "iam.googleapis.com",
    "iamcredentials.googleapis.com",
    "iap.googleapis.com",
];

/// Exactly what the IDE calls: the model's networks, rules, DNS policy,
/// disks, and instances, the operations it waits on, the quota and machine
/// facts the preflight reads, and the IAP tunnel to the model's VM.
/// `testIamPermissions` asks for this same list, so a role that has
/// drifted is named before anything fails.
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
    "iap.tunnelInstances.accessViaIAP",
    "resourcemanager.projects.get",
];

/// GCP project ids: 6–30 characters, a lowercase letter first, then
/// lowercase letters, digits, and hyphens, not ending in a hyphen.
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(SCRIPT.contains(&format!("\nROLE={ROLE}\n")));
        assert!(SCRIPT.contains(&format!("\nACCOUNT={ACCOUNT}\n")));
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
    fn project_ids_are_checked() {
        assert!(valid_project_id("my-project-1"));
        assert!(!valid_project_id("My-Project"));
        assert!(!valid_project_id("my-project-1; rm -rf ~"));
        assert!(!valid_project_id("short"));
        assert!(!valid_project_id("ends-with-"));
    }

    /// A gcloud that records its arguments, answers as a signed-in user in
    /// a project where nothing exists yet, and succeeds at everything else.
    fn stub(dir: &std::path::Path) {
        let path = dir.join("gcloud");
        crate::testing::install_stub(
            &path,
            r#"#!/usr/bin/env bash
echo "$*" >> "$GCLOUD_LOG"
case "$*" in
  "config get-value account") echo david@example.com ;;
  *" describe "*) exit 1 ;;
esac
exit 0
"#,
        );
    }

    fn run(dir: &std::path::Path, args: &[&str]) -> (std::process::Output, String) {
        let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap());
        let log = dir.join("calls");
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(SCRIPT)
            .arg("gcp-setup.sh")
            .args(args)
            .env("PATH", path)
            .env("GCLOUD_LOG", &log)
            .output()
            .unwrap();
        (out, std::fs::read_to_string(log).unwrap_or_default())
    }

    #[test]
    fn the_setup_makes_a_keyless_account_and_lets_the_user_act_as_it() {
        let dir = tempfile::tempdir().unwrap();
        stub(dir.path());
        let (out, calls) = run(dir.path(), &["my-project-1"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(calls.contains("iam roles create tasteIde --project=my-project-1"));
        assert!(calls.contains("iam service-accounts create taste-ide --project=my-project-1"));
        assert!(calls.contains(
            "projects add-iam-policy-binding my-project-1 --condition=None --role=projects/my-project-1/roles/tasteIde --member=serviceAccount:taste-ide@my-project-1.iam.gserviceaccount.com"
        ));
        assert!(calls.contains(
            "--role=roles/iam.serviceAccountTokenCreator --member=user:david@example.com"
        ));
        assert!(!calls.contains("keys create"), "no key is ever made");
        let enabled = calls
            .lines()
            .find(|l| l.starts_with("services enable"))
            .unwrap();
        for service in SERVICES {
            assert!(enabled.contains(service), "{service}");
        }
    }

    #[test]
    fn remove_takes_the_account_away_and_lists_what_is_left() {
        let dir = tempfile::tempdir().unwrap();
        stub(dir.path());
        let (out, calls) = run(dir.path(), &["--remove", "my-project-1"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(calls.contains(
            "iam service-accounts delete taste-ide@my-project-1.iam.gserviceaccount.com"
        ));
        assert!(calls.contains("compute instances list"));
        assert!(calls.contains("compute disks list"));
    }

    #[test]
    fn a_bad_project_id_never_reaches_gcloud() {
        let dir = tempfile::tempdir().unwrap();
        stub(dir.path());
        let (out, calls) = run(dir.path(), &["my-project-1;rm"]);
        assert!(!out.status.success());
        assert!(calls.is_empty());
    }
}

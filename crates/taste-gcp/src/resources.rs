//! A workspace's GCP resources as Compute, Cloud DNS, and Cloud Storage
//! request bodies.
//!
//! Everything here is a pure function from a typed spec to the JSON the
//! API takes, so what the IDE is about to create can be read, and
//! asserted on, before anything is created. One naming convention covers
//! every resource — `taste-<workspace>-<part>` — and one label covers
//! every resource that takes labels, so "delete everything this workspace
//! made" is a filter rather than a list somebody has to keep.
//!
//! Some hardening is not a parameter at all, because neither consumer of
//! this crate wants it off: an instance never carries a service account
//! (so its metadata server mints no credentials), never takes project-wide
//! SSH keys, never has OS Login or the serial console, and always has guest
//! attributes, which is how a VM reports to the IDE without a network path
//! of its own.

use anyhow::{bail, Result};
use serde_json::{json, Value};

/// The label every labelled resource carries, valued with the workspace id.
pub const WORKSPACE_LABEL: &str = "taste-workspace";
/// The label naming what a resource is for (`serve`, `stage`, `weights`, …).
pub const ROLE_LABEL: &str = "taste-role";

/// GCP resource names: a lowercase letter, then lowercase letters, digits,
/// and hyphens, not ending in a hyphen, 63 characters at most.
fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && !name.ends_with('-')
}

/// One workspace's identity in GCP: the eight hex digits of the root's
/// hash that already name its state directory
/// (`taste_core::state::workspace_state_dir`), so the two are recognisably
/// the same workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    id: String,
}

impl Workspace {
    pub fn new(id: &str) -> Result<Self> {
        if id.len() != 8 || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            bail!("a workspace id is eight lowercase hex digits, not {id:?}");
        }
        Ok(Self { id: id.to_string() })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// `taste-<id>-<part>`, checked against GCP's naming rule so a part
    /// that would make an invalid name fails here rather than at the API.
    pub fn name(&self, part: &str) -> Result<String> {
        let name = format!("taste-{}-{part}", self.id);
        if !valid_name(&name) {
            bail!("{name:?} is not a valid GCP resource name");
        }
        Ok(name)
    }

    /// The labels for a resource of this workspace playing `role`.
    pub fn labels(&self, role: &str) -> Value {
        json!({ WORKSPACE_LABEL: self.id, ROLE_LABEL: role })
    }

    /// The description given to resources that take no labels (networks,
    /// subnets, firewall rules, DNS policies), which is how a person in the
    /// console tells whose they are.
    pub fn description(&self, what: &str) -> String {
        format!("taste-ide workspace {}: {what}", self.id)
    }
}

/// Where a workspace's resources live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub project: String,
    pub region: String,
    pub zone: String,
}

impl Location {
    /// From a project and a zone; the region is the zone less its last
    /// `-<letter>`, which is how every GCP zone is spelled.
    pub fn new(project: &str, zone: &str) -> Result<Self> {
        let Some((region, suffix)) = zone.rsplit_once('-') else {
            bail!("{zone:?} is not a zone");
        };
        if suffix.len() != 1 || !suffix.as_bytes()[0].is_ascii_lowercase() || !region.contains('-')
        {
            bail!("{zone:?} is not a zone");
        }
        Ok(Self {
            project: project.to_string(),
            region: region.to_string(),
            zone: zone.to_string(),
        })
    }

    pub fn network_url(&self, network: &str) -> String {
        format!("projects/{}/global/networks/{network}", self.project)
    }

    /// The full URL Cloud DNS wants when it names a network, which unlike
    /// Compute's own fields does not accept a partial path.
    pub fn network_full_url(&self, network: &str) -> String {
        format!(
            "https://www.googleapis.com/compute/v1/{}",
            self.network_url(network)
        )
    }

    pub fn subnetwork_url(&self, subnetwork: &str) -> String {
        format!(
            "projects/{}/regions/{}/subnetworks/{subnetwork}",
            self.project, self.region
        )
    }

    pub fn machine_type_url(&self, machine_type: &str) -> String {
        format!("zones/{}/machineTypes/{machine_type}", self.zone)
    }

    pub fn disk_type_url(&self, disk_type: &str) -> String {
        format!("zones/{}/diskTypes/{disk_type}", self.zone)
    }

    pub fn disk_url(&self, disk: &str) -> String {
        format!("zones/{}/disks/{disk}", self.zone)
    }
}

/// A custom-mode VPC network: no automatic subnets, so nothing exists in
/// it that the workspace did not ask for.
pub fn network(ws: &Workspace, name: &str, what: &str) -> Value {
    json!({
        "name": name,
        "description": ws.description(what),
        "autoCreateSubnetworks": false,
        "routingConfig": { "routingMode": "REGIONAL" },
    })
}

/// A subnet with Private Google Access off and no IPv6, so the only way
/// out of it is whatever the firewall and an external address allow.
pub fn subnetwork(ws: &Workspace, loc: &Location, name: &str, network: &str, cidr: &str) -> Value {
    json!({
        "name": name,
        "description": ws.description("subnet"),
        "network": loc.network_url(network),
        "region": loc.region,
        "ipCidrRange": cidr,
        "privateIpGoogleAccess": false,
        "stackType": "IPV4_ONLY",
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Ingress,
    Egress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Allow,
    Deny,
}

/// What a firewall rule matches: every protocol, or TCP to some ports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Traffic {
    All,
    Tcp(Vec<u16>),
}

/// One firewall rule. Lower `priority` wins; at equal priority a deny
/// beats an allow, which is why a deny-all at 0 cannot be overridden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirewallSpec {
    pub name: String,
    pub network: String,
    pub direction: Direction,
    pub action: Action,
    pub priority: u16,
    /// Source ranges for an ingress rule, destination ranges for egress.
    pub ranges: Vec<String>,
    pub traffic: Traffic,
    pub what: &'static str,
}

pub fn firewall(ws: &Workspace, loc: &Location, spec: &FirewallSpec) -> Value {
    let matched = match &spec.traffic {
        Traffic::All => json!([{ "IPProtocol": "all" }]),
        Traffic::Tcp(ports) => json!([{
            "IPProtocol": "tcp",
            "ports": ports.iter().map(u16::to_string).collect::<Vec<_>>(),
        }]),
    };
    let mut body = json!({
        "name": spec.name,
        "description": ws.description(spec.what),
        "network": loc.network_url(&spec.network),
        "priority": spec.priority,
        "direction": match spec.direction {
            Direction::Ingress => "INGRESS",
            Direction::Egress => "EGRESS",
        },
    });
    let ranges_key = match spec.direction {
        Direction::Ingress => "sourceRanges",
        Direction::Egress => "destinationRanges",
    };
    body[ranges_key] = json!(spec.ranges);
    let action_key = match spec.action {
        Action::Allow => "allowed",
        Action::Deny => "denied",
    };
    body[action_key] = matched;
    body
}

/// A Cloud DNS server policy that forwards every query from `network` to
/// `target`, an address nothing answers on. The metadata server answers
/// DNS and no firewall rule reaches it, so this — not the firewall — is
/// what stops a VM resolving names, and with them a DNS-shaped way out.
pub fn dns_blackhole_policy(
    ws: &Workspace,
    loc: &Location,
    name: &str,
    network: &str,
    target: &str,
) -> Value {
    json!({
        "name": name,
        "description": ws.description("every DNS query forwarded to an address nothing answers on"),
        "enableInboundForwarding": false,
        "enableLogging": false,
        "networks": [{ "networkUrl": loc.network_full_url(network) }],
        "alternativeNameServerConfig": {
            "targetNameServers": [{ "ipv4Address": target, "forwardingPath": "private" }],
        },
    })
}

/// A Hyperdisk Balanced volume's provisioned performance. The first 3,000
/// IOPS and 140 MiB/s are free; anything above is billed for every hour
/// it is held, stopped or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskPerformance {
    pub iops: u32,
    pub throughput_mibps: u32,
}

impl DiskPerformance {
    /// The free baseline.
    pub const BASELINE: DiskPerformance = DiskPerformance {
        iops: 3000,
        throughput_mibps: 140,
    };
}

pub const HYPERDISK_BALANCED: &str = "hyperdisk-balanced";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskSpec {
    pub name: String,
    pub size_gib: u64,
    pub performance: DiskPerformance,
    pub role: &'static str,
}

pub fn disk(ws: &Workspace, loc: &Location, spec: &DiskSpec) -> Value {
    json!({
        "name": spec.name,
        "sizeGb": spec.size_gib.to_string(),
        "type": loc.disk_type_url(HYPERDISK_BALANCED),
        "provisionedIops": spec.performance.iops.to_string(),
        "provisionedThroughput": spec.performance.throughput_mibps.to_string(),
        "labels": ws.labels(spec.role),
    })
}

/// A regional Cloud Storage bucket that only IAM can open: uniform
/// access (no object ACLs), public access prevented, and no soft delete,
/// since otherwise every object replaced or removed is billed for seven
/// days more. It belongs to the project rather than one workspace, so it
/// carries the role label alone.
pub fn bucket(name: &str, region: &str) -> Value {
    json!({
        "name": name,
        "location": region.to_ascii_uppercase(),
        "storageClass": "STANDARD",
        "iamConfiguration": {
            "uniformBucketLevelAccess": { "enabled": true },
            "publicAccessPrevention": "enforced",
        },
        "softDeletePolicy": { "retentionDurationSeconds": "0" },
        "labels": { ROLE_LABEL: "weights" },
    })
}

/// A public image, by project and name — for Fedora CoreOS, the GCP entry
/// the pinned stream release names (`taste_devcontainer::guest`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub project: String,
    pub name: String,
}

impl ImageRef {
    pub fn url(&self) -> String {
        format!("projects/{}/global/images/{}", self.project, self.name)
    }
}

/// A disk the instance attaches that it did not create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attached {
    pub disk: String,
    /// What the guest sees it as: `/dev/disk/by-id/google-<device_name>`.
    pub device_name: String,
    pub read_only: bool,
}

/// What GCP does with a running instance when its host needs maintenance.
/// Instances with GPUs cannot be live-migrated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Maintenance {
    Migrate,
    Terminate,
}

/// What GCP does when an instance's maximum run duration is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnMaxRun {
    Stop,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSpec {
    pub name: String,
    pub role: &'static str,
    pub machine_type: String,
    pub image: ImageRef,
    pub boot_disk_gib: u64,
    pub network: String,
    pub subnetwork: String,
    /// An ephemeral external address. Without one, and with no Cloud NAT,
    /// an instance can neither be reached from nor reach the internet.
    pub external_address: bool,
    pub attached: Vec<Attached>,
    pub maintenance: Maintenance,
    /// The longest one run may last, enforced by GCP even against a hung
    /// guest, and what happens then.
    pub max_run: Option<(u64, OnMaxRun)>,
    /// Instance metadata beyond the fixed hardening keys — Ignition's
    /// `user-data`, and whatever the IDE tells the guest.
    pub metadata: Vec<(String, String)>,
}

/// Metadata every instance carries, whatever it is for.
pub const HARDENING_METADATA: &[(&str, &str)] = &[
    ("enable-guest-attributes", "TRUE"),
    ("block-project-ssh-keys", "TRUE"),
    ("enable-oslogin", "FALSE"),
    ("serial-port-enable", "FALSE"),
];

pub fn instance(ws: &Workspace, loc: &Location, spec: &InstanceSpec) -> Value {
    let mut disks = vec![json!({
        "boot": true,
        "autoDelete": true,
        "deviceName": "boot",
        "initializeParams": {
            "sourceImage": spec.image.url(),
            "diskSizeGb": spec.boot_disk_gib.to_string(),
            "diskType": loc.disk_type_url(HYPERDISK_BALANCED),
            "labels": ws.labels(spec.role),
        },
    })];
    for attached in &spec.attached {
        disks.push(json!({
            "source": loc.disk_url(&attached.disk),
            "deviceName": attached.device_name,
            "mode": if attached.read_only { "READ_ONLY" } else { "READ_WRITE" },
            "autoDelete": false,
        }));
    }

    let mut interface = json!({
        "network": loc.network_url(&spec.network),
        "subnetwork": loc.subnetwork_url(&spec.subnetwork),
        "stackType": "IPV4_ONLY",
    });
    if spec.external_address {
        interface["accessConfigs"] = json!([{ "type": "ONE_TO_ONE_NAT", "name": "external" }]);
    }

    let items: Vec<Value> = HARDENING_METADATA
        .iter()
        .map(|(key, value)| json!({ "key": key, "value": value }))
        .chain(
            spec.metadata
                .iter()
                .map(|(key, value)| json!({ "key": key, "value": value })),
        )
        .collect();

    let mut scheduling = json!({
        "provisioningModel": "STANDARD",
        "onHostMaintenance": match spec.maintenance {
            Maintenance::Migrate => "MIGRATE",
            Maintenance::Terminate => "TERMINATE",
        },
        // A VM that GCP stopped is not restarted behind the user's back:
        // the next request starts it, and the ledger sees that start.
        "automaticRestart": false,
    });
    if let Some((seconds, action)) = spec.max_run {
        scheduling["maxRunDuration"] = json!({ "seconds": seconds.to_string() });
        scheduling["instanceTerminationAction"] = json!(match action {
            OnMaxRun::Stop => "STOP",
            OnMaxRun::Delete => "DELETE",
        });
    }

    json!({
        "name": spec.name,
        "machineType": loc.machine_type_url(&spec.machine_type),
        "labels": ws.labels(spec.role),
        "disks": disks,
        "networkInterfaces": [interface],
        "serviceAccounts": [],
        "metadata": { "items": items },
        "scheduling": scheduling,
        "shieldedInstanceConfig": {
            "enableSecureBoot": true,
            "enableVtpm": true,
            "enableIntegrityMonitoring": true,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_ids_are_eight_hex_digits() {
        assert!(Workspace::new("0a1b2c3d").is_ok());
        assert!(Workspace::new("0A1B2C3D").is_err());
        assert!(Workspace::new("0a1b2c3").is_err());
        assert!(Workspace::new("0a1b2c3g").is_err());
    }

    #[test]
    fn names_follow_the_gcp_rule() {
        let ws = Workspace::new("0a1b2c3d").unwrap();
        assert_eq!(ws.name("serve").unwrap(), "taste-0a1b2c3d-serve");
        assert!(ws.name("Serve").is_err());
        assert!(ws.name("serve-").is_err());
        assert!(ws.name(&"x".repeat(48)).is_ok());
        assert!(ws.name(&"x".repeat(49)).is_err());
    }

    #[test]
    fn a_zone_names_its_region() {
        let loc = Location::new("p", "us-central1-a").unwrap();
        assert_eq!(loc.region, "us-central1");
        assert!(Location::new("p", "us-central1").is_err());
        assert!(Location::new("p", "central-1a").is_err());
    }

    #[test]
    fn an_instance_has_no_service_account_and_the_fixed_hardening() {
        let ws = Workspace::new("0a1b2c3d").unwrap();
        let loc = Location::new("p", "us-central1-a").unwrap();
        let body = instance(
            &ws,
            &loc,
            &InstanceSpec {
                name: "taste-0a1b2c3d-x".into(),
                role: "x",
                machine_type: "n4-standard-2".into(),
                image: ImageRef {
                    project: "fedora-coreos-cloud".into(),
                    name: "fedora-coreos-44-20260829-3-1-gcp-x86-64".into(),
                },
                boot_disk_gib: 20,
                network: "n".into(),
                subnetwork: "s".into(),
                external_address: false,
                attached: vec![],
                maintenance: Maintenance::Migrate,
                max_run: None,
                metadata: vec![],
            },
        );
        assert_eq!(body["serviceAccounts"], json!([]));
        assert!(body["networkInterfaces"][0].get("accessConfigs").is_none());
        let items = body["metadata"]["items"].as_array().unwrap();
        for (key, value) in HARDENING_METADATA {
            assert!(
                items
                    .iter()
                    .any(|i| i["key"] == *key && i["value"] == *value),
                "{key} missing"
            );
        }
        assert_eq!(body["scheduling"]["automaticRestart"], json!(false));
        assert_eq!(
            body["disks"][0]["initializeParams"]["sourceImage"],
            "projects/fedora-coreos-cloud/global/images/fedora-coreos-44-20260829-3-1-gcp-x86-64"
        );
    }

    #[test]
    fn a_firewall_rule_puts_ranges_and_traffic_on_the_right_side() {
        let ws = Workspace::new("0a1b2c3d").unwrap();
        let loc = Location::new("p", "us-central1-a").unwrap();
        let body = firewall(
            &ws,
            &loc,
            &FirewallSpec {
                name: "r".into(),
                network: "n".into(),
                direction: Direction::Egress,
                action: Action::Deny,
                priority: 0,
                ranges: vec!["0.0.0.0/0".into()],
                traffic: Traffic::All,
                what: "deny",
            },
        );
        assert_eq!(body["destinationRanges"], json!(["0.0.0.0/0"]));
        assert_eq!(body["denied"], json!([{ "IPProtocol": "all" }]));
        assert!(body.get("sourceRanges").is_none());
        assert!(body.get("allowed").is_none());
    }
}

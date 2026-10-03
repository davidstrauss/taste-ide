//! Environment hosts: the workspace's pool, in GCP (ENVIRONMENTS → "VM
//! provisioners", "Phase 3 — cloud provisioners").
//!
//! A host is the local pool's VM made somewhere else: the same Fedora
//! CoreOS, configured by the same Ignition (written by
//! `taste_devcontainer::provision::ignition`, handed in here as
//! `user-data`), holding environments the same way. What differs is only
//! how it is made and how it is reached, and both are here:
//!
//! - **Made** as a GCE instance on the workspace's own environment
//!   network: no service account, the fixed hardening, an ephemeral
//!   external address for egress, and a persistent boot disk that holds
//!   the checkouts, the images, and the volumes, and outlives a stop.
//! - **Reached** only through Identity-Aware Proxy: the network admits
//!   IAP's forwarders to port 22 and nothing else, so the host faces the
//!   internet with nothing, and the IDE dials ssh through `gcloud compute
//!   start-iap-tunnel` on a loopback port — after which it is, to every
//!   line above the substrate, a VM on `127.0.0.1:<port>` like any other.
//!
//! Environments get egress — the isolation standard grants a project the
//! internet (ENVIRONMENTS → "Isolation: the standard, and what meets
//! it") — so unlike the model's machine, this network carries no deny.

use anyhow::{Context, Result};
use http::Method;
use serde_json::Value;

use crate::resources::{
    self, Action, Direction, FirewallSpec, ImageRef, InstanceSpec, Location, Maintenance, Traffic,
    Workspace,
};
use crate::rest::Gcp;

/// The environment network's one subnet. Beside the model's, never over
/// them, so a project can hold both.
const HOSTS_CIDR: &str = "10.231.3.0/24";

/// The machine a host is made on, then the ones tried while a zone has
/// none of it to give: eight vCPUs and about 32 GB each, so a host holds
/// three environments at the default grant beside the guest's reserve.
pub const HOST_MACHINE: &str = "n4-standard-8";
pub const HOST_FALLBACKS: &[&str] = crate::model::SMALL_MACHINES;

/// The boot disk, which is the host's only disk: images, checkouts, and
/// volumes. Billed while the host is stopped, at about $8 a month.
pub const HOST_DISK_GIB: u64 = 100;

/// The role label every host carries, which is how a project's hosts are
/// told from its other instances.
pub const HOST_ROLE: &str = "env-host";

/// Every name the environment network uses, derived once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostNames {
    pub network: String,
    pub subnetwork: String,
    pub allow_ssh: String,
}

impl HostNames {
    pub fn new(ws: &Workspace) -> Result<Self> {
        Ok(Self {
            network: ws.name("envs")?,
            subnetwork: ws.name("envs")?,
            allow_ssh: ws.name("envs-allow-iap-ssh")?,
        })
    }
}

/// The one way in: IAP's forwarders to sshd. Everything else inbound is
/// refused by GCP's implied rule.
pub fn ssh_rule(names: &HostNames) -> FirewallSpec {
    FirewallSpec {
        name: names.allow_ssh.clone(),
        network: names.network.clone(),
        direction: Direction::Ingress,
        action: Action::Allow,
        priority: 1000,
        ranges: vec![crate::model::IAP_RANGE.to_string()],
        traffic: Traffic::Tcp(vec![22]),
        what: "the IDE's ssh to an environment host, through IAP only",
    }
}

/// Create the environment network, its subnet, and its one rule, each
/// repeatably.
pub async fn ensure_network(gcp: &Gcp, ws: &Workspace, loc: &Location) -> Result<HostNames> {
    let names = HostNames::new(ws)?;
    let project = &loc.project;
    gcp.ensure_compute(
        &format!("projects/{project}/global/networks"),
        &resources::network(ws, &names.network, "the environment hosts' network"),
    )
    .await?;
    gcp.ensure_compute(
        &format!("projects/{project}/regions/{}/subnetworks", loc.region),
        &resources::subnetwork(ws, loc, &names.subnetwork, &names.network, HOSTS_CIDR),
    )
    .await?;
    gcp.ensure_compute(
        &format!("projects/{project}/global/firewalls"),
        &resources::firewall(ws, loc, &ssh_rule(&names)),
    )
    .await?;
    Ok(names)
}

/// One host's request body, for `machine` in `loc`'s zone.
pub fn host_instance(
    ws: &Workspace,
    loc: &Location,
    names: &HostNames,
    name: &str,
    image: &ImageRef,
    user_data: &str,
) -> Value {
    resources::instance(
        ws,
        loc,
        &InstanceSpec {
            name: name.to_string(),
            role: HOST_ROLE,
            machine_type: HOST_MACHINE.to_string(),
            image: image.clone(),
            boot_disk_gib: HOST_DISK_GIB,
            network: names.network.clone(),
            subnetwork: names.subnetwork.clone(),
            // Its way out: environments reach the internet, there being no
            // Cloud NAT. Nothing comes in by it — no rule admits it.
            external_address: true,
            attached: Vec::new(),
            maintenance: Maintenance::Migrate,
            // No ceiling on a run: an environment works for as long as it
            // works. The guest powers itself off once the IDE has been gone
            // a while, and the IDE stops a host nothing runs in.
            max_run: None,
            metadata: vec![("user-data".to_string(), user_data.to_string())],
        },
    )
}

/// The zones of `region`, as Compute lists them, in name order: the order
/// a host is tried in when a zone has no machine to give.
pub async fn zones(gcp: &Gcp, project: &str, region: &str) -> Result<Vec<String>> {
    let url = format!(
        "{}/projects/{project}/regions/{region}",
        gcp.endpoints.compute
    );
    let answer = gcp.call(Method::GET, &url, None).await?;
    let mut zones: Vec<String> = answer["zones"]
        .as_array()
        .map(|zones| {
            zones
                .iter()
                .filter_map(|z| z.as_str()?.rsplit('/').next().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    zones.sort();
    anyhow::ensure!(!zones.is_empty(), "region {region} lists no zones");
    Ok(zones)
}

/// What a machine type gives: vCPUs and memory, as Compute states them.
pub async fn machine_facts(gcp: &Gcp, loc: &Location, machine: &str) -> Result<(u32, u64)> {
    let url = format!(
        "{}/projects/{}/zones/{}/machineTypes/{machine}",
        gcp.endpoints.compute, loc.project, loc.zone
    );
    let answer = gcp.call(Method::GET, &url, None).await?;
    let cpus = answer["guestCpus"]
        .as_u64()
        .context("a machine type without guestCpus")?;
    let memory_mib = answer["memoryMb"]
        .as_u64()
        .context("a machine type without memoryMb")?;
    Ok((cpus as u32, memory_mib))
}

fn host_path(loc: &Location, name: &str) -> String {
    format!(
        "projects/{}/zones/{}/instances/{name}",
        loc.project, loc.zone
    )
}

/// What one host is: its name, its image, and its Ignition.
#[derive(Debug, Clone)]
pub struct HostRequest<'a> {
    pub name: &'a str,
    pub image: &'a ImageRef,
    pub user_data: &'a str,
}

/// Make a host in the first zone of `zones` that can supply one of the
/// host machines, and say which zone and machine it got.
pub async fn create(
    gcp: &Gcp,
    ws: &Workspace,
    project: &str,
    zones: &[String],
    host: &HostRequest<'_>,
    report: &(dyn Fn(&str) + Sync),
) -> Result<(Location, String)> {
    let machines: Vec<&str> = std::iter::once(HOST_MACHINE)
        .chain(
            HOST_FALLBACKS
                .iter()
                .copied()
                .filter(|m| *m != HOST_MACHINE),
        )
        .collect();
    let mut last = None;
    for zone in zones {
        let loc = Location::new(project, zone)?;
        let names = ensure_network(gcp, ws, &loc).await?;
        let body = host_instance(ws, &loc, &names, host.name, host.image, host.user_data);
        match crate::lifecycle::create_instance(gcp, &loc, &body, &machines, report).await {
            Ok(machine) => return Ok((loc, machine)),
            Err(e) if format!("{e:#}").contains("could supply none of") => {
                report(&format!(
                    "{zone} has none of the host machines; trying the next zone"
                ));
                last = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no zone to try")))
}

/// The host's status (`RUNNING`, `TERMINATED`, …), or `None` if it is gone.
pub async fn status(gcp: &Gcp, loc: &Location, name: &str) -> Result<Option<String>> {
    crate::lifecycle::status(gcp, loc, name).await
}

/// Start a stopped host.
pub async fn start(gcp: &Gcp, loc: &Location, name: &str) -> Result<()> {
    gcp.act_compute(&host_path(loc, name), "start", None).await
}

/// Stop a running host. Its disk stays, and so does everything on it.
pub async fn stop(gcp: &Gcp, loc: &Location, name: &str) -> Result<()> {
    gcp.act_compute(&host_path(loc, name), "stop", None).await
}

/// Delete a host and its disk. Already gone is not an error.
pub async fn delete(gcp: &Gcp, loc: &Location, name: &str) -> Result<()> {
    gcp.remove_compute(&host_path(loc, name)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (Workspace, Location, HostNames) {
        let ws = Workspace::new("0a1b2c3d").unwrap();
        let loc = Location::new("proj", "us-central1-a").unwrap();
        let names = HostNames::new(&ws).unwrap();
        (ws, loc, names)
    }

    #[test]
    fn a_host_is_reached_through_iap_alone() {
        let (ws, loc, names) = fixture();
        let rule = resources::firewall(&ws, &loc, &ssh_rule(&names));
        assert_eq!(rule["direction"], "INGRESS");
        assert_eq!(rule["sourceRanges"], json!([crate::model::IAP_RANGE]));
        assert_eq!(
            rule["allowed"],
            json!([{ "IPProtocol": "tcp", "ports": ["22"] }])
        );
    }

    #[test]
    fn a_host_has_no_account_a_disk_that_outlives_a_stop_and_its_ignition() {
        let (ws, loc, names) = fixture();
        let image = crate::guest::fcos_image("44.20260829.3.1");
        let body = host_instance(&ws, &loc, &names, "taste-0a1b2c3d-env-x1y2z3", &image, "{}");
        assert_eq!(body["serviceAccounts"], json!([]));
        assert_eq!(body["labels"][resources::ROLE_LABEL], HOST_ROLE);
        let disks = body["disks"].as_array().unwrap();
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0]["initializeParams"]["diskSizeGb"], "100");
        // Egress by its own address; nothing admitted by it.
        assert!(body["networkInterfaces"][0].get("accessConfigs").is_some());
        assert!(body["scheduling"].get("maxRunDuration").is_none());
        let items = body["metadata"]["items"].as_array().unwrap();
        assert!(items
            .iter()
            .any(|i| i["key"] == "user-data" && i["value"] == "{}"));
        assert!(items
            .iter()
            .any(|i| i["key"] == "block-project-ssh-keys" && i["value"] == "TRUE"));
    }

    #[test]
    fn hosts_get_their_own_network_beside_the_models() {
        let (ws, _, names) = fixture();
        let model = crate::model::Names::new(&ws).unwrap();
        assert_ne!(names.network, model.serve_network);
        assert_ne!(names.network, model.stage_network);
        assert_eq!(names.network, "taste-0a1b2c3d-envs");
    }
}

//! The GLM-5.3 machines: a staging VM that fetches the pinned weights,
//! and a serving VM with no way out that only the IDE can reach
//! (ENVIRONMENTS → "A model on a cloud VM"; the evidence for every number
//! here is `docs/spikes/glm-on-gcp.md`).
//!
//! The two live on separate networks because they need opposite DNS: the
//! staging VM has to resolve Hugging Face, and the serving VM must resolve
//! nothing, and a Cloud DNS server policy applies to a whole network.
//! The lockdown is stated here as data — rules, a policy, the absence of a
//! service account — and the tests read it back, so a change that opens a
//! way out fails a test before it can be created.

use anyhow::Result;
use serde_json::Value;

use crate::resources::{
    self, Action, Attached, Direction, DiskPerformance, DiskSpec, FirewallSpec, ImageRef,
    InstanceSpec, Location, Maintenance, OnMaxRun, Traffic, Workspace,
};

/// One file of the pinned weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shard {
    pub file: &'static str,
    pub bytes: u64,
    /// Hex SHA-256, from the Hub's tree API at the pinned commit.
    pub sha256: &'static str,
}

/// The weights, pinned by repository, commit, quant, and per-shard digest.
/// Moving the pin means re-running `build-aux/gguf-tensors.py` against
/// the new files, because llama.cpp runs a GGUF without the DSA indexer
/// tensors densely and without a word (the spike's "Checking that the
/// sparse path is the one that runs").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Weights {
    pub repo: &'static str,
    pub commit: &'static str,
    pub quant: &'static str,
    pub shards: &'static [Shard],
}

impl Weights {
    pub fn total_bytes(&self) -> u64 {
        self.shards.iter().map(|s| s.bytes).sum()
    }

    pub fn url(&self, shard: &Shard) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}/{}",
            self.repo, self.commit, self.quant, shard.file
        )
    }
}

#[rustfmt::skip]
pub const GLM_5_3_Q8_0: Weights = Weights {
    repo: "unsloth/GLM-5.3-GGUF",
    commit: "346b3591c7f28d1a23716f97a065ecf12ec14771",
    quant: "Q8_0",
    shards: &[
        Shard { file: "GLM-5.3-Q8_0-00001-of-00017.gguf", bytes: 48305489632, sha256: "7fe0aacb07c33f113aed536300da6fb7fb1cf09e6202d67b6c178a2b878f4644" },
        Shard { file: "GLM-5.3-Q8_0-00002-of-00017.gguf", bytes: 49105526048, sha256: "ccc253748f43c3167a3801ab2178cead3042f2248c4624297b95c5aa62180873" },
        Shard { file: "GLM-5.3-Q8_0-00003-of-00017.gguf", bytes: 49299773536, sha256: "4829543a4bd76e7fe82c87a188a83bcdf012ccd5f6ba22e328f0c06ed46165d3" },
        Shard { file: "GLM-5.3-Q8_0-00004-of-00017.gguf", bytes: 49065113632, sha256: "7a16851636b4e0fbbd59399b5231f5c84793d588091431b3801f11cd76589c41" },
        Shard { file: "GLM-5.3-Q8_0-00005-of-00017.gguf", bytes: 49065113600, sha256: "f901d41f1c57d921d90dfb9860e3a2ab236558379a07fd3ca2e06e779b3e7003" },
        Shard { file: "GLM-5.3-Q8_0-00006-of-00017.gguf", bytes: 48843319392, sha256: "5f8b0c68a93415868cd3b61e3d23c96370ce1269a0c6658750fe5833347acf55" },
        Shard { file: "GLM-5.3-Q8_0-00007-of-00017.gguf", bytes: 49065113600, sha256: "f9a43526c53ab912da0ac54372dfd5c71e0e0064ad2a7177031bc1ee697d1cbf" },
        Shard { file: "GLM-5.3-Q8_0-00008-of-00017.gguf", bytes: 49065113632, sha256: "90ef2b86ae70ee0f4ff8f04e4be041d8707f427c7329eda80b36bb61679bf1da" },
        Shard { file: "GLM-5.3-Q8_0-00009-of-00017.gguf", bytes: 48843319392, sha256: "8be38eb8deecb6f01c858b06d13bf2be3515ed4eb036cfc6fce589e1b1c45d9d" },
        Shard { file: "GLM-5.3-Q8_0-00010-of-00017.gguf", bytes: 49065113600, sha256: "3aafa3a4e42506356bafb16ce54ad13a344a42445f1f11421787747e5ebc6b5d" },
        Shard { file: "GLM-5.3-Q8_0-00011-of-00017.gguf", bytes: 49065113632, sha256: "a57b126586a31baee6463df66b006c54359892954021272c2191c25743d41d0d" },
        Shard { file: "GLM-5.3-Q8_0-00012-of-00017.gguf", bytes: 48883731776, sha256: "bbd9238c813c90d9a1645e22e38be5231ca1c056498569486eeb9566f7906ffa" },
        Shard { file: "GLM-5.3-Q8_0-00013-of-00017.gguf", bytes: 49065113632, sha256: "54089f96058c3f058227121a93cdd978d03601205c4893ec2f96d4c1f38924a7" },
        Shard { file: "GLM-5.3-Q8_0-00014-of-00017.gguf", bytes: 49065113600, sha256: "28157088d398e3564442985c2183f617b1bc4979420ce8767ea883ba803f6d68" },
        Shard { file: "GLM-5.3-Q8_0-00015-of-00017.gguf", bytes: 48843319392, sha256: "f8601930e67ca579556041f59356ae1ba66d8c9cae11252c3a60048038d7e2e6" },
        Shard { file: "GLM-5.3-Q8_0-00016-of-00017.gguf", bytes: 49155939904, sha256: "6c9b48a9d49082fa8fc329edf59a1db4eb90ef58f7e9cfcc6d06ac204cf023a7" },
        Shard { file: "GLM-5.3-Q8_0-00017-of-00017.gguf", bytes: 17556349216, sha256: "79cd14a05a4c1b867e7d56bf1faaea2f7a88b1063fd992e4f9983a6168f42cdd" },
    ],
};

/// A machine the model can be served from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Machine {
    pub name: &'static str,
    pub vcpus: u32,
    /// As GCP's machine-type listing states it.
    pub memory_gb: u32,
    pub gpus: u32,
}

impl Machine {
    /// GPUs cannot be live-migrated, so a host's maintenance terminates
    /// an instance that has them.
    pub fn maintenance(&self) -> Maintenance {
        if self.gpus > 0 {
            Maintenance::Terminate
        } else {
            Maintenance::Migrate
        }
    }
}

/// The bake-off's candidates, every one on-demand (Phase 2 measures cost
/// per task on each and keeps one).
#[rustfmt::skip]
pub const CANDIDATES: &[Machine] = &[
    Machine { name: "c4-highmem-192", vcpus: 192, memory_gb: 1488, gpus: 0 },
    Machine { name: "c4d-highmem-192", vcpus: 192, memory_gb: 1512, gpus: 0 },
    Machine { name: "g4-standard-192", vcpus: 192, memory_gb: 720, gpus: 4 },
];

/// The staging VM only downloads and hashes, so it is small.
pub const STAGING_MACHINE: &str = "n4-standard-8";

const STAGE_CIDR: &str = "10.231.1.0/24";
const SERVE_CIDR: &str = "10.231.2.0/24";
/// Where the serving network's DNS goes: inside the VPC's private range
/// but in no subnet, so no VM ever holds it and nothing answers.
pub const DNS_BLACKHOLE: &str = "10.231.255.254";
/// The one port the serving VM listens on to the world, and only for a
/// TLS handshake that presents this workspace's client certificate. 443
/// because it is the port a café's network least often blocks.
pub const TLS_PORT: u16 = 443;

/// 801 GB of weights on an XFS volume, with room to spare.
pub const WEIGHTS_DISK_GIB: u64 = 850;
pub const BOOT_DISK_GIB: u64 = 20;
/// What the weights disk is provisioned at while staging or loading.
/// Phase 2 settles both numbers, and whether the disk can drop to
/// [`DiskPerformance::BASELINE`] while the VM is stopped: at the free
/// 140 MiB/s a cold load takes ~95 minutes, and every MiB/s above it is
/// billed whether or not anything reads.
pub const WEIGHTS_LOADING: DiskPerformance = DiskPerformance {
    iops: 10_000,
    throughput_mibps: 2_400,
};
/// GCP's own ceiling on one run of the serving VM: ten hours, then STOP,
/// whatever the guest or the IDE is doing.
pub const SERVE_MAX_RUN_SECONDS: u64 = 10 * 60 * 60;
/// The staging VM deletes itself after six hours whatever happened; the
/// disk it filled is not auto-deleted with it.
pub const STAGE_MAX_RUN_SECONDS: u64 = 6 * 60 * 60;
/// The guest's name for the weights disk: `/dev/disk/by-id/google-weights`.
pub const WEIGHTS_DEVICE: &str = "weights";

/// Every resource name the model uses, derived once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Names {
    pub stage_network: String,
    pub stage_subnetwork: String,
    pub serve_network: String,
    pub serve_subnetwork: String,
    pub stage_allow_https: String,
    pub stage_deny_egress: String,
    pub serve_deny_egress: String,
    pub serve_allow_tls: String,
    pub serve_dns_policy: String,
    pub weights_disk: String,
    pub staging: String,
    pub serving: String,
}

impl Names {
    pub fn new(ws: &Workspace) -> Result<Self> {
        Ok(Self {
            stage_network: ws.name("stage")?,
            stage_subnetwork: ws.name("stage")?,
            serve_network: ws.name("serve")?,
            serve_subnetwork: ws.name("serve")?,
            stage_allow_https: ws.name("stage-allow-https")?,
            stage_deny_egress: ws.name("stage-deny-egress")?,
            serve_deny_egress: ws.name("serve-deny-egress")?,
            serve_allow_tls: ws.name("serve-allow-tls")?,
            serve_dns_policy: ws.name("serve-dns")?,
            weights_disk: ws.name("weights")?,
            staging: ws.name("stage")?,
            serving: ws.name("serve")?,
        })
    }
}

/// The firewall rules, staging's then serving's.
pub fn firewall_specs(names: &Names) -> Vec<FirewallSpec> {
    let everywhere = || vec!["0.0.0.0/0".to_string()];
    vec![
        // Staging reaches out on 443 and nothing else; nothing reaches it,
        // because no rule admits ingress and GCP's implied rule denies it.
        FirewallSpec {
            name: names.stage_allow_https.clone(),
            network: names.stage_network.clone(),
            direction: Direction::Egress,
            action: Action::Allow,
            priority: 1000,
            ranges: everywhere(),
            traffic: Traffic::Tcp(vec![443]),
            what: "staging fetches over HTTPS",
        },
        FirewallSpec {
            name: names.stage_deny_egress.clone(),
            network: names.stage_network.clone(),
            direction: Direction::Egress,
            action: Action::Deny,
            priority: 65000,
            ranges: everywhere(),
            traffic: Traffic::All,
            what: "staging reaches nothing else",
        },
        // Serving reaches nothing. A deny at priority 0 is above every
        // allow there could be, and wins a tie with one.
        FirewallSpec {
            name: names.serve_deny_egress.clone(),
            network: names.serve_network.clone(),
            direction: Direction::Egress,
            action: Action::Deny,
            priority: 0,
            ranges: everywhere(),
            traffic: Traffic::All,
            what: "the model's machine reaches nothing",
        },
        // From anywhere, because the IDE's address changes as the laptop
        // moves; what admits a connection is the client certificate, which
        // the TLS terminator demands before anything else happens.
        FirewallSpec {
            name: names.serve_allow_tls.clone(),
            network: names.serve_network.clone(),
            direction: Direction::Ingress,
            action: Action::Allow,
            priority: 1000,
            ranges: everywhere(),
            traffic: Traffic::Tcp(vec![TLS_PORT]),
            what: "mutual TLS from the IDE",
        },
    ]
}

/// Everything the model needs, as request bodies in creation order.
#[derive(Debug, Clone)]
pub struct ModelPlan {
    pub names: Names,
    pub networks: Vec<Value>,
    pub subnetworks: Vec<Value>,
    pub firewalls: Vec<Value>,
    pub dns_policy: Value,
    pub weights_disk: Value,
    pub staging: Value,
    pub serving: Value,
}

/// What the guests are told, which Phase 2 fills: each VM's Ignition
/// config as `user-data`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuestConfigs {
    pub staging_user_data: Option<String>,
    pub serving_user_data: Option<String>,
}

pub fn plan(
    ws: &Workspace,
    loc: &Location,
    image: &ImageRef,
    machine: &Machine,
    guests: &GuestConfigs,
) -> Result<ModelPlan> {
    let names = Names::new(ws)?;
    let user_data = |config: &Option<String>| -> Vec<(String, String)> {
        config
            .iter()
            .map(|c| ("user-data".to_string(), c.clone()))
            .collect()
    };
    let staging = InstanceSpec {
        name: names.staging.clone(),
        role: "stage",
        machine_type: STAGING_MACHINE.to_string(),
        image: image.clone(),
        boot_disk_gib: BOOT_DISK_GIB,
        network: names.stage_network.clone(),
        subnetwork: names.stage_subnetwork.clone(),
        // Its way out to Hugging Face, there being no Cloud NAT.
        external_address: true,
        attached: vec![Attached {
            disk: names.weights_disk.clone(),
            device_name: WEIGHTS_DEVICE.to_string(),
            read_only: false,
        }],
        maintenance: Maintenance::Migrate,
        max_run: Some((STAGE_MAX_RUN_SECONDS, OnMaxRun::Delete)),
        metadata: user_data(&guests.staging_user_data),
    };
    let serving = InstanceSpec {
        name: names.serving.clone(),
        role: "serve",
        machine_type: machine.name.to_string(),
        image: image.clone(),
        boot_disk_gib: BOOT_DISK_GIB,
        network: names.serve_network.clone(),
        subnetwork: names.serve_subnetwork.clone(),
        // The address the IDE dials. Outbound it goes nowhere: the
        // priority-0 deny sees to that.
        external_address: true,
        attached: vec![Attached {
            disk: names.weights_disk.clone(),
            device_name: WEIGHTS_DEVICE.to_string(),
            read_only: true,
        }],
        maintenance: machine.maintenance(),
        max_run: Some((SERVE_MAX_RUN_SECONDS, OnMaxRun::Stop)),
        metadata: user_data(&guests.serving_user_data),
    };
    Ok(ModelPlan {
        networks: vec![
            resources::network(ws, &names.stage_network, "the staging VM's network"),
            resources::network(ws, &names.serve_network, "the model's network"),
        ],
        subnetworks: vec![
            resources::subnetwork(
                ws,
                loc,
                &names.stage_subnetwork,
                &names.stage_network,
                STAGE_CIDR,
            ),
            resources::subnetwork(
                ws,
                loc,
                &names.serve_subnetwork,
                &names.serve_network,
                SERVE_CIDR,
            ),
        ],
        firewalls: firewall_specs(&names)
            .iter()
            .map(|spec| resources::firewall(ws, loc, spec))
            .collect(),
        dns_policy: resources::dns_blackhole_policy(
            ws,
            loc,
            &names.serve_dns_policy,
            &names.serve_network,
            DNS_BLACKHOLE,
        ),
        weights_disk: resources::disk(
            ws,
            loc,
            &DiskSpec {
                name: names.weights_disk.clone(),
                size_gib: WEIGHTS_DISK_GIB,
                performance: WEIGHTS_LOADING,
                role: "weights",
            },
        ),
        staging: resources::instance(ws, loc, &staging),
        serving: resources::instance(ws, loc, &serving),
        names,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(machine: &Machine) -> ModelPlan {
        let ws = Workspace::new("0a1b2c3d").unwrap();
        let loc = Location::new("proj", "us-central1-a").unwrap();
        let image = ImageRef {
            project: "fedora-coreos-cloud".into(),
            name: "fedora-coreos-44-20260829-3-1-gcp-x86-64".into(),
        };
        plan(&ws, &loc, &image, machine, &GuestConfigs::default()).unwrap()
    }

    fn rules_on<'a>(plan: &'a ModelPlan, network: &str) -> Vec<&'a Value> {
        let url = format!("projects/proj/global/networks/{network}");
        plan.firewalls
            .iter()
            .filter(|r| r["network"] == url)
            .collect()
    }

    #[test]
    fn the_pin_adds_up_to_the_spikes_total() {
        assert_eq!(GLM_5_3_Q8_0.shards.len(), 17);
        assert_eq!(GLM_5_3_Q8_0.total_bytes(), 801_357_677_216);
        assert!(GLM_5_3_Q8_0.total_bytes() < WEIGHTS_DISK_GIB << 30);
        for shard in GLM_5_3_Q8_0.shards {
            assert_eq!(shard.sha256.len(), 64, "{}", shard.file);
        }
    }

    #[test]
    fn serving_reaches_nothing_and_nothing_overrides_that() {
        let plan = fixture(&CANDIDATES[0]);
        let rules = rules_on(&plan, &plan.names.serve_network);
        let deny = rules
            .iter()
            .find(|r| r["direction"] == "EGRESS" && r.get("denied").is_some())
            .expect("a deny-all egress rule");
        assert_eq!(deny["priority"], json!(0));
        assert_eq!(deny["denied"], json!([{ "IPProtocol": "all" }]));
        assert_eq!(deny["destinationRanges"], json!(["0.0.0.0/0"]));
        assert!(
            !rules
                .iter()
                .any(|r| r["direction"] == "EGRESS" && r.get("allowed").is_some()),
            "no egress allow on the serving network"
        );
    }

    #[test]
    fn serving_admits_only_tls_and_never_ssh() {
        let plan = fixture(&CANDIDATES[0]);
        let ingress: Vec<_> = rules_on(&plan, &plan.names.serve_network)
            .into_iter()
            .filter(|r| r["direction"] == "INGRESS")
            .collect();
        assert_eq!(ingress.len(), 1);
        assert_eq!(
            ingress[0]["allowed"],
            json!([{ "IPProtocol": "tcp", "ports": ["443"] }])
        );
        for rule in &plan.firewalls {
            assert!(!rule.to_string().contains("\"22\""), "{rule}");
        }
    }

    #[test]
    fn staging_reaches_only_https_and_admits_nothing() {
        let plan = fixture(&CANDIDATES[0]);
        let rules = rules_on(&plan, &plan.names.stage_network);
        assert!(!rules.iter().any(|r| r["direction"] == "INGRESS"));
        let allow = rules
            .iter()
            .find(|r| r.get("allowed").is_some())
            .expect("an HTTPS allow");
        let deny = rules
            .iter()
            .find(|r| r.get("denied").is_some())
            .expect("a deny below it");
        assert_eq!(
            allow["allowed"],
            json!([{ "IPProtocol": "tcp", "ports": ["443"] }])
        );
        assert!(allow["priority"].as_u64() < deny["priority"].as_u64());
        // Above GCP's implied allow-all egress at 65535.
        assert!(deny["priority"].as_u64().unwrap() < 65535);
    }

    #[test]
    fn serving_dns_goes_to_an_address_no_subnet_holds() {
        let plan = fixture(&CANDIDATES[0]);
        let target = &plan.dns_policy["alternativeNameServerConfig"]["targetNameServers"][0];
        assert_eq!(target["ipv4Address"], DNS_BLACKHOLE);
        assert_eq!(target["forwardingPath"], "private");
        assert_eq!(
            plan.dns_policy["networks"][0]["networkUrl"],
            format!(
                "https://www.googleapis.com/compute/v1/projects/proj/global/networks/{}",
                plan.names.serve_network
            )
        );
        for subnet in &plan.subnetworks {
            let cidr = subnet["ipCidrRange"].as_str().unwrap();
            let prefix = cidr.trim_end_matches("0/24");
            assert!(
                !DNS_BLACKHOLE.starts_with(prefix),
                "{cidr} holds the black hole"
            );
        }
    }

    #[test]
    fn no_subnet_has_private_google_access() {
        let plan = fixture(&CANDIDATES[0]);
        for subnet in &plan.subnetworks {
            assert_eq!(subnet["privateIpGoogleAccess"], json!(false));
        }
    }

    #[test]
    fn serving_holds_the_weights_read_only_and_staging_writes_them() {
        let plan = fixture(&CANDIDATES[0]);
        let weights = |vm: &Value| {
            vm["disks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["deviceName"] == WEIGHTS_DEVICE)
                .cloned()
                .unwrap()
        };
        assert_eq!(weights(&plan.serving)["mode"], "READ_ONLY");
        assert_eq!(weights(&plan.staging)["mode"], "READ_WRITE");
        assert_eq!(weights(&plan.serving)["autoDelete"], json!(false));
        assert_eq!(weights(&plan.staging)["autoDelete"], json!(false));
    }

    #[test]
    fn gcp_bounds_every_run() {
        let plan = fixture(&CANDIDATES[0]);
        assert_eq!(
            plan.serving["scheduling"]["maxRunDuration"]["seconds"],
            SERVE_MAX_RUN_SECONDS.to_string()
        );
        assert_eq!(
            plan.serving["scheduling"]["instanceTerminationAction"],
            "STOP"
        );
        assert_eq!(
            plan.staging["scheduling"]["instanceTerminationAction"],
            "DELETE"
        );
        assert_eq!(plan.serving["scheduling"]["provisioningModel"], "STANDARD");
    }

    #[test]
    fn a_gpu_machine_terminates_for_maintenance() {
        let gpu = CANDIDATES.iter().find(|m| m.gpus > 0).unwrap();
        assert_eq!(
            fixture(gpu).serving["scheduling"]["onHostMaintenance"],
            "TERMINATE"
        );
        let cpu = CANDIDATES.iter().find(|m| m.gpus == 0).unwrap();
        assert_eq!(
            fixture(cpu).serving["scheduling"]["onHostMaintenance"],
            "MIGRATE"
        );
    }

    #[test]
    fn every_labelled_resource_names_the_workspace() {
        let plan = fixture(&CANDIDATES[0]);
        for labelled in [&plan.weights_disk, &plan.staging, &plan.serving] {
            assert_eq!(labelled["labels"][resources::WORKSPACE_LABEL], "0a1b2c3d");
        }
    }
}

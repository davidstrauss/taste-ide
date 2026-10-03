//! A model's machines: a staging VM that fetches the pinned weights, and a
//! serving VM with no address, no way out, and one way in, IAP
//! (ENVIRONMENTS → "A model on a cloud VM"; the evidence for every number
//! here is `docs/spikes/glm-on-gcp.md`).
//!
//! Which model is a [`ModelSpec`]: GLM-5.3 is what the route is for, and
//! [`GPT_OSS_20B`] is the small one the whole path is proved with first
//! (David, 2026-10-03: "Let's start with a less ambitious model to test
//! that things can work") — the same staging, the same lockdown, the same
//! tunnel, at a sixtieth of the bytes and a cent of the time.
//!
//! The weights live in the project's bucket, mirrored there once from
//! Hugging Face, and nothing standing is a disk (the spike's "Weights
//! from GCS instead of a disk"): the serving VM pulls them into memory on
//! each boot through a **window** — Private Google Access on its subnet and
//! one egress rule to `private.googleapis.com` — that the IDE opens before
//! the VM exists and closes before the server starts. The window's rule is
//! [`window_firewall`], kept out of the standing rules so that what stands
//! is "reaches nothing", and what the window adds is one rule, created and
//! deleted.
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
    self, Action, Direction, FirewallSpec, ImageRef, InstanceSpec, Location, Maintenance, OnMaxRun,
    Traffic, Workspace,
};
use crate::rest::Gcp;

/// One file of the pinned weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shard {
    /// The file's name, which is also its name on the weights disk.
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
    /// The directory in the repository the shards are in; empty for the
    /// repository's root.
    pub dir: &'static str,
    pub shards: &'static [Shard],
}

impl Weights {
    pub fn total_bytes(&self) -> u64 {
        self.shards.iter().map(|s| s.bytes).sum()
    }

    /// The shard's name in the bucket: repository, commit, and file, so a
    /// new pin is mirrored beside the old one rather than over it.
    pub fn object(&self, shard: &Shard) -> String {
        let dir = if self.dir.is_empty() {
            String::new()
        } else {
            format!("{}/", self.dir)
        };
        format!("{}/{}/{dir}{}", self.repo, self.commit, shard.file)
    }

    /// The largest shard, which is what a staging VM's disk must hold at
    /// once.
    pub fn largest(&self) -> u64 {
        self.shards.iter().map(|s| s.bytes).max().unwrap_or(0)
    }

    pub fn url(&self, shard: &Shard) -> String {
        let dir = if self.dir.is_empty() {
            String::new()
        } else {
            format!("{}/", self.dir)
        };
        format!(
            "https://huggingface.co/{}/resolve/{}/{dir}{}",
            self.repo, self.commit, shard.file
        )
    }
}

#[rustfmt::skip]
pub const GLM_5_3_Q8_0: Weights = Weights {
    repo: "unsloth/GLM-5.3-GGUF",
    commit: "346b3591c7f28d1a23716f97a065ecf12ec14771",
    dir: "Q8_0",
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

/// The smoke test's weights: OpenAI's gpt-oss-20b in its native MXFP4,
/// the model this project's private-model route was first proved against,
/// so a working answer here is a known answer.
#[rustfmt::skip]
pub const GPT_OSS_20B_MXFP4: Weights = Weights {
    repo: "ggml-org/gpt-oss-20b-GGUF",
    commit: "ef9b12f2ff56c69cf32153a02784e7a3c88bf524",
    dir: "",
    shards: &[
        Shard { file: "gpt-oss-20b-MXFP4.gguf", bytes: 12109566624, sha256: "27cd6c432c7672cb812a92f611cf3ba7bbc35928262bb1e1253ff4ee6ae35901" },
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

/// Machines of one size, in order of preference, for when a zone cannot
/// supply the first: every one takes Hyperdisk, which is what the plan's
/// boot disks are, so the same request fits any of them
/// (`lifecycle::create_instance`).
pub const SMALL_MACHINES: &[&str] = &[
    "n4-standard-8",
    "c4-standard-8",
    "n4d-standard-8",
    "c4d-standard-8",
    // The generation before, which also takes Hyperdisk Balanced and is
    // less contended: us-central1-b had none of the four above to give on
    // 2026-10-03.
    "c3-standard-8",
    "c3d-standard-8",
];

/// The smoke test serves from the staging VM's shape: 32 GB holds a 12 GB
/// model and its cache, at about $0.38 an hour on demand.
pub const SMOKE_MACHINE: Machine = Machine {
    name: "n4-standard-8",
    vcpus: 8,
    memory_gb: 32,
    gpus: 0,
};

/// One model the route can run: its pinned weights, the machine it is
/// served from unless the caller picks another, and what llama-server is
/// told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelSpec {
    pub slug: &'static str,
    /// What the model is called where a person reads it.
    pub label: &'static str,
    pub weights: Weights,
    pub machine: Machine,
    /// Machines to try, in order, when a zone cannot supply `machine`.
    pub fallbacks: &'static [&'static str],
    /// llama-server's `-c`, which the context gauge measures against.
    pub context_tokens: u32,
}

/// What the route is for.
pub const GLM_5_3: ModelSpec = ModelSpec {
    slug: "glm-5-3",
    label: "GLM-5.3",
    weights: GLM_5_3_Q8_0,
    machine: CANDIDATES[0],
    fallbacks: &["c4d-highmem-192", "g4-standard-192"],
    context_tokens: 200_000,
};

/// The smoke test: every part of the route, with a model a cheap machine
/// can hold twice over — 12 GB of weights in memory, and the server's own
/// buffers beside them.
pub const GPT_OSS_20B: ModelSpec = ModelSpec {
    slug: "gpt-oss-20b",
    label: "gpt-oss-20b",
    weights: GPT_OSS_20B_MXFP4,
    machine: SMOKE_MACHINE,
    fallbacks: SMALL_MACHINES,
    context_tokens: 65_536,
};

const STAGE_CIDR: &str = "10.231.1.0/24";
const SERVE_CIDR: &str = "10.231.2.0/24";
/// Where the serving network's DNS goes: inside the VPC's private range
/// but in no subnet, so no VM ever holds it and nothing answers.
pub const DNS_BLACKHOLE: &str = "10.231.255.254";
/// llama-server's port on the serving VM's internal address, the one port
/// anything may reach, and only through IAP.
pub const SERVER_PORT: u16 = 8080;
/// Where Identity-Aware Proxy's TCP forwarding connects from: Google's
/// documented range for it, and the only source the serving VM admits.
pub const IAP_RANGE: &str = "35.235.240.0/20";

/// `private.googleapis.com`: the four addresses Private Google Access
/// answers on for every Google API, and the only place the window opens
/// to (<https://docs.cloud.google.com/vpc/docs/configure-private-google-access>).
pub const GOOGLE_APIS_RANGE: &str = "199.36.153.8/30";
/// The first of them, which the guest dials by address, its DNS being
/// the black hole.
pub const GOOGLE_APIS_ADDRESS: &str = "199.36.153.8";
/// The serving network's deny-all. Not 0, which is the window's: a deny
/// wins a tie, so the one allow that ever outranks it has to be above it.
pub const SERVE_DENY_PRIORITY: u16 = 1;

/// Where the machines go until the zone is one of the project's choices
/// (`project::CloudProject`): what the connection test checks the quotas
/// in.
pub const DEFAULT_REGION: &str = "us-central1";

pub const BOOT_DISK_GIB: u64 = 20;
/// The staging VM's boot disk: the system, the largest shard while it is
/// checked and uploaded, and the server image saved beside it.
pub fn staging_boot_gib(spec: &ModelSpec) -> u64 {
    BOOT_DISK_GIB + spec.weights.largest().div_ceil(1 << 30) + 4
}
/// GCP's own ceiling on one run of the serving VM: ten hours, then it is
/// deleted, whatever the guest or the IDE is doing. Nothing on it outlives
/// a boot — the weights are in memory — so a stop would keep only a disk
/// to pay for.
pub const SERVE_MAX_RUN_SECONDS: u64 = 10 * 60 * 60;
/// The staging VM deletes itself after six hours whatever happened.
pub const STAGE_MAX_RUN_SECONDS: u64 = 6 * 60 * 60;

/// The project's bucket for weights, shared by every workspace in it:
/// what is mirrored once is read by all of them. Bucket names are global,
/// and a project id is too.
pub fn bucket_name(project: &str) -> String {
    format!("taste-weights-{project}")
}

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
    pub serve_allow_iap: String,
    pub serve_window: String,
    pub serve_dns_policy: String,
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
            serve_allow_iap: ws.name("serve-allow-iap")?,
            serve_window: ws.name("serve-window")?,
            serve_dns_policy: ws.name("serve-dns")?,
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
        // Serving reaches nothing. The deny is above every allow but the
        // window's, which exists only while the weights load.
        FirewallSpec {
            name: names.serve_deny_egress.clone(),
            network: names.serve_network.clone(),
            direction: Direction::Egress,
            action: Action::Deny,
            priority: SERVE_DENY_PRIORITY,
            ranges: everywhere(),
            traffic: Traffic::All,
            what: "the model's machine reaches nothing",
        },
        // Only IAP's forwarders, and only to llama-server. IAP admits a
        // connection only for a principal IAM lets tunnel to this VM, so
        // the laptop's address never matters and the VM needs none of its
        // own.
        FirewallSpec {
            name: names.serve_allow_iap.clone(),
            network: names.serve_network.clone(),
            direction: Direction::Ingress,
            action: Action::Allow,
            priority: 1000,
            ranges: vec![IAP_RANGE.to_string()],
            traffic: Traffic::Tcp(vec![SERVER_PORT]),
            what: "llama-server, through IAP only",
        },
    ]
}

/// The window: the serving VM's one way out, to Google's APIs on 443,
/// while it pulls the weights. Created before the VM exists and deleted
/// before the server starts (`lifecycle::serve`); never part of the
/// standing rules.
pub fn window_firewall(names: &Names) -> FirewallSpec {
    FirewallSpec {
        name: names.serve_window.clone(),
        network: names.serve_network.clone(),
        direction: Direction::Egress,
        action: Action::Allow,
        priority: 0,
        ranges: vec![GOOGLE_APIS_RANGE.to_string()],
        traffic: Traffic::Tcp(vec![443]),
        what: "the weights, from the bucket, while they load",
    }
}

/// Everything the model needs, as request bodies in creation order.
#[derive(Debug, Clone)]
pub struct ModelPlan {
    pub names: Names,
    pub networks: Vec<Value>,
    pub subnetworks: Vec<Value>,
    pub firewalls: Vec<Value>,
    pub dns_policy: Value,
    /// The window's rule, made and removed around each load.
    pub window: Value,
    pub bucket: Value,
    pub staging: Value,
    pub serving: Value,
}

/// What the guests are told: each VM's Ignition config as `user-data`
/// (`guest`), and the key llama-server checks, which the serving VM reads
/// from its own metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuestConfigs {
    pub staging_user_data: Option<String>,
    pub serving_user_data: Option<String>,
    pub serving_key: Option<String>,
}

pub fn plan(
    ws: &Workspace,
    loc: &Location,
    image: &ImageRef,
    spec: &ModelSpec,
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
        boot_disk_gib: staging_boot_gib(spec),
        network: names.stage_network.clone(),
        subnetwork: names.stage_subnetwork.clone(),
        // Its way out to Hugging Face and the bucket, there being no
        // Cloud NAT.
        external_address: true,
        attached: Vec::new(),
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
        // None: the IDE reaches it through IAP, and with no address and no
        // NAT it has no route to the internet, before the deny even
        // applies. The window reaches Google's APIs by Private Google
        // Access, which needs no address either.
        external_address: false,
        // Nothing but its boot disk: the weights are in memory.
        attached: Vec::new(),
        maintenance: machine.maintenance(),
        max_run: Some((SERVE_MAX_RUN_SECONDS, OnMaxRun::Delete)),
        metadata: user_data(&guests.serving_user_data)
            .into_iter()
            .chain(guests.serving_user_data.iter().map(|config| {
                (
                    crate::guest::CONFIG_ATTRIBUTE.to_string(),
                    crate::guest::config_hash(config),
                )
            }))
            .chain(
                guests
                    .serving_key
                    .iter()
                    .map(|key| (crate::guest::KEY_ATTRIBUTE.to_string(), key.clone())),
            )
            .collect(),
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
        window: resources::firewall(ws, loc, &window_firewall(&names)),
        bucket: resources::bucket(&bucket_name(&loc.project), &loc.region),
        staging: resources::instance(ws, loc, &staging),
        serving: resources::instance(ws, loc, &serving),
        names,
    })
}

/// Create everything the machines stand on — networks, subnets, rules,
/// and the DNS black hole — in an order where no way out
/// exists even for a moment: each network's deny rule is created before
/// the rule that admits anything. Every step is an `ensure`, so running it
/// again finishes what a failure interrupted. It does not repair a
/// resource that exists but differs from the plan; that is the
/// reconciliation Phase 3 adds.
pub async fn ensure_foundation(gcp: &Gcp, loc: &Location, plan: &ModelPlan) -> Result<()> {
    let project = &loc.project;
    for network in &plan.networks {
        gcp.ensure_compute(&format!("projects/{project}/global/networks"), network)
            .await?;
    }
    for subnetwork in &plan.subnetworks {
        gcp.ensure_compute(
            &format!("projects/{project}/regions/{}/subnetworks", loc.region),
            subnetwork,
        )
        .await?;
    }
    let mut firewalls: Vec<&Value> = plan.firewalls.iter().collect();
    firewalls.sort_by_key(|rule| rule.get("allowed").is_some());
    for rule in firewalls {
        gcp.ensure_compute(&format!("projects/{project}/global/firewalls"), rule)
            .await?;
    }
    gcp.ensure_dns_policy(project, &plan.dns_policy).await?;
    Ok(())
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
        plan(
            &ws,
            &loc,
            &image,
            &GLM_5_3,
            machine,
            &GuestConfigs::default(),
        )
        .unwrap()
    }

    fn rules_on<'a>(plan: &'a ModelPlan, network: &str) -> Vec<&'a Value> {
        let url = format!("projects/proj/global/networks/{network}");
        plan.firewalls
            .iter()
            .filter(|r| r["network"] == url)
            .collect()
    }

    #[test]
    fn each_pin_has_its_own_objects() {
        assert_eq!(
            GPT_OSS_20B.weights.object(&GPT_OSS_20B.weights.shards[0]),
            "ggml-org/gpt-oss-20b-GGUF/ef9b12f2ff56c69cf32153a02784e7a3c88bf524/gpt-oss-20b-MXFP4.gguf"
        );
        assert_eq!(
            GLM_5_3.weights.object(&GLM_5_3.weights.shards[16]),
            "unsloth/GLM-5.3-GGUF/346b3591c7f28d1a23716f97a065ecf12ec14771/Q8_0/GLM-5.3-Q8_0-00017-of-00017.gguf"
        );
        assert_eq!(bucket_name("taste-ide"), "taste-weights-taste-ide");
        // A staging disk holds the largest shard, with room for the image.
        assert!(
            staging_boot_gib(&GLM_5_3) << 30 > GLM_5_3.weights.largest() + (BOOT_DISK_GIB << 30)
        );
        assert_eq!(
            GPT_OSS_20B.weights.url(&GPT_OSS_20B.weights.shards[0]),
            "https://huggingface.co/ggml-org/gpt-oss-20b-GGUF/resolve/ef9b12f2ff56c69cf32153a02784e7a3c88bf524/gpt-oss-20b-MXFP4.gguf"
        );
        assert_eq!(
            GLM_5_3.weights.url(&GLM_5_3.weights.shards[0]),
            "https://huggingface.co/unsloth/GLM-5.3-GGUF/resolve/346b3591c7f28d1a23716f97a065ecf12ec14771/Q8_0/GLM-5.3-Q8_0-00001-of-00017.gguf"
        );
    }

    #[test]
    fn the_pin_adds_up_to_the_spikes_total() {
        assert_eq!(GLM_5_3_Q8_0.shards.len(), 17);
        assert_eq!(GLM_5_3_Q8_0.total_bytes(), 801_357_677_216);
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
        assert_eq!(deny["priority"], json!(SERVE_DENY_PRIORITY));
        assert_eq!(deny["denied"], json!([{ "IPProtocol": "all" }]));
        assert_eq!(deny["destinationRanges"], json!(["0.0.0.0/0"]));
        assert!(
            !rules
                .iter()
                .any(|r| r["direction"] == "EGRESS" && r.get("allowed").is_some()),
            "no standing egress allow on the serving network"
        );
    }

    #[test]
    fn the_window_opens_onto_googles_apis_alone() {
        let plan = fixture(&CANDIDATES[0]);
        let window = &plan.window;
        assert_eq!(window["direction"], "EGRESS");
        assert_eq!(window["destinationRanges"], json!([GOOGLE_APIS_RANGE]));
        assert_eq!(
            window["allowed"],
            json!([{ "IPProtocol": "tcp", "ports": ["443"] }])
        );
        // Above the deny, which would otherwise win.
        assert!(window["priority"].as_u64().unwrap() < u64::from(SERVE_DENY_PRIORITY));
        assert_eq!(
            window["network"],
            format!("projects/proj/global/networks/{}", plan.names.serve_network)
        );
        assert!(!plan.firewalls.contains(window), "never a standing rule");
        assert!(GOOGLE_APIS_RANGE.starts_with(GOOGLE_APIS_ADDRESS));
    }

    #[test]
    fn serving_admits_only_iap_to_the_server_and_never_ssh() {
        let plan = fixture(&CANDIDATES[0]);
        let ingress: Vec<_> = rules_on(&plan, &plan.names.serve_network)
            .into_iter()
            .filter(|r| r["direction"] == "INGRESS")
            .collect();
        assert_eq!(ingress.len(), 1);
        assert_eq!(ingress[0]["sourceRanges"], json!([IAP_RANGE]));
        assert_eq!(
            ingress[0]["allowed"],
            json!([{ "IPProtocol": "tcp", "ports": ["8080"] }])
        );
        for rule in &plan.firewalls {
            assert!(!rule.to_string().contains("\"22\""), "{rule}");
        }
    }

    #[test]
    fn serving_has_no_address_on_the_internet() {
        let plan = fixture(&CANDIDATES[0]);
        assert!(plan.serving["networkInterfaces"][0]
            .get("accessConfigs")
            .is_none());
        // Staging keeps one: it is the only way out to Hugging Face.
        assert!(plan.staging["networkInterfaces"][0]
            .get("accessConfigs")
            .is_some());
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
    fn nothing_standing_is_a_disk() {
        let plan = fixture(&CANDIDATES[0]);
        for vm in [&plan.serving, &plan.staging] {
            let disks = vm["disks"].as_array().unwrap();
            assert_eq!(disks.len(), 1, "only the boot disk");
            assert_eq!(disks[0]["autoDelete"], json!(true));
        }
        let bucket = &plan.bucket;
        assert_eq!(bucket["location"], "US-CENTRAL1");
        assert_eq!(
            bucket["iamConfiguration"]["publicAccessPrevention"],
            "enforced"
        );
        assert_eq!(bucket["softDeletePolicy"]["retentionDurationSeconds"], "0");
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
            "DELETE"
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
        for labelled in [&plan.staging, &plan.serving] {
            assert_eq!(labelled["labels"][resources::WORKSPACE_LABEL], "0a1b2c3d");
        }
    }

    #[tokio::test]
    async fn the_foundation_denies_before_it_allows() {
        use crate::rest::mock::{gcp, serve};
        let done = || {
            json!({
                "name": "op",
                "status": "DONE",
                "selfLink": "https://www.googleapis.com/compute/v1/projects/proj/global/operations/op",
            })
        };
        let mock = serve(vec![
            (
                "POST",
                "/compute/v1/projects/proj/global/networks",
                200,
                done(),
            ),
            (
                "POST",
                "/compute/v1/projects/proj/global/networks",
                200,
                done(),
            ),
            (
                "POST",
                "/compute/v1/projects/proj/regions/us-central1/subnetworks",
                200,
                done(),
            ),
            (
                "POST",
                "/compute/v1/projects/proj/regions/us-central1/subnetworks",
                200,
                done(),
            ),
            (
                "POST",
                "/compute/v1/projects/proj/global/firewalls",
                200,
                done(),
            ),
            (
                "POST",
                "/compute/v1/projects/proj/global/firewalls",
                200,
                done(),
            ),
            (
                "POST",
                "/compute/v1/projects/proj/global/firewalls",
                200,
                done(),
            ),
            (
                "POST",
                "/compute/v1/projects/proj/global/firewalls",
                200,
                done(),
            ),
            ("POST", "/dns/v1/projects/proj/policies", 200, json!({})),
        ])
        .await;
        let plan = fixture(&CANDIDATES[0]);
        let loc = Location::new("proj", "us-central1-a").unwrap();
        ensure_foundation(&gcp(&mock), &loc, &plan).await.unwrap();

        let asked = mock.asked.lock().unwrap();
        let rules: Vec<&Value> = asked
            .iter()
            .filter(|a| a.path.ends_with("/firewalls"))
            .map(|a| a.body.as_ref().unwrap())
            .collect();
        let first_allow = rules
            .iter()
            .position(|r| r.get("allowed").is_some())
            .unwrap();
        assert!(rules[..first_allow]
            .iter()
            .all(|r| r.get("denied").is_some()));
        assert_eq!(first_allow, 2, "both denies before either allow");
    }
}

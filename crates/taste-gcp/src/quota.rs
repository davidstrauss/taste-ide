//! Whether the project's quotas let the IDE create a machine, asked before
//! it tries (David, 2026-10-03: "I want to know, as a user, if a quota is
//! too low").
//!
//! A machine draws on several quotas at once — vCPUs in all regions, vCPUs
//! in its region, its family's vCPUs there, and for a GPU machine its GPU
//! family's and all GPUs — and a new project's are far below what the
//! model's machines need (32 vCPUs in all regions, 24 of C4, no GPUs at
//! all). The limits are read through the Cloud Quotas API, which knows
//! the per-family quotas the older Compute listing does not, and each one
//! short is named with its limit and the need, so what to ask Google for
//! is the sentence itself. What it does not see is usage: a limit that
//! fits can still be taken by machines already running, and Compute's own
//! `QUOTA_EXCEEDED` at create time says so (`lifecycle::create_instance`).

use anyhow::Result;
use serde_json::Value;

use crate::model::CANDIDATES;
use crate::rest::Gcp;

/// One quota a machine draws on, and how much of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    /// The Cloud Quotas id, `CPUS-PER-VM-FAMILY-per-project-region`.
    pub quota_id: String,
    /// The dimensions that select the limit, `region` and `vm_family`.
    pub dimensions: Vec<(&'static str, String)>,
    pub amount: i64,
    /// What a person calls it: "C4 vCPUs in us-central1".
    pub label: String,
}

/// A quota below what a machine needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortfall {
    pub need: Need,
    pub limit: i64,
}

impl std::fmt::Display for Shortfall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is {}, and it needs {}",
            self.need.label, self.limit, self.need.amount
        )
    }
}

/// A machine type's family, vCPUs, and GPUs, read from its name
/// (`c4-highmem-192`, `c4-highmem-192-lssd`, `n4-standard-8`) and, for
/// GPUs, from the candidates that state them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    pub family: String,
    pub vcpus: i64,
    pub gpus: i64,
}

pub fn shape(machine: &str) -> Option<Shape> {
    let mut parts = machine.split('-');
    let family = parts.next()?.to_ascii_uppercase();
    let vcpus = parts.nth(1)?.parse().ok()?;
    let gpus = CANDIDATES
        .iter()
        .find(|m| m.name == machine)
        .map_or(0, |m| i64::from(m.gpus));
    Some(Shape {
        family,
        vcpus,
        gpus,
    })
}

/// The GPU family a machine family's GPUs count against.
fn gpu_family(family: &str) -> Option<&'static str> {
    match family {
        "G4" => Some("NVIDIA_RTX_PRO_6000"),
        _ => None,
    }
}

/// What creating `machine` in `region` draws on.
pub fn needs(machine: &str, region: &str) -> Vec<Need> {
    let Some(shape) = shape(machine) else {
        return Vec::new();
    };
    let region_dim = || ("region", region.to_string());
    let mut needs = vec![
        Need {
            quota_id: "CPUS-ALL-REGIONS-per-project".into(),
            dimensions: Vec::new(),
            amount: shape.vcpus,
            label: "vCPUs in all regions".into(),
        },
        Need {
            quota_id: "CPUS-per-project-region".into(),
            dimensions: vec![region_dim()],
            amount: shape.vcpus,
            label: format!("vCPUs in {region}"),
        },
        Need {
            quota_id: "CPUS-PER-VM-FAMILY-per-project-region".into(),
            dimensions: vec![region_dim(), ("vm_family", shape.family.clone())],
            amount: shape.vcpus,
            label: format!("{} vCPUs in {region}", shape.family),
        },
    ];
    if shape.gpus > 0 {
        if let Some(gpu_family) = gpu_family(&shape.family) {
            needs.push(Need {
                quota_id: "GPUS-PER-GPU-FAMILY-per-project-region".into(),
                dimensions: vec![region_dim(), ("gpu_family", gpu_family.to_string())],
                amount: shape.gpus,
                label: format!("{} GPUs in {region}", gpu_family.replace('_', " ")),
            });
        }
        needs.push(Need {
            quota_id: "GPUS-ALL-REGIONS-per-project".into(),
            dimensions: Vec::new(),
            amount: shape.gpus,
            label: "GPUs in all regions".into(),
        });
    }
    needs
}

/// The limit a quota info gives for `dimensions`: the most specific entry
/// whose dimensions all match. An entry with no value is a limit of 0
/// (what a GPU quota never granted looks like), and -1 is no limit.
/// `None` when nothing applies, which is "this quota does not govern
/// that", not zero.
pub fn limit(info: &Value, dimensions: &[(&str, String)]) -> Option<i64> {
    let entries = info["dimensionsInfos"].as_array()?;
    let mut best: Option<(usize, i64)> = None;
    for entry in entries {
        let dims = entry["dimensions"].as_object();
        let matches = dims.is_none_or(|dims| {
            dims.iter().all(|(key, value)| {
                dimensions
                    .iter()
                    .any(|(k, v)| k == key && value.as_str() == Some(v.as_str()))
            })
        });
        if !matches {
            continue;
        }
        let specificity = dims.map_or(0, |d| d.len());
        let value = entry["details"]["value"]
            .as_str()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if best.is_none_or(|(s, _)| specificity > s) {
            best = Some((specificity, value));
        }
    }
    best.map(|(_, value)| value)
}

/// The quota IDs to ask for one need: an older family has a quota of its
/// own (`C3-CPUS-per-project-region`) rather than a row in the per-family
/// one, so that is asked when the per-family one has no row for it.
fn fallback_id(need: &Need) -> Option<String> {
    let family = need
        .dimensions
        .iter()
        .find(|(k, _)| *k == "vm_family")
        .map(|(_, v)| v)?;
    Some(format!("{family}-CPUS-per-project-region"))
}

/// Whether the per-family quota has a row naming the family at all.
fn names_family(info: &Value, family: &str) -> bool {
    info["dimensionsInfos"].as_array().is_some_and(|entries| {
        entries
            .iter()
            .any(|e| e["dimensions"]["vm_family"].as_str() == Some(family))
    })
}

/// Every quota too low for `machine` in `region`. A quota the API does not
/// know is not counted as short: it is Compute's to refuse at create time.
pub async fn shortfalls(
    gcp: &Gcp,
    project: &str,
    region: &str,
    machine: &str,
) -> Result<Vec<Shortfall>> {
    let mut short = Vec::new();
    for need in needs(machine, region) {
        let mut info = gcp.quota_info(project, &need.quota_id).await?;
        let family = need
            .dimensions
            .iter()
            .find(|(k, _)| *k == "vm_family")
            .map(|(_, v)| v.clone());
        if let (Some(family), Some(found)) = (&family, &info) {
            if !names_family(found, family) {
                info = match fallback_id(&need) {
                    Some(id) => gcp.quota_info(project, &id).await?,
                    None => None,
                };
            }
        }
        let Some(info) = info else { continue };
        let Some(limit) = limit(&info, &need.dimensions) else {
            continue;
        };
        if limit >= 0 && limit < need.amount {
            short.push(Shortfall { need, limit });
        }
    }
    Ok(short)
}

/// Where a person asks for more.
pub fn console_url(project: &str) -> String {
    format!("https://console.cloud.google.com/iam-admin/quotas?project={project}")
}

/// One sentence for a machine the quotas do not allow.
pub fn sentence(machine: &str, short: &[Shortfall]) -> String {
    let listed: Vec<String> = short.iter().map(Shortfall::to_string).collect();
    format!("{machine} does not fit the quotas: {}", listed.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_machine_name_says_its_family_and_size() {
        assert_eq!(
            shape("c4-highmem-192"),
            Some(Shape {
                family: "C4".into(),
                vcpus: 192,
                gpus: 0
            })
        );
        assert_eq!(shape("c4-highmem-192-lssd").unwrap().vcpus, 192);
        assert_eq!(shape("c4d-highmem-192").unwrap().family, "C4D");
        assert_eq!(shape("g4-standard-192").unwrap().gpus, 4);
        assert_eq!(shape("n4-standard-8").unwrap().vcpus, 8);
        assert_eq!(shape("nonsense"), None);
    }

    #[test]
    fn a_gpu_machine_needs_gpu_quotas_and_a_cpu_one_does_not() {
        let cpu = needs("c4-highmem-192", "us-central1");
        assert_eq!(cpu.len(), 3);
        assert!(cpu.iter().all(|n| n.amount == 192));
        assert_eq!(
            cpu[2].dimensions,
            vec![
                ("region", "us-central1".to_string()),
                ("vm_family", "C4".to_string())
            ]
        );
        let gpu = needs("g4-standard-192", "us-central1");
        assert!(gpu
            .iter()
            .any(|n| n.quota_id == "GPUS-ALL-REGIONS-per-project" && n.amount == 4));
        assert!(gpu.iter().any(|n| n
            .dimensions
            .contains(&("gpu_family", "NVIDIA_RTX_PRO_6000".to_string()))));
    }

    /// The shape the API answered for project taste-ide on 2026-10-03.
    fn per_family() -> Value {
        json!({ "quotaId": "CPUS-PER-VM-FAMILY-per-project-region", "dimensionsInfos": [
            { "dimensions": { "region": "us-central1", "vm_family": "C4" }, "details": { "value": "24" } },
            { "dimensions": { "region": "us-central1", "vm_family": "N4" }, "details": { "value": "200" } },
            { "dimensions": { "vm_family": "C4" }, "details": { "value": "24" } },
            { "dimensions": { "vm_family": "N4" }, "details": { "value": "24" } },
            { "dimensions": { "region": "us-central1" }, "details": {} },
            { "details": {} },
        ] })
    }

    #[test]
    fn the_most_specific_row_is_the_limit() {
        let us = |family: &str| {
            vec![
                ("region", "us-central1".to_string()),
                ("vm_family", family.to_string()),
            ]
        };
        assert_eq!(limit(&per_family(), &us("C4")), Some(24));
        // The region's own row, not the family's default of 24.
        assert_eq!(limit(&per_family(), &us("N4")), Some(200));
        let elsewhere = vec![
            ("region", "europe-west4".to_string()),
            ("vm_family", "N4".to_string()),
        ];
        assert_eq!(limit(&per_family(), &elsewhere), Some(24));
    }

    #[test]
    fn a_quota_with_no_value_is_zero_and_minus_one_is_none_at_all() {
        let gpus = json!({ "dimensionsInfos": [
            { "dimensions": { "gpu_family": "NVIDIA_RTX_PRO_6000" }, "details": {} },
        ] });
        let want = vec![
            ("region", "us-central1".to_string()),
            ("gpu_family", "NVIDIA_RTX_PRO_6000".to_string()),
        ];
        assert_eq!(limit(&gpus, &want), Some(0));
        let all = json!({ "dimensionsInfos": [ { "details": {} } ] });
        assert_eq!(limit(&all, &[]), Some(0));
        let unlimited = json!({ "dimensionsInfos": [ { "details": { "value": "-1" } } ] });
        assert_eq!(limit(&unlimited, &[]), Some(-1));
    }

    #[tokio::test]
    async fn the_shortfalls_are_named_with_their_limits() {
        use crate::rest::mock::{gcp, serve};
        let base = "/v1/projects/p/locations/global/services/compute.googleapis.com/quotaInfos";
        let mock = serve(vec![
            (
                "GET",
                "quotaInfos/CPUS-ALL-REGIONS-per-project",
                200,
                json!({ "dimensionsInfos": [ { "details": { "value": "32" } } ] }),
            ),
            (
                "GET",
                "quotaInfos/CPUS-per-project-region",
                200,
                json!({ "dimensionsInfos": [
                    { "dimensions": { "region": "us-central1" }, "details": { "value": "200" } },
                ] }),
            ),
            (
                "GET",
                "quotaInfos/CPUS-PER-VM-FAMILY-per-project-region",
                200,
                per_family(),
            ),
        ])
        .await;
        let short = shortfalls(&gcp(&mock), "p", "us-central1", "c4-highmem-192")
            .await
            .unwrap();
        assert_eq!(short.len(), 2);
        assert_eq!(
            short[0].to_string(),
            "vCPUs in all regions is 32, and it needs 192"
        );
        assert_eq!(
            short[1].to_string(),
            "C4 vCPUs in us-central1 is 24, and it needs 192"
        );
        assert!(mock
            .asked
            .lock()
            .unwrap()
            .iter()
            .all(|a| a.path.contains(base)));
    }
}

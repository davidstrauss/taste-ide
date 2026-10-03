//! Driving a model's machines: stage the weights, serve them, stop the
//! serving VM, and take everything down.
//!
//! Every step is repeatable, as the resource calls under it are
//! (`rest::Gcp::ensure_compute`): staging again finds the shards already
//! checked on the disk and fetches none of them, serving a VM that is up
//! waits only for it to say so, and tearing down what is already gone is
//! not an error. How far a VM has got is read from the guest attributes it
//! writes (`guest`), through the Compute API — the VMs have no other way
//! to say anything, and the IDE no other way to ask.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use http::Method;
use serde_json::{json, Value};

use crate::guest::{KEY_ATTRIBUTE, NAMESPACE};
use crate::model::{ensure_foundation, ModelPlan};
use crate::resources::Location;
use crate::rest::{is_not_found, is_unavailable_machine, Gcp};

/// How often a VM is asked how far it has got.
const POLL: Duration = Duration::from_secs(10);
/// How long serving may take to answer: boot, image load, and a model
/// read off the disk.
const SERVE_DEADLINE: Duration = Duration::from_secs(45 * 60);

fn instance_path(loc: &Location, name: &str) -> String {
    format!(
        "projects/{}/zones/{}/instances/{name}",
        loc.project, loc.zone
    )
}

/// What `name` has written under the taste namespace, key by key; empty
/// when it has written nothing yet.
pub async fn guest_attributes(
    gcp: &Gcp,
    loc: &Location,
    name: &str,
) -> Result<HashMap<String, String>> {
    let url = format!(
        "{}/{}/getGuestAttributes?queryPath={NAMESPACE}/",
        gcp.endpoints.compute,
        instance_path(loc, name)
    );
    let answer = match gcp.call(Method::GET, &url, None).await {
        Ok(answer) => answer,
        Err(e) if is_not_found(&e) => return Ok(HashMap::new()),
        Err(e) => return Err(e),
    };
    Ok(answer["queryValue"]["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some((
                        item["key"].as_str()?.to_string(),
                        item["value"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default())
}

/// The instance's status (`RUNNING`, `TERMINATED`, …), or `None` if there
/// is no such instance.
pub async fn status(gcp: &Gcp, loc: &Location, name: &str) -> Result<Option<String>> {
    Ok(gcp
        .get_compute(&instance_path(loc, name))
        .await?
        .and_then(|instance| instance["status"].as_str().map(str::to_string)))
}

/// Create the instance `body` describes, trying `machines` in order while
/// the zone cannot supply one (David, 2026-10-02: "I'd rather not play
/// capacity games" — so the IDE plays them, and says which machine it
/// got). Returns the machine that was created.
pub async fn create_instance(
    gcp: &Gcp,
    loc: &Location,
    body: &Value,
    machines: &[&str],
    report: &(dyn Fn(&str) + Sync),
) -> Result<String> {
    let collection = format!("projects/{}/zones/{}/instances", loc.project, loc.zone);
    let mut last = None;
    for machine in machines {
        let mut body = body.clone();
        body["machineType"] = json!(loc.machine_type_url(machine));
        match gcp.ensure_compute(&collection, &body).await {
            Ok(()) => return Ok(machine.to_string()),
            Err(e) if is_unavailable_machine(&e) => {
                report(&format!(
                    "{} has no {machine} to give right now; trying the next",
                    loc.zone
                ));
                last = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last
        .unwrap_or_else(|| anyhow::anyhow!("no machine to try"))
        .context(format!(
            "{} could supply none of {}",
            loc.zone,
            machines.join(", ")
        )))
}

async fn act(gcp: &Gcp, loc: &Location, name: &str, verb: &str) -> Result<()> {
    let url = format!(
        "{}/{}/{verb}",
        gcp.endpoints.compute,
        instance_path(loc, name)
    );
    let operation = gcp.call(Method::POST, &url, None).await?;
    gcp.wait(operation).await?;
    Ok(())
}

/// Fetch and check the weights onto their disk with a staging VM, which
/// is deleted once it has said it is done. `report` hears each step.
pub async fn stage(
    gcp: &Gcp,
    loc: &Location,
    plan: &ModelPlan,
    spec: &crate::model::ModelSpec,
    report: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    let machines: Vec<&str> = std::iter::once(crate::model::STAGING_MACHINE)
        .chain(
            crate::model::SMALL_MACHINES
                .iter()
                .copied()
                .filter(|m| *m != crate::model::STAGING_MACHINE),
        )
        .collect();
    report("creating the networks, rules, and the weights disk");
    ensure_foundation(gcp, loc, plan).await?;
    let disk_path = format!(
        "projects/{}/zones/{}/disks/{}",
        loc.project, loc.zone, plan.names.weights_disk
    );
    let staged = crate::guest::staged_label(spec);
    if let Some(disk) = gcp.get_compute(&disk_path).await? {
        if disk["labels"][crate::guest::STAGED_LABEL] == staged.as_str() {
            report("the weights disk already holds this pin; nothing to stage");
            return Ok(());
        }
    }
    // Staging writes the disk, and Hyperdisk Balanced has one writer: a
    // serving VM still holding it goes first. It is replaced anyway, since
    // what it was serving is what is being staged anew.
    if status(gcp, loc, &plan.names.serving).await?.is_some() {
        report("removing the serving VM so the disk can be staged");
        gcp.remove_compute(&instance_path(loc, &plan.names.serving))
            .await?;
    }
    let name = &plan.names.staging;
    // A staging VM left from a run that did not finish is replaced: the
    // disk keeps whatever it already checked, so nothing is fetched twice.
    if let Some(status) = status(gcp, loc, name).await? {
        if status != "RUNNING" {
            gcp.remove_compute(&instance_path(loc, name)).await?;
        }
    }
    report("starting the staging VM");
    let machine = create_instance(gcp, loc, &plan.staging, &machines, report).await?;
    report(&format!("staging on {machine}"));
    let mut said = String::new();
    loop {
        tokio::time::sleep(POLL).await;
        let attributes = guest_attributes(gcp, loc, name).await?;
        let stage = attributes.get("stage").cloned().unwrap_or_default();
        let progress = attributes
            .get("progress")
            .and_then(|p| p.split_once('/'))
            .and_then(|(done, total)| Some((done.parse::<u64>().ok()?, total.parse::<u64>().ok()?)))
            .map(|(done, total)| format!(" · {} of {} MiB", done >> 20, total >> 20))
            .unwrap_or_default();
        let now = format!(
            "staging: {}{progress}",
            if stage.is_empty() { "booting" } else { &stage }
        );
        if now != said {
            report(&now);
            said = now;
        }
        if stage == "done" {
            break;
        }
        if let Some(why) = stage.strip_prefix("failed: ") {
            bail!("staging failed: {why}");
        }
        if status(gcp, loc, name).await?.as_deref() != Some("RUNNING") && !stage.is_empty() {
            bail!("the staging VM stopped before it was done (last said: {stage})");
        }
    }
    report("deleting the staging VM; the disk keeps the weights");
    gcp.remove_compute(&instance_path(loc, name)).await?;
    // The disk says what it holds, so the next run need not ask a VM.
    if let Some(disk) = gcp.get_compute(&disk_path).await? {
        let mut labels = disk["labels"].clone();
        labels[crate::guest::STAGED_LABEL] = json!(staged);
        let url = format!("{}/{disk_path}/setLabels", gcp.endpoints.compute);
        let operation = gcp
            .call(
                Method::POST,
                &url,
                Some(&json!({
                    "labels": labels,
                    "labelFingerprint": disk["labelFingerprint"],
                })),
            )
            .await?;
        gcp.wait(operation).await?;
    }
    Ok(())
}

/// Bring the serving VM up — created, started, or already running — and
/// wait until llama-server answers on it.
pub async fn serve(
    gcp: &Gcp,
    loc: &Location,
    plan: &ModelPlan,
    spec: &crate::model::ModelSpec,
    report: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    let name = &plan.names.serving;
    // A serving VM booted from another config is replaced: Ignition runs on
    // a machine's first boot only, so a fix to the guest would otherwise
    // never reach the VM that is already there.
    if let Some(instance) = gcp.get_compute(&instance_path(loc, name)).await? {
        let wanted = metadata_value(&plan.serving, crate::guest::CONFIG_ATTRIBUTE);
        let has = metadata_value(&instance, crate::guest::CONFIG_ATTRIBUTE);
        let failed = guest_attributes(gcp, loc, name)
            .await?
            .get("serve")
            .is_some_and(|said| said.starts_with("failed: "));
        if wanted.is_some() && wanted != has {
            report("the serving VM was booted from another config; replacing it");
            gcp.remove_compute(&instance_path(loc, name)).await?;
        } else if failed {
            // Its service has exited and will not come back by itself; a
            // fresh boot is the retry.
            report("the serving VM failed last time; replacing it");
            gcp.remove_compute(&instance_path(loc, name)).await?;
        }
    }
    let before = guest_attributes(gcp, loc, name)
        .await?
        .get("ready")
        .cloned();
    match status(gcp, loc, name).await?.as_deref() {
        None => {
            report("creating the serving VM");
            let chosen = plan.serving["machineType"]
                .as_str()
                .and_then(|url| url.rsplit('/').next())
                .unwrap_or(spec.machine.name)
                .to_string();
            let machines: Vec<&str> = std::iter::once(chosen.as_str())
                .chain(spec.fallbacks.iter().copied().filter(|m| *m != chosen))
                .collect();
            let machine = create_instance(gcp, loc, &plan.serving, &machines, report).await?;
            report(&format!("serving on {machine}"));
        }
        Some("RUNNING") => {
            if before.is_some() {
                report("the serving VM is already up");
                return Ok(());
            }
        }
        Some(_) => {
            report("starting the serving VM");
            act(gcp, loc, name, "start").await?;
        }
    }
    let deadline = Instant::now() + SERVE_DEADLINE;
    let mut said = String::new();
    loop {
        tokio::time::sleep(POLL).await;
        let attributes = guest_attributes(gcp, loc, name).await?;
        let ready = attributes.get("ready");
        if ready.is_some() && ready != before.as_ref() {
            report("the model answers");
            return Ok(());
        }
        if let Some(why) = attributes
            .get("serve")
            .and_then(|said| said.strip_prefix("failed: "))
        {
            bail!("the serving VM failed: {why}");
        }
        let now = format!(
            "serving: {}",
            attributes
                .get("serve")
                .map(String::as_str)
                .unwrap_or("booting")
        );
        if now != said {
            report(&now);
            said = now;
        }
        if Instant::now() > deadline {
            bail!(
                "the serving VM did not answer within {} minutes",
                SERVE_DEADLINE.as_secs() / 60
            );
        }
    }
}

/// One value from an instance's (or an instance body's) metadata.
fn metadata_value(instance: &Value, key: &str) -> Option<String> {
    instance["metadata"]["items"]
        .as_array()?
        .iter()
        .find(|item| item["key"] == key)
        .and_then(|item| item["value"].as_str())
        .map(str::to_string)
}

/// The key llama-server checks, as the instance's metadata holds it.
pub async fn serving_key(gcp: &Gcp, loc: &Location, plan: &ModelPlan) -> Result<String> {
    let instance = gcp
        .get_compute(&instance_path(loc, &plan.names.serving))
        .await?
        .context("there is no serving VM")?;
    instance["metadata"]["items"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|item| item["key"] == KEY_ATTRIBUTE)
                .and_then(|item| item["value"].as_str())
        })
        .map(str::to_string)
        .context("the serving VM has no key in its metadata")
}

/// Stop the serving VM, if it is running. Its disks stay; a stopped VM
/// bills for nothing else.
pub async fn stop(gcp: &Gcp, loc: &Location, plan: &ModelPlan) -> Result<()> {
    let name = &plan.names.serving;
    if status(gcp, loc, name).await?.as_deref() == Some("RUNNING") {
        act(gcp, loc, name, "stop").await?;
    }
    Ok(())
}

/// Delete everything the plan creates, last made first. What is already
/// gone is skipped.
pub async fn teardown(
    gcp: &Gcp,
    loc: &Location,
    plan: &ModelPlan,
    report: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    let project = &loc.project;
    for name in [&plan.names.serving, &plan.names.staging] {
        report(&format!("deleting {name}"));
        gcp.remove_compute(&instance_path(loc, name)).await?;
    }
    report(&format!("deleting {}", plan.names.weights_disk));
    gcp.remove_compute(&format!(
        "projects/{project}/zones/{}/disks/{}",
        loc.zone, plan.names.weights_disk
    ))
    .await?;
    report(&format!("deleting {}", plan.names.serve_dns_policy));
    gcp.remove_dns_policy(project, &plan.names.serve_dns_policy)
        .await?;
    for rule in &plan.firewalls {
        if let Some(name) = rule["name"].as_str() {
            gcp.remove_compute(&format!("projects/{project}/global/firewalls/{name}"))
                .await?;
        }
    }
    for subnetwork in &plan.subnetworks {
        if let Some(name) = subnetwork["name"].as_str() {
            gcp.remove_compute(&format!(
                "projects/{project}/regions/{}/subnetworks/{name}",
                loc.region
            ))
            .await?;
        }
    }
    for network in &plan.networks {
        if let Some(name) = network["name"].as_str() {
            report(&format!("deleting {name}"));
            gcp.remove_compute(&format!("projects/{project}/global/networks/{name}"))
                .await?;
        }
    }
    Ok(())
}

/// A request body for the model, Anthropic's Messages API as llama-server
/// serves it: the smoke test's one question.
pub fn smoke_request(model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 200,
        "messages": [{ "role": "user", "content": "Reply with one short sentence saying you are running." }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rest::mock::{gcp, serve as mock};

    #[tokio::test]
    async fn guest_attributes_are_read_key_by_key_and_none_is_empty() {
        let mock = mock(vec![
            (
                "GET",
                "/getGuestAttributes?queryPath=taste/",
                200,
                json!({ "queryValue": { "items": [
                    { "namespace": "taste", "key": "stage", "value": "fetching a.gguf" },
                    { "namespace": "taste", "key": "progress", "value": "10/20" },
                ] } }),
            ),
            (
                "GET",
                "/getGuestAttributes?queryPath=taste/",
                404,
                json!({ "error": { "code": 404, "status": "NOT_FOUND", "message": "none yet" } }),
            ),
        ])
        .await;
        let loc = Location::new("p", "us-central1-a").unwrap();
        let gcp = gcp(&mock);
        let first = guest_attributes(&gcp, &loc, "vm").await.unwrap();
        assert_eq!(first["stage"], "fetching a.gguf");
        assert_eq!(first["progress"], "10/20");
        assert!(guest_attributes(&gcp, &loc, "vm").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_key_is_read_back_from_the_instance() {
        let mock = mock(vec![(
            "GET",
            "/compute/v1/projects/p/zones/us-central1-a/instances/taste-0a1b2c3d-serve",
            200,
            json!({ "status": "RUNNING", "metadata": { "items": [
                { "key": "user-data", "value": "{}" },
                { "key": "taste-key", "value": "k3y" },
            ] } }),
        )])
        .await;
        let loc = Location::new("p", "us-central1-a").unwrap();
        let ws = crate::resources::Workspace::new("0a1b2c3d").unwrap();
        let plan = crate::model::plan(
            &ws,
            &loc,
            &crate::guest::fcos_image("44.20260829.3.1"),
            &crate::model::GPT_OSS_20B,
            &crate::model::GPT_OSS_20B.machine,
            &crate::model::GuestConfigs::default(),
        )
        .unwrap();
        assert_eq!(serving_key(&gcp(&mock), &loc, &plan).await.unwrap(), "k3y");
    }
}

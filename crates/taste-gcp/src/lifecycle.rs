//! Driving a model's machines: mirror the weights into the bucket, serve
//! them, stop the serving VM, and take everything down.
//!
//! Every step is repeatable, as the resource calls under it are
//! (`rest::Gcp::ensure_compute`): staging again finds the shards already
//! in the bucket and fetches none of them, serving a VM that is up
//! answers at once, and tearing down what is already gone is not an
//! error. How far a VM has got is read from the guest attributes it
//! writes (`guest`), through the Compute API — the VMs have no other way
//! to say anything, and the IDE no other way to ask.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use http::Method;
use serde_json::{json, Value};

use crate::guest::{
    image_object, FETCH_ATTRIBUTE, IMAGE_FILE, KEY_ATTRIBUTE, NAMESPACE, UPLOADS_ATTRIBUTE,
    WINDOW_ATTRIBUTE,
};
use crate::model::{
    bucket_name, ensure_foundation, ModelPlan, ModelSpec, Shard, SERVE_DENY_PRIORITY,
};
use crate::resources::Location;
use crate::rest::{is_not_found, is_unavailable_machine, Gcp};
use crate::setup::service_account;
use crate::signed;

/// How often a VM is asked how far it has got.
const POLL: Duration = Duration::from_secs(10);
/// How long the serving VM may take to boot and pull the weights, which
/// is also the longest the window can stay open on its account.
const FETCH_DEADLINE: Duration = Duration::from_secs(30 * 60);
/// How long a fetch URL lasts: past the deadline, and no further.
const FETCH_URL_LIFETIME: Duration = Duration::from_secs(45 * 60);
/// How long the server may take to answer once the window is shut.
const SERVE_DEADLINE: Duration = Duration::from_secs(30 * 60);

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

/// The shards the bucket does not yet hold, by name, size, and the digest
/// they were uploaded with; and whether it lacks the server image.
pub async fn missing<'a>(
    gcp: &Gcp,
    bucket: &str,
    spec: &'a ModelSpec,
) -> Result<(Vec<&'a Shard>, Option<u64>)> {
    let objects = gcp.list_objects(bucket, "").await?;
    let find = |name: &str| objects.iter().find(|o| o["name"] == name);
    let shards = spec
        .weights
        .shards
        .iter()
        .filter(|shard| {
            !find(&spec.weights.object(shard)).is_some_and(|o| {
                o["size"].as_str() == Some(&shard.bytes.to_string())
                    && o["metadata"]["sha256"] == shard.sha256
            })
        })
        .collect();
    let image = find(&image_object())
        .and_then(|o| o["size"].as_str())
        .and_then(|size| size.parse().ok());
    Ok((shards, image))
}

/// Mirror the pinned weights and the server image into the project's
/// bucket with a staging VM, which is deleted once it has said it is done.
/// Only what the bucket lacks is fetched, so staging a pin that is there
/// creates nothing. `report` hears each step.
pub async fn stage(
    gcp: &Gcp,
    loc: &Location,
    plan: &ModelPlan,
    spec: &ModelSpec,
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
    report("creating the networks, rules, and the bucket");
    ensure_foundation(gcp, loc, plan).await?;
    gcp.ensure_bucket(&loc.project, &plan.bucket).await?;
    let bucket = bucket_name(&loc.project);
    let (shards, image) = missing(gcp, &bucket, spec).await?;
    if shards.is_empty() && image.is_some() {
        report("the bucket already holds this pin; nothing to stage");
        return Ok(());
    }
    let name = &plan.names.staging;
    // A staging VM left from a run that did not finish holds URLs for
    // what was missing then; it is replaced.
    gcp.remove_compute(&instance_path(loc, name)).await?;

    // An upload URL for each missing object, signed for the digest it must
    // arrive with and for as long as the staging VM may live.
    let account = service_account(&loc.project);
    let expires = Duration::from_secs(crate::model::STAGE_MAX_RUN_SECONDS + 3600);
    let mut uploads = String::new();
    for shard in &shards {
        let object = spec.weights.object(shard);
        let url = signed::sign(
            gcp,
            &account,
            &signed::Request {
                method: "POST",
                bucket: &bucket,
                object: &object,
                headers: vec![
                    ("x-goog-resumable".into(), "start".into()),
                    ("x-goog-meta-sha256".into(), shard.sha256.into()),
                ],
                expires,
            },
        )
        .await?;
        uploads.push_str(&format!("{} {url}\n", shard.file));
    }
    if image.is_none() {
        let object = image_object();
        let url = signed::sign(
            gcp,
            &account,
            &signed::Request {
                method: "POST",
                bucket: &bucket,
                object: &object,
                headers: vec![("x-goog-resumable".into(), "start".into())],
                expires,
            },
        )
        .await?;
        uploads.push_str(&format!("{IMAGE_FILE} {url}\n"));
    }
    let mut body = plan.staging.clone();
    push_metadata(&mut body, UPLOADS_ATTRIBUTE, &uploads);

    report(&format!(
        "starting the staging VM for {} shard{}{}",
        shards.len(),
        if shards.len() == 1 { "" } else { "s" },
        if image.is_none() {
            " and the server image"
        } else {
            ""
        }
    ));
    let machine = create_instance(gcp, loc, &body, &machines, report).await?;
    report(&format!("staging on {machine}"));
    let mut said = String::new();
    loop {
        tokio::time::sleep(POLL).await;
        let attributes = guest_attributes(gcp, loc, name).await?;
        let stage = attributes.get("stage").cloned().unwrap_or_default();
        let now = format!(
            "staging: {}{}",
            if stage.is_empty() { "booting" } else { &stage },
            progress(&attributes)
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
    report("deleting the staging VM; the bucket keeps the weights");
    gcp.remove_compute(&instance_path(loc, name)).await?;
    // The bucket is the authority on what it holds, not the VM's word.
    let (shards, image) = missing(gcp, &bucket, spec).await?;
    if let Some(shard) = shards.first() {
        bail!(
            "staging finished, but the bucket still lacks {}",
            shard.file
        );
    }
    if image.is_none() {
        bail!("staging finished, but the bucket still lacks the server image");
    }
    Ok(())
}

/// " · 10 of 20 MiB" from a guest's `progress` attribute, or nothing.
fn progress(attributes: &HashMap<String, String>) -> String {
    attributes
        .get("progress")
        .and_then(|p| p.split_once('/'))
        .and_then(|(done, total)| Some((done.parse::<u64>().ok()?, total.parse::<u64>().ok()?)))
        .map(|(done, total)| format!(" · {} of {} MiB", done >> 20, total >> 20))
        .unwrap_or_default()
}

fn push_metadata(body: &mut Value, key: &str, value: &str) {
    if let Some(items) = body["metadata"]["items"].as_array_mut() {
        items.push(json!({ "key": key, "value": value }));
    }
}

fn firewall_path(loc: &Location, name: &str) -> String {
    format!("projects/{}/global/firewalls/{name}", loc.project)
}

fn subnetwork_path(loc: &Location, name: &str) -> String {
    format!(
        "projects/{}/regions/{}/subnetworks/{name}",
        loc.project, loc.region
    )
}

/// Open the serving network's window onto Google's APIs: Private Google
/// Access on its subnet, and the one rule that outranks the deny.
pub async fn open_window(gcp: &Gcp, loc: &Location, plan: &ModelPlan) -> Result<()> {
    // A foundation made before the window existed holds the deny at 0,
    // where it would win the tie with the window's rule.
    let deny = firewall_path(loc, &plan.names.serve_deny_egress);
    if let Some(rule) = gcp.get_compute(&deny).await? {
        if rule["priority"] != json!(SERVE_DENY_PRIORITY) {
            gcp.patch_compute(&deny, &json!({ "priority": SERVE_DENY_PRIORITY }))
                .await?;
        }
    }
    gcp.act_compute(
        &subnetwork_path(loc, &plan.names.serve_subnetwork),
        "setPrivateIpGoogleAccess",
        Some(&json!({ "privateIpGoogleAccess": true })),
    )
    .await?;
    gcp.ensure_compute(
        &format!("projects/{}/global/firewalls", loc.project),
        &plan.window,
    )
    .await
}

/// Shut the window: the rule first, since it is what admits a connection,
/// then Private Google Access. Shutting a shut window is not an error, so
/// every serve starts by shutting one an interrupted run left open.
pub async fn close_window(gcp: &Gcp, loc: &Location, plan: &ModelPlan) -> Result<()> {
    gcp.remove_compute(&firewall_path(loc, &plan.names.serve_window))
        .await?;
    gcp.act_compute(
        &subnetwork_path(loc, &plan.names.serve_subnetwork),
        "setPrivateIpGoogleAccess",
        Some(&json!({ "privateIpGoogleAccess": false })),
    )
    .await
}

/// Set and remove keys in a running instance's metadata, against the
/// fingerprint it was read with.
async fn set_metadata(
    gcp: &Gcp,
    loc: &Location,
    name: &str,
    set: &[(&str, &str)],
    remove: &[&str],
) -> Result<()> {
    let path = instance_path(loc, name);
    let instance = gcp
        .get_compute(&path)
        .await?
        .with_context(|| format!("{name} is gone"))?;
    let mut items: Vec<Value> = instance["metadata"]["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|item| {
            let key = item["key"].as_str().unwrap_or_default();
            !remove.contains(&key) && !set.iter().any(|(k, _)| *k == key)
        })
        .collect();
    items.extend(
        set.iter()
            .map(|(key, value)| json!({ "key": key, "value": value })),
    );
    gcp.act_compute(
        &path,
        "setMetadata",
        Some(&json!({
            "fingerprint": instance["metadata"]["fingerprint"],
            "items": items,
        })),
    )
    .await
}

/// What the serving VM said once its downloads were done: how many bytes,
/// in how many seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fetched {
    pub bytes: u64,
    pub seconds: f64,
}

impl Fetched {
    fn parse(said: &str) -> Option<Self> {
        let mut words = said.split_whitespace().skip(1);
        Some(Self {
            bytes: words.next()?.parse().ok()?,
            seconds: words.next()?.parse().ok()?,
        })
    }

    pub fn gigabytes_per_second(&self) -> f64 {
        self.bytes as f64 / 1e9 / self.seconds.max(0.1)
    }
}

/// Create the serving VM with its fetch list, and wait until it says its
/// downloads are done.
async fn create_and_fetch(
    gcp: &Gcp,
    loc: &Location,
    plan: &ModelPlan,
    spec: &ModelSpec,
    fetch: &str,
    report: &(dyn Fn(&str) + Sync),
) -> Result<Fetched> {
    let name = &plan.names.serving;
    let mut body = plan.serving.clone();
    push_metadata(&mut body, FETCH_ATTRIBUTE, fetch);
    let chosen = body["machineType"]
        .as_str()
        .and_then(|url| url.rsplit('/').next())
        .unwrap_or(spec.machine.name)
        .to_string();
    let machines: Vec<&str> = std::iter::once(chosen.as_str())
        .chain(spec.fallbacks.iter().copied().filter(|m| *m != chosen))
        .collect();
    report("creating the serving VM");
    let machine = create_instance(gcp, loc, &body, &machines, report).await?;
    report(&format!("serving on {machine}"));
    let deadline = Instant::now() + FETCH_DEADLINE;
    let mut said = String::new();
    loop {
        tokio::time::sleep(POLL).await;
        let attributes = guest_attributes(gcp, loc, name).await?;
        if let Some(fetched) = attributes.get("fetched").and_then(|f| Fetched::parse(f)) {
            return Ok(fetched);
        }
        if let Some(why) = attributes
            .get("serve")
            .and_then(|said| said.strip_prefix("failed: "))
        {
            bail!("the serving VM failed: {why}");
        }
        let now = format!(
            "serving: {}{}",
            attributes
                .get("serve")
                .map(String::as_str)
                .unwrap_or("booting"),
            progress(&attributes)
        );
        if now != said {
            report(&now);
            said = now;
        }
        if Instant::now() > deadline {
            bail!(
                "the serving VM had not fetched the weights after {} minutes",
                FETCH_DEADLINE.as_secs() / 60
            );
        }
    }
}

/// Bring the serving VM up and wait until llama-server answers on it:
/// the window opened, the VM created with signed URLs for what it pulls,
/// the window shut the moment it has pulled them — whatever else went
/// wrong — and only then the guest told so.
pub async fn serve(
    gcp: &Gcp,
    loc: &Location,
    plan: &ModelPlan,
    spec: &ModelSpec,
    report: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    let name = &plan.names.serving;
    ensure_foundation(gcp, loc, plan).await?;
    close_window(gcp, loc, plan).await?;
    let bucket = bucket_name(&loc.project);
    let (shards, image) = missing(gcp, &bucket, spec).await?;
    let Some(image_bytes) = image.filter(|_| shards.is_empty()) else {
        bail!("the bucket does not hold this pin yet; stage it first");
    };

    if let Some(instance) = gcp.get_compute(&instance_path(loc, name)).await? {
        let wanted = metadata_value(&plan.serving, crate::guest::CONFIG_ATTRIBUTE);
        let has = metadata_value(&instance, crate::guest::CONFIG_ATTRIBUTE);
        let ready = guest_attributes(gcp, loc, name)
            .await?
            .contains_key("ready");
        if instance["status"] == "RUNNING" && wanted == has && ready {
            report("the serving VM is already up");
            return Ok(());
        }
        // Anything else is a VM that cannot become ready by itself: its
        // weights went with its last boot, or it is from another config.
        report("replacing the serving VM");
        gcp.remove_compute(&instance_path(loc, name)).await?;
    }

    let account = service_account(&loc.project);
    let get = |object: String| {
        let account = account.clone();
        let bucket = bucket.clone();
        async move {
            signed::sign(
                gcp,
                &account,
                &signed::Request {
                    method: "GET",
                    bucket: &bucket,
                    object: &object,
                    headers: Vec::new(),
                    expires: FETCH_URL_LIFETIME,
                },
            )
            .await
        }
    };
    let mut fetch = String::new();
    for shard in spec.weights.shards {
        let url = get(spec.weights.object(shard)).await?;
        fetch.push_str(&format!("{} {} {url}\n", shard.file, shard.bytes));
    }
    let url = get(image_object()).await?;
    fetch.push_str(&format!("{IMAGE_FILE} {image_bytes} {url}\n"));

    report("opening the window to the bucket");
    open_window(gcp, loc, plan).await?;
    let fetched = create_and_fetch(gcp, loc, plan, spec, &fetch, report).await;
    report("closing the window");
    close_window(gcp, loc, plan).await?;
    let fetched = fetched?;
    report(&format!(
        "fetched {:.1} GB in {:.1} s: {:.2} GB/s",
        fetched.bytes as f64 / 1e9,
        fetched.seconds,
        fetched.gigabytes_per_second()
    ));
    set_metadata(
        gcp,
        loc,
        name,
        &[(WINDOW_ATTRIBUTE, "closed")],
        &[FETCH_ATTRIBUTE],
    )
    .await?;

    let deadline = Instant::now() + SERVE_DEADLINE;
    let mut said = String::new();
    loop {
        tokio::time::sleep(POLL).await;
        let attributes = guest_attributes(gcp, loc, name).await?;
        if attributes.contains_key("ready") {
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

/// Stop serving: the VM is deleted, since nothing on it outlives its boot
/// and a stopped one would keep only a disk to pay for. The weights stay
/// in the bucket.
pub async fn stop(gcp: &Gcp, loc: &Location, plan: &ModelPlan) -> Result<()> {
    gcp.remove_compute(&instance_path(loc, &plan.names.serving))
        .await
}

/// Delete everything the plan creates for this workspace, last made
/// first. What is already gone is skipped. The bucket stays: it is the
/// project's, and every workspace in it reads the same weights.
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
    gcp.remove_compute(&firewall_path(loc, &plan.names.serve_window))
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

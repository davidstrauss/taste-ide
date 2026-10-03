//! What the model's two VMs are told to do: an Ignition config for each,
//! for the Fedora CoreOS the local pool boots too, so a guest in GCP and a
//! guest under libvirt are the same system.
//!
//! The **staging VM** mirrors the pinned weights into the project's
//! bucket, once: it downloads each shard the IDE says is missing, checks
//! it against its SHA-256, and uploads it through a signed resumable
//! upload the IDE made for exactly that object and digest; then the same
//! for the pinned llama.cpp server image, checked by its image ID. It has
//! egress to port 443 and nothing else, no credential of its own, and
//! nothing in it runs the weights — a download, a hash, and an upload are
//! all it does with them.
//!
//! The **serving VM** pulls the weights into memory on every boot (the
//! spike's "Weights from GCS instead of a disk"): parallel ranged GETs of
//! signed URLs into a tmpfs, by address, through the window the IDE opened
//! for it (`model::window_firewall`). It says how long that took, checks
//! every shard against the pinned digests in its own Ignition — so a
//! bucket that was tampered with feeds it nothing — and loads the image,
//! checked by ID. Then it waits: the server starts only once the IDE has
//! closed the window AND a fresh connection to Google's APIs fails, so
//! nothing that came from the model runs while there is a way out. It runs
//! llama-server on its internal address with the key the IDE wrote into
//! the instance's metadata, and reports `taste/ready` once `/health`
//! answers.
//!
//! Both report by writing guest attributes to the metadata server **by
//! IP**, since the serving network resolves no names at all
//! (`model::DNS_BLACKHOLE`), and the IDE reads them back through the
//! Compute API. Both mask Zincati: a guest is replaced, never updated, and
//! the serving one could not reach an update anyway.

use base64::Engine;
use serde_json::{json, Value};

use crate::model::{ModelSpec, GOOGLE_APIS_ADDRESS, SERVER_PORT};
use crate::signed::HOST;

/// The llama.cpp server, CPU build, pinned by its amd64 manifest digest:
/// release `b11371` (2026-10-03), after the GLM indexer work (#25407).
pub const SERVER_IMAGE: &str =
    "ghcr.io/ggml-org/llama.cpp@sha256:5d58fdc8b2cadfad36c89ecf38b4a50069653500551a6842b0157a2d82d065f3";

/// That image's ID — the digest of its config, which `podman save` and
/// `podman load` leave as it was, unlike the manifest's. Read from the
/// registry's manifest for [`SERVER_IMAGE`] (its `config.digest`). Both
/// guests check it: staging before it uploads, serving after it loads.
pub const SERVER_IMAGE_ID: &str =
    "400a06a24bc582ed4a8ae5bbaac185e1d16ae269e0dd206ce464022c54ccc69e";

/// The server image's name in the bucket.
pub fn image_object() -> String {
    format!("images/llama.cpp/{SERVER_IMAGE_ID}.tar")
}

/// What the archive is called on the guests, and in the lists the IDE
/// gives them.
pub const IMAGE_FILE: &str = "llama-server.tar";

/// The Fedora CoreOS release the cloud guests boot: the local pool's own
/// (`taste_devcontainer::guest::RELEASE`), held equal to it by a test in
/// `taste-app`, so a guest in GCP and one under libvirt are one system.
pub const FCOS_RELEASE: &str = "44.20260829.3.1";

/// A stable hash of a guest's Ignition config, kept in the instance's
/// metadata (`CONFIG_ATTRIBUTE`), so a VM booted from an older config is
/// known and replaced: Ignition runs only on a machine's first boot.
pub fn config_hash(ignition: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in ignition.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The metadata key [`config_hash`] is kept under.
pub const CONFIG_ATTRIBUTE: &str = "taste-config";

/// What the server image is called once a guest has it.
pub const LOCAL_TAG: &str = "localhost/taste-llama-server:pinned";

/// The guest attribute namespace both VMs write to.
pub const NAMESPACE: &str = "taste";
/// The metadata key llama-server's API key is read from.
pub const KEY_ATTRIBUTE: &str = "taste-key";
/// The staging VM's list of what to upload: `file url` a line, each URL a
/// signed resumable-upload start for one object.
pub const UPLOADS_ATTRIBUTE: &str = "taste-uploads";
/// The serving VM's list of what to fetch: `file bytes url` a line, each
/// URL a signed GET, good for this boot's window.
pub const FETCH_ATTRIBUTE: &str = "taste-fetch";
/// Set to `closed` by the IDE once the window is shut.
pub const WINDOW_ATTRIBUTE: &str = "taste-window";

/// How much of an object one ranged GET asks for.
pub const SLICE_BYTES: u64 = 256 << 20;

/// Fedora CoreOS's GCP image for a release, by the project's naming
/// convention: `44.20260829.3.1` is `fedora-coreos-44-20260829-3-1-gcp-x86-64`
/// in `fedora-coreos-cloud`.
pub fn fcos_image(release: &str) -> crate::resources::ImageRef {
    crate::resources::ImageRef {
        project: "fedora-coreos-cloud".into(),
        name: format!("fedora-coreos-{}-gcp-x86-64", release.replace('.', "-")),
    }
}

/// The shell every unit starts with: a way to say how far it has got, and
/// a way to read what the IDE said.
const PREAMBLE: &str = r#"#!/bin/bash
set -euo pipefail
MD=http://169.254.169.254/computeMetadata/v1/instance
# By IP, never by name: the serving network resolves nothing.
say() {
  curl -sf -X PUT -H "Metadata-Flavor: Google" --data "$2" \
    "$MD/guest-attributes/taste/$1" >/dev/null || true
}
# One metadata value, or nothing.
md() {
  curl -sf -H "Metadata-Flavor: Google" "$MD/attributes/$1" || true
}
WEIGHTS=/var/mnt/weights
"#;

/// The pinned shards as `sha256 bytes file url` lines.
fn manifest(spec: &ModelSpec) -> String {
    spec.weights
        .shards
        .iter()
        .map(|shard| {
            format!(
                "{} {} {} {}\n",
                shard.sha256,
                shard.bytes,
                shard.file,
                spec.weights.url(shard)
            )
        })
        .collect()
}

/// The staging script: fetch, check, upload, report, power off.
pub fn staging_script(spec: &ModelSpec) -> String {
    let manifest = manifest(spec);
    format!(
        r#"{PREAMBLE}
fail() {{ say stage "failed: $1"; poweroff; exit 1; }}
trap 'fail "line $LINENO: $BASH_COMMAND"' ERR
WORK=/var/lib/taste-stage
mkdir -p "$WORK"
cd "$WORK"
md {UPLOADS_ATTRIBUTE} > uploads
[ -s uploads ] || fail "the IDE listed nothing to upload"
cat > manifest <<'MANIFEST'
{manifest}MANIFEST

# Start a resumable upload with the signed URL, then send the file to the
# session it opens. The headers are the ones the IDE signed, value for
# value, or Google refuses the signature.
upload() {{
  local file=$1 url=$2 sha=${{3:-}} session
  local -a meta=()
  [ -z "$sha" ] || meta=(-H "x-goog-meta-sha256: $sha")
  session=$(curl -sS -f -X POST -H "x-goog-resumable: start" "${{meta[@]}}" \
    -H "Content-Length: 0" -D - -o /dev/null "$url" \
    | tr -d '\r' | sed -n 's/^[Ll]ocation: //p')
  [ -n "$session" ] || fail "no upload session for $file"
  curl -sS -f -T "$file" -o /dev/null "$session" || fail "uploading $file"
}}

TOTAL=$(awk 'NR == FNR {{ want[$1]; next }} ($3 in want) {{ t += $2 }} END {{ print t + 0 }}' uploads manifest)
DONE=0
say progress "0/$TOTAL"
# sha256 bytes file url, one shard a line; only those the IDE listed.
while read -r sha bytes file url <&3; do
  dest=$(awk -v f="$file" '$1 == f {{ print $2 }}' uploads)
  [ -n "$dest" ] || continue
  say stage "fetching $file"
  curl -fL --retry 8 --retry-delay 10 --retry-all-errors -C - -o "$file.part" "$url" \
    || fail "downloading $file"
  echo "$sha  $file.part" | sha256sum -c --status - \
    || {{ rm -f "$file.part"; fail "$file does not match its digest"; }}
  say stage "uploading $file"
  upload "$file.part" "$dest" "$sha"
  rm -f "$file.part"
  DONE=$((DONE + bytes))
  say progress "$DONE/$TOTAL"
done 3< manifest

dest=$(awk '$1 == "{IMAGE_FILE}" {{ print $2 }}' uploads)
if [ -n "$dest" ]; then
  say stage "saving the server image"
  podman pull {SERVER_IMAGE}
  [ "$(podman image inspect --format '{{{{.Id}}}}' {SERVER_IMAGE})" = {SERVER_IMAGE_ID} ] \
    || fail "the server image is not the pinned one"
  # Saved under a plain local tag: `podman save` writes the manifest anew,
  # so an archive named by the pulled digest no longer matches it and will
  # not load (the first live serving run, 2026-10-03). The ID survives.
  podman tag {SERVER_IMAGE} {LOCAL_TAG}
  rm -f {IMAGE_FILE}
  podman save --format oci-archive -o {IMAGE_FILE} {LOCAL_TAG}
  say stage "uploading the server image"
  upload {IMAGE_FILE} "$dest"
  rm -f {IMAGE_FILE}
fi

say stage "done"
poweroff
"#
    )
}

/// The serving script: pull into memory, check, wait for the window to
/// close, serve.
pub fn serving_script(spec: &ModelSpec) -> String {
    let model = spec.weights.shards[0].file;
    let context = spec.context_tokens;
    // The weights and the image archive, which is removed once loaded.
    let tmpfs_bytes = spec.weights.total_bytes() + (2 << 30);
    let checks: String = spec
        .weights
        .shards
        .iter()
        .map(|shard| format!("{} {}\n", shard.sha256, shard.file))
        .collect();
    format!(
        r#"{PREAMBLE}
# A failure is said, not left as the last thing that was said: the IDE
# waits on these attributes and would otherwise wait out its deadline.
trap 'say serve "failed: line $LINENO: $BASH_COMMAND"' ERR
die() {{ say serve "failed: $1"; exit 1; }}
BOOT=$(cat /proc/sys/kernel/random/boot_id)
KEY=$(md {KEY_ATTRIBUTE})
[ -n "$KEY" ] || die "no key in the metadata"
md {FETCH_ATTRIBUTE} > /run/taste-fetch
[ -s /run/taste-fetch ] || die "the IDE listed nothing to fetch"

say serve "making room for the weights"
mkdir -p "$WEIGHTS"
mountpoint -q "$WEIGHTS" || mount -t tmpfs -o size={tmpfs_bytes},mode=0755 tmpfs "$WEIGHTS"
cd "$WEIGHTS"

# One byte range of one object, written in place. Retried whole rather
# than by curl, since a retry inside curl would append to what the pipe
# has already written; and a 200 is a failure, since it would be the
# whole object at this slice's offset.
slice() {{
  local url=$1 file=$2 from=$3 to=$4 try hdr
  hdr=$(mktemp)
  for try in 1 2 3 4 5; do
    if curl -sS -f --connect-timeout 10 --resolve "{HOST}:443:{GOOGLE_APIS_ADDRESS}" \
         -r "$from-$to" -D "$hdr" "$url" \
       | dd of="$file" bs=4M seek="$from" oflag=seek_bytes conv=notrunc iflag=fullblock status=none \
       && grep -q '^HTTP/[0-9.]* 206' "$hdr"; then
      rm -f "$hdr"
      return 0
    fi
    sleep $((try * 2))
  done
  echo "$file at $from: $(head -1 "$hdr")" >&2
  rm -f "$hdr"
  return 1
}}
verify() {{
  echo "$1  $2" | sha256sum -c --status - || {{ echo "$2 does not match its digest" >&2; return 1; }}
}}
export -f slice verify

while read -r file bytes url; do
  truncate -s "$bytes" "$file"
  from=0
  while [ "$from" -lt "$bytes" ]; do
    to=$((from + {SLICE_BYTES} - 1))
    [ "$to" -lt "$bytes" ] || to=$((bytes - 1))
    echo "$url $file $from $to"
    from=$((to + 1))
  done
done < /run/taste-fetch > /run/taste-slices
TOTAL=$(awk '{{ t += $2 }} END {{ print t + 0 }}' /run/taste-fetch)
PARALLEL=$(( $(nproc) * 4 ))
[ "$PARALLEL" -le 64 ] || PARALLEL=64

say serve "fetching the weights"
( while sleep 5; do
    landed=$(du -s --block-size=1 "$WEIGHTS" | cut -f1)
    say progress "$landed/$TOTAL"
  done ) &
REPORTER=$!
START=$(date +%s.%N)
xargs -P "$PARALLEL" -L 1 bash -c 'set -o pipefail; slice "$@"' _ \
  < /run/taste-slices 2> /run/taste-fetch.err \
  || die "fetching: $(tail -1 /run/taste-fetch.err)"
END=$(date +%s.%N)
kill $REPORTER || true
say progress "$TOTAL/$TOTAL"
# Done with the network: the IDE closes the window on this.
say fetched "$BOOT $TOTAL $(awk -v a="$START" -v b="$END" 'BEGIN {{ printf "%.1f", b - a }}')"

say serve "checking the weights"
xargs -P "$PARALLEL" -L 1 bash -c 'verify "$@"' _ 2> /run/taste-verify.err <<'CHECKS' \
  || die "$(tail -1 /run/taste-verify.err)"
{checks}CHECKS

say serve "loading the server image"
podman load -q -i {IMAGE_FILE}
rm -f {IMAGE_FILE}
[ "$(podman image inspect --format '{{{{.Id}}}}' {LOCAL_TAG})" = {SERVER_IMAGE_ID} ] \
  || die "the server image is not the pinned one"

# The IDE says the window is shut, and this checks it: a fresh connection
# to Google's APIs has to fail before anything from the model runs.
say serve "waiting for the window to close"
until [ "$(md {WINDOW_ATTRIBUTE})" = closed ]; do sleep 2; done
for try in $(seq 30); do
  curl -s -o /dev/null --connect-timeout 3 --resolve "{HOST}:443:{GOOGLE_APIS_ADDRESS}" \
    "https://{HOST}/" || break
  [ "$try" -lt 30 ] || die "the window to Google's APIs is still open"
  sleep 2
done

say serve "loading the model"
# Not exec'd: the server exiting is a failure to say too, and an exec'd
# shell is not there to say it. Its own last line is the reason. Mapped
# in place and not repacked: tmpfs pages cannot be evicted, so a copy
# into the server's own buffers would hold the weights twice.
trap - ERR
rc=0
podman run --rm --name llama --network host --security-opt label=disable \
  -v "$WEIGHTS:/models:ro" {SERVER_IMAGE_ID} \
  -m "/models/{model}" --host 0.0.0.0 --port {SERVER_PORT} \
  --jinja -c {context} --load-mode mmap --no-repack --api-key "$KEY" || rc=$?
said=$(journalctl -q -u taste-serve.service -n 1 -o cat || true)
say serve "failed: the server exited ($rc): ${{said#*: }}"
exit 1
"#
    )
}

/// Readiness: `/health` on loopback, reported once with the boot it is
/// for, so a ready from the last run is never read as this one's.
const READY_SCRIPT: &str = r#"
trap 'say serve "failed: line $LINENO: $BASH_COMMAND"' ERR
until curl -sf -o /dev/null "http://127.0.0.1:PORT/health"; do sleep 5; done
say ready "$(cat /proc/sys/kernel/random/boot_id)"
say serve "ready"
"#;

fn data_url(text: &str) -> String {
    format!(
        "data:;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(text)
    )
}

fn unit(name: &str, description: &str, exec: &str, after: &str, kind: &str) -> Value {
    json!({
        "name": name,
        "enabled": true,
        "contents": format!(
            "[Unit]\nDescription={description}\nWants=network-online.target\n\
             After=network-online.target{after}\n\n\
             [Service]\nType={kind}\nExecStart={exec}\nRestart=no\n\
             StandardOutput=journal+console\nStandardError=journal+console\n\n\
             [Install]\nWantedBy=multi-user.target\n"
        ),
    })
}

fn script_file(path: &str, text: &str) -> Value {
    json!({ "path": path, "mode": 0o755, "contents": { "source": data_url(text) } })
}

fn ignition(files: Vec<Value>, units: Vec<Value>) -> String {
    let mut units = units;
    units.push(json!({ "name": "zincati.service", "mask": true }));
    json!({
        "ignition": { "version": "3.4.0" },
        "storage": { "files": files },
        "systemd": { "units": units },
    })
    .to_string()
}

/// The staging VM's Ignition config.
pub fn staging_ignition(spec: &ModelSpec) -> String {
    ignition(
        vec![script_file(
            "/usr/local/bin/taste-stage",
            &staging_script(spec),
        )],
        vec![unit(
            "taste-stage.service",
            "Mirror the pinned weights into the bucket",
            "/usr/local/bin/taste-stage",
            "",
            "oneshot",
        )],
    )
}

/// The serving VM's Ignition config.
pub fn serving_ignition(spec: &ModelSpec) -> String {
    let ready = format!(
        "{PREAMBLE}{}",
        READY_SCRIPT.replace("PORT", &SERVER_PORT.to_string())
    );
    ignition(
        vec![
            script_file("/usr/local/bin/taste-serve", &serving_script(spec)),
            script_file("/usr/local/bin/taste-ready", &ready),
        ],
        vec![
            unit(
                "taste-serve.service",
                "Serve the model",
                "/usr/local/bin/taste-serve",
                "",
                "simple",
            ),
            unit(
                "taste-ready.service",
                "Say when the model answers",
                "/usr/local/bin/taste-ready",
                " taste-serve.service",
                "oneshot",
            ),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GLM_5_3, GPT_OSS_20B};

    fn decoded(ignition: &str, path: &str) -> String {
        let config: Value = serde_json::from_str(ignition).unwrap();
        let file = config["storage"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == path)
            .unwrap_or_else(|| panic!("{path}"));
        let source = file["contents"]["source"].as_str().unwrap();
        let b64 = source.strip_prefix("data:;base64,").unwrap();
        String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn staging_checks_every_shard_against_its_pin() {
        let script = decoded(&staging_ignition(&GLM_5_3), "/usr/local/bin/taste-stage");
        for shard in GLM_5_3.weights.shards {
            assert!(script.contains(shard.sha256), "{}", shard.file);
            assert!(
                script.contains(&GLM_5_3.weights.url(shard)),
                "{}",
                shard.file
            );
        }
        assert!(script.contains("sha256sum -c"));
        // Uploaded with the digest the IDE signed, never before the check.
        let check = script.find("sha256sum -c").unwrap();
        let upload = script.find("upload \"$file.part\"").unwrap();
        assert!(check < upload);
        assert!(script.contains("x-goog-meta-sha256: $sha"));
        assert!(script.contains(&format!("podman pull {SERVER_IMAGE}")));
        assert!(script.contains(SERVER_IMAGE_ID));
        // Saved by a plain tag, never by the digest it was pulled as.
        assert!(script.contains(&format!(
            "podman save --format oci-archive -o {IMAGE_FILE} {LOCAL_TAG}"
        )));
        assert!(script.trim_end().ends_with("poweroff"));
    }

    #[test]
    fn serving_reaches_only_the_bucket_and_only_by_address() {
        let script = decoded(
            &serving_ignition(&GPT_OSS_20B),
            "/usr/local/bin/taste-serve",
        );
        for forbidden in [
            "podman pull",
            "dnf ",
            "rpm-ostree",
            "huggingface",
            "metadata.google.internal",
        ] {
            assert!(!script.contains(forbidden), "{forbidden}");
        }
        // Every https:// is to Google's APIs, dialled by address.
        for (at, _) in script.match_indices("https://") {
            assert!(
                script[at..].starts_with(&format!("https://{HOST}/")),
                "{}",
                &script[at..at + 40]
            );
        }
        assert!(script.contains(&format!("--resolve \"{HOST}:443:{GOOGLE_APIS_ADDRESS}\"")));
        assert!(script.contains(":/models:ro"));
        assert!(script.contains("--api-key \"$KEY\""));
        assert!(script.contains("-c 65536"));
        assert!(script.contains("/models/gpt-oss-20b-MXFP4.gguf"));
        assert!(script.contains("--no-repack"));
        assert!(script.contains("mount -t tmpfs"));
    }

    #[test]
    fn nothing_from_the_model_runs_before_the_window_is_shut() {
        let script = serving_script(&GPT_OSS_20B);
        let fetched = script.find("say fetched").unwrap();
        let verified = script.find("checking the weights").unwrap();
        let closed = script.find("= closed ]").unwrap();
        let probed = script.find("still open").unwrap();
        let served = script.find("podman run").unwrap();
        assert!(fetched < verified && verified < closed && closed < probed && probed < served);
        // Every shard is checked against the digest pinned here, not one
        // the bucket supplies.
        for shard in GPT_OSS_20B.weights.shards {
            assert!(script.contains(&format!("{} {}\n", shard.sha256, shard.file)));
        }
        assert!(script.contains(SERVER_IMAGE_ID));
    }

    #[test]
    fn both_guests_never_update_themselves() {
        for config in [
            staging_ignition(&GPT_OSS_20B),
            serving_ignition(&GPT_OSS_20B),
        ] {
            let value: Value = serde_json::from_str(&config).unwrap();
            assert!(value["systemd"]["units"]
                .as_array()
                .unwrap()
                .iter()
                .any(|u| u["name"] == "zincati.service" && u["mask"] == true));
            assert_eq!(value["ignition"]["version"], "3.4.0");
        }
    }

    #[test]
    fn every_script_parses() {
        let ready = format!("{PREAMBLE}{}", READY_SCRIPT.replace("PORT", "8080"));
        for (name, script) in [
            ("staging", staging_script(&GLM_5_3)),
            ("staging (smoke)", staging_script(&GPT_OSS_20B)),
            ("serving", serving_script(&GLM_5_3)),
            ("serving (smoke)", serving_script(&GPT_OSS_20B)),
            ("ready", ready),
        ] {
            let checked = std::process::Command::new("bash")
                .args(["-n", "-c", &script])
                .output()
                .expect("bash");
            assert!(
                checked.status.success(),
                "{name}: {}",
                String::from_utf8_lossy(&checked.stderr)
            );
        }
    }

    #[test]
    fn a_slice_is_fetched_and_written_where_it_belongs() {
        // The slice function on its own, against a stand-in curl that
        // serves a range of a local file the way GCS answers a ranged GET.
        let dir = tempfile::tempdir().unwrap();
        let object = dir.path().join("object");
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&object, &data).unwrap();
        crate::testing::install_stub(
            &dir.path().join("curl"),
            r#"#!/usr/bin/env bash
while [ $# -gt 0 ]; do
  case "$1" in
    -r) range=$2; shift ;;
    -D) hdr=$2; shift ;;
    --resolve|--connect-timeout) shift ;;
  esac
  shift
done
from=${range%-*}; to=${range#*-}
echo "HTTP/1.1 206 Partial Content" > "$hdr"
tail -c +$((from + 1)) "$OBJECT" | head -c $((to - from + 1))
"#,
        );
        let script = serving_script(&GPT_OSS_20B);
        let start = script.find("slice() {").unwrap();
        let end = script[start..].find("\n}\n").unwrap() + start + 3;
        let function = &script[start..end];
        let out = dir.path().join("out");
        let run = format!(
            "set -euo pipefail\n{function}\ntruncate -s 10000 {out}\n\
             slice u {out} 4096 9999\nslice u {out} 0 4095\n",
            out = out.display()
        );
        let path = format!(
            "{}:{}",
            dir.path().display(),
            std::env::var("PATH").unwrap()
        );
        let status = std::process::Command::new("bash")
            .args(["-c", &run])
            .env("PATH", path)
            .env("OBJECT", &object)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read(&out).unwrap(), data);
    }

    #[test]
    fn every_guest_says_when_it_fails_and_writes_to_the_console() {
        for config in [
            staging_ignition(&GPT_OSS_20B),
            serving_ignition(&GPT_OSS_20B),
        ] {
            let value: Value = serde_json::from_str(&config).unwrap();
            for unit in value["systemd"]["units"].as_array().unwrap() {
                if let Some(contents) = unit["contents"].as_str() {
                    assert!(
                        contents.contains("StandardOutput=journal+console"),
                        "{}",
                        unit["name"]
                    );
                }
            }
        }
        assert!(serving_script(&GPT_OSS_20B).contains("say serve \"failed:"));
        assert!(staging_script(&GPT_OSS_20B).contains("fail \"line $LINENO"));
    }

    #[test]
    fn the_image_name_follows_the_release() {
        assert_eq!(
            fcos_image("44.20260829.3.1").url(),
            "projects/fedora-coreos-cloud/global/images/fedora-coreos-44-20260829-3-1-gcp-x86-64"
        );
        assert!(image_object().ends_with(&format!("{SERVER_IMAGE_ID}.tar")));
    }
}

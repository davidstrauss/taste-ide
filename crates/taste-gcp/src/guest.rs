//! What the model's two VMs are told to do: an Ignition config for each,
//! for the Fedora CoreOS the local pool boots too, so a guest in GCP and a
//! guest under libvirt are the same system.
//!
//! The **staging VM** formats the weights disk if it is new, downloads
//! every pinned shard (resuming a partial one), checks each against its
//! SHA-256, pulls the pinned llama.cpp server image by digest and saves it
//! to the disk, reports how far it has got through guest attributes, and
//! powers off. It has egress to port 443 and nothing else, and nothing in
//! it runs the weights — a download and a hash are all it does with them.
//!
//! The **serving VM** mounts the weights disk read-only, loads the image
//! the staging VM saved, and runs llama-server on its internal address
//! with the key the IDE wrote into the instance's metadata, reporting
//! `taste/ready` once `/health` answers. It has no address on the internet
//! and no route out, so everything it needs is on that disk or in its own
//! image: no package, no pull, and no name to resolve.
//!
//! Both report by writing guest attributes to the metadata server **by
//! IP**, since the serving network resolves no names at all
//! (`model::DNS_BLACKHOLE`), and the IDE reads them back through the
//! Compute API. Both mask Zincati: a guest is replaced, never updated, and
//! the serving one could not reach an update anyway.

use base64::Engine;
use serde_json::{json, Value};

use crate::model::{ModelSpec, SERVER_PORT};

/// The llama.cpp server, CPU build, pinned by its amd64 manifest digest:
/// release `b11371` (2026-10-03), after the GLM indexer work (#25407).
pub const SERVER_IMAGE: &str =
    "ghcr.io/ggml-org/llama.cpp@sha256:5d58fdc8b2cadfad36c89ecf38b4a50069653500551a6842b0157a2d82d065f3";

/// The Fedora CoreOS release the cloud guests boot: the local pool's own
/// (`taste_devcontainer::guest::RELEASE`), held equal to it by a test in
/// `taste-app`, so a guest in GCP and one under libvirt are one system.
pub const FCOS_RELEASE: &str = "44.20260829.3.1";

/// The weights volume's filesystem label. XFS allows twelve characters,
/// and a longer one fails `mkfs.xfs` outright (the first live staging
/// run, 2026-10-03, with `taste-weights`).
pub const VOLUME_LABEL: &str = "weights";

/// What a staged disk holds, as one label value: a hash of every pinned
/// shard digest and the server image, so a disk staged for this pin is
/// known without a VM, and a pin that moved is staged again.
pub fn staged_label(spec: &ModelSpec) -> String {
    // The whole staging script, which names every shard's digest and the
    // image, and also how they are put on the disk: a change to that is a
    // change to what the disk holds (an image saved the old way would not
    // load), so it stages again — which costs only the image, since the
    // shards already checked are skipped. Not a security boundary; an
    // identity for what was staged.
    config_hash(&staging_script(spec))
}

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

/// The disk label that records [`staged_label`].
pub const STAGED_LABEL: &str = "taste-staged";

/// What the server image is called once the staging VM has saved it.
pub const LOCAL_TAG: &str = "localhost/taste-llama-server:pinned";

/// The guest attribute namespace both VMs write to.
pub const NAMESPACE: &str = "taste";
/// The metadata key llama-server's API key is read from.
pub const KEY_ATTRIBUTE: &str = "taste-key";

/// Fedora CoreOS's GCP image for a release, by the project's naming
/// convention: `44.20260829.3.1` is `fedora-coreos-44-20260829-3-1-gcp-x86-64`
/// in `fedora-coreos-cloud`.
pub fn fcos_image(release: &str) -> crate::resources::ImageRef {
    crate::resources::ImageRef {
        project: "fedora-coreos-cloud".into(),
        name: format!("fedora-coreos-{}-gcp-x86-64", release.replace('.', "-")),
    }
}

/// The shell every unit starts with: a way to say how far it has got.
const PREAMBLE: &str = r#"#!/bin/bash
set -euo pipefail
MD=http://169.254.169.254/computeMetadata/v1/instance
# By IP, never by name: the serving network resolves nothing.
say() {
  curl -sf -X PUT -H "Metadata-Flavor: Google" --data "$2" \
    "$MD/guest-attributes/taste/$1" >/dev/null || true
}
WEIGHTS=/var/mnt/weights
DEV=/dev/disk/by-id/google-weights
"#;

/// The staging script: fetch, check, save, report, power off.
pub fn staging_script(spec: &ModelSpec) -> String {
    let manifest: String = spec
        .weights
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
        .collect();
    let total = spec.weights.total_bytes();
    format!(
        r#"{PREAMBLE}
fail() {{ say stage "failed: $1"; poweroff; exit 1; }}
trap 'fail "line $LINENO: $BASH_COMMAND"' ERR
say stage "formatting"
[ -e "$DEV" ] || fail "no weights disk at $DEV"
blkid "$DEV" >/dev/null 2>&1 || mkfs.xfs -q -L {VOLUME_LABEL} "$DEV"
mkdir -p "$WEIGHTS"
mountpoint -q "$WEIGHTS" || mount "$DEV" "$WEIGHTS"
cd "$WEIGHTS"

# Bytes on disk so far, reported every ten seconds while files arrive.
TOTAL={total}
( while sleep 10; do
    done=$(du -cb --apparent-size *.gguf *.gguf.part 2>/dev/null | tail -1 | cut -f1 || true)
    say progress "${{done:-0}}/$TOTAL"
  done ) &
REPORTER=$!

# sha256 bytes file url, one shard a line.
while read -r sha bytes file url; do
  if [ -f "$file.ok" ] && [ "$(stat -c %s "$file")" = "$bytes" ]; then
    continue
  fi
  say stage "fetching $file"
  curl -fL --retry 8 --retry-delay 10 --retry-all-errors -C - -o "$file.part" "$url" \
    || fail "downloading $file"
  echo "$sha  $file.part" | sha256sum -c --status - || {{ rm -f "$file.part"; fail "$file does not match its digest"; }}
  mv "$file.part" "$file"
  touch "$file.ok"
done <<'MANIFEST'
{manifest}MANIFEST
kill $REPORTER || true
say progress "$TOTAL/$TOTAL"

say stage "saving the server image"
podman pull {SERVER_IMAGE}
podman image inspect --format '{{{{.Id}}}}' {SERVER_IMAGE} > image.id
# Saved under a plain local tag: `podman save` writes the manifest anew,
# so an archive named by the pulled digest no longer matches it and will
# not load (the first live serving run, 2026-10-03). The serving VM runs
# the image by its ID, which the save leaves as it was.
podman tag {SERVER_IMAGE} {LOCAL_TAG}
rm -f llama-server.tar
podman save --format oci-archive -o llama-server.tar {LOCAL_TAG}

sync
cd /
umount "$WEIGHTS"
say stage "done"
poweroff
"#
    )
}

/// The serving script: mount, load, serve.
pub fn serving_script(spec: &ModelSpec) -> String {
    let model = spec.weights.shards[0].file;
    let context = spec.context_tokens;
    format!(
        r#"{PREAMBLE}
# A failure is said, not left as the last thing that was said: the IDE
# waits on these attributes and would otherwise wait out its deadline.
trap 'say serve "failed: line $LINENO: $BASH_COMMAND"' ERR
say serve "mounting the weights"
mkdir -p "$WEIGHTS"
mountpoint -q "$WEIGHTS" || mount -o ro "$DEV" "$WEIGHTS"
KEY=$(curl -sf -H "Metadata-Flavor: Google" "$MD/attributes/{KEY_ATTRIBUTE}")
IMAGE=$(cat "$WEIGHTS/image.id")
podman image exists "$IMAGE" || {{ say serve "loading the server image"; podman load -q -i "$WEIGHTS/llama-server.tar"; }}
say serve "loading the model"
# Not exec'd: the server exiting is a failure to say too, and an exec'd
# shell is not there to say it. Its own last line is the reason.
trap - ERR
rc=0
podman run --rm --name llama --network host --security-opt label=disable \
  -v "$WEIGHTS:/models:ro" "$IMAGE" \
  -m "/models/{model}" --host 0.0.0.0 --port {SERVER_PORT} \
  --jinja -c {context} --load-mode none --api-key "$KEY" || rc=$?
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
            "Fetch and check the pinned weights",
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
    use crate::model::{GLM_5_3, GPT_OSS_20B, WEIGHTS_DEVICE};

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
        assert!(script.contains(&format!("podman pull {SERVER_IMAGE}")));
        // Saved by a plain tag, never by the digest it was pulled as.
        assert!(script.contains(&format!(
            "podman save --format oci-archive -o llama-server.tar {LOCAL_TAG}"
        )));
        assert!(script.trim_end().ends_with("poweroff"));
    }

    #[test]
    fn serving_reaches_for_nothing_outside_its_disk() {
        let script = decoded(
            &serving_ignition(&GPT_OSS_20B),
            "/usr/local/bin/taste-serve",
        );
        // No pull, no package, no download: the VM has no way out.
        for forbidden in ["podman pull", "dnf ", "rpm-ostree", "https://"] {
            assert!(!script.contains(forbidden), "{forbidden}");
        }
        // Read-only here, since GCP cannot attach Hyperdisk Balanced so.
        assert!(script.contains("mount -o ro"));
        assert!(script.contains(":/models:ro"));
        assert!(script.contains("--api-key \"$KEY\""));
        assert!(script.contains("-c 65536"));
        assert!(script.contains("/models/gpt-oss-20b-MXFP4.gguf"));
        // The metadata server by IP, never by name.
        assert!(!script.contains("metadata.google.internal"));
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
            ("serving", serving_script(&GPT_OSS_20B)),
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
    fn the_staged_label_is_stable_and_names_the_pin() {
        let small = staged_label(&GPT_OSS_20B);
        assert_eq!(small, staged_label(&GPT_OSS_20B));
        assert_ne!(small, staged_label(&GLM_5_3));
        assert_eq!(small.len(), 16);
        assert!(small.bytes().all(|b| b.is_ascii_hexdigit()));
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
    fn the_volume_label_fits_xfs() {
        assert!(VOLUME_LABEL.len() <= 12);
        assert!(staging_script(&GPT_OSS_20B).contains(&format!("-L {VOLUME_LABEL} ")));
    }

    #[test]
    fn the_scripts_find_the_disk_the_plan_attaches() {
        // `/dev/disk/by-id/google-<device name>` is GCP's own naming.
        assert!(PREAMBLE.contains(&format!("/dev/disk/by-id/google-{WEIGHTS_DEVICE}")));
    }

    #[test]
    fn the_image_name_follows_the_release() {
        assert_eq!(
            fcos_image("44.20260829.3.1").url(),
            "projects/fedora-coreos-cloud/global/images/fedora-coreos-44-20260829-3-1-gcp-x86-64"
        );
    }
}

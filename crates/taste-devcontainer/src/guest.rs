//! **The guest image: which operating system a VM is built from.**
//!
//! Not to be confused with a **VM provisioner**, which is a place VMs come
//! from (docs/ENVIRONMENTS.md → "VM provisioners"). A provisioner needs an
//! image; this is the image.
//!
//! Fedora CoreOS, and the choice is about portability rather than taste.
//! One stream document names, for a single release, both the qcow2 a local
//! `qemu:///session` boots **and** the AWS, GCP and kubevirt image ids for
//! the same bits — so "which guest" is one answer on a laptop and in a
//! cloud, and a workspace that moves between them is running the same
//! operating system rather than two that have to be kept in step. FCOS's
//! stream also moves on a cadence, which is what keeps the kernel that
//! isolation depends on from ageing in place — through the pin and fresh
//! VMs, never through a guest updating itself (below).
//!
//! # Pinned, not followed
//!
//! The release below is a constant, and moving it is a commit. That is the
//! rule every fetched artifact in this tree follows — `ensure_gvproxy`,
//! `taste_models` — and it matters more here than anywhere: this file is
//! booted as a virtual machine, and an image that changed under the IDE
//! would change what every environment runs without anyone having asked.
//! [`check_stream`] exists so the pin can be *known* to be behind without
//! being moved automatically; it reports, it does not act.
//!
//! The download and the digest check are [`taste_models::fetch_pinned`],
//! reused rather than reimplemented: a second downloader would be a second
//! place to get the verification subtly wrong.
//!
//! # One update path: the pin, and fresh VMs
//!
//! A running guest does **not** update itself. FCOS would, through
//! zincati, and it reboots to do so — every container in the guest killed
//! at a moment nobody chose — so `crate::provision` switches auto-updates
//! off in Ignition. The pin here decides what new VMs are built from, and
//! an environment gets a newer guest by moving to a fresh VM through backup
//! and restore, never by the VM changing under it (David, 2026-09-20).
//! What that asks of the pin is that it be *known* to be behind, which is
//! [`check_stream`]'s job: it reports, so the fleet can offer the move, and
//! it never acts.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The stream this project follows. `stable` rather than `testing`: the
/// guest is infrastructure, and the IDE is where the excitement belongs.
pub const STREAM: &str = "stable";

/// Where [`check_stream`] reads the current release from.
pub const STREAM_URL: &str = "https://builds.coreos.fedoraproject.org/streams/stable.json";

/// The pinned release. Moving this means moving the digests below with it,
/// in the same commit, from the same stream document.
pub const RELEASE: &str = "44.20260829.3.1";

const QCOW_URL_X86_64: &str = "https://builds.coreos.fedoraproject.org/prod/streams/stable/builds/44.20260829.3.1/x86_64/fedora-coreos-44.20260829.3.1-qemu.x86_64.qcow2.xz";
const QCOW_SHA256_X86_64: &str = "a0aa13c4c88519c9c3ee6a16101f84c9f4184fc4eb1e687779b31177e540c12e";
const QCOW_UNCOMPRESSED_SHA256_X86_64: &str =
    "46d90f2b792b17ea3b9105326ee068c6a2756d804757ebb8e4ea3aaf9e7ac75c";
const QCOW_URL_AARCH64: &str = "https://builds.coreos.fedoraproject.org/prod/streams/stable/builds/44.20260829.3.1/aarch64/fedora-coreos-44.20260829.3.1-qemu.aarch64.qcow2.xz";
const QCOW_SHA256_AARCH64: &str =
    "2451e271691faa49f6f7f3d9a87d5aabe8b52ffcbcd7cf8790d2b574c389c10a";
const QCOW_UNCOMPRESSED_SHA256_AARCH64: &str =
    "d526f511cd7bd48c6a369310d09f6e6598b7a1a90b5e6776e99f7cc223e1fcda";

/// One architecture's pinned qemu image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestImage {
    pub release: &'static str,
    pub arch: &'static str,
    pub url: &'static str,
    /// Hex SHA-256 of the compressed artifact, as the stream document
    /// states it.
    pub sha256: &'static str,
    /// Size of the compressed artifact.
    ///
    /// Measured from the published file rather than stated by the stream,
    /// which gives only a digest — so it is a second check that can fail
    /// only when the digest would have failed anyway. It earns its place
    /// twice over regardless: it is what makes a download report honest
    /// progress for a gigabyte, and what lets presence be checked without
    /// rehashing one.
    pub bytes: u64,
    /// Hex SHA-256 of the **decompressed** qcow2, as the release's own
    /// `meta.json` states it. The file a VM actually boots from is this
    /// one, so it is checked in its own right after `xz` has run, not
    /// inferred from the compressed digest.
    pub uncompressed_sha256: &'static str,
    /// Size of the decompressed qcow2, for the presence check.
    pub uncompressed_bytes: u64,
}

impl GuestImage {
    /// The file name this lands under, which carries the release so two
    /// pins can coexist while one is being moved to the other.
    pub fn file_name(&self) -> String {
        format!("fedora-coreos-{}-qemu.{}.qcow2.xz", self.release, self.arch)
    }

    /// Where it lives once fetched.
    pub fn path(&self) -> PathBuf {
        images_dir().join(self.file_name())
    }

    /// Whether it is already here, at the right size. Not by digest: that
    /// is checked when the file arrives, and rehashing a gigabyte on every
    /// launch is a cost with no buyer — the same reasoning `taste_models`
    /// applies to a model, and the same half-measure, since a file this
    /// process wrote and nothing else touches is its own record.
    pub fn is_present(&self) -> bool {
        std::fs::metadata(self.path()).is_ok_and(|meta| meta.len() == self.bytes)
    }

    /// The decompressed qcow2 every VM's disk overlays — the **base
    /// image**, beside the compressed download and named without the
    /// `.xz`, exactly as `xz -d` would name it.
    pub fn base_path(&self) -> PathBuf {
        images_dir().join(format!(
            "fedora-coreos-{}-qemu.{}.qcow2",
            self.release, self.arch
        ))
    }

    /// Whether the base image is here, at the right size — the same
    /// half-measure as [`Self::is_present`], for the same reason.
    pub fn base_is_present(&self) -> bool {
        std::fs::metadata(self.base_path()).is_ok_and(|meta| meta.len() == self.uncompressed_bytes)
    }

    /// The base image, fetched and decompressed if this host has never had
    /// it, and verified against the pinned uncompressed digest before it is
    /// given its name.
    ///
    /// `xz` runs on the host through the same wrapper every host program
    /// is reached by; the sandbox's own `xz` would do, but the file it
    /// writes is one qemu reads from the host, so the host writes it.
    pub async fn ensure_base(
        &self,
        sandboxed: bool,
        progress: impl Fn(u64, u64) + Send + Sync + 'static,
    ) -> Result<PathBuf> {
        let base = self.base_path();
        if self.base_is_present() {
            return Ok(base);
        }
        let compressed = if self.is_present() {
            self.path()
        } else {
            self.fetch(progress).await?
        };
        // Decompress to a `.part` name and rename only once the digest
        // matches, the way `fetch_pinned` does: a half-written or wrong
        // base must never carry the name a VM boots from.
        let part = base.with_extension("qcow2.part");
        let _ = std::fs::remove_file(&part);
        let (program, args) = taste_core::podman::host_argv(
            sandboxed,
            "sh",
            [
                "-c".to_string(),
                "xz -dc -T0 -- \"$1\" > \"$2\"".into(),
                "xz".into(),
                compressed.display().to_string(),
                part.display().to_string(),
            ],
        );
        let output = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .context("running xz")?;
        if !output.status.success() {
            let _ = std::fs::remove_file(&part);
            bail!(
                "decompressing {}: {}",
                compressed.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let expected = self.uncompressed_sha256;
        let hashed_part = part.clone();
        let digest = tokio::task::spawn_blocking(move || sha256_file(&hashed_part))
            .await
            .context("hashing the base image")??;
        if digest != expected {
            let _ = std::fs::remove_file(&part);
            bail!(
                "the decompressed guest image does not match its pin: expected {expected}, got {digest}"
            );
        }
        std::fs::rename(&part, &base).with_context(|| format!("installing {}", base.display()))?;
        Ok(base)
    }

    /// Fetch it, verifying the pin before it takes its name.
    pub async fn fetch(
        &self,
        progress: impl Fn(u64, u64) + Send + Sync + 'static,
    ) -> Result<PathBuf> {
        let target = self.path();
        if let Some(dir) = target.parent() {
            tokio::fs::create_dir_all(dir)
                .await
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        taste_models::fetch_pinned(
            "the guest image",
            self.url,
            self.sha256,
            Some(self.bytes),
            &target,
            progress,
        )
        .await
    }
}

/// The pinned image for an architecture, or an error naming the ones there
/// are — a refusal a person can act on, rather than a silent fall back to
/// the wrong machine's bits.
pub fn image_for(arch: &str) -> Result<GuestImage> {
    match arch {
        "x86_64" => Ok(GuestImage {
            release: RELEASE,
            arch: "x86_64",
            url: QCOW_URL_X86_64,
            sha256: QCOW_SHA256_X86_64,
            bytes: 1_023_919_552,
            uncompressed_sha256: QCOW_UNCOMPRESSED_SHA256_X86_64,
            uncompressed_bytes: 2_100_494_336,
        }),
        "aarch64" => Ok(GuestImage {
            release: RELEASE,
            arch: "aarch64",
            url: QCOW_URL_AARCH64,
            sha256: QCOW_SHA256_AARCH64,
            bytes: 825_142_040,
            uncompressed_sha256: QCOW_UNCOMPRESSED_SHA256_AARCH64,
            uncompressed_bytes: 2_048_196_608,
        }),
        other => bail!("no pinned guest image for {other}; this project pins x86_64 and aarch64"),
    }
}

/// The pinned image for the architecture the IDE is running on.
pub fn image() -> Result<GuestImage> {
    image_for(std::env::consts::ARCH)
}

/// Hex SHA-256 of a file, streamed: the base image is two gigabytes.
fn sha256_file(path: &Path) -> Result<String> {
    use sha2::Digest as _;
    use std::io::Read as _;
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// `$XDG_DATA_HOME/taste-ide/guests`, beside the models directory and
/// separate from it: a bootable operating system is not a model, and a
/// directory that said otherwise would be the kind of small lie that
/// outlives everyone who understood it.
pub fn images_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("taste-ide").join("guests")
}

/// What a stream document says the current release is, and where its
/// artifacts are.
///
/// Read, never applied. This is how the pin is *known* to be behind — the
/// "keeping them updated" half of managing guest images — and moving it is
/// still a person writing the new release and digests into the constants
/// above. An IDE that silently re-pinned itself would be an IDE that
/// changed what every environment boots between two launches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamRelease {
    pub release: String,
    pub url: String,
    pub sha256: String,
    /// The decompressed qcow2's digest, which the stream states beside the
    /// compressed one.
    pub uncompressed_sha256: String,
    /// The same bits as a cloud image, where the stream names one. This is
    /// the portability the choice of FCOS is for, and it is why the check
    /// parses them even though nothing consumes them yet.
    pub cloud: Vec<CloudImage>,
}

/// One cloud's id for the release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudImage {
    /// `aws`, `gcp`, `kubevirt`, … as the stream spells it.
    pub platform: String,
    /// The image name or id, which for AWS is per-region and therefore a
    /// summary rather than a single value.
    pub image: String,
}

/// Parse a Fedora CoreOS stream document for one architecture.
pub fn parse_stream(json: &str, arch: &str) -> Result<StreamRelease> {
    let doc: serde_json::Value =
        serde_json::from_str(json).context("the stream document is not JSON")?;
    let arch_doc = doc
        .get("architectures")
        .and_then(|a| a.get(arch))
        .with_context(|| format!("the stream document has no {arch}"))?;
    let qemu = arch_doc
        .get("artifacts")
        .and_then(|a| a.get("qemu"))
        .context("the stream document has no qemu artifact")?;
    let release = qemu
        .get("release")
        .and_then(|r| r.as_str())
        .context("the qemu artifact has no release")?
        .to_string();
    let disk = qemu
        .get("formats")
        .and_then(|f| f.get("qcow2.xz"))
        .and_then(|f| f.get("disk"))
        .context("the qemu artifact has no qcow2.xz disk")?;
    let url = disk
        .get("location")
        .and_then(|l| l.as_str())
        .context("the disk has no location")?
        .to_string();
    let sha256 = disk
        .get("sha256")
        .and_then(|s| s.as_str())
        .context("the disk has no sha256")?
        .to_string();
    let uncompressed_sha256 = disk
        .get("uncompressed-sha256")
        .and_then(|s| s.as_str())
        .context("the disk has no uncompressed-sha256")?
        .to_string();
    let cloud = arch_doc
        .get("images")
        .and_then(|i| i.as_object())
        .map(|images| {
            let mut out: Vec<CloudImage> = images
                .iter()
                .map(|(platform, value)| CloudImage {
                    platform: platform.clone(),
                    image: cloud_image_name(value),
                })
                .collect();
            out.sort_by(|a, b| a.platform.cmp(&b.platform));
            out
        })
        .unwrap_or_default();
    Ok(StreamRelease {
        release,
        url,
        sha256,
        uncompressed_sha256,
        cloud,
    })
}

/// A cloud entry's human answer. GCP and kubevirt name one image; AWS
/// lists one per region, which is a count rather than a value.
fn cloud_image_name(value: &serde_json::Value) -> String {
    if let Some(name) = value.get("image").and_then(|v| v.as_str()) {
        return name.to_string();
    }
    if let Some(regions) = value.get("regions").and_then(|r| r.as_object()) {
        return format!("{} region(s)", regions.len());
    }
    "unnamed".to_string()
}

/// Whether the pin is the stream's current release.
pub fn pin_is_current(stream: &StreamRelease) -> bool {
    stream.release == RELEASE
}

/// Read a stream document from a file. The fetch is the caller's — this
/// crate does no network of its own, and a check that ran on every launch
/// would be a phone home nobody asked for.
pub fn check_stream(path: &Path, arch: &str) -> Result<StreamRelease> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the stream document {}", path.display()))?;
    parse_stream(&text, arch)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real stream document, trimmed to the shape this parses. Kept as a
    /// fixture rather than fetched: a test that needs the network is a test
    /// that fails for reasons that are not about this code.
    const STREAM_FIXTURE: &str = r#"{
      "stream": "stable",
      "architectures": {
        "x86_64": {
          "artifacts": {
            "qemu": {
              "release": "44.20260829.3.1",
              "formats": {
                "qcow2.xz": {
                  "disk": {
                    "location": "https://builds.coreos.fedoraproject.org/prod/streams/stable/builds/44.20260829.3.1/x86_64/fedora-coreos-44.20260829.3.1-qemu.x86_64.qcow2.xz",
                    "signature": "https://example.invalid/sig",
                    "sha256": "a0aa13c4c88519c9c3ee6a16101f84c9f4184fc4eb1e687779b31177e540c12e",
                    "uncompressed-sha256": "46d90f2b792b17ea3b9105326ee068c6a2756d804757ebb8e4ea3aaf9e7ac75c"
                  }
                }
              }
            }
          },
          "images": {
            "aws": { "regions": { "us-east-1": {}, "eu-west-1": {} } },
            "gcp": { "image": "fedora-coreos-44-20260829-3-1-gcp-x86-64", "project": "fedora-coreos-cloud" },
            "kubevirt": { "image": "quay.io/fedora/fedora-coreos-kubevirt:stable" }
          }
        }
      }
    }"#;

    #[test]
    fn a_stream_document_yields_the_release_and_its_disk() {
        let stream = parse_stream(STREAM_FIXTURE, "x86_64").unwrap();
        assert_eq!(stream.release, "44.20260829.3.1");
        assert!(
            stream.url.ends_with("qemu.x86_64.qcow2.xz"),
            "{}",
            stream.url
        );
        assert_eq!(
            stream.sha256,
            "a0aa13c4c88519c9c3ee6a16101f84c9f4184fc4eb1e687779b31177e540c12e"
        );
    }

    /// The portability claim, checked: one document, one release, and the
    /// cloud ids for the same bits.
    #[test]
    fn the_same_release_names_its_cloud_images() {
        let stream = parse_stream(STREAM_FIXTURE, "x86_64").unwrap();
        let platforms: Vec<&str> = stream.cloud.iter().map(|c| c.platform.as_str()).collect();
        assert_eq!(platforms, vec!["aws", "gcp", "kubevirt"]);
        let gcp = stream.cloud.iter().find(|c| c.platform == "gcp").unwrap();
        assert!(gcp.image.contains("44-20260829-3-1"), "{}", gcp.image);
        // AWS is per-region, so the answer is a count and says so.
        let aws = stream.cloud.iter().find(|c| c.platform == "aws").unwrap();
        assert_eq!(aws.image, "2 region(s)");
    }

    /// The pin and the fixture are the same release, which is the check
    /// that keeps this file honest as the pin moves: the day someone bumps
    /// `RELEASE` without the fixture, this says so.
    #[test]
    fn the_pin_matches_the_document_it_was_taken_from() {
        let stream = parse_stream(STREAM_FIXTURE, "x86_64").unwrap();
        assert!(pin_is_current(&stream));
        let pinned = image_for("x86_64").unwrap();
        assert_eq!(pinned.url, stream.url, "the pinned URL drifted");
        assert_eq!(pinned.sha256, stream.sha256, "the pinned digest drifted");
        assert_eq!(
            pinned.uncompressed_sha256, stream.uncompressed_sha256,
            "the pinned uncompressed digest drifted"
        );
    }

    /// The base image is the download with its `.xz` taken off, beside
    /// it, and absent until it has been decompressed and checked.
    #[test]
    fn the_base_image_is_named_as_xz_would_name_it() {
        let image = image_for("x86_64").unwrap();
        let base = image.base_path();
        assert_eq!(base.parent(), image.path().parent());
        assert_eq!(
            base.file_name().unwrap().to_str().unwrap(),
            image.file_name().trim_end_matches(".xz")
        );
        assert_eq!(image.uncompressed_sha256.len(), 64);
        assert!(
            image.uncompressed_bytes > image.bytes,
            "a qcow2 is larger than its xz"
        );
    }

    /// A pin that is behind is reported, not acted on.
    #[test]
    fn an_older_pin_is_simply_not_current() {
        let moved = STREAM_FIXTURE.replace("44.20260829.3.1", "44.20261002.3.0");
        let stream = parse_stream(&moved, "x86_64").unwrap();
        assert!(!pin_is_current(&stream));
        // ...and the constants have not moved, because nothing here moves
        // them.
        assert_eq!(RELEASE, "44.20260829.3.1");
    }

    #[test]
    fn both_pinned_architectures_resolve_and_others_refuse() {
        for arch in ["x86_64", "aarch64"] {
            let image = image_for(arch).unwrap();
            assert_eq!(image.release, RELEASE);
            assert!(image.url.contains(arch), "{}", image.url);
            assert!(image.file_name().contains(arch));
            assert_eq!(image.sha256.len(), 64, "a sha256 is 64 hex characters");
            assert!(
                image.bytes > 500_000_000,
                "a CoreOS qcow2 is most of a gigabyte; {} looks wrong",
                image.bytes
            );
        }
        let refused = image_for("riscv64").unwrap_err();
        assert!(
            format!("{refused:#}").contains("riscv64"),
            "the refusal should name what was asked for"
        );
    }

    #[test]
    fn a_stream_without_the_architecture_says_which_one() {
        let missing = parse_stream(STREAM_FIXTURE, "aarch64").unwrap_err();
        assert!(format!("{missing:#}").contains("aarch64"));
    }

    /// Guests are not models, and do not live in the models directory.
    #[test]
    fn images_live_in_their_own_directory() {
        let dir = images_dir();
        assert!(dir.ends_with("guests"), "{}", dir.display());
        assert!(dir.starts_with(
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default())
                    .join(".local/share"))
        ));
    }
}

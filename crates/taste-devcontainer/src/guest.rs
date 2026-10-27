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
//! # The stream, followed — for new VMs only
//!
//! New VMs are built from the stream's current release (David,
//! 2026-09-22: "New VMs follow the stable stream, get updated, and old
//! ones get flagged"). Each reconcile reads the stream document
//! ([`refresh_from_stream`]) and records what it names beside the images;
//! [`image`] is that record, and the release compiled in below is only
//! the fallback for a machine that has never reached the stream. The
//! download is still checked — against the digests the stream document
//! states, fetched over HTTPS from Fedora's build host, where it used to
//! be against digests a commit wrote in — so what changed is who vouches
//! for the bits, not whether anything does.
//!
//! # What a VM boots
//!
//! is not the base image as Fedora ships it, once a release's **golden
//! base** is prepared from it (`crate::golden`): that base, booted once and
//! made into one of the IDE's, is what every later VM of the release
//! overlays, and the base image it came from is pruned once no VM
//! overlays it ([`prune_unused_bases`]).
//!
//! The download and the digest check are [`taste_models::fetch_pinned`],
//! reused rather than reimplemented: a second downloader would be a second
//! place to get the verification subtly wrong.
//!
//! # A running guest never changes
//!
//! A running guest does **not** update itself. FCOS would, through
//! zincati, and it reboots to do so — every container in the guest killed
//! at a moment nobody chose — so `crate::provision` switches auto-updates
//! off in Ignition. A VM keeps the release it was built from for its whole
//! life; an environment gets a newer guest by moving to a fresh VM through
//! backup and restore, never by the VM changing under it (David,
//! 2026-09-20). A VM whose release is behind the stream is flagged, and its
//! environments migrate (`crate::registry` → migrations).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use taste_core::{GuestImageFetch, GuestImagePhase};

/// The stream this project follows. `stable` rather than `testing`: the
/// guest is infrastructure, and the IDE is where the excitement belongs.
pub const STREAM: &str = "stable";

/// Where [`check_stream`] reads the current release from.
pub const STREAM_URL: &str = "https://builds.coreos.fedoraproject.org/streams/stable.json";

/// The compiled-in release: what a machine that has never read the stream
/// builds its first VM from. Moving it means moving the digests below with
/// it, in the same commit, from the same stream document — and it needs
/// moving only so that a fresh machine offline does not start far behind.
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

/// One architecture's qemu image: the stream's current release, or the
/// release compiled in when the stream has never been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestImage {
    pub release: String,
    pub arch: &'static str,
    pub url: String,
    /// Hex SHA-256 of the compressed artifact, as the stream document
    /// states it.
    pub sha256: String,
    /// Size of the compressed artifact, where it is known. The compiled-in
    /// release states it, measured from the published file; a release
    /// taken from the stream does not — the stream gives digests only —
    /// and its download's progress takes the server's Content-Length.
    pub bytes: Option<u64>,
    /// Hex SHA-256 of the **decompressed** qcow2, as the release's own
    /// stream entry states it. The file a VM actually boots from is this
    /// one, so it is checked in its own right after `xz` has run, not
    /// inferred from the compressed digest.
    pub uncompressed_sha256: String,
    /// Size of the decompressed qcow2, where known, for the presence check
    /// of a base image made before the digest marker existed.
    pub uncompressed_bytes: Option<u64>,
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
        // The name lands only once the digest matched (`fetch_pinned`), so
        // a file under it is the verified download; the size, where known,
        // is the second check it always was.
        std::fs::metadata(self.path())
            .is_ok_and(|meta| self.bytes.is_none_or(|bytes| meta.len() == bytes))
    }

    /// The decompressed qcow2 every VM's disk overlays — the **base
    /// image**, named as `xz -d` would name the download, which is removed
    /// once the base is verified.
    pub fn base_path(&self) -> PathBuf {
        images_dir().join(format!(
            "fedora-coreos-{}-qemu.{}.qcow2",
            self.release, self.arch
        ))
    }

    /// The **golden base** this release's VMs boot from: the base image
    /// booted once, given everything a VM of the IDE's carries, generalised,
    /// and compressed (`crate::golden`). Named for the release and for
    /// [`crate::golden::REVISION`], so a change to how it is made makes a
    /// new one rather than reusing an old one.
    pub fn golden_path(&self) -> PathBuf {
        images_dir().join(format!(
            "taste-base-{}-{}-r{}.qcow2",
            self.release,
            self.arch,
            crate::golden::REVISION
        ))
    }

    /// Whether the golden base is here. It takes its name only once it is
    /// whole (`crate::golden`), so a file under the name is the base.
    pub fn golden_is_present(&self) -> bool {
        self.golden_path().is_file()
    }

    /// Whether this release can make a VM without a download: its golden
    /// base, or the verified base image it is prepared from.
    pub fn base_is_present(&self) -> bool {
        self.golden_is_present() || self.source_is_present()
    }

    /// Whether the base image is here and was verified: its digest marker
    /// names this release's uncompressed digest — written only once the
    /// hash matched, so nothing rehashes two gigabytes on a launch — or,
    /// for a base made before markers, it is the size the compiled-in
    /// release states.
    pub fn source_is_present(&self) -> bool {
        let Ok(meta) = std::fs::metadata(self.base_path()) else {
            return false;
        };
        let marked = std::fs::read_to_string(self.base_marker_path())
            .is_ok_and(|digest| digest.trim() == self.uncompressed_sha256);
        marked || self.uncompressed_bytes == Some(meta.len())
    }

    /// Where the base image's verified digest is written.
    fn base_marker_path(&self) -> PathBuf {
        self.base_path().with_extension("qcow2.sha256")
    }

    /// This release's fetch lock, held until the returned file is
    /// dropped: an exclusive `flock` on a file beside the base image,
    /// waited for off the runtime's threads.
    async fn fetch_lock(&self) -> Result<std::fs::File> {
        lock_file(self.base_path().with_extension("qcow2.lock")).await
    }

    /// The day the release was built, as its version names it:
    /// `44.20260829.3.1` is 2026-08-29.
    pub fn release_date(&self) -> Option<String> {
        let stamp = self.release.split('.').nth(1)?;
        if stamp.len() != 8 || !stamp.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(format!("{}-{}-{}", &stamp[..4], &stamp[4..6], &stamp[6..]))
    }

    /// The day the base image was checked against the pin: the file takes
    /// its name only once its digest matches, and nothing writes it again,
    /// so its modification time is that moment.
    /// A golden base is made only from a verified one, and only once, so
    /// its own time is the same answer when the base it came from is gone.
    pub fn verified_on(&self) -> Option<String> {
        let modified = std::fs::metadata(self.base_path())
            .or_else(|_| std::fs::metadata(self.golden_path()))
            .ok()?
            .modified()
            .ok()?;
        let secs = modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        Some(civil_date(secs))
    }

    /// Where the image stands, read off the disk: what a header asking
    /// "is anything being fetched, and how far along" gets when no fetch
    /// is reporting to it. The download's part file is the measure while
    /// it exists, the decompression's while that one does, and a base
    /// image at the right size is ready.
    pub fn status(&self) -> GuestImageFetch {
        let unpacked = self.uncompressed_bytes.unwrap_or(0);
        let (phase, done, total) = if self.golden_is_present() {
            (GuestImagePhase::Ready, unpacked, unpacked)
        } else if crate::golden::preparing(self) {
            (GuestImagePhase::Preparing, 0, 0)
        } else if self.source_is_present() {
            (GuestImagePhase::Ready, unpacked, unpacked)
        } else if let Ok(part) = std::fs::metadata(self.base_path().with_extension("qcow2.part")) {
            (GuestImagePhase::Decompressing, part.len(), unpacked)
        } else if self.is_present() {
            (GuestImagePhase::Decompressing, 0, unpacked)
        } else if let Ok(part) = std::fs::metadata(self.path().with_extension("part")) {
            (
                GuestImagePhase::Fetching,
                part.len(),
                self.bytes.unwrap_or(0),
            )
        } else {
            (GuestImagePhase::Absent, 0, 0)
        };
        GuestImageFetch {
            release: self.release.clone(),
            phase,
            done,
            total,
        }
    }

    fn report(
        &self,
        report: &(dyn Fn(GuestImageFetch) + Send + Sync),
        phase: GuestImagePhase,
        done: u64,
        total: u64,
    ) {
        report(GuestImageFetch {
            release: self.release.clone(),
            phase,
            done,
            total,
        });
    }

    /// The base a VM of this release is made from: the golden base when it
    /// is here, else the base image, fetched and decompressed if this host
    /// has never had it, and verified against the pinned uncompressed
    /// digest before it is given its name. Every phase is reported as it
    /// starts and the fetch as it goes. Preparing the golden base from the
    /// base image is [`crate::golden::ensure`]'s, which needs a VM.
    ///
    /// `xz` runs on the host through the same wrapper every host program
    /// is reached by; the sandbox's own `xz` would do, but the file it
    /// writes is one qemu reads from the host, so the host writes it.
    pub async fn ensure_base(
        &self,
        sandboxed: bool,
        report: Arc<dyn Fn(GuestImageFetch) + Send + Sync>,
    ) -> Result<PathBuf> {
        let base = self.base_path();
        let unpacked = self.uncompressed_bytes.unwrap_or(0);
        if self.golden_is_present() {
            self.report(&*report, GuestImagePhase::Ready, unpacked, unpacked);
            return Ok(self.golden_path());
        }
        if self.source_is_present() {
            self.report(&*report, GuestImagePhase::Ready, unpacked, unpacked);
            return Ok(base);
        }
        // One fetch of a release at a time, on this machine: the fetch
        // ahead of the moves and a move's own fetch both wrote the same
        // `.part`, halving each other's bandwidth, and the slower one's
        // rename found the file already gone (2026-10-22, a move that
        // waited out both). A second caller waits here and finds the
        // image present. A file lock, so another project's IDE on this
        // machine waits the same way.
        let _lock = self.fetch_lock().await?;
        if self.golden_is_present() {
            self.report(&*report, GuestImagePhase::Ready, unpacked, unpacked);
            return Ok(self.golden_path());
        }
        if self.source_is_present() {
            self.report(&*report, GuestImagePhase::Ready, unpacked, unpacked);
            return Ok(base);
        }
        // A download already here that will not decompress or verify was
        // damaged after it landed — a disk, an earlier IDE's overlapping
        // fetches — and is fetched again at once, where re-fetching is
        // possible (David, 2026-10-22: "The IDE should be better able to
        // recover from stuff like corrupt image files"). One fetched just
        // now was checked against its pin, so its failure is the answer.
        let mut fresh = false;
        loop {
            let compressed = if self.is_present() {
                self.path()
            } else {
                fresh = true;
                let (release, known, fetch_report) =
                    (self.release.clone(), self.bytes, report.clone());
                self.report(&*report, GuestImagePhase::Fetching, 0, known.unwrap_or(0));
                self.fetch(move |done, total| {
                    fetch_report(GuestImageFetch {
                        release: release.clone(),
                        phase: GuestImagePhase::Fetching,
                        done,
                        total: known.unwrap_or(total),
                    })
                })
                .await?
            };
            match self.unpack(&compressed, sandboxed, report.clone()).await {
                Ok(base) => return Ok(base),
                Err(e) if !fresh => {
                    taste_core::app_log::push(
                        "warn",
                        "substrate",
                        &format!(
                            "the guest image {} here was damaged ({e:#}); fetching it again",
                            self.release
                        ),
                    );
                    // Not removed, it would be found again on the next
                    // pass: the damage is the answer then.
                    if compressed.exists() && std::fs::remove_file(&compressed).is_err() {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Decompress `compressed` to the base image and verify it against the
    /// pinned digest before it takes its name. On a failure the download
    /// is removed with what it made, so nothing damaged is tried twice.
    async fn unpack(
        &self,
        compressed: &std::path::Path,
        sandboxed: bool,
        report: Arc<dyn Fn(GuestImageFetch) + Send + Sync>,
    ) -> Result<PathBuf> {
        let base = self.base_path();
        let unpacked = self.uncompressed_bytes.unwrap_or(0);
        self.report(&*report, GuestImagePhase::Decompressing, 0, unpacked);
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
        // xz says nothing as it goes, so the part file's size is the
        // progress: read every second while it runs.
        let unpacking = {
            let (part, report, release) = (part.clone(), report.clone(), self.release.clone());
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    let done = std::fs::metadata(&part).map_or(0, |meta| meta.len());
                    report(GuestImageFetch {
                        release: release.clone(),
                        phase: GuestImagePhase::Decompressing,
                        done,
                        total: unpacked,
                    });
                }
            })
        };
        let output = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .await;
        unpacking.abort();
        let output = output.context("running xz")?;
        if !output.status.success() {
            let _ = std::fs::remove_file(&part);
            // The download is what would not decompress, and a stream's
            // release names no size to doubt it by (`is_present`): left,
            // every later try decompressed the same bad file (2026-10-22, a
            // `.xz` two overlapping fetches had written into). Gone, the
            // next try fetches it again.
            let _ = std::fs::remove_file(compressed);
            bail!(
                "decompressing {}: {}; the download was removed, and the next try fetches \
                 it again",
                compressed.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        // Two gigabytes read back and hashed is most of a minute on a slow
        // disk, so the check reports how far it has read, as the fetch does.
        let size = std::fs::metadata(&part).map_or(unpacked, |meta| meta.len());
        self.report(&*report, GuestImagePhase::Verifying, 0, size);
        let expected = self.uncompressed_sha256.clone();
        let hashed_part = part.clone();
        let (release, hash_report) = (self.release.clone(), report.clone());
        let digest = tokio::task::spawn_blocking(move || {
            sha256_file(&hashed_part, |done| {
                hash_report(GuestImageFetch {
                    release: release.clone(),
                    phase: GuestImagePhase::Verifying,
                    done,
                    total: size,
                })
            })
        })
        .await
        .context("hashing the base image")??;
        if digest != expected {
            let _ = std::fs::remove_file(&part);
            // The download decompressed to the wrong bytes: fetched again
            // next time, as above.
            let _ = std::fs::remove_file(compressed);
            bail!(
                "the decompressed guest image does not match its pin: expected {expected}, got {digest}"
            );
        }
        std::fs::rename(&part, &base).with_context(|| format!("installing {}", base.display()))?;
        std::fs::write(self.base_marker_path(), format!("{expected}\n"))
            .with_context(|| format!("marking {} verified", base.display()))?;
        // The download has done its work: every VM is made from the base,
        // and a base found damaged is fetched again ([`Self::ensure_base`]).
        let _ = std::fs::remove_file(compressed);
        let size = std::fs::metadata(&base).map_or(unpacked, |meta| meta.len());
        self.report(&*report, GuestImagePhase::Ready, size, size);
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
            &self.url,
            &self.sha256,
            self.bytes,
            &target,
            progress,
        )
        .await
    }
}

/// `YYYY-MM-DD` of a Unix time, in UTC.
pub fn date_of(secs: u64) -> String {
    civil_date(secs)
}

/// `YYYY-MM-DD` of a Unix time, in UTC (Hinnant's days-to-civil).
fn civil_date(secs: u64) -> String {
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The compiled-in release for an architecture, or an error naming the
/// ones there are — a refusal a person can act on, rather than a silent
/// fall back to the wrong machine's bits.
pub fn pinned_for(arch: &str) -> Result<GuestImage> {
    let image =
        |arch, url: &str, sha256: &str, bytes, uncompressed_sha256: &str, unpacked| GuestImage {
            release: RELEASE.to_string(),
            arch,
            url: url.to_string(),
            sha256: sha256.to_string(),
            bytes: Some(bytes),
            uncompressed_sha256: uncompressed_sha256.to_string(),
            uncompressed_bytes: Some(unpacked),
        };
    match arch {
        "x86_64" => Ok(image(
            "x86_64",
            QCOW_URL_X86_64,
            QCOW_SHA256_X86_64,
            1_023_919_552,
            QCOW_UNCOMPRESSED_SHA256_X86_64,
            2_100_494_336,
        )),
        "aarch64" => Ok(image(
            "aarch64",
            QCOW_URL_AARCH64,
            QCOW_SHA256_AARCH64,
            825_142_040,
            QCOW_UNCOMPRESSED_SHA256_AARCH64,
            2_048_196_608,
        )),
        other => bail!("no guest image for {other}; this project runs on x86_64 and aarch64"),
    }
}

/// The image new VMs are built from on `arch`: the stream's release as
/// last read ([`refresh_from_stream`]), else the compiled-in one. The
/// compiled-in release's own sizes are kept when the stream names it too.
pub fn image_for(arch: &str) -> Result<GuestImage> {
    let named = named_for(arch)?;
    if taste_core::network::metered() && !named.base_is_present() {
        if let Some(here) = newest_present(arch) {
            return Ok(here);
        }
    }
    Ok(named)
}

/// Whether [`image`] is a release already here in place of the one the
/// stream named, because the connection is metered.
pub fn held_back_by_metering() -> Option<String> {
    let arch = std::env::consts::ARCH;
    let named = named_for(arch).ok()?;
    let image = image_for(arch).ok()?;
    (image.release != named.release).then_some(named.release)
}

/// The newest base image on this machine that was verified when it
/// arrived — its digest marker beside it — as a [`GuestImage`] that needs
/// no download. On a metered connection a new VM boots this rather than
/// fetch the stream's newer release: a gigabyte by the IDE's own choice is
/// what a metered connection is for not spending (`taste_core::network`).
fn newest_present(arch: &str) -> Option<GuestImage> {
    newest_present_in(&images_dir(), arch)
}

/// [`newest_present`] in `dir`: a base image there counts when its digest
/// marker is beside it, which is written only once the digest matched.
fn newest_present_in(dir: &Path, arch: &str) -> Option<GuestImage> {
    let suffix = format!("-qemu.{arch}.qcow2");
    let golden_suffix = format!("-{arch}-r{}.qcow2", crate::golden::REVISION);
    let mut found: Vec<GuestImage> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            // A golden base is whole once it has its name, and is the
            // answer with no digest to carry.
            let (release, digest) = if let Some(release) = name
                .strip_prefix("taste-base-")
                .and_then(|rest| rest.strip_suffix(&golden_suffix))
            {
                (release.to_string(), String::new())
            } else {
                let release = name
                    .strip_prefix("fedora-coreos-")?
                    .strip_suffix(&suffix)?
                    .to_string();
                let marker = entry.path().with_extension("qcow2.sha256");
                let digest = std::fs::read_to_string(marker).ok()?.trim().to_string();
                (release, digest)
            };
            Some(GuestImage {
                release,
                arch: if arch == "aarch64" {
                    "aarch64"
                } else {
                    "x86_64"
                },
                url: String::new(),
                sha256: String::new(),
                bytes: None,
                uncompressed_sha256: digest,
                uncompressed_bytes: None,
            })
        })
        .collect();
    found.sort_by(|a, b| {
        if release_is_behind(&a.release, &b.release) {
            std::cmp::Ordering::Less
        } else if release_is_behind(&b.release, &a.release) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    found.pop()
}

/// The release the stream last named, else the compiled-in one.
fn named_for(arch: &str) -> Result<GuestImage> {
    let pinned = pinned_for(arch)?;
    match read_stream_record(arch) {
        Some(record) if record.release == pinned.release => Ok(pinned),
        Some(record) => Ok(GuestImage {
            release: record.release,
            arch: pinned.arch,
            url: record.url,
            sha256: record.sha256,
            bytes: None,
            uncompressed_sha256: record.uncompressed_sha256,
            uncompressed_bytes: None,
        }),
        None => Ok(pinned),
    }
}

/// The image new VMs are built from on this machine's architecture.
pub fn image() -> Result<GuestImage> {
    image_for(std::env::consts::ARCH)
}

/// What the stream said when it was last read.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StreamRecord {
    pub release: String,
    pub url: String,
    pub sha256: String,
    pub uncompressed_sha256: String,
    /// Unix seconds.
    pub checked_at: u64,
}

fn stream_record_path(arch: &str) -> PathBuf {
    images_dir().join(format!("{STREAM}-{arch}.json"))
}

fn read_stream_record(arch: &str) -> Option<StreamRecord> {
    let text = std::fs::read_to_string(stream_record_path(arch)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The last reading of the stream on this machine's architecture.
pub fn last_stream_check() -> Option<StreamRecord> {
    read_stream_record(std::env::consts::ARCH)
}

/// Where the stream's releases are published, and so the only host an
/// artifact URL it names may point at.
const RELEASE_HOST: &str = "https://builds.coreos.fedoraproject.org/";

/// Read the stream document and record its current release for this
/// machine's architecture, which is what new VMs are built from from then
/// on. Refuses a document whose artifact is not on Fedora's build host.
/// Each reconcile calls it; failing — offline, the host down — leaves the
/// last record, or the compiled-in release, in force.
pub async fn refresh_from_stream() -> Result<StreamRecord> {
    let arch = std::env::consts::ARCH;
    let text =
        taste_models::fetch_text(STREAM_URL, 4 << 20, std::time::Duration::from_secs(15)).await?;
    let stream = parse_stream(&text, arch)?;
    if !stream.url.starts_with(RELEASE_HOST) {
        bail!(
            "the stream names its image at {}, which is not Fedora's build host",
            stream.url
        );
    }
    let checked_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let record = StreamRecord {
        release: stream.release,
        url: stream.url,
        sha256: stream.sha256,
        uncompressed_sha256: stream.uncompressed_sha256,
        checked_at,
    };
    std::fs::create_dir_all(images_dir())?;
    let path = stream_record_path(arch);
    let part = path.with_extension("json.part");
    std::fs::write(&part, serde_json::to_string_pretty(&record)?)?;
    std::fs::rename(&part, &path)?;
    Ok(record)
}

/// Whether `release` is behind `current`. Fedora CoreOS versions are
/// `<major>.<yyyymmdd>.<stream>.<n>`, compared numerically field by field.
pub fn release_is_behind(release: &str, current: &str) -> bool {
    let fields = |r: &str| -> Vec<u64> { r.split('.').map(|f| f.parse().unwrap_or(0)).collect() };
    fields(release) < fields(current)
}

/// Hex SHA-256 of a file, streamed: the base image is two gigabytes.
/// A file's SHA-256, telling `progress` how many bytes it has read every
/// 32 MiB.
fn sha256_file(path: &Path, progress: impl Fn(u64)) -> Result<String> {
    use sha2::Digest as _;
    use std::io::Read as _;
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut done = 0u64;
    const STEP: u64 = 32 << 20;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        let before = done;
        done += n as u64;
        if before / STEP != done / STEP {
            progress(done);
        }
    }
    progress(done);
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// An exclusive `flock` on `path`, made if it is not there, held until the
/// returned file is dropped and waited for off the runtime's threads: one
/// fetch, or one preparation of a base, at a time on this machine, and a
/// second caller waits and finds the work done. A file lock, so another
/// project's IDE waits the same way.
pub(crate) async fn lock_file(path: PathBuf) -> Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    tokio::task::spawn_blocking(move || {
        use std::os::fd::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        // SAFETY: the descriptor is the open file's, alive for the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("locking {}", path.display()));
        }
        Ok(file)
    })
    .await
    .context("waiting for a lock on the guest images")?
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

/// The base images no VM's disk is built on any more, removed with their
/// downloads and digest markers; the bytes that freed. The current
/// release's base is always kept — the next VM is made from it — and when
/// the current release cannot be told, nothing is removed, since removing
/// it would cost the next launch a gigabyte's download. Once its golden
/// base is here, that is the one kept, and the base image it was prepared
/// from goes as soon as no VM overlays it: a new golden base is prepared
/// from a fresh download (David, 2026-10-26: "toss out all other source
/// data (and redownload if I need a new VM base image)").
///
/// Each release is a base of its own, and a VM keeps the release it was
/// made from for life, so a base outlives its pin for as long as one disk
/// overlays it — and nothing removed it after that (2026-10-11: a base and
/// its download, three gigabytes, under VMs idle since September).
pub fn prune_unused_bases() -> u64 {
    let Ok(current) = image() else {
        return 0;
    };
    let keep = if current.golden_is_present() {
        current.golden_path()
    } else {
        current.base_path()
    };
    prune_bases_in(&images_dir(), &crate::provision::disks_dir(), &keep)
}

/// The downloads whose base image is here and verified, removed: a
/// gigabyte each, kept by earlier IDEs after the unpack, for which nothing
/// has a use (David, 2026-10-22: "As long as you can reuse the created base
/// image, we don't need to keep the compressed one"). The bytes that freed.
pub fn drop_unpacked_downloads() -> u64 {
    drop_unpacked_in(&images_dir())
}

fn drop_unpacked_in(images: &Path) -> u64 {
    let mut freed = 0;
    for entry in std::fs::read_dir(images).into_iter().flatten().flatten() {
        let download = entry.path();
        let Some(base) = download
            .to_str()
            .and_then(|path| path.strip_suffix(".xz"))
            .filter(|base| base.ends_with(".qcow2"))
            .map(PathBuf::from)
        else {
            continue;
        };
        // Verified: the digest marker is written only once the hash matched.
        if !(base.is_file() && base.with_extension("qcow2.sha256").is_file()) {
            continue;
        }
        let size = entry.metadata().map_or(0, |m| m.len());
        if std::fs::remove_file(&download).is_ok() {
            freed += size;
        }
    }
    freed
}

fn prune_bases_in(images: &Path, disks: &Path, current: &Path) -> u64 {
    let used: std::collections::HashSet<std::ffi::OsString> = std::fs::read_dir(disks)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| qcow2_backing_file(&entry.path()))
        .filter_map(|backing| backing.file_name().map(|n| n.to_os_string()))
        .collect();
    let mut freed = 0;
    for entry in std::fs::read_dir(images).into_iter().flatten().flatten() {
        let path = entry.path();
        let is_base = path.is_file() && path.extension().is_some_and(|x| x == "qcow2");
        let Some(name) = path.file_name() else {
            continue;
        };
        if !is_base || path == current || used.contains(name) {
            continue;
        }
        for file in [
            path.clone(),
            path.with_extension("qcow2.xz"),
            path.with_extension("qcow2.sha256"),
        ] {
            let size = std::fs::metadata(&file).map_or(0, |m| m.len());
            if std::fs::remove_file(&file).is_ok() {
                freed += size;
                tracing::info!("removed {}: no VM is built on it", file.display());
            }
        }
    }
    freed
}

/// The file a qcow2 overlays, from its header: the backing file's offset
/// as a big-endian u64 at 8 and its length as a u32 at 16 (the format's
/// documented layout). `None` for a disk with no backing file, or
/// anything that is not a qcow2.
pub fn qcow2_backing_file(path: &Path) -> Option<PathBuf> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0u8; 20];
    file.read_exact(&mut header).ok()?;
    if &header[..4] != b"QFI\xfb" {
        return None;
    }
    let offset = u64::from_be_bytes(header[8..16].try_into().ok()?);
    let length = u32::from_be_bytes(header[16..20].try_into().ok()?) as usize;
    if offset == 0 || length == 0 || length > 4096 {
        return None;
    }
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut name = vec![0u8; length];
    file.read_exact(&mut name).ok()?;
    use std::os::unix::ffi::OsStringExt;
    let backing = PathBuf::from(std::ffi::OsString::from_vec(name));
    Some(if backing.is_absolute() {
        backing
    } else {
        path.parent()?.join(backing)
    })
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

/// Whether the compiled-in release is the stream's current one.
pub fn pin_is_current(stream: &StreamRelease) -> bool {
    stream.release == RELEASE
}

/// Read a stream document from a file — the offline half of
/// [`refresh_from_stream`], for a document already on disk.
pub fn check_stream(path: &Path, arch: &str) -> Result<StreamRelease> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the stream document {}", path.display()))?;
    parse_stream(&text, arch)
}

#[cfg(test)]
mod tests {
    /// A qcow2 header, as much of it as the backing-file reader looks at,
    /// overlaying `backing` (or nothing).
    fn fake_qcow2(path: &Path, backing: Option<&str>) {
        let mut bytes = vec![0u8; 512];
        bytes[..4].copy_from_slice(b"QFI\xfb");
        bytes[4..8].copy_from_slice(&3u32.to_be_bytes());
        if let Some(backing) = backing {
            bytes[8..16].copy_from_slice(&256u64.to_be_bytes());
            bytes[16..20].copy_from_slice(&(backing.len() as u32).to_be_bytes());
            bytes[256..256 + backing.len()].copy_from_slice(backing.as_bytes());
        }
        std::fs::write(path, bytes).unwrap();
    }

    /// A base no disk overlays goes with its download and marker; the
    /// current one and one still under a disk stay.
    #[test]
    fn only_a_base_nothing_is_built_on_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path();
        let disks = images.join("machines");
        std::fs::create_dir_all(&disks).unwrap();
        let old = images.join("fedora-coreos-44.1-qemu.x86_64.qcow2");
        let used = images.join("fedora-coreos-44.2-qemu.x86_64.qcow2");
        let current = images.join("fedora-coreos-44.3-qemu.x86_64.qcow2");
        for base in [&old, &used, &current] {
            fake_qcow2(base, None);
            std::fs::write(base.with_extension("qcow2.xz"), "xz").unwrap();
        }
        std::fs::write(old.with_extension("qcow2.sha256"), "digest\n").unwrap();
        fake_qcow2(
            &disks.join("taste-a-b.qcow2"),
            Some(&used.display().to_string()),
        );
        assert_eq!(
            qcow2_backing_file(&disks.join("taste-a-b.qcow2")),
            Some(used.clone())
        );

        let freed = prune_bases_in(images, &disks, &current);
        assert!(freed > 0);
        assert!(!old.exists());
        assert!(!old.with_extension("qcow2.xz").exists());
        assert!(!old.with_extension("qcow2.sha256").exists());
        assert!(used.exists() && current.exists());
        assert!(current.with_extension("qcow2.xz").exists());
    }

    /// A download goes once its base is here and verified; one still being
    /// unpacked, or whose base was never verified, stays.
    #[test]
    fn a_download_goes_once_its_base_is_verified() {
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path();
        let verified = images.join("fedora-coreos-44.1-qemu.x86_64.qcow2");
        let unverified = images.join("fedora-coreos-44.2-qemu.x86_64.qcow2");
        let unpacking = images.join("fedora-coreos-44.3-qemu.x86_64.qcow2");
        for base in [&verified, &unverified, &unpacking] {
            std::fs::write(base.with_extension("qcow2.xz"), "xz").unwrap();
        }
        fake_qcow2(&verified, None);
        std::fs::write(verified.with_extension("qcow2.sha256"), "digest\n").unwrap();
        fake_qcow2(&unverified, None);
        fake_qcow2(&unpacking.with_extension("qcow2.part"), None);

        assert_eq!(drop_unpacked_in(images), 2);
        assert!(!verified.with_extension("qcow2.xz").exists());
        assert!(verified.exists());
        assert!(unverified.with_extension("qcow2.xz").exists());
        assert!(unpacking.with_extension("qcow2.xz").exists());
    }

    #[test]
    fn a_release_is_behind_by_its_fields_numerically() {
        assert!(release_is_behind("44.20260829.3.1", "44.20260912.3.0"));
        assert!(release_is_behind("43.20261230.3.1", "44.20260105.3.0"));
        assert!(!release_is_behind("44.20260829.3.1", "44.20260829.3.1"));
        assert!(!release_is_behind("44.20260912.3.0", "44.20260829.3.1"));
        assert!(release_is_behind("44.20260829.3.9", "44.20260829.3.10"));
    }

    #[test]
    fn the_release_names_its_day_and_a_unix_time_its_date() {
        let image = pinned_for("x86_64").unwrap();
        assert_eq!(image.release_date().as_deref(), Some("2026-08-29"));
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(1_789_000_000), "2026-09-10");
    }

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
        let pinned = pinned_for("x86_64").unwrap();
        assert_eq!(pinned.url, stream.url, "the pinned URL drifted");
        assert_eq!(pinned.sha256, stream.sha256, "the pinned digest drifted");
        assert_eq!(
            pinned.uncompressed_sha256, stream.uncompressed_sha256,
            "the pinned uncompressed digest drifted"
        );
    }

    /// A reading's fraction and whether its phase is one to show.
    #[test]
    fn a_reading_has_a_fraction_where_its_phase_has_a_measure() {
        let fetching = GuestImageFetch {
            release: "1.0".into(),
            phase: GuestImagePhase::Fetching,
            done: 25,
            total: 100,
        };
        assert_eq!(fetching.fraction(), Some(0.25));
        assert!(fetching.phase.active());
        assert!(!GuestImagePhase::Ready.active());
        assert!(!GuestImagePhase::Absent.active());
        assert_eq!(
            GuestImageFetch {
                release: "1.0".into(),
                phase: GuestImagePhase::Verifying,
                done: 0,
                total: 0
            }
            .fraction(),
            None
        );
        // The disk's answer for the pinned image names the release either
        // way, whatever this machine has of it.
        let status = pinned_for("x86_64").unwrap().status();
        assert_eq!(status.release, RELEASE);
    }

    /// The base image is the download with its `.xz` taken off, beside
    /// it, and absent until it has been decompressed and checked.
    #[test]
    fn the_base_image_is_named_as_xz_would_name_it() {
        let image = pinned_for("x86_64").unwrap();
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
            let image = pinned_for(arch).unwrap();
            assert_eq!(image.release, RELEASE);
            assert!(image.url.contains(arch), "{}", image.url);
            assert!(image.file_name().contains(arch));
            assert_eq!(image.sha256.len(), 64, "a sha256 is 64 hex characters");
            assert!(
                image.bytes.unwrap() > 500_000_000,
                "a CoreOS qcow2 is most of a gigabyte; {:?} looks wrong",
                image.bytes
            );
        }
        let refused = pinned_for("riscv64").unwrap_err();
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

    /// On a metered connection a new VM boots the newest verified base
    /// already here; a base with no digest marker beside it was never
    /// verified and does not count.
    #[test]
    fn the_newest_verified_base_here_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let base = |release: &str, marked: bool| {
            let path = dir
                .path()
                .join(format!("fedora-coreos-{release}-qemu.x86_64.qcow2"));
            std::fs::write(&path, b"qcow").unwrap();
            if marked {
                std::fs::write(
                    path.with_extension("qcow2.sha256"),
                    format!("{release}-digest\n"),
                )
                .unwrap();
            }
        };
        assert!(newest_present_in(dir.path(), "x86_64").is_none());
        base("44.20260829.3.1", true);
        base("44.20260913.3.2", true);
        base("44.20260927.3.0", false);
        let here = newest_present_in(dir.path(), "x86_64").unwrap();
        assert_eq!(here.release, "44.20260913.3.2");
        assert_eq!(here.uncompressed_sha256, "44.20260913.3.2-digest");
        assert!(newest_present_in(dir.path(), "aarch64").is_none());
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

//! Keys in this machine's TPM, through the host's `tpm2-tools`.
//!
//! The identity's three keys (ENVIRONMENTS → "The IDE's identity lives in
//! the TPM") are P-256 signing keys created under the owner hierarchy's
//! primary key. What the IDE keeps of each is two blobs: the public half,
//! and the private half **wrapped by the TPM** — encrypted under a key
//! that never leaves the chip, so the file is useless on any other machine
//! and to anyone who copies it. To sign, the primary is re-derived (it is
//! deterministic: the same template under the same hierarchy yields the
//! same key), the blobs are loaded under it, and the TPM signs a SHA-256
//! digest the IDE hands it.
//!
//! # Why the CLI rather than the TSS in-process
//!
//! The tools are the TSS's documented command line, on the host already
//! (Fedora ships `tpm2-tools` beside `tpm2-tss`), and they are reached the
//! way every other host program is (`taste_core::podman::host_argv`).
//! Linking the TSS instead would put a C library into the Rust build and,
//! inside the Flatpak, need `--device=all`, the sandbox's broadest grant,
//! to open one device. The price is a few process spawns per signature,
//! and signatures are rare: one per token exchange, about hourly, and one
//! per new connection to the model's VM. A signature blocks its thread for
//! that long, which is acceptable for a handshake and would not be for a
//! hot path.
//!
//! # Which TPM
//!
//! The kernel's resource manager, `/dev/tpmrm0`, always: it gives each
//! process its own view of the TPM's few object slots and flushes them
//! when the process ends. A TPM reached without one (a software TPM in a
//! test) has its transient objects flushed between steps instead, which
//! is the same thing done by hand. The device is `root:tss 0660`, so the
//! user joins `tss` once; [`describe_failure`] says so in those words when
//! that is what went wrong.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::signer::{subject_public_key_info, KeySigner};

/// The kernel's resource-managed TPM device.
pub const DEVICE_TCTI: &str = "device:/dev/tpmrm0";

/// What a P-256 signing key is: sign only, bound to this TPM and this
/// parent, generated inside the chip, usable without a password, and
/// exempt from dictionary-attack lockout (it has no password to guess).
const KEY_ATTRIBUTES: &str = "fixedtpm|fixedparent|sensitivedataorigin|userwithauth|sign|noda";

/// How to reach a TPM.
#[derive(Debug, Clone)]
pub struct Tpm {
    tcti: String,
    /// Flush transient objects between steps, for a TPM with no resource
    /// manager in front of it.
    flush: bool,
    sandboxed: bool,
}

impl Tpm {
    /// This machine's TPM, through the kernel's resource manager.
    pub fn device() -> Self {
        Self {
            tcti: DEVICE_TCTI.to_string(),
            flush: false,
            sandboxed: taste_core::podman::sandboxed(),
        }
    }

    /// A TPM at `tcti` with no resource manager — a software TPM for tests
    /// (`swtpm:host=127.0.0.1,port=N`).
    pub fn unmanaged(tcti: &str) -> Self {
        Self {
            tcti: tcti.to_string(),
            flush: true,
            sandboxed: taste_core::podman::sandboxed(),
        }
    }

    /// The argv for one tool, on the host whatever sandbox the IDE is in.
    fn argv(&self, tool: &str, args: &[&str]) -> (String, Vec<String>) {
        let mut all = vec!["-T".to_string(), self.tcti.clone()];
        all.extend(args.iter().map(|a| a.to_string()));
        taste_core::podman::host_argv(self.sandboxed, tool, all)
    }

    fn run(&self, tool: &str, args: &[&str]) -> Result<()> {
        let (program, argv) = self.argv(tool, args);
        let output = Command::new(&program)
            .args(&argv)
            .output()
            .with_context(|| format!("running {tool}"))?;
        if self.flush {
            let (program, argv) = self.argv("tpm2_flushcontext", &["-t"]);
            let _ = Command::new(program).args(argv).output();
        }
        if !output.status.success() {
            bail!(describe_failure(
                tool,
                &String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(())
    }

    /// Re-derive the owner hierarchy's primary into `context`.
    fn primary(&self, context: &Path) -> Result<()> {
        self.run(
            "tpm2_createprimary",
            &["-C", "o", "-G", "ecc", "-c", path_str(context)?],
        )
    }

    /// Create a new P-256 signing key and keep its blobs in `dir`.
    pub fn create_key(&self, dir: &Path) -> Result<TpmKey> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let scratch = Scratch::new(dir)?;
        let primary = scratch.path("primary.ctx");
        self.primary(&primary)?;
        self.run(
            "tpm2_create",
            &[
                "-C",
                path_str(&primary)?,
                "-G",
                "ecc256:ecdsa-sha256",
                "-a",
                KEY_ATTRIBUTES,
                "-u",
                path_str(&dir.join("key.pub"))?,
                "-r",
                path_str(&dir.join("key.priv"))?,
            ],
        )?;
        drop(scratch);
        self.open_key(dir)
    }

    /// A key created earlier, from its blobs in `dir`.
    pub fn open_key(&self, dir: &Path) -> Result<TpmKey> {
        let scratch = Scratch::new(dir)?;
        let key = self.load(dir, &scratch)?;
        let spki = scratch.path("key.der");
        self.run(
            "tpm2_readpublic",
            &["-c", path_str(&key)?, "-f", "der", "-o", path_str(&spki)?],
        )?;
        let spki = std::fs::read(&spki).context("reading the key's public half")?;
        let point = point_from_spki(&spki)?;
        Ok(TpmKey {
            tpm: self.clone(),
            dir: dir.to_path_buf(),
            point,
            lock: Mutex::new(()),
        })
    }

    /// Load the key's blobs under a freshly derived primary; the loaded
    /// key's context is in `scratch`.
    fn load(&self, dir: &Path, scratch: &Scratch) -> Result<PathBuf> {
        let primary = scratch.path("primary.ctx");
        let key = scratch.path("key.ctx");
        self.primary(&primary)?;
        self.run(
            "tpm2_load",
            &[
                "-C",
                path_str(&primary)?,
                "-u",
                path_str(&dir.join("key.pub"))?,
                "-r",
                path_str(&dir.join("key.priv"))?,
                "-c",
                path_str(&key)?,
            ],
        )?;
        Ok(key)
    }
}

/// What the user is told when a tool fails, in the cases that have a
/// remedy, and the tool's own words otherwise.
pub fn describe_failure(tool: &str, stderr: &str) -> String {
    let lower = stderr.to_lowercase();
    if lower.contains("permission denied") || lower.contains("eacces") {
        return "the TPM is open only to the `tss` group, and this user is not in it. \
                Join it once (`sudo usermod -aG tss $USER`; on an rpm-ostree system first \
                copy the `tss` line from /usr/lib/group into /etc/group), then log in again"
            .to_string();
    }
    if lower.contains("no such file or directory") && lower.contains("tpmrm0") {
        return "this machine has no TPM 2.0 the kernel can reach (/dev/tpmrm0 is missing)"
            .to_string();
    }
    let detail = stderr
        .lines()
        .rfind(|line| line.starts_with("ERROR"))
        .unwrap_or_else(|| stderr.trim());
    format!("{tool} failed: {detail}")
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("{} is not UTF-8", path.display()))
}

/// The point out of a P-256 SubjectPublicKeyInfo, refusing anything else.
fn point_from_spki(spki: &[u8]) -> Result<Vec<u8>> {
    if spki.len() != 91 || subject_public_key_info(&spki[26..]) != spki {
        bail!("the TPM's key is not the P-256 key it was created as");
    }
    Ok(spki[26..].to_vec())
}

/// A directory for one operation's working files, beside the key and so
/// under `$HOME`, where a host process started from inside the Flatpak can
/// see it. Removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(dir: &Path) -> Result<Self> {
        let mut name = [0u8; 8];
        getrandom::fill(&mut name).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
        let hex: String = name.iter().map(|b| format!("{b:02x}")).collect();
        let path = dir.join(format!(".run-{hex}"));
        std::fs::create_dir(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(Self(path))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One key in the TPM.
pub struct TpmKey {
    tpm: Tpm,
    dir: PathBuf,
    point: Vec<u8>,
    /// One signature at a time: the TPM is one device, and two loads of
    /// the same key gain nothing.
    lock: Mutex<()>,
}

impl std::fmt::Debug for TpmKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TpmKey").field("dir", &self.dir).finish()
    }
}

impl KeySigner for TpmKey {
    fn public_point(&self) -> &[u8] {
        &self.point
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>> {
        let _one_at_a_time = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let scratch = Scratch::new(&self.dir)?;
        let key = self.tpm.load(&self.dir, &scratch)?;
        let digest = scratch.path("digest");
        let signature = scratch.path("signature");
        std::fs::write(&digest, Sha256::digest(message))?;
        // `-f plain` is DER for ECDSA, which is what TLS and X.509 carry.
        self.tpm.run(
            "tpm2_sign",
            &[
                "-c",
                path_str(&key)?,
                "-g",
                "sha256",
                "-d",
                "-f",
                "plain",
                "-o",
                path_str(&signature)?,
                path_str(&digest)?,
            ],
        )?;
        Ok(std::fs::read(&signature)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_names_its_tpm() {
        let tpm = Tpm {
            tcti: DEVICE_TCTI.into(),
            flush: false,
            sandboxed: false,
        };
        let (program, argv) = tpm.argv("tpm2_sign", &["-c", "k"]);
        assert_eq!(program, "tpm2_sign");
        assert_eq!(argv, ["-T", "device:/dev/tpmrm0", "-c", "k"]);
    }

    #[test]
    fn inside_the_sandbox_the_tools_run_on_the_host() {
        let tpm = Tpm {
            tcti: DEVICE_TCTI.into(),
            flush: false,
            sandboxed: true,
        };
        let (program, argv) = tpm.argv("tpm2_sign", &[]);
        assert_eq!(program, "flatpak-spawn");
        assert_eq!(&argv[..2], ["--host", "tpm2_sign"]);
    }

    #[test]
    fn a_closed_device_is_explained_with_its_remedy() {
        let said = describe_failure(
            "tpm2_createprimary",
            "ERROR:tcti:src/tss2-tcti/tcti-device.c:452:Tss2_Tcti_Device_Init() Failed to open specified TCTI device file /dev/tpmrm0: Permission denied",
        );
        assert!(said.contains("`tss` group"), "{said}");
        assert!(said.contains("/usr/lib/group"), "{said}");
    }

    #[test]
    fn other_failures_keep_the_tools_last_error() {
        let said = describe_failure(
            "tpm2_load",
            "WARNING:esys:…\nERROR: Esys_Load(0x902) - tpm:warn(2.0): out of memory for object contexts\nERROR: Unable to run tpm2_load",
        );
        assert_eq!(said, "tpm2_load failed: ERROR: Unable to run tpm2_load");
    }

    #[test]
    fn only_a_p256_key_is_accepted() {
        let point = [4u8; 65];
        let spki = subject_public_key_info(&point);
        assert_eq!(point_from_spki(&spki).unwrap(), point);
        assert!(point_from_spki(&spki[..90]).is_err());
        let mut other_curve = spki.clone();
        other_curve[24] ^= 1;
        assert!(point_from_spki(&other_curve).is_err());
    }

    /// The real thing, against a software TPM: create a key, sign with it,
    /// verify the signature, issue the workspace's certificates through it,
    /// and check webpki accepts the chain. Needs `swtpm` and `tpm2-tools`
    /// on PATH, so it is run by a person: `TASTE_TPM_TESTS=1 cargo test -p
    /// taste-gcp -- --ignored tpm` (or the built test binary on a host that
    /// has them, the devcontainer having neither).
    #[test]
    #[ignore]
    fn a_software_tpm_signs_and_issues() {
        if std::env::var_os("TASTE_TPM_TESTS").is_none() {
            eprintln!("TASTE_TPM_TESTS is not set; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let status = Command::new("swtpm")
            .args([
                "socket",
                "--tpm2",
                "--tpmstate",
                &format!("dir={}", state.display()),
                "--server",
                &format!("type=tcp,port={port},bindaddr=127.0.0.1"),
                "--ctrl",
                &format!("type=tcp,port={},bindaddr=127.0.0.1", port + 1),
                "--flags",
                "not-need-init,startup-clear",
                "--daemon",
                "--pid",
                &format!("file={}", dir.path().join("pid").display()),
            ])
            .status()
            .expect("swtpm");
        assert!(status.success());
        struct Kill(PathBuf);
        impl Drop for Kill {
            fn drop(&mut self) {
                if let Ok(pid) = std::fs::read_to_string(&self.0) {
                    let _ = Command::new("kill").arg(pid.trim()).status();
                }
            }
        }
        let _swtpm = Kill(dir.path().join("pid"));

        let tpm = Tpm::unmanaged(&format!("swtpm:host=127.0.0.1,port={port}"));
        let ca = tpm.create_key(&dir.path().join("ca")).unwrap();
        let google = tpm.create_key(&dir.path().join("google")).unwrap();

        let signature = google.sign(b"taste").unwrap();
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_ASN1,
            google.public_point(),
        )
        .verify(b"taste", &signature)
        .unwrap();

        // Reopened from its blobs, it is the same key.
        let reopened = tpm.open_key(&dir.path().join("google")).unwrap();
        assert_eq!(reopened.public_point(), google.public_point());

        let ws = crate::resources::Workspace::new("0a1b2c3d").unwrap();
        let now = std::time::SystemTime::now();
        let ca_cert = crate::identity::issue_ca(&ws, &ca, now).unwrap();
        let leaf =
            crate::identity::issue_leaf(&ws, &ca, &google, crate::identity::Leaf::Google, now)
                .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca_cert.der.clone()).unwrap();
        rustls::server::WebPkiClientVerifier::builder_with_provider(
            std::sync::Arc::new(roots),
            std::sync::Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap()
        .verify_client_cert(
            &leaf.der,
            &[],
            rustls::pki_types::UnixTime::since_unix_epoch(
                now.duration_since(std::time::UNIX_EPOCH).unwrap(),
            ),
        )
        .unwrap();

        // Nothing of the operation is left beside the key.
        let left: Vec<_> = std::fs::read_dir(dir.path().join("google"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        let mut left = left;
        left.sort();
        assert_eq!(left, ["key.priv", "key.pub"]);
    }
}

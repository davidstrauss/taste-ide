//! Passkeys for the port browser: a software authenticator the IDE keeps
//! for each project, on this machine.
//!
//! WebKitGTK has no WebAuthn at all — no `navigator.credentials`, no
//! `PublicKeyCredential` — so a project's sign-in by passkey could not be
//! tried in its own port tab. The browser supplies the API to the page
//! (`taste-app`'s `passkey_menu.rs`), and every request comes here: a
//! registration makes a key pair and keeps it, an assertion signs with it.
//! Keys are held by named authenticators the user makes and picks in the
//! browser's address bar (David, 2026-10-24: "a project-level,
//! machine-local set"), so "alice" and "bob" are two people signing in to
//! the same dev site; whichever is chosen answers. One is always chosen,
//! and an authenticator that holds no key yet is what a site sees as a
//! stranger, which is how "sign up as somebody new" is posed.
//!
//! What it is for bounds what it does:
//!
//! - **The project's own pages only.** A request is answered for a page on
//!   this machine's loopback, on a port the environment forwards, and for
//!   no other origin: these keys sit in a file, and a real site's passkey
//!   does not belong in one.
//! - **On this machine, beside the project's other state.** The file is the
//!   workspace's state directory's, mode 0600 — never the checkout, never a
//!   container, so no agent and no repository code reads a private key.
//! - **ES256 only, attestation "none".** What every relying party accepts,
//!   and nothing that pretends to be hardware.
//! - **No user presence beyond the choice.** The chosen authenticator answers
//!   every request from the project's pages, flagged as user-present and
//!   user-verified: the user verified it by choosing it.
//!
//! The JSON it is given carries every binary field as base64url, as the
//! page's script sends it, and so does the JSON it returns.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The longest name an authenticator may have.
pub const MAX_NAME: usize = 40;

/// Why a request was not answered: a `DOMException` name the page's
/// promise rejects with, and what to tell the person reading its console.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub name: &'static str,
    pub message: String,
}

fn refuse(name: &'static str, message: impl Into<String>) -> Refusal {
    Refusal {
        name,
        message: message.into(),
    }
}

/// What the store makes when it holds no authenticator, so that one is
/// always chosen.
pub const FIRST_NAME: &str = "default";

/// An authenticator as the menu lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatorSummary {
    pub name: String,
    /// Where it is registered: `user · site` for each key it holds; none
    /// for one that has signed up nowhere yet.
    pub sites: Vec<String>,
    pub selected: bool,
}

/// A key the chosen authenticator could sign in with, as a page's sign-in
/// field offers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// The credential id, base64url.
    pub id: String,
    pub user_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PasskeyFile {
    #[serde(default)]
    selected: Option<String>,
    /// `passkeys` before the holders were called authenticators.
    #[serde(default, alias = "passkeys")]
    authenticators: Vec<Authenticator>,
    /// Whether a page's autofill sign-in (`mediation: "conditional"`) is
    /// offered the chosen authenticator's keys under its field.
    #[serde(default = "on")]
    autofill: bool,
}

impl Default for PasskeyFile {
    fn default() -> Self {
        Self {
            selected: None,
            authenticators: Vec::new(),
            autofill: true,
        }
    }
}

fn on() -> bool {
    true
}

impl PasskeyFile {
    /// One authenticator chosen, whatever the file said: a fresh one when
    /// there is none, the first when the choice names nobody.
    fn settle(&mut self) {
        if self.authenticators.is_empty() {
            self.authenticators.push(Authenticator::fresh(FIRST_NAME));
        }
        let known = self
            .selected
            .as_deref()
            .is_some_and(|name| self.authenticators.iter().any(|a| a.name == name));
        if !known {
            self.selected = Some(self.authenticators[0].name.clone());
        }
    }

    fn chosen(&mut self) -> Result<&mut Authenticator, Refusal> {
        self.settle();
        let name = self.selected.clone().unwrap_or_default();
        self.authenticators
            .iter_mut()
            .find(|a| a.name == name)
            .ok_or_else(|| refuse("UnknownError", "no authenticator is chosen"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Authenticator {
    name: String,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    credentials: Vec<Credential>,
}

impl Authenticator {
    fn fresh(name: &str) -> Self {
        Self {
            name: name.to_string(),
            created: Some(crate::state::now_rfc3339()),
            credentials: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Credential {
    /// base64url.
    id: String,
    rp_id: String,
    /// base64url.
    user_handle: String,
    #[serde(default)]
    user_name: String,
    /// The private key, PKCS#8, base64url.
    pkcs8: String,
    #[serde(default)]
    sign_count: u32,
    #[serde(default)]
    created: Option<String>,
}

/// The project's authenticators and the keys they hold: one handle every
/// port tab shares.
#[derive(Clone)]
pub struct Passkeys {
    inner: Arc<Mutex<Store>>,
}

struct Store {
    path: PathBuf,
    /// `None` until loaded, off the main thread ([`Passkeys::load`]).
    file: Option<PasskeyFile>,
}

impl Passkeys {
    /// The project's passkeys, not read yet.
    pub fn new(root: &Path) -> Self {
        Self::at(crate::state::workspace_state_dir(root).join("passkeys.json"))
    }

    /// Passkeys kept in `path`.
    pub fn at(path: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Store { path, file: None })),
        }
    }

    /// Read the file, once. **Blocking IO** — from `spawn_blocking`. A
    /// file that cannot be read is no authenticators, and is not written
    /// over until something is chosen, made, or used.
    pub fn load(&self) {
        let Ok(mut store) = self.inner.lock() else {
            return;
        };
        if store.file.is_some() {
            return;
        }
        let mut file: PasskeyFile = std::fs::read(&store.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        file.settle();
        store.file = Some(file);
    }

    pub fn loaded(&self) -> bool {
        self.inner.lock().is_ok_and(|store| store.file.is_some())
    }

    /// Every authenticator, in the order they were made.
    pub fn list(&self) -> Vec<AuthenticatorSummary> {
        self.with(|file| {
            file.authenticators
                .iter()
                .map(|authenticator| AuthenticatorSummary {
                    name: authenticator.name.clone(),
                    sites: authenticator
                        .credentials
                        .iter()
                        .map(|c| match c.user_name.as_str() {
                            "" => c.rp_id.clone(),
                            user => format!("{user} · {}", c.rp_id),
                        })
                        .collect(),
                    selected: file.selected.as_deref() == Some(authenticator.name.as_str()),
                })
                .collect()
        })
        .unwrap_or_default()
    }

    /// The chosen authenticator's name; `None` only until the file is read.
    pub fn selected(&self) -> Option<String> {
        self.with(|file| file.selected.clone()).flatten()
    }

    /// Choose `name` to answer the project's pages.
    pub fn select(&self, name: &str) -> bool {
        self.with(|file| {
            let known = file.authenticators.iter().any(|a| a.name == name);
            if known {
                file.selected = Some(name.to_string());
            }
            known
        })
        .unwrap_or(false)
    }

    /// Make an authenticator called `name`, holding no key, and choose it.
    pub fn create(&self, name: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("An authenticator needs a name".into());
        }
        if name.chars().count() > MAX_NAME || name.chars().any(char::is_control) {
            return Err(format!(
                "An authenticator's name is one line of at most {MAX_NAME} characters"
            ));
        }
        self.with(|file| {
            if file.authenticators.iter().any(|a| a.name == name) {
                return Err(format!("There is already an authenticator called {name}"));
            }
            file.authenticators.push(Authenticator::fresh(name));
            file.selected = Some(name.to_string());
            Ok(())
        })
        .unwrap_or_else(|| Err("The authenticators are not read yet".into()))
    }

    /// Delete the authenticator called `name` and every key it holds. The
    /// choice moves to the first left, or to a fresh one when none is.
    pub fn delete(&self, name: &str) -> bool {
        self.with(|file| {
            let before = file.authenticators.len();
            file.authenticators.retain(|a| a.name != name);
            let gone = file.authenticators.len() != before;
            file.settle();
            gone
        })
        .unwrap_or(false)
    }

    /// Whether a page's autofill sign-in is offered the chosen
    /// authenticator's keys.
    pub fn autofill(&self) -> bool {
        self.with(|file| file.autofill).unwrap_or(false)
    }

    pub fn set_autofill(&self, on: bool) {
        self.with(|file| file.autofill = on);
    }

    /// Write the file, mode 0600. **Blocking IO** — from `spawn_blocking`.
    pub fn persist(&self) -> Result<()> {
        let (path, text) = {
            let store = self
                .inner
                .lock()
                .map_err(|_| anyhow::anyhow!("the passkey store is poisoned"))?;
            let Some(file) = &store.file else {
                return Ok(());
            };
            (store.path.clone(), serde_json::to_vec_pretty(file)?)
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let part = path.with_extension("json.part");
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut out = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&part)
                .with_context(|| format!("writing {}", part.display()))?;
            out.write_all(&text)?;
            out.sync_all()?;
        }
        std::fs::rename(&part, &path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// `navigator.credentials.create`: a new key in the chosen authenticator for
    /// `options` (the page's `publicKey`), from a page at `origin`, which
    /// must be on this machine's loopback on one of `ports`.
    pub fn register(&self, origin: &str, ports: &[u16], options: &Value) -> Result<Value, Refusal> {
        let host = checked_origin(origin, ports)?;
        let rp_id = rp_id(&host, options["rp"]["id"].as_str())?;
        let challenge = bytes(&options["challenge"], "challenge")?;
        let user_handle = bytes(&options["user"]["id"], "user.id")?;
        if user_handle.is_empty() || user_handle.len() > 64 {
            return Err(refuse("TypeError", "user.id must be 1 to 64 bytes"));
        }
        let user_name = options["user"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if let Some(params) = options["pubKeyCredParams"].as_array() {
            if !params.is_empty() && !params.iter().any(|p| p["alg"].as_i64() == Some(-7)) {
                return Err(refuse(
                    "NotSupportedError",
                    "this authenticator makes ES256 keys (alg -7) only, and the page did not ask \
                     for one",
                ));
            }
        }
        let excluded = ids(&options["excludeCredentials"]);

        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng)
            .map_err(|_| refuse("UnknownError", "a key pair could not be made"))?;
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
            .map_err(|_| refuse("UnknownError", "the key pair made could not be read back"))?;
        let point = pair.public_key().as_ref().to_vec();
        let mut id = [0u8; 32];
        rng.fill(&mut id)
            .map_err(|_| refuse("UnknownError", "no randomness for a credential id"))?;

        let mut auth_data = rp_hash(&rp_id).to_vec();
        // User present, user verified, attested credential data included.
        auth_data.push(0x45);
        auth_data.extend_from_slice(&0u32.to_be_bytes());
        auth_data.extend_from_slice(&[0u8; 16]);
        auth_data.extend_from_slice(&(id.len() as u16).to_be_bytes());
        auth_data.extend_from_slice(&id);
        auth_data.extend_from_slice(&cose_key(&point));
        let mut attestation = Vec::new();
        cbor_head(5, 3, &mut attestation);
        cbor_text("fmt", &mut attestation);
        cbor_text("none", &mut attestation);
        cbor_text("attStmt", &mut attestation);
        cbor_head(5, 0, &mut attestation);
        cbor_text("authData", &mut attestation);
        cbor_bytes(&auth_data, &mut attestation);
        let client_data = client_data("webauthn.create", &challenge, origin);

        let id_text = URL_SAFE_NO_PAD.encode(id);
        let handle_text = URL_SAFE_NO_PAD.encode(&user_handle);
        self.with(|file| {
            let authenticator = file.chosen()?;
            if authenticator
                .credentials
                .iter()
                .any(|c| c.rp_id == rp_id && excluded.contains(&c.id))
            {
                return Err(refuse(
                    "InvalidStateError",
                    format!(
                        "authenticator {} is already registered with {rp_id}",
                        authenticator.name
                    ),
                ));
            }
            // One key per person per site, as an authenticator keeps them:
            // signing up again replaces it.
            authenticator
                .credentials
                .retain(|c| !(c.rp_id == rp_id && c.user_handle == handle_text));
            authenticator.credentials.push(Credential {
                id: id_text.clone(),
                rp_id: rp_id.clone(),
                user_handle: handle_text.clone(),
                user_name: user_name.clone(),
                pkcs8: URL_SAFE_NO_PAD.encode(pkcs8.as_ref()),
                sign_count: 0,
                created: Some(crate::state::now_rfc3339()),
            });
            Ok(())
        })
        .unwrap_or_else(|| Err(not_loaded()))?;

        Ok(json!({
            "id": id_text,
            "clientDataJSON": URL_SAFE_NO_PAD.encode(client_data.as_bytes()),
            "attestationObject": URL_SAFE_NO_PAD.encode(&attestation),
            "authenticatorData": URL_SAFE_NO_PAD.encode(&auth_data),
            "publicKey": URL_SAFE_NO_PAD.encode(spki(&point)),
            "publicKeyAlgorithm": -7,
            "transports": ["internal"],
        }))
    }

    /// `navigator.credentials.get`: a signature by the chosen
    /// authenticator's key for the site `options` names (the page's
    /// `publicKey`), from a page at `origin`, as [`Passkeys::register`]
    /// bounds it. `pick` is the key a sign-in field's offer was taken for
    /// ([`Passkeys::offers`]); without one, the newest key the page allows.
    pub fn assert(
        &self,
        origin: &str,
        ports: &[u16],
        options: &Value,
        pick: Option<&str>,
    ) -> Result<Value, Refusal> {
        let host = checked_origin(origin, ports)?;
        let rp_id = rp_id(&host, options["rpId"].as_str())?;
        let challenge = bytes(&options["challenge"], "challenge")?;
        let allowed = ids(&options["allowCredentials"]);
        let (credential, count) = self
            .with(|file| {
                let authenticator = file.chosen()?;
                let name = authenticator.name.clone();
                let credential = authenticator
                    .credentials
                    .iter_mut()
                    .rev()
                    .find(|c| {
                        c.rp_id == rp_id
                            && (allowed.is_empty() || allowed.contains(&c.id))
                            && pick.is_none_or(|id| id == c.id)
                    })
                    .ok_or_else(|| {
                        refuse(
                            "NotAllowedError",
                            format!(
                                "authenticator {name} holds no key for {rp_id}{}: sign up with \
                                 it chosen first",
                                if allowed.is_empty() && pick.is_none() {
                                    ""
                                } else {
                                    " that the page asked for"
                                }
                            ),
                        )
                    })?;
                credential.sign_count = credential.sign_count.saturating_add(1);
                Ok((credential.clone(), credential.sign_count))
            })
            .unwrap_or_else(|| Err(not_loaded()))?;

        let rng = SystemRandom::new();
        let pkcs8 = URL_SAFE_NO_PAD
            .decode(&credential.pkcs8)
            .map_err(|_| refuse("UnknownError", "the stored key could not be read"))?;
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &pkcs8, &rng)
            .map_err(|_| refuse("UnknownError", "the stored key could not be read"))?;
        let mut auth_data = rp_hash(&rp_id).to_vec();
        // User present, user verified.
        auth_data.push(0x05);
        auth_data.extend_from_slice(&count.to_be_bytes());
        let client_data = client_data("webauthn.get", &challenge, origin);
        let mut signed = auth_data.clone();
        signed.extend_from_slice(
            ring::digest::digest(&ring::digest::SHA256, client_data.as_bytes()).as_ref(),
        );
        let signature = pair
            .sign(&rng, &signed)
            .map_err(|_| refuse("UnknownError", "the key would not sign"))?;
        Ok(json!({
            "id": credential.id,
            "clientDataJSON": URL_SAFE_NO_PAD.encode(client_data.as_bytes()),
            "authenticatorData": URL_SAFE_NO_PAD.encode(&auth_data),
            "signature": URL_SAFE_NO_PAD.encode(signature.as_ref()),
            "userHandle": credential.user_handle,
        }))
    }

    /// The keys the chosen authenticator holds for the site a page's
    /// autofill request names, newest first, as its sign-in field lists
    /// them; none while autofill is off.
    pub fn offers(
        &self,
        origin: &str,
        ports: &[u16],
        options: &Value,
    ) -> Result<Vec<Offer>, Refusal> {
        let host = checked_origin(origin, ports)?;
        let rp_id = rp_id(&host, options["rpId"].as_str())?;
        let allowed = ids(&options["allowCredentials"]);
        self.with(|file| {
            if !file.autofill {
                return Ok(Vec::new());
            }
            Ok(file
                .chosen()?
                .credentials
                .iter()
                .rev()
                .filter(|c| c.rp_id == rp_id && (allowed.is_empty() || allowed.contains(&c.id)))
                .map(|c| Offer {
                    id: c.id.clone(),
                    user_name: c.user_name.clone(),
                })
                .collect())
        })
        .unwrap_or_else(|| Err(not_loaded()))
    }

    fn with<T>(&self, f: impl FnOnce(&mut PasskeyFile) -> T) -> Option<T> {
        let mut store = self.inner.lock().ok()?;
        store.file.as_mut().map(f)
    }
}

impl std::fmt::Debug for Passkeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Passkeys")
    }
}

fn not_loaded() -> Refusal {
    refuse(
        "NotAllowedError",
        "the project's passkeys are still being read; try again",
    )
}

/// The host of `origin` when it is one of this machine's loopback names on
/// one of `ports`: the only pages these keys answer.
fn checked_origin(origin: &str, ports: &[u16]) -> Result<String, Refusal> {
    let outside = || {
        refuse(
            "NotAllowedError",
            format!(
                "{origin} is not one of this environment's forwarded ports on localhost; the \
                 IDE's passkeys answer the project's own pages only"
            ),
        )
    };
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .ok_or_else(outside)?;
    let (host, port) = match rest.strip_prefix('[') {
        Some(v6) => {
            let (host, after) = v6.split_once(']').ok_or_else(outside)?;
            (format!("[{host}]"), after.strip_prefix(':'))
        }
        None => match rest.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), Some(port)),
            None => (rest.to_string(), None),
        },
    };
    let port: u16 = match port {
        Some(port) => port.parse().map_err(|_| outside())?,
        None if origin.starts_with("https") => 443,
        None => 80,
    };
    let host = host.to_ascii_lowercase();
    let loopback = host == "localhost"
        || host.ends_with(".localhost")
        || host == "127.0.0.1"
        || host == "[::1]";
    if !loopback || !ports.contains(&port) {
        return Err(outside());
    }
    Ok(host)
}

/// The site a request is for: the page's `rp.id` (or `rpId`) when it is
/// the page's host or a parent of it, else the host itself.
fn rp_id(host: &str, asked: Option<&str>) -> Result<String, Refusal> {
    match asked {
        None => Ok(host.to_string()),
        Some(id) => {
            let id = id.to_ascii_lowercase();
            if host == id || host.ends_with(&format!(".{id}")) {
                Ok(id)
            } else {
                Err(refuse(
                    "SecurityError",
                    format!("{id} is not this page's host or a parent of it ({host})"),
                ))
            }
        }
    }
}

fn bytes(value: &Value, what: &str) -> Result<Vec<u8>, Refusal> {
    let text = value
        .as_str()
        .ok_or_else(|| refuse("TypeError", format!("{what} is missing")))?;
    URL_SAFE_NO_PAD
        .decode(text.trim_end_matches('='))
        .or_else(|_| URL_SAFE.decode(text))
        .map_err(|_| refuse("TypeError", format!("{what} is not base64url")))
}

/// The ids of a list of credential descriptors, as stored.
fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["id"].as_str())
                .map(|id| id.trim_end_matches('=').to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn rp_hash(rp_id: &str) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, rp_id.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// The client data, in the order browsers write it, since a relying party
/// is given these bytes and hashes them as they are.
fn client_data(kind: &str, challenge: &[u8], origin: &str) -> String {
    format!(
        r#"{{"type":"{kind}","challenge":"{}","origin":{},"crossOrigin":false}}"#,
        URL_SAFE_NO_PAD.encode(challenge),
        serde_json::to_string(origin).unwrap_or_default()
    )
}

/// The public key as COSE: EC2, ES256, P-256, x, y.
fn cose_key(point: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    cbor_head(5, 5, &mut out);
    cbor_int(1, &mut out);
    cbor_int(2, &mut out);
    cbor_int(3, &mut out);
    cbor_int(-7, &mut out);
    cbor_int(-1, &mut out);
    cbor_int(1, &mut out);
    cbor_int(-2, &mut out);
    cbor_bytes(&point[1..33], &mut out);
    cbor_int(-3, &mut out);
    cbor_bytes(&point[33..65], &mut out);
    out
}

/// The public key as a SubjectPublicKeyInfo, for `getPublicKey()`.
fn spki(point: &[u8]) -> Vec<u8> {
    const PREFIX: [u8; 26] = [
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
    ];
    let mut out = PREFIX.to_vec();
    out.extend_from_slice(point);
    out
}

fn cbor_head(major: u8, n: u64, out: &mut Vec<u8>) {
    let major = major << 5;
    match n {
        0..=23 => out.push(major | n as u8),
        24..=0xff => out.extend_from_slice(&[major | 24, n as u8]),
        0x100..=0xffff => {
            out.push(major | 25);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(major | 26);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        }
        _ => {
            out.push(major | 27);
            out.extend_from_slice(&n.to_be_bytes());
        }
    }
}

fn cbor_int(n: i64, out: &mut Vec<u8>) {
    if n >= 0 {
        cbor_head(0, n as u64, out);
    } else {
        cbor_head(1, (-1 - n) as u64, out);
    }
}

fn cbor_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    cbor_head(2, bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

fn cbor_text(text: &str, out: &mut Vec<u8>) {
    cbor_head(3, text.len() as u64, out);
    out.extend_from_slice(text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{UnparsedPublicKey, ECDSA_P256_SHA256_ASN1};

    fn b64(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    fn unb64(text: &Value) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(text.as_str().unwrap()).unwrap()
    }

    fn store() -> (tempfile::TempDir, Passkeys) {
        let dir = tempfile::tempdir().unwrap();
        let passkeys = Passkeys::at(dir.path().join("passkeys.json"));
        passkeys.load();
        (dir, passkeys)
    }

    fn creation(user: &[u8]) -> Value {
        json!({
            "challenge": b64(b"sign-up challenge"),
            "rp": { "name": "SB", "id": "localhost" },
            "user": { "id": b64(user), "name": "alice@example.com", "displayName": "Alice" },
            "pubKeyCredParams": [{ "type": "public-key", "alg": -8 }, { "type": "public-key", "alg": -7 }],
        })
    }

    /// What a relying party checks of a sign-up and a sign-in, checked: the
    /// client data it hashes, the site's hash and flags in the
    /// authenticator data, and the signature, against the key the sign-up
    /// gave it.
    #[test]
    fn a_signup_and_a_signin_verify_as_a_relying_party_checks_them() {
        let (dir, passkeys) = store();
        passkeys.create("alice").unwrap();
        let origin = "http://localhost:8000";
        let made = passkeys
            .register(origin, &[8000, 5173], &creation(b"user-1"))
            .unwrap();
        let client: Value = serde_json::from_slice(&unb64(&made["clientDataJSON"])).unwrap();
        assert_eq!(client["type"], "webauthn.create");
        assert_eq!(client["origin"], origin);
        assert_eq!(client["challenge"], b64(b"sign-up challenge"));
        let made_data = unb64(&made["authenticatorData"]);
        assert_eq!(&made_data[..32], &rp_hash("localhost"));
        assert_eq!(made_data[32] & 0x45, 0x45);
        let spki = unb64(&made["publicKey"]);
        let point = &spki[26..];
        assert_eq!(point.len(), 65);
        // The attested credential data names the same id and key.
        let id = unb64(&made["id"]);
        assert_eq!(&made_data[55..55 + id.len()], &id[..]);
        assert_eq!(cose_key(point), made_data[55 + id.len()..]);

        let request = json!({
            "challenge": b64(b"sign-in challenge"),
            "rpId": "localhost",
            "allowCredentials": [{ "type": "public-key", "id": made["id"] }],
        });
        let signed = passkeys.assert(origin, &[8000], &request, None).unwrap();
        assert_eq!(signed["userHandle"], b64(b"user-1"));
        let data = unb64(&signed["authenticatorData"]);
        assert_eq!(&data[..32], &rp_hash("localhost"));
        assert_eq!(u32::from_be_bytes(data[33..37].try_into().unwrap()), 1);
        let client_bytes = unb64(&signed["clientDataJSON"]);
        let mut message = data.clone();
        message
            .extend_from_slice(ring::digest::digest(&ring::digest::SHA256, &client_bytes).as_ref());
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, point)
            .verify(&message, &unb64(&signed["signature"]))
            .expect("the signature verifies against the registered key");

        // Kept: read back from the file, it signs again, one count on.
        passkeys.persist().unwrap();
        let reopened = Passkeys::at(dir.path().join("passkeys.json"));
        reopened.load();
        assert_eq!(reopened.selected().as_deref(), Some("alice"));
        let again = reopened.assert(origin, &[8000], &request, None).unwrap();
        let data = unb64(&again["authenticatorData"]);
        assert_eq!(u32::from_be_bytes(data[33..37].try_into().unwrap()), 2);
    }

    /// The chosen authenticator answers, and another's keys are not its own.
    #[test]
    fn the_chosen_authenticator_answers_and_no_other() {
        let (_dir, passkeys) = store();
        passkeys.create("alice").unwrap();
        passkeys
            .register("http://localhost:8000", &[8000], &creation(b"a"))
            .unwrap();
        passkeys.create("bob").unwrap();
        assert_eq!(passkeys.selected().as_deref(), Some("bob"));
        let request = json!({ "challenge": b64(b"c") });
        let refused = passkeys
            .assert("http://localhost:8000", &[8000], &request, None)
            .unwrap_err();
        assert_eq!(refused.name, "NotAllowedError");
        assert!(
            refused.message.contains("bob holds no key for localhost"),
            "{refused:?}"
        );
        assert!(passkeys.select("alice"));
        assert!(!passkeys.select("carol"));
        assert!(passkeys
            .assert("http://localhost:8000", &[8000], &request, None)
            .is_ok());
        assert_eq!(
            passkeys.create("bob").unwrap_err(),
            "There is already an authenticator called bob"
        );
        assert!(passkeys.create(" ").is_err());
    }

    /// One is always chosen: a fresh store has one, a deleted choice moves
    /// to the first left, and deleting the last leaves a fresh one.
    #[test]
    fn one_authenticator_is_always_chosen() {
        let (_dir, passkeys) = store();
        assert_eq!(passkeys.selected().as_deref(), Some(FIRST_NAME));
        assert!(passkeys.list()[0].sites.is_empty());
        passkeys.create("alice").unwrap();
        passkeys
            .register("http://localhost:8000", &[8000], &creation(b"a"))
            .unwrap();
        assert!(passkeys.delete("alice"));
        assert_eq!(passkeys.selected().as_deref(), Some(FIRST_NAME));
        assert!(passkeys.delete(FIRST_NAME));
        let left = passkeys.list();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].name, FIRST_NAME);
        assert!(left[0].selected && left[0].sites.is_empty());
        assert!(!passkeys.delete("nobody"));
    }

    /// A file written while the holders were called passkeys reads back,
    /// its choice and keys whole, with autofill on.
    #[test]
    fn a_file_from_before_the_rename_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("passkeys.json");
        std::fs::write(
            &path,
            r#"{ "selected": "bob", "passkeys": [{ "name": "alice" }, { "name": "bob" }] }"#,
        )
        .unwrap();
        let passkeys = Passkeys::at(path);
        passkeys.load();
        let names: Vec<_> = passkeys.list().into_iter().map(|a| a.name).collect();
        assert_eq!(names, ["alice", "bob"]);
        assert_eq!(passkeys.selected().as_deref(), Some("bob"));
        assert!(passkeys.autofill());
    }

    /// A sign-in field is offered the chosen authenticator's keys for the
    /// site, newest first, and the one taken is the one that signs; with
    /// autofill off it is offered none.
    #[test]
    fn a_sign_in_field_offers_the_chosen_keys() {
        let (_dir, passkeys) = store();
        passkeys.create("alice").unwrap();
        let origin = "http://localhost:8000";
        let first = passkeys
            .register(origin, &[8000], &creation(b"user-1"))
            .unwrap();
        let mut second = creation(b"user-2");
        second["user"]["name"] = json!("alice-admin");
        let second = passkeys.register(origin, &[8000], &second).unwrap();
        let request = json!({ "challenge": b64(b"c"), "rpId": "localhost" });
        let offers = passkeys.offers(origin, &[8000], &request).unwrap();
        let users: Vec<_> = offers.iter().map(|o| o.user_name.as_str()).collect();
        assert_eq!(users, ["alice-admin", "alice@example.com"]);
        assert_eq!(offers[1].id, first["id"].as_str().unwrap());
        let signed = passkeys
            .assert(origin, &[8000], &request, Some(&offers[1].id))
            .unwrap();
        assert_eq!(signed["userHandle"], b64(b"user-1"));
        let newest = passkeys.assert(origin, &[8000], &request, None).unwrap();
        assert_eq!(newest["id"], second["id"]);
        assert!(passkeys
            .offers("http://localhost:6000", &[8000], &request)
            .is_err());

        passkeys.set_autofill(false);
        assert!(passkeys
            .offers(origin, &[8000], &request)
            .unwrap()
            .is_empty());
        passkeys.create("bob").unwrap();
        passkeys.set_autofill(true);
        assert!(passkeys
            .offers(origin, &[8000], &request)
            .unwrap()
            .is_empty());
    }

    /// Only the project's own pages are answered, for their own site.
    #[test]
    fn only_the_projects_pages_are_answered() {
        let (_dir, passkeys) = store();
        passkeys.create("alice").unwrap();
        for origin in [
            "https://github.com",
            "http://localhost:6000",
            "http://192.168.1.4:8000",
            "file:///etc/passwd",
        ] {
            let refused = passkeys
                .register(origin, &[8000], &creation(b"a"))
                .unwrap_err();
            assert_eq!(refused.name, "NotAllowedError", "{origin}");
        }
        for origin in [
            "http://127.0.0.1:8000",
            "http://[::1]:8000",
            "http://app.localhost:8000",
        ] {
            let mut options = creation(b"a");
            options["rp"]["id"] = Value::Null;
            assert!(
                passkeys.register(origin, &[8000], &options).is_ok(),
                "{origin}"
            );
        }
        let mut options = creation(b"a");
        options["rp"]["id"] = json!("example.com");
        assert_eq!(
            passkeys
                .register("http://localhost:8000", &[8000], &options)
                .unwrap_err()
                .name,
            "SecurityError"
        );
        options["rp"]["id"] = json!("localhost");
        options["pubKeyCredParams"] = json!([{ "type": "public-key", "alg": -257 }]);
        assert_eq!(
            passkeys
                .register("http://localhost:8000", &[8000], &options)
                .unwrap_err()
                .name,
            "NotSupportedError"
        );
    }

    #[test]
    fn the_file_is_the_users_alone() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, passkeys) = store();
        passkeys.create("alice").unwrap();
        passkeys.persist().unwrap();
        let mode = std::fs::metadata(dir.path().join("passkeys.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

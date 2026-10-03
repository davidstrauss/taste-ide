//! The workspace's certificates: a CA, a leaf that authenticates to
//! Google, and a leaf that authenticates to the model's VM.
//!
//! Each key is a [`KeySigner`] — in the product, a key in the TPM — so
//! issuing a certificate is asking the CA's holder for one signature, and
//! nothing here ever sees a private key. The certificates themselves are
//! public: the CA's goes into the Workload Identity provider's trust store
//! and the VM's TLS terminator, and the IDE keeps all three in its state
//! directory.
//!
//! The shape is what Workload Identity Federation with X.509 certificates
//! asks for: a chain no deeper than five, a leaf with `digitalSignature`
//! and `keyEncipherment` key usage, `CA:FALSE`, and a life of at most 390
//! days. Leaves are issued for [`LEAF_DAYS`] and renewed [`RENEW_DAYS`]
//! before they lapse, by the same CA key, which leaves the provider's
//! trust store as it was.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyIdMethod, KeyUsagePurpose, SerialNumber,
};
use rustls::pki_types::CertificateDer;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::resources::Workspace;
use crate::setup;
use crate::signer::{CertKey, KeySigner};

/// Inside the provider's 390-day limit, with room for a clock that is a
/// day out.
pub const LEAF_DAYS: u64 = 365;
const _: () = assert!(LEAF_DAYS < 390, "the provider refuses leaves over 390 days");
/// A leaf is reissued once it has this little life left.
pub const RENEW_DAYS: u64 = 30;
/// The CA outlives many leaves; replacing it means running the setup
/// script again, since the provider trusts it by name.
pub const CA_DAYS: u64 = 3650;
/// Backdating, so a server whose clock is a little behind still sees the
/// certificate as valid.
const BACKDATE: Duration = Duration::from_secs(60 * 60);

const DAY: u64 = 24 * 60 * 60;

/// What a leaf is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leaf {
    /// Exchanged at Google's token service for an access token.
    Google,
    /// Presented to the model VM's TLS terminator.
    Tunnel,
}

/// One issued certificate and when it lapses, kept beside it so deciding
/// whether to renew never means parsing X.509.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issued {
    pub der: CertificateDer<'static>,
    pub not_after: SystemTime,
}

impl Issued {
    /// PEM with `\n` line endings, which is what goes into a YAML trust
    /// store pasted into a shell.
    pub fn pem(&self) -> String {
        use base64::Engine;
        let body = base64::engine::general_purpose::STANDARD.encode(self.der.as_ref());
        let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
        for line in body.as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
            pem.push('\n');
        }
        pem.push_str("-----END CERTIFICATE-----\n");
        pem
    }

    pub fn needs_renewal(&self, now: SystemTime) -> bool {
        self.not_after
            .duration_since(now)
            .map_or(true, |left| left < Duration::from_secs(RENEW_DAYS * DAY))
    }
}

fn offset(t: SystemTime) -> Result<OffsetDateTime> {
    let secs = t.duration_since(UNIX_EPOCH).context("a time before 1970")?;
    Ok(OffsetDateTime::from_unix_timestamp(secs.as_secs() as i64)?)
}

fn random_serial() -> Result<SerialNumber> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    // A positive INTEGER: the top bit clear.
    bytes[0] &= 0x7f;
    Ok(SerialNumber::from_slice(&bytes))
}

/// A key's identifier, RFC 7093's first method: the leftmost 160 bits of
/// SHA-256 over the public key. Stated rather than left to rcgen, whose
/// default without its own crypto backend is an identifier zero bytes long
/// — which RFC 5280 does not allow a CA, and which then empties every
/// leaf's authority key identifier too.
fn key_id(key: &dyn KeySigner) -> KeyIdMethod {
    KeyIdMethod::PreSpecified(Sha256::digest(key.public_point())[..20].to_vec())
}

/// The CA's parameters. Rebuilt the same way at every renewal, because a
/// leaf's issuer is named from them, and its authority key identifier
/// derived from them, and both have to match the certificate the provider
/// trusts.
fn ca_params(ws: &Workspace, ca: &dyn KeySigner) -> CertificateParams {
    let mut params = CertificateParams::default();
    params.key_identifier_method = key_id(ca);
    let mut name = DistinguishedName::new();
    name.push(DnType::OrganizationName, "taste-ide");
    name.push(DnType::CommonName, format!("taste-{} CA", ws.id()));
    params.distinguished_name = name;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params
}

/// The workspace's CA certificate, self-signed by the CA key.
pub fn issue_ca(ws: &Workspace, ca: &dyn KeySigner, now: SystemTime) -> Result<Issued> {
    let not_after = now + Duration::from_secs(CA_DAYS * DAY);
    let mut params = ca_params(ws, ca);
    params.not_before = offset(now - BACKDATE)?;
    params.not_after = offset(not_after)?;
    params.serial_number = Some(random_serial()?);
    let cert = params
        .self_signed(&CertKey(ca))
        .context("self-signing the workspace CA")?;
    Ok(Issued {
        der: cert.der().clone(),
        not_after,
    })
}

/// The common name a leaf carries. Google maps the Google leaf's to
/// `google.subject`, and the provider admits that one name only.
pub fn common_name(ws: &Workspace, leaf: Leaf) -> String {
    match leaf {
        Leaf::Google => setup::subject(ws),
        Leaf::Tunnel => format!("taste-{}-tunnel", ws.id()),
    }
}

/// A leaf for `key`, issued by the CA key.
pub fn issue_leaf(
    ws: &Workspace,
    ca: &dyn KeySigner,
    key: &dyn KeySigner,
    leaf: Leaf,
    now: SystemTime,
) -> Result<Issued> {
    let not_after = now + Duration::from_secs(LEAF_DAYS * DAY);
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::OrganizationName, "taste-ide");
    name.push(DnType::CommonName, common_name(ws, leaf));
    params.distinguished_name = name;
    params.is_ca = IsCa::ExplicitNoCa;
    // `keyEncipherment` means nothing for an EC key; it is here because the
    // provider's documented leaf profile lists it.
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    params.use_authority_key_identifier_extension = true;
    params.key_identifier_method = key_id(key);
    params.not_before = offset(now - BACKDATE)?;
    params.not_after = offset(not_after)?;
    params.serial_number = Some(random_serial()?);
    let issuer = Issuer::new(ca_params(ws, ca), CertKey(ca));
    let cert = params
        .signed_by(&CertKey(key), &issuer)
        .context("issuing a leaf from the workspace CA")?;
    Ok(Issued {
        der: cert.der().clone(),
        not_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::testing::MemorySigner;
    use std::sync::Arc;

    fn ws() -> Workspace {
        Workspace::new("0a1b2c3d").unwrap()
    }

    #[test]
    fn a_leaf_chains_to_its_ca_as_a_client_certificate() {
        let now = SystemTime::now();
        let ca = MemorySigner::generate();
        let key = MemorySigner::generate();
        let ca_cert = issue_ca(&ws(), &ca, now).unwrap();
        let leaf = issue_leaf(&ws(), &ca, &key, Leaf::Google, now).unwrap();

        // webpki is what a TLS server checks a client certificate with, so
        // this is the test that the certificates are acceptable as built.
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca_cert.der.clone()).unwrap();
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap();
        verifier
            .verify_client_cert(
                &leaf.der,
                &[],
                rustls::pki_types::UnixTime::since_unix_epoch(
                    now.duration_since(UNIX_EPOCH).unwrap(),
                ),
            )
            .unwrap();
    }

    #[test]
    fn a_leaf_from_another_ca_is_refused() {
        let now = SystemTime::now();
        let ours = MemorySigner::generate();
        let theirs = MemorySigner::generate();
        let key = MemorySigner::generate();
        let ca_cert = issue_ca(&ws(), &ours, now).unwrap();
        let leaf = issue_leaf(&ws(), &theirs, &key, Leaf::Google, now).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca_cert.der.clone()).unwrap();
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap();
        assert!(verifier
            .verify_client_cert(
                &leaf.der,
                &[],
                rustls::pki_types::UnixTime::since_unix_epoch(
                    now.duration_since(UNIX_EPOCH).unwrap(),
                ),
            )
            .is_err());
    }

    #[test]
    fn leaves_are_renewed_a_month_before_they_lapse() {
        let now = SystemTime::now();
        let ca = MemorySigner::generate();
        let leaf = issue_leaf(&ws(), &ca, &ca, Leaf::Tunnel, now).unwrap();
        assert!(!leaf.needs_renewal(now));
        let late = now + Duration::from_secs((LEAF_DAYS - RENEW_DAYS + 1) * DAY);
        assert!(leaf.needs_renewal(late));
        assert!(leaf.needs_renewal(now + Duration::from_secs(400 * DAY)));
    }

    #[test]
    fn the_ca_pem_is_what_the_setup_script_accepts() {
        let ca = MemorySigner::generate();
        let cert = issue_ca(&ws(), &ca, SystemTime::now()).unwrap();
        crate::setup::cloud_shell_script(&ws(), "my-project-1", &cert.pem()).unwrap();
    }

    #[test]
    fn key_identifiers_are_twenty_bytes_and_chain() {
        let now = SystemTime::now();
        let ca = MemorySigner::generate();
        let key = MemorySigner::generate();
        let ca_cert = issue_ca(&ws(), &ca, now).unwrap();
        let leaf = issue_leaf(&ws(), &ca, &key, Leaf::Google, now).unwrap();
        let ca_id = Sha256::digest(ca.public_point())[..20].to_vec();
        let key_id = Sha256::digest(key.public_point())[..20].to_vec();
        let contains = |der: &[u8], needle: &[u8]| der.windows(needle.len()).any(|w| w == needle);
        // subjectKeyIdentifier: OID 2.5.29.14, an OCTET STRING of 20 bytes.
        let ski = |id: &[u8]| {
            [
                &[0x06, 0x03, 0x55, 0x1d, 0x0e, 0x04, 0x16, 0x04, 0x14][..],
                id,
            ]
            .concat()
        };
        assert!(contains(&ca_cert.der, &ski(&ca_id)));
        assert!(contains(&leaf.der, &ski(&key_id)));
        // The leaf's authorityKeyIdentifier carries the CA's.
        assert!(contains(&leaf.der, &[&[0x80, 0x14][..], &ca_id].concat()));
        // And no identifier anywhere is empty.
        for der in [&ca_cert.der, &leaf.der] {
            assert!(!contains(
                der,
                &[0x06, 0x03, 0x55, 0x1d, 0x0e, 0x04, 0x02, 0x04, 0x00]
            ));
        }
    }
}

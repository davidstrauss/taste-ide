//! A key that signs where it lives and is never anywhere else.
//!
//! The IDE's identity is three keys in the TPM (ENVIRONMENTS → "The IDE's
//! identity lives in the TPM"), and everything that uses them — building
//! certificates, completing a TLS handshake — needs only a public key and
//! a signature. [`KeySigner`] is that and nothing more, so a private key
//! has no way to cross it.
//!
//! One algorithm, ECDSA P-256 with SHA-256, because it is the one every
//! holder in question has: every TPM 2.0, every PIV security key, Workload
//! Identity Federation's accepted list, and TLS 1.2 and 1.3 alike.

use std::fmt;
use std::sync::Arc;

use anyhow::Result;
use rustls::pki_types::SubjectPublicKeyInfoDer;
use rustls::sign::{Signer, SigningKey};
use rustls::{SignatureAlgorithm, SignatureScheme};

/// A P-256 private key held somewhere it cannot be read from.
pub trait KeySigner: Send + Sync + fmt::Debug {
    /// The public key as an uncompressed point: `0x04 || X || Y`, 65 bytes.
    fn public_point(&self) -> &[u8];

    /// An ECDSA signature over SHA-256 of `message`, DER-encoded. The
    /// holder does the hashing, because a TPM signs a digest it is handed
    /// and a key in memory hashes as part of signing; either way the
    /// caller hands over the message.
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>>;
}

/// The SubjectPublicKeyInfo for a P-256 point, which is what TLS and X.509
/// both call a public key.
pub fn subject_public_key_info(point: &[u8]) -> Vec<u8> {
    // SEQUENCE { SEQUENCE { id-ecPublicKey, prime256v1 }, BIT STRING point }
    const PREFIX: [u8; 26] = [
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
    ];
    let mut der = PREFIX.to_vec();
    der.extend_from_slice(point);
    der
}

/// A [`KeySigner`] as `rcgen` signs certificates with it.
pub struct CertKey<'a>(pub &'a dyn KeySigner);

impl rcgen::PublicKeyData for CertKey<'_> {
    fn der_bytes(&self) -> &[u8] {
        self.0.public_point()
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

impl rcgen::SigningKey for CertKey<'_> {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        // rcgen's error has no room for a reason, so it is logged here
        // before it is lost.
        self.0.sign(message).map_err(|e| {
            tracing::warn!("the CA key's holder refused to sign a certificate: {e:#}");
            rcgen::Error::RemoteKeyError
        })
    }
}

/// A [`KeySigner`] as rustls presents it in a client handshake.
#[derive(Debug, Clone)]
pub struct TlsKey(pub Arc<dyn KeySigner>);

impl SigningKey for TlsKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        offered
            .contains(&SignatureScheme::ECDSA_NISTP256_SHA256)
            .then(|| Box::new(self.clone()) as Box<dyn Signer>)
    }

    fn public_key(&self) -> Option<SubjectPublicKeyInfoDer<'_>> {
        Some(subject_public_key_info(self.0.public_point()).into())
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}

impl Signer for TlsKey {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        self.0
            .sign(message)
            .map_err(|e| rustls::Error::General(format!("the key's holder refused to sign: {e}")))
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}

/// A key in memory, for tests — which is exactly the thing the product
/// never does, so it is compiled only into tests and into crates that ask
/// for the `testing` feature to test against this one.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};

    pub struct MemorySigner {
        pair: EcdsaKeyPair,
        pkcs8: Vec<u8>,
    }

    impl fmt::Debug for MemorySigner {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("MemorySigner")
        }
    }

    impl MemorySigner {
        pub fn generate() -> Self {
            let rng = SystemRandom::new();
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng)
                .expect("generating a P-256 key")
                .as_ref()
                .to_vec();
            let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &pkcs8, &rng)
                .expect("reading back a P-256 key");
            Self { pair, pkcs8 }
        }

        /// The private key, for a test server that needs one of its own.
        pub fn pkcs8(&self) -> &[u8] {
            &self.pkcs8
        }
    }

    impl KeySigner for MemorySigner {
        fn public_point(&self) -> &[u8] {
            self.pair.public_key().as_ref()
        }

        fn sign(&self, message: &[u8]) -> Result<Vec<u8>> {
            let signature = self
                .pair
                .sign(&SystemRandom::new(), message)
                .map_err(|_| anyhow::anyhow!("signing failed"))?;
            Ok(signature.as_ref().to_vec())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::MemorySigner;
    use super::*;
    use ring::signature::{UnparsedPublicKey, ECDSA_P256_SHA256_ASN1};

    #[test]
    fn a_signature_verifies_against_the_point() {
        let key = MemorySigner::generate();
        assert_eq!(key.public_point().len(), 65);
        assert_eq!(key.public_point()[0], 0x04);
        let signature = key.sign(b"message").unwrap();
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, key.public_point())
            .verify(b"message", &signature)
            .unwrap();
    }

    #[test]
    fn the_key_info_is_what_the_certificate_builder_writes() {
        let key = MemorySigner::generate();
        let built = rcgen::PublicKeyData::subject_public_key_info(&CertKey(&key));
        assert_eq!(subject_public_key_info(key.public_point()), built);
    }

    #[test]
    fn tls_offers_the_key_only_for_p256() {
        let key = TlsKey(Arc::new(MemorySigner::generate()));
        assert!(key
            .choose_scheme(&[SignatureScheme::RSA_PSS_SHA256])
            .is_none());
        let signer = key
            .choose_scheme(&[
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::ECDSA_NISTP256_SHA256,
            ])
            .unwrap();
        assert_eq!(signer.scheme(), SignatureScheme::ECDSA_NISTP256_SHA256);
    }
}

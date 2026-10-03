//! Trading the workspace's Google leaf for an access token.
//!
//! Workload Identity Federation with X.509 certificates: the IDE opens a
//! mutual-TLS connection to Google's token service, presenting the Google
//! leaf, whose key signs the handshake from wherever it lives, and asks
//! for an access token for the provider's audience. What comes back is
//! good for about an hour. No secret is sent, and none is stored: the
//! certificate is public, and the proof is the signature only the key's
//! holder can make.
//!
//! The request is the one Google's documentation gives
//! (docs/spikes/glm-on-gcp.md → "Credentials and the tunnel"): a token
//! exchange whose subject token is a JSON array of base64 DER
//! certificates, leaf first.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use base64::Engine;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use rustls::client::ResolvesClientCert;
use rustls::pki_types::CertificateDer;
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig, RootCertStore, SignatureScheme};
use serde::Deserialize;
use serde_json::json;

use crate::signer::{KeySigner, TlsKey};

/// Google's token service, on its mutual-TLS host.
pub const STS_MTLS_ENDPOINT: &str = "https://sts.mtls.googleapis.com/v1/token";
/// The access token's scope. Broad by name; what the token can actually
/// do is the custom role the setup granted the federated identity.
pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// How long before its stated expiry a token stops being used, so no
/// request goes out with one that lapses on the way.
pub const EXPIRY_MARGIN: Duration = Duration::from_secs(5 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A bearer token for Google's APIs, and when it lapses.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken {
    pub token: String,
    pub expires_at: SystemTime,
}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessToken")
            .field("token", &"…")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl AccessToken {
    /// Usable for another [`EXPIRY_MARGIN`] at least.
    pub fn is_fresh(&self, now: SystemTime) -> bool {
        self.expires_at
            .duration_since(now)
            .is_ok_and(|left| left > EXPIRY_MARGIN)
    }
}

/// The trust anchors for Google's own endpoints: webpki's roots, so they
/// are the same inside and outside the Flatpak sandbox, as the proxy's are.
pub fn google_roots() -> RootCertStore {
    RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    }
}

/// One client certificate, offered whenever the server's schemes allow
/// P-256, which is the only kind of key there is.
#[derive(Debug)]
struct OneCertificate(Arc<CertifiedKey>);

impl ResolvesClientCert for OneCertificate {
    fn resolve(
        &self,
        _root_hint_subjects: &[&[u8]],
        schemes: &[SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        schemes
            .contains(&SignatureScheme::ECDSA_NISTP256_SHA256)
            .then(|| self.0.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// A TLS client that trusts `roots` and authenticates with `chain`, its
/// first certificate's key held by `key`.
pub fn mutual_tls_config(
    roots: RootCertStore,
    chain: Vec<CertificateDer<'static>>,
    key: Arc<dyn KeySigner>,
) -> Result<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certified = CertifiedKey::new(chain, Arc::new(TlsKey(key)));
    Ok(ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_root_certificates(roots)
        .with_client_cert_resolver(Arc::new(OneCertificate(Arc::new(certified)))))
}

#[derive(Deserialize)]
struct Granted {
    access_token: String,
    expires_in: u64,
}

#[derive(Deserialize)]
struct Refused {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

/// Exchange `leaf` for an access token for `audience`
/// ([`crate::setup::Federation::audience`]), over a connection built from
/// `config` — which is [`mutual_tls_config`] with `leaf` and its key.
pub async fn exchange(
    config: ClientConfig,
    endpoint: &str,
    audience: &str,
    leaf: &CertificateDer<'_>,
) -> Result<AccessToken> {
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_only()
        .enable_http1()
        .build();
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build(https);

    let subject_token =
        serde_json::to_string(&[base64::engine::general_purpose::STANDARD.encode(leaf)])?;
    let body = json!({
        "grant_type": "urn:ietf:params:oauth:grant-type:token-exchange",
        "audience": audience,
        "scope": SCOPE,
        "requested_token_type": "urn:ietf:params:oauth:token-type:access_token",
        "subject_token_type": "urn:ietf:params:oauth:token-type:mtls",
        "subject_token": subject_token,
    });
    let request = http::Request::post(endpoint)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body.to_string())))?;

    let asked = SystemTime::now();
    let response = tokio::time::timeout(REQUEST_TIMEOUT, client.request(request))
        .await
        .context("Google's token service did not answer within 30s")?
        .context("reaching Google's token service with the workspace's certificate")?;
    let status = response.status();
    let bytes = response.into_body().collect().await?.to_bytes();
    if !status.is_success() {
        if let Ok(refused) = serde_json::from_slice::<Refused>(&bytes) {
            bail!(
                "Google's token service refused the workspace's certificate ({status}): {}{}",
                refused.error,
                refused
                    .error_description
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default()
            );
        }
        bail!("Google's token service answered {status}");
    }
    let granted: Granted =
        serde_json::from_slice(&bytes).context("reading the token service's answer")?;
    Ok(AccessToken {
        token: granted.access_token,
        expires_at: asked + Duration::from_secs(granted.expires_in),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{issue_ca, issue_leaf, Issued, Leaf};
    use crate::resources::Workspace;
    use crate::signer::testing::MemorySigner;
    use crate::signer::CertKey;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::server::WebPkiClientVerifier;
    use rustls::ServerConfig;
    use std::convert::Infallible;
    use std::sync::Mutex;

    struct Fixture {
        ca_cert: Issued,
        key: Arc<MemorySigner>,
        leaf: Issued,
    }

    fn fixture() -> Fixture {
        let ws = Workspace::new("0a1b2c3d").unwrap();
        let now = SystemTime::now();
        let ca = MemorySigner::generate();
        let key = Arc::new(MemorySigner::generate());
        let ca_cert = issue_ca(&ws, &ca, now).unwrap();
        let leaf = issue_leaf(&ws, &ca, key.as_ref(), Leaf::Google, now).unwrap();
        Fixture { ca_cert, key, leaf }
    }

    /// What the mock token service saw.
    #[derive(Default)]
    struct Seen {
        client_certificate: Option<Vec<u8>>,
        path: Option<String>,
        body: Option<serde_json::Value>,
    }

    /// A token service on loopback that demands a client certificate from
    /// `trusted_ca`, answers one request with `status` and `answer`, and
    /// returns its address, the client config that trusts it, and what it
    /// saw.
    async fn token_service(
        trusted_ca: &Issued,
        status: u16,
        answer: &'static str,
    ) -> (String, RootCertStore, Arc<Mutex<Seen>>) {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server_key = MemorySigner::generate();
        let mut server_params =
            rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
        server_params.serial_number = Some(rcgen::SerialNumber::from_slice(&[1]));
        let server_cert = server_params.self_signed(&CertKey(&server_key)).unwrap();
        let mut client_roots = RootCertStore::empty();
        client_roots.add(trusted_ca.der.clone()).unwrap();
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(client_roots), provider.clone())
                .build()
                .unwrap();
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                vec![server_cert.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.pkcs8().to_vec())),
            )
            .unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(server_cert.der().clone()).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let record = seen.clone();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let Ok(tls) = tokio_rustls::TlsAcceptor::from(Arc::new(config))
                .accept(tcp)
                .await
            else {
                return;
            };
            record.lock().unwrap().client_certificate = tls
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|chain| chain.first())
                .map(|cert| cert.to_vec());
            let service = service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                let record = record.clone();
                async move {
                    let path = request.uri().path().to_string();
                    let bytes = request.into_body().collect().await.unwrap().to_bytes();
                    let mut seen = record.lock().unwrap();
                    seen.path = Some(path);
                    seen.body = serde_json::from_slice(&bytes).ok();
                    let mut response = hyper::Response::new(Full::new(Bytes::from(answer)));
                    *response.status_mut() = hyper::StatusCode::from_u16(status).unwrap();
                    Ok::<_, Infallible>(response)
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(tls), service)
                .await;
        });
        (format!("https://{address}/v1/token"), roots, seen)
    }

    #[tokio::test]
    async fn the_leaf_is_presented_and_named_and_a_token_comes_back() {
        let f = fixture();
        let (endpoint, roots, seen) = token_service(
            &f.ca_cert,
            200,
            r#"{"access_token":"ya29.test","issued_token_type":"urn:ietf:params:oauth:token-type:access_token","token_type":"Bearer","expires_in":3599}"#,
        )
        .await;
        let config = mutual_tls_config(roots, vec![f.leaf.der.clone()], f.key.clone()).unwrap();
        let before = SystemTime::now();
        let token = exchange(
            config,
            &endpoint,
            "//iam.googleapis.com/audience",
            &f.leaf.der,
        )
        .await
        .unwrap();

        assert_eq!(token.token, "ya29.test");
        assert!(token.expires_at >= before + Duration::from_secs(3599));
        assert!(token.is_fresh(SystemTime::now()));

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.client_certificate.as_deref(),
            Some(f.leaf.der.as_ref())
        );
        assert_eq!(seen.path.as_deref(), Some("/v1/token"));
        let body = seen.body.as_ref().unwrap();
        assert_eq!(
            body["grant_type"],
            "urn:ietf:params:oauth:grant-type:token-exchange"
        );
        assert_eq!(
            body["subject_token_type"],
            "urn:ietf:params:oauth:token-type:mtls"
        );
        assert_eq!(body["audience"], "//iam.googleapis.com/audience");
        let chain: Vec<String> =
            serde_json::from_str(body["subject_token"].as_str().unwrap()).unwrap();
        assert_eq!(
            chain,
            vec![base64::engine::general_purpose::STANDARD.encode(&f.leaf.der)]
        );
    }

    #[tokio::test]
    async fn a_refusal_is_said_in_the_services_own_words() {
        let f = fixture();
        let (endpoint, roots, _) = token_service(
            &f.ca_cert,
            400,
            r#"{"error":"invalid_grant","error_description":"The certificate is not trusted by the provider."}"#,
        )
        .await;
        let config = mutual_tls_config(roots, vec![f.leaf.der.clone()], f.key.clone()).unwrap();
        let error = exchange(config, &endpoint, "aud", &f.leaf.der)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid_grant"), "{error}");
        assert!(error.contains("not trusted by the provider"), "{error}");
    }

    #[tokio::test]
    async fn a_server_that_trusts_another_ca_ends_the_handshake() {
        let f = fixture();
        let stranger = MemorySigner::generate();
        let other_ca = issue_ca(
            &Workspace::new("ffffffff").unwrap(),
            &stranger,
            SystemTime::now(),
        )
        .unwrap();
        let (endpoint, roots, seen) = token_service(&other_ca, 200, "{}").await;
        let config = mutual_tls_config(roots, vec![f.leaf.der.clone()], f.key.clone()).unwrap();
        assert!(exchange(config, &endpoint, "aud", &f.leaf.der)
            .await
            .is_err());
        assert!(seen.lock().unwrap().body.is_none());
    }

    #[test]
    fn a_token_is_stale_inside_the_margin() {
        let now = SystemTime::now();
        let token = AccessToken {
            token: "ya29.secret".into(),
            expires_at: now + EXPIRY_MARGIN,
        };
        assert!(!token.is_fresh(now));
        assert!(token.is_fresh(now - Duration::from_secs(1)));
        assert!(!format!("{token:?}").contains("secret"));
    }
}

//! The IDE's local models: one file each, pinned by URL and digest, fetched
//! once into the user's data directory.
//!
//! The pin is the same rule adapter packages follow (no `latest`): a model
//! that changed under the IDE would change what the microphone hears or
//! what a search means, and a digest mismatch is refused rather than
//! trusted. The download is the IDE's own process talking to one host;
//! nothing of the user's — voice, code, queries — ever leaves the machine,
//! because every model here runs locally. The specs live with what uses
//! them (`taste_voice::BASE_EN`, `taste_semantic::EMBEDDING`); this crate
//! is the fetch, the pin check and the directory, once.
//!
//! [`fetch_pinned`] is that fetch on its own, for pinned artifacts that are
//! not models and do not live in the models directory — the VM guest image
//! (`taste_devcontainer::guest`) is one. It is exposed rather than copied
//! because a second downloader would be a second place for the digest check
//! to be got wrong, and the digest check is the entire point.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// One model file: where it comes from, what it must hash to, how big it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelSpec {
    pub name: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    /// Hex SHA-256 of the file, computed from a real download.
    pub sha256: &'static str,
    pub bytes: u64,
}

const MAX_REDIRECTS: usize = 5;

/// `$XDG_DATA_HOME/taste-ide/models`, or `~/.local/share/taste-ide/models`.
pub fn models_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("taste-ide").join("models")
}

pub fn model_path(spec: &ModelSpec) -> PathBuf {
    models_dir().join(spec.file)
}

/// Present and the right size. The digest is checked when the file
/// arrives, not on every start: a 150 MB hash per launch is a cost, and a
/// file this process wrote and nothing else touches is its own record.
pub fn is_present(spec: &ModelSpec) -> bool {
    std::fs::metadata(model_path(spec)).is_ok_and(|meta| meta.len() == spec.bytes)
}

/// Fetch the model into place, reporting `(downloaded, total)` as bytes
/// land. Writes a `.part` file and renames it only after the digest and
/// the size both match; a mismatch removes the part and names both
/// digests, so a changed upstream is visible rather than trusted.
pub async fn download(
    spec: &ModelSpec,
    progress: impl Fn(u64, u64) + Send + Sync + 'static,
) -> Result<PathBuf> {
    fetch_pinned(
        spec.name,
        spec.url,
        spec.sha256,
        Some(spec.bytes),
        &model_path(spec),
        progress,
    )
    .await
}

/// Fetch one pinned artifact to `target`, verifying its digest before it
/// takes that name.
///
/// The general form of [`download`]. `expected_bytes` is checked too when
/// the caller knows it; a pin that carries only a digest passes `None`,
/// which is the case for artifacts whose publisher states a hash and not a
/// size.
///
/// Writes a `.part` file and renames it only once the digest matches. A
/// mismatch removes the part and names both digests, so a changed upstream
/// is visible rather than trusted — which for a thing the IDE is about to
/// boot as a virtual machine is the difference between a pin and a wish.
pub async fn fetch_pinned(
    name: &str,
    url: &str,
    sha256: &str,
    expected_bytes: Option<u64>,
    target: &std::path::Path,
    progress: impl Fn(u64, u64) + Send + Sync + 'static,
) -> Result<PathBuf> {
    let target = target.to_path_buf();
    let part = target.with_extension("part");
    if let Some(dir) = target.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating {}", dir.display()))?;
    }

    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_only()
        .enable_http1()
        .wrap_connector(http);
    let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(https);

    // Hugging Face answers `resolve/` with a redirect to its CDN; the
    // legacy client follows nothing on its own.
    let mut url: String = url.to_string();
    let mut response = None;
    for _ in 0..=MAX_REDIRECTS {
        let uri: http::Uri = url
            .parse()
            .with_context(|| format!("bad model URL {url}"))?;
        let got = client
            .get(uri)
            .await
            .with_context(|| format!("fetching {url}"))?;
        if got.status().is_redirection() {
            let next = got
                .headers()
                .get(http::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .context("redirect without a Location")?;
            url = if next.starts_with('/') {
                let base: http::Uri = url.parse()?;
                format!(
                    "{}://{}{}",
                    base.scheme_str().unwrap_or("https"),
                    base.authority().map(|a| a.as_str()).unwrap_or_default(),
                    next
                )
            } else {
                next.to_string()
            };
            continue;
        }
        if !got.status().is_success() {
            bail!("{} answered {} for {url}", name, got.status());
        }
        response = Some(got);
        break;
    }
    let response = response.with_context(|| format!("too many redirects fetching {name}"))?;
    let total = response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .or(expected_bytes)
        .unwrap_or(0);

    let mut file = tokio::fs::File::create(&part)
        .await
        .with_context(|| format!("creating {}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;
    let mut body = response.into_body();
    progress(0, total);
    while let Some(frame) = body.frame().await {
        let frame = frame.with_context(|| format!("reading the {name} download"))?;
        if let Some(chunk) = frame.data_ref() {
            hasher.update(chunk);
            file.write_all(chunk).await?;
            downloaded += chunk.len() as u64;
            progress(downloaded, total);
        }
    }
    file.flush().await?;
    drop(file);

    let digest = format!("{:x}", hasher.finalize());
    let wrong_size = expected_bytes.is_some_and(|want| downloaded != want);
    if digest != sha256 || wrong_size {
        let _ = tokio::fs::remove_file(&part).await;
        bail!(
            "{name} did not match its pin: got {downloaded} bytes, sha256 {digest}; \
             expected {} bytes, sha256 {sha256}. Nothing was kept.",
            expected_bytes
                .map(|b| b.to_string())
                .unwrap_or_else(|| "any".into()),
        );
    }
    tokio::fs::rename(&part, &target)
        .await
        .with_context(|| format!("placing {}", target.display()))?;
    Ok(target)
}

/// A small document over HTTPS, whole, as text: what a stream document
/// is (`taste_devcontainer::guest::refresh_from_stream`). Refused past
/// `limit` bytes, since a document that size is not what was asked for,
/// and after `timeout`, since the caller has a start to get on with.
pub async fn fetch_text(url: &str, limit: usize, timeout: std::time::Duration) -> Result<String> {
    tokio::time::timeout(timeout, fetch_text_inner(url, limit))
        .await
        .with_context(|| format!("{url} did not answer within {}s", timeout.as_secs()))?
}

async fn fetch_text_inner(url: &str, limit: usize) -> Result<String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_only()
        .enable_http1()
        .wrap_connector(http);
    let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(https);
    let uri: http::Uri = url.parse().with_context(|| format!("bad URL {url}"))?;
    let response = client
        .get(uri)
        .await
        .with_context(|| format!("fetching {url}"))?;
    if !response.status().is_success() {
        bail!("{url} answered {}", response.status());
    }
    let mut body = response.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.with_context(|| format!("reading {url}"))?;
        if let Some(chunk) = frame.data_ref() {
            bytes.extend_from_slice(chunk);
            if bytes.len() > limit {
                bail!("{url} is larger than the {limit} bytes asked for");
            }
        }
    }
    String::from_utf8(bytes).with_context(|| format!("{url} is not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_lives_under_the_data_dir_and_presence_means_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_DATA_HOME", dir.path());
        assert_eq!(
            models_dir(),
            dir.path().join("taste-ide").join("models"),
            "XDG_DATA_HOME wins"
        );
        const SPEC: ModelSpec = ModelSpec {
            name: "probe",
            file: "probe.bin",
            url: "https://example.invalid/probe.bin",
            sha256: "00",
            bytes: 10,
        };
        assert!(!is_present(&SPEC));
        std::fs::create_dir_all(models_dir()).unwrap();
        std::fs::write(model_path(&SPEC), b"short").unwrap();
        assert!(!is_present(&SPEC), "a truncated file is not the model");
        std::fs::write(model_path(&SPEC), b"0123456789").unwrap();
        assert!(is_present(&SPEC), "the whole file is");
    }
}

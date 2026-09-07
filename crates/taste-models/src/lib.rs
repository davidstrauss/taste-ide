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
    let target = model_path(spec);
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
    let mut url: String = spec.url.to_string();
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
            bail!("the model host answered {} for {url}", got.status());
        }
        response = Some(got);
        break;
    }
    let response = response.context("too many redirects fetching the model")?;
    let total = response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(spec.bytes);

    let mut file = tokio::fs::File::create(&part)
        .await
        .with_context(|| format!("creating {}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;
    let mut body = response.into_body();
    progress(0, total);
    while let Some(frame) = body.frame().await {
        let frame = frame.context("reading the model download")?;
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
    if digest != spec.sha256 || downloaded != spec.bytes {
        let _ = tokio::fs::remove_file(&part).await;
        bail!(
            "the model {} did not match its pin: got {downloaded} bytes, sha256 {digest}; \
             expected {} bytes, sha256 {}. Nothing was kept.",
            spec.name,
            spec.bytes,
            spec.sha256
        );
    }
    tokio::fs::rename(&part, &target)
        .await
        .with_context(|| format!("placing {}", target.display()))?;
    Ok(target)
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

//! V4 signed URLs for Cloud Storage, signed by the IDE's service account
//! through IAM's `signBlob` — the way a VM with no service account reads
//! the weights, and the staging VM writes them, without a credential of
//! its own (the spike's "Weights from GCS instead of a disk").
//!
//! A URL names one object, one method, and the headers that must come
//! with it, and lapses after at most seven days; it is as capable as the
//! account that signed it, and no more. The signing follows Google's
//! published V4 process
//! (<https://docs.cloud.google.com/storage/docs/access-control/signing-urls-manually>):
//! a canonical request, hashed into a string to sign, signed with the
//! account's Google-held key, and appended hex.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use sha2::{Digest, Sha256};

use crate::rest::Gcp;

/// The XML API's host, which is the only one signed URLs work on.
pub const HOST: &str = "storage.googleapis.com";

/// The longest a V4 signed URL may last.
pub const MAX_EXPIRY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

const ALGORITHM: &str = "GOOG4-RSA-SHA256";

/// Percent-encode everything but RFC 3986's unreserved characters, as
/// both the canonical query string and a JSON API query parameter want.
pub fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// An object name as a path: each segment encoded, the slashes kept.
fn encode_path(object: &str) -> String {
    object.split('/').map(encode).collect::<Vec<_>>().join("/")
}

/// `at` as V4 wants it: `YYYYMMDDTHHMMSSZ`, and the date alone.
pub fn timestamp(at: SystemTime) -> (String, String) {
    let seconds = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    // Howard Hinnant's days-to-civil, for the proleptic Gregorian date.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let date = format!("{year:04}{month:02}{day:02}");
    let time = format!(
        "{date}T{:02}{:02}{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    );
    (time, date)
}

/// One request to be signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request<'a> {
    pub method: &'a str,
    pub bucket: &'a str,
    pub object: &'a str,
    /// Headers the request must carry, beyond `host`, which is always
    /// signed: `x-goog-resumable: start` for an upload, and whatever
    /// `x-goog-meta-*` the object is to be stored with.
    pub headers: Vec<(String, String)>,
    pub expires: Duration,
}

/// A request made canonical: the URL without its signature, and the
/// string the signature is over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsigned {
    pub url: String,
    pub string_to_sign: String,
}

/// The canonical form of `request` by `account` at `at`.
pub fn prepare(account: &str, request: &Request<'_>, at: SystemTime) -> Result<Unsigned> {
    if request.expires > MAX_EXPIRY || request.expires.is_zero() {
        bail!(
            "a signed URL lasts between a second and seven days, not {:?}",
            request.expires
        );
    }
    let (time, date) = timestamp(at);
    let scope = format!("{date}/auto/storage/goog4_request");
    let mut headers: Vec<(String, String)> = std::iter::once(("host".into(), HOST.into()))
        .chain(
            request
                .headers
                .iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string())),
        )
        .collect();
    headers.sort();
    let signed_headers = headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect();
    let path = format!("/{}/{}", request.bucket, encode_path(request.object));
    // Already in order: Algorithm, Credential, Date, Expires,
    // SignedHeaders.
    let query = [
        ("X-Goog-Algorithm", ALGORITHM.to_string()),
        ("X-Goog-Credential", format!("{account}/{scope}")),
        ("X-Goog-Date", time.clone()),
        ("X-Goog-Expires", request.expires.as_secs().to_string()),
        ("X-Goog-SignedHeaders", signed_headers.clone()),
    ]
    .iter()
    .map(|(key, value)| format!("{key}={}", encode(value)))
    .collect::<Vec<_>>()
    .join("&");
    let canonical = format!(
        "{}\n{path}\n{query}\n{canonical_headers}\n{signed_headers}\nUNSIGNED-PAYLOAD",
        request.method
    );
    let digest: String = Sha256::digest(canonical.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(Unsigned {
        url: format!("https://{HOST}{path}?{query}"),
        string_to_sign: format!("{ALGORITHM}\n{time}\n{scope}\n{digest}"),
    })
}

/// A signed URL for `request`, signed now by `account` through IAM.
pub async fn sign(gcp: &Gcp, account: &str, request: &Request<'_>) -> Result<String> {
    let unsigned = prepare(account, request, SystemTime::now())?;
    let signature: String = gcp
        .sign_blob(account, unsigned.string_to_sign.as_bytes())
        .await?
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(format!("{}&X-Goog-Signature={signature}", unsigned.url))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = "taste-ide@p.iam.gserviceaccount.com";

    fn get(object: &str) -> Request<'_> {
        Request {
            method: "GET",
            bucket: "taste-weights-p",
            object,
            headers: Vec::new(),
            expires: Duration::from_secs(3600),
        }
    }

    #[test]
    fn only_the_unreserved_characters_go_unencoded() {
        assert_eq!(encode("aZ0-_.~"), "aZ0-_.~");
        assert_eq!(encode("a/b@c d"), "a%2Fb%40c%20d");
        assert_eq!(
            encode_path("org/repo/file name.gguf"),
            "org/repo/file%20name.gguf"
        );
    }

    #[test]
    fn timestamps_are_utc_and_civil() {
        let at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(
            timestamp(at),
            ("20231114T221320Z".to_string(), "20231114".to_string())
        );
        let leap = UNIX_EPOCH + Duration::from_secs(1_709_164_800);
        assert_eq!(timestamp(leap).1, "20240229");
        assert_eq!(timestamp(UNIX_EPOCH).0, "19700101T000000Z");
    }

    #[test]
    fn a_get_is_canonical_and_names_its_scope() {
        let at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let unsigned = prepare(ACCOUNT, &get("org/repo/model.gguf"), at).unwrap();
        assert_eq!(
            unsigned.url,
            "https://storage.googleapis.com/taste-weights-p/org/repo/model.gguf\
             ?X-Goog-Algorithm=GOOG4-RSA-SHA256\
             &X-Goog-Credential=taste-ide%40p.iam.gserviceaccount.com%2F20231114%2Fauto%2Fstorage%2Fgoog4_request\
             &X-Goog-Date=20231114T221320Z&X-Goog-Expires=3600&X-Goog-SignedHeaders=host"
        );
        let lines: Vec<&str> = unsigned.string_to_sign.lines().collect();
        assert_eq!(lines[0], "GOOG4-RSA-SHA256");
        assert_eq!(lines[1], "20231114T221320Z");
        assert_eq!(lines[2], "20231114/auto/storage/goog4_request");
        assert_eq!(lines[3].len(), 64);
    }

    #[test]
    fn an_upload_signs_its_headers_in_order() {
        let at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let request = Request {
            method: "POST",
            headers: vec![
                ("x-goog-resumable".into(), "start".into()),
                ("X-Goog-Meta-Sha256".into(), " abc ".into()),
            ],
            ..get("a.gguf")
        };
        let unsigned = prepare(ACCOUNT, &request, at).unwrap();
        assert!(unsigned
            .url
            .ends_with("X-Goog-SignedHeaders=host%3Bx-goog-meta-sha256%3Bx-goog-resumable"));
        // A different header value is a different signature.
        let other = Request {
            headers: vec![
                ("x-goog-resumable".into(), "start".into()),
                ("x-goog-meta-sha256".into(), "abd".into()),
            ],
            ..request.clone()
        };
        assert_ne!(
            prepare(ACCOUNT, &other, at).unwrap().string_to_sign,
            unsigned.string_to_sign
        );
    }

    #[test]
    fn a_url_lasts_at_most_seven_days() {
        let at = SystemTime::now();
        let long = Request {
            expires: MAX_EXPIRY + Duration::from_secs(1),
            ..get("a")
        };
        assert!(prepare(ACCOUNT, &long, at).is_err());
        let never = Request {
            expires: Duration::ZERO,
            ..get("a")
        };
        assert!(prepare(ACCOUNT, &never, at).is_err());
    }
}

//! A one-purpose HTTPS GET, for the one artifact the IDE fetches.
//!
//! This exists for [`crate::machine::ensure_gvproxy`] and nothing else. It
//! is deliberately small and deliberately unfriendly: no cache, no
//! resumption, no progress, a hard size cap, and a caller that verifies a
//! pinned sha256 over every byte before the result is allowed to become a
//! file. The transport is not trusted to deliver the right bytes; the hash
//! is what says they are right.
//!
//! rustls with webpki roots, never openssl and never the host trust store —
//! the same choice the auth proxy documents, for the same two reasons: the
//! Flatpak runtime ships no openssl, and the trust anchors must be
//! identical inside and outside the sandbox.
//!
//! Blocking, on purpose. It runs from [`crate::machine::Helpers::arrange`],
//! which is itself called from machine lifecycle operations already running
//! off the GTK thread, and a synchronous function is easier to reason about
//! than a second async surface used once.

use std::io::Read;

use anyhow::{bail, Context, Result};

/// Ceiling on a fetched artifact. gvproxy is ~13 MB; anything an order of
/// magnitude past that is a redirect to something we did not ask for.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Redirects to follow. GitHub release downloads take two hops (to
/// `objects.githubusercontent.com`); a handful of headroom, and no loops.
const MAX_REDIRECTS: usize = 5;

/// GET a URL over HTTPS and return the body.
///
/// `https` only — a pinned artifact fetched over plaintext would still be
/// hash-checked, but offering the option invites a call site that forgets.
pub fn get(url: &str) -> Result<Vec<u8>> {
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        match request(&url)? {
            Response::Body(bytes) => return Ok(bytes),
            Response::Redirect(location) => {
                url = location;
            }
        }
    }
    bail!("too many redirects fetching {url}")
}

enum Response {
    Body(Vec<u8>),
    Redirect(String),
}

fn request(url: &str) -> Result<Response> {
    let (host, port, path) = split_url(url)?;

    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server = rustls::pki_types::ServerName::try_from(host.clone())
        .with_context(|| format!("{host} is not a valid server name"))?;
    let mut session = rustls::ClientConnection::new(std::sync::Arc::new(config), server)
        .context("starting a TLS session")?;
    let mut socket = std::net::TcpStream::connect((host.as_str(), port))
        .with_context(|| format!("connecting to {host}:{port}"))?;
    let mut tls = rustls::Stream::new(&mut session, &mut socket);

    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: taste-ide\r\n\
         Accept: */*\r\n\
         Connection: close\r\n\r\n"
    );
    std::io::Write::write_all(&mut tls, request.as_bytes())
        .with_context(|| format!("sending the request to {host}"))?;

    let mut raw = Vec::new();
    // `Connection: close` means the body ends at EOF, so the cap is the
    // only thing standing between a hostile server and this process's
    // memory.
    let read = tls
        .take(MAX_BYTES + 1)
        .read_to_end(&mut raw)
        .or_else(|e| match e.kind() {
            // A server that closes without a clean TLS shutdown is
            // ordinary; the bytes already read are still the response.
            std::io::ErrorKind::UnexpectedEof if !raw.is_empty() => Ok(raw.len()),
            _ => Err(e),
        })
        .with_context(|| format!("reading the response from {host}"))?;
    if read as u64 > MAX_BYTES {
        bail!("{url} is larger than the {MAX_BYTES}-byte ceiling for a fetched artifact");
    }

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("the response had no header terminator")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let body = raw[split + 4..].to_vec();

    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .context("the response had no status line")?;

    match status {
        200 => Ok(Response::Body(body)),
        301..=308 => {
            let location = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.trim()
                        .eq_ignore_ascii_case("location")
                        .then(|| value.trim().to_string())
                })
                .context("a redirect with no Location")?;
            Ok(Response::Redirect(location))
        }
        other => bail!("{url} answered HTTP {other}"),
    }
}

/// `https://host[:port]/path` → its three parts. Anything else is refused.
fn split_url(url: &str) -> Result<(String, u16, String)> {
    let rest = url
        .strip_prefix("https://")
        .with_context(|| format!("{url} is not https"))?;
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() {
        bail!("{url} has no host");
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host.to_string(),
            port.parse().with_context(|| format!("{url} has no port"))?,
        ),
        None => (authority.to_string(), 443),
    };
    Ok((host, port, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_into_host_port_and_path() {
        assert_eq!(
            split_url("https://github.com/containers/x/releases/download/v1/bin").unwrap(),
            (
                "github.com".to_string(),
                443,
                "/containers/x/releases/download/v1/bin".to_string()
            )
        );
        assert_eq!(
            split_url("https://example.test:8443/a").unwrap(),
            ("example.test".to_string(), 8443, "/a".to_string())
        );
        assert_eq!(
            split_url("https://example.test").unwrap().2,
            "/",
            "a bare host still asks for something"
        );
    }

    /// Plaintext is refused outright. The hash would still catch a
    /// substituted binary, but a call site that could reach for `http://`
    /// is one that will.
    #[test]
    fn plaintext_and_nonsense_are_refused() {
        assert!(split_url("http://example.test/a").is_err());
        assert!(split_url("ftp://example.test/a").is_err());
        assert!(split_url("https:///a").is_err());
        assert!(split_url("not a url").is_err());
    }
}

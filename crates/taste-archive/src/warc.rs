//! WARC (ISO 28500): records one after another, each a version line,
//! headers, and a block of `Content-Length` bytes, the whole file or each
//! record gzipped. A `response` record's block is the HTTP response as it
//! arrived, its body still chunked or compressed as the server sent it; a
//! `resource` record's is the thing itself; a `revisit` record says a URL
//! was captured again and came back the same as an earlier capture.

use std::borrow::Cow;
use std::io::Read as _;

use crate::mhtml::{header, headers, mime_of, param};
use crate::{Captured, Read, MAX_UNPACKED};

pub(crate) fn read(bytes: &[u8]) -> Result<Read, String> {
    let data: Cow<[u8]> = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::MultiGzDecoder::new(bytes)
            .take(MAX_UNPACKED as u64 + 1)
            .read_to_end(&mut out)
            .map_err(|e| format!("the WARC's gzip does not unpack: {e}"))?;
        if out.len() > MAX_UNPACKED {
            return Err("the WARC unpacks to more than the viewer reads".into());
        }
        Cow::Owned(out)
    } else {
        Cow::Borrowed(bytes)
    };
    let data = data.as_ref();
    let mut captures: Vec<Captured> = Vec::new();
    // A revisit's URL, and the URL it is the same as.
    let mut revisits: Vec<(String, String)> = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        while pos < data.len() && matches!(data[pos], b'\r' | b'\n') {
            pos += 1;
        }
        if pos >= data.len() {
            break;
        }
        if !data[pos..].starts_with(b"WARC/") {
            return Err(format!("this is not a WARC record, at byte {pos}"));
        }
        let version_end = data[pos..]
            .iter()
            .position(|b| *b == b'\n')
            .ok_or("a WARC record ends in its version line")?;
        let (fields, block_start) = headers(data, pos + version_end + 1)?;
        let length: usize = header(&fields, "content-length")
            .and_then(|l| l.trim().parse().ok())
            .ok_or("a WARC record has no Content-Length")?;
        let block = data
            .get(block_start..block_start + length)
            .ok_or("the WARC ends inside a record")?;
        pos = block_start + length;
        let kind = header(&fields, "warc-type").unwrap_or("");
        let Some(target) = header(&fields, "warc-target-uri")
            .map(|t| t.trim().trim_matches(['<', '>']).to_string())
        else {
            continue;
        };
        let content_type = header(&fields, "content-type").unwrap_or("");
        match kind {
            "response" if mime_of(content_type) == "application/http" => {
                if let Some(captured) = http_response(block, &target) {
                    captures.push(captured);
                }
            }
            "resource" => captures.push(Captured {
                url: target,
                status: 200,
                mime: mime_of(content_type),
                charset: param(content_type, "charset"),
                location: None,
                body: block.to_vec(),
            }),
            "revisit" => {
                let same_as = header(&fields, "warc-refers-to-target-uri")
                    .map(|t| t.trim().trim_matches(['<', '>']).to_string())
                    .unwrap_or_else(|| target.clone());
                revisits.push((target, same_as));
            }
            _ => {}
        }
    }
    for (url, same_as) in revisits {
        if url == same_as && captures.iter().any(|c| c.url == url) {
            continue;
        }
        if let Some(earlier) = captures.iter().find(|c| c.url == same_as).cloned() {
            captures.push(Captured { url, ..earlier });
        }
    }
    Ok(Read {
        captures,
        pages: Vec::new(),
    })
}

/// A `response` record's block: the status, the headers that matter, and
/// the body as the page saw it, its transfer and content encodings undone.
fn http_response(block: &[u8], url: &str) -> Option<Captured> {
    let mut fields = [httparse::EMPTY_HEADER; 256];
    let mut response = httparse::Response::new(&mut fields);
    let httparse::Status::Complete(head) = response.parse(block).ok()? else {
        return None;
    };
    let status = response.code?;
    let field = |name: &str| {
        response
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| String::from_utf8_lossy(h.value).trim().to_string())
    };
    let content_type = field("content-type").unwrap_or_default();
    let mut body = block[head..].to_vec();
    if field("transfer-encoding").is_some_and(|t| t.to_ascii_lowercase().contains("chunked")) {
        if let Some(whole) = dechunk(&body) {
            body = whole;
        }
    }
    if let Some(encoding) = field("content-encoding") {
        if let Some(decoded) = decode(&body, &encoding.to_ascii_lowercase()) {
            body = decoded;
        }
    }
    Some(Captured {
        url: url.to_string(),
        status,
        mime: mime_of(&content_type),
        charset: param(&content_type, "charset"),
        location: field("location"),
        body,
    })
}

/// A chunked body made whole; `None` when it is not chunked after all.
fn dechunk(body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(body.len());
    let mut pos = 0;
    loop {
        let line_end = pos + body.get(pos..)?.iter().position(|b| *b == b'\n')?;
        let size_text = String::from_utf8_lossy(&body[pos..line_end]);
        let size_text = size_text.trim().split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).ok()?;
        pos = line_end + 1;
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(body.get(pos..pos + size)?);
        pos += size;
        // The chunk's own line break.
        if body.get(pos) == Some(&b'\r') {
            pos += 1;
        }
        if body.get(pos) == Some(&b'\n') {
            pos += 1;
        }
    }
}

/// A body's content encoding undone: gzip, deflate (zlib-wrapped, or raw
/// as some servers send it), or brotli. `None` when it does not decode,
/// and the body is kept as it is: an archiver that stored it decoded
/// still names the encoding.
fn decode(body: &[u8], encoding: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let limit = MAX_UNPACKED as u64;
    let read = match encoding.trim() {
        "gzip" | "x-gzip" => flate2::read::MultiGzDecoder::new(body)
            .take(limit)
            .read_to_end(&mut out),
        "deflate" => {
            let zlib = flate2::read::ZlibDecoder::new(body)
                .take(limit)
                .read_to_end(&mut out);
            if zlib.is_err() {
                out.clear();
                flate2::read::DeflateDecoder::new(body)
                    .take(limit)
                    .read_to_end(&mut out)
            } else {
                zlib
            }
        }
        "br" => brotli_decompressor::Decompressor::new(body, 4096)
            .take(limit)
            .read_to_end(&mut out),
        _ => return None,
    };
    read.ok().map(|_| out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: &str, uri: &str, content_type: &str, block: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "WARC/1.1\r\nWARC-Type: {kind}\r\nWARC-Target-URI: {uri}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            block.len()
        )
        .into_bytes();
        out.extend_from_slice(block);
        out.extend_from_slice(b"\r\n\r\n");
        out
    }

    #[test]
    fn a_chunked_gzipped_response_comes_back_as_its_page() {
        let mut gz = Vec::new();
        {
            use std::io::Write as _;
            let mut encoder = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::fast());
            encoder.write_all(b"<p>hello</p>").unwrap();
            encoder.finish().unwrap();
        }
        let mut http = b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nTransfer-Encoding: chunked\r\nContent-Encoding: gzip\r\n\r\n".to_vec();
        http.extend_from_slice(format!("{:x}\r\n", gz.len()).as_bytes());
        http.extend_from_slice(&gz);
        http.extend_from_slice(b"\r\n0\r\n\r\n");
        let mut warc = record(
            "warcinfo",
            "urn:x",
            "application/warc-fields",
            b"software: test\r\n",
        );
        warc.extend(record(
            "response",
            "<https://example.com/>",
            "application/http; msgtype=response",
            &http,
        ));
        warc.extend(record(
            "revisit",
            "https://example.com/again",
            "application/http",
            b"",
        ));
        let read = read(&warc).unwrap();
        assert_eq!(
            read.captures.len(),
            1,
            "a revisit of another URL with no earlier capture is passed over"
        );
        let page = &read.captures[0];
        assert_eq!(
            (page.url.as_str(), page.status, page.mime.as_str()),
            ("https://example.com/", 200, "text/html")
        );
        assert_eq!(page.charset.as_deref(), Some("utf-8"));
        assert_eq!(page.body, b"<p>hello</p>");
    }

    #[test]
    fn a_revisit_serves_what_it_refers_to() {
        let http = b"HTTP/1.1 200 OK\r\nContent-Type: text/css\r\n\r\np{}";
        let mut warc = record(
            "response",
            "https://example.com/a.css",
            "application/http; msgtype=response",
            http,
        );
        let revisit = b"WARC/1.1\r\nWARC-Type: revisit\r\nWARC-Target-URI: https://example.com/a.css?v=2\r\nWARC-Refers-To-Target-URI: https://example.com/a.css\r\nContent-Length: 0\r\n\r\n\r\n\r\n";
        warc.extend_from_slice(revisit);
        let read = read(&warc).unwrap();
        assert_eq!(read.captures.len(), 2);
        assert_eq!(read.captures[1].url, "https://example.com/a.css?v=2");
        assert_eq!(read.captures[1].body, b"p{}");
    }
}

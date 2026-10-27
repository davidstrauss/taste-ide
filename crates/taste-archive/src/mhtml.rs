//! MHTML (RFC 2557): a page and what it needs as the parts of one MIME
//! `multipart/related` message, each part named by its `Content-Location`
//! or its `Content-ID`. What Chrome and Edge write when a page is saved as
//! one file.

use base64::Engine;

use crate::{Captured, Read};

/// A message's or a part's headers from `at`: each name lower case, a
/// folded line joined to the one before; and where its body starts.
pub(crate) fn headers(bytes: &[u8], at: usize) -> Result<(Vec<(String, String)>, usize), String> {
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut pos = at;
    loop {
        let end = bytes[pos..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|e| pos + e)
            .ok_or("the headers never end")?;
        let line = String::from_utf8_lossy(&bytes[pos..end]);
        let line = line.trim_end_matches('\r');
        pos = end + 1;
        if line.is_empty() {
            return Ok((headers, pos));
        }
        if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = headers.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
}

/// A header's value, by its lower-case name.
pub(crate) fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// A parameter of a header's value: `boundary` of `multipart/related;
/// boundary="x"`.
pub(crate) fn param(value: &str, name: &str) -> Option<String> {
    value.split(';').skip(1).find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key.trim().eq_ignore_ascii_case(name)).then(|| value.trim().trim_matches('"').to_string())
    })
}

/// The media type of a `Content-Type`, lower case, without parameters.
pub(crate) fn mime_of(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn quoted_printable(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] != b'=' {
            out.push(input[i]);
            i += 1;
            continue;
        }
        match input.get(i + 1..i + 3) {
            Some([b'\r', b'\n']) => i += 3,
            Some([b'\n', _]) => i += 2,
            Some(hex) => match std::str::from_utf8(hex)
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                Some(byte) => {
                    out.push(byte);
                    i += 3;
                }
                None => {
                    out.push(b'=');
                    i += 1;
                }
            },
            // `=` with less than two bytes after it: a soft break whose
            // line ending went with the part's, trimmed before the next
            // delimiter.
            None => i = input.len(),
        }
    }
    out
}

fn base64(input: &[u8]) -> Result<Vec<u8>, String> {
    let compact: Vec<u8> = input
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );
    engine
        .decode(compact)
        .map_err(|e| format!("a base64 part does not decode: {e}"))
}

/// Where `needle` is in `haystack` from `from`.
pub(crate) fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

pub(crate) fn read(bytes: &[u8]) -> Result<Read, String> {
    let (top, body) = headers(bytes, 0)?;
    let content_type = header(&top, "content-type").ok_or("this MHTML has no Content-Type")?;
    let boundary = param(content_type, "boundary")
        .ok_or("this MHTML's Content-Type names no boundary: it is not multipart")?;
    let start = param(content_type, "start").map(|s| s.trim_matches(['<', '>']).to_string());
    let snapshot = header(&top, "snapshot-content-location").map(str::to_string);
    let delimiter = format!("--{boundary}").into_bytes();
    let mut captures = Vec::new();
    let mut root: Option<String> = None;
    let mut pos = find(bytes, &delimiter, body).ok_or("this MHTML has no parts")?;
    loop {
        let after = pos + delimiter.len();
        if bytes.get(after..after + 2) == Some(b"--") {
            break;
        }
        let Some(line_end) = bytes[after..].iter().position(|b| *b == b'\n') else {
            break;
        };
        let part_start = after + line_end + 1;
        let next = find(bytes, &delimiter, part_start);
        let mut part_end = next.unwrap_or(bytes.len());
        // The line break before the next delimiter belongs to it.
        if part_end > part_start && bytes[part_end - 1] == b'\n' {
            part_end -= 1;
            if part_end > part_start && bytes[part_end - 1] == b'\r' {
                part_end -= 1;
            }
        }
        let part = &bytes[part_start..part_end.max(part_start)];
        if let Ok((part_headers, content)) = headers(part, 0) {
            let raw = &part[content..];
            let encoding = header(&part_headers, "content-transfer-encoding")
                .unwrap_or("")
                .to_ascii_lowercase();
            let body = match encoding.as_str() {
                "base64" => base64(raw)?,
                "quoted-printable" => quoted_printable(raw),
                _ => raw.to_vec(),
            };
            let content_type = header(&part_headers, "content-type").unwrap_or("");
            let id = header(&part_headers, "content-id")
                .map(|id| format!("cid:{}", id.trim_matches(['<', '>'])));
            let location = header(&part_headers, "content-location").map(str::to_string);
            let is_root = match (&start, &id) {
                (Some(start), Some(id)) => id.strip_prefix("cid:") == Some(start.as_str()),
                (Some(_), None) => false,
                (None, _) => root.is_none(),
            };
            let names: Vec<String> = location.iter().chain(id.iter()).cloned().collect();
            if is_root {
                root = location
                    .clone()
                    .or_else(|| snapshot.clone())
                    .or_else(|| id.clone());
            }
            for url in names {
                captures.push(Captured {
                    url,
                    status: 200,
                    mime: mime_of(content_type),
                    charset: param(content_type, "charset"),
                    location: None,
                    body: body.clone(),
                });
            }
        }
        match next {
            Some(next) => pos = next,
            None => break,
        }
    }
    if captures.is_empty() {
        return Err("this MHTML has no part with a location".into());
    }
    let pages = root.into_iter().map(|url| (url, None)).collect();
    Ok(Read { captures, pages })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_printable_decodes_escapes_and_soft_breaks() {
        assert_eq!(
            quoted_printable(b"caf=C3=A9 =3D x=\r\ny"),
            "café = xy".as_bytes()
        );
        assert_eq!(quoted_printable(b"a=\nb="), b"ab");
    }

    #[test]
    fn folded_headers_and_parameters_are_read() {
        let message = b"Content-Type: multipart/related;\r\n\ttype=\"text/html\";\r\n\tboundary=\"----B--\"\r\n\r\nbody";
        let (headers, at) = headers(message, 0).unwrap();
        let content_type = header(&headers, "content-type").unwrap();
        assert_eq!(param(content_type, "boundary").as_deref(), Some("----B--"));
        assert_eq!(mime_of(content_type), "multipart/related");
        assert_eq!(&message[at..], b"body");
    }
}

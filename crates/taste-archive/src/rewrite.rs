//! URLs made into paths in the archive, and the HTML and CSS that name
//! them rewritten to those paths.

use std::cell::RefCell;

use lol_html::html_content::{ContentType, Element};
use lol_html::{element, rewrite_str, text, RewriteStrSettings};
use url::Url;

use crate::Captured;

/// Where `url` is served: `/<scheme>/<host[:port]><path>[?query]` for a
/// web URL, `/cid/<id>` for an MHTML part known by its `Content-ID`, and
/// `None` for anything else.
pub fn served_path(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    match parsed.scheme() {
        "http" | "https" => {
            let host = parsed.host_str()?;
            let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
            let query = parsed.query().map(|q| format!("?{q}")).unwrap_or_default();
            Some(format!(
                "/{}/{host}{port}{}{query}",
                parsed.scheme(),
                parsed.path()
            ))
        }
        "cid" => Some(format!("/cid/{}", parsed.path())),
        _ => None,
    }
}

/// How the captures of one URL are told apart from another's: the URL
/// without its fragment.
pub(crate) fn key_of(url: &str) -> String {
    match Url::parse(url) {
        Ok(mut parsed) => {
            parsed.set_fragment(None);
            parsed.to_string()
        }
        Err(_) => url.split('#').next().unwrap_or(url).to_string(),
    }
}

/// A URL as an archived page or stylesheet wrote it, made into its path in
/// the archive, its fragment kept: `None` for one that stays as it is (an
/// anchor in the page, `data:`, a script's `javascript:`, a mail link).
fn rewrite_url(value: &str, base: &Url) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.starts_with('#') {
        return None;
    }
    let lower = value.to_ascii_lowercase();
    if ["data:", "blob:", "javascript:", "about:", "mailto:", "tel:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
    {
        return None;
    }
    let joined = base.join(value).ok()?;
    let fragment = joined
        .fragment()
        .map(|f| format!("#{f}"))
        .unwrap_or_default();
    Some(format!("{}{fragment}", served_path(joined.as_str())?))
}

/// A `srcset`, each candidate's URL rewritten and its descriptor kept.
fn rewrite_srcset(value: &str, base: &Url) -> String {
    value
        .split(',')
        .map(|candidate| {
            let trimmed = candidate.trim();
            let (url, descriptor) = trimmed
                .split_once(char::is_whitespace)
                .unwrap_or((trimmed, ""));
            let url = rewrite_url(url, base).unwrap_or_else(|| url.to_string());
            if descriptor.is_empty() {
                url
            } else {
                format!("{url} {}", descriptor.trim())
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// CSS with every `url(…)` and every `@import "…"` rewritten.
pub(crate) fn rewrite_css(css: &str, base: &Url) -> String {
    // Lowered for matching only: ASCII lowering keeps every byte offset.
    let lower = css.to_ascii_lowercase();
    let mut out = String::with_capacity(css.len());
    let mut i = 0;
    while i < css.len() {
        let opener = if lower[i..].starts_with("url(") {
            Some(4)
        } else if lower[i..].starts_with("@import") {
            Some(7)
        } else {
            None
        };
        let Some(opener) = opener else {
            let ch = css[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        };
        let is_url = opener == 4;
        out.push_str(&css[i..i + opener]);
        i += opener;
        let spaces = css[i..].len() - css[i..].trim_start().len();
        out.push_str(&css[i..i + spaces]);
        i += spaces;
        let quote = css[i..].chars().next().filter(|c| *c == '"' || *c == '\'');
        let start = i + quote.map_or(0, |_| 1);
        let end = match quote {
            Some(q) => css[start..].find(q).map(|e| start + e),
            // An `@import` without quotes is an `url(…)`, which the next
            // turn of the loop takes.
            None if is_url => css[start..].find(')').map(|e| start + e),
            None => continue,
        };
        let Some(end) = end else {
            out.push_str(&css[i..]);
            break;
        };
        let raw = &css[start..end];
        let value = raw.trim_end();
        let rewritten = rewrite_url(value, base).unwrap_or_else(|| value.to_string());
        match quote {
            Some(q) => {
                out.push(q);
                out.push_str(&rewritten);
                out.push(q);
                i = end + 1;
            }
            None => {
                out.push_str(&rewritten);
                out.push_str(&raw[value.len()..]);
                i = end;
            }
        }
    }
    out
}

/// Rewrite the URL in `el`'s `name`, if it has one.
fn rewrite_attribute(el: &mut Element, name: &str, base: &Url) {
    if let Some(value) = el.get_attribute(name) {
        if let Some(rewritten) = rewrite_url(&value, base) {
            let _ = el.set_attribute(name, &rewritten);
        }
    }
}

/// An archived page with every URL it names rewritten into the archive,
/// its `<base>` followed; its subresource integrity hashes and its own
/// Content-Security-Policy dropped, since the first no longer match a
/// rewritten stylesheet and the second would refuse the archive's origin.
/// The page as it was, when it cannot be rewritten.
pub(crate) fn rewrite_html(html: &str, url: &Url) -> String {
    let base = RefCell::new(url.clone());
    let css = RefCell::new(String::new());
    let settings = RewriteStrSettings::new()
        .append_element_content_handler(element!("base[href]", |el| {
            if let Some(href) = el.get_attribute("href") {
                let joined = base.borrow().join(&href);
                if let Ok(joined) = joined {
                    *base.borrow_mut() = joined;
                }
            }
            if let Some(path) = served_path(base.borrow().as_str()) {
                let _ = el.set_attribute("href", &path);
            }
            Ok(())
        }))
        .append_element_content_handler(element!("[href]", |el| {
            if el.tag_name() != "base" {
                rewrite_attribute(el, "href", &base.borrow());
            }
            Ok(())
        }))
        .append_element_content_handler(element!("[src]", |el| {
            rewrite_attribute(el, "src", &base.borrow());
            Ok(())
        }))
        .append_element_content_handler(element!("[poster]", |el| {
            rewrite_attribute(el, "poster", &base.borrow());
            Ok(())
        }))
        .append_element_content_handler(element!("[action]", |el| {
            rewrite_attribute(el, "action", &base.borrow());
            Ok(())
        }))
        .append_element_content_handler(element!("object[data]", |el| {
            rewrite_attribute(el, "data", &base.borrow());
            Ok(())
        }))
        .append_element_content_handler(element!("[srcset]", |el| {
            if let Some(value) = el.get_attribute("srcset") {
                let _ = el.set_attribute("srcset", &rewrite_srcset(&value, &base.borrow()));
            }
            Ok(())
        }))
        .append_element_content_handler(element!("[imagesrcset]", |el| {
            if let Some(value) = el.get_attribute("imagesrcset") {
                let _ = el.set_attribute("imagesrcset", &rewrite_srcset(&value, &base.borrow()));
            }
            Ok(())
        }))
        .append_element_content_handler(element!("[style]", |el| {
            if let Some(value) = el.get_attribute("style") {
                let _ = el.set_attribute("style", &rewrite_css(&value, &base.borrow()));
            }
            Ok(())
        }))
        .append_element_content_handler(element!("[integrity]", |el| {
            el.remove_attribute("integrity");
            Ok(())
        }))
        .append_element_content_handler(element!("meta[http-equiv]", |el| {
            let equiv = el.get_attribute("http-equiv").unwrap_or_default();
            if equiv.eq_ignore_ascii_case("content-security-policy") {
                el.remove();
            } else if equiv.eq_ignore_ascii_case("refresh") {
                if let Some(content) = el.get_attribute("content") {
                    let lower = content.to_ascii_lowercase();
                    if let Some(at) = lower.find("url=") {
                        let target = content[at + 4..].trim().trim_matches(['"', '\'']);
                        if let Some(rewritten) = rewrite_url(target, &base.borrow()) {
                            let _ = el.set_attribute(
                                "content",
                                &format!("{}url={rewritten}", &content[..at]),
                            );
                        }
                    }
                }
            }
            Ok(())
        }))
        .append_element_content_handler(text!("style", |chunk| {
            css.borrow_mut().push_str(chunk.as_str());
            if chunk.last_in_text_node() {
                let whole = std::mem::take(&mut *css.borrow_mut());
                chunk.replace(&rewrite_css(&whole, &base.borrow()), ContentType::Html);
            } else {
                chunk.remove();
            }
            Ok(())
        }));
    rewrite_str(html, settings).unwrap_or_else(|_| html.to_string())
}

/// The text of an HTML page or a stylesheet: its byte order mark, then
/// the charset its response named, then the one it names itself, then
/// UTF-8.
fn decode_text(body: &[u8], charset: Option<&str>, html: bool) -> String {
    if let Some((encoding, bom)) = encoding_rs::Encoding::for_bom(body) {
        return encoding
            .decode_without_bom_handling(&body[bom..])
            .0
            .into_owned();
    }
    let named = charset
        .map(str::to_string)
        .or_else(|| declared_charset(body, html));
    let encoding = named
        .and_then(|label| encoding_rs::Encoding::for_label(label.trim().as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    encoding.decode_without_bom_handling(body).0.into_owned()
}

/// The charset a page names in its first kilobyte (`<meta charset>` or
/// its `http-equiv` spelling), or a stylesheet in its `@charset` rule.
fn declared_charset(body: &[u8], html: bool) -> Option<String> {
    let head = String::from_utf8_lossy(&body[..body.len().min(1024)]).to_ascii_lowercase();
    let after = if html {
        &head[head.find("charset=")? + 8..]
    } else {
        head.strip_prefix("@charset")?
    };
    let label: String = after
        .trim_start_matches([' ', '"', '\''])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
        .collect();
    (!label.is_empty()).then_some(label)
}

/// A capture as it is served: HTML and CSS rewritten into the archive and
/// given as UTF-8 behind a byte order mark, which a browser's parser takes
/// over any charset the page names; anything else as it was captured.
pub(crate) fn resource(captured: &Captured) -> (String, Vec<u8>) {
    let mime = if captured.mime.is_empty() {
        "application/octet-stream".to_string()
    } else {
        captured.mime.clone()
    };
    let Ok(base) = Url::parse(&captured.url) else {
        return (mime, captured.body.clone());
    };
    let rewritten = match mime.as_str() {
        "text/html" | "application/xhtml+xml" => {
            let text = decode_text(&captured.body, captured.charset.as_deref(), true);
            rewrite_html(&text, &base)
        }
        "text/css" => {
            let text = decode_text(&captured.body, captured.charset.as_deref(), false);
            rewrite_css(&text, &base)
        }
        _ => return (mime, captured.body.clone()),
    };
    let mut body = "\u{feff}".as_bytes().to_vec();
    body.extend_from_slice(rewritten.as_bytes());
    (mime, body)
}

/// An HTML capture's `<title>`, its common entities spelled out.
pub(crate) fn title_of(captured: &Captured) -> Option<String> {
    if captured.mime != "text/html" {
        return None;
    }
    let text = decode_text(&captured.body, captured.charset.as_deref(), true);
    let lower = text.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    let title = text[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    (!title.is_empty()).then_some(title)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/docs/page.html").unwrap()
    }

    #[test]
    fn a_url_is_served_under_its_scheme_and_host() {
        assert_eq!(
            served_path("https://example.com:8443/a/b.css?x=1#top").as_deref(),
            Some("/https/example.com:8443/a/b.css?x=1")
        );
        assert_eq!(
            served_path("cid:frame-1@mhtml.blink").as_deref(),
            Some("/cid/frame-1@mhtml.blink")
        );
        assert_eq!(served_path("data:text/plain,x"), None);
    }

    #[test]
    fn relative_absolute_and_protocol_relative_links_all_lead_into_the_archive() {
        let b = base();
        assert_eq!(
            rewrite_url("style.css", &b).as_deref(),
            Some("/https/example.com/docs/style.css")
        );
        assert_eq!(
            rewrite_url("/img/a.png", &b).as_deref(),
            Some("/https/example.com/img/a.png")
        );
        assert_eq!(
            rewrite_url("//cdn.example.net/x.js", &b).as_deref(),
            Some("/https/cdn.example.net/x.js")
        );
        assert_eq!(
            rewrite_url("http://other.org/p#s", &b).as_deref(),
            Some("/http/other.org/p#s")
        );
        assert_eq!(rewrite_url("#section", &b), None);
        assert_eq!(rewrite_url("data:image/png;base64,AAAA", &b), None);
        assert_eq!(rewrite_url("javascript:void(0)", &b), None);
    }

    #[test]
    fn css_urls_and_imports_are_rewritten_and_the_rest_kept() {
        let css = r#"@import "print.css"; a { background: url( 'bg.png' ) } b { src: url(font.woff2) format("woff2") } c { x: url(data:image/png;base64,AA) }"#;
        assert_eq!(
            rewrite_css(css, &base()),
            r#"@import "/https/example.com/docs/print.css"; a { background: url( '/https/example.com/docs/bg.png' ) } b { src: url(/https/example.com/docs/font.woff2) format("woff2") } c { x: url(data:image/png;base64,AA) }"#
        );
    }

    #[test]
    fn a_page_is_rewritten_through_its_base_and_loses_what_would_refuse_it() {
        let html = r##"<html><head><base href="https://cdn.example.net/site/"><meta http-equiv="Content-Security-Policy" content="default-src 'self'"><link rel="stylesheet" href="a.css" integrity="sha384-x"><style>p { background: url(p.png) }</style></head><body><img src="i.png" srcset="i.png 1x, i@2x.png 2x"><a href="#top">top</a></body></html>"##;
        let out = rewrite_html(html, &base());
        assert!(
            out.contains(r#"<base href="/https/cdn.example.net/site/">"#),
            "{out}"
        );
        assert!(!out.contains("Content-Security-Policy"), "{out}");
        assert!(
            out.contains(r#"href="/https/cdn.example.net/site/a.css""#),
            "{out}"
        );
        assert!(!out.contains("integrity"), "{out}");
        assert!(
            out.contains("url(/https/cdn.example.net/site/p.png)"),
            "{out}"
        );
        assert!(
            out.contains(r#"srcset="/https/cdn.example.net/site/i.png 1x, /https/cdn.example.net/site/i@2x.png 2x""#),
            "{out}"
        );
        assert!(out.contains(r##"href="#top""##), "{out}");
    }

    #[test]
    fn a_page_in_another_charset_is_served_as_utf8() {
        let captured = Captured {
            url: "https://example.com/".into(),
            status: 200,
            mime: "text/html".into(),
            charset: None,
            location: None,
            body: b"<meta charset=\"iso-8859-1\"><p>caf\xe9</p>".to_vec(),
        };
        let (mime, body) = resource(&captured);
        assert_eq!(mime, "text/html");
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with('\u{feff}'));
        assert!(text.contains("café"), "{text}");
    }
}

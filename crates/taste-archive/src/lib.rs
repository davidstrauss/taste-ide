//! What a web archive holds, made into one shape: the pages a reader may
//! open, and every resource by the path it is served at in the IDE's
//! viewer, its HTML and CSS rewritten so their links lead back into the
//! archive rather than out to the web (David, 2026-10-26: "support for
//! viewing HTML and various web archive formats that bundle resources").
//!
//! Three formats, each a published one: MHTML (RFC 2557, what Chrome and
//! Edge save as one file), WARC (ISO 28500, what wget, Webrecorder, and the
//! Internet Archive write), and WACZ (Webrecorder's zip of WARCs with its
//! list of pages). The bytes are someone else's, so this runs in the
//! confined `taste-archive` helper and never in the IDE.
//!
//! A resource captured at `https://example.com/a/b.css?x=1` is served at
//! `/https/example.com/a/b.css?x=1`, and every URL in the archive's pages
//! and stylesheets is rewritten to that form, root-relative, so it resolves
//! against whatever origin the viewer serves the archive from. A part of
//! an MHTML file known only by its `Content-ID` is served at `/cid/<id>`.
//! What a page's scripts reach for at run time is not rewritten, and the
//! viewer's policy keeps it off the network.

mod mhtml;
mod rewrite;
mod warc;
mod zip;

use std::collections::HashMap;

pub use rewrite::served_path;

/// The archive formats read here, by their file names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Mhtml,
    Warc,
    Wacz,
}

impl Format {
    /// The format a file's name says it is: `.mht` or `.mhtml`, `.warc`
    /// or `.warc.gz`, or `.wacz`.
    pub fn of(name: &str) -> Option<Self> {
        let name = name.to_ascii_lowercase();
        if name.ends_with(".mht") || name.ends_with(".mhtml") {
            Some(Self::Mhtml)
        } else if name.ends_with(".warc") || name.ends_with(".warc.gz") {
            Some(Self::Warc)
        } else if name.ends_with(".wacz") {
            Some(Self::Wacz)
        } else {
            None
        }
    }

    /// The word the helper is asked with.
    pub fn name(self) -> &'static str {
        match self {
            Self::Mhtml => "mhtml",
            Self::Warc => "warc",
            Self::Wacz => "wacz",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "mhtml" => Some(Self::Mhtml),
            "warc" => Some(Self::Warc),
            "wacz" => Some(Self::Wacz),
            _ => None,
        }
    }
}

/// A page of the archive a reader may open.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Page {
    /// Where it was captured from.
    pub url: String,
    /// Where it is served.
    pub path: String,
    pub title: Option<String>,
}

/// One thing the archive serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub path: String,
    pub mime: String,
    pub body: Vec<u8>,
}

/// What an archive holds, ready to serve.
#[derive(Debug, Default)]
pub struct Bundle {
    pub pages: Vec<Page>,
    pub resources: Vec<Resource>,
}

/// The most an archive may unpack to: a page and what it needs, not a
/// crawl of a site.
pub const MAX_UNPACKED: usize = 2 * 1024 * 1024 * 1024;

/// A response as a format reader found it, before rewriting.
#[derive(Debug, Clone)]
pub(crate) struct Captured {
    pub url: String,
    pub status: u16,
    /// Its media type, without parameters, lower case.
    pub mime: String,
    pub charset: Option<String>,
    /// Where a redirect sends it.
    pub location: Option<String>,
    pub body: Vec<u8>,
}

/// What a format reader hands on: every capture, and the pages it names,
/// when it names them (MHTML its root, WACZ its page list).
#[derive(Debug, Default)]
pub(crate) struct Read {
    pub captures: Vec<Captured>,
    pub pages: Vec<(String, Option<String>)>,
}

/// Read `bytes` as `format`.
pub fn read(format: Format, bytes: &[u8]) -> Result<Bundle, String> {
    let read = match format {
        Format::Mhtml => mhtml::read(bytes)?,
        Format::Warc => warc::read(bytes)?,
        Format::Wacz => read_wacz(bytes)?,
    };
    Ok(bundle(read))
}

/// A WACZ: its WARCs, and its list of pages.
fn read_wacz(bytes: &[u8]) -> Result<Read, String> {
    let entries = zip::entries(bytes)?;
    let mut read = Read::default();
    let mut warcs = 0;
    for entry in &entries {
        let name = entry.name.as_str();
        if name.starts_with("archive/") && (name.ends_with(".warc") || name.ends_with(".warc.gz")) {
            let unpacked = zip::unpack(bytes, entry)?;
            read.captures.extend(warc::read(&unpacked)?.captures);
            warcs += 1;
        } else if name == "pages/pages.jsonl" {
            let unpacked = zip::unpack(bytes, entry)?;
            read.pages.extend(pages_of_jsonl(&unpacked));
        }
    }
    if warcs == 0 {
        return Err("this WACZ holds no WARC under archive/".into());
    }
    Ok(read)
}

/// A WACZ's `pages.jsonl`: a header line, then a page a line, each with
/// its `url` and maybe its `title`.
fn pages_of_jsonl(bytes: &[u8]) -> Vec<(String, Option<String>)> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|page| {
            let url = page.get("url")?.as_str()?.to_string();
            let title = page
                .get("title")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            Some((url, title))
        })
        .collect()
}

/// The captures made into what is served: one resource a URL, the first
/// good capture of it; a redirect served as what it leads to; HTML and CSS
/// rewritten into the archive.
fn bundle(read: Read) -> Bundle {
    let mut by_url: HashMap<String, Captured> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for captured in read.captures {
        let key = rewrite::key_of(&captured.url);
        match by_url.get(&key) {
            // A later capture of the same URL replaces an earlier one only
            // when the earlier failed and this one did not.
            Some(earlier) if earlier.status < 400 || captured.status >= 400 => {}
            Some(_) => {
                by_url.insert(key, captured);
            }
            None => {
                order.push(key.clone());
                by_url.insert(key, captured);
            }
        }
    }
    // A redirect's target, followed a few hops, by its source's key.
    let resolve = |key: &str| -> String {
        let mut key = key.to_string();
        for _ in 0..5 {
            let Some(captured) = by_url.get(&key) else {
                break;
            };
            let target = match (&captured.location, (300..400).contains(&captured.status)) {
                (Some(location), true) => url::Url::parse(&captured.url)
                    .ok()
                    .and_then(|base| base.join(location).ok())
                    .map(|target| rewrite::key_of(target.as_str())),
                _ => None,
            };
            match target {
                Some(target) if target != key && by_url.contains_key(&target) => key = target,
                _ => break,
            }
        }
        key
    };
    let mut rewritten: HashMap<String, (String, Vec<u8>)> = HashMap::new();
    let mut resources = Vec::new();
    for key in &order {
        let target = resolve(key);
        let Some(captured) = by_url.get(&target) else {
            continue;
        };
        if (300..400).contains(&captured.status) {
            continue;
        }
        let Some(path) = served_path(&by_url[key].url) else {
            continue;
        };
        let (mime, body) = rewritten
            .entry(target.clone())
            .or_insert_with(|| rewrite::resource(captured))
            .clone();
        resources.push(Resource { path, mime, body });
    }
    // The pages: the ones the archive names, else every HTML page it
    // captured whole.
    let named: Vec<(String, Option<String>)> = if read.pages.is_empty() {
        order
            .iter()
            .filter(|key| {
                let captured = &by_url[*key];
                captured.status == 200 && captured.mime == "text/html"
            })
            .map(|key| (by_url[key].url.clone(), None))
            .collect()
    } else {
        read.pages
    };
    let mut pages = Vec::new();
    for (url, title) in named {
        let key = resolve(&rewrite::key_of(&url));
        let Some(captured) = by_url.get(&key) else {
            continue;
        };
        let Some(path) = served_path(&captured.url) else {
            continue;
        };
        if pages.iter().any(|p: &Page| p.path == path) {
            continue;
        }
        let title = title.or_else(|| rewrite::title_of(captured));
        pages.push(Page {
            url: captured.url.clone(),
            path,
            title,
        });
    }
    Bundle { pages, resources }
}

/// The helper's reply, as the IDE reads it: one line of JSON — the pages,
/// and each resource's path, type, and length — and then the resources'
/// bodies, one after another, in that order.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Header {
    pub pages: Vec<Page>,
    pub resources: Vec<Entry>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub path: String,
    pub mime: String,
    pub len: usize,
}

impl Bundle {
    /// The reply's header line.
    pub fn header(&self) -> Header {
        Header {
            pages: self.pages.clone(),
            resources: self
                .resources
                .iter()
                .map(|r| Entry {
                    path: r.path.clone(),
                    mime: r.mime.clone(),
                    len: r.body.len(),
                })
                .collect(),
        }
    }
}

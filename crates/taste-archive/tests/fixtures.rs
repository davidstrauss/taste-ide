//! The archive fixtures (`build-aux/headless/fixtures/archives`, made by
//! `make.sh` there: a WARC warcio recorded from a real server, the WACZ
//! around it, and an MHTML in Chrome's layout), read whole.

use std::path::PathBuf;

use taste_archive::{read, Bundle, Format};

const SITE: &str = "/http/127.0.0.1:8731";

fn fixture(name: &str) -> Bundle {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../build-aux/headless/fixtures/archives")
        .join(name);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    read(
        Format::of(name).expect("a fixture's name says its format"),
        &bytes,
    )
    .unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn text(bundle: &Bundle, path: &str) -> String {
    let resource = bundle
        .resources
        .iter()
        .find(|r| r.path == path)
        .unwrap_or_else(|| {
            panic!(
                "{path} is not served: {:?}",
                bundle.resources.iter().map(|r| &r.path).collect::<Vec<_>>()
            )
        });
    String::from_utf8(resource.body.clone()).expect("rewritten text is UTF-8")
}

/// Every one of the page's own files is served, its stylesheet and its
/// picture by the paths the rewritten page names them by.
fn the_page_leads_back_into_the_archive(bundle: &Bundle) {
    let page = text(bundle, &format!("{SITE}/index.html"));
    assert!(
        page.starts_with('\u{feff}'),
        "served as UTF-8 behind its mark"
    );
    assert!(page.contains("café"), "decoded, not mangled");
    assert!(
        page.contains(&format!(r#"href="{SITE}/style.css""#)),
        "{page}"
    );
    assert!(
        page.contains(&format!(r#"src="{SITE}/logo.svg""#)),
        "{page}"
    );
    assert!(
        page.contains(&format!(r#"href="{SITE}/other.html""#)),
        "{page}"
    );
    // The beacon was never captured: it is rewritten into the archive,
    // where nothing answers it, rather than left pointing at the network.
    assert!(
        page.contains(r#"src="/http/127.0.0.1:8765/beacon.png""#),
        "{page}"
    );
    for file in ["style.css", "logo.svg", "app.js"] {
        assert!(
            bundle
                .resources
                .iter()
                .any(|r| r.path == format!("{SITE}/{file}")),
            "{file} is served"
        );
    }
}

#[test]
fn a_warc_recorded_by_warcio_serves_its_pages() {
    let bundle = fixture("page.warc.gz");
    the_page_leads_back_into_the_archive(&bundle);
    let pages: Vec<_> = bundle.pages.iter().map(|p| p.path.as_str()).collect();
    assert_eq!(
        pages,
        [format!("{SITE}/index.html"), format!("{SITE}/other.html")]
    );
    assert_eq!(bundle.pages[0].title.as_deref(), Some("An HTML preview"));
    let css = bundle
        .resources
        .iter()
        .find(|r| r.path.ends_with("/style.css"))
        .unwrap();
    assert_eq!(css.mime, "text/css");
}

#[test]
fn a_wacz_lists_the_pages_its_page_list_names() {
    let bundle = fixture("page.wacz");
    the_page_leads_back_into_the_archive(&bundle);
    let pages: Vec<_> = bundle
        .pages
        .iter()
        .map(|p| (p.path.as_str(), p.title.as_deref()))
        .collect();
    assert_eq!(
        pages,
        [
            (
                format!("{SITE}/index.html").as_str(),
                Some("An HTML preview")
            ),
            (format!("{SITE}/other.html").as_str(), Some("Another page")),
        ]
    );
}

#[test]
fn an_mhtml_serves_its_root_and_a_frame_known_only_by_its_content_id() {
    let bundle = fixture("page.mhtml");
    the_page_leads_back_into_the_archive(&bundle);
    assert_eq!(bundle.pages.len(), 1);
    assert_eq!(bundle.pages[0].path, format!("{SITE}/index.html"));
    let page = text(&bundle, &format!("{SITE}/index.html"));
    assert!(page.contains(r#"src="/cid/frame-1@mhtml.blink""#), "{page}");
    let frame = text(&bundle, "/cid/frame-1@mhtml.blink");
    assert!(frame.contains("Another page of the checkout."), "{frame}");
}

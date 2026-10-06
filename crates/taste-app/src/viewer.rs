//! **Files the editor shows rather than edits**: images, PDFs, and audio
//! and video — what GNOME's own viewers open (David, 2026-10-05: "Add the
//! ability to view PDFs in the file editor"; "Image support would be
//! welcome, too. Can we embed all the things GNOME can usually open for
//! viewing?").
//!
//! - **Images** are GTK's own: a texture decoded off the main thread by the
//!   loaders the system has (PNG, JPEG, GIF, WebP, TIFF, BMP, ICO, and
//!   AVIF/HEIF where those loaders are installed), drawn scaled to fit.
//! - **Audio and video** are GTK's media widget, on GStreamer.
//! - **PDFs** are pdf.js (`build-aux/vendor-pdfjs.sh`), pinned and served
//!   out of the binary on a scheme of the IDE's own into the WebKit view
//!   the port tabs use: text selection, find, and zoom come with it, and
//!   nothing reaches the network — the page loads only from that scheme.
//!
//! The file's bytes are read through the files service like any other
//! file's, so a viewer works the same on a checkout in a VM. What is
//! text-like — SVG, which is XML — stays in the text editor, where it can
//! be edited.
//!
//! A viewer does not follow its file as it changes. When the file changes
//! on disk, the tab says so in a banner and offers to reload it (David,
//! 2026-10-05: "If I don't automatically see changes as they happen on an
//! open viewer tab …, I should get a banner in the tab asking me to reload
//! to see changes").

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

/// What a viewer shows a file as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    Image,
    Pdf,
    Media,
}

/// The viewer a file name calls for, or `None` for one the text editor
/// keeps: by the name's content type, which needs no read. A type that is
/// text underneath (SVG is XML) is the text editor's.
pub fn kind_for(path: &Path) -> Option<ViewKind> {
    let name = path.file_name()?.to_str()?;
    let (content_type, _) = gtk::gio::functions::content_type_guess(Some(name), None::<&[u8]>);
    if gtk::gio::functions::content_type_is_a(&content_type, "text/plain") {
        return None;
    }
    let mime = gtk::gio::functions::content_type_get_mime_type(&content_type)?;
    let mime = mime.as_str();
    if mime == "application/pdf" {
        Some(ViewKind::Pdf)
    } else if mime.starts_with("image/") {
        Some(ViewKind::Image)
    } else if mime.starts_with("video/") || mime.starts_with("audio/") {
        Some(ViewKind::Media)
    } else {
        None
    }
}

/// The largest file a viewer reads whole. A viewer of a checkout in a VM
/// has it over the files service, in one piece.
pub const MAX_VIEW_BYTES: u64 = 512 * 1024 * 1024;

/// A file read for its viewer, off the main thread: an image decoded
/// there, anything else as its bytes — and what the bytes were, so a later
/// change on disk can be told from an event about the same file.
pub struct Loaded {
    content: Content,
    pub fingerprint: u64,
}

enum Content {
    Image(gtk::gdk::Texture),
    Bytes(glib::Bytes),
}

/// What a file's bytes are, for telling a real change from an event that
/// changed nothing.
pub fn fingerprint(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Read and, for an image, decode `path` for a viewer of `kind`. Blocking:
/// run on the blocking pool.
pub fn load(
    files: &taste_core::files::Files,
    path: &Path,
    kind: ViewKind,
) -> Result<Loaded, String> {
    let size = files.stat(path).map_err(|e| e.to_string())?.size;
    if size > MAX_VIEW_BYTES {
        return Err(format!(
            "{} MB is more than a viewer reads ({} MB)",
            size / (1024 * 1024),
            MAX_VIEW_BYTES / (1024 * 1024)
        ));
    }
    let bytes = glib::Bytes::from_owned(files.read(path).map_err(|e| e.to_string())?);
    let fingerprint = fingerprint(&bytes);
    let content = match kind {
        ViewKind::Image => gtk::gdk::Texture::from_bytes(&bytes)
            .map(Content::Image)
            .map_err(|e| format!("this image could not be decoded: {e}"))?,
        ViewKind::Pdf | ViewKind::Media => Content::Bytes(bytes),
    };
    Ok(Loaded {
        content,
        fingerprint,
    })
}

/// The bar a tab shows when its file changed on disk under it: what
/// happened, Reload to take the disk's version, and — for a tab that can
/// be edited — Keep Mine, which writes the tab's version over the disk's.
/// A banner's look with two answers, which `AdwBanner` (one button) cannot
/// hold (David, 2026-10-05: "… a banner in the tab asking me to reload to
/// see changes (and, for things I can edit, the option overwrite changes
/// on disk with my version)").
///
/// A viewer's bar has a third control instead of Keep Mine: Reload
/// automatically, which makes the tab follow its file, and keeps the bar
/// up for as long as it is on, saying so (David, 2026-10-05: "Give me a
/// toggle on the 'file modified' banner that will cause it to
/// automatically reload on change. Keep the banner present as long as
/// that's enabled"). An editable tab has no such toggle: what it would
/// reload over is unsaved work.
pub struct ChangedBar {
    pub widget: gtk::Revealer,
    title: gtk::Label,
    reload: gtk::Button,
    keep: Option<gtk::Button>,
    auto: Option<gtk::Switch>,
}

impl ChangedBar {
    pub fn new(editable: bool) -> Self {
        let title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.add_css_class("changed-bar");
        row.append(&title);
        let keep = editable.then(|| {
            let keep = gtk::Button::builder()
                .label("Keep Mine")
                .tooltip_text("Write your version over the file on disk")
                .valign(gtk::Align::Center)
                .build();
            row.append(&keep);
            keep
        });
        let auto = (!editable).then(|| {
            let switch = gtk::Switch::builder()
                .valign(gtk::Align::Center)
                .tooltip_text("Reload this tab whenever the file changes")
                .build();
            let label = gtk::Label::builder()
                .label("Reload automatically")
                .mnemonic_widget(&switch)
                .build();
            switch.update_relation(&[gtk::accessible::Relation::LabelledBy(&[label.upcast_ref()])]);
            row.append(&label);
            row.append(&switch);
            switch
        });
        let reload = gtk::Button::builder()
            .label("Reload")
            .tooltip_text("Take the version on disk")
            .css_classes(["suggested-action"])
            .valign(gtk::Align::Center)
            .build();
        row.append(&reload);
        let widget = gtk::Revealer::builder()
            .child(&row)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(false)
            .build();
        Self {
            widget,
            title,
            reload,
            keep,
            auto,
        }
    }

    /// Whether the Reload button is offered: not while the tab reloads by
    /// itself and has nothing waiting.
    pub fn set_reload_visible(&self, visible: bool) {
        self.reload.set_visible(visible);
    }

    pub fn connect_auto(&self, f: impl Fn(bool) + 'static) {
        if let Some(auto) = &self.auto {
            auto.connect_active_notify(move |switch| f(switch.is_active()));
        }
    }

    pub fn set_title(&self, title: &str) {
        self.title.set_label(title);
    }

    pub fn set_revealed(&self, revealed: bool) {
        self.widget.set_reveal_child(revealed);
    }

    pub fn connect_reload(&self, f: impl Fn() + 'static) {
        self.reload.connect_clicked(move |_| f());
    }

    pub fn connect_keep(&self, f: impl Fn() + 'static) {
        if let Some(keep) = &self.keep {
            keep.connect_clicked(move |_| f());
        }
    }
}

/// One file shown in a viewer tab: the content, and above it the bar that
/// says when the file has changed on disk since.
pub struct ViewerPage {
    pub widget: gtk::Box,
    pub kind: ViewKind,
    pub path: PathBuf,
    banner: ChangedBar,
    content: gtk::Box,
    /// What a click on the banner's Reload does: the editor's own read and
    /// redraw, handed in when the tab is made.
    reload: RefCell<Option<Rc<dyn Fn()>>>,
    /// This page's PDF, while it has one, on the viewer scheme.
    pdf_id: Cell<Option<u64>>,
    /// What the shown bytes were ([`fingerprint`]).
    shown: Cell<u64>,
    /// The file changed on disk and the tab still shows what it was.
    pending: Cell<bool>,
    /// The bar's Reload automatically is on.
    auto: Cell<bool>,
    /// The PDF's view, while it has one: a reload replaces the document in
    /// it rather than the view, so the scroll and the zoom hold.
    pdf_view: RefCell<Option<webkit6::WebView>>,
    /// The PDF's text, a string a page, once read ([`Self::read_text`]),
    /// for the universal find (David, 2026-10-06: "Universal find should
    /// work on open PDFs. You should even OCR if necessary"). A page drawn
    /// as a picture reads as empty until OCR has read it.
    text: RefCell<Option<Vec<String>>>,
    /// Which reading of the text is current: a reload starts another, and
    /// an older one finishing late is not taken.
    text_generation: Cell<u64>,
    /// OCR is under way, or done, for the text as it stands.
    ocr_started: Cell<bool>,
    /// Pages OCR has yet to read; zero when none are waiting.
    ocr_pending: Cell<usize>,
    /// Where the file is read from, for OCR, which runs beside it.
    source: RefCell<Option<(taste_core::files::Files, PathBuf)>>,
    /// Told when the text changes — read, or a page read by OCR.
    on_text: RefCell<Option<Rc<dyn Fn()>>>,
    this: RefCell<std::rc::Weak<Self>>,
}

impl ViewerPage {
    pub fn new(path: &Path, kind: ViewKind, loaded: Loaded) -> Rc<Self> {
        let banner = ChangedBar::new(false);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_vexpand(true);
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&banner.widget);
        widget.append(&content);
        let page = Rc::new(Self {
            widget,
            kind,
            path: path.to_path_buf(),
            banner,
            content,
            reload: RefCell::new(None),
            pdf_id: Cell::new(None),
            shown: Cell::new(loaded.fingerprint),
            pending: Cell::new(false),
            auto: Cell::new(false),
            pdf_view: RefCell::new(None),
            text: RefCell::new(None),
            text_generation: Cell::new(0),
            ocr_started: Cell::new(false),
            ocr_pending: Cell::new(0),
            source: RefCell::new(None),
            on_text: RefCell::new(None),
            this: RefCell::new(std::rc::Weak::new()),
        });
        *page.this.borrow_mut() = Rc::downgrade(&page);
        {
            let weak = Rc::downgrade(&page);
            page.banner.connect_auto(move |on| {
                let Some(page) = weak.upgrade() else { return };
                page.auto.set(on);
                // Turned on with a change waiting: take it now.
                if on && page.pending.get() {
                    page.reload_now();
                }
                page.sync_bar();
            });
        }
        {
            let weak = Rc::downgrade(&page);
            page.banner.connect_reload(move || {
                if let Some(page) = weak.upgrade() {
                    page.reload_now();
                }
            });
        }
        page.show(loaded);
        page
    }

    /// The icon the editor's mode button wears for this tab.
    pub fn icon(&self) -> &'static str {
        match self.kind {
            ViewKind::Image => "image-x-generic-symbolic",
            ViewKind::Pdf => "x-office-document-symbolic",
            ViewKind::Media => "multimedia-player-symbolic",
        }
    }

    /// What Reload does; the editor's read, which ends in [`Self::show`].
    pub fn set_reload(&self, reload: Rc<dyn Fn()>) {
        *self.reload.borrow_mut() = Some(reload);
    }

    /// TASTE_PROBE_CHANGED only: pose the bar — `pending`, a change
    /// waiting; `auto`, Reload automatically on.
    #[doc(hidden)]
    pub fn pose_for_probe(&self, variant: &str) {
        match variant {
            // Scrolled down, then reloaded: the scroll it comes back at is
            // printed, for checking that a reload holds it.
            "reload" => {
                let Some(view) = self.pdf_view.borrow().clone() else {
                    return;
                };
                let js = |script: &'static str, view: &webkit6::WebView| {
                    webkit6::prelude::WebViewExt::evaluate_javascript(
                        view,
                        script,
                        None,
                        None,
                        None::<&gtk::gio::Cancellable>,
                        |result| {
                            if let Ok(value) = result {
                                eprintln!("probe viewer scroll: {}", value.to_str());
                            }
                        },
                    );
                };
                let reload = self.reload.borrow().clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(1500), {
                    let view = view.clone();
                    move || {
                        js("'scrolled to ' + window.tasteScroll(1800)", &view);
                        glib::timeout_add_local_once(
                            std::time::Duration::from_millis(400),
                            move || {
                                if let Some(reload) = reload {
                                    reload();
                                }
                                glib::timeout_add_local_once(
                                    std::time::Duration::from_millis(1500),
                                    move || {
                                        js("'after reload ' + window.tasteScroll()", &view);
                                    },
                                );
                            },
                        );
                    }
                });
            }
            // `keys`: Page Down pressed in the page, the scroll printed
            // before and after.
            "keys" => {
                let Some(view) = self.pdf_view.borrow().clone() else {
                    return;
                };
                glib::timeout_add_local_once(std::time::Duration::from_millis(2000), move || {
                    webkit6::prelude::WebViewExt::evaluate_javascript(
                        &view,
                        "const before = window.tasteScroll(); \
                         document.dispatchEvent(new KeyboardEvent('keydown', {key: 'PageDown', bubbles: true})); \
                         'page down: ' + before + ' -> ' + window.tasteScroll()",
                        None,
                        None,
                        None::<&gtk::gio::Cancellable>,
                        |result| {
                            if let Ok(value) = result {
                                eprintln!("probe viewer keys: {}", value.to_str());
                            }
                        },
                    );
                });
            }
            // `page:N`: turned to page N once the document is up.
            page if page.starts_with("page:") => {
                let Some(view) = self.pdf_view.borrow().clone() else {
                    return;
                };
                let script = format!("window.tastePage({})", &page[5..]);
                glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
                    webkit6::prelude::WebViewExt::evaluate_javascript(
                        &view,
                        &script,
                        None,
                        None,
                        None::<&gtk::gio::Cancellable>,
                        |_| {},
                    );
                });
            }
            "auto" => {
                if let Some(auto) = &self.banner.auto {
                    auto.set_active(true);
                }
            }
            _ => {
                self.file_changed(self.shown.get().wrapping_add(1));
            }
        }
    }

    fn reload_now(&self) {
        let reload = self.reload.borrow().clone();
        if let Some(reload) = reload {
            reload();
        }
    }

    /// The bar as the tab stands: up for as long as it reloads by itself,
    /// saying so; up with Reload while a change waits; away otherwise.
    fn sync_bar(&self) {
        let (auto, pending) = (self.auto.get(), self.pending.get());
        self.banner.set_title(if auto {
            "Following this file as it changes"
        } else {
            "This file has changed on disk"
        });
        self.banner.set_reload_visible(!auto);
        self.banner.set_revealed(auto || pending);
    }

    /// The file on disk now has bytes of `fingerprint`: when they are not
    /// the ones shown, reload — by itself, when the bar's toggle is on — or
    /// say so and offer it. An event about the file that changed nothing —
    /// a touch, its creation seen late — does nothing. Says whether the
    /// file had changed.
    pub fn file_changed(&self, fingerprint: u64) -> bool {
        if fingerprint == self.shown.get() {
            return false;
        }
        if self.auto.get() {
            self.reload_now();
        } else {
            self.pending.set(true);
            self.sync_bar();
        }
        true
    }

    /// Show `loaded`, replacing whatever was shown; nothing waits now. A
    /// PDF already on screen takes its new document in place, holding its
    /// scroll and its zoom (David, 2026-10-05: "I'd like to hold the scroll
    /// position (as much as possible) on reload").
    pub fn show(&self, loaded: Loaded) {
        self.pending.set(false);
        self.sync_bar();
        let in_place = self.pdf_view.borrow().clone().zip(self.pdf_id.get());
        if let (Some((view, id)), Content::Bytes(bytes)) = (in_place, &loaded.content) {
            pdf_documents().borrow_mut().insert(id, bytes.clone());
            self.shown.set(loaded.fingerprint);
            webkit6::prelude::WebViewExt::evaluate_javascript(
                &view,
                "window.tasteReload && window.tasteReload()",
                None,
                None,
                None::<&gtk::gio::Cancellable>,
                |_| {},
            );
            // The new document's text, read once its reload has settled.
            self.read_text();
            return;
        }
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        if let Some(id) = self.pdf_id.take() {
            pdf_documents().borrow_mut().remove(&id);
        }
        self.shown.set(loaded.fingerprint);
        let shown: gtk::Widget = match (self.kind, loaded.content) {
            (ViewKind::Image, Content::Image(texture)) => image_view(&texture),
            (ViewKind::Media, Content::Bytes(bytes)) => media_view(&bytes),
            (ViewKind::Pdf, Content::Bytes(bytes)) => {
                let id = next_pdf_id();
                pdf_documents().borrow_mut().insert(id, bytes);
                self.pdf_id.set(Some(id));
                let view = pdf_view(id);
                *self.pdf_view.borrow_mut() = Some(view.clone());
                // Its text, once the page that draws it has loaded.
                let weak = self.this.borrow().clone();
                webkit6::prelude::WebViewExt::connect_load_changed(&view, move |_, event| {
                    if event == webkit6::LoadEvent::Finished {
                        if let Some(page) = weak.upgrade() {
                            page.read_text();
                        }
                    }
                });
                view.upcast()
            }
            _ => gtk::Label::new(Some("This file could not be shown.")).upcast(),
        };
        shown.set_vexpand(true);
        self.content.append(&shown);
    }

    /// Take the keyboard, as the tab comes to the front: a PDF pages with
    /// Page Up and Down, which go to whatever has the focus.
    pub fn focus(&self) {
        if let Some(view) = self.pdf_view.borrow().as_ref() {
            view.grab_focus();
        }
    }

    /// Where the file is read from: OCR runs beside it.
    pub fn set_source(&self, files: taste_core::files::Files, path: PathBuf) {
        *self.source.borrow_mut() = Some((files, path));
    }

    /// Who is told when the text changes.
    pub fn set_on_text(&self, hook: Rc<dyn Fn()>) {
        *self.on_text.borrow_mut() = Some(hook);
    }

    /// The PDF's text, a string a page, once read; `None` before, and for
    /// anything but a PDF.
    pub fn text_pages(&self) -> Option<Vec<String>> {
        self.text.borrow().clone()
    }

    /// Whether its text is still coming: being read, or pages being read
    /// by OCR.
    pub fn text_pending(&self) -> bool {
        self.pdf_view.borrow().is_some()
            && (self.text.borrow().is_none() || self.ocr_pending.get() > 0)
    }

    fn notify_text(&self) {
        let hook = self.on_text.borrow().clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Read the PDF's text from the page that draws it — pdf.js's own
    /// reading, a string a page — and say so when it lands.
    fn read_text(&self) {
        let Some(view) = self.pdf_view.borrow().clone() else {
            return;
        };
        let generation = self.text_generation.get() + 1;
        self.text_generation.set(generation);
        *self.text.borrow_mut() = None;
        self.ocr_started.set(false);
        self.ocr_pending.set(0);
        let weak = self.this.borrow().clone();
        glib::spawn_future_local(async move {
            let read = webkit6::prelude::WebViewExt::call_async_javascript_function_future(
                &view,
                "await window.tasteReady; return await window.tasteText();",
                None,
                None,
                None,
            )
            .await;
            let Some(page) = weak.upgrade() else { return };
            if page.text_generation.get() != generation {
                return;
            }
            let pages = read
                .ok()
                .map(|value| value.to_str().to_string())
                .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok());
            if let Some(pages) = pages {
                *page.text.borrow_mut() = Some(pages);
                page.notify_text();
            }
        });
    }

    /// Read the pages with no text — scans, slides that are pictures — by
    /// OCR, beside the file ([`OCR_SCRIPT`]), one at a time, each told as
    /// it lands. Once for the text as it stands; a page OCR cannot read
    /// stays empty.
    pub fn read_textless_pages(&self) {
        if self.ocr_started.get() {
            return;
        }
        let Some(pages) = self.text.borrow().clone() else {
            return;
        };
        let Some((files, path)) = self.source.borrow().clone() else {
            return;
        };
        let textless: Vec<usize> = pages
            .iter()
            .enumerate()
            .filter(|(_, text)| text.trim().is_empty())
            .map(|(index, _)| index)
            .collect();
        self.ocr_started.set(true);
        if textless.is_empty() {
            return;
        }
        self.ocr_pending.set(textless.len());
        let generation = self.text_generation.get();
        let weak = self.this.borrow().clone();
        glib::spawn_future_local(async move {
            for index in textless {
                let (files, path) = (files.clone(), path.clone());
                let read = crate::runtime::runtime()
                    .spawn_blocking(move || ocr_page(&files, &path, index + 1))
                    .await;
                let Some(page) = weak.upgrade() else { return };
                if page.text_generation.get() != generation {
                    return;
                }
                page.ocr_pending
                    .set(page.ocr_pending.get().saturating_sub(1));
                match read {
                    Ok(Ok(text)) => {
                        if let Some(pages) = page.text.borrow_mut().as_mut() {
                            if let Some(slot) = pages.get_mut(index) {
                                *slot = text;
                            }
                        }
                        page.notify_text();
                    }
                    Ok(Err(why)) => {
                        tracing::info!("no OCR for {}: {why}", path_name(&page.path));
                        page.ocr_pending.set(0);
                        page.notify_text();
                        return;
                    }
                    Err(_) => return,
                }
            }
        });
    }

    /// A hit the find's listing chose: its page, with the query lit on it.
    pub fn find(&self, query: &taste_core::search::Query, page: usize) {
        let Some(view) = self.pdf_view.borrow().clone() else {
            return;
        };
        let script = format!(
            "window.tasteFind && window.tasteFind({}, {}, {page})",
            serde_json::to_string(query.text.trim()).unwrap_or_else(|_| "\"\"".into()),
            query.case_sensitive()
        );
        webkit6::prelude::WebViewExt::evaluate_javascript(
            &view,
            &script,
            None,
            None,
            None::<&gtk::gio::Cancellable>,
            |_| {},
        );
    }

    /// Say why the file could not be shown, in place of it.
    pub fn show_error(&self, why: &str) {
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        // The view is gone with it: the next reload starts a new one.
        self.pdf_view.borrow_mut().take();
        if let Some(id) = self.pdf_id.take() {
            pdf_documents().borrow_mut().remove(&id);
        }
        let status = adw::StatusPage::builder()
            .icon_name("dialog-warning-symbolic")
            .title("Cannot show this file")
            .description(glib::markup_escape_text(why).as_str())
            .vexpand(true)
            .build();
        self.content.append(&status);
    }
}

impl Drop for ViewerPage {
    fn drop(&mut self) {
        if let Some(id) = self.pdf_id.take() {
            pdf_documents().borrow_mut().remove(&id);
        }
    }
}

/// An image, scaled down to fit and never up, centred on the pane, with
/// its size under it.
fn image_view(texture: &gtk::gdk::Texture) -> gtk::Widget {
    let picture = gtk::Picture::for_paintable(texture);
    picture.set_content_fit(gtk::ContentFit::ScaleDown);
    picture.set_can_shrink(true);
    picture.set_vexpand(true);
    picture.set_hexpand(true);
    let size = gtk::Label::builder()
        .label(format!("{} × {}", texture.width(), texture.height()))
        .css_classes(["caption", "dim-label"])
        .margin_top(6)
        .margin_bottom(6)
        .build();
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.set_margin_top(12);
    column.set_margin_start(12);
    column.set_margin_end(12);
    column.append(&picture);
    column.append(&size);
    column.upcast()
}

/// Audio or video, with GTK's own controls, from the bytes read.
fn media_view(bytes: &glib::Bytes) -> gtk::Widget {
    let stream = gtk::gio::MemoryInputStream::from_bytes(bytes);
    let media = gtk::MediaFile::for_input_stream(&stream);
    let video = gtk::Video::builder()
        .media_stream(&media)
        .autoplay(false)
        .vexpand(true)
        .hexpand(true)
        .build();
    video.upcast()
}

// --- PDFs ----------------------------------------------------------------

/// Run beside the PDF, with its path and a page number: the page drawn at
/// 200 dpi and read by Tesseract, its text on stdout. Exit 3 when the
/// files service has no OCR to run.
const OCR_SCRIPT: &str = r#"command -v tesseract >/dev/null 2>&1 || exit 3
pdftoppm -r 200 -f "$2" -l "$2" -singlefile -png "$1" | tesseract stdin stdout 2>/dev/null
"#;

/// One page of the PDF at `path`, read by OCR where the file is.
fn ocr_page(files: &taste_core::files::Files, path: &Path, page: usize) -> Result<String, String> {
    let cwd = path.parent().unwrap_or(path);
    let out = files
        .exec(
            cwd,
            &[
                "sh".into(),
                "-c".into(),
                OCR_SCRIPT.into(),
                "taste-ocr".into(),
                path.display().to_string(),
                page.to_string(),
            ],
        )
        .map_err(|e| e.to_string())?;
    match out.status {
        0 => Ok(out.stdout_utf8()),
        3 => Err("the files service has no OCR yet; it does once the IDE restarts".into()),
        _ => Err(out.stderr_utf8().trim().to_string()),
    }
}

fn path_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The scheme the PDF viewer's page, its pdf.js, and the document itself
/// are served on: `taste-pdf://<id>/…`, one id per open PDF.
const PDF_SCHEME: &str = "taste-pdf";

const PDF_MIN_MJS: &[u8] = include_bytes!("../vendor/pdfjs/pdf.min.mjs");
const PDF_WORKER_MJS: &[u8] = include_bytes!("../vendor/pdfjs/pdf.worker.min.mjs");
const PDF_VIEWER_MJS: &[u8] = include_bytes!("../vendor/pdfjs/pdf_viewer.mjs");
const PDF_VIEWER_CSS: &[u8] = include_bytes!("../vendor/pdfjs/pdf_viewer.css");

/// The page that draws the PDF: pdf.js's viewer components — the pages
/// in one scrolling column, their text layer for selection and find — with
/// the document fitted to the width, Ctrl and the wheel to zoom, and the
/// pane's own background behind the pages. PDF scripting is never on: the
/// sandbox that runs a PDF's JavaScript is not served, and `eval` is off.
///
/// A reload never shows the document coming apart (David, 2026-10-05:
/// "Make PDF reloads less wonky"). The page keeps two panes: the one on
/// screen, and one behind it the new document is drawn into — at the zoom
/// the old one was at, scrolled to the same place on the same page, in
/// the PDF's own coordinates rather than in pixels, so a page above that
/// grew or shrank does not move it — and they change places once the
/// page in view has been drawn, or after a second and a half whatever has
/// been. A newer reload takes over from one still drawing, and a file that
/// does not parse — a PDF caught half-written by the build making it —
/// leaves the one on screen as it is: the write that finishes it is a
/// change of its own.
const PDF_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8">
<meta name="color-scheme" content="light dark">
<link rel="stylesheet" href="pdf_viewer.css">
<style>
html, body { margin: 0; height: 100%; background: transparent; }
.pane { position: absolute; inset: 0; overflow: auto; }
.pane.behind { visibility: hidden; z-index: -1; }
.pdfViewer .page { border: none; margin: 12px auto;
  box-shadow: 0 1px 3px rgba(0,0,0,.25); }
#failed { font: 15px/1.5 system-ui, sans-serif; margin: 15vh auto; max-width: 32em;
  padding: 0 24px; opacity: .8; }
</style></head>
<body>
<script>
// Ready once the first document is up: what the IDE asks of the page —
// its text, a find — waits on this, since the module below loads first.
window.tasteReady = new Promise((resolve) => { window.tasteReadyResolve = resolve; });
</script>
<div id="a" class="pane"><div class="pdfViewer"></div></div>
<div id="b" class="pane behind"><div class="pdfViewer"></div></div>
<script type="module">
try {
  const pdfjsLib = await import("./pdf.min.mjs");
  globalThis.pdfjsLib = pdfjsLib;
  pdfjsLib.GlobalWorkerOptions.workerSrc = "./pdf.worker.min.mjs";
  const { EventBus, PDFViewer, PDFLinkService, PDFFindController } =
    await import("./pdf_viewer.mjs");
  // Images are decoded and drawn by pdf.js itself, never handed to the
  // browser's decoder or to an offscreen canvas: on a WebKit drawing with
  // the GPU, images of one pixel size came out as one another — four
  // photographs of the same size as two, each twice (2026-10-05).
  const open = (url) => pdfjsLib.getDocument({
    url, isEvalSupported: false, enableXfa: false,
    isOffscreenCanvasSupported: false, isImageDecoderSupported: false,
  }).promise;
  const pane = (container) => {
    const eventBus = new EventBus();
    const linkService = new PDFLinkService({ eventBus });
    const findController = new PDFFindController({ eventBus, linkService });
    const viewer = new PDFViewer({ container, eventBus, linkService, findController });
    linkService.setViewer(viewer);
    const it = { container, eventBus, linkService, viewer, findController, doc: null, location: null };
    eventBus.on("updateviewarea", (event) => { it.location = event.location; });
    return it;
  };
  const show = (it, doc) => {
    it.doc = doc;
    it.viewer.setDocument(doc);
    it.linkService.setDocument(doc, null);
  };
  let shown = pane(document.getElementById("a"));
  let behind = pane(document.getElementById("b"));
  shown.eventBus.on("pagesinit", () => {
    shown.viewer.currentScaleValue = "page-width";
  }, { once: true });
  show(shown, await open("document.pdf"));

  let generation = 0;
  // The reload under way, if one is: the text is read from the document
  // that comes out of it, not the one going.
  let settled = Promise.resolve();
  window.tasteReload = () => {
    settled = reload();
    return settled;
  };
  // Each page's text, as pdf.js reads it from the document — the
  // universal find's for a PDF (viewer.rs, `ViewerPage::read_text`). A
  // page drawn as a picture has none; the IDE reads those by OCR.
  window.tasteText = async () => {
    await settled;
    const doc = shown.doc, pages = [];
    for (let n = 1; n <= doc.numPages; n++) {
      const content = await (await doc.getPage(n)).getTextContent();
      pages.push(content.items.map((item) => (item.str || "") + (item.hasEOL ? "\n" : "")).join(""));
    }
    return JSON.stringify(pages);
  };
  // A hit chosen in the find's listing: its page, and the query lit on it
  // by pdf.js's own find.
  window.tasteFind = (query, caseSensitive, page) => {
    if (page) shown.viewer.currentPageNumber = page;
    shown.eventBus.dispatch("find", {
      source: null, type: "", query, caseSensitive, entireWord: false,
      highlightAll: true, findPrevious: false, matchDiacritics: false,
    });
  };
  const reload = async () => {
    const mine = ++generation;
    let next;
    try {
      next = await open("document.pdf?v=" + Date.now());
    } catch (error) {
      return;
    }
    if (mine !== generation) { next.destroy(); return; }
    const into = behind;
    const scale = shown.viewer.currentScaleValue;
    // At the very top it stays there: a place in the PDF's coordinates is
    // a page's edge, which leaves out the margin above the first page.
    const at = shown.container.scrollTop === 0 ? null : shown.location;
    await new Promise((resolve) => {
      let page = 1, done = false;
      const finish = () => {
        if (done) return;
        done = true;
        into.eventBus.off("pagerendered", drawn);
        resolve();
      };
      const drawn = (event) => { if (event.pageNumber === page) finish(); };
      into.eventBus.on("pagesinit", () => {
        into.viewer.currentScaleValue = scale;
        if (at) {
          page = Math.min(at.pageNumber, next.numPages);
          into.viewer.scrollPageIntoView(page === at.pageNumber
            ? { pageNumber: page, destArray: [null, { name: "XYZ" }, at.left, at.top, null],
                allowNegativeOffset: true }
            : { pageNumber: page });
        }
      }, { once: true });
      into.eventBus.on("pagerendered", drawn);
      setTimeout(finish, 1500);
      show(into, next);
    });
    if (mine !== generation) return;
    const from = shown;
    into.container.classList.remove("behind");
    from.container.classList.add("behind");
    shown = into;
    behind = from;
    const old = from.doc;
    from.doc = null;
    from.viewer.setDocument(null);
    from.linkService.setDocument(null, null);
    if (old) old.destroy();
  };
  // For the probe: a page to turn to.
  window.tastePage = (n) => { shown.viewer.currentPageNumber = n; return n; };
  // For the probe: the scroll of the pane on screen, and its page.
  window.tasteScroll = (top) => {
    if (top !== undefined) shown.container.scrollTop = top;
    return shown.container.scrollTop + " (page " + shown.viewer.currentPageNumber + ")";
  };
  window.tasteReadyResolve();
  // The keys a reader pages with. The document scrolls inside a pane, not
  // the page, so the browser's own Page Up and Down — which scroll the
  // page — moved nothing (David, 2026-10-06: "pgup/pgdown should also work
  // in pdfs"). A screenful less a line, so a line stays in view across
  // the turn; Space as Page Down, Shift+Space as Page Up; Home and End.
  document.addEventListener("keydown", (event) => {
    if (event.ctrlKey || event.altKey || event.metaKey) return;
    const pane = shown.container, line = 40;
    const screen = Math.max(pane.clientHeight - line, line);
    const by = {
      PageDown: screen, PageUp: -screen, ArrowDown: line, ArrowUp: -line,
      " ": event.shiftKey ? -screen : screen,
    }[event.key];
    if (by !== undefined) {
      pane.scrollBy({ top: by });
    } else if (event.key === "Home") {
      pane.scrollTop = 0;
    } else if (event.key === "End") {
      pane.scrollTop = pane.scrollHeight;
    } else {
      return;
    }
    event.preventDefault();
  });
  for (const container of document.querySelectorAll(".pane")) {
    container.addEventListener("wheel", (event) => {
      if (!event.ctrlKey) return;
      event.preventDefault();
      shown.viewer.currentScale *= event.deltaY < 0 ? 1.1 : 1 / 1.1;
    }, { passive: false });
  }
  new ResizeObserver(() => {
    for (const it of [shown, behind]) {
      if (it.doc && it.viewer.currentScaleValue === "page-width") {
        it.viewer.currentScaleValue = "page-width";
      }
    }
  }).observe(document.body);
} catch (error) {
  document.body.innerHTML = "";
  const p = document.createElement("p");
  p.id = "failed";
  p.textContent = "This PDF could not be shown: " + error;
  document.body.append(p);
}
</script></body></html>
"#;

thread_local! {
    static PDF_DOCUMENTS: Rc<RefCell<HashMap<u64, glib::Bytes>>> = Rc::default();
    static NEXT_PDF: Cell<u64> = const { Cell::new(1) };
}

fn pdf_documents() -> Rc<RefCell<HashMap<u64, glib::Bytes>>> {
    PDF_DOCUMENTS.with(Rc::clone)
}

fn next_pdf_id() -> u64 {
    NEXT_PDF.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    })
}

/// The answer the viewer scheme gives for `uri`: the page, a pdf.js file,
/// or the document of the PDF the host names; `None` for anything else.
fn pdf_resource(uri: &str) -> Option<(glib::Bytes, &'static str)> {
    let rest = uri.strip_prefix(&format!("{PDF_SCHEME}://"))?;
    let (id, file) = rest.split_once('/').unwrap_or((rest, ""));
    let file = file.split(['?', '#']).next().unwrap_or("");
    let fixed = |bytes: &'static [u8], mime| Some((glib::Bytes::from_static(bytes), mime));
    match file {
        "" | "index.html" => fixed(PDF_PAGE.as_bytes(), "text/html"),
        "pdf.min.mjs" => fixed(PDF_MIN_MJS, "text/javascript"),
        "pdf.worker.min.mjs" => fixed(PDF_WORKER_MJS, "text/javascript"),
        "pdf_viewer.mjs" => fixed(PDF_VIEWER_MJS, "text/javascript"),
        "pdf_viewer.css" => fixed(PDF_VIEWER_CSS, "text/css"),
        "document.pdf" => {
            let id: u64 = id.parse().ok()?;
            let bytes = pdf_documents().borrow().get(&id).cloned()?;
            Some((bytes, "application/pdf"))
        }
        _ => None,
    }
}

/// Register the viewer scheme on the default web context, once: secure and
/// CORS-enabled, which is what lets the page import pdf.js as modules.
fn register_pdf_scheme() {
    thread_local! {
        static REGISTERED: Cell<bool> = const { Cell::new(false) };
    }
    if REGISTERED.with(|done| done.replace(true)) {
        return;
    }
    let Some(context) = webkit6::WebContext::default() else {
        return;
    };
    if let Some(security) = context.security_manager() {
        security.register_uri_scheme_as_secure(PDF_SCHEME);
        security.register_uri_scheme_as_cors_enabled(PDF_SCHEME);
    }
    context.register_uri_scheme(PDF_SCHEME, |request| {
        let uri = request.uri().map(|u| u.to_string()).unwrap_or_default();
        match pdf_resource(&uri) {
            Some((bytes, mime)) => {
                let stream = gtk::gio::MemoryInputStream::from_bytes(&bytes);
                request.finish(&stream, bytes.len() as i64, Some(mime));
            }
            None => {
                let mut error = glib::Error::new(
                    gtk::gio::IOErrorEnum::NotFound,
                    &format!("{uri} is not part of the PDF viewer"),
                );
                request.finish_error(&mut error);
            }
        }
    });
}

/// The PDF of `id`, in a WebKit view that loads nothing but the viewer
/// scheme: navigation anywhere else is refused, and a link the user
/// clicks opens in their browser instead.
fn pdf_view(id: u64) -> webkit6::WebView {
    register_pdf_scheme();
    let session = webkit6::NetworkSession::new_ephemeral();
    let view = webkit6::WebView::builder()
        .network_session(&session)
        .vexpand(true)
        .hexpand(true)
        .build();
    webkit6::prelude::WebViewExt::set_background_color(
        &view,
        &gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0),
    );
    // The page's canvases drawn on the CPU: with WebKit's GPU canvas, a
    // PDF's images of one pixel size were drawn as one another (David,
    // 2026-10-05: four photographs on a slide came out as two, each
    // twice). Set by name, where this WebKit has it (2.46 and later).
    if let Some(settings) = webkit6::prelude::WebViewExt::settings(&view) {
        if settings
            .find_property("enable-2d-canvas-acceleration")
            .is_some()
        {
            settings.set_property("enable-2d-canvas-acceleration", false);
        }
    }
    webkit6::prelude::WebViewExt::connect_decide_policy(&view, |view, decision, kind| {
        use webkit6::prelude::PolicyDecisionExt;
        let navigation = match kind {
            webkit6::PolicyDecisionType::NavigationAction
            | webkit6::PolicyDecisionType::NewWindowAction => decision
                .downcast_ref::<webkit6::NavigationPolicyDecision>()
                .and_then(|d| d.navigation_action())
                .and_then(|action| action.request())
                .and_then(|request| request.uri()),
            _ => return false,
        };
        let Some(uri) = navigation else {
            return false;
        };
        if uri.starts_with(&format!("{PDF_SCHEME}://")) {
            return false;
        }
        // Somewhere else: the user's browser has it, if it is a page at
        // all; this view goes nowhere.
        decision.ignore();
        if uri.starts_with("https://") || uri.starts_with("http://") || uri.starts_with("mailto:") {
            let window = view.root().and_downcast::<gtk::Window>();
            gtk::UriLauncher::new(&uri).launch(
                window.as_ref(),
                None::<&gtk::gio::Cancellable>,
                |_| {},
            );
        }
        true
    });
    webkit6::prelude::WebViewExt::load_uri(&view, &format!("{PDF_SCHEME}://{id}/index.html"));
    view
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bar's three states: away with nothing changed; up with Reload
    /// when a change waits; and, with Reload automatically on, up for as
    /// long as it is on, taking each change by itself — and away again
    /// when it is turned off with nothing waiting.
    #[test]
    fn the_bar_reloads_by_itself_while_its_toggle_is_on() {
        crate::gtk_test::on_gtk_thread("viewer bar: no display — skipped", || {
            let loaded = |fingerprint| Loaded {
                content: Content::Bytes(glib::Bytes::from_static(b"")),
                fingerprint,
            };
            let page = ViewerPage::new(Path::new("/x/talk.mp3"), ViewKind::Media, loaded(1));
            let reloads = Rc::new(Cell::new(0));
            {
                let reloads = reloads.clone();
                page.set_reload(Rc::new(move || reloads.set(reloads.get() + 1)));
            }
            let revealed = || page.banner.widget.reveals_child();
            assert!(!revealed());
            page.file_changed(1);
            assert!(!revealed(), "the same bytes are no change");
            page.file_changed(2);
            assert!(revealed());
            assert_eq!(reloads.get(), 0);
            // On, with a change waiting: it is taken at once, and the bar
            // stays up after the reload lands.
            page.banner.auto.as_ref().unwrap().set_active(true);
            assert_eq!(reloads.get(), 1);
            page.show(loaded(2));
            assert!(revealed(), "up for as long as it is on");
            page.file_changed(3);
            assert_eq!(reloads.get(), 2, "a change reloads by itself");
            page.show(loaded(3));
            // Off with nothing waiting: the bar goes.
            page.banner.auto.as_ref().unwrap().set_active(false);
            assert!(!revealed());
        });
    }

    #[test]
    fn a_file_name_says_which_viewer_and_text_stays_text() {
        for (name, kind) in [
            ("slides.pdf", Some(ViewKind::Pdf)),
            ("Hero.PNG", Some(ViewKind::Image)),
            ("photo.jpeg", Some(ViewKind::Image)),
            ("clip.webm", Some(ViewKind::Media)),
            ("talk.mp3", Some(ViewKind::Media)),
            ("logo.svg", None),
            ("main.rs", None),
            ("README.md", None),
        ] {
            assert_eq!(kind_for(Path::new(name)), kind, "{name}");
        }
    }

    #[test]
    fn the_scheme_serves_the_viewer_and_only_the_documents_it_holds() {
        assert_eq!(
            pdf_resource("taste-pdf://7/index.html").unwrap().1,
            "text/html"
        );
        assert_eq!(
            pdf_resource("taste-pdf://7/pdf.worker.min.mjs").unwrap().1,
            "text/javascript"
        );
        assert!(pdf_resource("taste-pdf://7/document.pdf").is_none());
        pdf_documents()
            .borrow_mut()
            .insert(7, glib::Bytes::from_static(b"%PDF-1.7"));
        assert_eq!(
            pdf_resource("taste-pdf://7/document.pdf")
                .unwrap()
                .0
                .as_ref(),
            b"%PDF-1.7"
        );
        assert!(pdf_resource("taste-pdf://7/../../etc/passwd").is_none());
        assert!(pdf_resource("https://example.com/").is_none());
        pdf_documents().borrow_mut().remove(&7);
    }
}

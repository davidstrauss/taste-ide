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
//! be edited, and previews there as a picture drawn by the confined helper
//! (`editor::Editor::show_svg`), with an image's zoom ([`ZoomImage`]).
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
    /// The PDF's zoom and layout controls, on the editor's view bar.
    pdf_tools: RefCell<Option<Rc<PdfTools>>>,
    /// An image's picture and its zoom, kept across a reload.
    image: RefCell<Option<Rc<ZoomImage>>>,
    /// The PDF's text, a string a page, once read ([`Self::read_text`]),
    /// for the universal find (David, 2026-10-06: "Universal find should
    /// work on open PDFs. You should even OCR if necessary"). A page drawn
    /// as a picture reads as empty until OCR has read it.
    text: RefCell<Option<Vec<String>>>,
    /// Which reading of the text is current: a reload starts another, and
    /// an older one finishing late is not taken.
    text_generation: Cell<u64>,
    /// A reading is under way.
    reading: Cell<bool>,
    /// The page that draws the PDF has loaded: before, it has nothing to
    /// read the text with.
    loaded: Cell<bool>,
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
            pdf_tools: RefCell::new(None),
            image: RefCell::new(None),
            text: RefCell::new(None),
            text_generation: Cell::new(0),
            reading: Cell::new(false),
            loaded: Cell::new(false),
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
            // `single`, one page at a time; `next`, that and the Right
            // arrow pressed; `zoom:VALUE`, a zoom ("page-fit", "1.5") —
            // each with the toolbar's word for what is shown printed.
            layout if layout == "single" || layout == "next" || layout.starts_with("zoom:") => {
                let Some(view) = self.pdf_view.borrow().clone() else {
                    return;
                };
                let tools = self.pdf_tools.borrow().clone();
                let script = match layout {
                    "single" => "window.tasteSetView('single')".to_string(),
                    "next" => "window.tasteSetView('single'); \
                               setTimeout(() => document.dispatchEvent(new KeyboardEvent('keydown', \
                               {key: 'ArrowRight', bubbles: true})), 300)"
                        .to_string(),
                    zoom => format!("window.tasteSetView(null, {:?})", &zoom[5..]),
                };
                glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
                    run_js(&view, &script);
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(1200),
                        move || {
                            if let Some(tools) = tools {
                                eprintln!(
                                    "probe viewer tools: zoom {:?}, single {}",
                                    tools.zoom.label.label(),
                                    tools.single.is_active()
                                );
                            }
                            webkit6::prelude::WebViewExt::evaluate_javascript(
                                &view,
                                "'page ' + window.tasteScroll()",
                                None,
                                None,
                                None::<&gtk::gio::Cancellable>,
                                |result| {
                                    if let Ok(value) = result {
                                        eprintln!("probe viewer page: {}", value.to_str());
                                    }
                                },
                            );
                        },
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
        // An image again: the new drawing in the same picture, at the zoom
        // it was at.
        let image = self.image.borrow().clone();
        if let (Some(image), Content::Image(texture)) = (image, &loaded.content) {
            self.shown.set(loaded.fingerprint);
            image.set_picture(
                texture,
                (f64::from(texture.width()), f64::from(texture.height())),
                1.0,
            );
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
            (ViewKind::Image, Content::Image(texture)) => {
                let image = ZoomImage::new(
                    &texture,
                    (f64::from(texture.width()), f64::from(texture.height())),
                );
                *self.image.borrow_mut() = Some(image.clone());
                image.scroller.clone().upcast()
            }
            (ViewKind::Media, Content::Bytes(bytes)) => media_view(&bytes),
            (ViewKind::Pdf, Content::Bytes(bytes)) => {
                let id = next_pdf_id();
                pdf_documents().borrow_mut().insert(id, bytes);
                self.pdf_id.set(Some(id));
                // The page tells the toolbar its mode and zoom, however
                // they changed, through a message of its own.
                let manager = webkit6::UserContentManager::new();
                manager.register_script_message_handler("tasteView", None);
                let single = LAST_SINGLE.with(Cell::get);
                let view = pdf_view(id, &manager, single);
                let tools = PdfTools::new(&view, single);
                {
                    let tools = Rc::downgrade(&tools);
                    manager.connect_script_message_received(Some("tasteView"), move |_, value| {
                        if let Some(tools) = tools.upgrade() {
                            tools.reported(&value.to_str());
                        }
                    });
                }
                *self.pdf_tools.borrow_mut() = Some(tools.clone());
                *self.pdf_view.borrow_mut() = Some(view.clone());
                self.loaded.set(false);
                // Its text, once the page that draws it has loaded.
                let weak = self.this.borrow().clone();
                webkit6::prelude::WebViewExt::connect_load_changed(&view, move |_, event| {
                    if event == webkit6::LoadEvent::Finished {
                        if let Some(page) = weak.upgrade() {
                            page.loaded.set(true);
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

    /// The controls the editor's view bar carries for this tab, on its
    /// left: a PDF's zoom and layout, an image's zoom.
    pub fn tools(&self) -> Option<gtk::Widget> {
        if let Some(tools) = self.pdf_tools.borrow().as_ref() {
            return Some(tools.widget.clone().upcast());
        }
        self.image
            .borrow()
            .as_ref()
            .map(|image| image.tools.clone().upcast())
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

    /// Read the text again when there is none and no reading is under way:
    /// a reading that failed — a reload that threw under it — leaves the
    /// PDF unsearchable until something reads it again, and a query is
    /// that something.
    pub fn ensure_text(&self) {
        if self.loaded.get()
            && self.pdf_view.borrow().is_some()
            && self.text.borrow().is_none()
            && !self.reading.get()
        {
            self.read_text();
        }
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
        self.reading.set(true);
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
            page.reading.set(false);
            // A reading that failed says why: silence here is a PDF the
            // find never answers for, with nothing to say what went wrong.
            let pages = match read {
                Ok(value) => {
                    let json = value.to_str().to_string();
                    match serde_json::from_str::<Vec<String>>(&json) {
                        Ok(pages) => Some(pages),
                        Err(e) => {
                            tracing::warn!(
                                "reading {}'s text: not a list of pages ({e}): {}",
                                path_name(&page.path),
                                json.chars().take(200).collect::<String>()
                            );
                            None
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("reading {}'s text: {e}", path_name(&page.path));
                    None
                }
            };
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
///
/// Down the right edge, the document's overview strip — the source map's
/// and the markdown preview's twin (`preview_map.rs`), for a PDF (David,
/// 2026-10-13: "it needs one"): every page drawn small at the strip's
/// width, a document taller than the strip scrolling within it in step
/// with the view, the visible part a translucent block in the accent
/// colour, a press or a drag putting that point at the middle of the view,
/// and the wheel scrolling the document. Where the view is maps to the
/// strip page by page — a place a fraction of the way down a page is that
/// fraction of the way down its picture — so the block is exact at any
/// zoom and over the gaps between pages. The pictures are drawn again
/// from the document a reload brings, and swapped in once all are.
const PDF_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8">
<meta name="color-scheme" content="light dark">
<link rel="stylesheet" href="pdf_viewer.css">
<style>
html, body { margin: 0; height: 100%; background: transparent; }
:root { --map-width: 96px; --map-accent: #3584e4; }
.pane { position: absolute; inset: 0 var(--map-width) 0 0; overflow: auto; }
#map { position: absolute; top: 0; right: 0; width: var(--map-width); height: 100%;
  cursor: default; touch-action: none; }
.pane.behind { visibility: hidden; z-index: -1; }
.pdfViewer .page { border: none; margin: 12px auto;
  box-shadow: 0 1px 3px rgba(0,0,0,.25); }
/* One page at a time, inside the 5px pdf.js leaves above and below a
   fitted page: at 12 the whole page stood a few pixels taller than the
   pane, its foot under the edge. */
body.single .pdfViewer .page { margin: 2px auto; }
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
<canvas id="map"></canvas>
<script type="module">
try {
  const pdfjsLib = await import("./pdf.min.mjs");
  globalThis.pdfjsLib = pdfjsLib;
  pdfjsLib.GlobalWorkerOptions.workerSrc = "./pdf.worker.min.mjs";
  const { EventBus, PDFViewer, PDFLinkService, PDFFindController, ScrollMode } =
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
  // How the document is laid out and how large (the tab's toolbar,
  // viewer.rs): the pages in one scrolling column at the pane's width, or
  // one page at a time fitted whole (David, 2026-10-20: "For single-page,
  // I'd like 'page fit' as the default level. For continuous, 'page
  // width.'"). The mode the tab opens in comes in the address.
  const view = {
    mode: new URLSearchParams(location.search).get("mode") === "single" ? "single" : "continuous",
  };
  const fitOf = (mode) => mode === "single" ? "page-fit" : "page-width";
  const scrollModeOf = (mode) => mode === "single" ? ScrollMode.PAGE : ScrollMode.VERTICAL;
  view.scale = fitOf(view.mode);
  const single = () => view.mode === "single";
  const layOut = () => document.body.classList.toggle("single", single());
  // The toolbar told the mode and the zoom, however they changed: its
  // buttons, Ctrl and the wheel, the keys.
  const report = () => {
    const handlers = window.webkit && window.webkit.messageHandlers;
    if (!handlers || !handlers.tasteView || !shown.doc) return;
    handlers.tasteView.postMessage(JSON.stringify({
      mode: view.mode,
      value: String(shown.viewer.currentScaleValue),
      percent: Math.round(shown.viewer.currentScale * 100),
    }));
  };
  for (const it of [shown, behind]) {
    it.eventBus.on("scalechanging", () => {
      if (it !== shown) return;
      setTimeout(() => { view.scale = shown.viewer.currentScaleValue; report(); }, 0);
    });
  }
  shown.eventBus.on("pagesinit", () => {
    layOut();
    shown.viewer.scrollMode = scrollModeOf(view.mode);
    shown.viewer.currentScaleValue = view.scale;
    report();
  }, { once: true });
  // The toolbar's: a mode, at its own fit unless a zoom is given, and the
  // page in view kept.
  window.tasteSetView = (mode, scale) => {
    if (!shown.doc) return;
    if (mode && mode !== view.mode) {
      view.mode = mode;
      layOut();
      const page = shown.viewer.currentPageNumber;
      shown.viewer.scrollMode = scrollModeOf(mode);
      shown.viewer.currentPageNumber = page;
      if (!scale) scale = fitOf(mode);
    }
    if (scale) {
      view.scale = scale;
      shown.viewer.currentScaleValue = scale;
    }
    if (window.tasteMapDraw) window.tasteMapDraw();
    report();
  };
  window.tasteZoom = (steps) => {
    if (!shown.doc) return;
    if (steps > 0) shown.viewer.increaseScale(); else shown.viewer.decreaseScale();
  };
  show(shown, await open("document.pdf"));

  let generation = 0;
  // The reload under way, if one is: the text is read from the document
  // that comes out of it, not the one going.
  let settled = Promise.resolve();
  // Settled whatever happens in it: the text read after a reload waits
  // on this, and one that threw left every later reading failing with it.
  window.tasteReload = () => {
    settled = reload().catch(() => {});
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
        into.viewer.scrollMode = scrollModeOf(view.mode);
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
    report();
    if (window.tasteMapBuild) window.tasteMapBuild();
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
  // The overview strip.
  const map = document.getElementById("map");
  const GAP = 2;
  // Each page's picture, as tall as the strip's width makes it, and the
  // canvas it is drawn on once it has been.
  let thumbs = [];
  let mapGeneration = 0;
  const mapWidth = () => map.clientWidth;
  const build = async (doc) => {
    const mine = ++mapGeneration;
    const first = thumbs.length === 0;
    const next = [];
    if (first) thumbs = next;
    const ratio = window.devicePixelRatio || 1;
    for (let n = 1; n <= doc.numPages; n++) {
      let page;
      try { page = await doc.getPage(n); } catch (error) { return; }
      if (mine !== mapGeneration) return;
      const unit = page.getViewport({ scale: 1 });
      const width = mapWidth();
      const entry = { h: width * unit.height / unit.width, canvas: null };
      next.push(entry);
      const viewport = page.getViewport({ scale: width * ratio / unit.width });
      const canvas = document.createElement("canvas");
      canvas.width = Math.max(1, Math.floor(viewport.width));
      canvas.height = Math.max(1, Math.floor(viewport.height));
      try {
        await page.render({ canvasContext: canvas.getContext("2d"), viewport }).promise;
      } catch (error) {
        continue;
      }
      if (mine !== mapGeneration) return;
      entry.canvas = canvas;
      if (first) draw();
    }
    if (mine === mapGeneration) { thumbs = next; draw(); }
  };
  // Each page's span in the view and in the strip, top to bottom. One
  // page at a time, the view holds only the page in it, so the strip lays
  // the pictures out alone, and the view is that page's.
  const spans = () => {
    const out = [];
    let y = 0;
    if (single()) {
      for (let i = 0; i < thumbs.length; i++) {
        out.push({ mapTop: y, mapBottom: y + thumbs[i].h });
        y += thumbs[i].h + GAP;
      }
      return out;
    }
    for (let i = 0; i < thumbs.length; i++) {
      const view = shown.viewer.getPageView(i);
      const div = view && view.div;
      if (!div) break;
      out.push({ top: div.offsetTop, bottom: div.offsetTop + div.offsetHeight,
                 mapTop: y, mapBottom: y + thumbs[i].h });
      y += thumbs[i].h + GAP;
    }
    return out;
  };
  // A place in the view as a place in the strip, and back; between pages,
  // the edge of the nearer one.
  const toMap = (list, y) => {
    for (const s of list) {
      if (y <= s.bottom) {
        const f = Math.min(Math.max((y - s.top) / Math.max(s.bottom - s.top, 1), 0), 1);
        return s.mapTop + f * (s.mapBottom - s.mapTop);
      }
    }
    return list.length ? list[list.length - 1].mapBottom : 0;
  };
  const toView = (list, y) => {
    for (const s of list) {
      if (y <= s.mapBottom) {
        const f = Math.min(Math.max((y - s.mapTop) / Math.max(s.mapBottom - s.mapTop, 1), 0), 1);
        return s.top + f * (s.bottom - s.top);
      }
    }
    return list.length ? list[list.length - 1].bottom : 0;
  };
  // How far the pictures are scrolled up in the strip: none while they
  // fit, and in step with the view while they do not.
  const offsetOf = (list) => {
    const pane = shown.container;
    const drawn = list.length ? list[list.length - 1].mapBottom : 0;
    const room = map.clientHeight;
    if (single()) {
      const n = list.length, at = shown.viewer.currentPageNumber - 1;
      return drawn > room && n > 1 ? (drawn - room) * Math.min(Math.max(at / (n - 1), 0), 1) : 0;
    }
    const most = pane.scrollHeight - pane.clientHeight;
    return drawn > room && most > 0 ? (drawn - room) * Math.min(Math.max(pane.scrollTop / most, 0), 1) : 0;
  };
  let queued = false;
  const draw = () => {
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => {
      queued = false;
      const ratio = window.devicePixelRatio || 1;
      const w = map.clientWidth, h = map.clientHeight;
      if (map.width !== Math.round(w * ratio) || map.height !== Math.round(h * ratio)) {
        map.width = Math.round(w * ratio);
        map.height = Math.round(h * ratio);
      }
      const ctx = map.getContext("2d");
      ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
      ctx.clearRect(0, 0, w, h);
      const list = spans();
      if (!list.length) return;
      const offset = offsetOf(list);
      for (let i = 0; i < list.length; i++) {
        const top = list[i].mapTop - offset;
        const height = list[i].mapBottom - list[i].mapTop;
        if (top > h || top + height < 0) continue;
        if (thumbs[i].canvas) {
          ctx.drawImage(thumbs[i].canvas, 0, top, w, height);
        } else {
          ctx.fillStyle = "rgba(255,255,255,.6)";
          ctx.fillRect(0, top, w, height);
        }
      }
      const pane = shown.container;
      let top, bottom;
      if (single()) {
        const s = list[Math.min(shown.viewer.currentPageNumber - 1, list.length - 1)];
        const of = (y) => Math.min(Math.max(y / Math.max(pane.scrollHeight, 1), 0), 1);
        top = s.mapTop + of(pane.scrollTop) * (s.mapBottom - s.mapTop) - offset;
        bottom = s.mapTop + of(pane.scrollTop + pane.clientHeight) * (s.mapBottom - s.mapTop) - offset;
      } else {
        top = toMap(list, pane.scrollTop) - offset;
        bottom = toMap(list, pane.scrollTop + pane.clientHeight) - offset;
      }
      ctx.globalAlpha = 0.25;
      ctx.fillStyle = getComputedStyle(document.documentElement).getPropertyValue("--map-accent").trim() || '#3584e4';
      ctx.fillRect(0, top, w, Math.max(bottom - top, 2));
      ctx.globalAlpha = 1;
    });
  };
  // A press puts that point of the document at the middle of the view; a
  // drag keeps doing so.
  const jump = (event) => {
    const list = spans();
    if (!list.length) return;
    const y = event.clientY - map.getBoundingClientRect().top + offsetOf(list);
    const pane = shown.container;
    if (single()) {
      let i = list.findIndex((s) => y <= s.mapBottom);
      if (i < 0) i = list.length - 1;
      if (shown.viewer.currentPageNumber !== i + 1) shown.viewer.currentPageNumber = i + 1;
      const s = list[i];
      const f = Math.min(Math.max((y - s.mapTop) / Math.max(s.mapBottom - s.mapTop, 1), 0), 1);
      pane.scrollTop = f * pane.scrollHeight - pane.clientHeight / 2;
      return;
    }
    pane.scrollTop = toView(list, y) - pane.clientHeight / 2;
  };
  map.addEventListener("pointerdown", (event) => {
    map.setPointerCapture(event.pointerId);
    jump(event);
    event.preventDefault();
  });
  map.addEventListener("pointermove", (event) => {
    if (map.hasPointerCapture(event.pointerId)) jump(event);
  });
  map.addEventListener("wheel", (event) => {
    if (event.ctrlKey) return;
    shown.container.scrollBy({ top: event.deltaY });
    event.preventDefault();
  }, { passive: false });
  for (const it of [shown, behind]) {
    it.container.addEventListener("scroll", draw, { passive: true });
    it.eventBus.on("updateviewarea", draw);
    it.eventBus.on("pagesloaded", draw);
    it.eventBus.on("pagechanging", draw);
  }
  window.tasteMapDraw = draw;
  new ResizeObserver(draw).observe(map);
  window.tasteMapBuild = () => { if (shown.doc) build(shown.doc); };
  build(shown.doc);
  window.tasteReadyResolve();
  // The keys a reader pages with. The document scrolls inside a pane, not
  // the page, so the browser's own Page Up and Down — which scroll the
  // page — moved nothing (David, 2026-10-06: "pgup/pgdown should also work
  // in pdfs"). A screenful less a line, so a line stays in view across
  // the turn; Space as Page Down, Shift+Space as Page Up; Home and End.
  // Ctrl and = or −, a step of zoom; Ctrl and 0, the mode's own fit. One
  // page at a time, the keys that page on turn the page at its end — and
  // Left and Right always do; Home and End are the first and the last.
  document.addEventListener("keydown", (event) => {
    if (event.ctrlKey && !event.altKey && !event.metaKey) {
      if (event.key === "=" || event.key === "+") window.tasteZoom(1);
      else if (event.key === "-") window.tasteZoom(-1);
      else if (event.key === "0") window.tasteSetView(null, fitOf(view.mode));
      else return;
      event.preventDefault();
      return;
    }
    if (event.ctrlKey || event.altKey || event.metaKey) return;
    const pane = shown.container, line = 40;
    const screen = Math.max(pane.clientHeight - line, line);
    if (single()) {
      const v = shown.viewer;
      const atBottom = pane.scrollTop + pane.clientHeight >= pane.scrollHeight - 2;
      const atTop = pane.scrollTop <= 1;
      const key = event.key;
      const forward = key === "PageDown" || key === "ArrowRight"
        || (key === " " && !event.shiftKey) || (key === "ArrowDown" && atBottom);
      const back = key === "PageUp" || key === "ArrowLeft"
        || (key === " " && event.shiftKey) || (key === "ArrowUp" && atTop);
      if (forward) {
        if (atBottom || key === "ArrowRight") { if (v.nextPage()) pane.scrollTop = 0; }
        else pane.scrollBy({ top: screen });
      } else if (back) {
        if (atTop || key === "ArrowLeft") { if (v.previousPage()) pane.scrollTop = pane.scrollHeight; }
        else pane.scrollBy({ top: -screen });
      } else if (key === "ArrowDown") {
        pane.scrollBy({ top: line });
      } else if (key === "ArrowUp") {
        pane.scrollBy({ top: -line });
      } else if (key === "Home") {
        v.currentPageNumber = 1;
      } else if (key === "End") {
        v.currentPageNumber = v.pagesCount;
      } else {
        return;
      }
      event.preventDefault();
      return;
    }
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
  // The wheel past a page's end turns it, one page at a time: a wheel's
  // worth of travel there, and a breath after each turn so a touchpad's
  // glide does not run through several.
  let travel = 0, turnedAt = 0;
  for (const container of document.querySelectorAll(".pane")) {
    container.addEventListener("wheel", (event) => {
      if (event.ctrlKey) {
        event.preventDefault();
        shown.viewer.currentScale *= event.deltaY < 0 ? 1.1 : 1 / 1.1;
        return;
      }
      if (!single()) return;
      const pane = shown.container;
      const atBottom = pane.scrollTop + pane.clientHeight >= pane.scrollHeight - 2;
      const atTop = pane.scrollTop <= 1;
      if (!((event.deltaY > 0 && atBottom) || (event.deltaY < 0 && atTop))) {
        travel = 0;
        return;
      }
      event.preventDefault();
      if (performance.now() - turnedAt < 350) return;
      travel += event.deltaY;
      if (Math.abs(travel) < 120) return;
      const forward = travel > 0;
      travel = 0;
      turnedAt = performance.now();
      if (forward ? shown.viewer.nextPage() : shown.viewer.previousPage()) {
        pane.scrollTop = forward ? 0 : pane.scrollHeight;
      }
    }, { passive: false });
  }
  // A fit, kept as the pane is resized: the width, or the whole page.
  new ResizeObserver(() => {
    for (const it of [shown, behind]) {
      const value = it.doc && it.viewer.currentScaleValue;
      if (value === "page-width" || value === "page-fit" || value === "auto") {
        it.viewer.currentScaleValue = value;
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

thread_local! {
    /// The layout last chosen, which the next PDF opens in: a reader who
    /// wants slides one at a time wants the next deck that way too.
    static LAST_SINGLE: Cell<bool> = const { Cell::new(false) };
}

/// The zoom as GNOME's document viewer has it (David, 2026-10-20: "There
/// is a standard GNOME approach to zooming controls. Can we use that?"):
/// one button wearing the zoom in force as a percentage, whatever set it,
/// on a label of a fixed width so nothing beside it moves as it changes;
/// its menu the fits as radio items, the one in force checked, and the
/// steps under them. Ctrl and the wheel, and Ctrl and = or −, step it.
pub(crate) struct ZoomMenu {
    pub button: gtk::MenuButton,
    label: gtk::Label,
    fit: gtk::gio::SimpleAction,
}

/// The zoom menu's steps, Papers's.
const ZOOM_STEPS: &[f64] = &[0.5, 0.7, 0.85, 1.0, 1.25, 1.5, 1.75, 2.0, 3.0, 4.0];

/// A zoom as the button says it: "100%", "76.3%".
fn percent_words(percent: f64) -> String {
    if (percent - percent.round()).abs() < 0.05 {
        format!("{}%", percent.round())
    } else {
        format!("{percent:.1}%")
    }
}

impl ZoomMenu {
    /// `fits` as (what the menu says, what `on_fit` is told); a step tells
    /// `on_scale` its scale.
    pub(crate) fn new(
        fits: &[(&str, &str)],
        on_fit: impl Fn(&str) + 'static,
        on_scale: impl Fn(f64) + 'static,
    ) -> Self {
        let menu = gtk::gio::Menu::new();
        let fit_section = gtk::gio::Menu::new();
        for (label, value) in fits {
            fit_section.append(Some(label), Some(&format!("zoom.fit::{value}")));
        }
        menu.append_section(None, &fit_section);
        let steps = gtk::gio::Menu::new();
        for step in ZOOM_STEPS {
            steps.append(
                Some(&percent_words(step * 100.0)),
                Some(&format!("zoom.to::{step}")),
            );
        }
        menu.append_section(None, &steps);
        let label = gtk::Label::builder()
            .label("100%")
            // "1234.5%", the widest it says.
            .width_chars(7)
            .xalign(1.0)
            .build();
        let face = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        face.append(&label);
        face.append(&gtk::Image::from_icon_name("pan-down-symbolic"));
        let button = gtk::MenuButton::builder()
            .child(&face)
            .menu_model(&menu)
            .tooltip_text("Zoom")
            .css_classes(["flat"])
            .build();
        let actions = gtk::gio::SimpleActionGroup::new();
        let fit = gtk::gio::SimpleAction::new_stateful(
            "fit",
            Some(glib::VariantTy::STRING),
            &"".to_variant(),
        );
        fit.connect_activate(move |action, value| {
            if let Some(value) = value {
                action.set_state(value);
                if let Some(value) = value.str() {
                    on_fit(value);
                }
            }
        });
        actions.add_action(&fit);
        let to = gtk::gio::SimpleAction::new("to", Some(glib::VariantTy::STRING));
        {
            let fit = fit.clone();
            to.connect_activate(move |_, value| {
                if let Some(scale) = value.and_then(|v| v.str()).and_then(|v| v.parse().ok()) {
                    fit.set_state(&"".to_variant());
                    on_scale(scale);
                }
            });
        }
        actions.add_action(&to);
        button.insert_action_group("zoom", Some(&actions));
        Self { button, label, fit }
    }

    /// What is shown: the zoom in force, and the fit that set it if one did.
    pub(crate) fn show(&self, percent: f64, fit: Option<&str>) {
        self.label.set_label(&percent_words(percent));
        self.fit.set_state(&fit.unwrap_or("").to_variant());
    }
}

/// The zoom steps the buttons and Ctrl and the wheel move between.
const IMAGE_STEPS: &[f64] = &[
    0.1, 0.25, 0.5, 0.7, 0.85, 1.0, 1.25, 1.5, 1.75, 2.0, 3.0, 4.0, 6.0, 8.0,
];

/// How a picture is sized in its pane.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ImageZoom {
    /// Whole in the pane, never past its own size.
    Fit,
    /// As wide as the pane, scrolling down.
    FitWidth,
    /// At a scale of its own size.
    Scale(f64),
}

/// A picture with the PDF's zoom tools (David, 2026-10-20: "Images
/// actually should get the zoom and fit interface, too"): fit, fit width,
/// and steps from a tenth to eight times, the buttons and Ctrl and the
/// wheel moving between them, and the picture's size beside them. An
/// image file's viewer and an SVG file's preview. A picture drawn at a
/// scale of its own (an SVG's) is asked for again when a zoom goes past it
/// ([`ZoomImage::set_redraw`]), so a zoomed drawing stays sharp.
pub(crate) struct ZoomImage {
    /// The zoom out, the zoom, the zoom in, and the size: the view bar's
    /// left (`Editor::show_view_modes`).
    pub tools: gtk::Box,
    pub scroller: gtk::ScrolledWindow,
    picture: gtk::Picture,
    menu: ZoomMenu,
    size_label: gtk::Label,
    zoom: Cell<ImageZoom>,
    /// The picture's own size, in logical pixels.
    natural: Cell<(f64, f64)>,
    /// The scale the picture was drawn at, over its logical size.
    drawn_at: Cell<f64>,
    redraw: RefCell<Option<Rc<dyn Fn(f64)>>>,
}

impl ZoomImage {
    pub(crate) fn new(paintable: &impl IsA<gtk::gdk::Paintable>, natural: (f64, f64)) -> Rc<Self> {
        let picture = gtk::Picture::for_paintable(paintable);
        picture.set_can_shrink(true);
        let center = gtk::Box::new(gtk::Orientation::Vertical, 0);
        center.set_halign(gtk::Align::Center);
        center.set_valign(gtk::Align::Center);
        center.set_margin_top(12);
        center.set_margin_bottom(12);
        center.set_margin_start(12);
        center.set_margin_end(12);
        center.append(&picture);
        let scroller = gtk::ScrolledWindow::builder()
            .child(&center)
            .vexpand(true)
            .hexpand(true)
            .build();
        let size_label = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .margin_start(8)
            .build();
        let tools = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        let this = Rc::new_cyclic(|weak: &std::rc::Weak<Self>| {
            let (on_fit, on_scale) = (weak.clone(), weak.clone());
            let menu = ZoomMenu::new(
                &[("Best Fit", "fit"), ("Fit Width", "width")],
                move |fit| {
                    if let Some(this) = on_fit.upgrade() {
                        this.set_zoom(if fit == "width" {
                            ImageZoom::FitWidth
                        } else {
                            ImageZoom::Fit
                        });
                    }
                },
                move |scale| {
                    if let Some(this) = on_scale.upgrade() {
                        this.set_zoom(ImageZoom::Scale(scale));
                    }
                },
            );
            tools.append(&size_label);
            tools.append(&menu.button);
            Self {
                tools,
                scroller,
                picture,
                menu,
                size_label,
                zoom: Cell::new(ImageZoom::Fit),
                natural: Cell::new(natural),
                drawn_at: Cell::new(1.0),
                redraw: RefCell::new(None),
            }
        });
        // Ctrl and the wheel zoom, as in the PDF; the wheel alone scrolls.
        let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let weak = Rc::downgrade(&this);
            wheel.connect_scroll(move |controller, _, dy| {
                let ctrl = controller
                    .current_event_state()
                    .contains(gtk::gdk::ModifierType::CONTROL_MASK);
                let Some(this) = weak.upgrade().filter(|_| ctrl) else {
                    return glib::Propagation::Proceed;
                };
                this.step(if dy < 0.0 { 1 } else { -1 });
                glib::Propagation::Stop
            });
        }
        this.scroller.add_controller(wheel);
        // A fit's percentage is the pane's to decide, and changes with it:
        // said again whenever the picture's width does.
        {
            let weak = Rc::downgrade(&this);
            let last = Cell::new(-1);
            this.picture.add_tick_callback(move |picture, _| {
                if picture.width() != last.get() {
                    last.set(picture.width());
                    if let Some(this) = weak.upgrade() {
                        this.say_zoom();
                    }
                }
                glib::ControlFlow::Continue
            });
        }
        this.apply();
        this
    }

    /// A new drawing of the same picture — a reload, or an SVG drawn again
    /// — at `drawn_at` over its `natural` size, at the zoom in force.
    pub(crate) fn set_picture(
        &self,
        paintable: &impl IsA<gtk::gdk::Paintable>,
        natural: (f64, f64),
        drawn_at: f64,
    ) {
        self.picture.set_paintable(Some(paintable));
        self.natural.set(natural);
        self.drawn_at.set(drawn_at);
        self.apply();
    }

    /// Why the picture could not be drawn, where its size is said.
    pub(crate) fn show_error(&self, why: &str) {
        self.size_label.set_label(&format!("Not drawn: {why}"));
    }

    /// What draws the picture again at a scale, for one past its drawing.
    pub(crate) fn set_redraw(&self, redraw: Rc<dyn Fn(f64)>) {
        *self.redraw.borrow_mut() = Some(redraw);
    }

    fn set_zoom(&self, zoom: ImageZoom) {
        self.zoom.set(zoom);
        self.apply();
    }

    /// The scale on screen now, over the picture's own size. A fit's is
    /// the picture's box over its size — the box fills the pane, so Best
    /// Fit's is the lesser of the two ratios and never past one, and Fit
    /// Width's is the width's.
    fn scale_now(&self) -> f64 {
        let (width, height) = self.natural.get();
        let (boxed_w, boxed_h) = (
            f64::from(self.picture.width()),
            f64::from(self.picture.height()),
        );
        let known = width > 0.0 && height > 0.0 && boxed_w > 0.0 && boxed_h > 0.0;
        match self.zoom.get() {
            ImageZoom::Scale(scale) => scale,
            ImageZoom::Fit if known => (boxed_w / width).min(boxed_h / height).min(1.0),
            ImageZoom::FitWidth if known => boxed_w / width,
            _ => 1.0,
        }
    }

    /// A step in or out from the scale on screen, whatever the zoom was.
    fn step(&self, steps: i32) {
        let now = self.scale_now();
        let next = if steps > 0 {
            IMAGE_STEPS.iter().copied().find(|s| *s > now * 1.001)
        } else {
            IMAGE_STEPS.iter().rev().copied().find(|s| *s < now * 0.999)
        };
        if let Some(next) = next {
            self.set_zoom(ImageZoom::Scale(next));
        }
    }

    /// The zoom button, told the scale on screen and the fit that set it.
    fn say_zoom(&self) {
        self.menu.show(
            self.scale_now() * 100.0,
            match self.zoom.get() {
                ImageZoom::Fit => Some("fit"),
                ImageZoom::FitWidth => Some("width"),
                ImageZoom::Scale(_) => None,
            },
        );
    }

    fn apply(&self) {
        let (width, height) = self.natural.get();
        self.size_label
            .set_label(&format!("{} × {}", width.round(), height.round()));
        let center = self.picture.parent();
        match self.zoom.get() {
            ImageZoom::Fit => {
                self.scroller
                    .set_policy(gtk::PolicyType::Never, gtk::PolicyType::Never);
                self.picture.set_size_request(-1, -1);
                self.picture.set_content_fit(gtk::ContentFit::ScaleDown);
                self.picture.set_can_shrink(true);
                if let Some(center) = &center {
                    center.set_halign(gtk::Align::Fill);
                    center.set_valign(gtk::Align::Fill);
                }
                self.picture.set_hexpand(true);
                self.picture.set_vexpand(true);
            }
            ImageZoom::FitWidth => {
                self.scroller
                    .set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
                self.picture.set_size_request(-1, -1);
                self.picture.set_content_fit(gtk::ContentFit::Contain);
                self.picture.set_can_shrink(true);
                if let Some(center) = &center {
                    center.set_halign(gtk::Align::Fill);
                    center.set_valign(gtk::Align::Start);
                }
                self.picture.set_hexpand(true);
                self.picture.set_vexpand(false);
            }
            ImageZoom::Scale(scale) => {
                self.scroller
                    .set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
                self.picture.set_content_fit(gtk::ContentFit::Fill);
                self.picture.set_can_shrink(false);
                self.picture.set_size_request(
                    (width * scale).round().max(1.0) as i32,
                    (height * scale).round().max(1.0) as i32,
                );
                if let Some(center) = &center {
                    center.set_halign(gtk::Align::Center);
                    center.set_valign(gtk::Align::Center);
                }
                self.picture.set_hexpand(false);
                self.picture.set_vexpand(false);
            }
        }
        self.say_zoom();
        // Past what the picture was drawn at: drawn again, sharper — but
        // never past eight times, the most it is drawn at, so a drawing at
        // the most cannot ask for another.
        let wanted = (self.scale_now() * f64::from(crate::mermaid::display_scale())).min(8.0);
        if wanted > self.drawn_at.get() * 1.05 {
            if let Some(redraw) = self.redraw.borrow().clone() {
                redraw(wanted);
            }
        }
    }
}

/// A PDF's zoom and layout, over its view (David, 2026-10-20: "I'd like
/// zoom tools for PDFs and a choice of continuous or single-page"): zoom
/// out, the zoom — a menu of the two fits and the usual steps, which wears
/// the zoom in force — and zoom in; then the pages in one scrolling
/// column, or one at a time. Each layout opens at its own fit, the width
/// for a column and the whole page for one at a time. The page reports
/// every change back ([`PdfTools::reported`]), Ctrl and the wheel's
/// included, so what the toolbar says is what the page shows.
struct PdfTools {
    widget: gtk::Box,
    zoom: ZoomMenu,
    continuous: gtk::ToggleButton,
    single: gtk::ToggleButton,
    /// Set while the toolbar is told rather than pressed, so a report does
    /// not echo back to the page as a choice.
    quiet: Cell<bool>,
}

impl PdfTools {
    fn new(view: &webkit6::WebView, single: bool) -> Rc<Self> {
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        widget.add_css_class("pdf-tools");
        let zoom = {
            let (fit_view, step_view) = (view.clone(), view.clone());
            ZoomMenu::new(
                &[
                    ("Fit Page", "page-fit"),
                    ("Fit Width", "page-width"),
                    ("Automatic", "auto"),
                ],
                move |fit| run_js(&fit_view, &format!("window.tasteSetView(null, {fit:?})")),
                move |scale| {
                    run_js(
                        &step_view,
                        &format!("window.tasteSetView(null, \"{scale}\")"),
                    )
                },
            )
        };
        zoom.show(100.0, Some(if single { "page-fit" } else { "page-width" }));
        let gap = gtk::Box::builder().width_request(6).build();
        let continuous = gtk::ToggleButton::builder()
            .icon_name("view-continuous-symbolic")
            .tooltip_text("Continuous")
            .active(!single)
            .build();
        let single_button = gtk::ToggleButton::builder()
            .icon_name("view-paged-symbolic")
            .tooltip_text("Single page")
            .group(&continuous)
            .active(single)
            .build();
        let layout = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        layout.add_css_class("linked");
        layout.append(&continuous);
        layout.append(&single_button);
        widget.append(&zoom.button);
        widget.append(&gap);
        widget.append(&layout);
        let tools = Rc::new(Self {
            widget,
            zoom,
            continuous,
            single: single_button,
            quiet: Cell::new(false),
        });
        for (button, mode) in [(&tools.continuous, "continuous"), (&tools.single, "single")] {
            let (view, weak) = (view.clone(), Rc::downgrade(&tools));
            button.connect_toggled(move |button| {
                let Some(tools) = weak.upgrade() else { return };
                if !button.is_active() || tools.quiet.get() {
                    return;
                }
                LAST_SINGLE.with(|last| last.set(mode == "single"));
                run_js(&view, &format!("window.tasteSetView({mode:?})"));
            });
        }
        tools
    }

    /// What the page says it shows: `{"mode", "value", "percent"}`.
    fn reported(&self, json: &str) {
        let Ok(said) = serde_json::from_str::<serde_json::Value>(json) else {
            return;
        };
        let mode = said["mode"].as_str().unwrap_or("continuous");
        let value = said["value"].as_str().unwrap_or("");
        let fit = matches!(value, "page-fit" | "page-width" | "auto").then_some(value);
        self.quiet.set(true);
        self.zoom
            .show(said["percent"].as_f64().unwrap_or(100.0), fit);
        if mode == "single" {
            self.single.set_active(true);
        } else {
            self.continuous.set_active(true);
        }
        self.quiet.set(false);
    }
}

fn run_js(view: &webkit6::WebView, script: &str) {
    webkit6::prelude::WebViewExt::evaluate_javascript(
        view,
        script,
        None,
        None,
        None::<&gtk::gio::Cancellable>,
        |_| {},
    );
}

/// The PDF of `id`, in a WebKit view that loads nothing but the viewer
/// scheme: navigation anywhere else is refused, and a link the user
/// clicks opens in their browser instead.
/// It opens one page at a time when `single`, and `manager` carries its
/// reports to the toolbar.
fn pdf_view(id: u64, manager: &webkit6::UserContentManager, single: bool) -> webkit6::WebView {
    register_pdf_scheme();
    let session = webkit6::NetworkSession::new_ephemeral();
    let view = webkit6::WebView::builder()
        .network_session(&session)
        .user_content_manager(manager)
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
    // The overview strip's block in this desktop's accent colour, as the
    // source map's is; the page cannot ask GTK for it.
    webkit6::prelude::WebViewExt::connect_load_changed(&view, |view, event| {
        if event != webkit6::LoadEvent::Finished {
            return;
        }
        let accent = adw::StyleManager::default().accent_color_rgba();
        webkit6::prelude::WebViewExt::evaluate_javascript(
            view,
            &format!("document.documentElement.style.setProperty('--map-accent', '{accent}')"),
            None,
            None,
            None::<&gtk::gio::Cancellable>,
            |_| {},
        );
    });
    webkit6::prelude::WebViewExt::load_uri(
        &view,
        &format!(
            "{PDF_SCHEME}://{id}/index.html?mode={}",
            if single { "single" } else { "continuous" }
        ),
    );
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

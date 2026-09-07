//! The composer: one widget for every place the user writes more than a
//! word and acts on it — the chat prompt, the backlog's issue field, and
//! (its action row only, for now) the commit box.
//!
//! `docs/spikes/one-composer-and-evidence.md` → "One composer": the anatomy
//! is fixed and defined once. A chip row that is hidden until there is a
//! chip; the field, a `TextView` in the `.prompt-entry` card that grows to
//! `MAX_LINES` and then scrolls; and the action row — `+` for attachments,
//! the microphone, whatever buttons the home adds, and the primary pill.
//! Attachments come with it: the `+` menu, the drop target, the paste
//! handler and the chips all live here, and a home reads them back with
//! [`Composer::take_attachments`]. What differs by home is the primary
//! action's name, the extra buttons, and what the keys do — the field does
//! not.
//!
//! Voice is push-to-talk into the field (spike → Voice): hold the
//! microphone, speak, release; the words land at the cursor for the user
//! to read and fix before anything acts on them. A quick tap toggles
//! instead, for a long dictation. Capture and transcription are
//! `taste-voice`, on this side of the line.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use agent_client_protocol::schema::v1::{
    ContentBlock, EmbeddedResource, EmbeddedResourceResource, ImageContent, TextResourceContents,
};
use gtk::glib;
use taste_core::Workspace;

pub const MAX_TEXT_ATTACHMENT_BYTES: u64 = 256 * 1024;
pub const MAX_IMAGE_ATTACHMENT_BYTES: u64 = 5 * 1024 * 1024;
pub const ATTACHMENT_THUMBNAIL_PX: i32 = 56;
/// Lines the field grows to before it scrolls inside itself.
pub const MAX_LINES: i32 = 8;
/// A press shorter than this is a tap: it toggles recording rather than
/// bounding it, so a long dictation does not need a held finger.
const TAP: Duration = Duration::from_millis(350);
/// The longest one recording may run; whisper's window is thirty seconds
/// and a minute of speech is a paragraph the user would rather type.
const MAX_RECORDING: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachAs {
    /// The image dialog: a picture, or a refusal.
    Image,
    /// Attach Active File and the text dialog: a text resource.
    Text,
    /// A drop: whichever the file turns out to be.
    Either,
}

/// One attachment: what the chip says, and the block a prompt carries.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub label: String,
    pub block: ContentBlock,
}

impl Attachment {
    /// The bytes and a file name, for a home that stores files (the
    /// backlog) rather than sending blocks (the chat).
    pub fn as_file(&self) -> Option<(String, Vec<u8>)> {
        match &self.block {
            ContentBlock::Image(image) => {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&image.data)
                    .ok()?;
                let ext = match image.mime_type.as_str() {
                    "image/jpeg" => "jpg",
                    "image/webp" => "webp",
                    "image/gif" => "gif",
                    _ => "png",
                };
                let stem = self.label.trim_end_matches(&format!(".{ext}"));
                Some((format!("{stem}.{ext}"), bytes))
            }
            ContentBlock::Resource(resource) => match &resource.resource {
                EmbeddedResourceResource::TextResourceContents(text) => {
                    Some((self.label.clone(), text.text.clone().into_bytes()))
                }
                _ => None,
            },
            ContentBlock::Text(text) => Some((self.label.clone(), text.text.clone().into_bytes())),
            _ => None,
        }
    }
}

type Hook = Box<dyn Fn()>;
type NoticeHook = Box<dyn Fn(String)>;

enum Voice {
    Idle,
    Recording {
        recorder: taste_voice::Recorder,
        since: Instant,
        /// Set by a press while already recording: the release stops.
        stop_on_release: bool,
        /// The field had focus when this started, so the words belong at
        /// the cursor. Otherwise they go to the end, and the cursor after
        /// them, so Enter sends what was just said.
        at_cursor: bool,
    },
    Transcribing,
}

pub struct Composer {
    /// Chips, field and action row, in a vertical box with no margins of
    /// its own — the home sets the inset its column uses.
    pub widget: gtk::Box,
    pub entry: sourceview5::View,
    pub mic: gtk::Button,
    pub primary: gtk::Button,
    chips: gtk::FlowBox,
    placeholder: gtk::Label,
    level: gtk::LevelBar,
    attachments: RefCell<Vec<Attachment>>,
    workspace: Workspace,
    voice: RefCell<Voice>,
    on_change: RefCell<Option<Hook>>,
    on_notice: RefCell<Option<NoticeHook>>,
}

impl Composer {
    /// `primary` is the pill's label; `extras` sit between the microphone
    /// and the pill, in order. Every button in the row is a `pill-action`.
    pub fn new(workspace: &Workspace, primary: &str, extras: &[gtk::Button]) -> Rc<Self> {
        // A GtkSourceView, not a TextView: the chat hangs its slash-command
        // completion off it, and one field type keeps the two homes one
        // widget. Its style scheme is cleared so the theme's text colour
        // shows through the card (a silent skip here is how a black-on-grey
        // composer once shipped).
        let entry = sourceview5::View::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .top_margin(12)
            .bottom_margin(12)
            .left_margin(12)
            .right_margin(12)
            .pixels_inside_wrap(3)
            .hexpand(true)
            .build();
        entry.set_widget_name("composer-entry");
        match entry.buffer().downcast::<sourceview5::Buffer>() {
            Ok(buffer) => {
                sourceview5::prelude::BufferExt::set_style_scheme(
                    &buffer,
                    None::<&sourceview5::StyleScheme>,
                );
            }
            Err(buffer) => tracing::warn!(
                "composer buffer is {}, not a GtkSourceBuffer — style scheme not cleared",
                buffer.type_()
            ),
        }
        {
            let unhyphenated = gtk::TextTag::builder().insert_hyphens(false).build();
            entry.buffer().tag_table().add(&unhyphenated);
            entry.buffer().connect_changed(move |buffer| {
                let (start, end) = buffer.bounds();
                buffer.apply_tag(&unhyphenated, &start, &end);
            });
        }
        let scroller = gtk::ScrolledWindow::builder()
            .child(&entry)
            .vscrollbar_policy(gtk::PolicyType::External)
            .min_content_height(0)
            .max_content_height(120)
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .build();
        // The placeholder floats over the empty field and cannot be hit;
        // homes that want one set it (`set_placeholder`).
        let placeholder = gtk::Label::builder()
            .xalign(0.0)
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .margin_start(12)
            .margin_top(12)
            .can_target(false)
            .css_classes(["dim-label", "composer-placeholder"])
            .visible(false)
            .build();
        let field_overlay = gtk::Overlay::builder().child(&scroller).build();
        field_overlay.add_overlay(&placeholder);
        let field = gtk::Box::new(gtk::Orientation::Vertical, 0);
        field.add_css_class("prompt-entry");
        field.set_widget_name("composer");
        field.append(&field_overlay);
        grow_to_fit(&entry, &scroller);

        let attach_menu = gtk::gio::Menu::new();
        attach_menu.append(Some("Current Selection"), Some("composer.attach-selection"));
        attach_menu.append(Some("Active File"), Some("composer.attach-active"));
        attach_menu.append(Some("File…"), Some("composer.attach-file"));
        attach_menu.append(Some("Image…"), Some("composer.attach-image"));
        let attach_button = gtk::MenuButton::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Attach context (images can also be pasted or dropped)")
            .css_classes(["pill-action"])
            .menu_model(&attach_menu)
            .build();
        attach_button.set_size_request(34, -1);
        let mic = gtk::Button::builder()
            .icon_name("audio-input-microphone-symbolic")
            .tooltip_text(
                "Hold to talk; release to transcribe. Tap to start a longer dictation, tap \
                 again to stop. Ctrl+Shift+M dictates into the chat, Ctrl+Shift+I into a \
                 new issue.",
            )
            .css_classes(["pill-action", "composer-mic"])
            .build();
        mic.set_size_request(34, -1);
        let level = gtk::LevelBar::builder()
            .min_value(0.0)
            .max_value(1.0)
            .valign(gtk::Align::Center)
            .css_classes(["voice-level"])
            .visible(false)
            .build();
        level.set_size_request(40, 4);
        let primary_button = gtk::Button::builder()
            .label(primary)
            .css_classes(["pill-action"])
            .sensitive(false)
            .hexpand(true)
            .build();

        let chips = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(false)
            .max_children_per_line(12)
            .column_spacing(6)
            .row_spacing(4)
            .margin_top(6)
            .visible(false)
            .build();

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.append(&attach_button);
        row.append(&mic);
        row.append(&level);
        for extra in extras {
            extra.add_css_class("pill-action");
            extra.set_size_request(34, -1);
            row.append(extra);
        }
        row.append(&primary_button);

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 6);
        widget.append(&chips);
        widget.append(&field);
        widget.append(&row);

        let composer = Rc::new(Self {
            widget,
            entry: entry.clone(),
            mic: mic.clone(),
            primary: primary_button,
            chips,
            placeholder,
            level,
            attachments: RefCell::new(Vec::new()),
            workspace: workspace.clone(),
            voice: RefCell::new(Voice::Idle),
            on_change: RefCell::new(None),
            on_notice: RefCell::new(None),
        });

        {
            let actions = gtk::gio::SimpleActionGroup::new();
            let add = |name: &str, run: fn(&Rc<Composer>)| {
                let action = gtk::gio::SimpleAction::new(name, None);
                let weak = Rc::downgrade(&composer);
                action.connect_activate(move |_, _| {
                    if let Some(composer) = weak.upgrade() {
                        run(&composer);
                    }
                });
                actions.add_action(&action);
            };
            add("attach-selection", Composer::attach_selection);
            add("attach-active", Composer::attach_active_file);
            add("attach-file", |c| c.attach_via_dialog(false));
            add("attach-image", |c| c.attach_via_dialog(true));
            composer
                .widget
                .insert_action_group("composer", Some(&actions));
        }
        {
            let drop = gtk::DropTarget::new(
                gtk::gdk::FileList::static_type(),
                gtk::gdk::DragAction::COPY,
            );
            let weak = Rc::downgrade(&composer);
            drop.connect_drop(move |_, value, _, _| {
                let Some(composer) = weak.upgrade() else {
                    return false;
                };
                let Ok(files) = value.get::<gtk::gdk::FileList>() else {
                    return false;
                };
                let paths: Vec<std::path::PathBuf> = files
                    .files()
                    .iter()
                    .filter_map(|file| file.path())
                    .collect();
                composer.attach_from_disk(paths, AttachAs::Either);
                true
            });
            composer.widget.add_controller(drop);
        }
        {
            let weak = Rc::downgrade(&composer);
            entry.connect_paste_clipboard(move |view| {
                let Some(composer) = weak.upgrade() else {
                    return;
                };
                let clipboard = view.clipboard();
                if !clipboard
                    .formats()
                    .contains_type(gtk::gdk::Texture::static_type())
                {
                    return;
                }
                let weak = Rc::downgrade(&composer);
                clipboard.read_texture_async(gtk::gio::Cancellable::NONE, move |result| {
                    let Some(composer) = weak.upgrade() else {
                        return;
                    };
                    if let Ok(Some(texture)) = result {
                        use base64::Engine;
                        let png = texture.save_to_png_bytes();
                        let data = base64::engine::general_purpose::STANDARD.encode(png.as_ref());
                        composer.add_attachment(
                            "pasted image".into(),
                            ContentBlock::Image(ImageContent::new(data, "image/png")),
                        );
                    }
                });
            });
        }
        {
            let weak = Rc::downgrade(&composer);
            entry.buffer().connect_changed(move |_| {
                if let Some(composer) = weak.upgrade() {
                    composer.changed();
                }
            });
        }
        composer.wire_mic(&mic);
        composer
    }

    /// A hint in the empty field. Hidden the moment there is text.
    pub fn set_placeholder(&self, text: &str) {
        self.placeholder.set_label(text);
        self.placeholder.set_visible(self.text().is_empty());
    }

    pub fn set_on_change(&self, hook: impl Fn() + 'static) {
        *self.on_change.borrow_mut() = Some(Box::new(hook));
    }

    /// Where the composer says things that are not the text: an attachment
    /// it could not read, the speech model downloading, no microphone. The
    /// chat puts these on its meta row, the backlog on a toast.
    pub fn set_on_notice(&self, hook: impl Fn(String) + 'static) {
        *self.on_notice.borrow_mut() = Some(Box::new(hook));
    }

    fn notice(&self, text: String) {
        if let Some(hook) = self.on_notice.borrow().as_ref() {
            hook(text);
        }
    }

    fn changed(&self) {
        if !self.placeholder.label().is_empty() {
            self.placeholder.set_visible(self.text().is_empty());
        }
        if let Some(hook) = self.on_change.borrow().as_ref() {
            hook();
        }
    }

    pub fn text(&self) -> String {
        let buffer = self.entry.buffer();
        let (start, end) = buffer.bounds();
        buffer.text(&start, &end, true).to_string()
    }

    pub fn set_text(&self, text: &str) {
        self.entry.buffer().set_text(text);
        let end = self.entry.buffer().end_iter();
        self.entry.buffer().place_cursor(&end);
    }

    /// Text and attachments both.
    pub fn clear(self: &Rc<Self>) {
        self.entry.buffer().set_text("");
        self.attachments.borrow_mut().clear();
        self.refresh_chips();
    }

    pub fn attachment_count(&self) -> usize {
        self.attachments.borrow().len()
    }

    /// Hand over the attachments and clear the chips: what sending does.
    pub fn take_attachments(self: &Rc<Self>) -> Vec<Attachment> {
        let taken: Vec<Attachment> = self.attachments.borrow_mut().drain(..).collect();
        self.refresh_chips();
        taken
    }

    /// The pill's readiness: sensitive and suggested when there is
    /// something to act on.
    pub fn set_primary_ready(&self, ready: bool) {
        if self.primary.is_sensitive() != ready {
            self.primary.set_sensitive(ready);
        }
        if ready {
            self.primary.add_css_class("suggested-action");
        } else {
            self.primary.remove_css_class("suggested-action");
        }
    }

    pub fn add_attachment(self: &Rc<Self>, label: String, block: ContentBlock) {
        self.attachments
            .borrow_mut()
            .push(Attachment { label, block });
        self.refresh_chips();
    }

    fn refresh_chips(self: &Rc<Self>) {
        while let Some(child) = self.chips.first_child() {
            self.chips.remove(&child);
        }
        let attachments = self.attachments.borrow();
        self.chips.set_visible(!attachments.is_empty());
        for (index, attachment) in attachments.iter().enumerate() {
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            let thumbnail = match &attachment.block {
                ContentBlock::Image(image) => decode_image(image),
                _ => None,
            };
            match &thumbnail {
                Some(texture) => content.append(&image_thumbnail(texture)),
                None => content.append(
                    &gtk::Label::builder()
                        .label(&attachment.label)
                        .ellipsize(gtk::pango::EllipsizeMode::Middle)
                        .css_classes(["caption"])
                        .build(),
                ),
            }
            let close = gtk::Image::from_icon_name("window-close-symbolic");
            close.add_css_class("dim-label");
            content.append(&close);
            let chip = gtk::Button::builder()
                .child(&content)
                .tooltip_text(format!("Remove {}", attachment.label))
                .css_classes(["flat", "attachment-chip"])
                .halign(gtk::Align::Start)
                .valign(gtk::Align::Center)
                .build();
            chip.update_property(&[gtk::accessible::Property::Label(&format!(
                "Remove attachment {}",
                attachment.label
            ))]);
            let weak = Rc::downgrade(self);
            chip.connect_clicked(move |_| {
                let Some(composer) = weak.upgrade() else {
                    return;
                };
                composer.attachments.borrow_mut().remove(index);
                composer.refresh_chips();
            });
            self.chips.append(&chip);
        }
        drop(attachments);
        self.changed();
    }

    fn attach_selection(self: &Rc<Self>) {
        let Some(selection) = self.workspace.ide.selection() else {
            self.notice("no selection to attach".into());
            return;
        };
        let label = format!(
            "{}:{}–{}",
            selection
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            selection.start_line,
            selection.end_line
        );
        let uri = format!("file://{}", selection.path.display());
        let block = ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                selection.text,
                uri,
            )),
        ));
        self.add_attachment(label, block);
    }

    fn attach_active_file(self: &Rc<Self>) {
        let Some(active) = self
            .workspace
            .ide
            .open_files()
            .into_iter()
            .find(|f| f.active)
        else {
            self.notice("no active file to attach".into());
            return;
        };
        self.attach_from_disk(vec![active.path], AttachAs::Text);
    }

    /// Read files off the blocking pool and attach what they turn out to be.
    pub fn attach_from_disk(self: &Rc<Self>, paths: Vec<std::path::PathBuf>, as_: AttachAs) {
        if paths.is_empty() {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let read = crate::runtime::runtime().spawn_blocking(move || {
                paths
                    .iter()
                    .map(|path| match as_ {
                        AttachAs::Image => image_attachment(path),
                        AttachAs::Text => text_attachment(path),
                        AttachAs::Either => {
                            image_attachment(path).or_else(|_| text_attachment(path))
                        }
                    })
                    .map(|result| result.map_err(|e| e.to_string()))
                    .collect::<Vec<_>>()
            });
            let Ok(results) = read.await else { return };
            let Some(composer) = weak.upgrade() else {
                return;
            };
            for result in results {
                match result {
                    Ok((label, block)) => composer.add_attachment(label, block),
                    Err(e) => composer.notice(format!("cannot attach: {e}")),
                }
            }
        });
    }

    fn attach_via_dialog(self: &Rc<Self>, image: bool) {
        let Some(window) = self
            .widget
            .root()
            .and_then(|r| r.downcast::<gtk::Window>().ok())
        else {
            return;
        };
        let weak = Rc::downgrade(self);
        gtk::FileDialog::new().open(Some(&window), gtk::gio::Cancellable::NONE, move |result| {
            let Some(composer) = weak.upgrade() else {
                return;
            };
            let Ok(file) = result else { return };
            let Some(path) = file.path() else { return };
            composer.attach_from_disk(
                vec![path],
                if image {
                    AttachAs::Image
                } else {
                    AttachAs::Text
                },
            );
        });
    }

    // --- voice -----------------------------------------------------------

    fn wire_mic(self: &Rc<Self>, mic: &gtk::Button) {
        // The button's own click is not the gesture: a hold has a beginning
        // and an end, and GtkButton reports only the middle. Capture phase,
        // so the press reaches this before the button claims it.
        let gesture = gtk::GestureClick::new();
        gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let weak = Rc::downgrade(self);
            gesture.connect_pressed(move |gesture, _, _, _| {
                if let Some(composer) = weak.upgrade() {
                    composer.mic_pressed();
                }
                gesture.set_state(gtk::EventSequenceState::Claimed);
            });
        }
        {
            let weak = Rc::downgrade(self);
            gesture.connect_released(move |_, _, _, _| {
                if let Some(composer) = weak.upgrade() {
                    composer.mic_released();
                }
            });
        }
        {
            let weak = Rc::downgrade(self);
            gesture.connect_cancel(move |_, _| {
                if let Some(composer) = weak.upgrade() {
                    composer.mic_released();
                }
            });
        }
        mic.add_controller(gesture);
        // Keyboard: Space or Enter on the focused button toggles.
        let weak = Rc::downgrade(self);
        mic.connect_clicked(move |_| {
            let Some(composer) = weak.upgrade() else {
                return;
            };
            let recording = matches!(*composer.voice.borrow(), Voice::Recording { .. });
            if recording {
                composer.stop_recording();
            } else if composer.mic_is_idle() {
                composer.start_recording();
            }
        });
    }

    /// The hotkey's gesture: start dictating into this field, or stop and
    /// transcribe if it is already listening. The field takes focus so the
    /// words land somewhere the user is looking.
    pub fn toggle_dictation(self: &Rc<Self>) {
        let recording = matches!(*self.voice.borrow(), Voice::Recording { .. });
        if recording {
            self.stop_recording();
        } else if self.mic_is_idle() {
            self.start_recording();
            self.entry.grab_focus();
        } else {
            self.notice("still transcribing the last recording".into());
        }
    }

    /// A HELD gesture's two ends — Ctrl+D down and up, the controller's X
    /// down and up: start listening, then stop and transcribe. Idempotent at
    /// both ends, so a release with nothing running does nothing.
    pub fn dictate(self: &Rc<Self>, on: bool) {
        let recording = matches!(*self.voice.borrow(), Voice::Recording { .. });
        if on && !recording && self.mic_is_idle() {
            self.start_recording();
            self.entry.grab_focus();
        } else if !on && recording {
            self.stop_recording();
        }
    }

    fn mic_is_idle(&self) -> bool {
        matches!(*self.voice.borrow(), Voice::Idle)
    }

    fn mic_pressed(self: &Rc<Self>) {
        let mut voice = self.voice.borrow_mut();
        match &mut *voice {
            Voice::Idle => {
                drop(voice);
                self.start_recording();
            }
            Voice::Recording {
                stop_on_release, ..
            } => *stop_on_release = true,
            Voice::Transcribing => {}
        }
    }

    fn mic_released(self: &Rc<Self>) {
        let stop = match &*self.voice.borrow() {
            Voice::Recording {
                since,
                stop_on_release,
                ..
            } => *stop_on_release || since.elapsed() >= TAP,
            _ => false,
        };
        if stop {
            self.stop_recording();
        }
    }

    fn start_recording(self: &Rc<Self>) {
        // Read before anything moves focus: the mic button takes it on a
        // click, the hotkey hands it to the field afterwards.
        let at_cursor = self.entry.has_focus();
        match crate::voice::readiness() {
            crate::voice::Readiness::Ready => {}
            crate::voice::Readiness::Downloading => {
                self.notice("the speech model is still downloading".into());
                return;
            }
            crate::voice::Readiness::Absent => {
                // The meter beside the microphone is the download's
                // progress bar; only the start and the end are sentences.
                let weak = Rc::downgrade(self);
                crate::voice::fetch_model(move |progress| {
                    let Some(composer) = weak.upgrade() else {
                        return;
                    };
                    use crate::voice::Progress;
                    match progress {
                        Progress::Started => {
                            composer.mic.set_sensitive(false);
                            composer.level.set_value(0.0);
                            composer.level.add_css_class("downloading");
                            composer.level.set_visible(true);
                            composer.notice(format!(
                                "Downloading the speech model ({}, {} MB) — the meter beside \
                                 the microphone is its progress",
                                crate::voice::MODEL.name,
                                crate::voice::MODEL.bytes / (1024 * 1024)
                            ));
                        }
                        Progress::Bytes { done, total } => {
                            let fraction = if total > 0 {
                                done as f64 / total as f64
                            } else {
                                0.0
                            };
                            composer.level.set_value(fraction);
                            composer.level.set_tooltip_text(Some(&format!(
                                "Downloading the speech model — {:.0}% ({} of {} MB)",
                                fraction * 100.0,
                                done / (1024 * 1024),
                                total / (1024 * 1024)
                            )));
                        }
                        Progress::Done | Progress::Failed(_) => {
                            composer.level.set_visible(false);
                            composer.level.remove_css_class("downloading");
                            composer.level.set_tooltip_text(None);
                            composer.mic.set_sensitive(true);
                            composer.notice(match progress {
                                Progress::Done => {
                                    "Speech model ready — hold the microphone to talk".to_string()
                                }
                                Progress::Failed(e) => {
                                    format!("The speech model could not be fetched: {e}")
                                }
                                _ => unreachable!(),
                            });
                        }
                    }
                });
                return;
            }
        }
        match taste_voice::Recorder::start() {
            Ok(recorder) => {
                *self.voice.borrow_mut() = Voice::Recording {
                    recorder,
                    since: Instant::now(),
                    stop_on_release: false,
                    at_cursor,
                };
                self.mic.add_css_class("recording");
                self.level.set_value(0.0);
                self.level.set_visible(true);
                let weak = Rc::downgrade(self);
                glib::timeout_add_local(Duration::from_millis(50), move || {
                    let Some(composer) = weak.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    let (level, over) = match &*composer.voice.borrow() {
                        Voice::Recording {
                            recorder, since, ..
                        } => (recorder.level(), since.elapsed() >= MAX_RECORDING),
                        _ => return glib::ControlFlow::Break,
                    };
                    // RMS of speech sits around 0.05–0.2; scale so a normal
                    // voice fills most of the bar.
                    composer.level.set_value(f64::from((level * 6.0).min(1.0)));
                    if over {
                        composer.stop_recording();
                        return glib::ControlFlow::Break;
                    }
                    glib::ControlFlow::Continue
                });
            }
            Err(e) => self.notice(format!("{e:#}")),
        }
    }

    fn stop_recording(self: &Rc<Self>) {
        let taken = std::mem::replace(&mut *self.voice.borrow_mut(), Voice::Transcribing);
        let Voice::Recording {
            recorder,
            at_cursor,
            ..
        } = taken
        else {
            *self.voice.borrow_mut() = taken;
            return;
        };
        self.mic.remove_css_class("recording");
        self.level.set_visible(false);
        let samples = recorder.stop();
        if !taste_voice::has_speech(&samples) {
            *self.voice.borrow_mut() = Voice::Idle;
            return;
        }
        self.mic.set_sensitive(false);
        let weak = Rc::downgrade(self);
        crate::voice::transcribe(samples, move |result| {
            let Some(composer) = weak.upgrade() else {
                return;
            };
            *composer.voice.borrow_mut() = Voice::Idle;
            composer.mic.set_sensitive(true);
            match result {
                Ok(text) if !text.is_empty() => composer.insert_spoken(&text, at_cursor),
                Ok(_) => {}
                Err(e) => composer.notice(format!("could not transcribe: {e}")),
            }
        });
    }

    /// Spoken words land at the cursor — or, when the field was not being
    /// edited, at the end with the cursor after them, so Enter sends what
    /// was just said — with the spacing a typist would have left, and the
    /// field takes focus so the next keystroke edits.
    fn insert_spoken(&self, text: &str, at_cursor: bool) {
        let buffer = self.entry.buffer();
        let mut cursor = if at_cursor {
            buffer.iter_at_mark(&buffer.get_insert())
        } else {
            buffer.end_iter()
        };
        let before = {
            let start = buffer.start_iter();
            buffer.text(&start, &cursor, true).to_string()
        };
        let needs_space = before.chars().last().is_some_and(|c| !c.is_whitespace());
        let mut spoken = if needs_space {
            format!(" {text}")
        } else {
            text.to_string()
        };
        // Spoken into the middle of a sentence, the words part from what
        // follows too — and the cursor stays after the words, not after
        // the space.
        let end = buffer.end_iter();
        let after_cursor = buffer.text(&cursor, &end, true).to_string();
        let trailing_space = after_cursor
            .chars()
            .next()
            .is_some_and(|c| !c.is_whitespace());
        if trailing_space {
            spoken.push(' ');
        }
        buffer.insert(&mut cursor, &spoken);
        if trailing_space {
            cursor.backward_char();
        }
        buffer.place_cursor(&cursor);
        self.entry.grab_focus();
    }
}

/// Grow the field with its text up to `MAX_LINES`, then scroll inside.
fn grow_to_fit(entry: &sourceview5::View, scroller: &gtk::ScrolledWindow) {
    let measured_entry = entry.clone();
    let scroller = scroller.clone();
    let adjustment = scroller.vadjustment();
    let queued = Rc::new(Cell::new(false));
    let fit = Rc::new(move |adjustment: &gtk::Adjustment| {
        let visible = adjustment.page_size();
        if visible <= 0.0 {
            return; // not allocated yet
        }
        let metrics = measured_entry.pango_context().metrics(None, None);
        let line = match metrics.height() {
            h if h > 0 => h / gtk::pango::SCALE,
            _ => (metrics.ascent() + metrics.descent()) / gtk::pango::SCALE,
        };
        let floor = line + 24; // the view's top and bottom margins
        let ceiling = line * MAX_LINES + 24;
        if scroller.max_content_height() != ceiling {
            scroller.set_max_content_height(ceiling);
        }
        let overflow = (adjustment.upper() - visible).ceil() as i32;
        if overflow == 0 {
            return;
        }
        let current = scroller.min_content_height();
        let target = (current + overflow).clamp(floor, ceiling);
        if target != current {
            scroller.set_min_content_height(target);
        }
    });
    adjustment.connect_changed(move |adjustment| {
        // Coalesce: the adjustment changes several times per keystroke,
        // and resizing inside its own notification is how the old code
        // ended up a frame behind.
        if queued.replace(true) {
            return;
        }
        let adjustment = adjustment.clone();
        let queued = queued.clone();
        let fit = fit.clone();
        glib::idle_add_local_once(move || {
            queued.set(false);
            fit(&adjustment);
        });
    });
}

pub fn text_attachment(path: &std::path::Path) -> anyhow::Result<(String, ContentBlock)> {
    let meta = std::fs::metadata(path)?;
    anyhow::ensure!(
        meta.len() <= MAX_TEXT_ATTACHMENT_BYTES,
        "{} is larger than {}KB",
        path.display(),
        MAX_TEXT_ATTACHMENT_BYTES / 1024
    );
    let text = std::fs::read_to_string(path)
        .map_err(|_| anyhow::anyhow!("{} is not text", path.display()))?;
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let uri = format!("file://{}", path.display());
    let block = ContentBlock::Resource(EmbeddedResource::new(
        EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(text, uri)),
    ));
    Ok((label, block))
}

pub fn image_attachment(path: &std::path::Path) -> anyhow::Result<(String, ContentBlock)> {
    use base64::Engine;
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        other => anyhow::bail!("unsupported image type: {other:?}"),
    };
    let meta = std::fs::metadata(path)?;
    anyhow::ensure!(
        meta.len() <= MAX_IMAGE_ATTACHMENT_BYTES,
        "{} is larger than {}MB",
        path.display(),
        MAX_IMAGE_ATTACHMENT_BYTES / (1024 * 1024)
    );
    let bytes = std::fs::read(path)?;
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok((label, ContentBlock::Image(ImageContent::new(data, mime))))
}

pub fn image_thumbnail(texture: &gtk::gdk::Texture) -> gtk::Picture {
    let picture = gtk::Picture::for_paintable(texture);
    picture.set_content_fit(gtk::ContentFit::Cover);
    picture.set_size_request(ATTACHMENT_THUMBNAIL_PX, ATTACHMENT_THUMBNAIL_PX);
    picture
}

pub fn decode_image(image: &ImageContent) -> Option<gtk::gdk::Texture> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .ok()?;
    gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).ok()
}

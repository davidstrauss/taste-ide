//! **A file dragged out of the tree, to a browser, a file manager, or the
//! chat's composer.**
//!
//! What a drop needs is a file on this host, and the tree lists a checkout
//! that is usually in a VM. Where the folder holds the same file — the
//! primary's checkout, mirrored into the folder the user opened less what
//! git ignores (`taste_git::mirror`) — the drag carries that path, as a
//! file manager's would. Where it does not — another environment's
//! checkout, or an ignored file that does not sync — the drag carries a
//! promise, and the file is copied out of the VM when the drop asks for it
//! ([`FileDrag`]), into a folder of the IDE's own under the user cache, so
//! nothing is fetched for a drag that is let go of over nothing.
//!
//! Either way the drop is served by GTK's own serializers for a file list
//! (`text/uri-list`, and the document portal's transfer for a sandboxed
//! receiver), which is what a browser's upload, a file manager, and the
//! composer's `FileList` drop target all read.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use taste_core::files::Files;

/// The most a drag copies out of a VM: the copy is held in memory on its
/// way (`Files::read`), and a drop that wanted a disk image is better told
/// so than served by a minute's stall.
const MOST: u64 = 1 << 30;

/// How long a copy made for a drop is kept: long enough for the receiver to
/// read it — a browser reads an upload when the form is sent, not at the
/// drop — and then collected by the next drag.
const KEPT: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// What a row's drag carries: the file on this host as it is, or one in a
/// checkout this host reaches only through its files service.
pub enum Source {
    Host(PathBuf),
    Remote { files: Files, path: PathBuf },
}

/// The drag's content for `source`.
pub fn provider(source: Source) -> gdk::ContentProvider {
    match source {
        Source::Host(path) => gdk::ContentProvider::for_value(
            &gdk::FileList::from_array(&[gio::File::for_path(path)]).to_value(),
        ),
        Source::Remote { files, path } => FileDrag::new(files, path).upcast(),
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct FileDrag {
        pub(super) from: RefCell<Option<(Files, PathBuf)>>,
        /// The copy, once made: every format a drop asks for is served
        /// from the one copy, and a second asking while the first is under
        /// way waits for it.
        pub(super) staged: Rc<tokio::sync::OnceCell<PathBuf>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FileDrag {
        const NAME: &'static str = "TasteFileDrag";
        type Type = super::FileDrag;
        type ParentType = gdk::ContentProvider;
    }

    impl ObjectImpl for FileDrag {}

    impl ContentProviderImpl for FileDrag {
        /// A file list's MIME types, and not its type: a drop inside the
        /// IDE asks for the type by value, which has to be there at once,
        /// so it is sent the URI list instead and waits for the copy as a
        /// browser does.
        fn formats(&self) -> gdk::ContentFormats {
            let list = gdk::ContentFormats::for_type(gdk::FileList::static_type())
                .union_serialize_mime_types();
            list.mime_types()
                .iter()
                .fold(gdk::ContentFormatsBuilder::new(), |builder, mime| {
                    builder.add_mime_type(mime)
                })
                .build()
        }

        fn write_mime_type_future(
            &self,
            mime_type: &str,
            stream: &gio::OutputStream,
            io_priority: glib::Priority,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), glib::Error>> + 'static>>
        {
            let (staged, from) = (self.staged.clone(), self.from.borrow().clone());
            let (mime_type, stream) = (mime_type.to_string(), stream.clone());
            Box::pin(async move {
                let path = staged
                    .get_or_try_init(|| async move {
                        let (files, path) = from.ok_or_else(|| "nothing to copy".to_string())?;
                        crate::runtime::runtime()
                            .spawn_blocking(move || stage(&files, &path))
                            .await
                            .map_err(|e| e.to_string())?
                    })
                    .await
                    .map_err(|e| glib::Error::new(gio::IOErrorEnum::Failed, &e))?
                    .clone();
                // GDK's own serializers for a file list, as a drag of a
                // value-backed provider is served — the provider for a value
                // writes no MIME type itself.
                let list = gdk::FileList::from_array(&[gio::File::for_path(path)]);
                gdk::content_serialize_future(&stream, &mime_type, &list.to_value(), io_priority)
                    .await
            })
        }
    }
}

glib::wrapper! {
    /// A file in a checkout this host reaches through its files service,
    /// copied out when a drop asks for it.
    pub struct FileDrag(ObjectSubclass<imp::FileDrag>) @extends gdk::ContentProvider;
}

impl FileDrag {
    fn new(files: Files, path: PathBuf) -> Self {
        let this: Self = glib::Object::new();
        *this.imp().from.borrow_mut() = Some((files, path));
        this
    }
}

/// Where copies for drops go: a folder of the IDE's under the user cache,
/// one folder per copy so each keeps its file's own name — the name a
/// browser's upload sends.
fn drops_dir() -> PathBuf {
    glib::user_cache_dir().join("taste-ide").join("drops")
}

/// Copy `path` out of `files` for a drop, and collect the copies made for
/// drops before it. Blocking.
fn stage(files: &Files, path: &Path) -> Result<PathBuf, String> {
    let name = path
        .file_name()
        .ok_or_else(|| format!("{} is not a file", path.display()))?;
    let size = files.stat(path).map_err(|e| e.to_string())?.size;
    if size > MOST {
        return Err(format!(
            "{} is {} MiB; a drag copies at most {} MiB out of a VM",
            path.display(),
            size >> 20,
            MOST >> 20
        ));
    }
    let bytes = files.read(path).map_err(|e| e.to_string())?;
    let root = drops_dir();
    collect(&root);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = root.join(format!("{stamp:x}"));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let copy = dir.join(name);
    std::fs::write(&copy, bytes).map_err(|e| e.to_string())?;
    Ok(copy)
}

/// Remove the copies older than [`KEPT`].
fn collect(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > KEPT);
        if old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A copy keeps its file's name, in a folder of its own, and the bytes
    /// are the file's.
    #[test]
    fn a_copy_for_a_drop_keeps_the_name_and_the_bytes() {
        let dir = std::env::temp_dir().join(format!("taste-drag-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("deck.pdf");
        std::fs::write(&file, b"%PDF-1.7").unwrap();
        let copy = stage(&Files::Local, &file).unwrap();
        assert_eq!(copy.file_name().unwrap(), "deck.pdf");
        assert_eq!(std::fs::read(&copy).unwrap(), b"%PDF-1.7");
        assert_ne!(copy, file);
        std::fs::remove_dir_all(copy.parent().unwrap()).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// What a drop reads from a file in a checkout this host reaches only
    /// through its files service: a URI list naming a copy of the file, made
    /// when the drop asked — and the same copy for a second asking.
    #[test]
    fn a_drop_reads_a_copy_made_when_it_asks() {
        crate::gtk_test::on_gtk_thread("file drag: no display — skipped", || {
            let dir = std::env::temp_dir().join(format!("taste-drag-drop-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join("notes.md");
            std::fs::write(&file, b"# notes").unwrap();
            let provider = provider(Source::Remote {
                files: Files::Local,
                path: file.clone(),
            });
            let mimes = provider.formats().mime_types();
            assert!(mimes.iter().any(|m| m == "text/uri-list"), "{mimes:?}");
            assert!(!provider
                .formats()
                .contains_type(gdk::FileList::static_type()));
            let read = || {
                let stream = gio::MemoryOutputStream::new_resizable();
                glib::MainContext::default()
                    .block_on(provider.write_mime_type_future(
                        "text/uri-list",
                        &stream,
                        glib::Priority::DEFAULT,
                    ))
                    .unwrap();
                stream.close(gio::Cancellable::NONE).unwrap();
                String::from_utf8(stream.steal_as_bytes().to_vec()).unwrap()
            };
            let first = read();
            let uri = first.lines().next().unwrap().trim().to_string();
            let copy = gio::File::for_uri(&uri).path().unwrap();
            assert_ne!(copy, file);
            assert_eq!(copy.file_name().unwrap(), "notes.md");
            assert_eq!(std::fs::read(&copy).unwrap(), b"# notes");
            assert_eq!(read(), first, "one copy serves every asking");
            std::fs::remove_dir_all(copy.parent().unwrap()).unwrap();
            std::fs::remove_dir_all(&dir).unwrap();
        });
    }
}

//! Images decoded through glycin, the GNOME platform's image loading: each
//! format's loader runs in a process of its own, sandboxed — bwrap outside
//! a Flatpak, `flatpak-spawn --sandbox` inside one, and seccomp in either —
//! and hands back pixels. GDK's own `Texture::from_bytes` decodes in the
//! calling process, with libpng, libjpeg, libtiff, or gdk-pixbuf's loaders,
//! and the bytes here are what a repository or an agent wrote: an image
//! file, a picture in a preview, an attachment in a transcript. A decoder
//! bug they could drive would run with everything the IDE's process holds
//! (CLAUDE.md → "Third-party code that parses untrusted content runs in a
//! confined helper"). The sandbox is glycin's rather than `taste_confine`'s
//! because it is the platform's own, and inside the Flatpak it is the
//! nested sandbox Flatpak itself provides.
//!
//! libglycin-2's C API, through the handful of calls a still frame needs.
//! The library ships beside the loaders it talks to (the GNOME runtime, the
//! devcontainer image's `glycin-libs`), so the two never disagree on the
//! protocol the way a client compiled into this binary could.
//!
//! Blocking, since every load starts a loader: [`decode`] runs on the
//! blocking pool, and [`decode_later`] is the main thread's way in.

use std::cell::RefCell;
use std::collections::VecDeque;

use gtk::gdk;
use gtk::glib;
use gtk::glib::translate::{from_glib_full, from_glib_none, ToGlibPtr};
use gtk::prelude::*;

mod ffi {
    use gtk::glib::ffi::{GBytes, GError};
    use std::ffi::{c_int, c_uint};

    #[repr(C)]
    pub struct GlyLoader {
        _private: [u8; 0],
    }
    #[repr(C)]
    pub struct GlyImage {
        _private: [u8; 0],
    }
    #[repr(C)]
    pub struct GlyFrame {
        _private: [u8; 0],
    }

    /// `GLY_MEMORY_SELECTION_R8G8B8A8_PREMULTIPLIED`: the one format asked
    /// for, which glycin converts every other into.
    pub const SELECT_R8G8B8A8_PREMULTIPLIED: c_uint = 1 << 2;
    /// `GLY_MEMORY_R8G8B8A8_PREMULTIPLIED`, what a frame then says it is.
    pub const R8G8B8A8_PREMULTIPLIED: c_int = 2;

    /// `GLY_SANDBOX_SELECTOR_NOT_SANDBOXED`, which only the tests ask for,
    /// and only where the sandbox cannot start ([`super::tests`]).
    #[cfg(test)]
    pub const NOT_SANDBOXED: c_int = 3;

    #[link(name = "glycin-2")]
    extern "C" {
        pub fn gly_loader_new_for_bytes(bytes: *mut GBytes) -> *mut GlyLoader;
        #[cfg(test)]
        pub fn gly_loader_set_sandbox_selector(loader: *mut GlyLoader, selector: c_int);
        pub fn gly_loader_set_accepted_memory_formats(loader: *mut GlyLoader, selection: c_uint);
        pub fn gly_loader_load(loader: *mut GlyLoader, error: *mut *mut GError) -> *mut GlyImage;
        pub fn gly_image_next_frame(image: *mut GlyImage, error: *mut *mut GError)
            -> *mut GlyFrame;
        pub fn gly_frame_get_width(frame: *mut GlyFrame) -> u32;
        pub fn gly_frame_get_height(frame: *mut GlyFrame) -> u32;
        pub fn gly_frame_get_stride(frame: *mut GlyFrame) -> u32;
        pub fn gly_frame_get_memory_format(frame: *mut GlyFrame) -> c_int;
        pub fn gly_frame_get_buf_bytes(frame: *mut GlyFrame) -> *mut GBytes;
    }
}

/// A GObject this module was handed in full, released when it goes.
struct Owned<T>(*mut T);

impl<T> Drop for Owned<T> {
    fn drop(&mut self) {
        // SAFETY: every `Owned` holds a reference returned `transfer full`
        // and not yet released.
        unsafe { glib::gobject_ffi::g_object_unref(self.0.cast()) }
    }
}

/// The error a failed call filled in, as words.
///
/// # Safety
/// `error` is a `GError` the call handed over in full, or null.
unsafe fn reason(error: *mut glib::ffi::GError) -> String {
    if error.is_null() {
        return "glycin gave no reason".into();
    }
    let error: glib::Error = from_glib_full(error);
    error.message().to_string()
}

/// The first frame of the image `bytes` hold, decoded by glycin's loader
/// for its format, in glycin's sandbox. Blocking: run on the blocking pool.
pub fn decode(bytes: &glib::Bytes) -> Result<gdk::Texture, String> {
    decode_with(bytes, false)
}

/// [`decode`], or with glycin's sandbox off when `unsandboxed`, which only
/// the tests can ask for.
fn decode_with(bytes: &glib::Bytes, unsandboxed: bool) -> Result<gdk::Texture, String> {
    // SAFETY: each call gets a live object of the type it takes; what comes
    // back `transfer full` is owned by an `Owned` from that moment, and the
    // frame's buffer, `transfer none`, is referenced before the frame goes.
    unsafe {
        let loader = Owned(ffi::gly_loader_new_for_bytes(
            ToGlibPtr::<*const glib::ffi::GBytes>::to_glib_none(bytes).0 as *mut _,
        ));
        ffi::gly_loader_set_accepted_memory_formats(loader.0, ffi::SELECT_R8G8B8A8_PREMULTIPLIED);
        #[cfg(test)]
        if unsandboxed {
            ffi::gly_loader_set_sandbox_selector(loader.0, ffi::NOT_SANDBOXED);
        }
        #[cfg(not(test))]
        let _ = unsandboxed;
        let mut error = std::ptr::null_mut();
        let image = ffi::gly_loader_load(loader.0, &mut error);
        if image.is_null() {
            return Err(reason(error));
        }
        let image = Owned(image);
        let frame = ffi::gly_image_next_frame(image.0, &mut error);
        if frame.is_null() {
            return Err(reason(error));
        }
        let frame = Owned(frame);
        let format = ffi::gly_frame_get_memory_format(frame.0);
        if format != ffi::R8G8B8A8_PREMULTIPLIED {
            return Err(format!(
                "glycin answered memory format {format}, not the one asked for"
            ));
        }
        let (width, height) = (
            ffi::gly_frame_get_width(frame.0),
            ffi::gly_frame_get_height(frame.0),
        );
        let stride = ffi::gly_frame_get_stride(frame.0) as usize;
        let pixels: glib::Bytes = from_glib_none(ffi::gly_frame_get_buf_bytes(frame.0));
        let (Ok(w), Ok(h)) = (i32::try_from(width), i32::try_from(height)) else {
            return Err(format!("{width}×{height} is larger than a texture holds"));
        };
        if w == 0
            || h == 0
            || stride < width as usize * 4
            || pixels.len() < stride * height as usize
        {
            return Err(format!(
                "glycin answered {width}×{height} at stride {stride} in {} bytes",
                pixels.len()
            ));
        }
        Ok(gdk::MemoryTexture::new(
            w,
            h,
            gdk::MemoryFormat::R8g8b8a8Premultiplied,
            &pixels,
            stride,
        )
        .upcast())
    }
}

/// How many decoded pictures the main thread keeps: a transcript's rows
/// are built again as they scroll back into view, and each would otherwise
/// start a loader again.
const KEPT: usize = 32;

thread_local! {
    static DECODED: RefCell<VecDeque<(u64, gdk::Texture)>> = const { RefCell::new(VecDeque::new()) };
}

/// Decode on the blocking pool what `bytes` produces there, and hand the
/// result to `apply` on the main thread. `key` names the bytes (a hash of
/// them, or of what they are decoded from): a picture decoded under the
/// same key lately is handed over at once, before this returns.
pub fn decode_later(
    key: u64,
    bytes: impl FnOnce() -> Result<glib::Bytes, String> + Send + 'static,
    apply: impl FnOnce(Result<gdk::Texture, String>) + 'static,
) {
    let kept = DECODED.with(|kept| {
        kept.borrow()
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, texture)| texture.clone())
    });
    if let Some(texture) = kept {
        apply(Ok(texture));
        return;
    }
    glib::spawn_future_local(async move {
        let decoded = crate::runtime::runtime()
            .spawn_blocking(move || decode(&bytes()?))
            .await
            .unwrap_or_else(|e| Err(format!("the decode did not finish: {e}")));
        if let Ok(texture) = &decoded {
            DECODED.with(|kept| {
                let mut kept = kept.borrow_mut();
                if kept.len() == KEPT {
                    kept.pop_front();
                }
                kept.push_back((key, texture.clone()));
            });
        }
        apply(decoded);
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// [`decode`] as the tests run it: in glycin's sandbox wherever it
    /// starts, and otherwise with it off, saying so. It does not start
    /// inside a container — this repository's devcontainer, CI's — because
    /// bwrap cannot mount a fresh `/proc` and devpts under rootless
    /// podman's masks and SELinux label; the IDE runs on the host or in
    /// its Flatpak, where it does (`flatpak-spawn --sandbox` there). What
    /// the fallback still checks is everything of this module's own: the
    /// calls, the format asked for, the stride, and the texture.
    pub(crate) fn decode_here(bytes: &glib::Bytes) -> Result<gdk::Texture, String> {
        match decode(bytes) {
            Err(why) if why.contains("Loader process exited early") => {
                eprintln!("glycin's sandbox does not start here ({why}); decoding without it");
                decode_with(bytes, true)
            }
            decoded => decoded,
        }
    }

    /// A two-by-one PNG, red then half-transparent blue, through a real
    /// loader: the pixels arrive premultiplied, at the size the file says.
    /// Needs glycin's loaders and a sandbox that starts, as the IDE does.
    #[test]
    fn a_png_decodes_through_the_loader() {
        let pixels = glib::Bytes::from_static(&[255, 0, 0, 255, 0, 0, 255, 128]);
        let png = gdk::MemoryTexture::new(2, 1, gdk::MemoryFormat::R8g8b8a8, &pixels, 8)
            .save_to_png_bytes();
        let texture = decode_here(&png).unwrap();
        assert_eq!((texture.width(), texture.height()), (2, 1));
        let mut downloaded = [0u8; 8];
        texture.download(&mut downloaded, 8);
        // GDK downloads as premultiplied BGRA (`GDK_MEMORY_DEFAULT`).
        assert_eq!(&downloaded[..4], &[0, 0, 255, 255]);
        assert_eq!(downloaded[7], 128);
        assert!(
            downloaded[4] >= 127 && downloaded[4] <= 129,
            "{downloaded:?}"
        );
    }

    #[test]
    fn what_is_not_an_image_is_refused_with_glycins_reason() {
        let why = decode_here(&glib::Bytes::from_static(b"not an image")).unwrap_err();
        assert!(!why.is_empty());
    }
}

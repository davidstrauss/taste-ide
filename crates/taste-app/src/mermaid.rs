//! Mermaid diagrams in rendered Markdown: a ```mermaid block is drawn as
//! the diagram it describes (David, 2026-10-09: "I want Mermaid diagrams in
//! Markdown to render in preview").
//!
//! **Drawn in a confined helper, never in this process.** The renderer —
//! merman for the layout and the SVG, resvg for the pixels, and their
//! dependencies, about eighty crates — parses text a repository or an
//! agent wrote, and this process holds the user's home, the network, and
//! the portal that runs host commands. So it lives in `taste-diagram`,
//! which `taste-confine` locks down before its first instruction: the
//! system's files and the fonts to read, nothing to write, no socket, no
//! signal out. A hostile dependency or a parser bug an input can drive owns
//! that process and nothing more (David, 2026-10-09: "Build the sandboxed
//! helper first, then add Mermaid"). What comes back is checked before it
//! is believed: a size inside the cap, exactly the bytes that size needs.
//! A kernel that cannot confine it gets no diagrams — the code block,
//! and the reason — rather than an unconfined renderer.
//!
//! **Pictures, not a web page.** The preview is GTK widgets, so a diagram
//! arrives as a picture beside the others; a WebKit view per diagram would
//! be a web process, and a scroll trap, per code block. The helper sets
//! the diagram in the window's interface font, in libadwaita's palette for
//! the light or dark the window is in (taste-diagram's render.rs).
//!
//! One helper, kept running and asked one diagram at a time; a reply that
//! does not come in time kills it, and the next diagram starts another.
//! Finished pictures are cached by source, theme, face, and scale, so a
//! preview that redraws on every edit redraws an unchanged diagram from
//! memory, without the flash of an empty box.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

/// Whether a fenced block's info string names Mermaid.
pub fn is_mermaid(info: &str) -> bool {
    info.split_whitespace()
        .next()
        .is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid"))
}

/// The longest side of a picture the helper may send: its own cap
/// (`taste_diagram`'s `MAX_RASTER_SIDE`). Anything larger is a lie about
/// the size, and the reply is refused before a byte of it is allocated.
const MAX_SIDE: u64 = 8192;

/// How long the helper has to answer: its own deadline is thirty seconds a
/// diagram, and a reply that has not come in fifteen past that is a
/// helper stuck.
const REPLY_DEADLINE: Duration = Duration::from_secs(45);

/// A drawn diagram: premultiplied RGBA at `width`×`height` pixels, which
/// stand for `logical_width` at the scale it was drawn for.
struct Raster {
    width: u32,
    height: u32,
    logical_width: f32,
    rgba: Vec<u8>,
}

/// Where the helper is: beside this executable (`target/debug` in
/// development, `/app/bin` in the Flatpak), or wherever
/// `TASTE_DIAGRAM_BIN` names.
fn helper_path() -> Result<PathBuf, String> {
    if let Some(named) = std::env::var_os("TASTE_DIAGRAM_BIN") {
        return Ok(PathBuf::from(named));
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("taste-diagram")))
        .filter(|path| path.is_file())
        .ok_or_else(|| "the diagram helper (taste-diagram) is not installed beside the IDE".into())
}

/// The running helper and its two pipes.
struct Helper {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The one helper, started on first use.
static HELPER: Mutex<Option<Helper>> = Mutex::new(None);

impl Helper {
    fn start(family: &str) -> Result<Self, String> {
        let program = helper_path()?;
        let policy = taste_confine::Policy::for_program(&program)
            .map_err(|e| format!("{e:#}"))?
            .with_fonts();
        let mut command = Command::new(&program);
        command
            .arg(family)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        taste_confine::confine(&mut command, &policy).map_err(|e| format!("{e:#}"))?;
        let mut child = command
            .spawn()
            .map_err(|e| format!("starting {}: {e}", program.display()))?;
        let stdin = child.stdin.take().ok_or("the helper has no stdin")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("the helper has no stdout")?);
        let mut helper = Self {
            child,
            stdin,
            stdout,
        };
        let ready = helper.within(REPLY_DEADLINE, |helper| helper.line())?;
        if ready.get("ready").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(error_of(&ready)
                .unwrap_or("the helper did not start")
                .to_string());
        }
        Ok(helper)
    }

    /// Run `read` with the helper killed if it has not finished by
    /// `deadline`: the read then ends at the pipe's close. The process is
    /// not waited on until it is dropped, so its pid cannot be another's.
    fn within<T, E>(
        &mut self,
        deadline: Duration,
        read: impl FnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<T, E> {
        let done = Arc::new(AtomicBool::new(false));
        let pid = self.child.id() as libc::pid_t;
        {
            let done = done.clone();
            std::thread::spawn(move || {
                std::thread::sleep(deadline);
                if !done.load(Ordering::SeqCst) {
                    // SAFETY: a signal to our own unreaped child.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            });
        }
        let result = read(self);
        done.store(true, Ordering::SeqCst);
        result
    }

    fn line(&mut self) -> Result<serde_json::Value, String> {
        let mut line = String::new();
        // A reply line is a small JSON object; a line longer than this is
        // not one.
        let read = (&mut self.stdout)
            .take(64 * 1024)
            .read_line(&mut line)
            .map_err(|e| format!("reading the helper: {e}"))?;
        if read == 0 {
            return Err("the diagram helper stopped".into());
        }
        serde_json::from_str(&line)
            .map_err(|_| "the diagram helper said something unreadable".into())
    }

    /// One diagram. The error says whether the helper is still in step:
    /// a refusal it SAID leaves it ready for the next request; anything
    /// else (no reply, an unreadable one, a size refused with its bytes
    /// still in the pipe) does not.
    fn draw(
        &mut self,
        source: &str,
        dark: bool,
        family: &str,
        scale: f32,
    ) -> Result<Raster, (String, bool)> {
        let request = serde_json::json!({
            "source": source, "dark": dark, "family": family, "scale": scale,
        });
        writeln!(self.stdin, "{request}")
            .and_then(|()| self.stdin.flush())
            .map_err(|e| (format!("writing to the diagram helper: {e}"), false))?;
        self.within(REPLY_DEADLINE, |helper| {
            let header = helper.line().map_err(|why| (why, false))?;
            if let Some(why) = error_of(&header) {
                return Err((why.to_string(), true));
            }
            let (width, height, logical_width) =
                checked_size(&header).map_err(|why| (why, false))?;
            let mut rgba = vec![0u8; (width * height * 4) as usize];
            helper
                .stdout
                .read_exact(&mut rgba)
                .map_err(|e| (format!("reading the diagram: {e}"), false))?;
            Ok(Raster {
                width: width as u32,
                height: height as u32,
                logical_width,
                rgba,
            })
        })
    }
}

fn error_of(reply: &serde_json::Value) -> Option<&str> {
    reply.get("error").and_then(serde_json::Value::as_str)
}

/// The size a reply claims, if it is one this side will allocate: whole
/// pixels inside the cap on both sides, and a finite width to show it at.
fn checked_size(header: &serde_json::Value) -> Result<(u64, u64, f32), String> {
    let side = |key: &str| {
        header
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .filter(|side| (1..=MAX_SIDE).contains(side))
    };
    let logical = header
        .get("logical_width")
        .and_then(serde_json::Value::as_f64)
        .filter(|width| width.is_finite() && *width > 0.0 && *width <= MAX_SIDE as f64)
        .map(|width| width as f32);
    match (side("width"), side("height"), logical) {
        (Some(width), Some(height), Some(logical)) => Ok((width, height, logical)),
        _ => Err("the diagram helper sent a picture of an impossible size".into()),
    }
}

/// Draw through the helper, starting it if it is not running; a helper
/// that fails mid-request is discarded, and the next request starts
/// another. Blocking: called off the main thread.
fn render(source: &str, dark: bool, family: &str, scale: f32) -> Result<Raster, String> {
    let mut slot = HELPER.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        *slot = Some(Helper::start(family)?);
    }
    let helper = slot.as_mut().expect("started above");
    helper
        .draw(source, dark, family, scale)
        .map_err(|(why, in_step)| {
            if !in_step {
                // Gone, or out of step: the next diagram starts a fresh
                // helper rather than read a picture's bytes as a header.
                slot.take();
            }
            why
        })
}

/// What a finished render is cached under.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    source: String,
    dark: bool,
    family: String,
    scale: i32,
}

/// A finished picture: the texture, the size it stands for, and what it
/// holds in memory.
#[derive(Clone)]
struct Drawn {
    texture: gtk::gdk::Texture,
    width: f32,
    bytes: usize,
}

/// How much of finished pictures is kept, in bytes. Each is a texture the
/// GPU holds while it is shown, and counting pictures did not bound that:
/// one document's sixteen diagrams were 586 MB at twice their size
/// (2026-10-09, beside "a device memory allocation has failed").
const CACHE_BYTES: usize = 256 << 20;

thread_local! {
    static CACHE: RefCell<(HashMap<Key, Drawn>, usize)> = RefCell::new((HashMap::new(), 0));
}

fn cached(key: &Key) -> Option<Drawn> {
    CACHE.with(|cache| cache.borrow().0.get(key).cloned())
}

fn keep(key: Key, drawn: Drawn) {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.1 + drawn.bytes > CACHE_BYTES {
            cache.0.clear();
            cache.1 = 0;
        }
        cache.1 += drawn.bytes;
        cache.0.insert(key, drawn);
    });
}

/// The window's interface font, by family: what the diagram is set in.
fn interface_family() -> String {
    gtk::Settings::default()
        .and_then(|settings| settings.gtk_font_name())
        .and_then(|name| gtk::pango::FontDescription::from_string(&name).family())
        .map(|family| family.to_string())
        .unwrap_or_else(|| "Adwaita Sans".to_string())
}

/// The scale to draw at before the box is on a monitor: the largest any
/// monitor has, so a diagram drawn off-screen — the preview builds its
/// next rendering beside the one on screen — is sharp wherever it lands.
fn display_scale() -> i32 {
    gtk::gdk::Display::default()
        .map(|display| {
            let monitors = display.monitors();
            (0..monitors.n_items())
                .filter_map(|i| monitors.item(i).and_downcast::<gtk::gdk::Monitor>())
                .map(|monitor| monitor.scale_factor())
                .max()
                .unwrap_or(1)
        })
        .unwrap_or(1)
        .max(1)
}

/// A ```mermaid block as the diagram it describes. Drawn as soon as it is
/// made, not when it is first shown, so a rendering built off-screen gets
/// whole (`hold`, released once the picture or the reason it failed is in
/// place); again if the monitor it lands on has another scale, and
/// whenever the window goes light or dark. While it draws, the box says
/// so where the diagram will be (David, 2026-10-09: "show that the
/// diagram is generating in the places where they will appear"); if it
/// cannot be drawn, `fallback` — the block as code — stands in its place,
/// with the reason under it.
pub fn diagram(
    source: &str,
    fallback: gtk::Widget,
    hold: Option<crate::markdown_view::Hold>,
) -> gtk::Widget {
    let holder = gtk::Box::new(gtk::Orientation::Vertical, 4);
    holder.set_hexpand(true);
    let source = source.to_string();
    let hold = Rc::new(RefCell::new(hold));
    let drawn_at = Rc::new(std::cell::Cell::new(display_scale()));
    let draw: Rc<dyn Fn(&gtk::Box, i32)> = Rc::new(move |holder: &gtk::Box, scale: i32| {
        let key = Key {
            source: source.clone(),
            dark: adw::StyleManager::default().is_dark(),
            family: interface_family(),
            scale,
        };
        if let Some(drawn) = cached(&key) {
            show(holder, &drawn, &key.source);
            hold.borrow_mut().take();
            return;
        }
        // The first drawing says it is coming; a redraw — the other
        // theme, a sharper scale — keeps the picture it has until the new
        // one replaces it.
        if holder.first_child().is_none() {
            holder.append(&drawing());
        }
        let weak = holder.downgrade();
        let fallback = fallback.clone();
        let hold = hold.clone();
        glib::spawn_future_local(async move {
            let job = key.clone();
            let rendered = crate::runtime::runtime()
                .spawn_blocking(move || {
                    render(&job.source, job.dark, &job.family, job.scale as f32)
                })
                .await
                .unwrap_or_else(|e| Err(format!("the renderer stopped: {e}")));
            let Some(holder) = weak.upgrade() else { return };
            match rendered {
                Ok(raster) => {
                    let bytes = raster.rgba.len();
                    let texture = gtk::gdk::MemoryTexture::new(
                        raster.width as i32,
                        raster.height as i32,
                        gtk::gdk::MemoryFormat::R8g8b8a8Premultiplied,
                        &glib::Bytes::from_owned(raster.rgba),
                        raster.width as usize * 4,
                    );
                    let drawn = Drawn {
                        texture: texture.upcast(),
                        width: raster.logical_width,
                        bytes,
                    };
                    keep(key.clone(), drawn.clone());
                    show(&holder, &drawn, &key.source);
                }
                Err(why) => {
                    tracing::info!("a Mermaid diagram was not drawn: {why}");
                    clear(&holder);
                    holder.append(&fallback);
                    holder.append(
                        &gtk::Label::builder()
                            .label(format!("This Mermaid diagram could not be drawn: {why}"))
                            .xalign(0.0)
                            .wrap(true)
                            .wrap_mode(gtk::pango::WrapMode::WordChar)
                            .css_classes(["caption", "dim-label"])
                            .build(),
                    );
                }
            }
            hold.borrow_mut().take();
        });
    });
    draw(&holder, drawn_at.get());
    {
        let draw = draw.clone();
        let drawn_at = drawn_at.clone();
        holder.connect_realize(move |holder| {
            let scale = holder.scale_factor().max(1);
            if drawn_at.replace(scale) != scale {
                draw(holder, scale);
            }
        });
    }
    // Light and dark are two drawings, not one recoloured: the style
    // manager says when to make the other, for as long as the box exists.
    let style = adw::StyleManager::default();
    let weak = holder.downgrade();
    let handler = style.connect_dark_notify(move |_| {
        if let Some(holder) = weak.upgrade() {
            draw(&holder, drawn_at.get());
        }
    });
    let handler = RefCell::new(Some(handler));
    holder.connect_destroy(move |_| {
        if let Some(handler) = handler.take() {
            adw::StyleManager::default().disconnect(handler);
        }
    });
    holder.upcast()
}

/// What stands where a diagram will be while it is drawn.
fn drawing() -> gtk::Widget {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::Center)
        .margin_top(12)
        .margin_bottom(12)
        .build();
    let spinner = adw::Spinner::new();
    row.append(&spinner);
    row.append(
        &gtk::Label::builder()
            .label("Drawing diagram…")
            .css_classes(["dim-label"])
            .build(),
    );
    row.upcast()
}

fn clear(holder: &gtk::Box) {
    while let Some(child) = holder.first_child() {
        holder.remove(&child);
    }
}

/// The picture in the box: at the size it was drawn for and never larger,
/// scaled down to a narrower column, centred as a figure is.
fn show(holder: &gtk::Box, drawn: &Drawn, source: &str) {
    clear(holder);
    let picture = gtk::Picture::builder()
        .paintable(&drawn.texture)
        .content_fit(gtk::ContentFit::ScaleDown)
        .can_shrink(true)
        .build();
    // What kind of diagram it is, for a screen reader: its first word.
    let kind = source.split_whitespace().next().unwrap_or("Mermaid");
    picture.update_property(&[gtk::accessible::Property::Label(&format!(
        "Diagram ({kind})"
    ))]);
    let clamp = adw::Clamp::builder()
        .maximum_size(drawn.width.ceil() as i32)
        .tightening_threshold(drawn.width.ceil() as i32)
        .child(&picture)
        .build();
    holder.append(&clamp);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mermaid_fence_is_recognised_by_its_first_word() {
        assert!(is_mermaid("mermaid"));
        assert!(is_mermaid("Mermaid {theme: dark}"));
        assert!(!is_mermaid("rust"));
        assert!(!is_mermaid(""));
        assert!(!is_mermaid("mermaidjs"));
    }

    /// The helper's word on how big a picture is decides an allocation in
    /// this process, so a size outside the cap is refused before it is
    /// believed.
    #[test]
    fn a_reply_must_claim_a_size_this_side_will_allocate() {
        let size = |w: serde_json::Value, h: serde_json::Value, l: serde_json::Value| {
            checked_size(&serde_json::json!({ "width": w, "height": h, "logical_width": l }))
        };
        assert_eq!(size(10.into(), 20.into(), 5.0.into()), Ok((10, 20, 5.0)));
        for (w, h, l) in [
            (0.into(), 20.into(), 5.0.into()),
            (10.into(), 8193.into(), 5.0.into()),
            (10.into(), (-1).into(), 5.0.into()),
            (u64::MAX.into(), 20.into(), 5.0.into()),
            (10.into(), 20.into(), 0.0.into()),
            (10.into(), 20.into(), serde_json::Value::Null),
            (10.5.into(), 20.into(), 5.0.into()),
        ] {
            assert!(size(w, h, l).is_err());
        }
    }
}

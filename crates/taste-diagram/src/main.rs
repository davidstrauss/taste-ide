//! `taste-diagram`: Mermaid diagrams, drawn in a process of their own.
//!
//! The renderer is roughly eighty crates of someone else's code parsing
//! text a repository or an agent wrote, and the IDE's own process holds the
//! user's home, the network, and the portal that runs host commands. So it
//! runs here instead, confined by the IDE before its first instruction
//! (`taste-confine`: no file but the system's and the fonts, no write
//! anywhere, no socket, no signal out), and answers over the two pipes it
//! was handed (David, 2026-10-09: "Build the sandboxed helper first, then
//! add Mermaid").
//!
//! **It will not run unconfined.** Before reading a request it checks that
//! it is under `no_new_privs` and a seccomp filter and that Landlock keeps
//! it out of `/`; a launch that skipped confinement — a bug in a caller, or
//! someone running it by hand — gets a refusal, not a renderer.
//!
//! Protocol, on stdin and stdout:
//!   ← `{"ready": true}` once the fonts are read, or `{"error": "…"}`
//!   → `{"source": "…", "dark": bool, "family": "…", "scale": n}`
//!   ← `{"width": w, "height": h, "logical_width": x}` and then exactly
//!     `w * h * 4` bytes of premultiplied RGBA, or `{"error": "…"}`
//! One request at a time; the process lives until stdin closes.

mod render;

use std::io::{BufRead, Write};

use serde::Deserialize;

#[derive(Deserialize)]
struct Request {
    source: String,
    dark: bool,
    family: String,
    scale: f32,
}

/// The longest source accepted: a diagram, not a document.
const MAX_SOURCE: usize = 64 * 1024;

fn main() {
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let say = |out: &mut std::io::BufWriter<std::io::StdoutLock>, value: serde_json::Value| {
        let _ = writeln!(out, "{value}");
        let _ = out.flush();
    };
    if let Err(why) = confined() {
        say(&mut out, serde_json::json!({ "error": why }));
        std::process::exit(2);
    }
    // The interface font the IDE will ask for most; any other is read on
    // first use.
    render::warm(
        &std::env::args()
            .nth(1)
            .unwrap_or_else(|| "Adwaita Sans".into()),
    );
    say(&mut out, serde_json::json!({ "ready": true }));

    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(e) => {
                say(
                    &mut out,
                    serde_json::json!({ "error": format!("bad request: {e}") }),
                );
                continue;
            }
        };
        if request.source.len() > MAX_SOURCE {
            say(
                &mut out,
                serde_json::json!({ "error": format!("the diagram is longer than {} KiB", MAX_SOURCE / 1024) }),
            );
            continue;
        }
        // A panic in the renderer is one diagram's failure, said as one,
        // not the end of the process every later diagram needs.
        let drawn = std::panic::catch_unwind(|| {
            render::render(
                &request.source,
                request.dark,
                &request.family,
                request.scale.clamp(1.0, 4.0),
            )
        })
        .unwrap_or_else(|_| Err("the renderer failed on this diagram".to_string()));
        match drawn {
            Ok(raster) => {
                say(
                    &mut out,
                    serde_json::json!({
                        "width": raster.width,
                        "height": raster.height,
                        "logical_width": raster.logical_width,
                    }),
                );
                let _ = out.write_all(&raster.rgba);
                let _ = out.flush();
            }
            Err(why) => say(&mut out, serde_json::json!({ "error": why })),
        }
    }
}

/// Whether this process is confined as `taste-confine` confines: the
/// three things it can ask about itself.
fn confined() -> Result<(), String> {
    // SAFETY: prctl queries with no pointers.
    let (no_new_privs, seccomp) = unsafe {
        (
            libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0),
            libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0),
        )
    };
    if no_new_privs != 1 || seccomp != 2 {
        return Err(
            "taste-diagram runs only confined, and this process is not: it is \
                    started by the IDE (taste-confine), not by hand"
                .to_string(),
        );
    }
    // The root is never granted, so a confined process cannot list it.
    if std::fs::read_dir("/").is_ok() {
        return Err("taste-diagram runs only under Landlock, and this process can read /".into());
    }
    Ok(())
}

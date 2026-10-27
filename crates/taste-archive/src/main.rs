//! `taste-archive`: a web archive unpacked in a process of its own.
//!
//! An archive is bytes the web, a repository, or an agent wrote, and the
//! readers here (MIME, gzip, HTTP, zip, and an HTML rewriter) handle all of
//! it, while the IDE's own process holds the user's home, the network, and
//! the portal that runs host commands. So the reading runs here, confined
//! by the IDE before its first instruction (`taste-confine`: no file but
//! the system's, no write anywhere, no socket, no signal out), and the IDE
//! hands it the archive over a pipe (CLAUDE.md → third-party code that
//! parses untrusted content).
//!
//! **It will not run unconfined**, as `taste-diagram` will not: a launch
//! that skipped confinement gets a refusal and exit status 2.
//!
//! Protocol: `argv[1]` is the format (`mhtml`, `warc`, or `wacz`), stdin
//! the archive, then its close. Stdout is one line of JSON — the pages and
//! each resource's path, type, and length (`taste_archive::Header`) — and
//! then the resources' bodies one after another in that order; or one line
//! `{"error": why}` and nothing after.

use std::io::{Read, Write};

/// The most an archive may be: the viewer reads no larger file.
const MAX_ARCHIVE: u64 = 512 * 1024 * 1024;

fn main() {
    let mut out = std::io::stdout().lock();
    let fail = |out: &mut std::io::StdoutLock, why: &str| {
        let _ = writeln!(out, "{}", serde_json::json!({ "error": why }));
        let _ = out.flush();
    };
    if let Err(why) = confined() {
        fail(&mut out, &why);
        std::process::exit(2);
    }
    let format = std::env::args()
        .nth(1)
        .and_then(|name| taste_archive::Format::from_name(&name));
    let Some(format) = format else {
        fail(&mut out, "usage: taste-archive mhtml|warc|wacz < archive");
        std::process::exit(2);
    };
    let mut input = Vec::new();
    if let Err(e) = std::io::stdin()
        .lock()
        .take(MAX_ARCHIVE + 1)
        .read_to_end(&mut input)
    {
        fail(&mut out, &format!("reading the archive: {e}"));
        return;
    }
    if input.len() as u64 > MAX_ARCHIVE {
        fail(&mut out, "the archive is larger than 512 MiB");
        return;
    }
    // A panic in a reader is this archive's failure, said as one.
    let bundle = match std::panic::catch_unwind(|| taste_archive::read(format, &input)) {
        Ok(Ok(bundle)) => bundle,
        Ok(Err(why)) => return fail(&mut out, &why),
        Err(_) => return fail(&mut out, "the archive reader failed on this file"),
    };
    let header = match serde_json::to_string(&bundle.header()) {
        Ok(header) => header,
        Err(e) => return fail(&mut out, &format!("writing the index: {e}")),
    };
    let _ = writeln!(out, "{header}");
    for resource in &bundle.resources {
        if out.write_all(&resource.body).is_err() {
            return;
        }
    }
    let _ = out.flush();
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
            "taste-archive runs only confined, and this process is not: it is started by \
             the IDE (taste-confine), not by hand"
                .to_string(),
        );
    }
    // The root is never granted, so a confined process cannot list it.
    if std::fs::read_dir("/").is_ok() {
        return Err("taste-archive runs only under Landlock, and this process can read /".into());
    }
    Ok(())
}

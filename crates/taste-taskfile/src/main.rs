//! `taste-taskfile`: what a project's tasks require, read in a process of
//! its own.
//!
//! A Taskfile is YAML a repository wrote, and the YAML parser is someone
//! else's code, while the IDE's own process holds the user's home, the
//! network, and the portal that runs host commands. So the reading runs
//! here, confined by the IDE before its first instruction (`taste-confine`:
//! no file but the system's, no write anywhere, no socket, no signal out),
//! and the IDE hands it the files' text over a pipe (CLAUDE.md →
//! third-party code that parses untrusted content).
//!
//! **It will not run unconfined**, as `taste-diagram` will not: a launch
//! that skipped confinement gets a refusal and exit status 2.
//!
//! Protocol: one request on stdin, then its close —
//! `{"root": …, "files": {path: text}, "tasks": [{"name", "taskfile",
//! "line", "column"}]}` (`taste_taskfile::Request`) — and one line on
//! stdout: `{"tasks": {name: {"vars": [{"name", "allowed"}]} | {"unknown":
//! why}}}`, or `{"error": why}`.

use std::io::{Read, Write};

/// The most a request may be: Taskfiles, not a repository.
const MAX_REQUEST: u64 = 16 * 1024 * 1024;

fn main() {
    let reply = match confined() {
        Ok(()) => serve(),
        Err(why) => {
            say(&serde_json::json!({ "error": why }));
            std::process::exit(2);
        }
    };
    say(&reply);
}

fn serve() -> serde_json::Value {
    let mut input = Vec::new();
    if let Err(e) = std::io::stdin()
        .lock()
        .take(MAX_REQUEST + 1)
        .read_to_end(&mut input)
    {
        return serde_json::json!({ "error": format!("reading the request: {e}") });
    }
    if input.len() as u64 > MAX_REQUEST {
        return serde_json::json!({ "error": "the Taskfiles are longer than 16 MiB" });
    }
    let request: taste_taskfile::Request = match serde_json::from_slice(&input) {
        Ok(request) => request,
        Err(e) => return serde_json::json!({ "error": format!("bad request: {e}") }),
    };
    // A panic in the parser is this listing's failure, said as one.
    match std::panic::catch_unwind(|| taste_taskfile::answer(&request)) {
        Ok(tasks) => serde_json::json!({ "tasks": tasks }),
        Err(_) => serde_json::json!({ "error": "the Taskfile reader failed on these files" }),
    }
}

fn say(value: &serde_json::Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
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
            "taste-taskfile runs only confined, and this process is not: it is started by \
             the IDE (taste-confine), not by hand"
                .to_string(),
        );
    }
    // The root is never granted, so a confined process cannot list it.
    if std::fs::read_dir("/").is_ok() {
        return Err("taste-taskfile runs only under Landlock, and this process can read /".into());
    }
    Ok(())
}

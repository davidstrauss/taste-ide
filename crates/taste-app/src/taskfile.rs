//! What each listed task asks to be given, read from the Taskfiles by the
//! confined `taste-taskfile` helper: the Tasks section's play asks for it,
//! and the file tree's menus offer a task that takes `FILE` on a file and
//! one that takes `DIR` on a folder (`taste_core::tasks::FILE_VAR`).
//!
//! **Read in the helper, never here.** A Taskfile is YAML a repository
//! wrote, and the parser is someone else's code, so it runs in a process
//! `taste-confine` locks down before its first instruction (CLAUDE.md →
//! third-party code that parses untrusted content). This side reads the
//! files through the files service, hands their text over, and checks the
//! reply before believing it: a task it did not ask about, a variable's
//! name Task would not take, or a list of values past any Taskfile's is
//! dropped, never shown.
//!
//! **`task` still says what the tasks are.** Only the files its listing
//! names are read, and only the tasks it listed are asked about; the helper
//! adds what `--json` leaves out. A helper that cannot start, or a reply
//! that does not check out, leaves every task's `requires` unknown, and
//! the play runs a task by name as it did before.

use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use taste_core::tasks::{Allowed, Listing, Required};
use taste_core::Files;

/// The most of one Taskfile handed over: a Taskfile, not a data file.
const MAX_FILE: usize = 1024 * 1024;
/// The most Taskfiles one listing reads.
const MAX_FILES: usize = 64;
/// How long the helper has to answer.
const DEADLINE: Duration = Duration::from_secs(10);
/// The most of a reply read.
const MAX_REPLY: u64 = 4 * 1024 * 1024;
/// The most values one variable's `enum` may offer before the reply is
/// taken for something other than a Taskfile's, and the longest value.
const MAX_VALUES: usize = 256;
const MAX_VALUE: usize = 1024;

/// The last request's answer, by the request's text: the listing comes
/// again on many events, and the Taskfiles seldom change between them.
static LAST: Mutex<Option<(String, BTreeMap<String, Option<Vec<Required>>>)>> = Mutex::new(None);

/// Fill in each listed task's `requires`. `root` is the checkout as the
/// files service has it, and `workdir` the same place in the container,
/// where `task` named the files. Blocking: file reads and a process.
pub fn annotate(listing: &mut Listing, files: &Files, root: &Path, workdir: Option<&str>) {
    let Listing::Tasks { tasks, .. } = listing else {
        return;
    };
    let Some(workdir) = workdir else {
        return;
    };
    if tasks.is_empty() {
        return;
    }
    let local = |taskfile: &str| -> Option<PathBuf> {
        let rel = Path::new(taskfile).strip_prefix(workdir).ok()?;
        Some(root.join(rel))
    };
    // The root Taskfile: the first of Task's names there is, as Task chose.
    let Some(root_name) = taste_core::conventions::TASKFILE_NAMES
        .iter()
        .find(|name| files.is_file(&root.join(name)))
    else {
        return;
    };
    let root_file = format!("{}/{root_name}", workdir.trim_end_matches('/'));
    let mut texts = BTreeMap::new();
    let wanted = tasks
        .iter()
        .filter_map(|task| task.location.as_ref().map(|at| at.taskfile.clone()))
        .chain([root_file.clone()]);
    for taskfile in wanted {
        if texts.contains_key(&taskfile) || texts.len() >= MAX_FILES {
            continue;
        }
        let Some(path) = local(&taskfile) else {
            continue;
        };
        match files.read(&path) {
            Ok(bytes) if bytes.len() <= MAX_FILE => {
                if let Ok(text) = String::from_utf8(bytes) {
                    texts.insert(taskfile, text);
                }
            }
            _ => {}
        }
    }
    let sites: Vec<serde_json::Value> = tasks
        .iter()
        .filter_map(|task| {
            let at = task.location.as_ref()?;
            Some(serde_json::json!({
                "name": task.name, "taskfile": at.taskfile,
                "line": at.line, "column": at.column,
            }))
        })
        .collect();
    let request =
        serde_json::json!({ "root": root_file, "files": texts, "tasks": sites }).to_string();
    let cached = LAST
        .lock()
        .unwrap()
        .as_ref()
        .filter(|(asked, _)| *asked == request)
        .map(|(_, answer)| answer.clone());
    let answer = match cached {
        Some(answer) => answer,
        None => {
            let asked: HashSet<&str> = tasks.iter().map(|task| task.name.as_str()).collect();
            match ask(&request).and_then(|reply| checked(&reply, &asked)) {
                Ok(answer) => {
                    *LAST.lock().unwrap() = Some((request, answer.clone()));
                    answer
                }
                Err(why) => {
                    tracing::warn!("the Taskfiles were not read: {why}");
                    return;
                }
            }
        }
    };
    for task in tasks.iter_mut() {
        task.requires = answer.get(&task.name).cloned().flatten();
    }
}

/// Where the helper is: beside this executable (`target/debug` in
/// development, `/app/bin` in the Flatpak), or wherever
/// `TASTE_TASKFILE_BIN` names.
fn helper_path() -> Result<PathBuf, String> {
    if let Some(named) = std::env::var_os("TASTE_TASKFILE_BIN") {
        return Ok(PathBuf::from(named));
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("taste-taskfile")))
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            "the Taskfile helper (taste-taskfile) is not installed beside the IDE".into()
        })
}

/// The helper, confined, asked once: its reply's one line.
fn ask(request: &str) -> Result<serde_json::Value, String> {
    let program = helper_path()?;
    let policy = taste_confine::Policy::for_program(&program).map_err(|e| format!("{e:#}"))?;
    let mut command = Command::new(&program);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    taste_confine::confine(&mut command, &policy).map_err(|e| format!("{e:#}"))?;
    let mut child = command
        .spawn()
        .map_err(|e| format!("starting {}: {e}", program.display()))?;
    let mut stdin = child.stdin.take().ok_or("the helper has no stdin")?;
    let mut stdout = child.stdout.take().ok_or("the helper has no stdout")?;
    // Killed at the deadline, so the read below ends at the pipe's close;
    // waited on only after, so its pid cannot be another's by then.
    let pid = child.id() as libc::pid_t;
    let (done, finished) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if finished.recv_timeout(DEADLINE).is_err() {
            // SAFETY: a signal to our own unreaped child.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    });
    let request = request.to_string();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(request.as_bytes());
    });
    let mut reply = Vec::new();
    let read = (&mut stdout).take(MAX_REPLY).read_to_end(&mut reply);
    let _ = done.send(());
    let _ = writer.join();
    let _ = child.kill();
    let _ = child.wait();
    read.map_err(|e| format!("reading the helper: {e}"))?;
    serde_json::from_slice(&reply)
        .map_err(|_| "the Taskfile helper said something unreadable".into())
}

/// The reply, believed only as far as it checks out: an answer per task
/// asked about, `None` for one the helper could not read.
fn checked(
    reply: &serde_json::Value,
    asked: &HashSet<&str>,
) -> Result<BTreeMap<String, Option<Vec<Required>>>, String> {
    if let Some(why) = reply.get("error").and_then(serde_json::Value::as_str) {
        return Err(why.to_string());
    }
    let tasks = reply
        .get("tasks")
        .and_then(serde_json::Value::as_object)
        .ok_or("the reply names no tasks")?;
    let mut answer = BTreeMap::new();
    for (name, found) in tasks {
        if !asked.contains(name.as_str()) {
            return Err(format!("the reply names {name}, which was not asked about"));
        }
        let Some(vars) = found.get("vars").and_then(serde_json::Value::as_array) else {
            answer.insert(name.clone(), None);
            continue;
        };
        let mut required = Vec::new();
        for var in vars {
            required.push(checked_var(var).ok_or(format!("{name}'s variables do not check out"))?);
        }
        answer.insert(name.clone(), Some(required));
    }
    Ok(answer)
}

fn checked_var(var: &serde_json::Value) -> Option<Required> {
    let name = var.get("name")?.as_str()?;
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        || name.len() > 128
    {
        return None;
    }
    let allowed = match var.get("allowed")? {
        serde_json::Value::String(word) if word == "any" => Allowed::Any,
        serde_json::Value::String(word) if word == "unresolved" => Allowed::Unresolved,
        other => {
            let values = other.get("one_of")?.as_array()?;
            if values.len() > MAX_VALUES {
                return None;
            }
            let values = values
                .iter()
                .map(|v| {
                    v.as_str()
                        .filter(|v| v.len() <= MAX_VALUE)
                        .map(str::to_string)
                })
                .collect::<Option<Vec<_>>>()?;
            Allowed::OneOf(values)
        }
    };
    Some(Required {
        name: name.to_string(),
        allowed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_is_believed_only_as_far_as_it_checks_out() {
        let asked: HashSet<&str> = ["fmt", "build"].into_iter().collect();
        let reply = serde_json::json!({"tasks": {
            "fmt": {"vars": [
                {"name": "FILE", "allowed": "any"},
                {"name": "MODE", "allowed": {"one_of": ["check", "write"]}},
                {"name": "LEVEL", "allowed": "unresolved"},
            ]},
            "build": {"unknown": "not read"},
        }});
        let answer = checked(&reply, &asked).unwrap();
        let fmt = answer["fmt"].as_ref().unwrap();
        assert_eq!(fmt[0].allowed, Allowed::Any);
        assert_eq!(
            fmt[1].allowed,
            Allowed::OneOf(vec!["check".into(), "write".into()])
        );
        assert_eq!(fmt[2].allowed, Allowed::Unresolved);
        assert_eq!(answer["build"], None);

        let stranger = serde_json::json!({"tasks": {"rm": {"vars": []}}});
        assert!(checked(&stranger, &asked).is_err());
        let bad_name = serde_json::json!({"tasks": {"fmt": {"vars": [
            {"name": "A B", "allowed": "any"}
        ]}}});
        assert!(checked(&bad_name, &asked).is_err());
        let said = serde_json::json!({"error": "bad request"});
        assert_eq!(checked(&said, &asked).unwrap_err(), "bad request");
    }
}

//! The IDE as git's and ssh's askpass: the question a deliberate Pull or
//! Push runs into — a passphrase, a PIN, a host key to accept — and the
//! notice that a security key wants a touch, asked in the IDE's own strip.
//!
//! Background git never asks (`taste_git::non_interactive_env`). A Pull
//! or Push the user pressed may have to, and the terminal the IDE was
//! launched from is the wrong place for the question — nobody is looking
//! there, and a desktop launch has none (David, 2026-09-16: "I want it to
//! prompt me if I'm explicitly pushing/pulling and it's necessary"). So
//! those steps run with `GIT_ASKPASS` and `SSH_ASKPASS` pointing at this
//! binary in its `--askpass` mode. Both variables name a program that is
//! run with the prompt as its argument and read for the answer on stdout,
//! so the binary is reached through a two-line script in the workspace's
//! state directory that adds the flag ([`helper_script`]).
//!
//! The helper does not draw. It connects to the running IDE over a socket
//! named in its environment (`TASTE_ASKPASS_SOCKET`), sends the prompt and
//! what kind of answer it needs, and the IDE asks in the safe-mode
//! banner's strip — one place for what the IDE has to say about the
//! environment — then sends the answer back (David, 2026-09-16: "Can we
//! have the banner handle: SSH passphrase/PIN requests; Notice to tap a
//! token for presence detection?"). A notice is a request with no answer:
//! ssh runs the askpass with `SSH_ASKPASS_PROMPT=none` to say "touch your
//! key" and kills it when the touch lands, so the strip shows the notice
//! for as long as the connection lasts. With no IDE to reach the helper
//! falls back to a small window of its own. A cancelled question exits
//! non-zero, which git and ssh read as "no answer": the step fails into
//! its toast.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use adw::prelude::*;
use gtk::glib;
use taste_core::event::AskKind;
use taste_core::{Event, EventBus};

/// Questions the banner has not answered yet, by id.
static PENDING: OnceLock<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Option<String>>>>> =
    OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn pending() -> &'static Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Option<String>>>> {
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Where the running IDE for `workspace_root` listens for its helper.
pub fn socket_path(workspace_root: &Path) -> PathBuf {
    taste_core::mcp::runtime_socket(&format!(
        "taste-{}-askpass.sock",
        taste_core::environment::workspace_key(workspace_root)
    ))
}

/// Listen for the helper. One connection is one question: a JSON line in
/// (`prompt`, `kind`), a JSON line out (`answer`, or `cancel`). A notice
/// gets no line out; it stands until the helper goes away.
pub fn serve(workspace_root: &Path, events: EventBus) {
    let path = socket_path(workspace_root);
    crate::runtime::runtime().spawn(async move {
        let _ = tokio::fs::remove_file(&path).await;
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let listener = match tokio::net::UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(e) => {
                tracing::warn!("askpass socket {} could not be bound: {e}", path.display());
                return;
            }
        };
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let events = events.clone();
            tokio::spawn(async move {
                if let Err(e) = converse(stream, &events).await {
                    tracing::debug!("askpass helper conversation ended: {e}");
                }
            });
        }
    });
}

async fn converse(stream: tokio::net::UnixStream, events: &EventBus) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let (reader, mut writer) = stream.into_split();
    let mut lines = tokio::io::BufReader::new(reader).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    let request: serde_json::Value = serde_json::from_str(&line)?;
    let prompt = request["prompt"].as_str().unwrap_or("").trim().to_string();
    let kind = match request["kind"].as_str() {
        Some("secret") => AskKind::Secret,
        Some("confirm") => AskKind::Confirm,
        Some("notice") => AskKind::Notice,
        _ => AskKind::Text,
    };
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = tokio::sync::oneshot::channel::<Option<String>>();
    pending().lock().unwrap().insert(id, sender);
    events.publish(Event::AskRequested { id, prompt, kind });
    // Whichever comes first: the banner's answer, or the helper leaving —
    // ssh kills its notice when the touch lands, and gives up on a question
    // when its own timeout passes.
    let gone = async { while let Ok(Some(_)) = lines.next_line().await {} };
    let outcome = tokio::select! {
        answer = receiver => answer.ok().flatten(),
        _ = gone => None,
    };
    pending().lock().unwrap().remove(&id);
    events.publish(Event::AskDone { id });
    if kind != AskKind::Notice {
        let reply = match outcome {
            Some(answer) => serde_json::json!({ "answer": answer }),
            None => serde_json::json!({ "cancel": true }),
        };
        let mut text = reply.to_string();
        text.push('\n');
        writer.write_all(text.as_bytes()).await?;
    }
    Ok(())
}

/// The banner's answer to question `id`: `None` is a cancel.
pub fn answer(id: u64, answer: Option<String>) {
    if let Some(sender) = pending().lock().unwrap().remove(&id) {
        let _ = sender.send(answer);
    }
}

/// `taste-ide --askpass <prompt…>`: ask the running IDE, or failing that
/// a window of our own; print the answer.
pub fn run(prompt: &str) -> glib::ExitCode {
    let kind = kind_of(prompt, std::env::var("SSH_ASKPASS_PROMPT").ok().as_deref());
    // Said here, in the helper's own process, because naming the key reads
    // its `.pub`, and the IDE's GTK thread reads no files.
    let named = passphrase_prompt(prompt);
    let prompt = named.as_deref().unwrap_or(prompt);
    if let Some(socket) = std::env::var_os("TASTE_ASKPASS_SOCKET") {
        match ask_ide(Path::new(&socket), prompt, kind) {
            Ok(Some(answer)) => {
                println!("{answer}");
                return glib::ExitCode::SUCCESS;
            }
            Ok(None) => return glib::ExitCode::FAILURE,
            Err(e) => {
                eprintln!("taste-ide --askpass: the IDE could not be asked ({e}); asking here")
            }
        }
    }
    ask_in_window(prompt, kind)
}

/// ssh's question for a key's secret in the IDE's words — a passphrase
/// or a security key's PIN — with the key by the name its owner gave it:
/// the `.pub` comment ("amutable-nitro"), the file's name when it has
/// none. The path, when there is one, goes on a second line for the
/// strip's tooltip, since a line of home directory is a thing to read past
/// (David, 2026-09-28: "Is there a cleaner key name we could show?").
/// `None` for any other prompt, which is shown as ssh wrote it.
///
/// The wordings, from OpenSSH itself: ssh's "Enter passphrase for key
/// '<path>':" and ssh-add's "Enter passphrase for <path>:"; a PIN as ssh
/// asks it, "Enter PIN for <TYPE> key <path or fingerprint>:", and as the
/// agent asks it, the same with " again " for a retry; and a FIDO device's
/// own, "Enter PIN for '<device>':".
fn passphrase_prompt(prompt: &str) -> Option<String> {
    let prompt = prompt.trim();
    if let Some(rest) = prompt.strip_prefix("Enter passphrase for ") {
        let rest = rest.strip_prefix("key ").unwrap_or(rest);
        let rest = rest.trim_end().trim_end_matches(':');
        // ssh-add may add " (will confirm each use)" after the path.
        let path = rest
            .split(" (")
            .next()
            .unwrap_or(rest)
            .trim()
            .trim_matches('\'');
        let key = key_by_path(Path::new(path))?;
        return Some(key.say("Passphrase", false));
    }
    let rest = prompt.strip_prefix("Enter PIN")?;
    let (again, rest) = match rest.trim_start().strip_prefix("again") {
        Some(rest) => (true, rest),
        None => (false, rest),
    };
    let rest = rest.trim_start().strip_prefix("for ")?;
    let rest = rest.trim_end().trim_end_matches(':').trim_end();
    // A device by its own name, while a key is being made on it.
    if let Some(device) = rest.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) {
        return Some(format!(
            "PIN for security key {device}{}",
            if again { " (try again)" } else { "" }
        ));
    }
    // "<TYPE> key <which>": which is a path, a fingerprint, or a comment.
    let (kind, which) = rest.split_once(" key ")?;
    let security = kind.to_ascii_uppercase().ends_with("-SK");
    let key = if which.starts_with("SHA256:") {
        key_by_fingerprint(which).unwrap_or(Key {
            security,
            name: which.to_string(),
            path: None,
        })
    } else if Path::new(which).is_file() || Path::new(&format!("{which}.pub")).is_file() {
        key_by_path(Path::new(which))?
    } else {
        Key {
            security,
            name: which.to_string(),
            path: None,
        }
    };
    Some(key.say("PIN", again))
}

/// A key as the strip names it.
struct Key {
    security: bool,
    name: String,
    path: Option<PathBuf>,
}

impl Key {
    fn say(&self, what: &str, again: bool) -> String {
        let kind = if self.security {
            "security key"
        } else {
            "SSH key"
        };
        let again = if again { " (try again)" } else { "" };
        match &self.path {
            Some(path) => format!("{what} for {kind} {}{again}\n{}", self.name, path.display()),
            None => format!("{what} for {kind} {}{again}", self.name),
        }
    }
}

/// The key a private key file is, named from its `.pub`.
fn key_by_path(path: &Path) -> Option<Key> {
    let public = std::fs::read_to_string(format!("{}.pub", path.display())).ok();
    let (security, comment) = match public.as_deref().and_then(|text| text.lines().next()) {
        Some(line) => {
            let mut fields = line.split_whitespace();
            let kind = fields.next().unwrap_or_default();
            let comment = fields.skip(1).collect::<Vec<_>>().join(" ");
            (kind.starts_with("sk-"), comment)
        }
        None => (false, String::new()),
    };
    let name = if comment.is_empty() {
        path.file_name()?.to_string_lossy().into_owned()
    } else {
        comment
    };
    Some(Key {
        security,
        name,
        path: Some(path.to_path_buf()),
    })
}

/// The key in `~/.ssh` whose fingerprint is `fingerprint`, as the agent
/// names a key it asks a PIN for.
fn key_by_fingerprint(fingerprint: &str) -> Option<Key> {
    let dir = PathBuf::from(std::env::var_os("HOME")?).join(".ssh");
    std::fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
        let public = entry.path();
        if public.extension().is_none_or(|ext| ext != "pub") {
            return None;
        }
        let out = std::process::Command::new("ssh-keygen")
            .arg("-lf")
            .arg(&public)
            .output()
            .ok()?;
        let listed = String::from_utf8_lossy(&out.stdout);
        if !listed.split_whitespace().any(|field| field == fingerprint) {
            return None;
        }
        key_by_path(&public.with_extension(""))
    })
}

/// What a prompt wants, from ssh's word for it when it gives one
/// (`SSH_ASKPASS_PROMPT`: `none` for a notice, `confirm` for yes/no) and
/// from the prompt's own words otherwise.
fn kind_of(prompt: &str, ssh_prompt: Option<&str>) -> AskKind {
    match ssh_prompt {
        Some("none") => AskKind::Notice,
        Some("confirm") => AskKind::Confirm,
        _ => {
            let lower = prompt.to_ascii_lowercase();
            if lower.contains("password") || lower.contains("passphrase") || lower.contains("pin") {
                AskKind::Secret
            } else if lower.contains("(yes/no") {
                AskKind::Confirm
            } else {
                AskKind::Text
            }
        }
    }
}

/// One question over the socket. `Ok(None)` is a cancel; a notice returns
/// `Ok(None)` only when the connection ends, which for a notice is never
/// from our side — ssh kills this process first.
fn ask_ide(socket: &Path, prompt: &str, kind: AskKind) -> anyhow::Result<Option<String>> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    let kind_word = match kind {
        AskKind::Secret => "secret",
        AskKind::Text => "text",
        AskKind::Confirm => "confirm",
        AskKind::Notice => "notice",
    };
    let request = serde_json::json!({ "prompt": prompt, "kind": kind_word });
    stream.write_all(format!("{request}\n").as_bytes())?;
    let mut reply = String::new();
    std::io::BufReader::new(stream).read_line(&mut reply)?;
    if reply.trim().is_empty() {
        return Ok(None);
    }
    let reply: serde_json::Value = serde_json::from_str(reply.trim())?;
    Ok(reply["answer"].as_str().map(str::to_string))
}

/// The helper's own window, for a run with no IDE to ask.
fn ask_in_window(prompt: &str, kind: AskKind) -> glib::ExitCode {
    if gtk::init().is_err() {
        eprintln!("taste-ide --askpass: no display to ask on");
        return glib::ExitCode::FAILURE;
    }
    let _ = adw::init();
    let window = gtk::Window::builder()
        .title("Taste")
        .default_width(440)
        .resizable(false)
        .build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    let heading = gtk::Label::builder()
        .label("Git needs an answer")
        .css_classes(["title-4"])
        .xalign(0.0)
        .build();
    let question = gtk::Label::builder()
        .label(prompt.trim())
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .selectable(true)
        .build();
    let entry: Option<gtk::Widget> = match kind {
        AskKind::Secret => Some(
            gtk::PasswordEntry::builder()
                .show_peek_icon(true)
                .activates_default(true)
                .build()
                .upcast(),
        ),
        AskKind::Text => Some(
            gtk::Entry::builder()
                .activates_default(true)
                .build()
                .upcast(),
        ),
        AskKind::Confirm | AskKind::Notice => None,
    };
    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    let cancel = gtk::Button::with_label(if kind == AskKind::Confirm {
        "No"
    } else {
        "Cancel"
    });
    let ok = gtk::Button::builder()
        .label(if kind == AskKind::Confirm {
            "Yes"
        } else {
            "Answer"
        })
        .css_classes(["suggested-action"])
        .build();
    if kind != AskKind::Notice {
        buttons.append(&cancel);
        buttons.append(&ok);
    }
    content.append(&heading);
    content.append(&question);
    if let Some(entry) = &entry {
        content.append(entry);
    }
    content.append(&buttons);
    window.set_child(Some(&content));
    window.set_default_widget(Some(&ok));

    let answer: std::rc::Rc<std::cell::RefCell<Option<String>>> = Default::default();
    let main_loop = glib::MainLoop::new(None, false);
    {
        let answer = answer.clone();
        let entry = entry.clone();
        let window = window.clone();
        ok.connect_clicked(move |_| {
            let text = match &entry {
                Some(entry) => entry
                    .downcast_ref::<gtk::PasswordEntry>()
                    .map(|e| e.text().to_string())
                    .or_else(|| {
                        entry
                            .downcast_ref::<gtk::Entry>()
                            .map(|e| e.text().to_string())
                    })
                    .unwrap_or_default(),
                None => "yes".to_string(),
            };
            *answer.borrow_mut() = Some(text);
            window.close();
        });
    }
    {
        let window = window.clone();
        cancel.connect_clicked(move |_| window.close());
    }
    {
        let main_loop = main_loop.clone();
        window.connect_close_request(move |_| {
            main_loop.quit();
            glib::Propagation::Proceed
        });
    }
    window.present();
    if let Some(entry) = &entry {
        entry.grab_focus();
    }
    main_loop.run();
    let taken = answer.borrow_mut().take();
    match taken {
        Some(text) => {
            println!("{text}");
            glib::ExitCode::SUCCESS
        }
        None => glib::ExitCode::FAILURE,
    }
}

/// The script git and ssh are pointed at: this binary, with the flag, in
/// the workspace's own state directory. Rewritten on every call, so a
/// binary that moved (a rebuild, an install) is followed. `None` when it
/// cannot be written, and the caller falls back to asking nothing.
pub fn helper_script(workspace_root: &Path) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    helper_script_in(
        &taste_core::state::workspace_state_dir(workspace_root),
        &exe,
    )
}

/// [`helper_script`] with the directory and binary given — for the test.
fn helper_script_in(dir: &Path, exe: &Path) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join("askpass.sh");
    let script = format!(
        "#!/bin/sh\n# Written by taste-ide: git's and ssh's askpass for a Pull or Push \
         the user pressed.\nexec {} --askpass \"$@\"\n",
        shell_quote(&exe.display().to_string())
    );
    std::fs::write(&path, script).ok()?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).ok()?;
    Some(path)
}

/// The environment a deliberate Pull or Push runs with: git's own
/// terminal prompt off (there is no terminal to speak of), and the
/// question routed to this binary, which asks the running IDE. Falls back
/// to the background's silence when the helper cannot be written.
pub fn env_for(workspace_root: &Path) -> Vec<(String, String)> {
    match helper_script(workspace_root) {
        Some(helper) => taste_git::interactive_env(&helper, Some(&socket_path(workspace_root))),
        None => taste_git::non_interactive_env(),
    }
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_passphrase_prompt_names_the_key_its_owner_named() {
        let dir = std::env::temp_dir().join(format!("taste-askpass-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("id_ed25519_sk");
        std::fs::write(
            dir.join("id_ed25519_sk.pub"),
            "sk-ssh-ed25519@openssh.com AAAAGnNr amutable-nitro\n",
        )
        .unwrap();
        let plain = dir.join("id_rsa");
        // ssh-add's words, and ssh's.
        assert_eq!(
            passphrase_prompt(&format!("Enter passphrase for {}: ", key.display())).as_deref(),
            Some(
                format!(
                    "Passphrase for security key amutable-nitro\n{}",
                    key.display()
                )
                .as_str()
            )
        );
        assert_eq!(
            passphrase_prompt(&format!("Enter passphrase for key '{}': ", plain.display()))
                .as_deref(),
            Some(format!("Passphrase for SSH key id_rsa\n{}", plain.display()).as_str())
        );
        // A PIN, as ssh asks it over the file, and as the agent asks it
        // again over a name it cannot place.
        assert_eq!(
            passphrase_prompt(&format!("Enter PIN for ED25519-SK key {}: ", key.display()))
                .as_deref(),
            Some(format!("PIN for security key amutable-nitro\n{}", key.display()).as_str())
        );
        assert_eq!(
            passphrase_prompt("Enter PIN again for ECDSA-SK key work-yubikey: ").as_deref(),
            Some("PIN for security key work-yubikey (try again)")
        );
        assert_eq!(
            passphrase_prompt("Enter PIN for 'Nitrokey 3': ").as_deref(),
            Some("PIN for security key Nitrokey 3")
        );
        assert_eq!(passphrase_prompt("root@host's password: "), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_kind_comes_from_ssh_when_it_says_and_from_the_words_otherwise() {
        assert_eq!(
            kind_of("Confirm user presence for key ED25519-SK", Some("none")),
            AskKind::Notice
        );
        assert_eq!(
            kind_of(
                "Are you sure you want to continue connecting (yes/no/[fingerprint])?",
                Some("confirm")
            ),
            AskKind::Confirm
        );
        assert_eq!(kind_of("root@host's password: ", None), AskKind::Secret);
        assert_eq!(
            kind_of("Enter passphrase for key '/home/u/.ssh/id_ed25519': ", None),
            AskKind::Secret
        );
        assert_eq!(
            kind_of("Enter PIN for authenticator: ", None),
            AskKind::Secret
        );
        assert_eq!(
            kind_of(
                "Are you sure you want to continue connecting (yes/no)?",
                None
            ),
            AskKind::Confirm
        );
        assert_eq!(
            kind_of("Username for 'https://github.com': ", None),
            AskKind::Text
        );
    }

    #[test]
    fn the_helper_script_runs_this_binary_in_askpass_mode() {
        let dir = std::env::temp_dir().join(format!(
            "taste-askpass-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let path =
            helper_script_in(&dir, Path::new("/opt/taste ide/bin/taste-ide")).expect("a helper");
        let script = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(script.starts_with("#!/bin/sh\n"), "{script}");
        assert!(script.contains("--askpass \"$@\""), "{script}");
        // The binary's path is quoted, spaces and all.
        assert!(
            script.contains("'/opt/taste ide/bin/taste-ide' --askpass"),
            "{script}"
        );
    }

    /// The whole loop, in-process: a "helper" connects, the request is
    /// published, the banner answers, the helper reads the answer.
    #[tokio::test]
    async fn a_question_over_the_socket_is_published_and_the_answer_comes_back() {
        let dir = std::env::temp_dir().join(format!("taste-askpass-sock-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let socket = dir.join("ask.sock");
        let _ = std::fs::remove_file(&socket);
        let events = EventBus::new();
        let seen = events.subscribe();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server_events = events.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            converse(stream, &server_events).await.unwrap();
        });
        let socket_for_helper = socket.clone();
        let helper = tokio::task::spawn_blocking(move || {
            ask_ide(
                &socket_for_helper,
                "Enter PIN for authenticator: ",
                AskKind::Secret,
            )
        });
        let asked = loop {
            match seen.recv().await {
                Ok(Event::AskRequested { id, prompt, kind }) => break (id, prompt, kind),
                Ok(_) => continue,
                Err(_) => panic!("the request was never published"),
            }
        };
        assert_eq!(asked.1, "Enter PIN for authenticator:");
        assert_eq!(asked.2, AskKind::Secret);
        answer(asked.0, Some("1234".into()));
        assert_eq!(helper.await.unwrap().unwrap().as_deref(), Some("1234"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

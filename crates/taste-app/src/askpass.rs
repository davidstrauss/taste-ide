//! The IDE as git's and ssh's askpass: one small dialog for the question a
//! deliberate Pull or Push runs into — a password, a passphrase, a host
//! key to accept.
//!
//! Background git never asks (`taste_git::non_interactive_env`). A Pull
//! or Push the user pressed may have to, and the terminal the IDE was
//! launched from is the wrong place for the question — nobody is looking
//! there, and a desktop launch has none (David, 2026-09-16: "I want it to
//! prompt me if I'm explicitly pushing/pulling and it's necessary"). So
//! those steps run with `GIT_ASKPASS` and `SSH_ASKPASS` pointing at this
//! binary in its `--askpass` mode, which is what every graphical git
//! client does, and the prompt appears as a window.
//!
//! Both variables name a program that is run with the prompt as its
//! argument and read for the answer on stdout, so the binary is reached
//! through a two-line script in the workspace's state directory that adds
//! the flag ([`helper_script`]). A cancelled dialog exits non-zero, which
//! git and ssh read as "no answer": the step fails into its toast.

use std::path::{Path, PathBuf};

use adw::prelude::*;
use gtk::glib;

/// `taste-ide --askpass <prompt…>`: show the prompt, print the answer.
pub fn run(prompt: &str) -> glib::ExitCode {
    if gtk::init().is_err() {
        eprintln!("taste-ide --askpass: no display to ask on");
        return glib::ExitCode::FAILURE;
    }
    let _ = adw::init();
    // A password or passphrase is hidden; a host-key question ("yes/no")
    // or a username is not.
    let secret = {
        let lower = prompt.to_ascii_lowercase();
        lower.contains("password") || lower.contains("passphrase")
    };
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
    let entry: gtk::Widget = if secret {
        gtk::PasswordEntry::builder()
            .show_peek_icon(true)
            .activates_default(true)
            .build()
            .upcast()
    } else {
        gtk::Entry::builder()
            .activates_default(true)
            .build()
            .upcast()
    };
    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::builder()
        .label("Answer")
        .css_classes(["suggested-action"])
        .build();
    buttons.append(&cancel);
    buttons.append(&ok);
    content.append(&heading);
    content.append(&question);
    content.append(&entry);
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
            let text = entry
                .downcast_ref::<gtk::PasswordEntry>()
                .map(|e| e.text().to_string())
                .or_else(|| {
                    entry
                        .downcast_ref::<gtk::Entry>()
                        .map(|e| e.text().to_string())
                })
                .unwrap_or_default();
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
    entry.grab_focus();
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
/// question routed to [`run`]. Falls back to the background's silence
/// when the helper cannot be written.
pub fn env_for(workspace_root: &Path) -> Vec<(String, String)> {
    match helper_script(workspace_root) {
        Some(helper) => taste_git::interactive_env(&helper),
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
    fn a_password_or_passphrase_is_hidden_and_a_host_key_question_is_not() {
        for prompt in ["root@host's password: ", "Enter passphrase for key"] {
            let lower = prompt.to_ascii_lowercase();
            assert!(lower.contains("password") || lower.contains("passphrase"));
        }
        let host_key = "Are you sure you want to continue connecting (yes/no/[fingerprint])?";
        let lower = host_key.to_ascii_lowercase();
        assert!(!(lower.contains("password") || lower.contains("passphrase")));
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
}

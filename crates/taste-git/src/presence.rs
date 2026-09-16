//! Whether fetching from a remote would ask the user to touch a key.
//!
//! A FIDO security key (`sk-ssh-ed25519@openssh.com`,
//! `sk-ecdsa-sha2-nistp256@openssh.com`) signs only with the user present,
//! and the agent asks for that presence with a prompt on the desktop. The
//! file tree fetches in the background to keep its ahead/behind counts
//! honest, and a background fetch over such a key is a "tap your key"
//! dialog popping up every few minutes for nothing the user asked for
//! (David, 2026-09-16: "I don't want background polling to pop open
//! modals telling me to tap the key"). So the IDE asks first: is the
//! remote reached over SSH, and would SSH reach for a key that needs a
//! touch? If so, nothing fetches in the background; Pull, the deliberate
//! act, still does.
//!
//! Two documented surfaces answer the second question, and either is
//! enough: `ssh-add -L` lists the agent's keys with their types, and
//! `ssh -G <host>` prints the client configuration that host would get,
//! `identityfile` lines included, whose `.pub` files carry the type. A
//! key that was minted with `no-touch-required` is indistinguishable
//! from one that was not, and is held back with it — the cost of that is
//! a stale count, the cost of the other mistake is the dialog.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Whether `url` reaches its remote over SSH: `ssh://`, `git+ssh://`, or
/// the scp-like `user@host:path`. Anything with another scheme, and any
/// local path, is not.
pub fn is_ssh_url(url: &str) -> bool {
    let url = url.trim();
    if let Some((scheme, _)) = url.split_once("://") {
        return matches!(scheme, "ssh" | "git+ssh" | "ssh+git");
    }
    // scp-like: `host:path` or `user@host:path`, with a host before the
    // colon. A bare path (`/srv/repo.git`, `../other`, `C:\\repo`) is not.
    if url.starts_with('/') || url.starts_with('.') || url.starts_with('~') {
        return false;
    }
    match url.split_once(':') {
        Some((host, _)) => !host.is_empty() && !host.contains('/') && host.len() > 1,
        None => false,
    }
}

/// The host `ssh` would be asked for, as `ssh -G` wants it (with the
/// user, so a `Match user` block applies), or `None` when `url` is not an
/// SSH URL.
pub fn ssh_target(url: &str) -> Option<String> {
    if !is_ssh_url(url) {
        return None;
    }
    let url = url.trim();
    let after_scheme = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => url,
    };
    let authority = after_scheme.split(['/', ':']).next().unwrap_or_default();
    (!authority.is_empty()).then(|| authority.to_string())
}

/// Whether an `ssh-add -L` listing names a key that needs a touch.
pub fn listing_has_presence_key(listing: &str) -> bool {
    listing
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .any(|kind| kind.starts_with("sk-"))
}

/// The identity files `ssh -G` output names, `~` expanded.
pub fn identity_files(ssh_g: &str, home: Option<&Path>) -> Vec<PathBuf> {
    ssh_g
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(' ')?;
            key.eq_ignore_ascii_case("identityfile")
                .then(|| value.trim())
        })
        .map(|value| match (value.strip_prefix("~/"), home) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => PathBuf::from(value),
        })
        .collect()
}

/// Whether any of `files` is a key that needs a touch: its `.pub` (or the
/// file itself, for a public key path) begins with an `sk-` type.
pub fn identity_files_need_presence(files: &[PathBuf]) -> bool {
    files.iter().any(|file| {
        let public = if file.extension().is_some_and(|ext| ext == "pub") {
            file.clone()
        } else {
            let mut public = file.clone().into_os_string();
            public.push(".pub");
            PathBuf::from(public)
        };
        std::fs::read_to_string(public)
            .ok()
            .is_some_and(|text| listing_has_presence_key(&text))
    })
}

/// Why a fetch from `url` would ask the user for a touch, or `None` when
/// it would not (or `url` is not reached over SSH). Runs `ssh-add -L` and
/// `ssh -G`, both local and quick; blocking, so a caller on the GTK thread
/// runs it on the blocking pool.
pub fn fetch_needs_presence(url: &str) -> Option<String> {
    let target = ssh_target(url)?;
    let agent_listing = Command::new("ssh-add")
        .arg("-L")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default();
    if listing_has_presence_key(&agent_listing) {
        return Some(format!(
            "the SSH agent holds a security key that needs a touch, and {target} is reached \
             over SSH"
        ));
    }
    let config = Command::new("ssh")
        .args(["-G", &target])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if identity_files_need_presence(&identity_files(&config, home.as_deref())) {
        return Some(format!(
            "the SSH identity configured for {target} is a security key that needs a touch"
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_urls_are_told_from_the_rest() {
        assert!(is_ssh_url("ssh://root@89.250.66.249/var/www/html"));
        assert!(is_ssh_url("git+ssh://git@github.com/o/r.git"));
        assert!(is_ssh_url("git@github.com:davidstrauss/taste-ide.git"));
        assert!(is_ssh_url("github.com:o/r.git"));
        assert!(!is_ssh_url("https://github.com/davidstrauss/taste-ide.git"));
        assert!(!is_ssh_url("git://example.org/r.git"));
        assert!(!is_ssh_url("file:///srv/r.git"));
        assert!(!is_ssh_url("/srv/repos/r.git"));
        assert!(!is_ssh_url("../sibling"));
        assert!(!is_ssh_url("~/r.git"));
    }

    #[test]
    fn the_ssh_target_is_the_authority_with_its_user() {
        assert_eq!(
            ssh_target("ssh://root@89.250.66.249/var/www/html").as_deref(),
            Some("root@89.250.66.249")
        );
        assert_eq!(
            ssh_target("git@github.com:o/r.git").as_deref(),
            Some("git@github.com")
        );
        assert_eq!(ssh_target("https://github.com/o/r.git"), None);
    }

    #[test]
    fn a_security_key_is_seen_in_the_agents_listing_and_in_identity_files() {
        let listing = "ssh-ed25519 AAAAC3 user@host\n\
                       sk-ssh-ed25519@openssh.com AAAAGnNr user@host\n";
        assert!(listing_has_presence_key(listing));
        assert!(!listing_has_presence_key("ssh-ed25519 AAAAC3 user@host\n"));
        assert!(!listing_has_presence_key("The agent has no identities.\n"));

        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(
            ssh.join("id_ecdsa_sk.pub"),
            "sk-ecdsa-sha2-nistp256@openssh.com AAAAInNr user@host\n",
        )
        .unwrap();
        std::fs::write(ssh.join("id_ed25519.pub"), "ssh-ed25519 AAAAC3 user@host\n").unwrap();
        let config = "user root\nidentityfile ~/.ssh/id_rsa\nidentityfile ~/.ssh/id_ecdsa_sk\n";
        let files = identity_files(config, Some(dir.path()));
        assert_eq!(
            files,
            vec![ssh.join("id_rsa"), ssh.join("id_ecdsa_sk")],
            "{files:?}"
        );
        assert!(identity_files_need_presence(&files));
        assert!(!identity_files_need_presence(&[ssh.join("id_ed25519")]));
    }
}

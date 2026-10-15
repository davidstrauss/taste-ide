//! A submodule on its parent's host is fetched the way the parent is.
//!
//! A repository cloned over HTTPS from github.com often has submodules
//! whose `.gitmodules` names them over SSH (`git@github.com:org/lib`), or
//! the other way round, because whoever added them used their own habit.
//! The user's keys answer the scheme the parent uses — that is the one
//! they cloned with — so a submodule on the other scheme asks for
//! credentials they may not have set up here, or for a key touch the
//! parent never needed. So every fetch the IDE makes of a submodule, the
//! parent's own fetch that reaches into them, and the parent's push that
//! takes their commits along, carries git's `url.<base>.insteadOf`
//! rewrites from the parent's URL: on the parent's host, the other scheme
//! is reached as the parent's (David, 2026-10-14: "if the parent repo uses
//! HTTPS to github.com, have submodules use that, too, even if they are set
//! to use SSH. Do the same in the other direction"; 2026-10-15: "I want
//! pushes to use the same rewrite"). The parent's own remotes on that host
//! follow it too. A rewrite given on the command line, not written to any
//! config, so `.gitmodules` and the user's own settings stay as they are.

/// How a URL reaches its remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scheme {
    Https,
    Ssh,
}

/// A remote URL as far as the rewrite needs it: its scheme, its host
/// (lowercase, without a port or a user), and the prefix every URL of that
/// remote's host shares — the part a rewrite replaces.
#[derive(Debug, PartialEq, Eq)]
struct Remote {
    scheme: Scheme,
    host: String,
    prefix: String,
}

fn parse(url: &str) -> Option<Remote> {
    let url = url.trim();
    if let Some((scheme, rest)) = url.split_once("://") {
        let scheme = match scheme.to_ascii_lowercase().as_str() {
            "https" | "http" => Scheme::Https,
            "ssh" | "git+ssh" | "ssh+git" => Scheme::Ssh,
            _ => return None,
        };
        let authority = rest.split('/').next()?;
        let host = authority.rsplit('@').next()?.split(':').next()?;
        if host.is_empty() {
            return None;
        }
        return Some(Remote {
            scheme,
            host: host.to_ascii_lowercase(),
            prefix: format!("{}/", &url[..url.len() - rest.len() + authority.len()]),
        });
    }
    if !crate::presence::is_ssh_url(url) {
        return None;
    }
    // scp-like: `[user@]host:path`.
    let (authority, _) = url.split_once(':')?;
    let host = authority.rsplit('@').next()?;
    Some(Remote {
        scheme: Scheme::Ssh,
        host: host.to_ascii_lowercase(),
        prefix: format!("{authority}:"),
    })
}

/// The prefixes a URL on `host` in the other scheme starts with, as
/// `.gitmodules` files write them: `git@host:` and `ssh://git@host/` for
/// SSH, `https://host/` for HTTPS.
fn other_prefixes(parent: &Remote) -> Vec<String> {
    let host = &parent.host;
    let others = match parent.scheme {
        Scheme::Https => vec![
            format!("git@{host}:"),
            format!("ssh://git@{host}/"),
            format!("ssh://{host}/"),
        ],
        Scheme::Ssh => vec![format!("https://{host}/"), format!("http://{host}/")],
    };
    others
        .into_iter()
        .filter(|other| *other != parent.prefix)
        .collect()
}

/// The `-c url.<base>.insteadOf=<other>` arguments that fetch a URL on
/// `parent_url`'s host the way `parent_url` is fetched, to go before a git
/// subcommand; empty for a parent that is not on a host (a local path) or
/// not over HTTPS or SSH.
pub fn align_args(parent_url: Option<&str>) -> Vec<String> {
    let Some(parent) = parent_url.and_then(parse) else {
        return Vec::new();
    };
    other_prefixes(&parent)
        .into_iter()
        .flat_map(|other| {
            [
                "-c".to_string(),
                format!("url.{}.insteadOf={other}", parent.prefix),
            ]
        })
        .collect()
}

/// `url` as [`align_args`] has git fetch it: on `parent_url`'s host in the
/// other scheme, rewritten to the parent's; otherwise as it is. What the
/// credentials for a submodule's fetch are chosen by.
pub fn aligned(parent_url: Option<&str>, url: &str) -> String {
    let Some(parent) = parent_url.and_then(parse) else {
        return url.to_string();
    };
    other_prefixes(&parent)
        .into_iter()
        .find_map(|other| {
            url.strip_prefix(other.as_str())
                .map(|rest| format!("{}{rest}", parent.prefix))
        })
        .unwrap_or_else(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_read_for_their_scheme_host_and_prefix() {
        let remote = |scheme, host: &str, prefix: &str| {
            Some(Remote {
                scheme,
                host: host.into(),
                prefix: prefix.into(),
            })
        };
        assert_eq!(
            parse("https://github.com/org/repo.git"),
            remote(Scheme::Https, "github.com", "https://github.com/")
        );
        assert_eq!(
            parse("https://me@GitHub.com:8443/org/repo"),
            remote(Scheme::Https, "github.com", "https://me@GitHub.com:8443/")
        );
        assert_eq!(
            parse("git@github.com:org/repo.git"),
            remote(Scheme::Ssh, "github.com", "git@github.com:")
        );
        assert_eq!(
            parse("ssh://git@gitlab.example.com:2222/org/repo"),
            remote(
                Scheme::Ssh,
                "gitlab.example.com",
                "ssh://git@gitlab.example.com:2222/"
            )
        );
        assert_eq!(parse("/srv/repo.git"), None);
        assert_eq!(parse("../lib.git"), None);
        assert_eq!(parse("file:///srv/repo.git"), None);
    }

    /// An HTTPS parent fetches its host's SSH submodules over HTTPS, and
    /// an SSH parent its host's HTTPS ones over SSH; another host's, and
    /// the parent's own scheme, are left as they are.
    #[test]
    fn a_submodule_on_the_parents_host_takes_the_parents_scheme() {
        let https = Some("https://github.com/org/deck.git");
        assert_eq!(
            aligned(https, "git@github.com:org/template.git"),
            "https://github.com/org/template.git"
        );
        assert_eq!(
            aligned(https, "ssh://git@github.com/org/template.git"),
            "https://github.com/org/template.git"
        );
        assert_eq!(
            aligned(https, "git@gitlab.com:org/template.git"),
            "git@gitlab.com:org/template.git"
        );
        assert_eq!(
            aligned(https, "https://github.com/org/template.git"),
            "https://github.com/org/template.git"
        );
        let ssh = Some("git@github.com:org/deck.git");
        assert_eq!(
            aligned(ssh, "https://github.com/org/template.git"),
            "git@github.com:org/template.git"
        );
        assert_eq!(
            aligned(ssh, "ssh://git@github.com/org/template.git"),
            "ssh://git@github.com/org/template.git"
        );
        assert_eq!(aligned(None, "git@github.com:x"), "git@github.com:x");
        assert_eq!(
            align_args(https)[..2],
            [
                "-c".to_string(),
                "url.https://github.com/.insteadOf=git@github.com:".to_string()
            ]
        );
        assert!(align_args(Some("/srv/deck.git")).is_empty());
    }

    /// The rewrite as git applies it: a submodule whose `.gitmodules`
    /// names it by a URL that does not exist is cloned from the one the
    /// rewrite turns it into.
    #[test]
    fn git_clones_a_submodule_through_the_rewrite() {
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        let git = |cwd: &std::path::Path, args: &[&str]| {
            let out = Command::new("git")
                .current_dir(cwd)
                .args(args)
                .env("GIT_ALLOW_PROTOCOL", "file")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main", "lib"]);
        std::fs::write(root.join("lib/lib.txt"), "lib\n").unwrap();
        git(&root.join("lib"), &["add", "-A"]);
        git(&root.join("lib"), &["commit", "-q", "-m", "lib"]);
        git(root, &["init", "-q", "-b", "main", "parent"]);
        let parent = root.join("parent");
        git(
            &parent,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                root.join("lib").to_str().unwrap(),
                "lib",
            ],
        );
        // What `.gitmodules` says now: SSH to a host, which this test
        // cannot reach.
        git(
            &parent,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.lib.url",
                "git@example.com:lib",
            ],
        );
        git(&parent, &["commit", "-qam", "lib over ssh"]);
        git(root, &["clone", "-q", parent.to_str().unwrap(), "folder"]);
        // The rewrite under test, with the parent's "host" a local path:
        // `file://` stands in for HTTPS, which a test has no server for.
        let base = format!("file://{}/", root.display());
        let mut args = vec![
            "-c".to_string(),
            format!("url.{base}.insteadOf=git@example.com:"),
        ];
        args.extend(
            ["submodule", "update", "--init", "--", "lib"]
                .iter()
                .map(|s| s.to_string()),
        );
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        git(&root.join("folder"), &args);
        assert_eq!(
            std::fs::read_to_string(root.join("folder/lib/lib.txt")).unwrap(),
            "lib\n"
        );
    }
}

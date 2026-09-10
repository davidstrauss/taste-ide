---
title: The file tree says "not a git repository" when it merely failed to open one
state: open
reporter: primary
created: 2026-09-10T06:20:02Z
updated: 2026-09-10T06:20:02Z
labels: bug, git, filetree
---

The file tree's branch label reads "not a git repository" over a checkout that
plainly is one, and it offers an enabled "Initialize Repository" button next to
it. Reported from the primary environment on `main`, with the tree listing 217
files and the header still showing "Stashed 1" — a count only git could have
produced — in the same frame as the claim that there is no git.

## Root cause

`crates/taste-git/src/lib.rs:175`:

```rust
pub fn discover(root: &Path) -> Option<Self> {
    let repo = Repository::discover(root).ok()?;
    let workdir = repo.workdir()?.to_path_buf();
    Some(Self { repo, workdir })
}
```

`.ok()?` discards the `git2::Error`, and with it the class and code that are the
only way to tell these apart:

- `GIT_ENOTFOUND` — there really is no repository, and "Initialize Repository" is
  the right offer.
- an ownership rejection, an EACCES, an SELinux denial, a locked or corrupt ref,
  a bare repository (`repo.workdir()` is `None`) — the repository exists and
  could not be opened.

Every one of them becomes the same `None`. `filetree.rs:3359` then renders that
`None` as a statement of fact about the world.

Discovery is the *only* fatal step in the snapshot. `filetree.rs:3230-3246`
builds it with `git.status().unwrap_or_default()`,
`git.stashed_paths().unwrap_or_default()`, and `git.sync_status().ok()` — every
other failure degrades quietly. So the one call that cannot fail gracefully is
also the one that reports its failure as an assertion.

## Why this is more than a wrong label

`filetree.rs:3363-3367` sets the label, and in the same arm sets
`init_button.set_sensitive(true)`, with `set_visible(!is_repo)` at line 3371.
A transient failure to open an existing repository therefore presents the user
with a live button offering to initialize over it. Whatever that button does, it
should not be reachable on the strength of an error nobody looked at.

The `None` arm also leaves the section counts from the last successful refresh
in place, which is how the pane comes to say "Stashed 1" and "not a git
repository" at once. Either reset them or say the state is stale; do not show
both.

## Leading hypothesis for this host, to be confirmed rather than assumed

`.devcontainer/devcontainer.json` mounts the checkout with
`type=bind,...,Z` — a private SELinux relabel. Commit `bb1fe68` ("An
environment's clone shares no inode, so one container's relabel stops taking git
away from the others") is the same class of failure, already hit once. If a
relabel is denying the host-side IDE access to `.git`, discovery fails, and
today that is indistinguishable from an empty directory.

Confirm before building on it: `ls -Z .git` on the host, and `ausearch -m avc -ts
recent` for a denial against the IDE process. If it is something else, the fix
below is still correct — it is what makes the real cause legible.

## The fix

`discover` reports why it failed. Either return a `Result`, or a three-way
outcome distinguishing "opened", "no repository here", and "present but
unopenable, because <error>". Use `ide_references` on `GitWorkspace::discover`
to find every caller before changing the signature; several crates use it, and
`taste-git` links no GTK, so the type has to stay usable from all of them.

Then the file tree:

- Genuinely no repository → today's behavior, including the Initialize button.
- Present but unopenable → say so, name the reason, and **do not** offer
  Initialize. The right words are "git unavailable" and the error, not a claim
  about whether a repository exists.
- Log the error either way. It is currently thrown away at the only point where
  anything knows what it was.

## Tests

Cover the discrimination in `taste-git`: a directory that is not a repository, a
repository that opens, and a repository made unopenable (a bare repo is the
portable case, since `repo.workdir()` returns `None` for it and that path is
conflated today too). Assert the outcomes differ. A permissions case would be
better still if it can be made to run unprivileged and deterministically; skip
it rather than write one that is flaky.

## Gate

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, and `cargo test --workspace`. This changes the file tree's header, so
pose it and look: `TASTE_PROBE_CHECK=1`. Oxford commas in everything written.
Commit per verified batch; never push.

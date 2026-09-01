#!/bin/sh
# Build the repository the docs/screenshots are taken against.
#
# The shots show the file-tree pane, and that pane shows the branch you are
# on and what is dirty in it. Taken straight from a working checkout they
# bake in whatever that checkout happened to be — an agent worktree's
# generated branch name, a half-finished rebase, someone's scratch file —
# and the frames then document the photographer rather than the IDE.
#
# So the shots are taken against a fixture: this project's own tracked
# files, committed once, on `main`. It is the same source tree the reader
# is looking at, which is what keeps the editor pane honest; it is just not
# anybody's working state.
#
# Run it INSIDE the devcontainer, with the repository's .git reachable
# (a worktree's .git is a file pointing at an absolute host path, so that
# path has to be mounted too):
#
#   podman run --rm --userns=keep-id:uid=1000,gid=1000 \
#     -v "$PWD:/workspaces/taste-ide:z" \
#     -v "$REPO/.git:$REPO/.git:z" \
#     -v taste-ide-cargo:/home/dev/.cargo \
#     taste-ide-devcontainer sh build-aux/headless/fixture-repo.sh
#
# It prints the path it built, which is what `WORKSPACE=` wants:
#
#   WORKSPACE=$(sh build-aux/headless/fixture-repo.sh) \
#     sh build-aux/headless/shoot.sh hero
set -e

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# The directory's NAME is on screen: the window title and the file
# tree's root row are both the folder's basename, so the fixture has to be
# called what the project is called or every frame is titled after the
# scaffolding that took it.
FIXTURE="${FIXTURE:-/tmp/taste-shot/taste-ide}"

# Rebuilt from scratch every time. A fixture that accumulated state across
# runs would drift into being somebody's working checkout again, which is
# the thing it exists to avoid.
rm -rf "$FIXTURE"
mkdir -p "$FIXTURE"

# Tracked files only, at HEAD: no target/, no .gitignore'd scratch, nothing
# the archive does not carry. `git archive` is what makes that exact.
git -C "$ROOT" archive HEAD | tar -x -C "$FIXTURE"

cd "$FIXTURE"
git init -q -b main
# An identity for the fixture's one commit. Not the user's: these commits
# never leave /tmp, and borrowing a name for them would put it in a git log
# nobody asked for.
git -c user.email=fixture@taste.invalid -c user.name="Taste Screenshots" \
    -c commit.gpgsign=false \
    -c init.defaultBranch=main \
    add -A
git -c user.email=fixture@taste.invalid -c user.name="Taste Screenshots" \
    -c commit.gpgsign=false \
    commit -q -m "taste-ide"

# A session already under way, not a pristine clone. The tree's git columns,
# the Dirty filter's count and the commit box are a third of what the
# file-tree pane IS, and a screenshot of them all reading zero would be a
# screenshot of the one state a real workspace is never in.
#
# Edited rather than invented: real files, plausibly touched, so the badges
# point at rows a reader can recognise.
printf '\n// Scratch: measuring the row height against the panel.\n' \
    >> crates/taste-app/src/envstrip.rs
printf '\n// Scratch: the claim column wants a size group.\n' \
    >> crates/taste-app/src/backlog.rs
git -c user.email=fixture@taste.invalid -c user.name="Taste Screenshots" \
    add crates/taste-app/src/backlog.rs

echo "$FIXTURE"

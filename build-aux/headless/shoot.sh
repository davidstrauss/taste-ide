#!/bin/sh
# Take one docs/screenshots shot, headless, at the size the docs use.
#
# CLAUDE.md → Building describes this recipe; it lives here so it is a
# thing you run rather than a paragraph you reassemble. Broadway clamps
# the display to 1024x768 (see broadway-client.py), so a shot that must be
# 1440x900 wants Xvfb and the X11 backend instead — which is what this is.
#
# Run it INSIDE the devcontainer, from the workspace root:
#
#   podman run --rm --userns=keep-id:uid=1000,gid=1000 \
#     -v "$PWD:/workspaces/taste-ide:z" -v taste-ide-cargo:/home/dev/.cargo \
#     taste-ide-devcontainer sh build-aux/headless/shoot.sh watching
#
# The view name is a TASTE_PROBE_VIEW value; the shot lands in
# docs/screenshots/<view>.png, dark and optipng'd, exactly as the ones
# beside it were made.
#
# Set WORKSPACE= to shoot against a folder other than this checkout. The
# docs set is taken that way — see build-aux/headless/fixture-repo.sh —
# because the file-tree pane shows the branch you are on and what is dirty
# in it, and the frames should not document whichever branch the
# photographer happened to be standing on.
set -e

VIEW="${1:?usage: shoot.sh <probe-view> [probe-chat] [pane]}"
CHAT="${2:-}"
# Which pane's shot to keep. Most views are shot whole ("window"); the
# ones that are about a single pane do not shoot the window at all — see
# the target list in window.rs — so they name their pane here.
PANE="${3:-window}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

# Which folder the app OPENS. Defaults to this checkout, which is what you
# want while looking at a change in progress; the docs set overrides it with
# the fixture repository (build-aux/headless/fixture-repo.sh).
WORKSPACE="${WORKSPACE:-$ROOT}"

: "${DISPLAY_NUM:=:9}"
: "${SCREEN:=1440x900x24}"

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/taste-shoot-run}"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

Xvfb "$DISPLAY_NUM" -screen 0 "$SCREEN" >/tmp/xvfb.log 2>&1 &
XVFB=$!
# shellcheck disable=SC2064
trap "kill $XVFB 2>/dev/null || true" EXIT
# Xvfb is ready when its socket is, which is sooner than any fixed sleep
# and — unlike one — actually a guarantee.
i=0
while [ ! -e "/tmp/.X11-unix/X${DISPLAY_NUM#:}" ]; do
    i=$((i + 1))
    [ "$i" -gt 100 ] && { echo "Xvfb did not start; see /tmp/xvfb.log" >&2; exit 1; }
    sleep 0.1
done

# The screenshots are dark: it is the theme the app is designed against,
# and the one every existing shot in docs/screenshots was taken in.
DISPLAY="$DISPLAY_NUM" GDK_BACKEND=x11 \
    ADW_DEBUG_COLOR_SCHEME=prefer-dark \
    TASTE_PROBE_CHECK=1 TASTE_PROBE_VIEW="$VIEW" TASTE_PROBE_CHAT="$CHAT" \
    ./target/debug/taste-ide "$WORKSPACE" >/tmp/probe-run.log 2>&1 || {
    echo "the probe did not finish; see /tmp/probe-run.log" >&2
    tail -20 /tmp/probe-run.log >&2
    exit 1
}

SHOT="/tmp/probe-$PANE.png"
[ -f "$SHOT" ] || { echo "no $SHOT was written; see /tmp/probe-run.log" >&2; exit 1; }

# Blank-frame guard. The probe shoots on a timer, and on a cold start —
# the first run after a rebuild, or a loaded machine — it can fire before
# the window has painted, producing a uniform rectangle. A flat image
# compresses to almost nothing, so its SIZE is a reliable and cheap tell:
# a real shot is tens to hundreds of KB, a blank one a few. A single
# pane is smaller than the window, so its floor is lower.
SIZE=$(wc -c < "$SHOT")
MIN=${MIN_SHOT_BYTES:-40000}
[ "$PANE" = "window" ] || MIN=${MIN_SHOT_BYTES:-4000}
if [ "$SIZE" -lt "$MIN" ]; then
    echo "blank frame ($SIZE bytes) — the window had not painted when the probe fired." >&2
    echo "Run it again; the second run is warm." >&2
    exit 2
fi

mkdir -p docs/screenshots
cp "$SHOT" "docs/screenshots/$VIEW.png"
optipng -quiet -o5 "docs/screenshots/$VIEW.png"
echo "docs/screenshots/$VIEW.png"

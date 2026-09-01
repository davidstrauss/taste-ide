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
set -e

VIEW="${1:?usage: shoot.sh <probe-view> [probe-chat]}"
CHAT="${2:-}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

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
    ./target/debug/taste-ide "$ROOT" >/tmp/probe-run.log 2>&1 || {
    echo "the probe did not finish; see /tmp/probe-run.log" >&2
    tail -20 /tmp/probe-run.log >&2
    exit 1
}

[ -f /tmp/probe-window.png ] || { echo "no /tmp/probe-window.png was written" >&2; exit 1; }
mkdir -p docs/screenshots
cp /tmp/probe-window.png "docs/screenshots/$VIEW.png"
optipng -quiet -o5 "docs/screenshots/$VIEW.png"
echo "docs/screenshots/$VIEW.png"

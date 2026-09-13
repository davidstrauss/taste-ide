#!/usr/bin/env bash
# Type into Dispatch, headless, with every GTK CRITICAL fatal.
#
# The reproduction for i-0023 — `gdk_popup_present: assertion 'width > 0'
# failed`, which David hit once per typing burst. An assertion is a line in
# a log nobody reads until something fails on it, so this run makes it an
# abort: `G_DEBUG=fatal-criticals`, and the exit code is the verdict.
#
# Typing is not the same act as setting the text, which is why this exists
# beside shoot.sh rather than being a probe view. The slash-command
# completion, the buttons' per-keystroke tooltips, the restyle debounce and
# the completion popup's own frame clock all do their work in the GAPS
# between keystrokes; text put in the box in one go turns the main loop
# once and exercises none of it.
#
# Run it INSIDE the devcontainer, from the workspace root, after a build:
#
#   podman run --rm --userns=keep-id:uid=1000,gid=1000 \
#     -v "$PWD:/workspaces/taste-ide:z" -v taste-ide-cargo:/home/dev/.cargo \
#     taste-ide-devcontainer sh build-aux/headless/typing.sh
#
# With no argument it types the burst that used to fail; pass your own text
# to pose another one. TYPE_MS sets the gap between keystrokes.
set -euo pipefail

# The default burst, and every word in it is load-bearing:
#
#   - "container", "clone" and "comes" are ordinary prose words that PREFIX
#     command names (compact, context, clear), which is what used to pull
#     the command list up over a sentence — and each opening was a chance
#     at the zero-width popup.
#   - "/co" is a real slash command being completed, which still has to
#     open the list.
#   - "/cozz" runs past the last match, which is where the list has to
#     close rather than shrink to nothing.
TEXT="${1:-Relocation waits for the container and the clone comes up. /co /cozz}"

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

WORKSPACE="${WORKSPACE:-$ROOT}"
BIN="${BIN:-./target/debug/taste-ide}"
[ -x "$BIN" ] || { echo "no $BIN — build first" >&2; exit 1; }

: "${DISPLAY_NUM:=:11}"
: "${SCREEN:=1440x900x24}"

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/taste-typing-run}"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

rm -f "/tmp/.X11-unix/X${DISPLAY_NUM#:}"
Xvfb "$DISPLAY_NUM" -screen 0 "$SCREEN" >/tmp/xvfb-typing.log 2>&1 &
XVFB=$!
# shellcheck disable=SC2064
trap "kill $XVFB 2>/dev/null || true" EXIT
i=0
while [ ! -e "/tmp/.X11-unix/X${DISPLAY_NUM#:}" ]; do
    i=$((i + 1))
    [ "$i" -gt 100 ] && { echo "Xvfb did not start; see /tmp/xvfb-typing.log" >&2; exit 1; }
    sleep 0.1
done

# Under a session bus, because fatal-criticals is indiscriminate: without
# one, GIO complains about the bus it could not reach and the run aborts on
# a message that has nothing to do with the window.
set +e
dbus-run-session -- env \
    DISPLAY="$DISPLAY_NUM" GDK_BACKEND=x11 \
    ADW_DEBUG_COLOR_SCHEME=prefer-dark \
    G_DEBUG=fatal-criticals \
    TASTE_PROBE_CHECK=1 TASTE_PROBE_VIEW=backlog-composer \
    TASTE_PROBE_TYPE="$TEXT" TASTE_PROBE_TYPE_MS="${TYPE_MS:-60}" \
    "$BIN" "$WORKSPACE" >/tmp/typing-run.log 2>&1
STATUS=$?
set -e

if [ "$STATUS" -ne 0 ]; then
    echo "typing \"$TEXT\" failed an assertion (exit $STATUS):" >&2
    grep -E 'CRITICAL' /tmp/typing-run.log | tail -10 >&2
    echo "full log: /tmp/typing-run.log" >&2
    exit "$STATUS"
fi

echo "typed \"$TEXT\" with no assertion failed"

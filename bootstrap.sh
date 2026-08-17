#!/usr/bin/env bash
# taste-ide bootstrap: stock Silverblue → self-hosting, one command.
#
# Requires nothing beyond the Silverblue base image (podman). Builds the
# project's devcontainer image, builds the IDE inside it, and launches it
# with Wayland + GPU forwarded — running against this very repository.
# Idempotent: image layers and cargo artifacts are cached, so subsequent
# runs go straight to launch.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NAME="$(basename "$ROOT")"
WORKSPACE="/workspaces/$NAME"
IMAGE="taste-ide-devcontainer"
WAYLAND="${WAYLAND_DISPLAY:-wayland-0}"

command -v podman >/dev/null || {
    echo "error: podman is required (it is part of the Silverblue base image)" >&2
    exit 1
}
[ -S "${XDG_RUNTIME_DIR}/${WAYLAND}" ] || {
    echo "error: no Wayland socket at ${XDG_RUNTIME_DIR}/${WAYLAND}" >&2
    exit 1
}

# Host-side desktop integration for dev runs: GNOME resolves the app
# switcher icon by matching the window's app-id against a desktop file on
# the host, so install the .Devel-badged identity into the user's data
# dirs. Idempotent; the packaged Flatpak ships the unbadged identity.
DEV_ID="net.davidstrauss.Taste.Devel"
install -Dm644 "$ROOT/data/icons/hicolor/scalable/apps/$DEV_ID.svg" \
    "$HOME/.local/share/icons/hicolor/scalable/apps/$DEV_ID.svg"
mkdir -p "$HOME/.local/share/applications"
sed "s|@BOOTSTRAP@|$ROOT/bootstrap.sh|" "$ROOT/data/$DEV_ID.desktop" \
    > "$HOME/.local/share/applications/$DEV_ID.desktop"

echo "==> devcontainer image ($IMAGE)"
podman build -q -t "$IMAGE" "$ROOT/.devcontainer" >/dev/null

# The container has no Settings portal, so it cannot see the desktop's
# dark/light preference; forward it explicitly (static per launch).
COLOR_SCHEME=default
if command -v gsettings >/dev/null 2>&1 \
    && gsettings get org.gnome.desktop.interface color-scheme 2>/dev/null | grep -q dark; then
    COLOR_SCHEME=prefer-dark
fi

# Host URL opener: the container has no browser. The app (and only the
# app — the token env is stripped before any agent spawns) drops
# token-named URL files here; this host-side loop opens them. The token
# keeps container processes from driving the host browser directly.
OPEN_DIR=$(mktemp -d)
# head reads a fixed count from urandom directly: no SIGPIPE, which
# `set -o pipefail` would otherwise turn into a silent exit 141.
OPEN_TOKEN=$(head -c 16 /dev/urandom | od -An -tx1 | tr -d " \n")
(
    while [ -d "$OPEN_DIR" ]; do
        for f in "$OPEN_DIR/$OPEN_TOKEN".*; do
            [ -e "$f" ] || continue
            url=$(head -c 2048 "$f")
            rm -f "$f"
            case "$url" in
            http://* | https://*) xdg-open "$url" >/dev/null 2>&1 & ;;
            esac
        done
        sleep 0.3
    done
) &

echo "==> build + launch (workspace: $WORKSPACE)"
# --network=host   sign-in OAuth callbacks reach the agent
# --device /dev/dri GPU rendering (logind's seat ACL maps through keep-id)
# label=disable    the Wayland socket cannot be relabeled
# taste-ide-home   persists agent sign-ins and caches across runs
# tmpfs runtime dir owned by the user (U=true chowns it — podman 5 rejects
# raw uid=/gid= tmpfs options): dconf and friends need a writable
# XDG_RUNTIME_DIR (only the Wayland socket is bound in from the host).
# The session bus is a forked dbus-daemon (not dbus-run-session) and the
# app execs directly under podman's --init, so Ctrl+C on this console
# reaches the app itself and closes it gracefully (state saved).
run_status=0
podman run --rm \
    --init \
    --userns=keep-id:uid=1000,gid=1000 \
    --security-opt label=disable \
    --network=host \
    --device /dev/dri \
    --mount "type=tmpfs,dst=/run/user/1000,tmpfs-mode=0700,U=true" \
    -v "$ROOT:$WORKSPACE" \
    -v taste-ide-home:/home/dev \
    -v taste-ide-cargo:/home/dev/.cargo \
    -v "${XDG_RUNTIME_DIR}/${WAYLAND}:/run/user/1000/${WAYLAND}" \
    -e "WAYLAND_DISPLAY=${WAYLAND}" \
    -e XDG_RUNTIME_DIR=/run/user/1000 \
    -e "ADW_DEBUG_COLOR_SCHEME=${COLOR_SCHEME}" \
    -v "$OPEN_DIR:/run/taste-host-open" \
    -e TASTE_HOST_OPEN_DIR=/run/taste-host-open \
    -e "TASTE_HOST_OPEN_TOKEN=$OPEN_TOKEN" \
    "$IMAGE" \
    bash -c "cd '$WORKSPACE' && cargo build --workspace \
        && export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
        && dbus-daemon --session --address=\"\$DBUS_SESSION_BUS_ADDRESS\" --fork \
        && exec ./target/debug/taste-ide '$WORKSPACE'" || run_status=$?
# Cleanup must run even when the app exits non-zero (set -e would
# otherwise skip it and leave the URL-opener loop running forever).
rm -rf "$OPEN_DIR"
exit $run_status

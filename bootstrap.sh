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

# Build the devcontainer image, showing the steps and — the part that
# reads as a hang — saying so when a step goes quiet. After a RUN's last
# line podman commits the layer, and under rootless fuse-overlayfs a
# multi-gigabyte layer is minutes of silence before the layer id prints.
# So: podman's own output, filtered to the step lines, plus a note every
# 20 quiet seconds saying what is happening and for how long. A cached
# image finishes in a second and prints one line.
build_image() {
    local line step="" quiet=0 total=0 lines=0
    # `read -t` times out with a status above 128 and leaves the loop
    # running; a real EOF is a status of 1. The pipe's status is podman's,
    # via pipefail — so a failed build still fails the bootstrap.
    podman build -t "$IMAGE" "$ROOT/.devcontainer" 2>&1 | while true; do
        if IFS= read -r -t 20 line; then
            quiet=0
            case "$line" in
                STEP\ *) step="${line%%: *}"; total=0; lines=0; echo "    $line" ;;
                --\>\ *|COMMIT\ *) [ "$lines" -gt 0 ] && echo "    $step done"; lines=0 ;;
                *) lines=$((lines + 1)) ;;
            esac
        else
            status=$?
            [ "$status" -gt 128 ] || break   # EOF: podman is finished
            quiet=$((quiet + 20)); total=$((total + 20))
            if [ -n "$step" ]; then
                if [ "$quiet" -eq 20 ] && [ "$lines" -gt 0 ]; then
                    echo "    … $step has printed nothing for ${quiet}s: its command is done and podman is committing the layer, which for a large layer under rootless fuse-overlayfs takes minutes and prints nothing until the layer id lands."
                else
                    echo "    … still $step: quiet for ${quiet}s"
                fi
            fi
        fi
    done
}
WAYLAND="${WAYLAND_DISPLAY:-wayland-0}"

# --flatpak: the production build — build, install, and run the real
# Flatpak on the host. This is how you exercise devcontainer supervision:
# the self-hosting container run cannot nest podman, the host app can.
if [ "${1:-}" = "--flatpak" ]; then
    command -v flatpak >/dev/null || {
        echo "error: flatpak is required (part of the Silverblue base)" >&2
        exit 1
    }
    flatpak remote-add --if-not-exists --user flathub \
        https://dl.flathub.org/repo/flathub.flatpakrepo
    flatpak install -y --user --noninteractive flathub org.flatpak.Builder
    flatpak run org.flatpak.Builder --user --force-clean --ccache \
        --install-deps-from=flathub --install \
        "$ROOT/build-aux/flatpak/.build" \
        "$ROOT/build-aux/flatpak/net.davidstrauss.Taste.json"
    exec flatpak run net.davidstrauss.Taste "$ROOT"
fi

# --host: the fast path — build in the container, run the binary on the
# host (works: libgit2 is vendored). Real portals, real devcontainer
# supervision; agents run confined in the devcontainer image.
if [ "${1:-}" = "--host" ]; then
    build_image
    # :z (shared), never :Z (private). A private relabel stamps this
    # container's own MCS categories onto the workspace, taking it from
    # a devcontainer already running under different ones — which denies
    # every process in there access to the project, the user's own
    # terminals included. Shared labelling leaves it reachable from both.
    podman run --rm --userns=keep-id:uid=1000,gid=1000 \
        -v "$ROOT:$WORKSPACE:z" -v taste-ide-cargo:/home/dev/.cargo \
        "$IMAGE" bash -c "cd '$WORKSPACE' && cargo build --workspace"
    export TASTE_AGENT_IMAGE="$IMAGE"
    exec "$ROOT/target/debug/taste-ide" "$ROOT"
fi

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
build_image

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
# Git identity: the container commits as the host user. The IDE's own
# supervisor does this for project devcontainers; the bootstrap container
# has no supervisor outside it, so the script inherits it here — only
# when the taste-ide-home volume doesn't already carry one.
GIT_NAME=$(git config --get user.name 2>/dev/null || true)
GIT_EMAIL=$(git config --get user.email 2>/dev/null || true)

# NO container runtime reaches this container. Forwarding the host's podman
# socket would hand every process in here — the agent, and any code the repo
# builds or tests — the ability to start host containers with arbitrary
# mounts, which is host root by another name. The IDE therefore runs
# self-hosted under the fallback semantics: the environment IS this
# container, and devcontainer lifecycle (build, reload, nuke) belongs to a
# host-side IDE. See docs/ARCHITECTURE.md → Self-hosting.

# The microphone, for voice input (docs/spikes/one-composer-and-evidence.md
# → Voice): PipeWire's socket, and its PulseAudio compatibility socket for
# GStreamer's pulsesrc, mounted when the host has them. Capture is the
# IDE's own process, never an agent's — the sockets land in the IDE's
# runtime dir, which no container the IDE supervises can see.
AUDIO_MOUNTS=""
for sock in pipewire-0 pulse/native; do
    if [ -S "${XDG_RUNTIME_DIR}/${sock}" ]; then
        AUDIO_MOUNTS="$AUDIO_MOUNTS -v ${XDG_RUNTIME_DIR}/${sock}:/run/user/1000/${sock}"
    fi
done

# A game controller, if one is ever plugged in: its evdev node is the
# seat's to read (systemd's uaccess), and the container sees it only if
# the directory is in.
INPUT_MOUNT=""
[ -d /dev/input ] && INPUT_MOUNT="-v /dev/input:/dev/input"
run_status=0
podman run --rm \
    --init \
    --userns=keep-id:uid=1000,gid=1000 \
    --security-opt label=disable \
    --network=host \
    --device /dev/dri \
    $INPUT_MOUNT \
    --mount "type=tmpfs,dst=/run/user/1000,tmpfs-mode=0700,U=true" \
    -v "$ROOT:$WORKSPACE" \
    -v taste-ide-home:/home/dev \
    -v taste-ide-cargo:/home/dev/.cargo \
    -v "${XDG_RUNTIME_DIR}/${WAYLAND}:/run/user/1000/${WAYLAND}" \
    -e "WAYLAND_DISPLAY=${WAYLAND}" \
    $AUDIO_MOUNTS \
    -e XDG_RUNTIME_DIR=/run/user/1000 \
    -e "ADW_DEBUG_COLOR_SCHEME=${COLOR_SCHEME}" \
    -v "$OPEN_DIR:/run/taste-host-open" \
    -e TASTE_HOST_OPEN_DIR=/run/taste-host-open \
    -e "TASTE_HOST_OPEN_TOKEN=$OPEN_TOKEN" \
    -e "TASTE_GIT_NAME=${GIT_NAME}" \
    -e "TASTE_GIT_EMAIL=${GIT_EMAIL}" \
    "$IMAGE" \
    bash -c "cd '$WORKSPACE' \
        && if [ -n \"\$TASTE_GIT_NAME\" ] && [ -n \"\$TASTE_GIT_EMAIL\" ] \
           && ! git config --global --get user.email >/dev/null; then \
               git config --global user.name \"\$TASTE_GIT_NAME\"; \
               git config --global user.email \"\$TASTE_GIT_EMAIL\"; \
           fi \
        && cargo build --workspace \
        && export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
        && dbus-daemon --session --address=\"\$DBUS_SESSION_BUS_ADDRESS\" --fork \
        && exec ./target/debug/taste-ide '$WORKSPACE'" || run_status=$?
# Cleanup must run even when the app exits non-zero (set -e would
# otherwise skip it and leave the URL-opener loop running forever).
rm -rf "$OPEN_DIR"
exit $run_status

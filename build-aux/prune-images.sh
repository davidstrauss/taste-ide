#!/usr/bin/env bash
# Remove the IDE's stale devcontainer images.
#
# Every devcontainer config edit mints a new `taste-img-<config-hash>`, and
# nothing has ever removed the one it replaced: sixteen days of edits on the
# author's host left 50 tagged images, 6 of them referenced by a container.
# This is that cleanup as a thing you run, ahead of the IDE doing it at the
# moment a new image replaces an old one.
#
# ## What decides that an image is garbage
#
# **Reachability, never ownership.** An image tag is content-addressed on
# the devcontainer config's own bytes, so two projects with byte-identical
# configs deliberately share one image (`taste_core::environment::
# env_image_tag`). Images do carry a `taste.workspace` label, but it names
# only whichever workspace built the image LAST — a shared name cannot
# express sole ownership — so a pruner keyed on that label would delete
# another window's image because ours happened to build it. This script
# therefore never reads the label to decide anything; it reads it only to
# group generations for `--keep`.
#
# An image is kept when a container still points at it, in any state and in
# any workspace, because every environment that exists has one. It is kept
# when it is among the newest `--keep` generations, so reverting a config
# edit is a tag flip rather than a rebuild. Everything else `taste-img-*` is
# a previous generation and goes.
#
# The safety argument is the one `taste_devcontainer::machine` already makes
# about the VM: images are cattle. The worst case for getting this wrong is
# a rebuild, never lost work.
#
# ## What it will not touch
#
# Anything not named `taste-img-*`. No `podman system prune`, no other
# tool's images (`vsc-*` from VS Code devcontainers), no base images. The
# rule `Supervisor::remove_volume` states applies here too: this manages
# this IDE's containers, not podman at large.
#
# Removal is by TAG, never by image id. Two config hashes that build to
# byte-identical layers share one id — there are such pairs on the author's
# host — and removing the id would untag a generation that is still live.
# Podman frees the storage when the last tag on an id goes, which is the
# behaviour wanted here and the reason the reclaim estimate below is an
# upper bound rather than a figure.
#
# Usage:
#
#   build-aux/prune-images.sh                 # say what would go, remove nothing
#   build-aux/prune-images.sh --apply         # remove it
#   build-aux/prune-images.sh --keep 0        # keep no previous generation
#   build-aux/prune-images.sh --apply --dangling
#
# `--dangling` adds untagged images older than an hour, left by rebuilds.
# Opt-in and age-bounded on purpose: an untagged image can be an
# intermediate layer of a build running in another window right now, and an
# hour is longer than any build here takes.
set -euo pipefail

APPLY=0
KEEP=1
DANGLING=0

while [ $# -gt 0 ]; do
    case "$1" in
        --apply) APPLY=1 ;;
        --dangling) DANGLING=1 ;;
        --keep) KEEP="${2:?--keep needs a number}"; shift ;;
        --keep=*) KEEP="${1#--keep=}" ;;
        -h|--help) sed -n '2,60p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1 (try --help)" >&2; exit 2 ;;
    esac
    shift
done
case "$KEEP" in
    ''|*[!0-9]*) echo "--keep takes a number, got: $KEEP" >&2; exit 2 ;;
esac

# Image ids every container points at, whatever state it is in. A stopped
# environment wants its image as much as a running one does, and an exited
# container is how most of the fleet spends its time.
referenced() {
    podman ps -a --format '{{.ImageID}}' 2>/dev/null | sort -u
}

# The IDE's own images, newest first, as `id<TAB>tag<TAB>workspace`. The
# `CreatedAt` field is zero-padded UTC, so it sorts as text.
ours() {
    podman images \
        --format '{{.CreatedAt}}\t{{.ID}}\t{{.Repository}}:{{.Tag}}\t{{index .Labels "taste.workspace"}}' \
        2>/dev/null |
        grep -F 'taste-img-' |
        sort -r |
        cut -f2-
}

REFERENCED="$(referenced)"
KEPT_LIVE=0
KEPT_RECENT=0
declare -A SEEN_PER_WORKSPACE=()
COLLECT=()

while IFS=$'\t' read -r id tag workspace; do
    [ -n "${tag:-}" ] || continue
    if printf '%s\n' "$REFERENCED" | grep -qx "$id"; then
        KEPT_LIVE=$((KEPT_LIVE + 1))
        continue
    fi
    # Generations are grouped by the label only to count them. An image
    # built before labelling, or by something else entirely, groups under
    # `-` and is counted on its own.
    group="${workspace:--}"
    seen="${SEEN_PER_WORKSPACE[$group]:-0}"
    if [ "$seen" -lt "$KEEP" ]; then
        SEEN_PER_WORKSPACE[$group]=$((seen + 1))
        KEPT_RECENT=$((KEPT_RECENT + 1))
        continue
    fi
    COLLECT+=("$tag")
done < <(ours)

echo "Kept: $KEPT_LIVE in use by a container, $KEPT_RECENT as the newest $KEEP per workspace."

if [ "${#COLLECT[@]}" -eq 0 ]; then
    echo "Nothing to collect."
else
    echo "Collectable (${#COLLECT[@]}):"
    printf '  %s\n' "${COLLECT[@]}"
    # An upper bound, and said as one: these tags share layers with each
    # other and with the images being kept, so podman will free less than
    # their sizes add up to — sometimes far less.
    bound=$(podman images --format '{{.Repository}}:{{.Tag}}\t{{.Size}}' 2>/dev/null |
        grep -F -f <(printf '%s\n' "${COLLECT[@]}") |
        awk -F'\t' '{split($2,s," ");
                     if (s[2]=="GB") b+=s[1]*1000; else if (s[2]=="MB") b+=s[1];
                     else if (s[2]=="kB") b+=s[1]/1000}
                    END {printf "%.1f", b/1000}')
    echo "  Upper bound on what this frees: ${bound} GB (shared layers mean less)."
fi

if [ "$DANGLING" -eq 1 ]; then
    mapfile -t DANGLERS < <(podman images -q --filter dangling=true --filter until=1h 2>/dev/null)
    echo "Dangling, untagged, older than an hour: ${#DANGLERS[@]}."
fi

if [ "$APPLY" -eq 0 ]; then
    echo
    echo "Nothing was removed. Re-run with --apply to remove it."
    exit 0
fi

if [ "${#COLLECT[@]}" -gt 0 ]; then
    echo
    echo "Removing ${#COLLECT[@]} tags..."
    # One at a time, and a failure is reported rather than fatal: an image
    # can be claimed by a container started since the scan, and the right
    # answer to that is to leave it and say so.
    for tag in "${COLLECT[@]}"; do
        if podman rmi "$tag" >/dev/null 2>&1; then
            echo "  removed $tag"
        else
            echo "  KEPT $tag (podman refused it — in use since the scan?)"
        fi
    done
fi

if [ "$DANGLING" -eq 1 ] && [ "${#DANGLERS[@]}" -gt 0 ]; then
    echo "Removing ${#DANGLERS[@]} dangling images..."
    for id in "${DANGLERS[@]}"; do
        podman rmi "$id" >/dev/null 2>&1 && echo "  removed $id" || echo "  KEPT $id"
    done
fi

echo
echo "Images now:"
podman system df 2>/dev/null | sed -n '1p;/^Images/p' | sed 's/^/  /'

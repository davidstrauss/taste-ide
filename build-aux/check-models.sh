#!/bin/sh
# Model-check the designs under docs/models with TLC, each configuration
# held to the outcome it must have: "holds", or the one invariant it must
# break, which is how a model shows it can see the bug a design closed
# rather than passing because it sees nothing.
#
# The sync's pass, docs/models/Sync.tla (docs/SYNC.md, layer 4): every interleaving of edits on both sides with every step of
# every pass, over a small universe. Four configurations, each with the
# outcome it must have:
#
#   Sync          writes checked at the write, the checkout observed by the
#                 pass itself: the sync as it is. Every property holds.
#   SyncNotFresh  checked writes, the checkout observed through a snapshot
#                 something else took: an edit is lost all the same. A
#                 snapshot from before a pass's own writes reads them as
#                 the checkout changing back, and the write that carries
#                 that "change" passes its check, since the side it lands on
#                 holds exactly what the pass expected. The check protects
#                 the side written; only a fresh look protects the side read
#                 from. (TLC found this; it was expected to hold.)
#   SyncNoCas     unchecked writes, the checkout observed by the pass: an
#                 edit between the pass's look and its write is lost.
#   SyncOld       neither: the sync before 2026-10-12. The edit is lost.
#
# The last three must break Safety, which is how this shows that the model
# can see the bugs the redesign closed, rather than passing because it sees
# nothing — and why the sync needs both halves of it.
#
# Then docs/models/SyncGit.tla, the same pass with git around it — commits,
# branches, Personal carried onto the folder's commits through a stash, each
# side following the other's switch — on four more:
#
#   SyncGit           the sync as it is: every property holds. The longest
#                     run here, about twenty minutes.
#   SyncGitIgnored    a fast-forward or a switch overwrites an ignored file,
#                     as git's defaults do: a version of Personal's is lost.
#   SyncGitHardReset  a stash that will not pop back is cleaned with reset
#                     --hard: an agent's edit made meanwhile is lost.
#   SyncGitClean      reset --hard and git clean: likewise.
#
# Then docs/models/Calls.tla, how the MCP server answers a connection's
# tool calls (docs/MCP-CALLS.md): every call answered, or refused, within
# its deadline, whatever the calls beside it do.
#
#   Calls            as built: every property holds.
#   CallsAsWas       the deadline inside the work, counted from the permit:
#                    a call whose work blocks its thread is never answered.
#   CallsFromPermit  the deadline apart, counted from the permit: a call
#                    queued behind slow work waits past it.
#   CallsBeside      the deadline a task beside the work, on its workers:
#                    work blocked in all of them leaves none to answer.
#   CallsFreeSlot    the permit given back at the answer: blocked work grows
#                    past the permits.
#
# `--quick` leaves out the long run.
#
# TLC runs from the release's own jar, checked against its digest, in a JRE
# image pinned by digest; neither is installed on this host. Each run is
# capped at 6 GiB.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
JAR_VERSION=1.7.4
JAR_SHA256=936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88
JAR_URL="https://github.com/tlaplus/tlaplus/releases/download/v$JAR_VERSION/tla2tools.jar"
IMAGE=docker.io/library/eclipse-temurin@sha256:80b6aebe2f990697ada73fdbe451c7e9ec5e4adbde7a1e3625bc665e3b2fc06e
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/taste-ide/tla"
JAR="$CACHE/tla2tools-$JAR_VERSION.jar"

mkdir -p "$CACHE"
if [ ! -f "$JAR" ]; then
    curl -sfL -o "$JAR.part" "$JAR_URL"
    mv "$JAR.part" "$JAR"
fi
echo "$JAR_SHA256  $JAR" | sha256sum -c --quiet

failed=0
check() {
    config=$1
    want=$2
    module=${3:-Sync}
    out=$(podman run --rm --memory=6g --memory-swap=6g \
        -v "$ROOT/docs/models:/models:ro,z" -v "$CACHE:/tla:ro,z" \
        "$IMAGE" java -Xmx4g -XX:+UseParallelGC -cp "/tla/tla2tools-$JAR_VERSION.jar" \
        tlc2.TLC -workers 4 -metadir /tmp/tlc -config "/models/$config.cfg" "/models/$module.tla" 2>&1) || true
    states=$(printf '%s\n' "$out" | grep -E 'distinct states found' | tail -1)
    if printf '%s\n' "$out" | grep -q 'No error has been found'; then
        got=holds
    else
        broke=$(printf '%s\n' "$out" | sed -n 's/^Error: Invariant \([A-Za-z]*\) is violated.*/\1/p' | head -1)
        got=${broke:+breaks-$broke}
        got=${got:-other}
    fi
    if [ "$got" = "$want" ]; then
        echo "ok    $config: $got ($states)"
    else
        echo "FAIL  $config: wanted $want, got $got"
        printf '%s\n' "$out" | tail -40
        failed=1
    fi
}

check Sync holds
check SyncNotFresh breaks-Safety
check SyncNoCas breaks-Safety
check SyncOld breaks-Safety
check SyncGitIgnored breaks-Safety SyncGit
check SyncGitHardReset breaks-Safety SyncGit
check SyncGitClean breaks-Safety SyncGit
check Calls holds Calls
check CallsAsWas breaks-AnsweredInTime Calls
check CallsFromPermit breaks-AnsweredInTime Calls
check CallsBeside breaks-AnsweredInTime Calls
check CallsFreeSlot breaks-Contained Calls
[ "${1:-}" = "--quick" ] || check SyncGit holds SyncGit
exit $failed

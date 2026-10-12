#!/bin/sh
# Model-check the sync's pass, docs/models/Sync.tla, with TLC (docs/SYNC.md,
# layer 4): every interleaving of edits on both sides with every step of
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
# The last three must FAIL Safety, which is how this shows that the model
# can see the bugs the redesign closed, rather than passing because it sees
# nothing — and why the sync needs both halves of it.
#
# TLC runs from the release's own jar, checked against its digest, in a JRE
# image pinned by digest; neither is installed on this host. Each run is
# capped at 4 GiB.
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
    out=$(podman run --rm --memory=4g --memory-swap=4g \
        -v "$ROOT/docs/models:/models:ro,z" -v "$CACHE:/tla:ro,z" \
        "$IMAGE" java -XX:+UseParallelGC -cp "/tla/tla2tools-$JAR_VERSION.jar" \
        tlc2.TLC -workers 4 -metadir /tmp/tlc -config "/models/$config.cfg" /models/Sync.tla 2>&1) || true
    states=$(printf '%s\n' "$out" | grep -E 'distinct states found' | tail -1)
    if printf '%s\n' "$out" | grep -q 'No error has been found'; then
        got=holds
    elif printf '%s\n' "$out" | grep -q 'Invariant Safety is violated'; then
        got=loses-an-edit
    else
        got=other
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
check SyncNotFresh loses-an-edit
check SyncNoCas loses-an-edit
check SyncOld loses-an-edit
exit $failed

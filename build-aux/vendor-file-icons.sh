#!/usr/bin/env bash
# Vendor the file-type icons: Material Icon Theme, pinned.
#
# The tree, the editor's tabs, and the search's file rows draw a file's
# icon from this set (`crates/taste-app/src/file_icons.rs`), because the
# desktop's own theme has about thirty file-type icons, faint ones, and
# none for a Cargo.toml or a Taskfile (David, 2026-09-28: "I'd prefer an
# icon set with more color and variety for specific formats").
#
# What it writes, all of it committed:
#   data/icons/hicolor/scalable/mimetypes/taste-mi-<name>.svg
#       every icon, prefixed so no desktop theme's name can shadow one;
#       the app already adds data/icons to the icon theme, and the Flatpak
#       installs the directory.
#   data/file-icons/material-icons.json
#       the set's own manifest — file names, extensions, folder names, and
#       the light theme's overrides — read by file_icons.rs.
#   data/file-icons/LICENSE, data/file-icons/VERSION
#
# Bump by changing VERSION and INTEGRITY together: the integrity is npm's
# own `dist.integrity` for that version, checked before anything is
# unpacked. Needs curl, openssl, base64, and tar; no node.
set -euo pipefail

VERSION=5.38.1
INTEGRITY="sha512-14cFM4NJGdbuo68rIZTq9TSX0f5BtA6VF+eNX3zq23Z5NoEeVtM4Zn4PpZ0ERl++50jCBrywKFWXmOjbxf0xTA=="

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
URL="https://registry.npmjs.org/material-icon-theme/-/material-icon-theme-${VERSION}.tgz"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

curl -fsSL "$URL" -o "$WORK/package.tgz"
actual="sha512-$(openssl dgst -sha512 -binary "$WORK/package.tgz" | base64 -w0)"
if [ "$actual" != "$INTEGRITY" ]; then
    echo "material-icon-theme ${VERSION}: integrity $actual, expected $INTEGRITY" >&2
    exit 1
fi

tar -xzf "$WORK/package.tgz" -C "$WORK"
icons="$ROOT/data/icons/hicolor/scalable/mimetypes"
rm -f "$icons"/taste-mi-*.svg
mkdir -p "$icons" "$ROOT/data/file-icons"
for svg in "$WORK"/package/icons/*.svg; do
    cp "$svg" "$icons/taste-mi-$(basename "$svg")"
done
cp "$WORK/package/dist/material-icons.json" "$ROOT/data/file-icons/material-icons.json"
cp "$WORK/package/LICENSE" "$ROOT/data/file-icons/LICENSE"
echo "$VERSION" > "$ROOT/data/file-icons/VERSION"
echo "vendored material-icon-theme ${VERSION}: $(ls "$icons"/taste-mi-*.svg | wc -l) icons"

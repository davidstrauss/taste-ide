#!/usr/bin/env bash
# Vendor pdf.js, pinned: what the editor's viewer draws a PDF with
# (`crates/taste-app/src/viewer.rs`), in the WebKit view the port tabs
# already use (David, 2026-10-05: "Add the ability to view PDFs in the file
# editor"). Poppler, the other way to draw one, is not in GNOME's Flatpak
# runtime, and pdf.js brings text selection, find, and zoom with it.
#
# What it writes, all of it committed, and served to the viewer out of the
# binary (`include_bytes!`) rather than from disk:
#   crates/taste-app/vendor/pdfjs/pdf.min.mjs, pdf.worker.min.mjs
#       the library and its worker
#   crates/taste-app/vendor/pdfjs/pdf_viewer.mjs, pdf_viewer.css
#       the viewer components: pages, the text layer, find
#   crates/taste-app/vendor/pdfjs/LICENSE, VERSION
#
# The full viewer's toolbar images are left out; this page has no toolbar.
# Bump by changing VERSION and INTEGRITY together: the integrity is npm's
# own `dist.integrity` for that version, checked before anything is
# unpacked. Needs curl, openssl, base64, and tar; no node.
set -euo pipefail

VERSION=6.4.299
INTEGRITY="sha512-AVl138zALtfaAPvADulE0PZThbYzCBS79nL4pOSL/6Sm/4AH5A21BD9VHt97OlCuzJuCpmeZtAtkinisF4Vb1g=="

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
URL="https://registry.npmjs.org/pdfjs-dist/-/pdfjs-dist-${VERSION}.tgz"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

curl -fsSL "$URL" -o "$WORK/package.tgz"
actual="sha512-$(openssl dgst -sha512 -binary "$WORK/package.tgz" | base64 -w0)"
if [ "$actual" != "$INTEGRITY" ]; then
    echo "pdfjs-dist ${VERSION}: integrity $actual, expected $INTEGRITY" >&2
    exit 1
fi

tar -xzf "$WORK/package.tgz" -C "$WORK"
OUT="$ROOT/crates/taste-app/vendor/pdfjs"
rm -rf "$OUT"
mkdir -p "$OUT"
cp "$WORK/package/build/pdf.min.mjs" "$WORK/package/build/pdf.worker.min.mjs" "$OUT/"
cp "$WORK/package/web/pdf_viewer.mjs" "$WORK/package/web/pdf_viewer.css" "$OUT/"
cp "$WORK/package/LICENSE" "$OUT/LICENSE"
echo "$VERSION" > "$OUT/VERSION"
echo "pdf.js ${VERSION} vendored into ${OUT#"$ROOT"/}"

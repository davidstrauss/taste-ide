#!/bin/sh
# Remake the web archive fixtures beside this script from ../html, inside
# the devcontainer (it fetches warcio, Webrecorder's WARC library):
#
#   podman run --rm --userns=keep-id:uid=1000,gid=1000 \
#     -v "$PWD:/workspaces/taste-ide:z" -w /workspaces/taste-ide \
#     taste-ide-devcontainer sh build-aux/headless/fixtures/archives/make.sh
#
# page.warc.gz is real HTTP responses from a local server recorded by
# warcio; page.wacz is that WARC with its page list and package
# descriptor, as the WACZ 1.1.1 spec lays a WACZ out; page.mhtml is the
# same page as Chrome lays a "Save as single file" out: the page
# quoted-printable, the rest base64, a frame known only by its Content-ID.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
site=$(cd "$here/../html" && pwd)
# warcio appends to a WARC that is there, so each run starts from none.
rm -f "$here/page.warc.gz" "$here/page.wacz" "$here/page.mhtml"
venv=$(mktemp -d)
python3 -m venv "$venv"
"$venv/bin/pip" install --quiet 'warcio==1.7.5' 'requests==2.32.3'
cd "$site"
python3 -m http.server 8731 --bind 127.0.0.1 >/dev/null 2>&1 &
server=$!
trap 'kill $server' EXIT
sleep 1
"$venv/bin/python" - "$here" <<'PY'
import base64, email.generator, email.mime.base, email.mime.multipart, email.mime.text, hashlib, io, json, quopri, sys, zipfile
from warcio.capture_http import capture_http
import requests  # after capture_http, which patches it

here = sys.argv[1]
base = "http://127.0.0.1:8731/"
files = ["index.html", "style.css", "logo.svg", "app.js", "other.html"]
with capture_http(f"{here}/page.warc.gz"):
    for name in files:
        requests.get(base + name)

warc = open(f"{here}/page.warc.gz", "rb").read()
pages = [
    {"format": "json-pages-1.0", "id": "pages", "title": "All Pages"},
    {"id": "1", "url": base + "index.html", "title": "An HTML preview", "ts": "2026-10-26T00:00:00Z"},
    {"id": "2", "url": base + "other.html", "title": "Another page", "ts": "2026-10-26T00:00:01Z"},
]
jsonl = "".join(json.dumps(p) + "\n" for p in pages).encode()
def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()
resources = [
    {"name": "page.warc.gz", "path": "archive/page.warc.gz", "hash": digest(warc), "bytes": len(warc)},
    {"name": "pages.jsonl", "path": "pages/pages.jsonl", "hash": digest(jsonl), "bytes": len(jsonl)},
]
package = json.dumps({"profile": "data-package", "wacz_version": "1.1.1", "title": "An HTML preview",
                      "created": "2026-10-26T00:00:00Z", "software": "taste-ide fixture",
                      "resources": resources}, indent=2).encode()
with zipfile.ZipFile(f"{here}/page.wacz", "w") as z:
    z.writestr(zipfile.ZipInfo("archive/page.warc.gz", (2026, 10, 26, 0, 0, 0)), warc, zipfile.ZIP_STORED)
    z.writestr(zipfile.ZipInfo("pages/pages.jsonl", (2026, 10, 26, 0, 0, 0)), jsonl, zipfile.ZIP_DEFLATED)
    z.writestr(zipfile.ZipInfo("datapackage.json", (2026, 10, 26, 0, 0, 0)), package, zipfile.ZIP_DEFLATED)

# Chrome's layout: the page with a frame of other.html, known by cid.
page = open("index.html", encoding="utf-8").read().replace(
    "</ul>", '</ul>\n<iframe src="cid:frame-1@mhtml.blink" width="400" height="80"></iframe>')
boundary = "----MultipartBoundary--fixture0123456789----"
def part(headers, body):
    return "".join(f"{k}: {v}\r\n" for k, v in headers) + "\r\n" + body + "\r\n"
def b64(data):
    text = base64.b64encode(data).decode()
    return "\r\n".join(text[i:i + 76] for i in range(0, len(text), 76))
def qp(text):
    return quopri.encodestring(text.encode("utf-8")).decode().replace("\n", "\r\n")
parts = [
    part([("Content-Type", "text/html"), ("Content-ID", "<frame-0@mhtml.blink>"),
          ("Content-Transfer-Encoding", "quoted-printable"), ("Content-Location", base + "index.html")], qp(page)),
    part([("Content-Type", "text/html"), ("Content-ID", "<frame-1@mhtml.blink>"),
          ("Content-Transfer-Encoding", "quoted-printable")], qp(open("other.html", encoding="utf-8").read())),
]
for name, mime in [("style.css", "text/css"), ("logo.svg", "image/svg+xml"), ("app.js", "application/javascript")]:
    parts.append(part([("Content-Type", mime), ("Content-Transfer-Encoding", "base64"),
                       ("Content-Location", base + name)], b64(open(name, "rb").read())))
head = ("From: <Saved by Blink>\r\nSnapshot-Content-Location: " + base + "index.html\r\n"
        "Subject: An HTML preview\r\nDate: Mon, 26 Oct 2026 00:00:00 -0000\r\nMIME-Version: 1.0\r\n"
        f'Content-Type: multipart/related;\r\n\ttype="text/html";\r\n\tboundary="{boundary}"\r\n\r\n\r\n')
body = "".join(f"--{boundary}\r\n{p}" for p in parts) + f"--{boundary}--\r\n"
open(f"{here}/page.mhtml", "w", newline="").write(head + body)
PY
ls -la "$here"

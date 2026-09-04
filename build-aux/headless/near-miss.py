#!/usr/bin/env python3
"""Find edges that nearly line up in a probe geometry dump.

The one thing an eye is good at catching is two edges that almost match:
a card inset 12 beside a bar inset 10, a bubble two pixels inside the
composer's column. Those are invisible in the source — the 10 and the 12
live in different files, or one of them is a theme default nobody wrote
down — and a screenshot only shows them to somebody who already suspects.
This reads the geometry the probe already dumps (`TASTE_PROBE_CHECK=1`,
or the `ide_widget_geometry` MCP tool) and lists every pair of distinct
edges within a few pixels of each other, with the widgets that own them.

    near-miss.py /tmp/probe-run.log            # every `geometry <pane>:` in it
    near-miss.py /tmp/probe-run.log chat       # one pane
    near-miss.py geometry.json                 # one dump, saved from the tool

Which edges count: a widget that spans most of its pane's width (a bar, a
card, a row's content box) and is inset from its parent on that side —
the things that stand in a column. A GtkListBoxRow at x=0 is not one; the
card inside it is. Edges that differ by more than `--gap` are two
deliberate positions, and a difference of zero is what alignment looks
like; what is reported is the band in between. Exit status is 1 when any
near-miss is found, so the check can gate a change.

No third-party modules on purpose: it has to run on a bare host and in the
devcontainer alike, and the dump is plain JSON.
"""
import argparse
import json
import re
import sys

SPAN = 0.5  # a widget narrower than this fraction of the pane is not a bar
BORDER = 2  # a child this close to its parent's edge is the parent's border


def load(path):
    """Yield (target, tree) for every dump in a run log or a bare JSON file."""
    text = open(path, encoding="utf-8", errors="replace").read()
    if text.lstrip().startswith("{"):
        doc = json.loads(text)
        yield doc.get("target", path), doc["tree"]
        return
    for match in re.finditer(r"^geometry (\S+):\n(\{.*?\n\})\n", text, re.S | re.M):
        yield match.group(1), json.loads(match.group(2))["tree"]


def label(node):
    name = node.get("name")
    classes = [c for c in (node.get("css_classes") or []) if c not in ("horizontal", "vertical")]
    out = node.get("type", "?")
    if name:
        out += f"#{name}"
    if classes:
        out += "." + ".".join(classes)
    return out


def edges(tree):
    """Left and right edges of every column-spanning, inset, visible widget."""
    pane_w = tree["bounds"]["w"]
    pane_h = tree["bounds"]["h"]
    found = {"left": [], "right": []}

    def walk(node, parent, path):
        if not isinstance(node, dict):
            return
        b = node.get("bounds")
        here = path + [label(node)]
        if b and node.get("mapped", True) and node.get("visible", True):
            x, y, w, h = b["x"], b["y"], b["w"], b["h"]
            on_screen = y + h > 0 and y < pane_h and w > 0 and h > 0
            if on_screen and parent is not None and w >= SPAN * pane_w:
                px, pw = parent["bounds"]["x"], parent["bounds"]["w"]
                # A child a pixel or two inside its own parent is that
                # parent's border or focus ring, not an edge of its own: a
                # GtkFrame at 12 and its box at 13 are one line, and the
                # box inherits the frame's verdict.
                if x - px > BORDER:
                    found["left"].append((x, y, " > ".join(here[-3:])))
                if (px + pw) - (x + w) > BORDER:
                    found["right"].append((x + w, y, " > ".join(here[-3:])))
        children = node.get("children")
        if isinstance(children, list):
            for child in children:
                walk(child, node if b else parent, here)

    walk(tree, None, [])
    return found


def near_misses(points, gap):
    """Groups of distinct edge values whose neighbours are within `gap`."""
    by_value = {}
    for value, y, who in points:
        by_value.setdefault(value, []).append((y, who))
    values = sorted(by_value)
    groups, run = [], [values[0]] if values else []
    for prev, cur in zip(values, values[1:]):
        if 0 < cur - prev <= gap:
            run.append(cur)
        else:
            if len(run) > 1:
                groups.append(run)
            run = [cur]
    if len(run) > 1:
        groups.append(run)
    return [(g, by_value) for g in groups]


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("dump", help="probe run log, or one geometry JSON document")
    ap.add_argument("targets", nargs="*", help="pane names to check (default: all in the file)")
    ap.add_argument("--gap", type=float, default=5, help="largest difference still called a near-miss (px)")
    ap.add_argument("--all", action="store_true", help="also list every edge value, aligned or not")
    args = ap.parse_args()

    found_any = False
    for target, tree in load(args.dump):
        if args.targets and target not in args.targets:
            continue
        got = edges(tree)
        print(f"== {target}  ({int(tree['bounds']['w'])}x{int(tree['bounds']['h'])})")
        for side in ("left", "right"):
            if args.all:
                values = sorted({v for v, _, _ in got[side]})
                print(f"   {side} edges: {', '.join(str(int(v)) for v in values)}")
            for group, by_value in near_misses(got[side], args.gap):
                found_any = True
                print(f"   NEAR-MISS {side}: {' / '.join(str(int(v)) for v in group)}")
                for value in group:
                    owners = sorted(by_value[value])
                    for y, who in owners[:4]:
                        print(f"      {int(value):>5}  y={int(y):<5} {who}")
                    if len(owners) > 4:
                        print(f"             … and {len(owners) - 4} more at {int(value)}")
    sys.exit(1 if found_any else 0)


if __name__ == "__main__":
    main()

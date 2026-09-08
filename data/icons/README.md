# Icons

Everything here is the project's own work, drawn for this app, **except**
one file:

| File | Source | Licence |
| --- | --- | --- |
| `hicolor/scalable/status/taste-build-symbolic.svg` | [Bootstrap Icons](https://github.com/twbs/icons) `hammer` | MIT, Copyright (c) 2019-2024 The Bootstrap Authors |

The MIT notice travels in that file's own comment, which is where it has
to stay if the file is ever copied out of here. MIT is compatible with the
project's GPL-3.0; the notice is the whole of the obligation.

`taste-ide-symbolic.svg` is the application icon
(`hicolor/scalable/apps/net.davidstrauss.Taste.svg`) in one colour: the
same four paths, laid corner to corner and with hairline gaps cut between
the leaves, because a fourteen pixel row is not 128 and one colour is not
four. Its own comment says why each of those is there.

## One ink colour

Every `*-symbolic.svg` here draws its ink in `#241f31`. GTK recolours a
symbolic icon with the theme's foreground and ignores what the file says —
measured: two icons whose sources differed rendered the same neutral grey
— so the value is invisible in the app. It is not invisible anywhere the
SVG is rendered directly: a file manager's thumbnails, a browser, a docs
page. Three files had drifted to `#2e3436` and were brought back (David,
2026-09-08: "these icons are not the same color right now. Is that
intentional in the SVG itself?").

The `#fff` and `#000` inside a `<mask>` are not ink. They are luminance,
they are what makes the mask a mask, and they stay.

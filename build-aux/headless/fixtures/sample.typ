// A Typst file for the editor's highlighting (languages.rs).
#import "@preview/polylux:0.4.0": *
#set page(paper: "presentation-16-9", margin: 2cm)
#set text(font: "Inter", size: 20pt)

#let accent = rgb("#3584e4")
#show heading: it => text(fill: accent, it)

= Amutable at a glance
== Why now

We ship *systems* people _trust_, with `cargo` and care.
- Fast, at 1.5em and 50%
- Safe: see @intro and <intro>
+ Numbered, with $sum_(i=1)^n i = n(n+1)/2$

#if true [Shown *in markup* again] else { none }
/* A block comment, /* nested */ still a comment */
Visit https://typst.app for more \# escaped.

# Notebooks: the delta to the SaaS platforms

2026-10-11. What JupyterLab, Google Colab, VS Code's notebooks, and
Deepnote/Hex give a notebook that this IDE does not, after the robustness
and security pass (`46ad227`). Nothing here is built: each item is for
David to take or leave (2026-10-11: "Don't implement any parity items
without reviewing the delta with me first").

What the IDE already has, for the comparison: cells drawn natively; code
cells edited in place and run (gutter, Shift+Enter, Ctrl+Enter, Run All,
Interrupt, Restart); the kernel in the environment's container on its own
Jupyter; outputs written back into the file; text with ANSI colour,
Markdown, PNG, JPEG, retina, and SVG figures; the core ipywidgets drawn
natively, `interact` and Output among them; `update_display_data`; a
failed kernel handed to the agent; the minimap.

Cost is rough: S is a day or less, M a few days, L a week or more.

## A. Editing the notebook as a notebook

The largest gap: every platform edits the notebook's structure in place,
and here structure changes are made in the JSON.

| # | Item | Who has it | Cost | Recommendation |
| --- | --- | --- | --- | --- |
| A1 | Add a cell above or below, delete, move up and down, copy and paste cells | all | M | **Take.** The notebook is not usable as one without it. |
| A2 | Edit a Markdown cell in place (double-click, or Enter), rendered again when run or left | all | M | **Take.** |
| A3 | Change a cell's type (code, Markdown, raw); split and merge cells | all | S | **Take** with A1. |
| A4 | Command mode: Esc and Enter between editing and moving; A, B, DD, M, Y, Z, arrows between cells | Lab, Colab, VS Code | M | **Take, a subset.** Keyboard-first is a house value. |
| A5 | Collapse a cell's source or its outputs; collapse a heading's section | Lab, Colab, VS Code | S | Take. |
| A6 | A new notebook from the file tree, with this environment's kernel in its metadata | all | S | Take. |

## B. Running

| # | Item | Who has it | Cost | Recommendation |
| --- | --- | --- | --- | --- |
| B1 | Clear outputs (a cell, all); Restart and Clear; Restart and Run All | all | S | **Take.** |
| B2 | Run above, run below, run the selected cells | all | S | **Take.** |
| B3 | Queued and running told apart (`[*]` for both now); each cell's duration and when it last ran | VS Code, Colab, Deepnote | S | **Take.** |
| B4 | `input()` — the kernel's stdin requests, answered in a field under the cell | all | M | Take; `getpass` and interactive prompts need it. |
| B5 | Pick the kernel from those the container has; say which kernel and Python is running | all | S | Take. |
| B6 | An error's Prompt Agent on the cell ("explain this error and fix it"), as the failed-kernel banner does | Colab (Gemini), Lab AI | S | **Take.** Fits the orchestrator-level supervision David works from. |

## C. Output

| # | Item | Who has it | Cost | Recommendation |
| --- | --- | --- | --- | --- |
| C1 | DataFrames as real tables — today their text form shows | all | M | **Discuss.** No HTML renderer in this process; two routes that keep that: pandas' table-schema output (`application/vnd.dataresource+json`, opt-in on the kernel side) drawn natively, or a confined HTML-table parser in the helper. |
| C2 | LaTeX math in Markdown cells and outputs (`$x^2$`, `text/latex`) | all | M–L | **Discuss.** KaTeX and MathJax are JavaScript; a Rust math renderer in the confined helper (to SVG) is the route that fits. |
| C3 | JavaScript charts: Plotly, Bokeh, Altair/Vega | all (Lab with extensions) | S–L | **Discuss.** Cheapest: the container renders them to PNG or SVG (Plotly's kaleido, Altair's vl-convert), set up by the agent and said in its NOTEBOOKS instructions — the "IDE and container provided" shape. Native Vega rendering is a heavy helper. |
| C4 | `%matplotlib widget` (ipympl): pan, zoom, hover on figures | Lab, VS Code | M | Discuss. Its protocol rides the comms already relayed; a native canvas view. |
| C5 | Long outputs scroll in a box; copy an output; save or enlarge a figure | Lab, Colab, VS Code | S | Take. |
| C6 | Widgets with their own JavaScript (plotly FigureWidget, ipyleaflet, bqplot) | Lab, Colab | — | **Leave.** Their views are project-supplied JavaScript; drawing them is a web engine for project code. |
| C7 | Save widget state into the notebook's metadata, so the file shows its widgets elsewhere | Lab (opt-in) | S | Take; the IDE already reads it. |

## D. Code intelligence

| # | Item | Who has it | Cost | Recommendation |
| --- | --- | --- | --- | --- |
| D1 | Completion from the kernel (`complete_request`) | all | M | **Take.** Live objects complete as no static tool can. |
| D2 | Inspection: Shift+Tab docs, signature help (`inspect_request`) | all | S with D1 | Take with D1. |
| D3 | Diagnostics in cells (a language server over the notebook's cells) | VS Code | L | Leave for now; the IDE's LSP work is per file. |

## E. Navigating

| # | Item | Who has it | Cost | Recommendation |
| --- | --- | --- | --- | --- |
| E1 | Outline from the Markdown headings; jump to a section | Lab, Colab, VS Code | S | Take. |
| E2 | Find (and replace) across cells, in the IDE's one search | all | M | Take; the one query should answer in a notebook too. |
| E3 | A variable explorer and data viewer for the kernel's live objects | VS Code, Colab, Spyder | M–L | Discuss. Needs per-language introspection run silently in the kernel. |

## F. Files, review, and lifecycle

| # | Item | Who has it | Cost | Recommendation |
| --- | --- | --- | --- | --- |
| F1 | **A notebook diff**: the Changes face and the review surfaces showing cells and outputs, not JSON (nbdime) | VS Code, Lab (nbdime), Hex | M–L | **Take — the IDE's own priority.** Agents' notebook work is reviewed here, and a JSON diff of base64 is unreviewable. |
| F2 | Autosave | Lab, Colab, Deepnote | S | Discuss: the IDE has none for any file; a notebook's run writes the buffer, and losing a long run's outputs to a crash hurts. |
| F3 | Strip outputs on commit (nbstripout) as an option | VS Code (setting) | S | Discuss. |
| F4 | Export to HTML or PDF (nbconvert in the container) | all | S | Later; an agent can do it with `ide_exec` today. |

## G. Agents (beyond parity)

| # | Item | Cost | Recommendation |
| --- | --- | --- | --- |
| G1 | MCP tools for an agent to run cells in the user's live kernel, read their outputs and errors, and see what the notebook face shows | M | **Discuss.** The kernel is the environment's, so this is within the boundary; the question is whether an agent should share the user's kernel state or keep its own. |

## H. Out of scope, said so

Real-time collaboration and comments, scheduling and parameterised runs
(papermill), publishing and sharing links, Colab's forms (`#@param`), a
secrets manager, hardware selection beyond the environment's own sizing
and cloud hosts.

## The suggested first wave

A1–A4, B1–B3, B6, C5, and F1: the notebook edited as a notebook, run the
way the platforms run it, and reviewable when an agent changed it — the
items whose absence a user meets in the first five minutes, plus the one
the IDE's review-centred design makes its own.

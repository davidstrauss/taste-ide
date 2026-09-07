# Spike: workspace context for agents — semantic search, and what else

Design answer to David's request of 2026-09-06 to support VS Code's
"semantic search" workspace context, and to his question that followed:
"What else can I add to better support the AI's productivity?" The first
half shipped with this document (`taste-semantic`, `ide_semantic_search`);
the second is a ledger, ordered, with what each item would cost.

## What VS Code offers, and where this IDE already stands

VS Code's workspace-context reference lists what an agent can reach for:
text search and grep, file search by name or glob, "usages" (references,
implementations, definition), GitHub repository search, and semantic
search over a workspace index — `#codebase`, "code that matches the
meaning of your question, not just exact keywords", built once per
repository, kept current in the background, used by agents on their own
"when it makes sense", with text search as the fallback while an index is
not ready.

| VS Code | Here | Gap |
| --- | --- | --- |
| text search, grep | `ide_search`, `ide_find` (files, definitions, issues, branches, commits, terminals, chats) | none — ours reaches further |
| file search | `ide_list_files` | none |
| usages | `ide_references` | definitions and references, no "implementations" (no language server; see below) |
| semantic search | **`ide_semantic_search`, shipped with this spike** | the box in the title bar stays literal |
| GitHub repository search | — | deliberately none: the agent's world is this checkout |

## Semantic search, as built

**The model is local and pinned.** nomic-embed-text v1.5 at Q5_K_M — the
maintainers' own GGUF, 100 MB, 768 dimensions, Apache-2.0, trained on code
as well as prose, with documented `search_document:` / `search_query:`
prefixes — fetched once by `taste-models` (the speech model's fetcher,
moved out of `taste-voice` so there is one) with its digest checked, and
run through llama.cpp via `llama-cpp-2` — in a process of its own,
`taste-embed`, found beside the IDE binary. It had to be: `whisper-rs`
(voice) and `llama-cpp-sys-2` each bundle their own ggml, and the first
link of both into `taste-app` failed on the duplicate symbols. The helper
speaks one JSON object per line over stdio, loads the model once, and is
restarted once by its client if it dies — which also means a native
library's crash lands in a process that is not the IDE's. CPU only,
statically linked: a GPU build is a second toolchain to carry for a job
that runs in the background. A code-specific
model (Jina's `jina-embeddings-v2-base-code`) was considered and passed
over for now because no first-party GGUF of it exists, and a pin on a
third party's conversion is a supply-chain question the speech model
never had to answer.

**One store per workspace, a manifest per checkout.** `collect_files`
walks a checkout with `.gitignore` honoured; each text file under 256 KB
is hashed and, when the hash is new, cut into chunks on content
boundaries — a blank line or a definition (`symbols::definition`) starts
a chunk once the current one has eight lines, none runs past forty — and
each chunk's text is hashed. Vectors live once for the whole workspace,
content-addressed by that hash (`semantic/vectors.bin`, text kept beside
vector so a hit can show it); a checkout owns only a manifest of files
and chunk hashes. So an environment's clone, which is the primary plus a
branch's worth of change, is indexed for the cost of hashing it plus
embedding the chunks no checkout has seen — its own diff — and an edit
re-embeds the chunk it landed in, not the windows after it. Each
environment answers from its own manifest and never from the primary's
tree, which would be wrong exactly for the files its agent changed. A
chunk no manifest refers to is dropped when the store is written. Both
files are written atomically and loaded whole; a large repository is a
few thousand chunks, a few tens of megabytes, and a linear scan over unit
vectors answers a query in milliseconds. Measured on this repository,
190 files into 5,343 content-cut chunks: the first build takes about
twelve minutes on twelve threads (728 s; the 40-line windows before it
took 846 s for 3,257 chunks, being a third overlap), a question 76 ms,
and a build after a small edit re-embeds only the chunk it landed in —
under a second. An optimised
build of the helper made no difference (the cmake build of llama.cpp is
already optimised), and packing sixteen chunks into one forward pass was
slower (1,006 s), so the cost is the model's arithmetic; a smaller model
would be faster and worse, and David chose quality (2026-09-07: "Just go
with the higher quality model"). The build is background work with its
progress and time left shown beside the search box.

**Who keeps it.** The app (`taste-app/src/semantic.rs`) fetches the model
if the machine has never had it — announced, because 100 MB once is a
thing a person on a metered link should hear about — indexes the primary
checkout when the window opens, and re-indexes five seconds after the
last git change. An environment's clone is indexed when an agent first
asks; the tool answers "indexing" meanwhile. Every heavy step is on the
blocking pool; the GTK thread hears one toast when the index first exists
and a log line thereafter.

**Who asks.** `ide_semantic_search { query, limit }` on every socket. The
description and the initialize instructions say what it is for — the
question you cannot grep for — and when to prefer `ide_find`. The answer
is chunks with path, line range, text and score, plus the index's size, or
an honest "indexing" / "unavailable". Agents use it on their own, as VS
Code's do; nothing has to be typed.

**What was not done, and why.** The title-bar box is literal. A meaning
listing could join the text listings in the same panel shape, and the
design leaves that open, but a person at the box is looking for a word and
the box already has a rule (SEARCH.md: filter where the surface is a list,
list where it is a document) that a fuzzy ranking would sit oddly inside.
No cross-repository search: the agent's world is this checkout, by design.

## The ledger: what else would help an agent here

Ordered by how much it moves an agent's work per unit of the IDE's
complexity, against the bar in ARCHITECTURE.md (no extension points, one
way to do each thing, agents through ACP and MCP).

1. **Diagnostics as a tool** (`ide_diagnostics`). The one thing every
   agent asks for and none gets from this IDE: what the compiler says
   about a file right now. rust-analyzer's diagnostics are the honest
   source; the editor does not run a language server yet, and the agents
   run `cargo build` through `ide_exec` and parse the output themselves.
   A language-server client in the editor is a large change with its own
   spike; a cheaper first step is `cargo check --message-format=json`
   under `ide_exec`'s wing, cached per file hash, served as structured
   diagnostics with path, range, severity, message. Highest value.
2. **Tests as a tool** (`ide_test`). "Run the tests that cover this file"
   — the workspace's test command per convention (Rust: `cargo test -p
   <crate>`; the devcontainer's `test` task otherwise), run in the
   environment's container, parsed into pass/fail per test with the
   failing output attached. Agents already run tests through `ide_exec`;
   what they lack is the *structure*, which is what lets a coordinator's
   review say "17 passed, 1 failed" without reading a transcript.
3. **Symbols as a tool** (`ide_symbols`): the file's or the checkout's
   definitions (`symbols::index` already exists for the search), so an
   agent gets a file's outline in one call instead of reading it whole.
   Small, and it composes with semantic search: a hit's chunk plus the
   outline of its file is usually the whole context a question needs.
4. **A worker's brief that knows the code.** The issue is the first
   prompt; the coordinator's brief says to add constraints. The IDE
   could add, mechanically, the CLAUDE.md rules that name the files an
   issue mentions and the semantic hits for the issue's title — a
   generated "where to look" section. Cheap, uses what is here, and the
   kind of thing a human onboarding a colleague does.
5. **Implementations and call hierarchy** in `ide_references`. Needs a
   language server; comes with item 1's second step.
6. **Instructions files the agents already read.** Claude Code reads
   CLAUDE.md, Copilot `.github/copilot-instructions.md`, Gemini
   GEMINI.md. One file is the truth here (CLAUDE.md); the IDE could
   symlink or generate the others so every agent reads the same rules.
   Zero mechanism, one convention — and worth doing.
7. **Screenshots with geometry are already a tool**
   (`ide_screenshot`, `ide_widget_geometry`); the near-miss check
   (`near-miss.py`) is not. `ide_layout_check` — the probe's fit and
   near-miss verdicts as a tool — would let a UI-working agent judge its
   own frame the way this project's own agent does.

Not on the list, deliberately: a browser tool, web search, or any tool
that reaches outside the checkout and the IDE — those are the agent's own
(Claude Code has them) and the IDE's job is the workspace.

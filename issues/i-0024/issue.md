---
title: The Destroy dialog says a chat "keeps its conversation", and it does not
state: open
reporter: i-0022
created: 2026-09-13T05:53:52Z
updated: 2026-09-13T05:53:52Z
labels: ui, chat, environments, copy
---

`destroy_intervention` (`crates/taste-app/src/console.rs`, in the block that assembles the summary) tells the user:

> "{chat}" works here; it keeps its conversation but loses the files it was working on.

That was true when a chat could outlive its environment. It is not true now. `Event::EnvironmentRemoved` reaches `Chats::forget_environment` (`chats.rs:766`), which removes the chat from the strip, calls `pane.close()`, and persists the list without it — "An environment was destroyed: its conversation goes with it. There is nowhere else for a chat to live", as its own doc comment says. ARCHITECTURE.md agrees: "Destroying an environment destroys its chat with it."

So the one sentence in the dialog that is about what the user *keeps* is the one sentence that is wrong, in the dialog whose whole job is to say what is lost before the button is offered.

Found while working i-0022, which put the same enumeration on the MCP surface; the tool's wording says the chat goes, so the two surfaces now disagree about the same act.

**Done looks like:** the dialog says the conversation goes with the environment, in the same breath as the clone, the container, and the volumes. Worth checking at the same time whether the transcript is recoverable from anywhere at all after the removal (the ACP session id is persisted per chat entry, and that entry is what `persist()` drops) — if it is, the honest sentence is different again, and that is the sentence to write.

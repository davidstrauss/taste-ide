//! **The transcript as data.**
//!
//! A conversation's record (`ChatPane::record`) is turned here into the rows
//! the transcript shows — a message the user sent, an answer, a thought, a
//! step, a background task's report, the plan, a note — as plain data, and
//! nothing here touches a widget. The transcript is a list of these rows
//! (`super::rows`), whose widgets GTK builds when a row comes on screen and
//! drops when it leaves: so the whole of a long conversation can be held,
//! a restored one is turned into rows off the main thread, and nothing has
//! to stand in for rows not yet drawn (David, 2026-10-19: "Is there no
//! higher-level way to have this feed dynamically load and unload
//! elements?").
//!
//! [`Model::apply`] is the one way an update becomes rows, live and
//! restored alike: a conversation lived and the same conversation read back
//! from its record come to the same rows, which is tested rather than
//! hoped. It says what it changed ([`Change`]) for the view to follow, and
//! what the transcript's text mirror should say ([`Model::take_lines`]).
//!
//! The timeline's line is part of a row's data ([`Rail`]): a step's line
//! runs up to the step above it, or the prompt's box, and down to the step
//! below, and a prompt ends it. Decided as rows are added, here, rather
//! than by widgets reaching for their neighbours.

use std::collections::HashMap;
use std::time::Instant;

use agent_client_protocol::schema::v1::{
    ContentBlock, Plan, PlanEntryStatus, SessionUpdate, ToolCallContent, ToolCallStatus, ToolKind,
};

use super::{
    act_kind, act_output_json, briefing_notes, content_text, edit_title, ide_edit_shape,
    is_ide_edit_title, mcp_tool_name, replayed_attachment, step_title, task_notifications,
    thought_header, ActKind, PendingUserMessage, BRIEFING_TAIL, INTERRUPTED_THOUGHT,
};

pub(super) type RowId = u64;

/// A prompt the IDE sent, by the `messageId` its records carry: ids of
/// the IDE's own begin with [`PROMPT_PREFIX`], so an agent's own message
/// ids, on a replay, are never taken for one.
pub(super) type PromptId = String;

pub(super) const PROMPT_PREFIX: &str = "taste-prompt-";

/// Where a prompt the IDE sent has got to: data on its row, which its card
/// is drawn from — the badge, the fast-forward and stop, and the refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Delivery {
    /// In the agent's history, as everything a replay brings back is.
    Delivered,
    /// Gone to the agent, which has not started on it yet.
    Sent,
    /// Waiting behind the turn under way: fast-forward and stop are offered.
    Queued,
    /// Fast-forward clicked (a turn asked for); `told` once the agent has
    /// been told.
    Asking { told: bool },
    /// Held for the agent or its environment to come up: what it is
    /// waiting for, in words.
    Held(String),
    /// Taken after waiting behind a turn: the note it went out after.
    Waited(String),
    /// The agent refused it: what it said.
    Refused(String),
    /// Never sent: what went wrong.
    NotSent(String),
    /// The agent stopped before it took it.
    Lost,
    /// Read by the agent within the turn it waited behind, at its next step
    /// (`taste_core::nudge::TAKE_TOOL`), rather than sent after it.
    WithinTurn,
    /// Read back from the record never taken: the IDE closed first.
    Undelivered,
}

impl Delivery {
    /// Still on its way: neither taken nor given up on.
    pub fn in_flight(&self) -> bool {
        matches!(
            self,
            Delivery::Sent | Delivery::Queued | Delivery::Asking { .. } | Delivery::Held(_)
        )
    }
}

/// A row's piece of the timeline's line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Rail {
    /// The line runs up from this row's dot: to the step above, or across
    /// the gap from the prompt's box.
    pub top: bool,
    /// The line runs down to the next step.
    pub bottom: bool,
    /// The row stands under a prompt, and carries the gap below its box.
    pub after_prompt: bool,
}

/// What a dot on the rail says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Dot {
    /// A step of the answer: neutral.
    Plain,
    /// An aside — a note, a thought, the plan: hollow.
    Note,
    /// A background task's end, coloured by how it ended.
    Tone(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct User {
    pub text: String,
    pub attachments: Vec<(String, ContentBlock)>,
    /// The IDE's id for it, for a prompt it sent and is following.
    pub prompt: Option<PromptId>,
    pub delivery: Delivery,
    /// Sent while a turn was under way, and waiting behind it: the card
    /// stands inside that turn's timeline, the line running past it, since
    /// the turn goes on below it (David, 2026-10-19: "Discontinuity for the
    /// line"). Seated as a prompt once the agent takes it
    /// ([`Model::prompt_taken`]).
    pub within_turn: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Answer {
    pub text: String,
    /// The stream has ended: drawn as rendered prose rather than text as it
    /// arrives.
    pub done: bool,
    /// Shown whole rather than clipped: the answer the turn's suggested
    /// replies choose from ([`Model::unclip_turn`]).
    pub whole: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Thought {
    pub text: String,
    /// "Thought for 4s · …" once it is over; "Thinking…" until then.
    pub header: Option<String>,
}

/// One tool call, as the agent has reported it so far.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Step {
    pub call: String,
    /// What the agent called the call, whole.
    pub title: String,
    pub status: Option<ToolCallStatus>,
    pub kind: ToolKind,
    /// The call's content, as last reported: a snapshot of the whole.
    pub content: Option<Vec<ToolCallContent>>,
    /// The call's arguments, kept for every call: `ide_exec`'s IN is built
    /// from them.
    pub input: Option<serde_json::Value>,
    pub output: Option<serde_json::Value>,
    /// What the adapter's own shell call printed, when it has said.
    pub terminal: Option<String>,
    /// The act this call is, if it is one, and the answer it got.
    pub act: Option<ActKind>,
    pub act_output: Option<serde_json::Value>,
    /// Still going, from the last status that said.
    pub running: bool,
    /// The dot's tone: `ok`, `fail`, `live`, or `cut` for a call its turn
    /// ended under without saying how it went.
    pub tone: &'static str,
    /// A rebuild the agent asked for, whose call went with its container:
    /// finished, not failed.
    pub rebuild_took_the_agent: bool,
    /// A permission question about it is open.
    pub waiting: bool,
    /// How its permission was answered: an icon and what it says.
    pub permission: Option<(String, String)>,
    /// Opened onto its content.
    pub expanded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Task {
    pub title: String,
    pub report: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PlanCard {
    pub entries: Vec<(String, PlanEntryStatus)>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Kind {
    User(User),
    Answer(Answer),
    Thought(Thought),
    Step(Box<Step>),
    Task(Task),
    Plan(PlanCard),
    Note(String),
    /// Where a prompt was typed, which the agent took further down: the
    /// line runs through it when there are steps either side.
    Marker {
        prompt: PromptId,
        through: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Row {
    pub id: RowId,
    pub kind: Kind,
    /// Its piece of the timeline, for a row that stands on it.
    pub rail: Option<Rail>,
    pub dot: Dot,
    /// Its document is the editor's tab in front.
    pub lit: bool,
    /// When it was added, unix seconds: what a search hit says instead of
    /// a row number.
    pub at: u64,
    /// The prompt a note is about — its briefing — which it moves with.
    pub of_prompt: Option<PromptId>,
}

impl Row {
    /// What the row says, as text: for search, and the transcript's mirror.
    pub fn text(&self) -> String {
        match &self.kind {
            Kind::User(user) => user.text.clone(),
            Kind::Answer(answer) => answer.text.clone(),
            Kind::Thought(thought) => thought.text.clone(),
            Kind::Step(step) => step_title(&step.title, step.input.as_ref(), step.output.as_ref())
                .unwrap_or_else(|| step.title.clone()),
            Kind::Task(task) => task.title.clone(),
            Kind::Plan(plan) => plan
                .entries
                .iter()
                .map(|(entry, _)| entry.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            Kind::Note(text) => text.clone(),
            Kind::Marker { .. } => String::new(),
        }
    }
}

/// What a change to the model did, for the view to follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Change {
    Added(RowId),
    Changed(RowId),
    Removed(RowId),
}

/// The transcript's rows, and the state of the drawing of the turn under
/// way: the answer streaming, the steps that may still be updated, the
/// rail's last dot, the message being assembled.
#[derive(Debug, Default)]
pub(super) struct Model {
    rows: Vec<Row>,
    index: HashMap<RowId, usize>,
    next: RowId,
    changes: Vec<Change>,
    lines: Vec<(&'static str, String)>,
    answer: Option<RowId>,
    thought: Option<(RowId, Option<Instant>)>,
    steps: HashMap<String, RowId>,
    ide_edits: HashMap<String, String>,
    terminals: HashMap<String, String>,
    last_step: Option<RowId>,
    after_prompt: bool,
    pending_user: Option<PendingUserMessage>,
    plan: Option<RowId>,
    plan_snapshot: Option<Vec<(String, PlanEntryStatus)>>,
    last_prompt: Option<RowId>,
    /// Permission answers for calls with no row yet.
    marks: HashMap<String, (String, String)>,
    /// The answers since the turn's last step of work: what its suggested
    /// replies choose between, shown whole when they are offered.
    turn_answers: Vec<RowId>,
    /// The prompts the IDE sent, by id, and the notes each was briefed
    /// with: a prompt's records said twice — once more after a session's
    /// mark — make it once.
    prompts: HashMap<PromptId, RowId>,
    prompt_notes: HashMap<PromptId, Vec<String>>,
}

impl Model {
    /// An empty model whose first row is `first`: ids go on across a
    /// clear, so a key an editor tab holds never names another row.
    pub fn starting_at(first: RowId) -> Model {
        Model {
            next: first,
            ..Model::default()
        }
    }

    /// The id the next row will have.
    pub fn next_id(&self) -> RowId {
        self.next
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn row(&self, id: RowId) -> Option<&Row> {
        self.index.get(&id).map(|at| &self.rows[*at])
    }

    fn row_mut(&mut self, id: RowId) -> Option<&mut Row> {
        let at = *self.index.get(&id)?;
        Some(&mut self.rows[at])
    }

    /// What changed since the last time this was asked.
    pub fn take_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.changes)
    }

    /// The transcript mirror's lines since the last time this was asked:
    /// who, and what, one entry each.
    pub fn take_lines(&mut self) -> Vec<(&'static str, String)> {
        std::mem::take(&mut self.lines)
    }

    /// The row of the newest prompt drawn from the record.
    pub fn last_prompt(&self) -> Option<RowId> {
        self.last_prompt
    }

    /// The row a call's step is, if it has one.
    pub fn step_row(&self, call: &str) -> Option<RowId> {
        self.steps.get(call).copied()
    }

    /// Whether any step is still running.
    pub fn any_running(&self) -> bool {
        self.steps
            .values()
            .filter_map(|id| self.row(*id))
            .any(|row| matches!(&row.kind, Kind::Step(step) if step.running))
    }

    fn changed(&mut self, id: RowId) {
        if !self.changes.contains(&Change::Changed(id))
            && !self.changes.contains(&Change::Added(id))
        {
            self.changes.push(Change::Changed(id));
        }
    }

    fn push(&mut self, kind: Kind, rail: Option<Rail>, dot: Dot) -> RowId {
        let id = self.next;
        self.next += 1;
        self.index.insert(id, self.rows.len());
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        self.rows.push(Row {
            id,
            kind,
            rail,
            dot,
            lit: false,
            at,
            of_prompt: None,
        });
        self.changes.push(Change::Added(id));
        id
    }

    /// Take row `id` out.
    fn remove(&mut self, id: RowId) {
        let Some(at) = self.index.remove(&id) else {
            return;
        };
        self.rows.remove(at);
        for (index, row) in self.rows.iter().enumerate().skip(at) {
            self.index.insert(row.id, index);
        }
        let unseen = self.changes.contains(&Change::Added(id));
        self.changes
            .retain(|change| !matches!(change, Change::Added(x) | Change::Changed(x) if *x == id));
        if !unseen {
            self.changes.push(Change::Removed(id));
        }
        if self.last_step == Some(id) {
            self.last_step = None;
        }
    }

    /// Answer `id` taken back out when it is the agent's own report of its
    /// request to the API cut off — "API Error: Connection lost
    /// mid-response…" — and what cut it was a rebuild: the environment mid-
    /// rebuild as it arrived, or the step before it the rebuild call, which
    /// is how a restored conversation reads the same moment. The agent's
    /// container went with the rebuild, so its connection did too, and the
    /// IDE's note says that (David, 2026-10-22: "This error shouldn't show
    /// if it's expected from a container rebuild"). The timeline is
    /// rejoined as it was before the answer came.
    fn withdraw_lost_connection(&mut self, id: RowId, in_transition: bool) {
        let Some(&at) = self.index.get(&id) else {
            return;
        };
        let row = &self.rows[at];
        let Kind::Answer(answer) = &row.kind else {
            return;
        };
        let text = answer.text.trim_start();
        if !(text.starts_with("API Error:") && text.to_ascii_lowercase().contains("connection")) {
            return;
        }
        let rail = row.rail.unwrap_or_default();
        let before = self.rows[..at].iter().rev().find(|row| row.rail.is_some());
        let after_rebuild = matches!(
            before.map(|row| &row.kind),
            Some(Kind::Step(step))
                if mcp_tool_name(&step.title).as_deref() == Some("devcontainer_reload")
        );
        if !(in_transition || after_rebuild) {
            return;
        }
        let previous = before
            .map(|row| row.id)
            .filter(|_| rail.top && !rail.after_prompt);
        self.remove(id);
        self.answer = None;
        self.after_prompt = rail.after_prompt;
        if let Some(previous) = previous {
            if let Some(rail) = self.row_mut(previous).and_then(|row| row.rail.as_mut()) {
                rail.bottom = false;
            }
            self.changed(previous);
            self.last_step = Some(previous);
        }
    }

    /// A row on the timeline: its line runs up from the step before, or
    /// across the gap from a prompt above, and the step before's runs down
    /// to it.
    fn push_step(&mut self, kind: Kind, dot: Dot) -> RowId {
        let after_prompt = std::mem::take(&mut self.after_prompt);
        let previous = self.last_step;
        let rail = Rail {
            top: previous.is_some() || after_prompt,
            bottom: false,
            after_prompt,
        };
        let id = self.push(kind, Some(rail), dot);
        if let Some(previous) = previous {
            if let Some(row) = self.row_mut(previous) {
                if let Some(rail) = row.rail.as_mut() {
                    rail.bottom = true;
                }
            }
            self.changed(previous);
        }
        self.last_step = Some(id);
        id
    }

    /// The turn's timeline ends here: the last step's line stops at its
    /// dot, and what follows starts under a prompt's box.
    pub fn end_steps(&mut self) {
        if let Some(last) = self.last_step.take() {
            if let Some(row) = self.row_mut(last) {
                if let Some(rail) = row.rail.as_mut() {
                    rail.bottom = false;
                }
            }
            self.changed(last);
            // A prompt still waiting inside the turn — refused, lost, never
            // taken — is a card of its own once the turn is over: the line
            // that ran past it ends at the dot above (review, 2026-10-20).
            if let Some(&at) = self.index.get(&last) {
                let waiting: Vec<RowId> = self.rows[at + 1..]
                    .iter()
                    .filter(|row| {
                        matches!(
                            &row.kind,
                            Kind::User(User {
                                within_turn: true,
                                ..
                            })
                        )
                    })
                    .map(|row| row.id)
                    .collect();
                for id in waiting {
                    if let Some(Row {
                        kind: Kind::User(user),
                        ..
                    }) = self.row_mut(id)
                    {
                        user.within_turn = false;
                    }
                    self.changed(id);
                }
            }
        }
        self.after_prompt = true;
    }

    /// The line runs on down from step `id` again: a prompt that ended it
    /// there has moved on, and a step follows.
    pub fn rejoin(&mut self, id: RowId) {
        if let Some(rail) = self.row_mut(id).and_then(|row| row.rail.as_mut()) {
            rail.bottom = true;
        }
        self.changed(id);
    }

    /// The session is over: what it left streaming is closed, what it left
    /// running is settled, and the next session's calls, plan, and marks
    /// start fresh under them.
    pub fn end_session(&mut self) {
        self.finalize();
        self.settle_running();
        self.steps.clear();
        self.ide_edits.clear();
        self.terminals.clear();
        self.marks.clear();
        self.plan = None;
        self.plan_snapshot = None;
    }

    /// The answer being streamed, as far as it has come.
    pub fn writing(&self) -> Option<&str> {
        match &self.row(self.answer?)?.kind {
            Kind::Answer(answer) => Some(&answer.text),
            _ => None,
        }
    }

    /// The thought being streamed, as far as it has come.
    pub fn thinking(&self) -> Option<&str> {
        let (id, _) = self.thought?;
        match &self.row(id)?.kind {
            Kind::Thought(thought) => Some(&thought.text),
            _ => None,
        }
    }

    /// One update from the agent, or the user's message as recorded.
    /// `in_transition`: the environment is mid-rebuild, which is how a
    /// rebuild's call coming back failed is read as one that worked.
    pub fn apply(&mut self, update: SessionUpdate, in_transition: bool) {
        // Anything that is not another piece of the user's message ends it.
        if !matches!(update, SessionUpdate::UserMessageChunk(_)) {
            self.flush_user();
        }
        match update {
            SessionUpdate::UserMessageChunk(chunk) => {
                // A prompt the IDE sent says so in its `messageId`, and a
                // change of id is a new message.
                let prompt = chunk
                    .message_id
                    .as_ref()
                    .map(|id| id.0.to_string())
                    .filter(|id| id.starts_with(PROMPT_PREFIX));
                if prompt.is_some()
                    && self
                        .pending_user
                        .as_ref()
                        .is_some_and(|pending| pending.prompt != prompt)
                {
                    self.flush_user();
                }
                let pending = self
                    .pending_user
                    .get_or_insert_with(PendingUserMessage::default);
                if prompt.is_some() {
                    pending.prompt = prompt;
                }
                match content_text(&chunk.content) {
                    Some(text) if text.trim_end().ends_with(BRIEFING_TAIL) => {
                        pending.briefed.extend(briefing_notes(&text));
                    }
                    Some(text) => {
                        if !pending.text.is_empty() && !pending.text.ends_with('\n') {
                            pending.text.push('\n');
                        }
                        pending.text.push_str(&text);
                    }
                    None => {
                        if let Some(attachment) = replayed_attachment(&chunk.content) {
                            pending.attachments.push(attachment);
                        }
                    }
                }
            }
            SessionUpdate::AgentMessageChunk(chunk) => {
                if let Some(text) = content_text(&chunk.content) {
                    let id = match self.answer {
                        Some(id) => id,
                        None => {
                            let id = self.push_step(
                                Kind::Answer(Answer {
                                    text: String::new(),
                                    done: false,
                                    whole: false,
                                }),
                                Dot::Plain,
                            );
                            self.answer = Some(id);
                            id
                        }
                    };
                    if let Some(Row {
                        kind: Kind::Answer(answer),
                        ..
                    }) = self.row_mut(id)
                    {
                        answer.text.push_str(&text);
                    }
                    self.changed(id);
                    self.withdraw_lost_connection(id, in_transition);
                }
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                if let Some(text) = content_text(&chunk.content) {
                    let id = match self.thought {
                        Some((id, _)) => id,
                        None => {
                            let id = self.push_step(
                                Kind::Thought(Thought {
                                    text: String::new(),
                                    header: None,
                                }),
                                Dot::Note,
                            );
                            self.thought = Some((id, Some(Instant::now())));
                            id
                        }
                    };
                    let mut interrupted = false;
                    if let Some(Row {
                        kind: Kind::Thought(thought),
                        ..
                    }) = self.row_mut(id)
                    {
                        thought.text.push_str(&text);
                        // The API's own stand-in for work a turn ran out of
                        // room to finish: not a thought, a fact about the
                        // turn, and one the reader can act on.
                        let last = thought.text.lines().last().unwrap_or_default().trim();
                        if last == INTERRUPTED_THOUGHT {
                            let keep = thought.text.len()
                                - thought.text.lines().last().map_or(0, str::len);
                            thought.text.truncate(keep);
                            interrupted = true;
                        }
                    }
                    self.changed(id);
                    if interrupted {
                        self.note(
                            "the turn was cut off before it finished — send again to continue",
                        );
                    }
                }
            }
            SessionUpdate::ToolCall(mut call) => {
                let id = call.tool_call_id.to_string();
                // A step of work: what was said before it is not the answer
                // the replies, if any, will be about. Offering them is not
                // work.
                if mcp_tool_name(&call.title).as_deref() != Some("suggest_replies") {
                    self.turn_answers.clear();
                }
                // An edit through the IDE's tools reads as the edit it is.
                if is_ide_edit_title(&call.title) {
                    self.ide_edits.insert(id.clone(), call.title.clone());
                    call.kind = ToolKind::Edit;
                    if let Some((content, path)) =
                        ide_edit_shape(&call.title, call.raw_input.as_ref())
                    {
                        call.content = content;
                        call.title = edit_title(&call.title, &path);
                    }
                }
                // Narration after this call starts a new block below it.
                self.finalize();
                self.note_terminal(&id, call.meta.as_ref());
                self.upsert_step(
                    id,
                    Some(call.title.clone()),
                    Some(call.status),
                    Some(call.kind),
                    Some(call.content),
                    call.raw_input,
                    call.raw_output,
                    in_transition,
                );
            }
            SessionUpdate::ToolCallUpdate(mut update) => {
                let id = update.tool_call_id.to_string();
                if let Some(title) = update
                    .fields
                    .title
                    .as_deref()
                    .filter(|t| is_ide_edit_title(t))
                {
                    self.ide_edits.insert(id.clone(), title.to_string());
                }
                if let Some(title) = self.ide_edits.get(&id).cloned() {
                    update.fields.kind = Some(ToolKind::Edit);
                    match ide_edit_shape(&title, update.fields.raw_input.as_ref()) {
                        Some((content, path)) => {
                            update.fields.content = Some(content);
                            update.fields.title = Some(edit_title(&title, &path));
                        }
                        // The tool's own answer is a line of JSON: the diff
                        // it was drawn with stays, unless the edit failed.
                        None if update.fields.status != Some(ToolCallStatus::Failed) => {
                            update.fields.content = None;
                            update.fields.title = None;
                        }
                        None => {}
                    }
                }
                self.note_terminal(&id, update.meta.as_ref());
                self.upsert_step(
                    id,
                    update.fields.title,
                    update.fields.status,
                    update.fields.kind,
                    update.fields.content,
                    update.fields.raw_input,
                    update.fields.raw_output,
                    in_transition,
                );
            }
            SessionUpdate::Plan(plan) => {
                self.finalize();
                self.plan(&plan);
            }
            _ => {}
        }
    }

    /// Keep what a shell call printed, when the update reports it.
    fn note_terminal(
        &mut self,
        id: &str,
        meta: Option<&serde_json::Map<String, serde_json::Value>>,
    ) {
        let Some(data) = meta
            .and_then(|meta| meta.get("terminal_output"))
            .and_then(|output| output.get("data"))
            .and_then(serde_json::Value::as_str)
        else {
            return;
        };
        self.terminals
            .entry(id.to_string())
            .or_default()
            .push_str(data);
    }

    #[allow(clippy::too_many_arguments)]
    fn upsert_step(
        &mut self,
        call: String,
        title: Option<String>,
        status: Option<ToolCallStatus>,
        kind: Option<ToolKind>,
        content: Option<Vec<ToolCallContent>>,
        input: Option<serde_json::Value>,
        output: Option<serde_json::Value>,
        in_transition: bool,
    ) {
        self.finalize();
        let id = match self.steps.get(&call) {
            Some(id) => *id,
            None => {
                if let Some(title) = &title {
                    let line = step_title(title, input.as_ref(), output.as_ref())
                        .unwrap_or_else(|| title.clone());
                    self.lines.push(("tool", line));
                }
                let id = self.push_step(
                    Kind::Step(Box::new(Step {
                        call: call.clone(),
                        title: String::new(),
                        status: None,
                        kind: ToolKind::Other,
                        content: None,
                        input: None,
                        output: None,
                        terminal: None,
                        act: None,
                        act_output: None,
                        running: false,
                        tone: "live",
                        rebuild_took_the_agent: false,
                        waiting: false,
                        permission: None,
                        expanded: false,
                    })),
                    Dot::Plain,
                );
                self.steps.insert(call.clone(), id);
                id
            }
        };
        let terminal = self.terminals.get(&call).cloned();
        let mark = self.marks.remove(&call);
        if let Some(Row {
            kind: Kind::Step(step),
            ..
        }) = self.row_mut(id)
        {
            if let Some(title) = title {
                step.title = title;
                if step.act.is_none() {
                    step.act = act_kind(&step.title);
                }
            }
            if let Some(kind) = kind {
                step.kind = kind;
            }
            if let Some(input) = input {
                step.input = Some(input);
            }
            if let Some(output) = output {
                if step.act.is_some() {
                    if let Some(parsed) = act_output_json(&output) {
                        step.act_output = Some(parsed);
                    }
                }
                step.output = Some(output);
            }
            if let Some(content) = content {
                // An MCP result reaches the card as content text before —
                // or instead of — a raw output; an act reads its answer
                // from there too.
                if step.act.is_some() && step.act_output.is_none() {
                    step.act_output = content.iter().find_map(|item| match item {
                        ToolCallContent::Content(block) => content_text(&block.content)
                            .and_then(|text| act_output_json(&serde_json::Value::String(text))),
                        _ => None,
                    });
                }
                step.content = Some(content);
            }
            if terminal.is_some() {
                step.terminal = terminal;
            }
            if let Some(status) = status {
                step.running =
                    matches!(status, ToolCallStatus::Pending | ToolCallStatus::InProgress);
                step.status = Some(status);
                // A rebuild the agent asked for stops the container the
                // agent lives in, and its call goes with it: "Connection
                // closed" is that rebuild under way, not a failure of it.
                step.rebuild_took_the_agent = status == ToolCallStatus::Failed
                    && mcp_tool_name(&step.title).as_deref() == Some("devcontainer_reload")
                    && (in_transition
                        || step.output.as_ref().is_some_and(|out| {
                            out.to_string()
                                .to_ascii_lowercase()
                                .contains("connection closed")
                        }));
                step.tone = match status {
                    ToolCallStatus::Completed => "ok",
                    ToolCallStatus::Failed if step.rebuild_took_the_agent => "ok",
                    ToolCallStatus::Failed => "fail",
                    _ => "live",
                };
            }
            if let Some(mark) = mark {
                step.permission = Some(mark);
            }
        }
        self.changed(id);
    }

    /// The agent's plan — when it says something new. A plan update
    /// restates the whole checklist; the same checklist said again earns no
    /// row, and a turn's plan is rewritten in place.
    fn plan(&mut self, plan: &Plan) {
        if plan.entries.is_empty() {
            return;
        }
        let snapshot: Vec<(String, PlanEntryStatus)> = plan
            .entries
            .iter()
            .map(|entry| (entry.content.clone(), entry.status.clone()))
            .collect();
        if self.plan_snapshot.as_ref() == Some(&snapshot) {
            return;
        }
        self.plan_snapshot = Some(snapshot.clone());
        let card = PlanCard { entries: snapshot };
        match self.plan {
            Some(id) => {
                if let Some(row) = self.row_mut(id) {
                    row.kind = Kind::Plan(card);
                }
                self.changed(id);
            }
            None => {
                let id = self.push_step(Kind::Plan(card), Dot::Note);
                self.plan = Some(id);
            }
        }
    }

    /// A note from the IDE between the rows.
    pub fn note(&mut self, text: &str) -> RowId {
        self.finalize();
        self.lines.push(("note", text.to_string()));
        self.push_step(Kind::Note(text.to_string()), Dot::Note)
    }

    /// A note about prompt `prompt` — what it was briefed with — which goes
    /// where it goes. Said again for the same prompt, it is not drawn
    /// again.
    pub fn prompt_note(&mut self, prompt: &str, text: &str) -> Option<RowId> {
        let said = self.prompt_notes.entry(prompt.to_string()).or_default();
        if said.iter().any(|note| note == text) {
            return None;
        }
        said.push(text.to_string());
        let id = self.note(text);
        if let Some(row) = self.row_mut(id) {
            row.of_prompt = Some(prompt.to_string());
        }
        Some(id)
    }

    /// A prompt the IDE is sending, as its card, from the moment it is
    /// sent: it ends the turn before it, as any prompt does.
    pub fn prompt_sent(
        &mut self,
        prompt: &str,
        text: String,
        attachments: Vec<(String, ContentBlock)>,
        delivery: Delivery,
    ) -> RowId {
        if let Some(id) = self.prompts.get(prompt) {
            return *id;
        }
        self.close_stream();
        self.user_row(text, attachments, Some(prompt.to_string()), delivery)
    }

    /// The row prompt `prompt` is.
    pub fn prompt_row(&self, prompt: &str) -> Option<RowId> {
        self.prompts.get(prompt).copied()
    }

    /// The prompt row `id` is about: a prompt's own card, or its marker.
    pub fn prompt_of(&self, id: RowId) -> Option<&str> {
        match &self.row(id)?.kind {
            Kind::User(user) => user.prompt.as_deref(),
            Kind::Marker { prompt, .. } => Some(prompt),
            _ => None,
        }
    }

    /// Where prompt `prompt` has got to.
    pub fn delivery(&self, prompt: &str) -> Option<&Delivery> {
        match &self.row(*self.prompts.get(prompt)?)?.kind {
            Kind::User(user) => Some(&user.delivery),
            _ => None,
        }
    }

    /// The prompts still on their way.
    pub fn in_flight(&self) -> Vec<PromptId> {
        self.prompts
            .iter()
            .filter(|(prompt, _)| self.delivery(prompt).is_some_and(Delivery::in_flight))
            .map(|(prompt, _)| prompt.clone())
            .collect()
    }

    /// Prompt `prompt` has got to `delivery`.
    pub fn set_delivery(&mut self, prompt: &str, delivery: Delivery) {
        let Some(id) = self.prompts.get(prompt).copied() else {
            return;
        };
        if let Some(Row {
            kind: Kind::User(user),
            ..
        }) = self.row_mut(id)
        {
            if user.delivery == delivery {
                return;
            }
            user.delivery = delivery;
        }
        self.changed(id);
    }

    /// A prompt that waited inside a turn, taken with nothing of that turn
    /// drawn after it: it is a prompt where it stands. The turn's line ends
    /// at the dot above it, and the next step's runs up from its box.
    fn seat_in_place(&mut self, id: RowId, at: usize) {
        let within = matches!(
            self.row(id).map(|row| &row.kind),
            Some(Kind::User(User {
                within_turn: true,
                ..
            }))
        );
        if !within {
            return;
        }
        if let Some(Row {
            kind: Kind::User(user),
            ..
        }) = self.row_mut(id)
        {
            user.within_turn = false;
        }
        self.changed(id);
        let above = self.rows[..at]
            .iter()
            .rev()
            .find(|row| row.rail.is_some())
            .map(|row| row.id);
        if let Some(above) = above {
            if let Some(rail) = self.row_mut(above).and_then(|row| row.rail.as_mut()) {
                rail.bottom = false;
            }
            self.changed(above);
            if self.last_step == Some(above) {
                self.last_step = None;
                self.after_prompt = true;
            }
        }
        // The rows after it — its notes — are under a prompt now: they run
        // up from its box, across a prompt's gap.
        let first_after = self
            .index
            .get(&id)
            .and_then(|&at| self.rows[at + 1..].iter().find(|row| row.rail.is_some()))
            .map(|row| row.id);
        if let Some(first) = first_after {
            if let Some(rail) = self.row_mut(first).and_then(|row| row.rail.as_mut()) {
                rail.top = true;
                rail.after_prompt = true;
            }
            self.changed(first);
        }
        self.plan = None;
        self.turn_answers.clear();
    }

    /// The agent has taken prompt `prompt` — after waiting behind a turn,
    /// with the note it went out after. Where anything has been drawn since
    /// it was typed, it moves to the end, where the conversation took it,
    /// with its notes, and a marker stays where it was typed: a whole turn
    /// between a prompt and its answer read as the prompt coming before
    /// answers it never saw.
    pub fn prompt_taken(&mut self, prompt: &str, waited: Option<String>) {
        let Some(old) = self.prompts.get(prompt).copied() else {
            return;
        };
        self.set_delivery(
            prompt,
            match waited {
                Some(note) => Delivery::Waited(note),
                None => Delivery::Delivered,
            },
        );
        let Some(at) = self.index.get(&old).copied() else {
            return;
        };
        let ours = |row: &Row| row.of_prompt.as_deref() == Some(prompt);
        if self.rows[at + 1..].iter().all(ours) {
            self.seat_in_place(old, at);
            return;
        }
        self.finalize();
        let notes: Vec<(RowId, String)> = self.rows[at + 1..]
            .iter()
            .filter(|row| ours(row))
            .filter_map(|row| match &row.kind {
                Kind::Note(text) => Some((row.id, text.clone())),
                _ => None,
            })
            .collect();
        // The line runs on through the marker between two steps (David,
        // 2026-10-05: "This shouldn't break the line between the bullets
        // on the left").
        let above = at
            .checked_sub(1)
            .and_then(|before| self.rows.get(before))
            .filter(|row| row.rail.is_some())
            .map(|row| row.id);
        let below = self.rows[at + 1..]
            .iter()
            .find(|row| !ours(row))
            .is_some_and(|row| row.rail.is_some());
        let through = above.is_some() && below;
        let Some(Kind::User(mut user)) = self.row(old).map(|row| row.kind.clone()) else {
            return;
        };
        user.within_turn = false;
        if let Some(row) = self.row_mut(old) {
            row.kind = Kind::Marker {
                prompt: prompt.to_string(),
                through,
            };
        }
        self.changed(old);
        for (id, _) in &notes {
            self.remove(*id);
        }
        if let Some(above) = above.filter(|_| through) {
            self.rejoin(above);
        }
        // Seated again as a prompt is: the timeline above ends at its last
        // dot, and the next step's line runs up from the box.
        self.plan = None;
        self.turn_answers.clear();
        self.end_steps();
        let id = self.push(Kind::User(user), None, Dot::Plain);
        self.prompts.insert(prompt.to_string(), id);
        self.last_prompt = Some(id);
        for (_, text) in notes {
            let id = self.push_step(Kind::Note(text), Dot::Note);
            if let Some(row) = self.row_mut(id) {
                row.of_prompt = Some(prompt.to_string());
            }
        }
    }

    /// Take prompt `prompt` out, with its notes and its marker: withdrawn
    /// before it went, it is no part of the conversation.
    pub fn remove_prompt(&mut self, prompt: &str) {
        let ids: Vec<RowId> = self
            .rows
            .iter()
            .filter(|row| {
                row.of_prompt.as_deref() == Some(prompt) || self.prompt_of(row.id) == Some(prompt)
            })
            .map(|row| row.id)
            .collect();
        // The step the prompt stood under, which may have run its line down
        // to it — a prompt waiting inside a turn — or to its notes.
        let above = ids
            .iter()
            .filter_map(|id| self.index.get(id).copied())
            .min()
            .and_then(|at| self.rows[..at].iter().rev().find(|row| row.rail.is_some()))
            .map(|row| row.id);
        let had_last = self.last_step.is_some();
        for id in ids {
            self.remove(id);
        }
        self.prompts.remove(prompt);
        if self.last_prompt.is_some_and(|id| self.row(id).is_none()) {
            self.last_prompt = None;
        }
        // Mended where it went: the step above runs on only to a row that
        // runs up to it, and the turn's next step joins it when the
        // prompt's notes were the last of it (review, 2026-10-20).
        if let Some(above) = above {
            let Some(&at) = self.index.get(&above) else {
                return;
            };
            let runs_on = self.rows.get(at + 1).is_some_and(|row| {
                row.rail.is_some_and(|rail| rail.top && !rail.after_prompt)
                    || matches!(
                        &row.kind,
                        Kind::User(User {
                            within_turn: true,
                            ..
                        })
                    )
            });
            if !runs_on {
                if let Some(rail) = self.row_mut(above).and_then(|row| row.rail.as_mut()) {
                    rail.bottom = false;
                }
                self.changed(above);
            }
            if had_last && self.last_step.is_none() && at + 1 == self.rows.len() {
                self.last_step = Some(above);
            }
        }
    }

    /// One of the IDE's records of where a prompt got to, read back.
    fn delivery_record(&mut self, prompt: &str, state: &str, detail: Option<String>) {
        match state {
            "taken" => self.prompt_taken(prompt, detail),
            "refused" => self.set_delivery(prompt, Delivery::Refused(detail.unwrap_or_default())),
            "notSent" => self.set_delivery(prompt, Delivery::NotSent(detail.unwrap_or_default())),
            "lost" => self.set_delivery(prompt, Delivery::Lost),
            "withinTurn" => self.set_delivery(prompt, Delivery::WithinTurn),
            "withdrawn" => self.remove_prompt(prompt),
            _ => {}
        }
    }

    /// Rewrite a note in place.
    pub fn rewrite_note(&mut self, id: RowId, text: &str) {
        if let Some(row) = self.row_mut(id) {
            if let Kind::Note(note) = &mut row.kind {
                *note = text.to_string();
            }
        }
        self.changed(id);
    }

    /// Close what is streaming, and first, draw a user message still being
    /// assembled, so it lands above what follows it.
    pub fn finalize(&mut self) {
        self.flush_user();
        self.close_stream();
    }

    fn flush_user(&mut self) {
        let Some(mut pending) = self.pending_user.take() else {
            return;
        };
        let briefed = std::mem::take(&mut pending.briefed);
        let prompt = pending.prompt.clone();
        self.user_message(pending);
        for note in briefed {
            match &prompt {
                Some(prompt) => {
                    self.prompt_note(prompt, &note);
                }
                None => {
                    self.note(&note);
                }
            }
        }
    }

    fn user_message(&mut self, pending: PendingUserMessage) {
        if pending.text.trim().is_empty() && pending.attachments.is_empty() {
            return;
        }
        // A prompt the IDE sent, as it recorded it: once, whatever said it
        // again, and on its way until a record says where it got to.
        if let Some(prompt) = pending.prompt {
            self.prompt_sent(
                &prompt,
                pending.text.trim_end().to_string(),
                pending.attachments,
                Delivery::Sent,
            );
            return;
        }
        // The agent's block, if one is open, belongs above this message.
        self.close_stream();
        let mut text = pending.text.trim_end().to_string();
        // A background task's report, delivered as a user message, is a
        // step of the agent's work, and drawn as one.
        if let Some((notes, rest)) = task_notifications(&text) {
            for note in &notes {
                self.lines.push(("note", note.title()));
                let dot = note.tone().map_or(Dot::Plain, Dot::Tone);
                self.push_step(
                    Kind::Task(Task {
                        title: note.title(),
                        report: note.report.clone(),
                    }),
                    dot,
                );
            }
            if rest.is_empty() && pending.attachments.is_empty() {
                return;
            }
            text = rest;
        }
        // A message that waited behind a turn went out after a note saying
        // so: drawn as the card's badge. Several that went out together
        // come back as the cards they were, the attachments on the last.
        if let Some(parts) = taste_core::queued::split_all(&text) {
            let last = parts.len().saturating_sub(1);
            for (index, (note, rest)) in parts.into_iter().enumerate() {
                let attachments = if index == last {
                    pending.attachments.clone()
                } else {
                    Vec::new()
                };
                self.user_row(rest, attachments, None, Delivery::Waited(note));
            }
            return;
        }
        self.user_row(text, pending.attachments, None, Delivery::Delivered);
    }

    fn user_row(
        &mut self,
        text: String,
        attachments: Vec<(String, ContentBlock)>,
        prompt: Option<PromptId>,
        delivery: Delivery,
    ) -> RowId {
        // A prompt starts a turn: the turn owns its plan and its answer, and
        // the timeline above ends at its last dot. One sent while a turn is
        // under way starts nothing yet: it waits inside that turn, which
        // goes on below it, so the line runs on past it.
        let within_turn = matches!(delivery, Delivery::Queued | Delivery::Asking { .. })
            && self.last_step.is_some();
        if !within_turn {
            self.plan = None;
            self.turn_answers.clear();
            self.end_steps();
        } else if let Some(above) = self.last_step {
            // The turn goes on past the card: the step above runs down
            // into it.
            if let Some(rail) = self.row_mut(above).and_then(|row| row.rail.as_mut()) {
                rail.bottom = true;
            }
            self.changed(above);
        }
        self.lines.push(("you", text.clone()));
        let id = self.push(
            Kind::User(User {
                text,
                attachments,
                prompt: prompt.clone(),
                delivery,
                within_turn,
            }),
            None,
            Dot::Plain,
        );
        if let Some(prompt) = prompt {
            self.prompts.insert(prompt, id);
        }
        self.last_prompt = Some(id);
        id
    }

    fn close_stream(&mut self) {
        if let Some(id) = self.answer.take() {
            let mut text = None;
            if let Some(Row {
                kind: Kind::Answer(answer),
                ..
            }) = self.row_mut(id)
            {
                answer.done = true;
                text = Some(answer.text.clone());
            }
            if let Some(text) = text {
                self.lines.push(("agent", text));
            }
            self.turn_answers.push(id);
            self.changed(id);
        }
        if let Some((id, since)) = self.thought.take() {
            if let Some(Row {
                kind: Kind::Thought(thought),
                ..
            }) = self.row_mut(id)
            {
                let elapsed = since.map(|since| since.elapsed()).unwrap_or_default();
                thought.header = Some(thought_header(elapsed, &thought.text));
            }
            self.changed(id);
        }
    }

    /// Every step still marked running is settled: its turn, its session,
    /// or the history it came from is over, and the agent never said.
    pub fn settle_running(&mut self) {
        let ids: Vec<RowId> = self.steps.values().copied().collect();
        for id in ids {
            let settled = match self.row_mut(id) {
                Some(Row {
                    kind: Kind::Step(step),
                    ..
                }) if step.running => {
                    step.running = false;
                    step.tone = "cut";
                    true
                }
                _ => false,
            };
            if settled {
                self.changed(id);
            }
        }
    }

    /// A permission question about `call` opened or closed.
    pub fn mark_waiting(&mut self, call: &str, waiting: bool) {
        let Some(id) = self.steps.get(call).copied() else {
            return;
        };
        if let Some(Row {
            kind: Kind::Step(step),
            ..
        }) = self.row_mut(id)
        {
            step.waiting = waiting;
        }
        self.changed(id);
    }

    /// How `call`'s permission was answered — kept until its row exists.
    pub fn mark_permission(&mut self, call: &str, icon: &str, tooltip: &str) {
        let mark = (icon.to_string(), tooltip.to_string());
        let Some(id) = self.steps.get(call).copied() else {
            self.marks.insert(call.to_string(), mark);
            return;
        };
        if let Some(Row {
            kind: Kind::Step(step),
            ..
        }) = self.row_mut(id)
        {
            step.permission = Some(mark);
        }
        self.changed(id);
    }

    /// The turn's answers shown whole: its suggested replies are offered.
    pub fn unclip_turn(&mut self) {
        for id in std::mem::take(&mut self.turn_answers) {
            if let Some(Row {
                kind: Kind::Answer(answer),
                ..
            }) = self.row_mut(id)
            {
                answer.whole = true;
            }
            self.changed(id);
        }
    }

    /// A step opened or closed onto its content.
    pub fn set_expanded(&mut self, id: RowId, open: bool) {
        if let Some(Row {
            kind: Kind::Step(step),
            ..
        }) = self.row_mut(id)
        {
            if step.expanded == open {
                return;
            }
            step.expanded = open;
        }
        self.changed(id);
    }

    /// The row whose document is the editor's tab in front, and no other.
    pub fn light(&mut self, id: Option<RowId>) {
        let was: Vec<RowId> = self
            .rows
            .iter()
            .filter(|row| row.lit && Some(row.id) != id)
            .map(|row| row.id)
            .collect();
        for previous in was {
            if let Some(row) = self.row_mut(previous) {
                row.lit = false;
            }
            self.changed(previous);
        }
        if let Some(id) = id {
            if let Some(row) = self.row_mut(id) {
                if row.lit {
                    return;
                }
                row.lit = true;
            }
            self.changed(id);
        }
    }

    /// One entry of the record, read.
    pub fn read(&mut self, record: Record) {
        match record {
            Record::Update(update) => self.apply(*update, false),
            Record::Note { text, prompt } => match prompt {
                Some(prompt) => {
                    self.prompt_note(&prompt, &text);
                }
                None => {
                    self.note(&text);
                }
            },
            Record::Delivery {
                prompt,
                state,
                detail,
            } => {
                self.finalize();
                self.delivery_record(&prompt, &state, detail);
            }
        }
    }

    /// The model of `updates`, read as a record is read back, its first
    /// row `first`: what a restore draws, built off the main thread. Its
    /// mirror lines are left to take; its changes are not, since every row
    /// is new.
    pub fn of_record(first: RowId, updates: Vec<Record>) -> Model {
        let mut model = Model::starting_at(first);
        for record in updates {
            model.read(record);
        }
        model.finalize();
        model.settle_running();
        model.changes.clear();
        model
    }
}

/// One entry of a conversation's record, parsed.
#[derive(Debug, Clone)]
pub(super) enum Record {
    Update(Box<SessionUpdate>),
    /// The IDE's note, and the prompt it is about, if it is a briefing's.
    Note {
        text: String,
        prompt: Option<PromptId>,
    },
    /// Where a prompt the IDE sent got to: `taken` (with the note it went
    /// out after, if it waited), `refused` or `notSent` (with why),
    /// `lost`, or `withdrawn`.
    Delivery {
        prompt: PromptId,
        state: String,
        detail: Option<String>,
    },
}

/// The key a record of a prompt's delivery is kept under, its id the value.
pub(super) const DELIVERY_RECORD: &str = "tasteDelivery";
/// The key the prompt a note is about is kept under, beside the note.
pub(super) const NOTE_PROMPT: &str = "tastePrompt";

impl Record {
    pub fn update(update: SessionUpdate) -> Record {
        Record::Update(Box::new(update))
    }

    #[cfg(test)]
    pub fn note(text: &str) -> Record {
        Record::Note {
            text: text.to_string(),
            prompt: None,
        }
    }

    /// A record's JSON as an entry: an update, the IDE's note, where a
    /// prompt got to, or `None` for what draws nothing (a session's marker)
    /// or does not parse.
    pub fn parse(value: serde_json::Value, note_key: &str) -> Option<Record> {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        if let Some(note) = text(note_key) {
            return Some(Record::Note {
                text: note,
                prompt: text(NOTE_PROMPT),
            });
        }
        if let Some(prompt) = text(DELIVERY_RECORD) {
            return Some(Record::Delivery {
                prompt,
                state: text("state").unwrap_or_default(),
                detail: text("detail"),
            });
        }
        serde_json::from_value::<SessionUpdate>(value)
            .ok()
            .map(Record::update)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        ContentChunk, PlanEntry, PlanEntryPriority, TextContent, ToolCall, ToolCallUpdate,
        ToolCallUpdateFields,
    };

    fn text(t: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(t)))
    }

    fn conversation() -> Vec<Record> {
        let mut records = vec![
            Record::update(SessionUpdate::UserMessageChunk(text("Why is it slow?"))),
            Record::update(SessionUpdate::AgentThoughtChunk(text(
                "Probably the rebuild.",
            ))),
        ];
        let mut read = ToolCall::new("a1", "Read src/main.rs");
        read.status = ToolCallStatus::InProgress;
        records.push(Record::update(SessionUpdate::ToolCall(read)));
        let mut fields = ToolCallUpdateFields::new();
        fields.status = Some(ToolCallStatus::Completed);
        records.push(Record::update(SessionUpdate::ToolCallUpdate(
            ToolCallUpdate::new("a1", fields),
        )));
        records.push(Record::update(SessionUpdate::Plan(Plan::new(vec![
            PlanEntry::new(
                "Fix it",
                PlanEntryPriority::High,
                PlanEntryStatus::InProgress,
            ),
        ]))));
        records.push(Record::update(SessionUpdate::AgentMessageChunk(text(
            "It rebuilds ",
        ))));
        records.push(Record::update(SessionUpdate::AgentMessageChunk(text(
            "every time.",
        ))));
        records.push(Record::note("a note between turns"));
        records.push(Record::update(SessionUpdate::UserMessageChunk(text(
            "Fix it.",
        ))));
        let mut edit = ToolCall::new("b1", "Edit src/main.rs");
        edit.status = ToolCallStatus::InProgress;
        records.push(Record::update(SessionUpdate::ToolCall(edit)));
        records
    }

    /// A conversation lived record by record and the same record read back
    /// come to the same rows, the line and all.
    #[test]
    fn live_and_restored_are_the_same_rows() {
        let mut live = Model::default();
        for record in conversation() {
            match record {
                Record::Update(update) => live.apply(*update, false),
                note @ Record::Note { .. } => live.read(note),
                delivery @ Record::Delivery { .. } => live.read(delivery),
            }
        }
        live.finalize();
        live.settle_running();
        let restored = Model::of_record(0, conversation());
        // The thought's header says how long it took, which differs.
        let strip = |rows: &[Row]| -> Vec<Row> {
            rows.iter()
                .cloned()
                .map(|mut row| {
                    row.at = 0;
                    if let Kind::Thought(thought) = &mut row.kind {
                        thought.header = thought.header.as_ref().map(|_| String::new());
                    }
                    row
                })
                .collect()
        };
        assert_eq!(strip(live.rows()), strip(restored.rows()));
    }

    /// The line: a step's runs up to the one before and down to the one
    /// after, a prompt ends it, and the first step under a prompt runs up
    /// across the gap from its box.
    #[test]
    fn the_rail_runs_between_steps_and_stops_at_a_prompt() {
        let model = Model::of_record(0, conversation());
        let rails: Vec<(String, Option<Rail>)> = model
            .rows()
            .iter()
            .map(|row| {
                let what = match &row.kind {
                    Kind::User(_) => "user",
                    Kind::Thought(_) => "thought",
                    Kind::Step(_) => "step",
                    Kind::Plan(_) => "plan",
                    Kind::Answer(_) => "answer",
                    Kind::Note(_) => "note",
                    Kind::Marker { .. } => "marker",
                    Kind::Task(_) => "task",
                };
                (what.to_string(), row.rail)
            })
            .collect();
        let on = |top, bottom, after_prompt| {
            Some(Rail {
                top,
                bottom,
                after_prompt,
            })
        };
        assert_eq!(
            rails,
            vec![
                ("user".into(), None),
                ("thought".into(), on(true, true, true)),
                ("step".into(), on(true, true, false)),
                ("plan".into(), on(true, true, false)),
                ("answer".into(), on(true, true, false)),
                ("note".into(), on(true, false, false)),
                ("user".into(), None),
                ("step".into(), on(true, false, true)),
            ]
        );
        // A running step at the end of a record is settled.
        let Kind::Step(last) = &model.rows().last().unwrap().kind else {
            panic!("a step")
        };
        assert!(!last.running);
        assert_eq!(last.tone, "cut");
    }

    fn said(prompt: &str, words: &str) -> Record {
        Record::update(SessionUpdate::UserMessageChunk(
            text(words).message_id(agent_client_protocol::schema::v1::MessageId::new(prompt)),
        ))
    }

    fn step(id: &str) -> Record {
        let mut call = ToolCall::new(id.to_string(), format!("Read {id}.rs"));
        call.status = ToolCallStatus::Completed;
        Record::update(SessionUpdate::ToolCall(call))
    }

    /// The agent's report of its API connection cut off goes when a
    /// rebuild cut it — the call before it a rebuild, or the environment
    /// mid-rebuild — and the rebuild's line ends at its dot again; the
    /// same words anywhere else, and the agent's other words after a
    /// rebuild, stay.
    #[test]
    fn a_connection_a_rebuild_cut_is_not_reported_as_an_error() {
        const LOST: &str =
            "API Error: Connection lost mid-response. The response above may be incomplete.";
        let rebuild = || {
            let mut call = ToolCall::new("r1", "mcp__taste-ide__devcontainer_reload");
            call.status = ToolCallStatus::Completed;
            SessionUpdate::ToolCall(call)
        };
        let answers = |model: &Model| {
            model
                .rows()
                .iter()
                .filter(|row| matches!(row.kind, Kind::Answer(_)))
                .count()
        };

        let mut model = Model::default();
        model.apply(rebuild(), false);
        model.apply(SessionUpdate::AgentMessageChunk(text(LOST)), false);
        assert_eq!(answers(&model), 0, "{:?}", shape(&model));
        assert_eq!(model.rows()[0].rail.map(|rail| rail.bottom), Some(false));
        model.note("the agent stopped with its container");
        assert_eq!(model.rows()[0].rail.map(|rail| rail.bottom), Some(true));

        let mut model = Model::default();
        model.apply(rebuild(), false);
        model.apply(
            SessionUpdate::AgentMessageChunk(text("Rebuilding now.")),
            false,
        );
        assert_eq!(answers(&model), 1);

        let mut model = Model::default();
        let Record::Update(read) = step("a1") else {
            unreachable!()
        };
        model.apply(*read, false);
        model.apply(SessionUpdate::AgentMessageChunk(text(LOST)), false);
        assert_eq!(answers(&model), 1);
        let mut model = Model::default();
        model.apply(SessionUpdate::AgentMessageChunk(text(LOST)), true);
        assert_eq!(answers(&model), 0);
    }

    fn taken(prompt: &str, waited: Option<&str>) -> Record {
        Record::Delivery {
            prompt: prompt.into(),
            state: "taken".into(),
            detail: waited.map(str::to_string),
        }
    }

    /// What a row is, for comparing two models: its kind and words, its
    /// line, and where its prompt got to.
    fn shape(model: &Model) -> Vec<String> {
        model
            .rows()
            .iter()
            .map(|row| match &row.kind {
                Kind::User(user) => format!("user {:?} {:?}", user.text, user.delivery),
                Kind::Marker { through, .. } => format!("marker through={through}"),
                Kind::Note(text) => format!("note {text:?} {:?}", row.rail),
                Kind::Step(step) => format!("step {} {:?}", step.call, row.rail),
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// A message typed during a turn and taken after it is drawn where it
    /// was taken, its briefing with it, and a marker where it was typed
    /// with the line running through it — lived, and read back from the
    /// record, the same.
    #[test]
    fn a_prompt_taken_after_a_turn_moves_and_leaves_its_marker() {
        let first = format!("{PROMPT_PREFIX}1");
        let second = format!("{PROMPT_PREFIX}2");
        let mut live = Model::default();
        live.prompt_sent(&first, "Fix it.".into(), Vec::new(), Delivery::Sent);
        live.prompt_taken(&first, None);
        live.read(step("a"));
        live.prompt_sent(
            &second,
            "And the docs.".into(),
            Vec::new(),
            Delivery::Queued,
        );
        live.prompt_note(&second, "oriented with this prompt");
        live.read(step("b"));
        live.prompt_taken(&second, Some("[queued note]".into()));
        live.read(step("c"));
        let rows = shape(&live);
        assert!(
            rows[0].starts_with("user \"Fix it.\" Delivered"),
            "{rows:#?}"
        );
        assert!(rows[1].starts_with("step a"), "{rows:#?}");
        assert_eq!(rows[2], "marker through=true", "{rows:#?}");
        assert!(rows[3].starts_with("step b"), "{rows:#?}");
        assert!(rows[4].contains("Waited"), "{rows:#?}");
        assert!(rows[5].starts_with("note \"oriented"), "{rows:#?}");
        assert!(rows[6].starts_with("step c"), "{rows:#?}");
        assert_eq!(rows.len(), 7);
        // The step above the marker runs on down through it.
        let Some(Rail { bottom, .. }) = live.rows()[1].rail else {
            panic!("a step")
        };
        assert!(bottom);

        // As recorded: each prompt as it was sent, where it got to as it
        // got there — and the second prompt's message said twice, as a
        // session's mark says it again.
        let record = vec![
            said(&first, "Fix it."),
            taken(&first, None),
            step("a"),
            said(&second, "And the docs."),
            Record::Note {
                text: "oriented with this prompt".into(),
                prompt: Some(second.clone()),
            },
            said(&second, "And the docs."),
            step("b"),
            taken(&second, Some("[queued note]")),
            step("c"),
        ];
        let restored = Model::of_record(0, record);
        assert_eq!(shape(&restored), rows);
    }

    /// Every row of a turn's timeline runs down to the next, whatever
    /// kind each is — a step, an answer between steps, a note — and the
    /// line is unbroken but for the dots (David, 2026-10-20: "The line
    /// through the dots should be contiguous").
    #[test]
    fn the_line_runs_down_from_every_row_but_the_last() {
        let first = format!("{PROMPT_PREFIX}1");
        let mut model = Model::default();
        model.prompt_sent(&first, "Fix it.".into(), Vec::new(), Delivery::Sent);
        model.prompt_taken(&first, None);
        model.prompt_note(&first, "oriented with this prompt");
        model.read(step("a"));
        model.read(Record::update(SessionUpdate::AgentMessageChunk(text(
            "Reading a.",
        ))));
        model.read(step("b"));
        model.read(Record::update(SessionUpdate::AgentMessageChunk(text(
            "Done.",
        ))));
        let rails: Vec<(bool, bool)> = model
            .rows()
            .iter()
            .filter_map(|row| row.rail.map(|rail| (rail.top, rail.bottom)))
            .collect();
        let last = rails.len() - 1;
        for (index, (top, bottom)) in rails.iter().enumerate() {
            assert!(*top, "row {index} runs up: {rails:?}");
            assert_eq!(*bottom, index != last, "row {index} runs down: {rails:?}");
        }
    }

    /// A prompt waiting inside a turn leaves no line behind it: withdrawn,
    /// the step above it ends where it did; never taken by the time the
    /// turn is over, it is a card of its own; and seated in place with its
    /// notes, the first note runs up from its box.
    #[test]
    fn a_waiting_prompt_leaves_no_line_behind() {
        let first = format!("{PROMPT_PREFIX}1");
        let second = format!("{PROMPT_PREFIX}2");
        let third = format!("{PROMPT_PREFIX}3");
        let base = || {
            let mut model = Model::default();
            model.prompt_sent(&first, "Fix it.".into(), Vec::new(), Delivery::Sent);
            model.prompt_taken(&first, None);
            model.read(step("a"));
            model.prompt_sent(
                &second,
                "And the docs.".into(),
                Vec::new(),
                Delivery::Queued,
            );
            model
        };
        // Withdrawn.
        let mut model = base();
        model.remove_prompt(&second);
        assert!(model.rows()[1].rail.is_some_and(|rail| !rail.bottom));
        model.read(step("b"));
        let b = model.rows()[2].rail.expect("a step");
        assert!(b.top && !b.after_prompt, "b runs on from a: {b:?}");
        assert!(model.rows()[1].rail.is_some_and(|rail| rail.bottom));
        // Never taken, and the next prompt ends the turn.
        let mut model = base();
        model.set_delivery(&second, Delivery::Lost);
        model.prompt_sent(&third, "Next.".into(), Vec::new(), Delivery::Sent);
        assert!(matches!(
            &model.rows()[2].kind,
            Kind::User(User {
                within_turn: false,
                ..
            })
        ));
        assert!(model.rows()[1].rail.is_some_and(|rail| !rail.bottom));
        // Seated in place, with a note.
        let mut model = base();
        model.prompt_note(&second, "oriented with this prompt");
        model.prompt_taken(&second, None);
        let note = model.rows()[3].rail.expect("a note");
        assert!(note.top && note.after_prompt, "{note:?}");
    }

    /// A message sent during a turn waits inside it: the line runs on from
    /// the step above it to the step below, which is still that turn's
    /// (David, 2026-10-19: "Discontinuity for the line"). Taken with
    /// nothing drawn after it, it is seated where it stands: the turn's line
    /// ends at the dot above it, and the next step's runs up from its box.
    #[test]
    fn a_prompt_sent_during_a_turn_keeps_the_line_running() {
        let first = format!("{PROMPT_PREFIX}1");
        let second = format!("{PROMPT_PREFIX}2");
        let mut model = Model::default();
        model.prompt_sent(&first, "Fix it.".into(), Vec::new(), Delivery::Sent);
        model.prompt_taken(&first, None);
        model.read(step("a"));
        model.prompt_sent(
            &second,
            "And the docs.".into(),
            Vec::new(),
            Delivery::Queued,
        );
        model.read(step("b"));
        let rows = model.rows();
        assert!(rows[1].rail.is_some_and(|rail| rail.bottom), "a runs down");
        assert!(
            matches!(
                &rows[2].kind,
                Kind::User(User {
                    within_turn: true,
                    ..
                })
            ),
            "{:?}",
            rows[2].kind
        );
        let b = rows[3].rail.expect("a step");
        assert!(b.top && !b.after_prompt, "b runs up to a: {b:?}");

        let mut model = Model::default();
        model.prompt_sent(&first, "Fix it.".into(), Vec::new(), Delivery::Sent);
        model.prompt_taken(&first, None);
        model.read(step("a"));
        model.prompt_sent(
            &second,
            "And the docs.".into(),
            Vec::new(),
            Delivery::Queued,
        );
        model.prompt_taken(&second, Some("[queued note]".into()));
        model.read(step("c"));
        let rows = model.rows();
        assert_eq!(
            rows.len(),
            4,
            "seated where it stands: {:#?}",
            shape(&model)
        );
        assert!(
            rows[1].rail.is_some_and(|rail| !rail.bottom),
            "a's line ends"
        );
        assert!(matches!(
            &rows[2].kind,
            Kind::User(User {
                within_turn: false,
                ..
            })
        ));
        let c = rows[3].rail.expect("a step");
        assert!(c.top && c.after_prompt, "c runs up from the box: {c:?}");
    }

    /// A prompt the agent refused stays, marked; one withdrawn before it
    /// went goes, with its notes.
    #[test]
    fn a_refused_prompt_stays_and_a_withdrawn_one_goes() {
        let refused = format!("{PROMPT_PREFIX}r");
        let withdrawn = format!("{PROMPT_PREFIX}w");
        let model = Model::of_record(
            0,
            vec![
                said(&refused, "Delete everything."),
                Record::Delivery {
                    prompt: refused.clone(),
                    state: "refused".into(),
                    detail: Some("no".into()),
                },
                said(&withdrawn, "REBUILD RESULT"),
                Record::Note {
                    text: "briefed".into(),
                    prompt: Some(withdrawn.clone()),
                },
                Record::Delivery {
                    prompt: withdrawn.clone(),
                    state: "withdrawn".into(),
                    detail: None,
                },
            ],
        );
        assert_eq!(
            shape(&model),
            vec![format!(
                "user \"Delete everything.\" {:?}",
                Delivery::Refused("no".into())
            )]
        );
        // Never taken, and in nobody's hands: on its way, for the pane to
        // call undelivered.
        let stranded = Model::of_record(0, vec![said(&withdrawn, "Hello?")]);
        assert_eq!(stranded.in_flight(), vec![withdrawn]);
    }
}

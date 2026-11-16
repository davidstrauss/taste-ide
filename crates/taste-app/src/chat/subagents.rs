//! **The conversation's subagents, as data.**
//!
//! What the chat's Subagents tab lists (`super::agents_tab`): every
//! subagent this conversation's agent started, read from the same updates
//! the transcript's rows are read from ([`super::model::Model::apply`]), so
//! a conversation lived and the same conversation drawn back from its record
//! list the same subagents, which is tested rather than hoped. Nothing here
//! touches a widget.
//!
//! **Only what the agent reports.** The pinned Claude Code adapter
//! (`claude-agent-acp` 0.107.0, to a client that asks for none of its
//! subagent extensions, which this one does not) says this much about a
//! subagent, and the tab shows this much and nothing it would have to
//! guess:
//!
//! - its launch is a tool call named `Agent` (`Task` before Claude Code
//!   renamed it), whose `rawInput` carries `description`, `subagent_type`,
//!   `prompt`, and `run_in_background`;
//! - its own tool calls are tool calls of the conversation, each stamped
//!   with the launch's id in `_meta.claudeCode.parentToolUseId`; its
//!   prose and thinking are not sent;
//! - Claude Code's account of the launch rides on the launch's updates as
//!   `_meta.claudeCode.toolResponse`: for a background one `isAsync`,
//!   `agentId`, and `resolvedModel`; for a foreground one, once it ends,
//!   `agentType`, `resolvedModel`, `totalDurationMs`, `totalTokens`,
//!   `totalToolUseCount`, and, when it handed its report back,
//!   `handbackReport.text`. A replay (`session/load`) has none of it, and
//!   none of the subagent's own calls either;
//! - a foreground subagent ends with its launch: completed, or failed — and
//!   stopped when `_meta.claudeCode.nonExecutionKind` says the run was
//!   `interrupted` or `cancelled`;
//! - a background one's launch completes at once ("Async agent launched"),
//!   and its end is reported by nothing but its report: the
//!   `SubagentHandback` call it makes, whose `rawInput.message` is the
//!   report and whose `toolResponse.success` says it was delivered. Claude
//!   Code's own end-of-task frame (`task_notification`) reaches only a
//!   client with the adapter's native-subagent extension, and the
//!   `<task-notification>` message the model gets is not sent, live or
//!   replayed. What is known instead is that the adapter holds a turn open
//!   until every subagent it started has ended, so one still unreported
//!   when its turn ends has ENDED, and how is not known;
//! - an older adapter's replay did send the `<task-notification>` message,
//!   which names the launch (`<tool-use-id>`) and how it ended, and it is
//!   read here when it does.
//!
//! No usage or cost is reported for a background subagent, nor any
//! duration; and ACP has no way for a client to stop one subagent — Stop
//! cancels the turn (`session/cancel`), which Claude Code ends its
//! subagents with.

use std::collections::HashMap;
use std::time::Instant;

use agent_client_protocol::schema::v1::{ToolCallContent, ToolCallStatus};
use serde_json::Value;

use super::content_text;
use super::model::RowId;

/// Where a subagent's run has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum State {
    Running,
    /// Ended and said so: a foreground launch completed, or a background
    /// one's report was delivered.
    Finished,
    Failed,
    /// Its run was interrupted — by Stop, which cancels the turn.
    Stopped,
    /// Its turn ended without its saying how: the adapter holds a turn
    /// until the subagents it started have ended, so it is not running,
    /// and nothing more is known.
    Ended,
}

impl State {
    /// Said in a row: "Running", "Finished", ….
    pub fn word(self) -> &'static str {
        match self {
            State::Running => "Running",
            State::Finished => "Finished",
            State::Failed => "Failed",
            State::Stopped => "Stopped",
            State::Ended => "Ended",
        }
    }

    /// The rail's tone for it (`rail.<tone>`, the transcript's traffic
    /// light): `live` while it runs, then `ok`, `fail`, or `cut` — the
    /// hollow ring of a call that never said how it went.
    pub fn tone(self) -> &'static str {
        match self {
            State::Running => "live",
            State::Finished => "ok",
            State::Failed => "fail",
            State::Stopped | State::Ended => "cut",
        }
    }
}

/// One of a subagent's own tool calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Child {
    pub call: String,
    /// Its line in the transcript (`Row::text`).
    pub line: String,
    /// `live`, `ok`, `fail`, or `cut`.
    pub tone: &'static str,
    /// Its row in the transcript.
    pub row: Option<RowId>,
}

/// One subagent, as reported so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Subagent {
    /// The id of the tool call that launched it.
    pub call: String,
    /// The launch's row in the transcript: where "Show in Transcript" goes.
    pub row: Option<RowId>,
    pub description: Option<String>,
    /// `subagent_type`, or Claude Code's `agentType`.
    pub kind: Option<String>,
    pub prompt: Option<String>,
    pub background: bool,
    /// The model Claude Code ran it on.
    pub model: Option<String>,
    pub state: State,
    /// Why it failed or stopped, in the agent's words.
    pub why: Option<String>,
    /// Its report: what it handed back, or what a task notification held.
    pub report: Option<String>,
    /// What its launch returned to the agent, for a foreground subagent
    /// whose report is not known otherwise.
    pub result: Option<String>,
    pub duration_ms: Option<u64>,
    pub tokens: Option<u64>,
    pub tool_uses: Option<u64>,
    pub steps: Vec<Child>,
}

impl Subagent {
    fn new(call: &str) -> Subagent {
        Subagent {
            call: call.to_string(),
            row: None,
            description: None,
            kind: None,
            prompt: None,
            background: false,
            model: None,
            state: State::Running,
            why: None,
            report: None,
            result: None,
            duration_ms: None,
            tokens: None,
            tool_uses: None,
            steps: Vec::new(),
        }
    }

    /// Its name in a list: the description it was given, or what it is.
    pub fn title(&self) -> String {
        self.description
            .clone()
            .or_else(|| self.kind.clone())
            .unwrap_or_else(|| "Subagent".to_string())
    }

    /// What it is doing now: its latest step, while it runs.
    pub fn doing(&self) -> Option<&str> {
        (self.state == State::Running)
            .then(|| self.steps.last().map(|step| step.line.as_str()))
            .flatten()
    }
}

/// A tool call's update, as the subagents read it.
pub(super) struct Seen<'a> {
    pub call: &'a str,
    pub name: Option<&'a str>,
    pub title: Option<&'a str>,
    pub status: Option<ToolCallStatus>,
    pub input: Option<&'a Value>,
    pub content: Option<&'a [ToolCallContent]>,
    pub meta: Option<&'a serde_json::Map<String, Value>>,
    /// Its line in the transcript, and its row there.
    pub line: Option<String>,
    pub row: Option<RowId>,
}

/// The conversation's subagents, in the order they were launched, and what
/// changed since the view last asked.
#[derive(Debug, Default)]
pub(super) struct Subagents {
    list: Vec<Subagent>,
    by_call: HashMap<String, usize>,
    /// A subagent's own call: whose, and which of its steps.
    child_of: HashMap<String, (usize, usize)>,
    /// What a `SubagentHandback` call said, until it says it was delivered.
    handbacks: HashMap<String, String>,
    /// When each was first seen here: the running clock. Not data of the
    /// conversation — a restored one has nothing running — so it is kept
    /// apart from it.
    seen: HashMap<usize, Instant>,
    /// The ones the last settling ended, until anything else happens: a
    /// "stopped" note that follows says how they ended.
    settled: Vec<usize>,
    changed: Vec<usize>,
}

/// The launch's tool, by the name the adapter sends.
fn is_launch(tool: Option<&str>, input: Option<&Value>) -> bool {
    matches!(tool, Some("Agent" | "Task"))
        || input.is_some_and(|input| input.get("subagent_type").is_some())
}

fn text_of(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// A call's content, as text: its text blocks, joined.
fn content_words(content: &[ToolCallContent]) -> Option<String> {
    let text = content
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Content(block) => content_text(&block.content),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn tone_of(status: ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Completed => "ok",
        ToolCallStatus::Failed => "fail",
        _ => "live",
    }
}

impl Subagents {
    pub fn list(&self) -> &[Subagent] {
        &self.list
    }

    pub fn get(&self, at: usize) -> Option<&Subagent> {
        self.list.get(at)
    }

    /// How many are running.
    pub fn running(&self) -> usize {
        self.list
            .iter()
            .filter(|agent| agent.state == State::Running)
            .count()
    }

    /// When subagent `at` was first seen here, for its running clock.
    pub fn since(&self, at: usize) -> Option<Instant> {
        self.seen.get(&at).copied()
    }

    /// The order a list shows them in: the running first, then the rest,
    /// each newest first.
    pub fn order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.list.len()).rev().collect();
        order.sort_by_key(|at| self.list[*at].state != State::Running);
        order
    }

    /// The ones that changed since the last time this was asked, in no
    /// particular order, each once.
    pub fn take_changed(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.changed)
    }

    fn changed(&mut self, at: usize) {
        if !self.changed.contains(&at) {
            self.changed.push(at);
        }
    }

    /// Whether call `call` can be one of a subagent's own: one already is,
    /// or its update names a parent.
    pub fn may_be_child(&self, call: &str, meta: Option<&serde_json::Map<String, Value>>) -> bool {
        self.child_of.contains_key(call)
            || meta
                .and_then(|meta| meta.get("claudeCode"))
                .and_then(|claude| claude.get("parentToolUseId"))
                .is_some()
    }

    /// A tool call's update: a launch, a subagent's own call, or neither.
    pub fn tool(&mut self, seen: Seen) {
        self.settled.clear();
        let claude = seen
            .meta
            .and_then(|meta| meta.get("claudeCode"))
            .and_then(Value::as_object);
        let tool = seen
            .name
            .filter(|name| !name.trim().is_empty())
            .or_else(|| {
                claude
                    .and_then(|c| c.get("toolName"))
                    .and_then(Value::as_str)
            });
        if let Some(&at) = self.by_call.get(seen.call) {
            self.launch(at, &seen, claude);
            return;
        }
        if is_launch(tool, seen.input) {
            let at = self.list.len();
            self.list.push(Subagent::new(seen.call));
            self.by_call.insert(seen.call.to_string(), at);
            self.seen.insert(at, Instant::now());
            self.launch(at, &seen, claude);
            return;
        }
        let parent = claude
            .and_then(|c| c.get("parentToolUseId"))
            .and_then(Value::as_str)
            .and_then(|parent| self.by_call.get(parent))
            .copied();
        let (at, step) = match (self.child_of.get(seen.call).copied(), parent) {
            (Some(known), _) => known,
            (None, Some(at)) => {
                let step = self.list[at].steps.len();
                self.list[at].steps.push(Child {
                    call: seen.call.to_string(),
                    line: String::new(),
                    tone: "live",
                    row: None,
                });
                self.child_of.insert(seen.call.to_string(), (at, step));
                (at, step)
            }
            (None, None) => return,
        };
        {
            let child = &mut self.list[at].steps[step];
            if let Some(line) = seen.line.filter(|line| !line.trim().is_empty()) {
                child.line = line;
            }
            if seen.row.is_some() {
                child.row = seen.row;
            }
            if let Some(status) = seen.status {
                child.tone = tone_of(status);
            }
        }
        // The report a subagent hands back is its call's input, delivered
        // once the call says it was.
        if tool == Some("SubagentHandback") {
            if let Some(message) = text_of(seen.input.and_then(|input| input.get("message"))) {
                self.handbacks.insert(seen.call.to_string(), message);
            }
            let delivered = claude
                .and_then(|c| c.get("toolResponse"))
                .and_then(|response| response.get("success"))
                .and_then(Value::as_bool)
                == Some(true);
            if delivered {
                if let Some(report) = self.handbacks.remove(seen.call) {
                    let agent = &mut self.list[at];
                    agent.report = Some(report);
                    if agent.background && matches!(agent.state, State::Running | State::Ended) {
                        agent.state = State::Finished;
                    }
                }
            }
        }
        self.changed(at);
    }

    /// An update of subagent `at`'s launch.
    fn launch(&mut self, at: usize, seen: &Seen, claude: Option<&serde_json::Map<String, Value>>) {
        let agent = &mut self.list[at];
        if seen.row.is_some() {
            agent.row = seen.row;
        }
        // The adapter titles the launch by its description once the input
        // has it, and by the tool's name before.
        if let Some(title) = seen.title.map(str::trim) {
            if agent.description.is_none() && !matches!(title, "" | "Task" | "Agent") {
                agent.description = Some(title.to_string());
            }
        }
        if let Some(input) = seen.input {
            if let Some(description) = text_of(input.get("description")) {
                agent.description = Some(description);
            }
            if let Some(kind) = text_of(input.get("subagent_type")) {
                agent.kind = Some(kind);
            }
            if let Some(prompt) = text_of(input.get("prompt")) {
                agent.prompt = Some(prompt);
            }
            if input.get("run_in_background").and_then(Value::as_bool) == Some(true) {
                agent.background = true;
            }
        }
        let response = claude.and_then(|c| c.get("toolResponse"));
        if let Some(response) = response.filter(|response| response.is_object()) {
            if response.get("isAsync").and_then(Value::as_bool) == Some(true) {
                agent.background = true;
            }
            if agent.kind.is_none() {
                agent.kind = text_of(response.get("agentType"));
            }
            if let Some(model) = text_of(response.get("resolvedModel")) {
                agent.model = Some(model);
            }
            let count = |key: &str| response.get(key).and_then(Value::as_u64);
            agent.duration_ms = count("totalDurationMs").or(agent.duration_ms);
            agent.tokens = count("totalTokens").or(agent.tokens);
            agent.tool_uses = count("totalToolUseCount").or(agent.tool_uses);
            if let Some(report) = text_of(
                response
                    .get("handbackReport")
                    .and_then(|report| report.get("text")),
            ) {
                agent.report = Some(report);
            }
        }
        let stopped = claude
            .and_then(|c| c.get("nonExecutionKind"))
            .and_then(Value::as_str)
            .is_some_and(|kind| matches!(kind, "interrupted" | "cancelled"));
        let words = seen.content.and_then(content_words);
        match seen.status {
            // A background launch completes as it starts: the subagent
            // runs on.
            Some(ToolCallStatus::Completed) if agent.background => {}
            Some(ToolCallStatus::Completed) => {
                agent.state = State::Finished;
                if words.is_some() {
                    agent.result = words;
                }
            }
            Some(ToolCallStatus::Failed) => {
                agent.state = if stopped {
                    State::Stopped
                } else {
                    State::Failed
                };
                if words.is_some() {
                    agent.why = words;
                }
            }
            _ => {}
        }
        self.changed(at);
    }

    /// Every one still running has ended: its turn, its session, or the
    /// history it came from is over, and nothing said how.
    pub fn settle(&mut self) {
        self.settled.clear();
        for at in 0..self.list.len() {
            let agent = &mut self.list[at];
            let mut moved = false;
            if agent.state == State::Running {
                agent.state = State::Ended;
                self.settled.push(at);
                moved = true;
            }
            for step in &mut agent.steps {
                if step.tone == "live" {
                    step.tone = "cut";
                    moved = true;
                }
            }
            if moved {
                self.changed(at);
            }
        }
    }

    /// The turn was stopped: what was running in it — or what its end
    /// just settled — was stopped with it.
    pub fn stopped(&mut self) {
        let settled = std::mem::take(&mut self.settled);
        for at in 0..self.list.len() {
            let agent = &mut self.list[at];
            let ended_here = settled.contains(&at) && agent.state == State::Ended;
            if agent.state == State::Running || ended_here {
                agent.state = State::Stopped;
                self.changed(at);
            }
        }
        self.settle();
        self.settled.clear();
    }

    /// Anything other than a tool call happened: what the last settling
    /// ended stays ended.
    pub fn moved_on(&mut self) {
        self.settled.clear();
    }

    /// A `<task-notification>`: how the subagent its launch names ended,
    /// and its report.
    pub fn notified(&mut self, call: Option<&str>, status: Option<&str>, report: Option<&str>) {
        let Some(&at) = call.and_then(|call| self.by_call.get(call)) else {
            return;
        };
        let agent = &mut self.list[at];
        match status {
            Some("completed") => agent.state = State::Finished,
            Some("failed") => agent.state = State::Failed,
            Some("killed" | "stopped" | "cancelled") => agent.state = State::Stopped,
            _ => {}
        }
        if let Some(report) = report.filter(|report| !report.trim().is_empty()) {
            agent.report = Some(report.to_string());
        }
        self.changed(at);
    }

    /// What changed was all of it: the view is drawn from the list whole.
    pub fn forget_changes(&mut self) {
        self.changed.clear();
    }
}

/// How long something ran, in a list's words: "42s", "3m 05s", "1h 12m".
pub(super) fn ran_for(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(value: Value) -> serde_json::Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn seen<'a>(
        call: &'a str,
        name: Option<&'a str>,
        status: Option<ToolCallStatus>,
        input: Option<&'a Value>,
        meta: Option<&'a serde_json::Map<String, Value>>,
    ) -> Seen<'a> {
        Seen {
            call,
            name,
            title: None,
            status,
            input,
            content: None,
            meta,
            line: Some(format!("line of {call}")),
            row: None,
        }
    }

    #[test]
    fn a_background_subagent_runs_until_its_report_is_delivered() {
        let mut agents = Subagents::default();
        let input = serde_json::json!({
            "description": "Review the brokers",
            "subagent_type": "general-purpose",
            "prompt": "Read them.",
            "run_in_background": true,
        });
        agents.tool(seen(
            "launch",
            Some("Agent"),
            Some(ToolCallStatus::Pending),
            Some(&input),
            None,
        ));
        let launched = meta(serde_json::json!({ "claudeCode": { "toolResponse": {
            "isAsync": true, "status": "async_launched", "resolvedModel": "claude-opus-5-5"
        }}}));
        agents.tool(seen("launch", None, None, None, Some(&launched)));
        agents.tool(seen(
            "launch",
            None,
            Some(ToolCallStatus::Completed),
            None,
            None,
        ));
        assert_eq!(agents.list()[0].state, State::Running);
        let child = meta(serde_json::json!({ "claudeCode": {
            "toolName": "Bash", "parentToolUseId": "launch" }}));
        agents.tool(seen(
            "c1",
            Some("Bash"),
            Some(ToolCallStatus::InProgress),
            None,
            Some(&child),
        ));
        assert_eq!(agents.list()[0].doing(), Some("line of c1"));
        let message = serde_json::json!({ "message": "## Findings\n\nNone." });
        let handback = meta(serde_json::json!({ "claudeCode": {
            "toolName": "SubagentHandback", "parentToolUseId": "launch" }}));
        agents.tool(seen(
            "h1",
            Some("SubagentHandback"),
            None,
            Some(&message),
            Some(&handback),
        ));
        assert_eq!(agents.list()[0].state, State::Running);
        let delivered = meta(serde_json::json!({ "claudeCode": {
            "toolName": "SubagentHandback", "parentToolUseId": "launch",
            "toolResponse": { "success": true } }}));
        agents.tool(seen("h1", None, None, None, Some(&delivered)));
        let agent = &agents.list()[0];
        assert_eq!(agent.state, State::Finished);
        assert_eq!(agent.report.as_deref(), Some("## Findings\n\nNone."));
        assert_eq!(agent.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(agent.steps.len(), 2);
    }

    #[test]
    fn a_foreground_subagent_ends_with_its_launch() {
        let mut agents = Subagents::default();
        let input =
            serde_json::json!({ "description": "Map the planner", "subagent_type": "Explore" });
        agents.tool(seen(
            "f",
            Some("Agent"),
            Some(ToolCallStatus::Pending),
            Some(&input),
            None,
        ));
        let done = meta(serde_json::json!({ "claudeCode": { "toolResponse": {
            "status": "completed", "totalDurationMs": 74125, "totalTokens": 41774,
            "totalToolUseCount": 11, "handbackReport": { "text": "The map." }
        }}}));
        agents.tool(seen("f", None, None, None, Some(&done)));
        agents.tool(seen("f", None, Some(ToolCallStatus::Completed), None, None));
        let agent = &agents.list()[0];
        assert_eq!(agent.state, State::Finished);
        assert_eq!(agent.report.as_deref(), Some("The map."));
        assert_eq!(
            (agent.duration_ms, agent.tokens, agent.tool_uses),
            (Some(74125), Some(41774), Some(11))
        );
        // Interrupted: stopped, not failed.
        let interrupted =
            meta(serde_json::json!({ "claudeCode": { "nonExecutionKind": "interrupted" }}));
        agents.tool(seen(
            "g",
            Some("Agent"),
            Some(ToolCallStatus::InProgress),
            None,
            None,
        ));
        agents.tool(seen(
            "g",
            None,
            Some(ToolCallStatus::Failed),
            None,
            Some(&interrupted),
        ));
        assert_eq!(agents.list()[1].state, State::Stopped);
    }

    #[test]
    fn a_stop_after_the_turn_settled_reads_the_same_as_one_before() {
        // Live: the turn's end settles first, then the note says it was
        // stopped. Read back: the note comes while it still runs.
        let launch = |agents: &mut Subagents| {
            let input = serde_json::json!({ "run_in_background": true });
            agents.tool(seen(
                "b",
                Some("Agent"),
                Some(ToolCallStatus::Completed),
                Some(&input),
                None,
            ));
        };
        let mut live = Subagents::default();
        launch(&mut live);
        live.settle();
        live.stopped();
        let mut read = Subagents::default();
        launch(&mut read);
        read.stopped();
        read.settle();
        assert_eq!(live.list(), read.list());
        assert_eq!(live.list()[0].state, State::Stopped);
        // A turn that ended and was not stopped leaves it ended, whatever
        // the next turn does.
        let mut ended = Subagents::default();
        launch(&mut ended);
        ended.settle();
        ended.moved_on();
        ended.stopped();
        assert_eq!(ended.list()[0].state, State::Ended);
    }

    #[test]
    fn the_running_come_first_and_the_newest_before_the_older() {
        let mut agents = Subagents::default();
        for call in ["a", "b", "c"] {
            agents.tool(seen(
                call,
                Some("Agent"),
                Some(ToolCallStatus::InProgress),
                None,
                None,
            ));
        }
        agents.tool(seen("c", None, Some(ToolCallStatus::Completed), None, None));
        assert_eq!(agents.order(), vec![1, 0, 2]);
    }

    #[test]
    fn durations_read_as_a_list_says_them() {
        assert_eq!(ran_for(42), "42s");
        assert_eq!(ran_for(185), "3m 05s");
        assert_eq!(ran_for(4320), "1h 12m");
    }
}

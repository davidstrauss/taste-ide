//! Issues: a ref, not a service.
//!
//! One file per issue on `refs/taste/issues` in the user's main checkout —
//! no database, no server, no daemon, and nothing in the working tree. The
//! ref travels with the repository, so an issue queue survives a machine,
//! and it reaches GitHub only when the *user* pushes (see
//! [`GitWorkspace::push_command_including_issues`]).
//!
//! **Layout.** One directory per issue:
//!
//! ```text
//! issues/i-0001/issue.md              front-matter + markdown body
//! issues/i-0001/comments/0001.md      one file per comment
//! ```
//!
//! Three decisions worth their ink:
//!
//! - **The path is the id.** There is no `id:` in the front-matter, because
//!   two places that must agree eventually do not. `issues/i-0001/` names
//!   the issue; everything else is content.
//! - **Comments are sibling files, not appended sections.** Two agents
//!   commenting at once touch disjoint paths, so the compare-and-swap loser
//!   re-reads, re-allocates its number and re-applies — no read-modify-write
//!   over shared bytes, and no way to clobber a comment or a body edit. It
//!   also keeps diffs honest: a comment is an added file, never a hunk in
//!   the middle of someone else's prose.
//! - **Ids are short, monotonic and zero-padded** (`i-0001`), allocated as
//!   "one past the highest that exists". Allocation is re-done on every
//!   attempt of the CAS loop, so two concurrent creates end up as `i-0001`
//!   and `i-0002` rather than one overwriting the other. A UUID would dodge
//!   the race by being unreadable; humans type these into chat messages.
//!
//! **Every write is host-side.** Agents reach this only through the MCP
//! issue tools, which run in the IDE process against the main checkout.
//! Nothing here runs a hook, touches HEAD, the index, or the working tree.
//! All of it blocks: callers wrap it in `spawn_blocking`.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context, Result};
use git2::Oid;

use crate::refs::RefFile;
use crate::review::ENV_BRANCH_PREFIX;
use crate::GitWorkspace;

/// Where the queue lives. One ref, one name, everywhere.
pub const ISSUES_REF: &str = "refs/taste/issues";

/// The backlog order, beside the issues on the same ref: one id per line,
/// top of the queue first.
///
/// One file, not a `position:` field per issue, and that is the whole
/// reason it works. Ordering is a statement about the *list* — moving one
/// issue up moves another down — so a per-issue field would need N writes
/// to say one thing, and two of them landing out of order would leave the
/// queue with two issues claiming the same place. One file is one
/// compare-and-swap: the loser of a race re-reads the winner's list and
/// re-applies its move to it.
///
/// It lives at the ref root rather than under `issues/`, where the reader
/// would have to tell it apart from an issue directory.
pub const ISSUES_ORDER_PATH: &str = "order";

/// Where a fetch lands the remote's queue before anything local moves.
/// Fetching straight onto [`ISSUES_REF`] would let the remote overwrite
/// local issues that were never pushed; this is the tracking ref the
/// reconcile step compares against.
pub const ISSUES_TRACKING_REF: &str = "refs/taste/issues-remote";

/// The refspec the user's push carries alongside their branch.
pub const ISSUES_PUSH_REFSPEC: &str = "refs/taste/issues:refs/taste/issues";

/// How many times a write re-reads and retries when the ref moved under it.
const CAS_ATTEMPTS: usize = 8;

/// Open or closed. Two states, deliberately: "who is working on it" is the
/// assignee, and a third state derived from the same fact is a second
/// mechanism that can disagree with the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IssueState {
    Open,
    Closed,
}

impl IssueState {
    pub fn as_str(self) -> &'static str {
        match self {
            IssueState::Open => "open",
            IssueState::Closed => "closed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "open" => Some(IssueState::Open),
            "closed" | "done" => Some(IssueState::Closed),
            _ => None,
        }
    }

    pub fn is_closed(self) -> bool {
        self == IssueState::Closed
    }
}

/// A branch this issue's work lives on, and the tip it had when it was
/// linked.
///
/// The tip is recorded because the honest workflow deletes branches: the
/// user merges from the review inbox and presses Delete Branch, and an
/// issue whose linked branch has *gone* would otherwise be unclosable
/// forever. With the tip written down, "is this work in the target branch"
/// stays answerable after the branch name is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueLink {
    /// e.g. `agents/env-1`.
    pub branch: String,
    /// The branch's tip when it was linked, if it resolved then.
    pub tip: Option<Oid>,
}

impl IssueLink {
    fn render(&self) -> String {
        match self.tip {
            Some(oid) => format!("{}@{oid}", self.branch),
            None => self.branch.clone(),
        }
    }

    fn parse(token: &str) -> Option<Self> {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        match token.rsplit_once('@') {
            Some((branch, oid)) if !branch.is_empty() => Some(IssueLink {
                branch: branch.to_string(),
                tip: Oid::from_str(oid).ok(),
            }),
            _ => Some(IssueLink {
                branch: token.to_string(),
                tip: None,
            }),
        }
    }
}

/// One comment file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// Its number within the issue, from the filename.
    pub seq: u32,
    /// The environment that wrote it.
    pub author: String,
    /// Seconds since the epoch.
    pub created: i64,
    pub body: String,
}

/// One issue, as the queue renders it and the tools return it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// `i-0001` — the directory name, which is the whole identity.
    pub id: String,
    pub title: String,
    pub state: IssueState,
    /// The environment that filed it.
    pub reporter: String,
    /// The environment that claimed it, if any.
    pub assignee: Option<String>,
    /// Seconds since the epoch.
    pub created: i64,
    pub updated: i64,
    pub labels: Vec<String>,
    pub links: Vec<IssueLink>,
    pub body: String,
    pub comments: Vec<Comment>,
}

impl Issue {
    fn path(id: &str) -> String {
        format!("issues/{id}/issue.md")
    }

    fn comment_path(id: &str, seq: u32) -> String {
        format!("issues/{id}/comments/{seq:04}.md")
    }

    /// Front-matter + body, as the blob is written.
    ///
    /// Empty fields are omitted rather than written blank: claiming an
    /// unassigned issue then adds one line to the diff instead of editing
    /// one, which is what a reviewer wants to see.
    pub fn render(&self) -> String {
        let mut out = String::from("---\n");
        out.push_str(&format!("title: {}\n", one_line(&self.title)));
        out.push_str(&format!("state: {}\n", self.state.as_str()));
        out.push_str(&format!("reporter: {}\n", one_line(&self.reporter)));
        if let Some(assignee) = &self.assignee {
            out.push_str(&format!("assignee: {}\n", one_line(assignee)));
        }
        out.push_str(&format!("created: {}\n", format_utc(self.created)));
        out.push_str(&format!("updated: {}\n", format_utc(self.updated)));
        if !self.labels.is_empty() {
            out.push_str(&format!("labels: {}\n", self.labels.join(", ")));
        }
        if !self.links.is_empty() {
            let links: Vec<String> = self.links.iter().map(IssueLink::render).collect();
            out.push_str(&format!("links: {}\n", links.join(", ")));
        }
        out.push_str("---\n\n");
        out.push_str(self.body.trim_end());
        out.push('\n');
        out
    }

    /// Parse a blob written by [`Issue::render`]. Unknown keys are ignored
    /// and a missing one takes its default: a queue that refuses to render
    /// because a future version added a field would be the worst possible
    /// failure mode for a file format that lives in git.
    pub fn parse(id: &str, text: &str) -> Result<Self> {
        let (front, body) =
            split_front_matter(text).with_context(|| format!("{id} has no front-matter block"))?;
        let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
        for line in front.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some((key, value)) = line.split_once(':') {
                fields.insert(key.trim(), value.trim());
            }
        }
        let created = fields
            .get("created")
            .and_then(|v| parse_utc(v))
            .unwrap_or(0);
        Ok(Issue {
            id: id.to_string(),
            title: fields.get("title").unwrap_or(&"(untitled)").to_string(),
            state: fields
                .get("state")
                .and_then(|v| IssueState::parse(v))
                .unwrap_or(IssueState::Open),
            reporter: fields.get("reporter").unwrap_or(&"unknown").to_string(),
            assignee: fields
                .get("assignee")
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            created,
            updated: fields
                .get("updated")
                .and_then(|v| parse_utc(v))
                .unwrap_or(created),
            labels: split_list(fields.get("labels").copied().unwrap_or_default()),
            links: split_list(fields.get("links").copied().unwrap_or_default())
                .iter()
                .filter_map(|token| IssueLink::parse(token))
                .collect(),
            body: body.trim().to_string(),
            comments: Vec::new(),
        })
    }

    /// Whether this issue is assigned to `env`.
    pub fn claimed_by(&self, env: &str) -> bool {
        self.assignee.as_deref() == Some(env)
    }
}

impl Comment {
    fn render(&self) -> String {
        format!(
            "---\nauthor: {}\ncreated: {}\n---\n\n{}\n",
            one_line(&self.author),
            format_utc(self.created),
            self.body.trim_end()
        )
    }

    fn parse(seq: u32, text: &str) -> Self {
        let (front, body) = split_front_matter(text).unwrap_or(("", text));
        let mut author = "unknown".to_string();
        let mut created = 0;
        for line in front.lines() {
            if let Some((key, value)) = line.split_once(':') {
                match key.trim() {
                    "author" => author = value.trim().to_string(),
                    "created" => created = parse_utc(value.trim()).unwrap_or(0),
                    _ => {}
                }
            }
        }
        Comment {
            seq,
            author,
            created,
            body: body.trim().to_string(),
        }
    }
}

/// What a claim did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The caller now owns it.
    Claimed(Issue),
    /// The caller already owned it; nothing was written.
    AlreadyMine(Issue),
}

/// One branch an issue's close is gated on, checked against a target
/// branch.
///
/// This is [`crate::Mergedness`] — the ONE mergedness fact — under the name
/// the close gate uses it by. The review lifecycle asks the same function
/// about the same branches; two implementations of `ahead == 0` is one too
/// many, and the one that drifts is always the one nobody is looking at.
pub type LinkCheck = crate::Mergedness;

/// What an environment is working on: one claimed issue, as the fleet row
/// says it out loud.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// `i-0001`.
    pub id: String,
    pub title: String,
    pub state: IssueState,
}

/// Where in the backlog an issue should move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueMove {
    Up,
    Down,
    Top,
    Bottom,
}

/// One change `issue_update` may make. Every field is optional; an update
/// with none of them is a no-op that still answers with the issue.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueChange {
    pub state: Option<IssueState>,
    /// A new title. User-side only — the MCP surface does not offer it,
    /// because retitling another environment's issue is not an agent's
    /// call.
    pub title: Option<String>,
    pub body: Option<String>,
    /// Replaces the label set wholesale. User-side only, as `title` is.
    pub labels: Option<Vec<String>>,
    pub comment: Option<String>,
}

impl IssueChange {
    pub fn is_empty(&self) -> bool {
        *self == IssueChange::default()
    }
}

/// Where the local issues ref stands against what a fetch brought back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueSync {
    /// No tracking ref: the remote has no issues, or none was fetched.
    NoRemote,
    /// Local and remote agree.
    Unchanged,
    /// There was no local ref; the remote's is now it.
    Adopted,
    /// The local ref moved forward to the remote's.
    FastForwarded { gained: usize },
    /// Local has everything the remote does, and more. The user's push
    /// carries it.
    LocalAhead { ahead: usize },
    /// Both moved. Nothing was changed — this is the compare-and-swap
    /// problem across two machines, and the alpha answer is to say so
    /// rather than to invent a merge.
    Diverged { ahead: usize, behind: usize },
}

impl IssueSync {
    /// The one line the UI shows, or `None` when there is no news.
    pub fn warning(&self) -> Option<String> {
        match self {
            IssueSync::Diverged { ahead, behind } => Some(format!(
                "Issues diverged from the remote ({ahead} local, {behind} remote): your \
                 local queue was kept, and pushing it will be refused until one side is \
                 rebuilt on the other."
            )),
            _ => None,
        }
    }
}

/// A step of a compare-and-swap transaction: either a tree to commit, or a
/// decision that needed no write.
enum Step<T> {
    Commit {
        changes: Vec<RefFile>,
        message: String,
        value: T,
    },
    Done(T),
}

impl GitWorkspace {
    /// Every issue on the ref, lowest id first, comments attached.
    pub fn issues(&self) -> Result<Vec<Issue>> {
        let Some(tree) = self.read_tree_at_ref(ISSUES_REF)? else {
            return Ok(Vec::new());
        };
        let mut issues: BTreeMap<String, Issue> = BTreeMap::new();
        let mut comments: Vec<(String, Comment)> = Vec::new();
        for entry in &tree.entries {
            let Some((id, rest)) = issue_path_parts(&entry.path) else {
                continue;
            };
            let bytes = self.read_blob(entry.oid)?;
            let text = String::from_utf8_lossy(&bytes);
            if rest == "issue.md" {
                match Issue::parse(id, &text) {
                    Ok(issue) => {
                        issues.insert(id.to_string(), issue);
                    }
                    // One malformed issue must not blank the queue.
                    Err(e) => tracing::warn!("skipping issue {id}: {e:#}"),
                }
            } else if let Some(seq) = rest
                .strip_prefix("comments/")
                .and_then(|f| f.strip_suffix(".md"))
                .and_then(|n| n.parse::<u32>().ok())
            {
                comments.push((id.to_string(), Comment::parse(seq, &text)));
            }
        }
        for (id, comment) in comments {
            if let Some(issue) = issues.get_mut(&id) {
                issue.comments.push(comment);
            }
        }
        let mut out: Vec<Issue> = issues.into_values().collect();
        for issue in &mut out {
            issue.comments.sort_by_key(|c| c.seq);
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// The backlog, in the order the USER put it in.
    ///
    /// The order file is advisory in exactly one direction: it can only
    /// reorder issues that exist. Ids in it that no longer do are skipped,
    /// and issues it does not mention append in id order — so a queue that
    /// has never been reordered reads exactly as it did before there was an
    /// order file, and a create racing a reorder cannot lose the new issue.
    pub fn ordered_issues(&self) -> Result<Vec<Issue>> {
        let issues = self.issues()?;
        let order = self.issue_order()?;
        Ok(apply_order(issues, &order))
    }

    /// The order file's contents, as written: ids, one per line, unfiltered.
    /// Callers wanting the effective order want [`GitWorkspace::ordered_issues`].
    pub fn issue_order(&self) -> Result<Vec<String>> {
        let Some(bytes) = self.read_file_at_ref(ISSUES_REF, ISSUES_ORDER_PATH)? else {
            return Ok(Vec::new());
        };
        Ok(parse_order(&String::from_utf8_lossy(&bytes)))
    }

    /// Move one issue within the backlog, and write the whole order back.
    ///
    /// Every ordering write is a compare-and-swap on the issues ref like any
    /// other, and the order is recomputed from what is really there on each
    /// attempt — so a reorder that loses a race retries against the winner's
    /// list rather than reinstating the list it read first.
    pub fn issue_move(&self, id: &str, direction: IssueMove) -> Result<Vec<String>> {
        validate_id(id)?;
        let id = id.to_string();
        self.issue_transaction(move |git| {
            let order = git.effective_order()?;
            let Some(at) = order.iter().position(|other| other == &id) else {
                bail!("no issue {id} — issue_list shows what there is");
            };
            let to = match direction {
                IssueMove::Up => at.saturating_sub(1),
                IssueMove::Down => (at + 1).min(order.len() - 1),
                IssueMove::Top => 0,
                IssueMove::Bottom => order.len() - 1,
            };
            reorder_step(order, at, to, &id)
        })
    }

    /// Put an issue at an explicit position in the backlog. Indices past the
    /// end clamp to the end rather than failing: "put it last" is a thing a
    /// caller means, and an off-by-one is not worth an error.
    pub fn issue_reorder(&self, id: &str, index: usize) -> Result<Vec<String>> {
        validate_id(id)?;
        let id = id.to_string();
        self.issue_transaction(move |git| {
            let order = git.effective_order()?;
            let Some(at) = order.iter().position(|other| other == &id) else {
                bail!("no issue {id} — issue_list shows what there is");
            };
            let to = index.min(order.len() - 1);
            reorder_step(order, at, to, &id)
        })
    }

    /// Delete an issue: its directory and every comment in it, and its place
    /// in the order.
    ///
    /// The user's operation, never an agent's. Closing is how work ends;
    /// deleting is how a mistake is unmade, and the difference matters
    /// enough that only one of them is on the MCP surface.
    pub fn issue_delete(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        let id = id.to_string();
        self.issue_transaction(move |git| {
            let prefix = format!("issues/{id}/");
            let paths: Vec<String> = match git.read_tree_at_ref(ISSUES_REF)? {
                Some(tree) => tree
                    .entries
                    .iter()
                    .map(|entry| entry.path.clone())
                    .filter(|path| path.starts_with(&prefix))
                    .collect(),
                None => Vec::new(),
            };
            if paths.is_empty() {
                bail!("no issue {id} — issue_list shows what there is");
            }
            let mut changes: Vec<RefFile> = paths.into_iter().map(RefFile::delete).collect();
            let order = git.issue_order()?;
            if order.iter().any(|other| other == &id) {
                let kept: Vec<String> = order.into_iter().filter(|other| other != &id).collect();
                changes.push(RefFile::write(ISSUES_ORDER_PATH, render_order(&kept)));
            }
            Ok(Step::Commit {
                changes,
                message: format!("issues: delete {id}"),
                value: (),
            })
        })
    }

    /// The order as it stands, over the issues that actually exist.
    fn effective_order(&self) -> Result<Vec<String>> {
        let ids: Vec<String> = self.issues()?.into_iter().map(|issue| issue.id).collect();
        let order = self.issue_order()?;
        Ok(order_ids(ids, &order))
    }

    /// One issue by id, comments attached.
    pub fn issue(&self, id: &str) -> Result<Option<Issue>> {
        validate_id(id)?;
        let Some(bytes) = self.read_file_at_ref(ISSUES_REF, &Issue::path(id))? else {
            return Ok(None);
        };
        let mut issue = Issue::parse(id, &String::from_utf8_lossy(&bytes))?;
        if let Some(tree) = self.read_tree_at_ref(ISSUES_REF)? {
            let prefix = format!("issues/{id}/comments/");
            for entry in &tree.entries {
                let Some(seq) = entry
                    .path
                    .strip_prefix(&prefix)
                    .and_then(|f| f.strip_suffix(".md"))
                    .and_then(|n| n.parse::<u32>().ok())
                else {
                    continue;
                };
                let bytes = self.read_blob(entry.oid)?;
                issue
                    .comments
                    .push(Comment::parse(seq, &String::from_utf8_lossy(&bytes)));
            }
            issue.comments.sort_by_key(|c| c.seq);
        }
        Ok(Some(issue))
    }

    /// File an issue. The id is allocated inside the CAS loop, so two
    /// writers racing get two issues, never one overwritten.
    pub fn issue_create(
        &self,
        title: &str,
        body: &str,
        labels: &[String],
        reporter: &str,
    ) -> Result<Issue> {
        let title = one_line(title);
        if title.is_empty() {
            bail!("an issue needs a title");
        }
        let labels: Vec<String> = labels
            .iter()
            .map(|l| one_line(l))
            .filter(|l| !l.is_empty())
            .collect();
        self.issue_transaction(|git| {
            let now = now_seconds();
            let id = git.next_issue_id()?;
            let issue = Issue {
                id: id.clone(),
                title: title.clone(),
                state: IssueState::Open,
                reporter: reporter.to_string(),
                assignee: None,
                created: now,
                updated: now,
                labels: labels.clone(),
                links: Vec::new(),
                body: body.trim().to_string(),
                comments: Vec::new(),
            };
            Ok(Step::Commit {
                changes: vec![RefFile::write(Issue::path(&id), issue.render())],
                message: format!("issues: open {id} — {title}"),
                value: issue,
            })
        })
    }

    /// Claim an issue for `env`.
    ///
    /// The environment is the *caller's*, resolved from the socket a tool
    /// call arrived on — never a parameter, so no agent can assign work to
    /// another. A double claim cannot be silently lost: the second writer's
    /// compare-and-swap fails, it re-reads, and it finds the issue taken.
    pub fn issue_claim(&self, id: &str, env: &str) -> Result<ClaimOutcome> {
        validate_id(id)?;
        self.issue_transaction(|git| {
            let issue = git.require_issue(id)?;
            if issue.claimed_by(env) {
                return Ok(Step::Done(ClaimOutcome::AlreadyMine(issue)));
            }
            if let Some(other) = &issue.assignee {
                bail!(
                    "{id} is already claimed by {other} — the claim landed first and nothing \
                     was changed. Pick another issue, or ask {other} to hand it back."
                );
            }
            let mut claimed = issue;
            claimed.assignee = Some(env.to_string());
            claimed.updated = now_seconds();
            Ok(Step::Commit {
                changes: vec![RefFile::write(Issue::path(id), claimed.render())],
                message: format!("issues: {id} claimed by {env}"),
                value: ClaimOutcome::Claimed(claimed),
            })
        })
    }

    /// Change an issue's state, title, body or labels, and/or append a
    /// comment.
    ///
    /// **Closing is gated on verified mergedness.** An issue may only close
    /// once every branch its work lives on is reachable from `target` —
    /// checked here, in the write path, rather than left to the caller's
    /// good intentions. Those branches are the issue's explicit links AND
    /// **the branch of record of the environment that claimed it**: a claim
    /// is a structured env↔issue link, so the environment's branch is
    /// evidence whether or not anyone remembered to call `issue_link`. An
    /// issue with neither closes freely: not all issues produce code.
    pub fn issue_update(
        &self,
        id: &str,
        change: &IssueChange,
        target: &str,
        author: &str,
    ) -> Result<Issue> {
        validate_id(id)?;
        self.issue_transaction(|git| {
            let issue = git.require_issue(id)?;
            if change.is_empty() {
                return Ok(Step::Done(issue));
            }
            let mut updated = issue.clone();
            let mut changes = Vec::new();
            let mut what: Vec<String> = Vec::new();

            if let Some(state) = change.state {
                if state.is_closed() && !issue.state.is_closed() {
                    let checks = git.issue_merge_check(&issue, target)?;
                    let blocked: Vec<&LinkCheck> = checks.iter().filter(|c| !c.merged).collect();
                    if !blocked.is_empty() {
                        let detail: Vec<String> = blocked
                            .iter()
                            .map(|c| match &c.note {
                                Some(note) => format!("{} ({note})", c.branch),
                                None => format!(
                                    "{} is {} commit{} ahead of {target}",
                                    c.branch,
                                    c.ahead,
                                    if c.ahead == 1 { "" } else { "s" }
                                ),
                            })
                            .collect();
                        bail!(
                            "refused: {id} cannot close while its work is unmerged — {}. \
                             Nothing was changed. Closing an issue means the work is IN \
                             {target}, which is a query, not a belief: the environment \
                             publishes and flags itself for review, the user merges, and the \
                             close goes through then.",
                            detail.join("; ")
                        );
                    }
                }
                if state != issue.state {
                    updated.state = state;
                    what.push(state.as_str().to_string());
                }
            }
            if let Some(title) = &change.title {
                let title = one_line(title);
                if title.is_empty() {
                    bail!("an issue needs a title — nothing was changed");
                }
                if title != issue.title {
                    updated.title = title;
                    what.push("title".into());
                }
            }
            if let Some(body) = &change.body {
                updated.body = body.trim().to_string();
                what.push("body".into());
            }
            if let Some(labels) = &change.labels {
                let labels: Vec<String> = labels
                    .iter()
                    .map(|l| one_line(l))
                    .filter(|l| !l.is_empty())
                    .collect();
                if labels != issue.labels {
                    updated.labels = labels;
                    what.push("labels".into());
                }
            }
            if let Some(comment) = &change.comment {
                let comment = comment.trim();
                if !comment.is_empty() {
                    let seq = issue.comments.iter().map(|c| c.seq).max().unwrap_or(0) + 1;
                    let record = Comment {
                        seq,
                        author: author.to_string(),
                        created: now_seconds(),
                        body: comment.to_string(),
                    };
                    changes.push(RefFile::write(
                        Issue::comment_path(id, seq),
                        record.render(),
                    ));
                    updated.comments.push(record);
                    what.push("comment".into());
                }
            }
            if what.is_empty() {
                return Ok(Step::Done(issue));
            }
            updated.updated = now_seconds();
            changes.push(RefFile::write(Issue::path(id), updated.render()));
            Ok(Step::Commit {
                changes,
                message: format!("issues: {id} {}", what.join(", ")),
                value: updated,
            })
        })
    }

    /// Link an environment's branch of record to an issue.
    ///
    /// The branch must be an environment branch (`agents/<env>`) that
    /// already exists in this checkout, so a link always names something the
    /// user can actually look at. With one branch per environment this is
    /// usually redundant with the claim — the close gate checks the
    /// claimant's branch either way — and stays worth having for the case
    /// the claim cannot express: work that landed from an environment other
    /// than the one holding the issue.
    pub fn issue_link(&self, id: &str, branch: &str) -> Result<Issue> {
        validate_id(id)?;
        let branch = branch.trim().trim_start_matches("refs/heads/").to_string();
        if crate::review::env_of_branch(&branch).is_none() {
            bail!(
                "{branch} is not an environment branch — links name work on \
                 {ENV_BRANCH_PREFIX}<environment>, the one branch of record each \
                 environment publishes to. There is no topic in the name: an environment \
                 has exactly one branch, which is what makes it the unit of review."
            );
        }
        if branch.contains('@') {
            bail!("{branch} contains '@', which the link format reserves for the branch tip");
        }
        let tip = self
            .branches_matching(ENV_BRANCH_PREFIX)?
            .into_iter()
            .find(|b| b.name == branch)
            .map(|b| b.oid)
            .with_context(|| {
                format!(
                    "no branch {branch} in the user's checkout — that environment has not \
                     published yet. Publish first, then link what landed."
                )
            })?;
        self.issue_transaction(|git| {
            let issue = git.require_issue(id)?;
            if issue
                .links
                .iter()
                .any(|l| l.branch == branch && l.tip == Some(tip))
            {
                return Ok(Step::Done(issue));
            }
            let mut updated = issue;
            updated.links.retain(|l| l.branch != branch);
            updated.links.push(IssueLink {
                branch: branch.clone(),
                tip: Some(tip),
            });
            updated.links.sort_by(|a, b| a.branch.cmp(&b.branch));
            updated.updated = now_seconds();
            Ok(Step::Commit {
                changes: vec![RefFile::write(Issue::path(id), updated.render())],
                message: format!("issues: {id} links {branch}"),
                value: updated,
            })
        })
    }

    /// Is all of this issue's work in `target`? One [`LinkCheck`] per
    /// branch — the query behind the close gate, exposed so a caller can
    /// *ask* before it tries.
    ///
    /// The branches checked are the issue's explicit links plus the branch
    /// of record of the environment that claimed it (when that environment
    /// has published at all — an environment that never published is not
    /// evidence of anything, and gating on a branch that does not exist
    /// would make every claimed-but-not-yet-started issue unclosable).
    /// Duplicates collapse: a link naming the claimant's own branch, which
    /// is the ordinary case, is checked once.
    pub fn issue_merge_check(&self, issue: &Issue, target: &str) -> Result<Vec<LinkCheck>> {
        let mut out: Vec<LinkCheck> = Vec::new();
        for link in &issue.links {
            out.push(self.mergedness(&link.branch, link.tip, target)?);
        }
        if let Some(env) = &issue.assignee {
            let branch = crate::review::env_branch(env);
            if !out.iter().any(|check| check.branch == branch) {
                if let Some(check) = self.env_mergedness(env, target)? {
                    out.push(check);
                }
            }
        }
        Ok(out)
    }

    /// Every issue `env` holds a claim on, in backlog order — the "working
    /// on" half of the env↔issue link, read from the environment's side.
    ///
    /// Open issues only: a closed issue an environment happens to still be
    /// the assignee of is history, not work in flight.
    pub fn claims_for(&self, env: &str) -> Result<Vec<Claim>> {
        Ok(self
            .ordered_issues()?
            .into_iter()
            .filter(|issue| issue.claimed_by(env) && !issue.state.is_closed())
            .map(|issue| Claim {
                id: issue.id,
                title: issue.title,
                state: issue.state,
            })
            .collect())
    }

    /// Drop every claim `env` holds, leaving a comment on each saying why.
    ///
    /// Called when an environment is destroyed. Silence would be worse than
    /// either alternative: an issue assigned to a world that no longer
    /// exists is unclaimable by anyone else and looks, in the queue, exactly
    /// like work in progress. The comment is the trail — who held it, and
    /// what happened to them.
    ///
    /// One transaction for all of them, so a destroy either releases the
    /// whole set or none of it. Returns the ids released.
    pub fn release_claims(&self, env: &str, reason: &str) -> Result<Vec<String>> {
        let env = env.to_string();
        let reason = reason.trim().to_string();
        self.issue_transaction(move |git| {
            let held: Vec<Issue> = git
                .issues()?
                .into_iter()
                .filter(|issue| issue.claimed_by(&env))
                .collect();
            if held.is_empty() {
                return Ok(Step::Done(Vec::new()));
            }
            let now = now_seconds();
            let mut changes = Vec::new();
            let mut ids = Vec::new();
            for issue in held {
                let seq = issue.comments.iter().map(|c| c.seq).max().unwrap_or(0) + 1;
                let comment = Comment {
                    seq,
                    author: env.clone(),
                    created: now,
                    body: format!("Claim released: {reason}"),
                };
                changes.push(RefFile::write(
                    Issue::comment_path(&issue.id, seq),
                    comment.render(),
                ));
                let mut released = issue;
                released.assignee = None;
                released.updated = now;
                changes.push(RefFile::write(Issue::path(&released.id), released.render()));
                ids.push(released.id);
            }
            Ok(Step::Commit {
                message: format!("issues: {env} released {}", ids.join(", ")),
                changes,
                value: ids,
            })
        })
    }

    /// The user's push, carrying the issues ref when there is one.
    ///
    /// Byte-identical to [`GitWorkspace::push_command`] on a workspace that
    /// has never filed an issue — the ride-along appears the moment the ref
    /// does, and never before. Still user-only: agents have no push target
    /// and this is only ever built for a button the human pressed.
    pub fn push_command_including_issues(&self) -> (String, Vec<String>) {
        match self.read_ref(ISSUES_REF) {
            Ok(Some(_)) => self.push_command_with(&[ISSUES_PUSH_REFSPEC]),
            _ => self.push_command(),
        }
    }

    /// Fetch the remote's issues ref into the local tracking ref. Separate
    /// from the branch fetch because a remote with no issues ref makes this
    /// fail, and that is the normal case before the first push — callers
    /// treat its failure as silence.
    pub fn fetch_issues_command(&self) -> (String, Vec<String>) {
        self.git_command_owned(vec![
            "fetch".to_string(),
            self.push_remote(),
            format!("+{ISSUES_REF}:{ISSUES_TRACKING_REF}"),
        ])
    }

    /// Reconcile the fetched tracking ref into the local issues ref.
    ///
    /// Fast-forward or nothing. Two machines that both moved the ref are
    /// the compare-and-swap problem writ large, and the honest alpha answer
    /// is a sentence, not a merge UI: the local queue is kept, and the push
    /// that would overwrite the remote's is refused by git itself.
    pub fn reconcile_issues(&self) -> Result<IssueSync> {
        let Some(remote) = self.read_ref(ISSUES_TRACKING_REF)? else {
            return Ok(IssueSync::NoRemote);
        };
        let Some(local) = self.read_ref(ISSUES_REF)? else {
            self.repo
                .reference(ISSUES_REF, remote, false, "issues: adopted from the remote")
                .context("adopting the remote issues ref")?;
            return Ok(IssueSync::Adopted);
        };
        if local == remote {
            return Ok(IssueSync::Unchanged);
        }
        let (ahead, behind) = self
            .repo
            .graph_ahead_behind(local, remote)
            .context("comparing the local and remote issue refs")?;
        match (ahead, behind) {
            (0, 0) => Ok(IssueSync::Unchanged),
            (0, gained) => {
                self.repo
                    .reference_matching(
                        ISSUES_REF,
                        remote,
                        true,
                        local,
                        "issues: fast-forwarded to the remote",
                    )
                    .context("fast-forwarding the issues ref")?;
                Ok(IssueSync::FastForwarded { gained })
            }
            (ahead, 0) => Ok(IssueSync::LocalAhead { ahead }),
            (ahead, behind) => Ok(IssueSync::Diverged { ahead, behind }),
        }
    }

    /// The branch a close is verified against: the checked-out branch of
    /// the user's main checkout, or `HEAD` when it is detached.
    pub fn issue_target_branch(&self) -> String {
        self.branch_name().unwrap_or_else(|| "HEAD".to_string())
    }

    fn require_issue(&self, id: &str) -> Result<Issue> {
        self.issue(id)?
            .with_context(|| format!("no issue {id} — issue_list shows what there is"))
    }

    /// One past the highest id on the ref. Re-run on every attempt of the
    /// CAS loop, which is exactly what makes concurrent creates safe.
    fn next_issue_id(&self) -> Result<String> {
        let highest = match self.read_tree_at_ref(ISSUES_REF)? {
            Some(tree) => tree
                .entries
                .iter()
                .filter_map(|entry| issue_path_parts(&entry.path))
                .filter_map(|(id, _)| id.strip_prefix("i-"))
                .filter_map(|n| n.parse::<u32>().ok())
                .max()
                .unwrap_or(0),
            None => 0,
        };
        Ok(format!("i-{:04}", highest + 1))
    }

    /// Read, decide, commit — and when the ref moved under the commit, do
    /// the whole thing again on the new tip. The closure re-reads every
    /// attempt, so its decisions (an id, a comment number, whether the
    /// issue is already claimed) are always made against what is really
    /// there. Errors from the closure are the caller's answer and are never
    /// retried: "already claimed" does not become true by trying harder.
    fn issue_transaction<T>(
        &self,
        mut build: impl FnMut(&GitWorkspace) -> Result<Step<T>>,
    ) -> Result<T> {
        let mut last: Option<anyhow::Error> = None;
        for _ in 0..CAS_ATTEMPTS {
            // A fresh handle per attempt. Everything this closure decides —
            // the next id, the next comment number, whether the issue is
            // already claimed — is read from the ref, and libgit2 answers
            // ref lookups out of a per-handle cache. A retry that re-read
            // through the handle that just lost the race would be told the
            // same thing again and allocate the same id again; opening the
            // repository is how "re-read" means re-read.
            let git = GitWorkspace::discover(&self.workdir)
                .context("this checkout is no longer a git repository")?;
            // The tip the decision is made against, captured before the
            // decision and handed to the write. Every id and comment number
            // below is "one past what is on THIS tree", so committing onto
            // any other tree would silently overwrite whatever arrived in
            // between — a lost issue, with a tidy commit chain to prove
            // nothing went wrong.
            let base = git.read_ref(ISSUES_REF)?;
            match build(&git)? {
                Step::Done(value) => return Ok(value),
                Step::Commit {
                    changes,
                    message,
                    value,
                } => match git.commit_to_ref_at(ISSUES_REF, base, &changes, &message) {
                    Ok(_) => return Ok(value),
                    Err(e) => last = Some(e),
                },
            }
        }
        Err(last
            .unwrap_or_else(|| anyhow!("the issues ref could not be written"))
            .context(
                "the issues ref kept moving under this write — nothing was changed; try again",
            ))
    }
}

/// The order file's lines, trimmed, blanks dropped. Nothing here validates
/// against the issues that exist — [`order_ids`] does that, because the
/// file is allowed to lag the queue.
fn parse_order(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn render_order(ids: &[String]) -> String {
    let mut out = String::new();
    for id in ids {
        out.push_str(id);
        out.push('\n');
    }
    out
}

/// The effective order: listed ids that still exist, in file order, then
/// everything unlisted in id order (which is creation order — the ids are
/// monotonic).
fn order_ids(ids: Vec<String>, order: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(ids.len());
    for wanted in order {
        if ids.iter().any(|id| id == wanted) && !out.contains(wanted) {
            out.push(wanted.clone());
        }
    }
    for id in ids {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// ...applied to whole issues.
fn apply_order(issues: Vec<Issue>, order: &[String]) -> Vec<Issue> {
    let ids: Vec<String> = issues.iter().map(|issue| issue.id.clone()).collect();
    let wanted = order_ids(ids, order);
    let mut by_id: BTreeMap<String, Issue> = issues
        .into_iter()
        .map(|issue| (issue.id.clone(), issue))
        .collect();
    wanted
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect()
}

/// The write half of every reordering operation: take `at` out, put it back
/// at `to`, and commit the whole list.
fn reorder_step(
    mut order: Vec<String>,
    at: usize,
    to: usize,
    id: &str,
) -> Result<Step<Vec<String>>> {
    if at == to {
        return Ok(Step::Done(order));
    }
    let moved = order.remove(at);
    order.insert(to, moved);
    Ok(Step::Commit {
        changes: vec![RefFile::write(ISSUES_ORDER_PATH, render_order(&order))],
        message: format!("issues: order {id} to {}", to + 1),
        value: order,
    })
}

/// `issues/<id>/<rest>` → `(id, rest)`.
fn issue_path_parts(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("issues/")?;
    let (id, rest) = rest.split_once('/')?;
    (!id.is_empty() && !rest.is_empty()).then_some((id, rest))
}

fn validate_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 32
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        bail!("{id:?} is not an issue id — they look like i-0001");
    }
    Ok(())
}

/// Front-matter and body of a `---`-fenced document.
fn split_front_matter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let body = rest[end + 4..].trim_start_matches('\n');
    Some((&rest[..end], body))
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// Front-matter values are one line each: a newline in a title would end
/// the field and start a new one.
fn one_line(text: &str) -> String {
    text.replace(['\n', '\r'], " ").trim().to_string()
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `2026-08-31T09:12:04Z`. Written rather than pulled in: a date crate for
/// six lines of civil-calendar arithmetic is a dependency the Flatpak
/// manifest would have to carry forever.
pub fn format_utc(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// The inverse, tolerant of a missing time part.
pub fn parse_utc(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, time) = match text.split_once('T') {
        Some((date, time)) => (date, time.trim_end_matches('Z')),
        None => (text, "00:00:00"),
    };
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: u32 = date.next()?.parse().ok()?;
    let day: u32 = date.next()?.parse().ok()?;
    let mut time = time.split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next().unwrap_or("0").parse().ok()?;
    let second: i64 = time.next().unwrap_or("0").parse().ok()?;
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Howard Hinnant's `days_from_civil`: days since 1970-01-01.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i64;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Its inverse.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
impl GitWorkspace {
    /// Point a ref wherever, no compare-and-swap — for staging the
    /// two-machine situations `reconcile_issues` has to survive.
    fn set_ref_for_test(&self, name: &str, oid: Oid) {
        self.repo.reference(name, oid, true, "test").unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn temp_repo() -> (tempfile::TempDir, GitWorkspace) {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config
            .set_str("user.email", "test@example.invalid")
            .unwrap();
        drop(repo);
        let ws = GitWorkspace::discover(dir.path()).unwrap();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("base").unwrap();
        (dir, ws)
    }

    #[test]
    fn an_issue_round_trips_through_its_file() {
        let issue = Issue {
            id: "i-0007".into(),
            title: "The queue: it does not render".into(),
            state: IssueState::Open,
            reporter: "primary".into(),
            assignee: Some("env-1".into()),
            created: 1_756_000_000,
            updated: 1_756_000_500,
            labels: vec!["ui".into(), "git".into()],
            links: vec![IssueLink {
                branch: "agents/env-1".into(),
                tip: Oid::from_str("3f2a1b0c9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a").ok(),
            }],
            body: "Steps:\n\n1. open it\n2. despair".into(),
            comments: Vec::new(),
        };
        let text = issue.render();
        let back = Issue::parse("i-0007", &text).unwrap();
        assert_eq!(back, issue);
        // The title's colon survives: only the first one splits.
        assert!(
            text.contains("title: The queue: it does not render"),
            "{text}"
        );
    }

    #[test]
    fn empty_fields_are_omitted_so_a_claim_is_a_one_line_diff() {
        let issue = Issue {
            id: "i-0001".into(),
            title: "t".into(),
            state: IssueState::Open,
            reporter: "primary".into(),
            assignee: None,
            created: 0,
            updated: 0,
            labels: Vec::new(),
            links: Vec::new(),
            body: "b".into(),
            comments: Vec::new(),
        };
        let text = issue.render();
        assert!(!text.contains("assignee:"), "{text}");
        assert!(!text.contains("labels:"), "{text}");
        assert!(!text.contains("links:"), "{text}");
        assert_eq!(Issue::parse("i-0001", &text).unwrap().assignee, None);
    }

    #[test]
    fn utc_stamps_round_trip() {
        for seconds in [0_i64, 1_756_638_724, 951_782_400, 4_102_444_800] {
            let text = format_utc(seconds);
            assert_eq!(parse_utc(&text), Some(seconds), "{text}");
        }
        assert_eq!(format_utc(1_756_638_724), "2025-08-31T11:12:04Z");
    }

    #[test]
    fn ids_are_allocated_one_past_the_highest() {
        let (_dir, ws) = temp_repo();
        assert_eq!(ws.next_issue_id().unwrap(), "i-0001");
        let first = ws.issue_create("first", "body", &[], "primary").unwrap();
        assert_eq!(first.id, "i-0001");
        let second = ws.issue_create("second", "", &[], "primary").unwrap();
        assert_eq!(second.id, "i-0002");
        let ids: Vec<String> = ws.issues().unwrap().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, vec!["i-0001", "i-0002"]);
    }

    #[test]
    fn concurrent_creates_get_distinct_ids() {
        // Two writers on one repository, no coordination but the ref's own
        // compare-and-swap. Both issues must survive with different ids.
        //
        // Repeated, because this is a race and one round proves nothing: at
        // 50 rounds the bug this test was written for — a loser whose id was
        // allocated against one tree and committed onto another, producing a
        // tidy two-commit chain holding one issue — showed up every time.
        for round in 0..50 {
            let (dir, ws) = temp_repo();
            let root = dir.path().to_path_buf();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let other = {
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let ws = GitWorkspace::discover(&root).unwrap();
                    barrier.wait();
                    ws.issue_create("from the other thread", "", &[], "env-1")
                        .unwrap()
                })
            };
            barrier.wait();
            let mine = ws
                .issue_create("from this thread", "", &[], "primary")
                .unwrap();
            let theirs = other.join().unwrap();
            assert_ne!(
                mine.id, theirs.id,
                "round {round}: two writers must not share an id"
            );
            let issues = ws.issues().unwrap();
            assert_eq!(issues.len(), 2, "round {round}: both writes survived");
            let titles: Vec<&str> = issues.iter().map(|i| i.title.as_str()).collect();
            assert!(titles.contains(&"from this thread"), "{titles:?}");
            assert!(titles.contains(&"from the other thread"), "{titles:?}");
        }
    }

    #[test]
    fn a_write_committed_onto_a_stale_tip_is_refused() {
        // The narrow version of the same rule: a caller that decided
        // against one tip may not have its changes replayed onto another.
        let (_dir, ws) = temp_repo();
        let first = ws.issue_create("first", "", &[], "primary").unwrap();
        let stale = ws.read_ref(ISSUES_REF).unwrap();
        ws.issue_create("second", "", &[], "primary").unwrap();
        let refused = ws
            .commit_to_ref_at(
                ISSUES_REF,
                stale,
                &[RefFile::write(Issue::path(&first.id), "clobbered")],
                "onto a tip that moved",
            )
            .unwrap_err()
            .to_string();
        assert!(refused.contains("moved under this write"), "{refused}");
        assert_eq!(ws.issues().unwrap().len(), 2, "nothing was overwritten");
    }

    #[test]
    fn a_second_claim_fails_and_names_the_claimer() {
        let (_dir, ws) = temp_repo();
        let issue = ws.issue_create("claim me", "", &[], "primary").unwrap();
        match ws.issue_claim(&issue.id, "env-1").unwrap() {
            ClaimOutcome::Claimed(issue) => assert_eq!(issue.assignee.as_deref(), Some("env-1")),
            other => panic!("{other:?}"),
        }
        // Re-claiming by the owner is idempotent, not an error.
        assert!(matches!(
            ws.issue_claim(&issue.id, "env-1").unwrap(),
            ClaimOutcome::AlreadyMine(_)
        ));
        let refused = ws.issue_claim(&issue.id, "env-2").unwrap_err().to_string();
        assert!(refused.contains("already claimed by env-1"), "{refused}");
        // And the loser changed nothing.
        assert_eq!(
            ws.issue(&issue.id).unwrap().unwrap().assignee.as_deref(),
            Some("env-1")
        );
    }

    #[test]
    fn comments_are_sibling_files_numbered_in_order() {
        let (_dir, ws) = temp_repo();
        let issue = ws
            .issue_create("talk to me", "body", &[], "primary")
            .unwrap();
        for text in ["first", "second"] {
            ws.issue_update(
                &issue.id,
                &IssueChange {
                    comment: Some(text.into()),
                    ..Default::default()
                },
                "HEAD",
                "env-1",
            )
            .unwrap();
        }
        let read = ws.issue(&issue.id).unwrap().unwrap();
        assert_eq!(read.comments.len(), 2);
        assert_eq!(read.comments[0].seq, 1);
        assert_eq!(read.comments[0].body, "first");
        assert_eq!(read.comments[1].author, "env-1");
        let tree = ws.read_tree_at_ref(ISSUES_REF).unwrap().unwrap();
        assert!(tree
            .paths()
            .any(|p| p == format!("issues/{}/comments/0002.md", issue.id)));
    }

    #[test]
    fn an_unlinked_issue_closes_freely() {
        let (_dir, ws) = temp_repo();
        let issue = ws
            .issue_create("no code needed", "", &[], "primary")
            .unwrap();
        let closed = ws
            .issue_update(
                &issue.id,
                &IssueChange {
                    state: Some(IssueState::Closed),
                    ..Default::default()
                },
                &ws.issue_target_branch(),
                "primary",
            )
            .unwrap();
        assert_eq!(closed.state, IssueState::Closed);
    }

    #[test]
    fn a_linked_issue_refuses_to_close_until_the_branch_is_merged() {
        let (dir, ws) = temp_repo();
        let target = ws.issue_target_branch();
        // Work on a published branch, one commit ahead of the target.
        ws.create_branch("agents/env-1").unwrap();
        ws.switch_branch("agents/env-1").unwrap();
        fs::write(dir.path().join("b.txt"), "work\n").unwrap();
        ws.stage(Path::new("b.txt")).unwrap();
        ws.commit("the work").unwrap();
        ws.switch_branch(&target).unwrap();

        let issue = ws.issue_create("needs code", "", &[], "primary").unwrap();
        let linked = ws.issue_link(&issue.id, "agents/env-1").unwrap();
        assert_eq!(linked.links.len(), 1);
        assert!(linked.links[0].tip.is_some(), "the tip is recorded");

        let close = IssueChange {
            state: Some(IssueState::Closed),
            ..Default::default()
        };
        let refused = ws
            .issue_update(&issue.id, &close, &target, "primary")
            .unwrap_err()
            .to_string();
        assert!(refused.contains("agents/env-1"), "{refused}");
        assert!(refused.contains("1 commit ahead"), "{refused}");
        assert_eq!(
            ws.issue(&issue.id).unwrap().unwrap().state,
            IssueState::Open,
            "a refused close changes nothing"
        );

        // Merge it, and the same call goes through.
        let outcome = ws.merge_branch("agents/env-1").unwrap();
        assert!(outcome.advanced(), "{outcome:?}");
        let closed = ws
            .issue_update(&issue.id, &close, &target, "primary")
            .unwrap();
        assert_eq!(closed.state, IssueState::Closed);
    }

    #[test]
    fn a_merged_branch_stays_verifiable_after_it_is_deleted() {
        let (dir, ws) = temp_repo();
        let target = ws.issue_target_branch();
        ws.create_branch("agents/env-1").unwrap();
        ws.switch_branch("agents/env-1").unwrap();
        fs::write(dir.path().join("b.txt"), "work\n").unwrap();
        ws.stage(Path::new("b.txt")).unwrap();
        ws.commit("the work").unwrap();
        ws.switch_branch(&target).unwrap();
        let issue = ws.issue_create("needs code", "", &[], "primary").unwrap();
        ws.issue_link(&issue.id, "agents/env-1").unwrap();
        ws.merge_branch("agents/env-1").unwrap();
        ws.delete_ref("refs/heads/agents/env-1").unwrap();

        let issue = ws.issue(&issue.id).unwrap().unwrap();
        let checks = ws.issue_merge_check(&issue, &target).unwrap();
        assert!(checks[0].merged, "{checks:?}");
        let closed = ws
            .issue_update(
                &issue.id,
                &IssueChange {
                    state: Some(IssueState::Closed),
                    ..Default::default()
                },
                &target,
                "primary",
            )
            .unwrap();
        assert_eq!(closed.state, IssueState::Closed);
    }

    #[test]
    fn linking_refuses_anything_that_is_not_an_environment_branch() {
        let (_dir, ws) = temp_repo();
        let issue = ws.issue_create("t", "", &[], "primary").unwrap();
        let refused = ws.issue_link(&issue.id, "main").unwrap_err().to_string();
        assert!(refused.contains("not an environment branch"), "{refused}");
        // A dead-generation topic branch is not one either: an environment
        // has exactly one branch, and the name says which environment.
        let nested = ws
            .issue_link(&issue.id, "agents/env-9/nothing")
            .unwrap_err()
            .to_string();
        assert!(nested.contains("not an environment branch"), "{nested}");
        let missing = ws
            .issue_link(&issue.id, "agents/env-9")
            .unwrap_err()
            .to_string();
        assert!(missing.contains("has not published yet"), "{missing}");
    }

    #[test]
    fn the_push_carries_the_issues_ref_only_once_there_is_one() {
        let (_dir, ws) = temp_repo();
        assert_eq!(ws.push_command_including_issues(), ws.push_command());
        ws.issue_create("first", "", &[], "primary").unwrap();
        let (program, args) = ws.push_command_including_issues();
        assert_eq!(program, "git");
        assert!(args.contains(&ISSUES_PUSH_REFSPEC.to_string()), "{args:?}");
        assert!(args.contains(&"push".to_string()), "{args:?}");
        // The branch is still explicit — refspecs only come after a remote.
        assert!(
            args.iter().any(|a| a.starts_with("HEAD:refs/heads/")),
            "{args:?}"
        );
    }

    #[test]
    fn reconcile_adopts_fast_forwards_and_refuses_to_guess() {
        let (_dir, ws) = temp_repo();
        assert_eq!(ws.reconcile_issues().unwrap(), IssueSync::NoRemote);

        // A remote queue with nothing local: adopt it wholesale.
        ws.issue_create("from the remote", "", &[], "primary")
            .unwrap();
        let remote_tip = ws.read_ref(ISSUES_REF).unwrap().unwrap();
        ws.delete_ref(ISSUES_REF).unwrap();
        ws.set_ref_for_test(ISSUES_TRACKING_REF, remote_tip);
        assert_eq!(ws.reconcile_issues().unwrap(), IssueSync::Adopted);
        assert_eq!(ws.issues().unwrap().len(), 1);
        assert_eq!(ws.reconcile_issues().unwrap(), IssueSync::Unchanged);

        // The remote gains one: fast-forward.
        let base = ws.read_ref(ISSUES_REF).unwrap().unwrap();
        ws.issue_create("added remotely", "", &[], "primary")
            .unwrap();
        let ahead = ws.read_ref(ISSUES_REF).unwrap().unwrap();
        ws.set_ref_for_test(ISSUES_TRACKING_REF, ahead);
        ws.set_ref_for_test(ISSUES_REF, base);
        assert_eq!(
            ws.reconcile_issues().unwrap(),
            IssueSync::FastForwarded { gained: 1 }
        );
        assert_eq!(ws.read_ref(ISSUES_REF).unwrap(), Some(ahead));

        // Local moves on: nothing to do, the push carries it.
        ws.issue_create("added locally", "", &[], "primary")
            .unwrap();
        assert_eq!(
            ws.reconcile_issues().unwrap(),
            IssueSync::LocalAhead { ahead: 1 }
        );

        // Both moved: kept, warned about, never merged.
        let local = ws.read_ref(ISSUES_REF).unwrap().unwrap();
        ws.set_ref_for_test(ISSUES_REF, ahead);
        ws.issue_create("added remotely again", "", &[], "primary")
            .unwrap();
        let remote = ws.read_ref(ISSUES_REF).unwrap().unwrap();
        ws.set_ref_for_test(ISSUES_TRACKING_REF, remote);
        ws.set_ref_for_test(ISSUES_REF, local);
        let sync = ws.reconcile_issues().unwrap();
        assert_eq!(
            sync,
            IssueSync::Diverged {
                ahead: 1,
                behind: 1
            }
        );
        assert!(sync.warning().unwrap().contains("diverged"));
        assert_eq!(
            ws.read_ref(ISSUES_REF).unwrap(),
            Some(local),
            "divergence changes nothing locally"
        );
    }

    #[test]
    fn the_fetch_argv_names_the_tracking_ref() {
        let (_dir, ws) = temp_repo();
        let (program, args) = ws.fetch_issues_command();
        assert_eq!(program, "git");
        assert_eq!(
            args,
            vec![
                "-C".to_string(),
                ws.workdir().display().to_string(),
                "fetch".to_string(),
                "origin".to_string(),
                format!("+{ISSUES_REF}:{ISSUES_TRACKING_REF}"),
            ]
        );
    }

    /// The backlog is the user's list. An untouched queue reads in creation
    /// order; a moved issue stays where it was put; an issue created after
    /// the last reorder appends rather than jumping the queue.
    #[test]
    fn the_backlog_order_is_user_authored_and_appends_what_it_does_not_know() {
        let (_dir, ws) = temp_repo();
        let ids: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|t| ws.issue_create(t, "", &[], "primary").unwrap().id)
            .collect();
        let read = |ws: &GitWorkspace| -> Vec<String> {
            ws.ordered_issues()
                .unwrap()
                .into_iter()
                .map(|i| i.id)
                .collect()
        };
        assert_eq!(read(&ws), ids, "no order file is creation order");
        assert!(ws.issue_order().unwrap().is_empty());

        ws.issue_move(&ids[2], IssueMove::Top).unwrap();
        assert_eq!(
            read(&ws),
            vec![ids[2].clone(), ids[0].clone(), ids[1].clone()]
        );
        ws.issue_move(&ids[2], IssueMove::Down).unwrap();
        assert_eq!(
            read(&ws),
            vec![ids[0].clone(), ids[2].clone(), ids[1].clone()]
        );
        ws.issue_reorder(&ids[0], 99).unwrap();
        assert_eq!(
            read(&ws),
            vec![ids[2].clone(), ids[1].clone(), ids[0].clone()]
        );
        // Moving the top item up is a no-op, not an error or a rotation.
        ws.issue_move(&ids[2], IssueMove::Up).unwrap();
        assert_eq!(
            read(&ws),
            vec![ids[2].clone(), ids[1].clone(), ids[0].clone()]
        );

        // A new issue lands at the end: the order file does not mention it,
        // and unlisted ids append rather than sorting to the front.
        let fresh = ws.issue_create("d", "", &[], "primary").unwrap().id;
        assert_eq!(read(&ws).last().unwrap(), &fresh);
        // The file is one id per line, top first.
        let text = String::from_utf8(
            ws.read_file_at_ref(ISSUES_REF, ISSUES_ORDER_PATH)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(text, format!("{}\n{}\n{}\n", ids[2], ids[1], ids[0]));
    }

    /// Two reorders at once: one wins, the loser retries against the
    /// winner's list, and the queue ends up holding every issue exactly
    /// once. The failure this guards is a loser replaying the list it read
    /// first, which would silently undo the winner's move.
    #[test]
    fn concurrent_reorders_leave_one_consistent_list() {
        for round in 0..25 {
            let (dir, ws) = temp_repo();
            let ids: Vec<String> = ["a", "b", "c", "d"]
                .iter()
                .map(|t| ws.issue_create(t, "", &[], "primary").unwrap().id)
                .collect();
            let root = dir.path().to_path_buf();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let other = {
                let root = root.clone();
                let barrier = barrier.clone();
                let last = ids[3].clone();
                std::thread::spawn(move || {
                    let ws = GitWorkspace::discover(&root).unwrap();
                    barrier.wait();
                    ws.issue_move(&last, IssueMove::Top).unwrap();
                })
            };
            barrier.wait();
            ws.issue_move(&ids[0], IssueMove::Bottom).unwrap();
            other.join().unwrap();

            let order = ws.ordered_issues().unwrap();
            let seen: Vec<String> = order.into_iter().map(|i| i.id).collect();
            assert_eq!(seen.len(), 4, "round {round}: {seen:?}");
            for id in &ids {
                assert_eq!(
                    seen.iter().filter(|other| *other == id).count(),
                    1,
                    "round {round}: {id} appears once — {seen:?}"
                );
            }
            // Both moves are visible in the result: whichever landed
            // second was applied to the first one's list, not to a stale
            // copy of the original.
            assert!(
                seen[0] == ids[3] || seen[3] == ids[0],
                "round {round}: neither move survived — {seen:?}"
            );
        }
    }

    /// Deleting removes the issue, every comment in it, and its place in
    /// the order — the last one because an order file naming a deleted
    /// issue is a queue that remembers something it cannot show.
    #[test]
    fn deleting_an_issue_takes_its_comments_and_its_place_with_it() {
        let (_dir, ws) = temp_repo();
        let first = ws.issue_create("keep", "", &[], "primary").unwrap().id;
        let doomed = ws.issue_create("delete me", "", &[], "primary").unwrap().id;
        ws.issue_update(
            &doomed,
            &IssueChange {
                comment: Some("something happened".into()),
                ..Default::default()
            },
            "HEAD",
            "primary",
        )
        .unwrap();
        ws.issue_move(&doomed, IssueMove::Top).unwrap();
        assert_eq!(
            ws.issue_order().unwrap(),
            vec![doomed.clone(), first.clone()]
        );

        ws.issue_delete(&doomed).unwrap();
        assert_eq!(ws.issue(&doomed).unwrap(), None);
        assert_eq!(ws.issues().unwrap().len(), 1);
        assert_eq!(ws.issue_order().unwrap(), vec![first.clone()]);
        let tree = ws.read_tree_at_ref(ISSUES_REF).unwrap().unwrap();
        assert!(
            !tree
                .paths()
                .any(|p| p.starts_with(&format!("issues/{doomed}/"))),
            "no file of a deleted issue survives"
        );
        assert!(
            ws.issue_delete(&doomed).is_err(),
            "deleting twice is an error"
        );
    }

    /// A claim is an env↔issue link readable from both ends, and releasing
    /// it leaves a trail rather than a silently unassigned issue.
    #[test]
    fn a_claim_reads_from_both_ends_and_releases_with_a_trail() {
        let (_dir, ws) = temp_repo();
        let mine = ws.issue_create("mine", "", &[], "primary").unwrap().id;
        let theirs = ws.issue_create("theirs", "", &[], "primary").unwrap().id;
        ws.issue_claim(&mine, "calm-1").unwrap();
        ws.issue_claim(&theirs, "spry-2").unwrap();

        // Issue → environment.
        assert_eq!(
            ws.issue(&mine).unwrap().unwrap().assignee.as_deref(),
            Some("calm-1")
        );
        // Environment → issue, with the title the fleet row shows.
        let claims = ws.claims_for("calm-1").unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].id, mine);
        assert_eq!(claims[0].title, "mine");
        assert!(ws.claims_for("nobody-3").unwrap().is_empty());

        let released = ws
            .release_claims("calm-1", "the environment was destroyed")
            .unwrap();
        assert_eq!(released, vec![mine.clone()]);
        let after = ws.issue(&mine).unwrap().unwrap();
        assert_eq!(after.assignee, None, "it is claimable again");
        assert_eq!(after.comments.len(), 1);
        assert_eq!(after.comments[0].author, "calm-1");
        assert!(
            after.comments[0].body.contains("destroyed"),
            "{:?}",
            after.comments[0]
        );
        // Somebody else's claim is untouched, and releasing nothing is fine.
        assert_eq!(
            ws.issue(&theirs).unwrap().unwrap().assignee.as_deref(),
            Some("spry-2")
        );
        assert!(ws.release_claims("calm-1", "again").unwrap().is_empty());

        // A closed issue is history, not work in flight.
        ws.issue_claim(&mine, "calm-1").unwrap();
        ws.issue_update(
            &mine,
            &IssueChange {
                state: Some(IssueState::Closed),
                ..Default::default()
            },
            &ws.issue_target_branch(),
            "calm-1",
        )
        .unwrap();
        assert!(ws.claims_for("calm-1").unwrap().is_empty());
    }

    /// The close gate follows the CLAIM, not just explicit links: an
    /// environment that claimed an issue and published unmerged work cannot
    /// close it by omitting `issue_link`.
    #[test]
    fn the_close_gate_checks_the_claiming_environments_branch() {
        let (dir, ws) = temp_repo();
        let target = ws.issue_target_branch();
        ws.create_branch("agents/calm-1").unwrap();
        ws.switch_branch("agents/calm-1").unwrap();
        std::fs::write(dir.path().join("b.txt"), "work\n").unwrap();
        ws.stage(Path::new("b.txt")).unwrap();
        ws.commit("the work").unwrap();
        ws.switch_branch(&target).unwrap();

        let id = ws
            .issue_create("needs code", "", &[], "primary")
            .unwrap()
            .id;
        ws.issue_claim(&id, "calm-1").unwrap();
        let close = IssueChange {
            state: Some(IssueState::Closed),
            ..Default::default()
        };
        let refused = ws
            .issue_update(&id, &close, &target, "calm-1")
            .unwrap_err()
            .to_string();
        assert!(refused.contains("agents/calm-1"), "{refused}");
        assert!(refused.contains("1 commit ahead"), "{refused}");

        // An environment that never published gates on nothing: a claim is
        // not evidence, a branch is.
        let unstarted = ws
            .issue_create("no code yet", "", &[], "primary")
            .unwrap()
            .id;
        ws.issue_claim(&unstarted, "spry-2").unwrap();
        assert_eq!(
            ws.issue_update(&unstarted, &close, &target, "spry-2")
                .unwrap()
                .state,
            IssueState::Closed
        );

        ws.merge_branch("agents/calm-1").unwrap();
        assert_eq!(
            ws.issue_update(&id, &close, &target, "calm-1")
                .unwrap()
                .state,
            IssueState::Closed
        );
    }

    /// Title and labels are user-side edits; they go through the same
    /// transaction and do not disturb anything else.
    #[test]
    fn an_edit_changes_title_and_labels_in_place() {
        let (_dir, ws) = temp_repo();
        let id = ws
            .issue_create("typo in the tilte", "body", &["ui".into()], "primary")
            .unwrap()
            .id;
        let edited = ws
            .issue_update(
                &id,
                &IssueChange {
                    title: Some("typo in the title".into()),
                    labels: Some(vec!["ui".into(), "docs".into()]),
                    ..Default::default()
                },
                "HEAD",
                "primary",
            )
            .unwrap();
        assert_eq!(edited.title, "typo in the title");
        assert_eq!(edited.labels, vec!["ui".to_string(), "docs".to_string()]);
        assert_eq!(edited.body, "body", "an edit changes what it names");
        assert_eq!(edited.state, IssueState::Open);
        let refused = ws
            .issue_update(
                &id,
                &IssueChange {
                    title: Some("   ".into()),
                    ..Default::default()
                },
                "HEAD",
                "primary",
            )
            .unwrap_err()
            .to_string();
        assert!(refused.contains("needs a title"), "{refused}");
    }

    #[test]
    fn a_malformed_issue_does_not_blank_the_queue() {
        let (_dir, ws) = temp_repo();
        ws.issue_create("good", "", &[], "primary").unwrap();
        ws.commit_to_ref(
            ISSUES_REF,
            &[RefFile::write("issues/i-9999/issue.md", "not front-matter")],
            "a broken issue",
        )
        .unwrap();
        let issues = ws.issues().unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].title, "good");
    }
}

//! **The Subagents tab**: the subagents this chat's agent has running or
//! has run, beside Chat, Utilization, and Settings in the chat's header.
//!
//! A list drawn from the conversation's model (`super::subagents`), never
//! from anything of its own, so a restored conversation lists what the live
//! one did. It is GTK's virtualized list over the subagents' launch ids,
//! the running first and then the rest, each newest first; a change to one
//! is drawn into that one's row where the list has it bound — or moves that
//! one row, when it stops running — once a frame, and while the tab is not
//! on screen nothing is drawn at all: the list is put in order when it is
//! next opened. Activating a row opens its detail on a page of its own:
//! its prompt, its report, its own steps, and the way to its place in the
//! transcript.
//!
//! What the adapter does not report is not drawn, and nothing here offers
//! to stop one subagent: ACP has no request for that, and Stop cancels the
//! turn, which ends its subagents with it.

use std::collections::HashMap;

use super::model::RowId;
use super::subagents::{ran_for, State, Subagent};
use super::*;

/// The newest of a subagent's steps its page lists; the rest are in the
/// transcript, which the page says.
const STEPS_SHOWN: usize = 60;

/// The tab's glyph, plain and with the busy mark — a green dot, the
/// environment rows' "up", the way the gear wears the link's.
pub(super) const AGENTS_ICON: &str = "taste-subagents-symbolic";
pub(super) const AGENTS_RUNNING_ICON: &str = "taste-subagents-running-symbolic";

/// The tab's widgets, and what of them the model's changes reach.
pub(super) struct AgentsTab {
    /// The face: what the chat's overlay holds, or a grafted tab does.
    pub panel: adw::NavigationView,
    stack: gtk::Stack,
    /// The subagents' launch ids, in the order the list shows them.
    store: gtk::gio::ListStore,
    list: gtk::ListView,
    /// The rows the list has bound, by launch id.
    bound: RefCell<HashMap<String, RowParts>>,
    detail: RefCell<Option<Detail>>,
    /// Changed since the last frame drew them.
    pending: RefCell<Vec<String>>,
    queued: Cell<bool>,
    /// Changed while nobody was looking: put in order when next shown.
    stale: Cell<bool>,
    clock: RefCell<Option<glib::SourceId>>,
    /// The badge as last said, so a change that does not move it says
    /// nothing.
    badge: RefCell<(String, String)>,
}

/// A bound row's parts, which a change is drawn into.
struct RowParts {
    mark: gtk::Box,
    title: gtk::Label,
    facts: gtk::Label,
    doing: gtk::Label,
}

/// The page of one subagent.
struct Detail {
    call: String,
    page: adw::NavigationPage,
    title: gtk::Label,
    /// Its facts, as property rows: how it stands, what it is, where it
    /// runs, on what, and what it spent.
    props: gtk::ListBox,
    /// The row that says how it stands, which the clock says again.
    status: Option<adw::ActionRow>,
    show: gtk::Button,
    body: gtk::Box,
    /// What the rows and the body were drawn from, so a change that
    /// leaves them alone — the clock — draws nothing but the clock.
    shown: Option<Subagent>,
}

/// A section's heading, as the Utilization tab's are.
fn heading(text: &str, first: bool) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .css_classes(["heading"])
        .xalign(0.0)
        .margin_bottom(6)
        .margin_top(if first { 0 } else { 18 })
        .build()
}

/// The mark a state wears: the spinner while it runs, then the traffic
/// light — green, red, or the ring of an end nobody described.
fn fill_mark(slot: &gtk::Box, tone: &str) {
    while let Some(child) = slot.first_child() {
        slot.remove(&child);
    }
    if tone == "live" {
        slot.append(&crate::spinner::image(12));
        return;
    }
    let dot = gtk::Box::builder()
        .css_classes([
            "env-dot",
            match tone {
                "ok" => "green",
                "fail" => "red",
                _ => "unknown",
            },
        ])
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    slot.append(&dot);
}

/// A 16px square the mark stands centred in, so a dot and a spinner hold
/// the same column.
fn mark_slot() -> gtk::Box {
    gtk::Box::builder()
        .width_request(16)
        .height_request(16)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
        .build()
}

/// How it stands, and for how long: "Running · 12s", "Finished · 1m 14s"
/// — the time while it runs by this machine's clock, and once it has
/// ended only as reported.
fn standing(agent: &Subagent, since: Option<std::time::Instant>) -> String {
    let seconds = match agent.state {
        State::Running => since.map(|since| since.elapsed().as_secs()),
        _ => agent.duration_ms.map(|ms| ms / 1000),
    };
    match seconds {
        Some(seconds) => format!("{} · {}", agent.state.word(), ran_for(seconds)),
        None => agent.state.word().to_string(),
    }
}

/// What it spent, as far as it is reported: "42k tokens · 11 tool uses".
fn spent(agent: &Subagent) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(tokens) = agent.tokens {
        parts.push(format!("{} tokens", round_tokens(tokens)));
    }
    if let Some(uses) = agent.tool_uses {
        parts.push(format!(
            "{uses} tool use{}",
            if uses == 1 { "" } else { "s" }
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// What a row says under its title: what it is, where it runs, how it
/// stands, how long, and what it spent — as far as each is reported.
fn facts(agent: &Subagent, since: Option<std::time::Instant>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(kind) = &agent.kind {
        parts.push(kind.clone());
    }
    if agent.background {
        parts.push("Background".to_string());
    }
    parts.push(standing(agent, since));
    parts.extend(spent(agent));
    parts.join(" · ")
}

/// A fact of a subagent's, as a property row: what it is, small, over its
/// value.
fn property(title: &str, value: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(value)
        .subtitle_selectable(true)
        .css_classes(["property"])
        .activatable(false)
        .build()
}

impl AgentsTab {
    pub fn new() -> AgentsTab {
        let store = gtk::gio::ListStore::new::<glib::BoxedAnyObject>();
        let list = gtk::ListView::builder()
            .model(&gtk::NoSelection::new(Some(store.clone())))
            .single_click_activate(true)
            .css_classes(["navigation-sidebar", "subagent-list"])
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let empty = adw::StatusPage::builder()
            .icon_name(AGENTS_ICON)
            .title("No Subagents")
            .description(
                "None in this conversation yet. The ones its agent starts are listed \
                 here, with what each is doing.",
            )
            .css_classes(["compact"])
            .vexpand(true)
            .build();
        let stack = gtk::Stack::new();
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&scroller, Some("list"));
        stack.set_visible_child_name("empty");
        let list_page = adw::NavigationPage::builder()
            .title("Subagents")
            .tag("list")
            .child(&stack)
            .build();
        let panel = adw::NavigationView::new();
        panel.add(&list_page);
        // Opaque over the transcript, exactly as the other shades are.
        panel.add_css_class("background");
        panel.set_vexpand(true);
        panel.set_visible(false);
        AgentsTab {
            panel,
            stack,
            store,
            list,
            bound: RefCell::new(HashMap::new()),
            detail: RefCell::new(None),
            pending: RefCell::new(Vec::new()),
            queued: Cell::new(false),
            stale: Cell::new(false),
            clock: RefCell::new(None),
            badge: RefCell::new((String::new(), String::new())),
        }
    }
}

impl ChatPane {
    /// The tab's list and pages answering the user, once the pane exists.
    pub(super) fn wire_agents_tab(self: &Rc<Self>) {
        let tab = &self.agents;
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            item.set_child(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));
        });
        {
            let weak = Rc::downgrade(self);
            factory.connect_bind(move |_, item| {
                let Some(pane) = weak.upgrade() else { return };
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let (Some(holder), Some(call)) = (
                    item.child().and_downcast::<gtk::Box>(),
                    item.item().and_downcast::<glib::BoxedAnyObject>(),
                ) else {
                    return;
                };
                let call = call.borrow::<String>().clone();
                pane.bind_agent_row(&holder, &call);
            });
        }
        {
            let weak = Rc::downgrade(self);
            factory.connect_unbind(move |_, item| {
                let Some(pane) = weak.upgrade() else { return };
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                if let Some(call) = item.item().and_downcast::<glib::BoxedAnyObject>() {
                    pane.agents
                        .bound
                        .borrow_mut()
                        .remove(&*call.borrow::<String>());
                }
                if let Some(holder) = item.child().and_downcast::<gtk::Box>() {
                    while let Some(child) = holder.first_child() {
                        holder.remove(&child);
                    }
                }
            });
        }
        tab.list.set_factory(Some(&factory));
        {
            let weak = Rc::downgrade(self);
            tab.list.connect_activate(move |_, position| {
                let Some(pane) = weak.upgrade() else { return };
                let call = pane
                    .agents
                    .store
                    .item(position)
                    .and_downcast::<glib::BoxedAnyObject>()
                    .map(|call| call.borrow::<String>().clone());
                if let Some(call) = call {
                    pane.open_agent(&call);
                }
            });
        }
        {
            // Opened: what changed while it was shut is put in order now,
            // and the running clocks start.
            let weak = Rc::downgrade(self);
            tab.panel.connect_map(move |_| {
                let Some(pane) = weak.upgrade() else { return };
                if pane.agents.stale.replace(false) {
                    pane.resync_agents();
                } else {
                    pane.flush_agents();
                }
                pane.sync_agent_clock();
            });
        }
        {
            let weak = Rc::downgrade(self);
            tab.panel.connect_unmap(move |_| {
                let Some(pane) = weak.upgrade() else { return };
                let clock = pane.agents.clock.borrow_mut().take();
                if let Some(clock) = clock {
                    clock.remove();
                }
            });
        }
        {
            let weak = Rc::downgrade(self);
            tab.panel.connect_popped(move |_, page| {
                let Some(pane) = weak.upgrade() else { return };
                let gone = pane
                    .agents
                    .detail
                    .borrow()
                    .as_ref()
                    .is_some_and(|detail| detail.page == *page);
                if gone {
                    pane.agents.detail.borrow_mut().take();
                }
            });
        }
        self.sync_agents_badge();
    }

    /// The model's subagents moved: the badge says so at once, and the
    /// list draws what changed on the next frame it is on screen for.
    pub(super) fn follow_subagents(&self) {
        let changed = self.model.borrow_mut().take_subagent_changes();
        if changed.is_empty() {
            return;
        }
        let calls: Vec<String> = {
            let model = self.model.borrow();
            changed
                .iter()
                .filter_map(|at| model.subagents().get(*at))
                .map(|agent| agent.call.clone())
                .collect()
        };
        self.sync_agents_badge();
        let tab = &self.agents;
        if !tab.panel.is_mapped() {
            tab.stale.set(true);
            return;
        }
        {
            let mut pending = tab.pending.borrow_mut();
            for call in calls {
                if !pending.contains(&call) {
                    pending.push(call);
                }
            }
        }
        if tab.queued.replace(true) {
            return;
        }
        let weak = self.weak_self.borrow().clone();
        tab.panel.add_tick_callback(move |_, _| {
            if let Some(pane) = weak.upgrade() {
                pane.agents.queued.set(false);
                pane.flush_agents();
            }
            glib::ControlFlow::Break
        });
    }

    /// A conversation drawn whole — a restore, a clear — and the list with
    /// it: in order from the model, and the page of one that is no longer
    /// there closed.
    pub(super) fn reset_agents(&self) {
        self.model.borrow_mut().take_subagent_changes();
        self.agents.pending.borrow_mut().clear();
        let open = self
            .agents
            .detail
            .borrow()
            .as_ref()
            .map(|detail| detail.call.clone());
        if let Some(call) = open {
            let there = self
                .model
                .borrow()
                .subagents()
                .list()
                .iter()
                .any(|agent| agent.call == call);
            if !there {
                self.agents.panel.pop_to_tag("list");
                self.agents.detail.borrow_mut().take();
            }
        }
        self.sync_agents_badge();
        if self.agents.panel.is_mapped() {
            self.resync_agents();
        } else {
            self.agents.stale.set(true);
        }
    }

    /// The list from the model whole, in order.
    fn resync_agents(&self) {
        let tab = &self.agents;
        tab.pending.borrow_mut().clear();
        let items: Vec<glib::BoxedAnyObject> = {
            let model = self.model.borrow();
            let agents = model.subagents();
            agents
                .order()
                .into_iter()
                .filter_map(|at| agents.get(at))
                .map(|agent| glib::BoxedAnyObject::new(agent.call.clone()))
                .collect()
        };
        tab.store.splice(0, tab.store.n_items(), &items);
        tab.stack
            .set_visible_child_name(if items.is_empty() { "empty" } else { "list" });
        self.refresh_agent_detail();
    }

    /// Draw what changed since the last frame: a row's parts where the
    /// list has it bound, and a row moved where it no longer stands in
    /// order — one that stopped running goes down among the ended.
    fn flush_agents(&self) {
        let tab = &self.agents;
        let pending = std::mem::take(&mut *tab.pending.borrow_mut());
        if pending.is_empty() {
            return;
        }
        let rank: HashMap<String, usize> = {
            let model = self.model.borrow();
            let agents = model.subagents();
            agents
                .order()
                .into_iter()
                .enumerate()
                .filter_map(|(rank, at)| agents.get(at).map(|agent| (agent.call.clone(), rank)))
                .collect()
        };
        let calls_in_store = || -> Vec<String> {
            (0..tab.store.n_items())
                .filter_map(|position| tab.store.item(position))
                .filter_map(|item| item.downcast::<glib::BoxedAnyObject>().ok())
                .map(|item| item.borrow::<String>().clone())
                .collect()
        };
        let mut ordered = pending;
        ordered.sort_by_key(|call| rank.get(call).copied().unwrap_or(usize::MAX));
        for call in ordered {
            let Some(&own) = rank.get(&call) else {
                continue;
            };
            let calls = calls_in_store();
            let at = calls.iter().position(|shown| *shown == call);
            // Where it belongs among the others as they stand.
            let want = calls
                .iter()
                .filter(|shown| **shown != call)
                .filter(|shown| rank.get(*shown).is_some_and(|rank| *rank < own))
                .count() as u32;
            match at {
                Some(at) if at as u32 == want => self.patch_agent_row(&call),
                Some(at) => {
                    tab.store.remove(at as u32);
                    tab.store
                        .insert(want, &glib::BoxedAnyObject::new(call.clone()));
                }
                None => tab
                    .store
                    .insert(want, &glib::BoxedAnyObject::new(call.clone())),
            }
        }
        tab.stack
            .set_visible_child_name(if tab.store.n_items() == 0 {
                "empty"
            } else {
                "list"
            });
        self.refresh_agent_detail();
        self.sync_agent_clock();
    }

    /// The subagent whose launch was `call`, and when it was first seen.
    fn agent(&self, call: &str) -> Option<(Subagent, Option<std::time::Instant>)> {
        let model = self.model.borrow();
        let agents = model.subagents();
        let at = agents.list().iter().position(|agent| agent.call == call)?;
        Some((agents.list()[at].clone(), agents.since(at)))
    }

    /// Row `call`'s widgets, built into `holder` from the model now.
    fn bind_agent_row(&self, holder: &gtk::Box, call: &str) {
        while let Some(child) = holder.first_child() {
            holder.remove(&child);
        }
        let mark = mark_slot();
        // On the title's first line, which the row's own padding puts a
        // little under the row's top.
        mark.set_margin_top(2);
        let title = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build()
            .full_text_on_hover();
        let facts = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption", "dim-label"])
            .build()
            .full_text_on_hover();
        let doing = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption", "dim-label"])
            .build()
            .full_text_on_hover();
        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .build();
        text.append(&title);
        text.append(&facts);
        text.append(&doing);
        let chevron = gtk::Image::builder()
            .icon_name("go-next-symbolic")
            .css_classes(["dim-label"])
            .valign(gtk::Align::Center)
            .build();
        holder.set_spacing(10);
        holder.append(&mark);
        holder.append(&text);
        holder.append(&chevron);
        let parts = RowParts {
            mark,
            title,
            facts,
            doing,
        };
        self.agents
            .bound
            .borrow_mut()
            .insert(call.to_string(), parts);
        self.patch_agent_row(call);
    }

    /// Row `call`'s parts, said again from the model.
    fn patch_agent_row(&self, call: &str) {
        let Some((agent, since)) = self.agent(call) else {
            return;
        };
        let bound = self.agents.bound.borrow();
        let Some(parts) = bound.get(call) else {
            return;
        };
        let tone = agent.state.tone();
        if parts.mark.widget_name() != tone {
            parts.mark.set_widget_name(tone);
            fill_mark(&parts.mark, tone);
        }
        parts.mark.set_tooltip_text(Some(agent.state.word()));
        parts.title.set_label(&agent.title());
        parts.facts.set_label(&facts(&agent, since));
        match agent.doing() {
            Some(doing) => {
                parts.doing.set_label(&single_line(doing, 200));
                parts.doing.set_visible(true);
            }
            None => parts.doing.set_visible(false),
        }
    }

    /// One subagent's page, pushed over the list.
    fn open_agent(self: &Rc<Self>, call: &str) {
        let Some((agent, _)) = self.agent(call) else {
            return;
        };
        let back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("All Subagents")
            .css_classes(["flat", "circular"])
            .action_name("navigation.pop")
            .valign(gtk::Align::Start)
            .build();
        let title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .attributes(&no_hyphens())
            .css_classes(["title-4"])
            .build();
        // The back button's 34 against the title's first line.
        title.set_margin_top(4);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        top.append(&back);
        top.append(&title);
        let props = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let show = gtk::Button::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name("go-jump-symbolic")
                    .label("Show in Transcript")
                    .build(),
            )
            .tooltip_text("Go to where the agent started it in the conversation")
            .halign(gtk::Align::Start)
            .build();
        {
            let weak = Rc::downgrade(self);
            let call = call.to_string();
            show.connect_clicked(move |_| {
                let Some(pane) = weak.upgrade() else { return };
                let row = pane.agent(&call).and_then(|(agent, _)| agent.row);
                if let Some(row) = row {
                    pane.show_in_transcript(row);
                }
            });
        }
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        // The back button stands out in the gutter, so the title, the
        // facts, and every section below share one column.
        top.set_margin_start(-6);
        column.append(&top);
        column.append(&props);
        column.append(&show);
        column.append(&body);
        let scroller = gtk::ScrolledWindow::builder()
            .child(&column)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let page = adw::NavigationPage::builder()
            .title(agent.title())
            .tag("subagent")
            .child(&scroller)
            .build();
        *self.agents.detail.borrow_mut() = Some(Detail {
            call: call.to_string(),
            page: page.clone(),
            title,
            props,
            status: None,
            show,
            body,
            shown: None,
        });
        self.refresh_agent_detail();
        // One page deep, always: another subagent's replaces it.
        if self.agents.panel.visible_page_tag().as_deref() == Some("subagent") {
            self.agents.panel.replace_with_tags(&["list"]);
        }
        self.agents.panel.push(&page);
    }

    /// The open page, said again from the model: its facts every time —
    /// the clock is in them — and its body when what it shows changed.
    fn refresh_agent_detail(&self) {
        let call = match self.agents.detail.borrow().as_ref() {
            Some(detail) => detail.call.clone(),
            None => return,
        };
        let Some((agent, since)) = self.agent(&call) else {
            return;
        };
        let mut guard = self.agents.detail.borrow_mut();
        let Some(detail) = guard.as_mut() else {
            return;
        };
        detail.title.set_label(&agent.title());
        detail.page.set_title(&agent.title());
        detail.show.set_sensitive(agent.row.is_some());
        if detail.shown.as_ref() == Some(&agent) {
            if let Some(status) = &detail.status {
                status.set_subtitle(&standing(&agent, since));
            }
            return;
        }
        // Its facts, each as reported.
        detail.props.remove_all();
        let status = property("Status", &standing(&agent, since));
        let mark = mark_slot();
        mark.set_valign(gtk::Align::Center);
        fill_mark(&mark, agent.state.tone());
        status.add_prefix(&mark);
        detail.props.append(&status);
        detail.status = Some(status);
        if let Some(kind) = &agent.kind {
            detail.props.append(&property("Type", kind));
        }
        detail.props.append(&property(
            "Runs",
            if agent.background {
                "In the background"
            } else {
                "While the agent waits on it"
            },
        ));
        if let Some(model) = &agent.model {
            detail.props.append(&property("Model", model));
        }
        if let Some(spent) = spent(&agent) {
            detail.props.append(&property("Used", &spent));
        }
        let body = detail.body.clone();
        detail.shown = Some(agent.clone());
        drop(guard);
        while let Some(child) = body.first_child() {
            body.remove(&child);
        }
        self.fill_agent_body(&body, &agent);
    }

    /// A page's sections: what it handed back (or why it failed), what it
    /// was asked, and its own steps.
    fn fill_agent_body(&self, body: &gtk::Box, agent: &Subagent) {
        // Keyed by the launch's call, as a step's own documents are, so the
        // launch's step in the transcript wears the blue while one is the
        // tab in front (`highlight_document`).
        let key = |what: &str| format!("{}/subagent-{what}", agent.call);
        let mut first = true;
        let mut section = |text: &str| {
            body.append(&heading(text, first));
            first = false;
        };
        // A report's links, issue pills, and paths go where an answer's
        // do (`rows.rs`, `answer_widget`).
        let events = self.workspace.events.clone();
        let on_link: Rc<dyn Fn(&str)> =
            Rc::new(
                move |url: &str| match url.strip_prefix(crate::issue_pill::SCHEME) {
                    Some(id) => {
                        events.publish(taste_core::Event::RevealIssueRequested(id.to_string()))
                    }
                    None => events.publish(taste_core::Event::OpenUrlRequested(url.to_string())),
                },
            );
        let base = self.document_base();
        let prose = |text: &str, title: &str, what: &str| {
            let (head, hidden) =
                crate::chatdoc::clip_prose(text, PROSE_CLIP_LINES, PROSE_CLIP_CHARS);
            let shown = crate::markdown_view::render_in(
                &head,
                on_link.clone(),
                Some(self.issues.clone()),
                base.clone(),
            );
            // Flush to the column, as an answer's prose is set.
            shown.set_margin_top(0);
            shown.set_margin_bottom(0);
            shown.set_margin_start(0);
            shown.set_margin_end(0);
            body.append(&shown);
            if hidden > 0 {
                body.append(&self.more_button(
                    &key(what),
                    hidden,
                    title,
                    Document::Text {
                        title: format!("{} · {what}", agent.title()),
                        body: text.to_string(),
                        markdown: true,
                        role: crate::chatdoc::TextRole::Response,
                    },
                ));
            }
        };
        if let Some(report) = &agent.report {
            section("Report");
            prose(report, "Open the whole report in the editor", "report");
        } else if let (State::Failed | State::Stopped, Some(why)) = (agent.state, &agent.why) {
            section(if agent.state == State::Failed {
                "Why It Failed"
            } else {
                "Why It Stopped"
            });
            body.append(&plain(why));
        } else if let Some(result) = &agent.result {
            // A foreground launch's own answer, when no report came apart
            // from it — which is what the agent was given back.
            section("Returned");
            prose(result, "Open what it returned in the editor", "returned");
        } else if agent.state == State::Ended {
            section("Report");
            body.append(&plain(
                "Its turn ended without its report reaching the chat, so how it ended \
                 is not known.",
            ));
        }
        if let Some(prompt) = &agent.prompt {
            section("Prompt");
            let (head, hidden) = crate::chatdoc::clip_prose(prompt, 12, PROSE_CLIP_CHARS);
            body.append(&plain(&head));
            if hidden > 0 {
                body.append(&self.more_button(
                    &key("prompt"),
                    hidden,
                    "Open the whole prompt in the editor",
                    Document::Text {
                        title: format!("{} · prompt", agent.title()),
                        body: prompt.clone(),
                        markdown: false,
                        role: crate::chatdoc::TextRole::Prompt,
                    },
                ));
            }
        }
        if !agent.steps.is_empty() {
            let count = agent.steps.len();
            section(&format!("Steps · {count}"));
            let earlier = count.saturating_sub(STEPS_SHOWN);
            if earlier > 0 {
                body.append(
                    &gtk::Label::builder()
                        .label(format!(
                            "{earlier} earlier step{} — in the transcript",
                            if earlier == 1 { "" } else { "s" }
                        ))
                        .xalign(0.0)
                        .css_classes(["caption", "dim-label"])
                        .margin_bottom(4)
                        .build(),
                );
            }
            let steps = gtk::ListBox::builder()
                .selection_mode(gtk::SelectionMode::None)
                .css_classes(["boxed-list"])
                .build();
            for step in &agent.steps[earlier..] {
                steps.append(&self.agent_step_row(step.line.as_str(), step.tone, step.row));
            }
            body.append(&steps);
        } else if agent.state == State::Running {
            section("Steps");
            body.append(&plain("None reported yet."));
        }
    }

    /// One of a subagent's steps: its line, its light, and a click to its
    /// place in the transcript.
    fn agent_step_row(&self, line: &str, tone: &str, row: Option<RowId>) -> gtk::ListBoxRow {
        let mark = mark_slot();
        mark.set_valign(gtk::Align::Center);
        fill_mark(&mark, tone);
        let label = gtk::Label::builder()
            .label(single_line(line, 300))
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["monospace", "caption"])
            .build()
            .full_text_on_hover();
        // Inset as the property rows above are, so the two lists' marks
        // and text stand in the same columns.
        let content = gtk::Box::builder()
            .spacing(12)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(12)
            .build();
        content.append(&mark);
        content.append(&label);
        let item = gtk::ListBoxRow::builder().child(&content).build();
        match row {
            Some(row) => {
                item.set_activatable(true);
                item.set_tooltip_text(Some("Show this step in the transcript"));
                let weak = self.weak_self.borrow().clone();
                item.connect_activate(move |_| {
                    if let Some(pane) = weak.upgrade() {
                        pane.show_in_transcript(row);
                    }
                });
            }
            None => item.set_activatable(false),
        }
        item
    }

    /// The running clocks, ticking while the tab is on screen and any of
    /// its subagents is running, and stopped otherwise.
    fn sync_agent_clock(&self) {
        let tab = &self.agents;
        let wanted = tab.panel.is_mapped() && self.model.borrow().subagents().running() > 0;
        let running = tab.clock.borrow().is_some();
        if wanted == running {
            return;
        }
        if !wanted {
            if let Some(clock) = tab.clock.borrow_mut().take() {
                clock.remove();
            }
            return;
        }
        let weak = self.weak_self.borrow().clone();
        let clock = glib::timeout_add_seconds_local(1, move || {
            let Some(pane) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let running: Vec<String> = pane
                .model
                .borrow()
                .subagents()
                .list()
                .iter()
                .filter(|agent| agent.state == State::Running)
                .map(|agent| agent.call.clone())
                .collect();
            if running.is_empty() || !pane.agents.panel.is_mapped() {
                pane.agents.clock.borrow_mut().take();
                return glib::ControlFlow::Break;
            }
            for call in &running {
                pane.patch_agent_row(call);
            }
            pane.refresh_agent_detail();
            glib::ControlFlow::Continue
        });
        *tab.clock.borrow_mut() = Some(clock);
    }

    /// The tab's glyph and tooltip: the busy mark while any subagent runs,
    /// told to the grafted tab too, the way the utilization glyph is.
    pub(super) fn sync_agents_badge(&self) {
        let (running, total) = {
            let model = self.model.borrow();
            let agents = model.subagents();
            (agents.running(), agents.list().len())
        };
        let glyph = if running > 0 {
            AGENTS_RUNNING_ICON
        } else {
            AGENTS_ICON
        };
        let tooltip = match (running, total) {
            (0, 0) => "Subagents".to_string(),
            (0, n) => format!("Subagents — {n} in this conversation, none running"),
            (n, _) => format!("Subagents — {n} running"),
        };
        let said = (glyph.to_string(), tooltip.clone());
        if *self.agents.badge.borrow() == said {
            return;
        }
        *self.agents.badge.borrow_mut() = said;
        self.agents_tab.set_icon_name(glyph);
        self.agents_tab.set_tooltip_text(Some(&tooltip));
        if let Some(hook) = self.on_agents_badge.borrow().as_ref() {
            hook(glyph, &tooltip);
        }
    }

    /// Say the badge again to whoever wears it now — a grafted tab made
    /// after the last change.
    pub fn republish_agents_badge(&self) {
        let (glyph, tooltip) = self.agents.badge.borrow().clone();
        if let Some(hook) = self.on_agents_badge.borrow().as_ref() {
            hook(&glyph, &tooltip);
        }
    }

    pub fn set_on_agents_badge(&self, hook: impl Fn(&str, &str) + 'static) {
        *self.on_agents_badge.borrow_mut() = Some(Rc::new(hook));
    }

    /// Who brings the transcript on screen when the window's strip holds
    /// it as a tab of its own.
    pub fn set_on_show_transcript(&self, hook: impl Fn() + 'static) {
        *self.on_show_transcript.borrow_mut() = Some(Rc::new(hook));
    }

    /// The transcript in front, at row `row`, lit for a moment as a search
    /// hit is.
    fn show_in_transcript(&self, row: RowId) {
        if self.grafted.get() {
            if let Some(hook) = self.on_show_transcript.borrow().as_ref() {
                hook();
            }
        } else {
            self.chat_tab.set_active(true);
        }
        self.scroll_to_item_then(&Item::Row(row), 24.0, |widget| {
            widget.add_css_class("search-hit");
            let widget = widget.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(1400), move || {
                widget.remove_css_class("search-hit");
            });
        });
    }

    /// TASTE_PROBE_CHECK only: the tab in front, and with `detail` the page
    /// of the subagent at that place in the list (from its end, when
    /// negative).
    #[doc(hidden)]
    pub(super) fn open_agents_for_probe(self: &Rc<Self>, detail: Option<i32>) {
        self.agents_tab.set_active(true);
        if let Some(at) = detail {
            let pane = self.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
                let count = pane.agents.store.n_items() as i32;
                let at = if at < 0 { count + at } else { at };
                let call = pane
                    .agents
                    .store
                    .item(at.max(0) as u32)
                    .and_downcast::<glib::BoxedAnyObject>()
                    .map(|call| call.borrow::<String>().clone());
                if let Some(call) = call {
                    pane.open_agent(&call);
                }
            });
        }
    }
}

/// Words as they were written, in a column: wrapped, selectable.
fn plain(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .attributes(&no_hyphens())
        .selectable(true)
        .build()
}

/// The updates the pinned adapter sends for a turn that starts three
/// subagents — a foreground one that finishes, a background one still at
/// work, and one whose launch fails — in the shapes a real session's
/// record has them: the probe's conversation, and the roundtrip's.
pub(super) fn probe_subagent_updates() -> Vec<SessionUpdate> {
    let child = |call: &str, parent: &str, title: &str, status: &str| {
        serde_json::json!({
            "sessionUpdate": "tool_call", "toolCallId": call, "title": title,
            "kind": "execute", "status": status, "name": "Bash",
            "_meta": { "claudeCode": { "toolName": "Bash", "parentToolUseId": parent } },
        })
    };
    let done = |call: &str, parent: &str| {
        serde_json::json!({
            "sessionUpdate": "tool_call_update", "toolCallId": call, "status": "completed",
            "_meta": { "claudeCode": { "toolName": "Bash", "parentToolUseId": parent } },
        })
    };
    let values = vec![
        serde_json::json!({
            "sessionUpdate": "tool_call", "toolCallId": "probe-agent-map", "title": "Task",
            "kind": "think", "status": "pending", "name": "Agent", "rawInput": {},
            "_meta": { "claudeCode": { "toolName": "Agent" } },
        }),
        serde_json::json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "probe-agent-map",
            "title": "Map the gossip planner",
            "rawInput": {
                "description": "Map the gossip planner",
                "subagent_type": "Explore",
                "prompt": "In the Rust workspace, map what backlog item 22 needs: how \
                           ranges split and merge, what the planner reads, and where a \
                           load input would enter. Report with file:line references.",
            },
        }),
        child(
            "probe-agent-map-1",
            "probe-agent-map",
            "rg -n 'fn plan' crates/meta",
            "in_progress",
        ),
        done("probe-agent-map-1", "probe-agent-map"),
        child(
            "probe-agent-map-2",
            "probe-agent-map",
            "sed -n 90,170p crates/data/src/mover.rs",
            "in_progress",
        ),
        done("probe-agent-map-2", "probe-agent-map"),
        serde_json::json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "probe-agent-map",
            "_meta": { "claudeCode": { "toolName": "Agent", "toolResponse": {
                "status": "completed", "agentType": "Explore",
                "resolvedModel": "claude-opus-5-5", "totalDurationMs": 74125,
                "totalTokens": 41774, "totalToolUseCount": 11,
                "handbackReport": { "text":
                    "## Map for item 22\n\n\
                     - **Splits and merges**: `resize_ranges` at `crates/data/src/mover.rs:113`, \
                     acting only where this node is the first replica.\n\
                     - **The planner**: `plan()` at `crates/meta/src/planner.rs:22` reads the \
                     replicated state, a clock, and a settled flag — no load input.\n\
                     - **Where load would enter**: through `plan()`'s parameters or \
                     `ClusterState`.\n" },
            } } },
        }),
        serde_json::json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "probe-agent-map",
            "status": "completed",
            "content": [{ "type": "content", "content": { "type": "text",
                "text": "This agent's report was delivered to you as a message." } }],
        }),
        serde_json::json!({
            "sessionUpdate": "tool_call", "toolCallId": "probe-agent-review",
            "title": "Review the enrollment service", "kind": "think", "status": "pending",
            "name": "Agent",
            "rawInput": {
                "description": "Review the enrollment service",
                "subagent_type": "general-purpose",
                "run_in_background": true,
                "prompt": "Do a read-only security review of services/enrollment: token \
                           issuance, the device attestation path, and what an enrolled \
                           device can do before an operator approves it. Report findings \
                           most severe first, each with the file and line.",
            },
        }),
        serde_json::json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "probe-agent-review",
            "_meta": { "claudeCode": { "toolName": "Agent", "toolResponse": {
                "isAsync": true, "status": "async_launched",
                "resolvedModel": "claude-opus-5-5" } } },
        }),
        serde_json::json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "probe-agent-review",
            "status": "completed",
            "content": [{ "type": "content", "content": { "type": "text",
                "text": "Async agent launched successfully." } }],
        }),
        child(
            "probe-agent-review-1",
            "probe-agent-review",
            "git log --oneline -- services/enrollment",
            "in_progress",
        ),
        done("probe-agent-review-1", "probe-agent-review"),
        child(
            "probe-agent-review-2",
            "probe-agent-review",
            "rg -n 'attest' services/enrollment/src",
            "in_progress",
        ),
        done("probe-agent-review-2", "probe-agent-review"),
        child(
            "probe-agent-review-3",
            "probe-agent-review",
            "cargo test -p enrollment attestation",
            "in_progress",
        ),
        serde_json::json!({
            "sessionUpdate": "tool_call", "toolCallId": "probe-agent-wiki",
            "title": "Explore the wiki", "kind": "think", "status": "pending",
            "name": "Agent",
            "rawInput": { "description": "Explore the wiki", "subagent_type": "wiki-reader",
                          "prompt": "Find the enrollment runbook." },
        }),
        serde_json::json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "probe-agent-wiki",
            "status": "failed",
            "content": [{ "type": "content", "content": { "type": "text",
                "text": "Agent type 'wiki-reader' not found. Available agents: \
                         general-purpose, Explore, Plan." } }],
        }),
    ];
    values
        .into_iter()
        .map(|value| serde_json::from_value(value).expect("a probe update"))
        .collect()
}

impl ChatPane {
    /// TASTE_PROBE_CHECK only: `TASTE_PROBE_CHAT=subagents` — a turn that
    /// started three, one still running in the background — with
    /// `subagents-detail` the running one's page open, `subagents-report`
    /// the finished one's, and `subagents-empty` the tab open over a
    /// conversation that started none.
    #[doc(hidden)]
    pub(super) fn seed_subagents_for_probe(self: &Rc<Self>, variant: &str) {
        use agent_client_protocol::schema::v1::ContentChunk;
        let text = |t: &str| ContentChunk::new(ContentBlock::Text(TextContent::new(t)));
        self.render_update(SessionUpdate::UserMessageChunk(text(
            "Plan item 22, and have someone review the enrollment service in the \
             background while you do.",
        )));
        if variant != "subagents-empty" {
            for update in probe_subagent_updates() {
                self.render_update(update);
            }
        }
        self.render_update(SessionUpdate::AgentMessageChunk(text(
            "The map is in. The review is still running; I will read it when it lands.",
        )));
        self.finalize_stream();
        self.open_agents_for_probe(match variant {
            "subagents-detail" => Some(0),
            "subagents-report" => Some(-1),
            _ => None,
        });
    }
}

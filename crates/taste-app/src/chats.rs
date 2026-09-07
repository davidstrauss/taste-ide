//! The chat column: one environment's conversation, and no tab strip.
//!
//! ENVIRONMENTS.md → "Watching an environment". The environment panel is
//! the app's single top-level control, so this pane renders the *selected*
//! environment's chat the same way the file tree renders its files. There
//! is nothing to choose here, which is why there is nothing to choose
//! *with*: a tab strip would be a second environment switcher, sitting
//! beside the real one and able to disagree with it.
//!
//! Four rules, all load-bearing:
//!
//! - **One chat per environment, and the environment is the identity.**
//!   [`taste_core::state::WorkspaceState::set_chat`] enforces it in the
//!   state; a [`ChatPane`] is built for its environment and never re-aimed.
//! - **Chats are lazy, and once alive they stay alive.** A restored chat
//!   arms its session id and connects the first time its environment is
//!   selected. Selecting away never disconnects it: the pane keeps
//!   streaming into widgets nobody is looking at, and is exactly as it was
//!   when the user comes back.
//! - **An environment need not have a chat.** A human-created environment
//!   has no conversation until someone starts an agent in it, and the
//!   empty state is where that happens — the only way a chat is born by
//!   hand.
//! - **A chat the user cannot see still gets their attention.** Busy and
//!   waiting-on-permission leave the pane through [`Chats::binding_for`],
//!   which is what the environment panel's rows render.
//!
//! Below `CONSOLIDATED_MAX_WIDTH_SP` this column stops being a column: its
//! three views become three tabs in the window's one strip
//! ([`Chats::graft_faces`]). Still no tab strip of its own — the tabs are
//! of *views*, not of conversations, and there is still exactly one
//! conversation here, the selected environment's.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;
use taste_core::environment::EnvironmentId;
use taste_core::event::DevcontainerStateEvent;
use taste_core::state::ChatEntry;
use taste_core::Workspace;
use taste_devcontainer::EnvironmentRegistry;

use crate::backlog::PRIMARY_TITLE;
use crate::chat::{BusyHook, ChatPane, PersistHook};

/// The stack page the empty state lives on. Environments name their own
/// pages by slug, and a slug can never be this (it has no dot).
const EMPTY_PAGE: &str = "no.chat";

/// How the utilization tint reaches whoever is drawing the tab's glyph.
type UsageSeverityHook = Rc<dyn Fn(&str, &str)>;

struct Chat {
    env: EnvironmentId,
    pane: Rc<ChatPane>,
}

pub struct Chats {
    pub widget: crate::chat_column::ChatColumn,
    stack: gtk::Stack,
    /// The "no agent here yet" page, retitled per environment. One widget,
    /// because it says the same thing about whichever environment has no
    /// conversation.
    empty: adw::StatusPage,
    start_button: gtk::Button,
    workspace: Workspace,
    /// The workspace's environments: what a chat's agent is aimed at, and
    /// what says whether an environment still exists.
    environments: Arc<EnvironmentRegistry>,
    /// The IDE binary's path; each pane composes its own bridge command
    /// around its own environment's socket.
    bridge_command: String,
    chats: RefCell<Vec<Chat>>,
    /// The subscription pool, as last handed down. Kept here so a pane
    /// built later starts out knowing it.
    pool: RefCell<crate::fleet::PoolFacts>,
    /// The environment on screen. One selection, owned by the window and
    /// handed down — never a second copy this pane could drift from.
    current: RefCell<EnvironmentId>,
    /// Off until [`Chats::start`]: while restoring (and forever, in a probe
    /// instance) chats neither connect nor persist.
    live: Cell<bool>,
    /// How the column tells the MCP server which environment's socket
    /// serves the orchestration tools. The column is the authority on the
    /// role (it is one per workspace, and only something that can see every
    /// chat can move it); the server is the authority on the tools.
    /// "Something a panel row renders has changed" — a turn starting or
    /// ending, a permission request arriving. The rows are assembled
    /// elsewhere; this asks for that to happen again.
    on_activity: RefCell<Option<Rc<dyn Fn()>>>,
    /// Where the selected chat's utilization and settings faces go while
    /// the window is consolidated and they are tabs rather than shades.
    ///
    /// Slots rather than the widgets themselves, because the tabs outlive
    /// the selection: an `AdwTabPage`'s child is fixed for the page's life,
    /// and which conversation's figures belong in it is not.
    usage_slot: adw::Bin,
    settings_slot: adw::Bin,
    /// The slots as the editor's strip sees them: each wrapped in a
    /// `chat_column::ChatColumn`, so a tab view that measures every page
    /// measures the chat's stated width there and not whatever a settings
    /// shade or a usage face happens to contain (`AdwTabView` takes its
    /// minimum from its widest page, and the consolidated rung's floor is
    /// computed from it).
    usage_face: crate::chat_column::ChatColumn,
    settings_face: crate::chat_column::ChatColumn,
    /// Whose faces are in those slots, so a selection change can put them
    /// back where they came from.
    grafted_env: RefCell<Option<EnvironmentId>>,
    grafted: Cell<bool>,
    /// How the column asks for the utilization tab's glyph to be re-tinted.
    on_usage_severity: RefCell<Option<UsageSeverityHook>>,
    /// The results listing at the column's foot (results.rs): the hits in
    /// the conversation on screen. Every other conversation's count goes to
    /// its row in the backlog through `on_inner_hits`.
    results: Rc<crate::results::ResultsPanel>,
    search: RefCell<Option<std::rc::Weak<crate::search::Search>>>,
    on_inner_hits: RefCell<Option<Box<dyn Fn(HashMap<String, usize>)>>>,
}

/// The three views this column hands over when it stops being a column.
pub struct ChatFaces {
    pub chat: gtk::Widget,
    pub usage: gtk::Widget,
    pub settings: gtk::Widget,
}

impl Chats {
    pub fn new(
        workspace: Workspace,
        environments: Arc<EnvironmentRegistry>,
        bridge_command: String,
    ) -> Rc<Self> {
        // The empty state is a real invitation, not an apology: one line
        // saying what this environment is, and one button that starts the
        // conversation. It is the ONLY way a chat is created by hand, so it
        // carries the weight the "new tab" button used to.
        let empty = adw::StatusPage::builder()
            // An illustration, not a symbolic glyph blown up to 110px:
            // the editor's empty page sets the house style, and this is
            // its sibling — the same neutral furniture, the same carrot,
            // waiting rather than sad, because this page is an invitation.
            .icon_name("taste-no-agent")
            .title("No Agent Here Yet")
            .vexpand(true)
            .build();
        let start_button = gtk::Button::builder()
            .label("Start an Agent")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        empty.set_child(Some(&start_button));

        let stack = gtk::Stack::builder()
            .vexpand(true)
            // Panes are kept, never destroyed: a chat in another
            // environment goes on streaming into widgets nobody is looking
            // at, and comes back mid-sentence.
            .transition_type(gtk::StackTransitionType::None)
            // Only the conversation on screen is measured. A homogeneous
            // stack sizes itself to its widest page, so a card in a chat
            // nobody was looking at (measured: a 1201px unbreakable token
            // in a tool call) set this pane's minimum and clipped the
            // visible conversation's composer at the column's edge.
            .hhomogeneous(false)
            .vhomogeneous(false)
            .build();
        stack.add_named(&empty, Some(EMPTY_PAGE));

        // The column's outer edge — where it is a paned child at full width
        // and a tab page below the breakpoint. Both of those measure it for
        // the height they are about to give it, so this is exactly where
        // "how tall am I" must stop being able to change "how wide do I
        // need to be": see `chat_column`.
        // The results listing sits under the conversation, at the foot of
        // the column — the intervention-panel shape every document pane
        // uses (SEARCH.md rule 2) — and travels with the column when the
        // narrow rung grafts it into the editor's strip.
        let results = crate::results::ResultsPanel::new();
        let column_body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column_body.append(&stack);
        column_body.append(&results.widget);
        let widget = crate::chat_column::ChatColumn::new(&column_body);
        let usage_slot = adw::Bin::new();
        let settings_slot = adw::Bin::new();
        let usage_face = crate::chat_column::ChatColumn::new(&usage_slot);
        let settings_face = crate::chat_column::ChatColumn::new(&settings_slot);

        let chats = Rc::new(Self {
            widget,
            stack,
            empty,
            start_button: start_button.clone(),
            workspace,
            environments,
            bridge_command,
            chats: RefCell::new(Vec::new()),
            pool: RefCell::new(crate::fleet::PoolFacts::default()),
            current: RefCell::new(EnvironmentId::primary()),
            live: Cell::new(false),
            on_activity: RefCell::new(None),
            usage_slot,
            settings_slot,
            usage_face,
            settings_face,
            grafted_env: RefCell::new(None),
            grafted: Cell::new(false),
            on_usage_severity: RefCell::new(None),
            results,
            search: RefCell::new(None),
            on_inner_hits: RefCell::new(None),
        });

        {
            let weak = Rc::downgrade(&chats);
            start_button.connect_clicked(move |_| {
                let Some(chats) = weak.upgrade() else { return };
                let env = chats.current.borrow().clone();
                chats.start_agent_in(&env);
            });
        }

        chats.show_current();
        chats
    }

    // --- the one selection ------------------------------------------------

    /// Render this environment's chat. The window's single selection
    /// arrives here; nothing in this pane ever decides it.
    ///
    /// Cheap by construction: the pane already exists (or does not), so a
    /// switch is a stack page change and a first-activation, never a
    /// rebuild.
    pub fn show(self: &Rc<Self>, env: &EnvironmentId) {
        if *self.current.borrow() == *env {
            return;
        }
        *self.current.borrow_mut() = env.clone();
        self.show_current();
    }

    /// Answer the one query (SEARCH.md): the conversation on screen lists
    /// its hits under the transcript; every conversation's count goes to
    /// the backlog row of its environment. Transcripts are rows on screen,
    /// so this is a walk, not a job: it answers before the next frame.
    pub fn attach_search(
        self: &Rc<Self>,
        search: &Rc<crate::search::Search>,
        on_inner_hits: impl Fn(HashMap<String, usize>) + 'static,
    ) {
        *self.search.borrow_mut() = Some(Rc::downgrade(search));
        *self.on_inner_hits.borrow_mut() = Some(Box::new(on_inner_hits));
        {
            let weak = Rc::downgrade(self);
            search.subscribe("chats", move |query, _| {
                if let Some(chats) = weak.upgrade() {
                    chats.answer_search(query);
                }
            });
        }
        {
            let weak = Rc::downgrade(self);
            search.register_stepper(crate::search::Panel::Chat, move |step| {
                weak.upgrade().is_some_and(|chats| chats.results.step(step))
            });
        }
        {
            let search = Rc::downgrade(search);
            self.results.set_on_close(move || {
                if let Some(search) = search.upgrade() {
                    search.set_panel_hits(crate::search::Panel::Chat, 0);
                }
            });
        }
        // Selecting a hit — a step, a click — shows it in the conversation;
        // activating does the same, since there is nothing further to do.
        {
            let weak = Rc::downgrade(self);
            let reveal: Rc<dyn Fn(&crate::results::Target)> = Rc::new(move |target| {
                let Some(chats) = weak.upgrade() else { return };
                if let crate::results::Target::Transcript { row } = target {
                    if let Some(pane) = chats.selected() {
                        pane.scroll_to_transcript_row(*row);
                    }
                }
            });
            let on_select = reveal.clone();
            self.results.set_on_select(move |target| on_select(target));
            self.results.set_on_activate(move |target| reveal(target));
        }
    }

    /// Hits in every conversation — the caller's environment's, or
    /// everyone's — for `ide_find`.
    pub fn find_in_transcripts(
        &self,
        query: &crate::search::Query,
        scope: &taste_core::orchestration::FindScope,
    ) -> Vec<taste_core::orchestration::ChatHit> {
        let mut hits = Vec::new();
        for chat in self.chats.borrow().iter() {
            if let taste_core::orchestration::FindScope::Environment(wanted) = scope {
                if *wanted != chat.env {
                    continue;
                }
            }
            let (_, found) = chat.pane.search_transcript(query, 40);
            for hit in found {
                hits.push(taste_core::orchestration::ChatHit {
                    env: chat.env.clone(),
                    row: hit.row,
                    text: hit.text,
                });
            }
        }
        hits
    }

    /// The next hit in the conversation on screen, wrapping — a click on an
    /// environment's row that already has the panes.
    pub fn step_results(&self) -> bool {
        self.results.step_cycle()
    }

    fn answer_search(self: &Rc<Self>, query: &crate::search::Query) {
        use crate::results::{safe_markup, Group, Item, Target};
        let search = self.search.borrow().as_ref().and_then(|s| s.upgrade());
        if query.is_empty() {
            self.results.hide();
            if let Some(search) = &search {
                search.report("chats", crate::search::Status::default());
                search.set_panel_hits(crate::search::Panel::Chat, 0);
            }
            if let Some(hook) = self.on_inner_hits.borrow().as_ref() {
                hook(HashMap::new());
            }
            return;
        }
        let current = self.current.borrow().clone();
        let mut inner: HashMap<String, usize> = HashMap::new();
        let mut everywhere = 0;
        let mut on_screen = 0;
        let mut items: Vec<Item> = Vec::new();
        for chat in self.chats.borrow().iter() {
            let listed = chat.env == current;
            // Only the conversation on screen is walked widget by widget
            // (its rows are the hits' addresses); the others answer from
            // their text mirror, which is the difference between a
            // keystroke and a frame across a fleet of chats.
            let (count, hits) = if listed {
                chat.pane.search_transcript(query, 200)
            } else {
                (chat.pane.count_in_transcript(query), Vec::new())
            };
            everywhere += count;
            if count > 0 {
                inner.insert(chat.env.as_str().to_string(), count);
            }
            if listed {
                on_screen = count;
                items = hits
                    .into_iter()
                    .map(|hit| Item {
                        primary: safe_markup(&query.highlight_markup(&hit.text), &hit.text),
                        secondary: format!("row {}", hit.row + 1),
                        target: Target::Transcript { row: hit.row },
                    })
                    .collect();
            }
        }
        let subject = if current.is_primary() {
            "your conversation".to_string()
        } else {
            format!("{current}'s conversation")
        };
        self.results.show(
            query,
            &subject,
            vec![Group {
                title: String::new(),
                items,
            }],
            false,
            1,
            1,
        );
        if let Some(search) = &search {
            search.set_panel_hits(crate::search::Panel::Chat, on_screen);
            let total = self.chats.borrow().len();
            search.report(
                "chats",
                crate::search::Status {
                    hits: everywhere,
                    done: total,
                    total,
                    running: false,
                },
            );
        }
        if let Some(hook) = self.on_inner_hits.borrow().as_ref() {
            hook(inner);
        }
    }

    fn show_current(self: &Rc<Self>) {
        let env = self.current.borrow().clone();
        // The utilization and settings tabs are the SELECTED
        // conversation's, so a selection change moves them: the previous
        // pane gets its shades back and this one hands its own over. One
        // reparent each, and no widget is built or destroyed.
        if self.grafted.get() && self.grafted_env.borrow().as_ref() != Some(&env) {
            self.empty_slots();
            self.fill_slots();
        }
        let pane = self.pane_for(&env);
        // Only the chat on screen may raise window-level toasts, whose
        // actions route back to it.
        for chat in self.chats.borrow().iter() {
            chat.pane.set_selected(chat.env == env);
        }
        // The listing is the conversation on screen's: a new one answers
        // the standing query afresh.
        let standing = self
            .search
            .borrow()
            .as_ref()
            .and_then(|search| search.upgrade())
            .map(|search| search.query());
        if let Some(query) = standing.filter(|query| !query.is_empty()) {
            self.answer_search(&query);
        }
        match pane {
            Some(pane) => {
                self.stack.set_visible_child(&pane.widget);
                // What a send will do here is a property of the
                // environment, not of anything that just happened, so it
                // has to be true the moment the pane is looked at rather
                // than only after the next lifecycle event.
                pane.refresh_environment_state();
                if !self.live.get() {
                    return;
                }
                // First arrival is what spawns this environment's agent: a
                // remembered conversation comes back here, through the same
                // lazy `ensure_client` a single chat has always used.
                pane.activate();
                // Deferred: the page is still being mapped, and grabbing
                // focus into a widget that is not on screen yet silently
                // does nothing.
                glib::idle_add_local_once(move || pane.focus_composer());
            }
            None => {
                self.dress_empty(&env);
                self.stack.set_visible_child_name(EMPTY_PAGE);
            }
        }
    }

    // --- when the column stops being a column ------------------------------

    /// Hand this pane's three views over as tabs.
    ///
    /// ENVIRONMENTS.md → the responsive ladder. Below
    /// `CONSOLIDATED_MAX_WIDTH_SP` the window has one strip and no chat
    /// column: the conversation, its utilization and the agent's settings
    /// become three tabs at the end of it. The toggle strip that switched
    /// between them inside this pane hides — a row of tab-shaped controls
    /// inside a tab is the nested tab set the rung exists to abolish.
    ///
    /// The chat itself is the same widget the column was, so switching
    /// environments keeps working untouched. The other two are the
    /// *selected* conversation's, lifted out of its overlay, and they
    /// follow the selection through these slots.
    pub fn graft_faces(self: &Rc<Self>) -> ChatFaces {
        self.grafted.set(true);
        self.fill_slots();
        ChatFaces {
            chat: self.widget.clone().upcast(),
            usage: self.usage_face.clone().upcast(),
            settings: self.settings_face.clone().upcast(),
        }
    }

    /// The exact inverse: the faces go back into the selected pane's
    /// overlay and this is a column again. The caller has already taken the
    /// three widgets out of their tabs.
    pub fn ungraft_faces(self: &Rc<Self>) {
        self.grafted.set(false);
        self.empty_slots();
    }

    /// Put the selected conversation's faces in the slots.
    fn fill_slots(self: &Rc<Self>) {
        let env = self.current.borrow().clone();
        let Some(pane) = self.pane_for(&env) else {
            // No conversation here, so no utilization and no session to
            // configure. The tabs say that rather than showing a void:
            // the invitation to start one is on the chat tab, where it
            // belongs.
            self.usage_slot.set_child(Some(&nothing_here(
                "Utilization is measured per conversation, and this \
                 environment has none yet.",
            )));
            self.settings_slot.set_child(Some(&nothing_here(
                "Session settings belong to a conversation, and this \
                 environment has none yet.",
            )));
            return;
        };
        let (usage, settings) = pane.take_faces();
        self.usage_slot.set_child(Some(&usage));
        self.settings_slot.set_child(Some(&settings));
        *self.grafted_env.borrow_mut() = Some(env);
        // The tint the toggle wore has nowhere to live now but the tab's
        // own icon, so ask the pane to say it again.
        pane.refresh_usage_badge();
    }

    /// Say the utilization tint again.
    ///
    /// The tab that wears it is created by the editor *after* the faces
    /// move, so the icon the graft passes is a placeholder until the pane
    /// that knows the answer is asked for it — a conversation with no room
    /// left would otherwise wear a green glyph until its next usage
    /// update, which could be the end of the next turn.
    pub fn republish_usage_severity(&self) {
        let env = self.grafted_env.borrow().clone();
        if let Some(pane) = env.and_then(|env| self.pane_for(&env)) {
            pane.refresh_usage_badge();
        }
    }

    /// Take them out and give them back to whoever they belong to.
    fn empty_slots(self: &Rc<Self>) {
        let previous = self.grafted_env.borrow_mut().take();
        self.usage_slot.set_child(gtk::Widget::NONE);
        self.settings_slot.set_child(gtk::Widget::NONE);
        if let Some(pane) = previous.and_then(|env| self.pane_for(&env)) {
            pane.restore_faces();
        }
    }

    /// What the empty state says about the environment that has no chat.
    fn dress_empty(&self, env: &EnvironmentId) {
        let (title, description) = empty_state(env, self.environments.get(env).is_some());
        self.empty.set_title(&title);
        self.empty.set_description(Some(&description));
        // An environment that no longer exists cannot be given an agent.
        self.start_button
            .set_sensitive(self.environments.get(env).is_some());
    }

    // --- the chats --------------------------------------------------------

    pub fn pane_for(&self, env: &EnvironmentId) -> Option<Rc<ChatPane>> {
        self.chats
            .borrow()
            .iter()
            .find(|chat| chat.env == *env)
            .map(|chat| chat.pane.clone())
    }

    /// The chat on screen, if the selected environment has one.
    pub fn selected(&self) -> Option<Rc<ChatPane>> {
        self.pane_for(&self.current.borrow())
    }

    /// Start an agent in an environment that has none, and show it.
    ///
    /// This is the whole of "creating a chat" now: there is no new-chat
    /// gesture, because a conversation is not a thing you can have two of
    /// in one world. Making another chat means making another environment,
    /// which is the panel's own New Environment.
    pub fn start_agent_in(self: &Rc<Self>, env: &EnvironmentId) -> Option<Rc<ChatPane>> {
        self.environments.get(env)?;
        // A new conversation starts configured like the one the user was
        // just in: same agent, same model, same permission mode. Settings
        // used to travel with "new tab beside this one", and the reason
        // survives the tab strip — a person who chose an agent for this
        // workspace meant it for the workspace.
        let inherit = self.selected();
        let fresh = self.pane_for(env).is_none();
        let pane = self.ensure_pane(env);
        if fresh {
            if let Some(previous) = inherit {
                if !Rc::ptr_eq(&previous, &pane) {
                    pane.inherit_settings(&previous);
                }
            }
        }
        if *self.current.borrow() == *env {
            self.stack.set_visible_child(&pane.widget);
            pane.set_selected(true);
            if self.live.get() {
                pane.activate();
                let pane = pane.clone();
                glib::idle_add_local_once(move || pane.focus_composer());
            }
        }
        self.persist();
        Some(pane)
    }

    /// Who to tell when the utilization tint moves — set on every pane,
    /// because any of them can be the selected one, and acted on only for
    /// the one whose faces are in the slots.
    pub fn set_on_usage_severity(&self, hook: impl Fn(&str, &str) + 'static) {
        *self.on_usage_severity.borrow_mut() = Some(Rc::new(hook));
    }

    /// This environment's pane, building it if this is the first time
    /// anyone has wanted a conversation here.
    fn ensure_pane(self: &Rc<Self>, env: &EnvironmentId) -> Rc<ChatPane> {
        if let Some(pane) = self.pane_for(env) {
            return pane;
        }
        let pane = ChatPane::new(
            self.workspace.clone(),
            self.environments.clone(),
            self.bridge_command.clone(),
            env.clone(),
        );
        self.stack.add_named(&pane.widget, Some(env.as_str()));
        {
            let weak = Rc::downgrade(self);
            let persist: PersistHook = Rc::new(move || {
                if let Some(chats) = weak.upgrade() {
                    chats.persist();
                }
            });
            // A chat in an environment nobody is looking at reports its
            // work on that environment's row in the panel, which is the
            // whole of how a hidden conversation stays visible.
            let weak = Rc::downgrade(self);
            let busy: BusyHook = Rc::new(move |_| {
                if let Some(chats) = weak.upgrade() {
                    chats.note_activity();
                }
            });
            pane.set_hooks(persist, busy);
            // The utilization tint, forwarded only for the conversation
            // whose faces are actually in the tabs: every pane can report,
            // and one of them is on screen.
            {
                let weak = Rc::downgrade(self);
                let env = env.clone();
                pane.set_on_usage_severity(move |icon, tooltip| {
                    let Some(chats) = weak.upgrade() else { return };
                    if chats.grafted_env.borrow().as_ref() != Some(&env) {
                        return;
                    }
                    let hook = chats.on_usage_severity.borrow().clone();
                    if let Some(hook) = hook {
                        hook(icon, tooltip);
                    }
                });
            }
        }
        // A pane built after the last observation still knows what the
        // pool looked like: the snapshot is workspace-global, so a new
        // conversation has no reason to start out blank about it.
        pane.set_pool(&self.pool.borrow());
        self.chats.borrow_mut().push(Chat {
            env: env.clone(),
            pane: pane.clone(),
        });
        pane
    }

    /// The subscription pool, to every conversation at once.
    ///
    /// Every pane, not just the visible one: the utilization tab of a
    /// chat the user switches to must not be a second behind, and the
    /// figures are the same for all of them because the subscription is.
    pub fn set_pool(&self, pool: &crate::fleet::PoolFacts) {
        *self.pool.borrow_mut() = pool.clone();
        for chat in self.chats.borrow().iter() {
            chat.pane.set_pool(pool);
        }
    }

    /// Is the account's allowance exhausted right now — the API refusing
    /// turns? `Some` carries when it reopens, in words. Everything the IDE
    /// would start on its own (waking the coordinator, `issue_start`,
    /// `chat_send`) asks this first and stops, because continuing to spend
    /// an exhausted allowance is the user's decision (David, 2026-09-06:
    /// "Require user intervention to continue running if session
    /// allowances are exhausted"). The user's own prompts are not gated:
    /// typing one IS the intervention.
    pub fn allowance_exhausted(&self) -> Option<String> {
        let now = std::time::SystemTime::now();
        let pool = self.pool.borrow();
        pool.quota.current_exhaustion(now).map(|refusal| {
            refusal
                .until
                .and_then(|until| until.duration_since(now).ok())
                .map(taste_core::quota::describe_countdown)
                .unwrap_or_else(|| "at a time the API did not state".into())
        })
    }

    /// An environment was destroyed: its conversation goes with it. There
    /// is nowhere else for a chat to live, and a pane aimed at a clone that
    /// has been deleted is a pane whose every action fails.
    pub fn forget_environment(self: &Rc<Self>, env: &EnvironmentId) {
        let index = self.chats.borrow().iter().position(|chat| chat.env == *env);
        let Some(index) = index else { return };
        let chat = self.chats.borrow_mut().remove(index);
        chat.pane.close();
        self.stack.remove(&chat.pane.widget);
        if *self.current.borrow() == *env {
            self.show_current();
        }
        self.persist();
    }

    /// Bring the strip to life from persisted state: one armed chat per
    /// remembered environment, none of them connected.
    ///
    /// Nothing is activated here. The selected environment's chat connects
    /// through [`Chats::show_current`], which is the same path every later
    /// selection takes.
    pub fn start(self: &Rc<Self>, chats: &[ChatEntry]) {
        for entry in chats {
            let pane = self.ensure_pane(&entry.environment);
            pane.arm_from_entry(entry);
        }
        self.live.set(true);
        self.show_current();
        self.persist();
    }

    /// Every chat as restorable state, one per environment.
    pub fn snapshot(&self) -> Vec<ChatEntry> {
        self.chats
            .borrow()
            .iter()
            .map(|chat| chat.pane.chat_entry())
            .collect()
    }

    // --- what leaves the pane ---------------------------------------------

    /// Which chat works in this environment, as the environment panel and
    /// the fleet view render it.
    ///
    /// One chat or none — the invariant this whole pane is built on — so
    /// there is no "+2 more" to count any more, and no ambiguity about
    /// which conversation a row's spinner belongs to.
    pub fn binding_for(&self, env: &EnvironmentId) -> Option<crate::fleet::ChatBinding> {
        let chats = self.chats.borrow();
        let chat = chats.iter().find(|chat| chat.env == *env)?;
        Some(crate::fleet::ChatBinding {
            label: chat.pane.agent_name(),
            busy: chat.pane.is_busy(),
            // One chat, so no "any of them": this environment wants the
            // user exactly when its chat does.
            awaits_user: chat.pane.awaits_user(),
            orchestrator: chat.pane.is_orchestrator(),
        })
    }

    /// Every environment with a chat in it — the addressable chats, as an
    /// unknown-id refusal names them.
    pub fn bound_environments(&self) -> Vec<EnvironmentId> {
        self.chats
            .borrow()
            .iter()
            .map(|chat| chat.env.clone())
            .collect()
    }

    /// Which environment's chat answers to this notification key.
    ///
    /// A notification click is a request to go somewhere, and where is an
    /// environment — so the window routes it through the same transition
    /// every other way of arriving takes.
    pub fn environment_for_key(&self, key: &str) -> Option<EnvironmentId> {
        self.chats
            .borrow()
            .iter()
            .find(|chat| chat.pane.answers_to(key))
            .map(|chat| chat.env.clone())
    }

    /// TASTE_PROBE_CHECK only: a chat in this environment, without a clone
    /// or an agent. `live` stays off, so it renders and never connects.
    #[doc(hidden)]
    pub fn seed_for_probe(self: &Rc<Self>, slug: &str) {
        let Ok(env) = EnvironmentId::parse(slug) else {
            return;
        };
        *self.current.borrow_mut() = env.clone();
        let pane = self.ensure_pane(&env);
        pane.set_selected(true);
        self.stack.set_visible_child(&pane.widget);
    }

    /// The user came back to the window: every chat retires the
    /// notifications that were only telling them to.
    pub fn withdraw_informational(&self) {
        for chat in self.chats.borrow().iter() {
            chat.pane.withdraw_informational();
        }
    }

    /// An environment's container changed state: tell its chat, so the
    /// agent can move into or out of that container.
    ///
    /// A chat nobody is looking at gets this too. One whose environment
    /// came up should be running beside its files by the time the user
    /// selects it — and one whose container went away is exactly the one
    /// that would otherwise sit dead unnoticed.
    pub fn on_environment_state(&self, env: &EnvironmentId, state: &DevcontainerStateEvent) {
        if let Some(pane) = self.pane_for(env) {
            pane.on_environment_state(state);
        }
    }

    /// Something changed about a chat that the environment panel renders
    /// (busy, waiting on the user). The rows are assembled by the console
    /// from this pane's own answers, so all this has to do is ask for a
    /// re-render.
    fn note_activity(&self) {
        if let Some(hook) = self.on_activity.borrow().as_ref() {
            hook();
        }
    }

    /// How the column asks for the fleet rows to be re-assembled.
    pub fn set_on_activity(&self, hook: impl Fn() + 'static) {
        *self.on_activity.borrow_mut() = Some(Rc::new(hook));
    }

    // --- orchestration ----------------------------------------------------

    /// `chat_create`: an environment's first conversation, live and ready
    /// to be prompted.
    ///
    /// Everything the orchestrator asked for is applied *before* the agent
    /// spawns (the agent id and the model both belong to the session that
    /// is about to start), and the answer waits for that session to reach
    /// Ready — because until it does, the model it advertises is unknown
    /// and "the model you asked for does not exist" cannot be said
    /// honestly.
    ///
    /// The chat is created in the background. Stealing the user's selection
    /// because an agent delegated something would take the window away from
    /// whatever they were reading; the environment is in the panel, with
    /// its own spinner, whenever they want it.
    pub fn create_orchestrated(
        self: &Rc<Self>,
        env: EnvironmentId,
        agent: Option<String>,
        model: Option<String>,
        done: Box<dyn FnOnce(Result<taste_core::orchestration::CreatedChat, String>)>,
    ) {
        let Some(pane) = self.start_agent_in(&env) else {
            done(Err(format!("the environment {env} does not exist")));
            return;
        };
        if let Some(agent) = &agent {
            if !pane.set_agent_id(agent) {
                let known: Vec<String> = taste_acp::builtin_agents()
                    .iter()
                    .map(|spec| spec.id.clone())
                    .collect();
                self.forget_environment(&env);
                done(Err(format!(
                    "no agent {agent:?} — this IDE ships {known:?}. Nothing was created."
                )));
                return;
            }
        }
        pane.set_model_value(model.clone());
        // A chat created for an orchestrator is prompted immediately, so it
        // connects now rather than waiting for its environment to be
        // selected — the laziness is for conversations a person restored,
        // not for one that has a task coming.
        pane.activate();
        self.persist();
        pane.on_ready_once(Box::new(move |pane_at_ready| {
            let advertised = pane_at_ready.advertised_models();
            if let Some(wanted) = &model {
                let known = advertised.iter().any(|(value, _)| value == wanted);
                if !known {
                    let ids: Vec<&str> =
                        advertised.iter().map(|(value, _)| value.as_str()).collect();
                    // The chat stays: it exists, it is in its environment,
                    // and it has been told nothing. Destroying an
                    // environment over a mistyped model would be a larger
                    // surprise than an idle chat the user can see.
                    pane_at_ready.set_model_value(None);
                    done(Err(format!(
                        "{} does not offer a model {wanted:?} — it advertises {ids:?}. \
                         The chat {env} was created and is idle: it was NOT given the \
                         task. Destroy it from the environment panel, or dispatch to it \
                         with chat_send.",
                        pane_at_ready.agent_name()
                    )));
                    return;
                }
            }
            done(Ok(taste_core::orchestration::CreatedChat {
                chat: env,
                agent: pane_at_ready.agent_id(),
                model,
                note: "Its container is NOT running — a fresh environment starts in \
                       safe mode, so this agent can read, think and write but has no \
                       shell until the user starts it."
                    .to_string(),
            }));
        }));
    }

    /// Write the chats to workspace state. Called whenever one gains or
    /// loses a session, changes agent, model or permission mode, or the set
    /// of them changes — waiting for window close is how stale session ids
    /// survive an unclean exit.
    fn persist(&self) {
        if !self.live.get() {
            return;
        }
        let chats = self.snapshot();
        let root = self.workspace.root().to_path_buf();
        // Never on the GTK thread: this reads and writes a file.
        crate::runtime::runtime().spawn_blocking(move || {
            let mut state = taste_core::state::load(&root);
            state.root = root.clone();
            state.set_chats(chats);
            if let Err(e) = taste_core::state::save(&root, &state) {
                tracing::warn!("saving chats failed: {e:#}");
            }
        });
    }
}

/// What the chat column says about an environment with no conversation in
/// it — title and description, as the empty page renders them.
///
/// Pure, and tested: this text is the entire affordance for creating a
/// chat, so what it says about the user's own checkout versus an agent
/// environment is worth pinning down.
/// A tab with nothing in it yet, said in a sentence rather than left blank.
///
/// Utilization and session settings belong to a *conversation*; an
/// environment nobody has started an agent in has neither, and at the
/// consolidated rung those are tabs that exist whether or not there is
/// anything behind them. An empty tab reads as a bug; this reads as an
/// answer.
fn nothing_here(text: &str) -> gtk::Widget {
    adw::StatusPage::builder()
        .icon_name("taste-no-agent")
        .title("No Agent Here Yet")
        .description(text)
        .vexpand(true)
        .build()
        .upcast()
}

pub fn empty_state(env: &EnvironmentId, exists: bool) -> (String, String) {
    if !exists {
        return (
            "Environment Gone".to_string(),
            format!("{env} no longer exists. Pick another one from the panel below the files."),
        );
    }
    if env.is_primary() {
        return (
            "No Agent Here Yet".to_string(),
            format!(
                "Start one and it works in {PRIMARY_TITLE} — your own checkout, \
                 the files you are looking at."
            ),
        );
    }
    (
        "No Agent Here Yet".to_string(),
        format!(
            "Start one and it works in {env} — that environment's own clone \
             of the workspace, with its own devcontainer. Your files are \
             untouched."
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(slug: &str) -> EnvironmentId {
        EnvironmentId::parse(slug).unwrap()
    }

    /// The empty state names the world the agent would work in, because
    /// "start an agent" means something different in the user's own
    /// checkout than it does in a clone.
    #[test]
    fn the_empty_state_names_the_world_the_agent_would_work_in() {
        let (title, description) = empty_state(&EnvironmentId::primary(), true);
        assert_eq!(title, "No Agent Here Yet");
        assert!(description.contains(PRIMARY_TITLE), "{description}");
        assert!(description.contains("your own checkout"), "{description}");

        let (_, description) = empty_state(&env("calm-1"), true);
        assert!(description.contains("calm-1"), "{description}");
        assert!(description.contains("own clone"), "{description}");
        assert!(
            description.contains("Your files are untouched"),
            "an agent environment says what it will NOT touch: {description}"
        );
    }

    /// An environment destroyed under the view says so, and does not offer
    /// to start an agent in a clone that is gone.
    #[test]
    fn a_destroyed_environment_offers_nothing() {
        let (title, description) = empty_state(&env("calm-1"), false);
        assert_eq!(title, "Environment Gone");
        assert!(description.contains("no longer exists"), "{description}");
    }
}

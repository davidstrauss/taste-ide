//! The chat pane's tab strip: N chats in one pane.
//!
//! A tab IS a [`ChatPane`] — its session, transcript, composer and session
//! settings travel together, because they are all facets of one
//! conversation. This type owns the strip and nothing else: which chat is
//! selected, opening and closing them, and writing the list of them to
//! workspace state. Everything about *talking to an agent* stays in
//! `chat.rs`.
//!
//! Three rules, all load-bearing:
//!
//! - **Tabs are lazy.** A restored tab arms its session id and connects on
//!   first selection, so five remembered chats cost five labels, not five
//!   agent processes.
//! - **The window addresses the SELECTED pane.** Sign-in completion, the
//!   destroy-session toast, commit-message suggestions and the ui_probe
//!   "chat" target all resolve through [`ChatTabs::selected`], so a
//!   background chat can never be answered on the user's behalf.
//! - **The strip owns environment creation.** A chat asks for a world of
//!   its own; the strip names it (it holds the ordinals a readable slug is
//!   built from), clones it off the main thread, and re-aims the chat's
//!   agent at it. Binding is one-way here: closing a tab does not destroy
//!   its environment, and there is no unbind — the clone is where that
//!   agent's work lives, and both of those would be ways to lose it.
//!   Lifecycle for environments arrives with the fleet view (phase 5).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;
use taste_core::environment::EnvironmentId;
use taste_core::event::DevcontainerStateEvent;
use taste_core::state::ChatEntry;
use taste_core::Workspace;
use taste_devcontainer::EnvironmentRegistry;

use crate::chat::{BusyHook, ChatPane, EnvironmentHook, OpenEnvironmentHook, PersistHook};

/// Slug vocabulary for generated environment ids.
///
/// An environment id lands in container names, volume names, socket
/// filenames and a directory path, and the user reads it in a tab label
/// and (from phase 5) a fleet view. `env-3` would be all of correct,
/// unique and unmemorable; `brisk-3` is the same id a person can say out
/// loud. The ordinal keeps it unique and ties it to the tab it came from.
const ENVIRONMENT_ADJECTIVES: [&str; 12] = [
    "brisk", "calm", "clever", "eager", "keen", "lucid", "nimble", "plucky", "quiet", "spry",
    "steady", "wry",
];

struct Tab {
    page: adw::TabPage,
    pane: Rc<ChatPane>,
    /// Stable, never reused: the tab's number stays put while its
    /// neighbours come and go.
    ordinal: u32,
}

pub struct ChatTabs {
    pub widget: gtk::Box,
    view: adw::TabView,
    workspace: Workspace,
    /// The workspace's environments: where a chat's own world comes from,
    /// and what every pane consults for its environment's mode.
    environments: Arc<EnvironmentRegistry>,
    /// The IDE binary's path; each pane composes its own bridge command
    /// around its own environment's socket.
    bridge_command: String,
    tabs: RefCell<Vec<Tab>>,
    next_ordinal: Cell<u32>,
    /// Off until [`ChatTabs::start`]: while restoring (and forever, in a
    /// probe instance) tabs neither connect nor persist.
    live: Cell<bool>,
    /// How a chat asks the window to aim the panes at its environment.
    on_open_environment: RefCell<Option<OpenEnvironmentHook>>,
    /// How the strip tells the MCP server which environment's socket
    /// serves the orchestration tools. The strip is the authority on the
    /// role (it is one per workspace, and only something that can see
    /// every tab can move it); the server is the authority on the tools.
    on_orchestrator_changed: RefCell<Option<OrchestratorHook>>,
}

/// How the strip tells the MCP server where the orchestration tools go.
type OrchestratorHook = Rc<dyn Fn(Option<EnvironmentId>)>;

/// The tab glyph for the orchestrator.
///
/// An `AdwTabPage` indicator, not a colour or a badge: tabs here are
/// natural-width, so anything that changes a tab's *size* makes the strip
/// jump when a role moves. The indicator sits in the slot libadwaita keeps
/// for exactly this, beside the title, and it is a role marker rather than
/// a status — quiet on purpose.
const ORCHESTRATOR_ICON: &str = "system-users-symbolic";

impl ChatTabs {
    pub fn new(
        workspace: Workspace,
        environments: Arc<EnvironmentRegistry>,
        bridge_command: String,
    ) -> Rc<Self> {
        let view = adw::TabView::new();
        // Same tab idiom as the editor and the console: natural-width tabs
        // (opening a chat must not resize the others), always-visible bar,
        // and the new-tab button at the end of the strip.
        let tab_bar = adw::TabBar::builder()
            .view(&view)
            .autohide(false)
            .expand_tabs(false)
            .build();
        let new_tab_button = gtk::Button::builder()
            .icon_name("tab-new-symbolic")
            .tooltip_text("New chat (a fresh session with the same agent)")
            .css_classes(["flat"])
            .build();
        tab_bar.set_end_action_widget(Some(&new_tab_button));

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&tab_bar);
        widget.append(&view);
        view.set_vexpand(true);

        let tabs = Rc::new(Self {
            widget,
            view,
            workspace,
            environments,
            bridge_command,
            tabs: RefCell::new(Vec::new()),
            next_ordinal: Cell::new(1),
            live: Cell::new(false),
            on_open_environment: RefCell::new(None),
            on_orchestrator_changed: RefCell::new(None),
        });

        {
            let weak = Rc::downgrade(&tabs);
            new_tab_button.connect_clicked(move |_| {
                if let Some(tabs) = weak.upgrade() {
                    tabs.open_chat();
                }
            });
        }
        {
            // Closing a chat ends its session. The chat pane is a fixture
            // of the window, so the strip never empties: closing the last
            // chat leaves a fresh one in its place, set up like the one it
            // replaces. (Refusing the close instead — the console's idiom
            // for its pinned tabs — would leave a × that does nothing,
            // which those tabs avoid by not drawing one at all.)
            let weak = Rc::downgrade(&tabs);
            tabs.view.connect_close_page(move |view, page| {
                let Some(tabs) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                // Look up, THEN mutate: holding the borrow across the
                // removal is a panic, not a race.
                let index = tabs.tabs.borrow().iter().position(|tab| tab.page == *page);
                let closing = index.map(|index| tabs.tabs.borrow_mut().remove(index));
                if let Some(tab) = closing {
                    // The replacement goes in BEFORE the close, so the pane
                    // never spends a frame as an empty rectangle.
                    if view.n_pages() <= 1 {
                        tabs.add_pane(Some(&tab.pane));
                    }
                    // Ends the ACP session and tears the wire down; the
                    // conversation stays with the agent, but nothing in the
                    // IDE points at it any more.
                    tab.pane.close();
                }
                view.close_page_finish(page, true);
                glib::Propagation::Stop
            });
        }
        {
            let weak = Rc::downgrade(&tabs);
            tabs.view.connect_page_detached(move |_, _, _| {
                let Some(tabs) = weak.upgrade() else { return };
                tabs.retitle();
                tabs.persist();
            });
        }
        {
            let weak = Rc::downgrade(&tabs);
            tabs.view.connect_selected_page_notify(move |_| {
                let Some(tabs) = weak.upgrade() else { return };
                tabs.on_selection_changed();
            });
        }

        // One chat always exists. It stays dormant until `start`.
        tabs.add_pane(None);
        tabs.retitle();
        tabs
    }

    /// The chat the user is looking at. Always Some in practice — the strip
    /// refuses to close its last tab — so this never has to be unwrapped by
    /// callers.
    pub fn selected(&self) -> Rc<ChatPane> {
        let selected = self.view.selected_page();
        let tabs = self.tabs.borrow();
        selected
            .and_then(|page| tabs.iter().find(|tab| tab.page == page))
            .or_else(|| tabs.first())
            .map(|tab| tab.pane.clone())
            .expect("the chat strip always has a tab")
    }

    /// Open a new chat beside the current one: fresh ACP session, same
    /// agent and session settings.
    pub fn open_chat(self: &Rc<Self>) -> Rc<ChatPane> {
        let previous = self.selected();
        let pane = self.add_pane(Some(&previous));
        self.retitle();
        // Selecting runs the selection handler, which borrows `tabs`: take
        // the page out of the borrow first.
        let page = self.tabs.borrow().last().map(|tab| tab.page.clone());
        if let Some(page) = page {
            self.view.set_selected_page(&page);
        }
        // Selection did the activating; persisting the new tab is ours.
        self.persist();
        pane
    }

    /// Bring the strip to life from persisted state: one tab per remembered
    /// chat, the last-selected one connected, the rest armed and dormant.
    pub fn start(self: &Rc<Self>, chats: &[ChatEntry], active: usize) {
        // Built with `live` still off: adding tabs must not spawn agents.
        for (index, entry) in chats.iter().enumerate() {
            let pane = if index == 0 {
                let first = self.tabs.borrow()[0].pane.clone();
                first
            } else {
                self.add_pane(None)
            };
            pane.arm_from_entry(entry);
        }
        self.retitle();
        // Every tab is armed now, so the role can be settled against the
        // whole strip rather than against whichever tab restored first.
        self.settle_role();
        self.live.set(true);
        let target = active.min(self.view.n_pages().saturating_sub(1) as usize);
        let page = self.tabs.borrow().get(target).map(|tab| tab.page.clone());
        if let Some(page) = page {
            self.view.set_selected_page(&page);
        }
        // Whether or not the selection actually moved above, the selected
        // chat connects now — this is the greeting path, and an inert empty
        // box is not one.
        self.on_selection_changed();
        self.persist();
    }

    /// The tab list as restorable state: every chat left to right, and
    /// which one was selected.
    pub fn snapshot(&self) -> (Vec<ChatEntry>, usize) {
        let selected = self.view.selected_page();
        let tabs = self.tabs.borrow();
        let mut chats = Vec::with_capacity(tabs.len());
        let mut active = 0;
        for index in 0..self.view.n_pages() {
            let page = self.view.nth_page(index);
            let Some(tab) = tabs.iter().find(|tab| tab.page == page) else {
                continue;
            };
            if selected.as_ref() == Some(&page) {
                active = chats.len();
            }
            chats.push(tab.pane.chat_entry());
        }
        (chats, active)
    }

    fn add_pane(self: &Rc<Self>, inherit: Option<&ChatPane>) -> Rc<ChatPane> {
        let pane = ChatPane::new(
            self.workspace.clone(),
            self.environments.clone(),
            self.bridge_command.clone(),
        );
        if let Some(previous) = inherit {
            pane.inherit_settings(previous);
        }
        let page = self.view.append(&pane.widget);
        let ordinal = self.next_ordinal.get();
        self.next_ordinal.set(ordinal + 1);
        {
            let weak = Rc::downgrade(self);
            let persist: PersistHook = Rc::new(move || {
                if let Some(tabs) = weak.upgrade() {
                    tabs.retitle();
                    tabs.persist();
                }
            });
            let page_for_busy = page.clone();
            // AdwTabPage's own spinner: a chat working in a background tab
            // says so without costing a widget of our own.
            let busy: BusyHook = Rc::new(move |busy| page_for_busy.set_loading(busy));
            // The strip owns environment creation: it knows the ordinals a
            // readable slug is built from, and the clone is filesystem work
            // that must not touch the main thread.
            let weak = Rc::downgrade(self);
            let weak_pane = Rc::downgrade(&pane);
            let new_environment: EnvironmentHook = Rc::new(move || {
                if let (Some(tabs), Some(pane)) = (weak.upgrade(), weak_pane.upgrade()) {
                    tabs.give_environment(pane, ordinal);
                }
            });
            pane.set_hooks(persist, busy, new_environment);
            // The orchestrator role belongs to the strip, not to a pane:
            // it is one per workspace, and a pane cannot see its
            // neighbours to take it off them.
            let weak = Rc::downgrade(self);
            let weak_pane = Rc::downgrade(&pane);
            pane.set_on_role_changed(Rc::new(move |wanted: bool| {
                if let (Some(tabs), Some(pane)) = (weak.upgrade(), weak_pane.upgrade()) {
                    tabs.designate(pane, wanted);
                }
            }));
            // Watching, from the chat's own row. Resolved through the strip
            // at click time, so a pane built before the window wired itself
            // up still reaches the hook.
            let weak = Rc::downgrade(self);
            pane.set_on_open_environment(Rc::new(move |env: EnvironmentId| {
                if let Some(tabs) = weak.upgrade() {
                    let hook = tabs.on_open_environment.borrow().clone();
                    if let Some(hook) = hook {
                        hook(env);
                    }
                }
            }));
        }
        self.tabs.borrow_mut().push(Tab {
            page,
            pane: pane.clone(),
            ordinal,
        });
        pane
    }

    /// How a chat's "Open" row reaches the window's watching transition.
    pub fn set_on_open_environment(&self, hook: impl Fn(EnvironmentId) + 'static) {
        *self.on_open_environment.borrow_mut() = Some(Rc::new(hook));
    }

    // --- the orchestrator role -------------------------------------------

    /// How the strip tells the MCP server where to serve orchestration.
    pub fn set_on_orchestrator_changed(&self, hook: impl Fn(Option<EnvironmentId>) + 'static) {
        *self.on_orchestrator_changed.borrow_mut() = Some(Rc::new(hook));
    }

    /// The orchestrator's environment, if a chat holds the role and is
    /// bound. An orchestrator without an environment is not a state this
    /// strip allows — see [`ChatTabs::designate`].
    pub fn orchestrator_environment(&self) -> Option<EnvironmentId> {
        self.tabs
            .borrow()
            .iter()
            .find(|tab| tab.pane.is_orchestrator())
            .and_then(|tab| tab.pane.environment())
    }

    /// Move (or clear) the orchestrator role.
    ///
    /// Three things have to happen in this order, and the order is the
    /// whole of the correctness here:
    ///
    /// 1. **The chat must have an environment.** Orchestration tools are
    ///    served on an environment's MCP socket; every chat *without* one
    ///    shares the primary's, so designating an unbound chat would serve
    ///    execution authority to every other unbound chat in the
    ///    workspace. So an unbound chat gets a world of its own first —
    ///    in the same gesture, because "turn this on, then find the other
    ///    button" is not a gesture, it is a puzzle.
    /// 2. **The old holder loses it first.** One socket serves these, and
    ///    a moment with two chats believing they orchestrate is a moment
    ///    where the answer to "who may spawn agents" depends on timing.
    /// 3. **The server learns before the agent re-lists.** The tool list
    ///    is sent once per session, at `initialize`, so each affected chat
    ///    respawns afterwards — conversation intact via `session/load`,
    ///    exactly as relocation does it.
    fn designate(self: &Rc<Self>, pane: Rc<ChatPane>, wanted: bool) {
        if !wanted {
            pane.set_orchestrator_role(false);
            self.announce_orchestrator();
            pane.respawn_keeping_conversation();
            self.retitle();
            self.persist();
            return;
        }
        // An unbound chat earns its environment first, and takes the role
        // in the callback — a clone can fail, and a role that had already
        // moved would leave the workspace with an orchestrator whose tools
        // are served on nobody's socket.
        if pane.environment().is_none() {
            let ordinal = self
                .ordinal_of(&pane)
                .unwrap_or_else(|| self.next_ordinal.get());
            let weak = Rc::downgrade(self);
            let for_callback = pane.clone();
            self.give_environment_then(
                pane,
                ordinal,
                Some(Box::new(move |outcome: Result<EnvironmentId, String>| {
                    let Some(tabs) = weak.upgrade() else { return };
                    match outcome {
                        Ok(_) => tabs.take_role(for_callback),
                        // The pane's own row already says what went wrong;
                        // the switch goes back to off rather than claiming
                        // a role that cannot be served.
                        Err(_) => for_callback.set_orchestrator_role(false),
                    }
                })),
            );
            return;
        }
        self.take_role(pane);
    }

    /// Give this pane the role, taking it off whoever had it.
    fn take_role(self: &Rc<Self>, pane: Rc<ChatPane>) {
        let others: Vec<Rc<ChatPane>> = self
            .tabs
            .borrow()
            .iter()
            .filter(|tab| tab.pane.is_orchestrator() && !Rc::ptr_eq(&tab.pane, &pane))
            .map(|tab| tab.pane.clone())
            .collect();
        for other in &others {
            other.set_orchestrator_role(false);
        }
        pane.set_orchestrator_role(true);
        self.announce_orchestrator();
        // The old holder respawns too: it was just told it no longer
        // orchestrates, and an agent whose tool list still offers
        // chat_create would spend a turn discovering otherwise.
        for other in others {
            other.respawn_keeping_conversation();
        }
        pane.respawn_keeping_conversation();
        self.retitle();
        self.persist();
    }

    fn announce_orchestrator(&self) {
        let hook = self.on_orchestrator_changed.borrow().clone();
        if let Some(hook) = hook {
            hook(self.orchestrator_environment());
        }
    }

    /// Settle the role after a restore.
    ///
    /// The state file holds one flag per chat, and nothing stops a
    /// hand-edited (or half-written) file from claiming two — or from
    /// claiming one on a chat whose environment is gone. Both are settled
    /// here, once, with the first bound claimant winning, rather than left
    /// for whichever code path notices first.
    fn settle_role(self: &Rc<Self>) {
        let claimants: Vec<Rc<ChatPane>> = self
            .tabs
            .borrow()
            .iter()
            .filter(|tab| tab.pane.is_orchestrator())
            .map(|tab| tab.pane.clone())
            .collect();
        let winner = claimants
            .iter()
            .find(|pane| pane.environment().is_some())
            .cloned();
        for pane in claimants {
            let keeps = winner.as_ref().is_some_and(|w| Rc::ptr_eq(w, &pane));
            if !keeps {
                pane.set_orchestrator_role(false);
            }
        }
        self.announce_orchestrator();
    }

    /// The chat working in an environment — how orchestration tools
    /// resolve a chat id. The first, when a second tab shares the binding:
    /// the same rule [`ChatTabs::binding_for`] renders.
    pub fn pane_for_environment(&self, env: &EnvironmentId) -> Option<Rc<ChatPane>> {
        self.tabs
            .borrow()
            .iter()
            .find(|tab| tab.pane.environment().as_ref() == Some(env))
            .map(|tab| tab.pane.clone())
    }

    /// `chat_create`: a new tab, an environment of its own, and a live
    /// session ready to be prompted.
    ///
    /// Everything the orchestrator asked for is applied *before* the
    /// agent spawns (the agent id and the model both belong to the
    /// session that is about to start), and the answer waits for that
    /// session to reach Ready — because until it does, the model it
    /// advertises is unknown and "the model you asked for does not exist"
    /// cannot be said honestly.
    ///
    /// The tab is created in the background. Stealing the user's
    /// selection because an agent delegated something would take the
    /// window away from whatever they were reading; the tab is there, with
    /// its own spinner, whenever they want it.
    pub fn create_orchestrated(
        self: &Rc<Self>,
        agent: Option<String>,
        model: Option<String>,
        done: Box<dyn FnOnce(Result<taste_core::orchestration::CreatedChat, String>)>,
    ) {
        let pane = self.add_pane(None);
        if let Some(agent) = &agent {
            if !pane.set_agent_id(agent) {
                let known: Vec<String> = taste_acp::builtin_agents()
                    .iter()
                    .map(|spec| spec.id.clone())
                    .collect();
                self.close_pane(&pane);
                done(Err(format!(
                    "no agent {agent:?} — this IDE ships {known:?}. Nothing was created."
                )));
                return;
            }
        }
        pane.set_model_value(model.clone());
        self.retitle();
        let ordinal = self
            .ordinal_of(&pane)
            .unwrap_or_else(|| self.next_ordinal.get());
        let weak = Rc::downgrade(self);
        let for_ready = pane.clone();
        self.give_environment_then(
            pane,
            ordinal,
            Some(Box::new(move |outcome: Result<EnvironmentId, String>| {
                let Some(tabs) = weak.upgrade() else {
                    done(Err(
                        "the IDE closed while the environment was cloning".into()
                    ));
                    return;
                };
                let env = match outcome {
                    Ok(env) => env,
                    Err(reason) => {
                        tabs.close_pane(&for_ready);
                        done(Err(format!(
                            "the environment could not be created: {reason}"
                        )));
                        return;
                    }
                };
                tabs.retitle();
                tabs.persist();
                // Binding spawned the agent against the new clone. The
                // answer is owed the session it will actually run in.
                let pane = for_ready.clone();
                for_ready.on_ready_once(Box::new(move |pane_at_ready| {
                    let advertised = pane_at_ready.advertised_models();
                    if let Some(wanted) = &model {
                        let known = advertised.iter().any(|(value, _)| value == wanted);
                        if !known {
                            let ids: Vec<&str> =
                                advertised.iter().map(|(value, _)| value.as_str()).collect();
                            // The chat stays: it exists, it is bound, and
                            // it has been told nothing. Destroying an
                            // environment over a mistyped model would be a
                            // larger surprise than a tab the user can see
                            // and close.
                            pane_at_ready.set_model_value(None);
                            done(Err(format!(
                                "{} does not offer a model {wanted:?} — it advertises {ids:?}. \
                                 The chat {env} was created and is idle: it was NOT given the \
                                 task. Destroy it from the fleet view, or dispatch to it with \
                                 chat_send.",
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
                let _ = pane;
            })),
        );
    }

    /// Every environment a chat is working in — the addressable chats, as
    /// an unknown-id refusal names them.
    pub fn bound_environments(&self) -> Vec<EnvironmentId> {
        self.tabs
            .borrow()
            .iter()
            .filter_map(|tab| tab.pane.environment())
            .collect()
    }

    fn ordinal_of(&self, pane: &Rc<ChatPane>) -> Option<u32> {
        self.tabs
            .borrow()
            .iter()
            .find(|tab| Rc::ptr_eq(&tab.pane, pane))
            .map(|tab| tab.ordinal)
    }

    /// Close a tab the strip opened and then could not finish setting up.
    /// Goes through the view's own close path, so the bookkeeping (and the
    /// session teardown) is the same as a user pressing ×.
    fn close_pane(&self, pane: &Rc<ChatPane>) {
        let page = self
            .tabs
            .borrow()
            .iter()
            .find(|tab| Rc::ptr_eq(&tab.pane, pane))
            .map(|tab| tab.page.clone());
        if let Some(page) = page {
            self.view.close_page(&page);
        }
    }

    /// Which chat works in this environment, as the fleet view says it.
    ///
    /// Chats bound to nothing are the primary's — that is what an unbound
    /// chat means, not a missing value. More than one chat can work in one
    /// environment (a second tab given the same binding), so the row names
    /// the first and counts the rest, and is busy if any of them is.
    pub fn binding_for(&self, env: &EnvironmentId) -> Option<crate::fleet::ChatBinding> {
        let tabs = self.tabs.borrow();
        let mine: Vec<&Tab> = tabs
            .iter()
            .filter(|tab| {
                tab.pane
                    .environment()
                    .unwrap_or_else(EnvironmentId::primary)
                    == *env
            })
            .collect();
        let first = mine.first()?;
        let label = match mine.len() {
            1 => first.page.title().to_string(),
            n => format!("{} +{}", first.page.title(), n - 1),
        };
        Some(crate::fleet::ChatBinding {
            label,
            busy: mine.iter().any(|tab| tab.pane.is_busy()),
            orchestrator: mine.iter().any(|tab| tab.pane.is_orchestrator()),
        })
    }

    /// Bring a chat to the front, by the notification key its pane was
    /// built with. Returns whether one was found — a notification can
    /// outlive the chat it came from, and the desktop will happily hand
    /// back a click on it days later.
    pub fn select_by_notify_key(&self, key: &str) -> bool {
        let page = self
            .tabs
            .borrow()
            .iter()
            .find(|tab| tab.pane.answers_to(key))
            .map(|tab| tab.page.clone());
        match page {
            Some(page) => {
                self.view.set_selected_page(&page);
                true
            }
            None => false,
        }
    }

    /// Bring the chat working in an environment to the front — gadget
    /// mode's click-through. The same one-way lookup as
    /// [`ChatTabs::binding_for`]: the row knows its world, the strip knows
    /// which conversation is in it.
    pub fn select_for_environment(&self, env: &EnvironmentId) -> bool {
        let page = self
            .tabs
            .borrow()
            .iter()
            .find(|tab| {
                tab.pane
                    .environment()
                    .unwrap_or_else(EnvironmentId::primary)
                    == *env
            })
            .map(|tab| tab.page.clone());
        match page {
            Some(page) => {
                self.view.set_selected_page(&page);
                true
            }
            None => false,
        }
    }

    /// The user came back to the window: every chat retires the
    /// notifications that were only telling them to. Each pane withdraws
    /// its OWN ids — there is no global list to keep in step, which is the
    /// point of scoping the ids per pane.
    pub fn withdraw_informational(&self) {
        for tab in self.tabs.borrow().iter() {
            tab.pane.withdraw_informational();
        }
    }

    /// An environment's container changed state: tell the chats bound to
    /// it, so each can move its agent into or out of that container.
    ///
    /// Every chat, not just the selected one. A background tab whose
    /// environment came up should be running beside its files by the time
    /// the user looks at it — and a background tab whose container went
    /// away is exactly the one that would otherwise sit dead unnoticed.
    /// Chats bound elsewhere are untouched: environments are separate
    /// worlds, and one rebuilding does not disturb another.
    pub fn on_environment_state(&self, env: &EnvironmentId, state: &DevcontainerStateEvent) {
        let panes: Vec<Rc<ChatPane>> = self
            .tabs
            .borrow()
            .iter()
            .filter(|tab| {
                tab.pane
                    .environment()
                    .unwrap_or_else(EnvironmentId::primary)
                    == *env
            })
            .map(|tab| tab.pane.clone())
            .collect();
        for pane in panes {
            pane.on_environment_state(state);
        }
    }

    fn on_selection_changed(&self) {
        let selected = self.view.selected_page();
        let panes: Vec<(bool, Rc<ChatPane>)> = self
            .tabs
            .borrow()
            .iter()
            .map(|tab| (selected.as_ref() == Some(&tab.page), tab.pane.clone()))
            .collect();
        let mut active: Option<Rc<ChatPane>> = None;
        for (is_selected, pane) in panes {
            pane.set_selected(is_selected);
            if is_selected {
                active = Some(pane);
            }
        }
        if !self.live.get() {
            return;
        }
        if let Some(pane) = active {
            // First interaction with a tab is what spawns its agent: a
            // remembered conversation comes back here, through the same
            // lazy `ensure_client` a single chat has always used.
            pane.activate();
            // Switching to a chat puts the caret where you would type.
            // Deferred: the page is still being mapped, and grabbing focus
            // into a widget that is not on screen yet silently does nothing.
            let pane = pane.clone();
            glib::idle_add_local_once(move || pane.focus_composer());
        }
        self.persist();
    }

    /// Tab labels: the agent's name, the chat's number once there is more
    /// than one to tell apart, and — for a chat with a world of its own —
    /// which one.
    ///
    /// The environment suffix is deliberately quiet. It is a fact about
    /// where the chat works, not a status, and tabs are natural-width: a
    /// badge or a second line would make every bound chat's tab a
    /// different size from its neighbours.
    fn retitle(&self) {
        let tabs = self.tabs.borrow();
        let numbered = tabs.len() > 1;
        for tab in tabs.iter() {
            let name = tab.pane.agent_name();
            let mut title = if numbered {
                format!("{name} {}", tab.ordinal)
            } else {
                name.clone()
            };
            let environment = tab.pane.environment();
            if let Some(id) = &environment {
                title.push_str(&format!(" · {id}"));
            }
            tab.page.set_title(&title);
            // The role rides in the indicator slot, so a designated tab is
            // the same width as its neighbours.
            if tab.pane.is_orchestrator() {
                tab.page
                    .set_indicator_icon(Some(&gtk::gio::ThemedIcon::new(ORCHESTRATOR_ICON)));
                tab.page.set_indicator_tooltip(
                    "Orchestrator — this chat can create and drive other chats",
                );
            } else {
                tab.page.set_indicator_icon(None::<&gtk::gio::ThemedIcon>);
                tab.page.set_indicator_tooltip("");
            }
            let entry = tab.pane.chat_entry();
            let session = match entry.session_id {
                Some(session) => format!("session {session}"),
                None => "no session yet".to_string(),
            };
            let where_it_works = match &environment {
                Some(id) => format!("works in {id} — its own clone and devcontainer"),
                None => "works in your checkout (the primary environment)".to_string(),
            };
            let role = if tab.pane.is_orchestrator() {
                "\nOrchestrator: it can create and drive other chats"
            } else {
                ""
            };
            tab.page
                .set_tooltip(&format!("{name} · {session}\n{where_it_works}{role}"));
        }
    }

    /// TASTE_PROBE_CHECK only: bind the selected chat to a named
    /// environment so a headless screenshot shows the indicator, without
    /// cloning a repository or respawning an agent.
    #[doc(hidden)]
    pub fn seed_environment_for_probe(&self, id: &str) {
        self.selected().seed_environment_for_probe(id);
        self.retitle();
    }

    /// TASTE_PROBE_CHECK only: the orchestrator's strip — a designated
    /// chat and a sub-chat it created — so the role's two visual claims
    /// (a glyph on the tab, a marker in the fleet's chat column) can be
    /// looked at rather than asserted about.
    #[doc(hidden)]
    pub fn seed_orchestration_for_probe(self: &Rc<Self>, open_options: bool) {
        self.selected().seed_orchestrator_for_probe(open_options);
        let sub = self.add_pane(None);
        sub.seed_environment_for_probe("spry-2");
        sub.seed_transcript_for_probe();
        self.retitle();
    }

    /// Give one chat a world of its own: a clone of the workspace, a
    /// supervisor over it, and the chat re-aimed at both.
    ///
    /// The clone runs on a worker: it is a repository copy, and this
    /// repository is fast to clone but nothing guarantees the user's is.
    /// The row says what is happening meanwhile, and a failure puts the
    /// offer back rather than leaving a dead button.
    ///
    /// The container is deliberately NOT started. Environments are lazy —
    /// clone on creation, build on first need — and starting one runs its
    /// config's lifecycle commands, which is the user's call through the
    /// existing reload gates and not a side effect of clicking this.
    fn give_environment(self: &Rc<Self>, pane: Rc<ChatPane>, ordinal: u32) {
        self.give_environment_then(pane, ordinal, None);
    }

    /// ...and the same, telling `then` how it went.
    ///
    /// Two callers need the outcome rather than just the side effect:
    /// designating an unbound chat as the orchestrator (which may only
    /// take the role once there is a socket to serve it on) and
    /// `chat_create` (which owes a tool call an answer either way).
    #[allow(clippy::type_complexity)]
    fn give_environment_then(
        self: &Rc<Self>,
        pane: Rc<ChatPane>,
        ordinal: u32,
        then: Option<Box<dyn FnOnce(Result<EnvironmentId, String>)>>,
    ) {
        let id = match fresh_environment_id(ordinal, &self.environments.ids()) {
            Ok(id) => id,
            Err(e) => {
                pane.environment_failed(&format!("{e:#}"));
                if let Some(then) = then {
                    then(Err(format!("{e:#}")));
                }
                return;
            }
        };
        pane.environment_creating(&id);

        let registry = self.environments.clone();
        let weak = Rc::downgrade(self);
        let created = id.clone();
        glib::spawn_future_local(async move {
            let for_worker = created.clone();
            // Never on the GTK thread: this is a git clone.
            let handle = crate::runtime::runtime()
                .spawn_blocking(move || registry.create(for_worker).map(|_| ()));
            let outcome = match handle.await {
                Ok(result) => result.map_err(|e| format!("{e:#}")),
                Err(e) => Err(format!("the clone task did not finish: {e}")),
            };
            match outcome {
                Ok(()) => {
                    // Binding respawns the agent against the new aim; the
                    // conversation crosses on its session id.
                    pane.bind_environment(created.clone());
                    if let Some(tabs) = weak.upgrade() {
                        tabs.retitle();
                        tabs.persist();
                    }
                    if let Some(then) = then {
                        then(Ok(created));
                    }
                }
                Err(reason) => {
                    pane.environment_failed(&reason);
                    if let Some(then) = then {
                        then(Err(reason));
                    }
                }
            }
        });
    }

    /// Write the tab list to workspace state. Called whenever a chat gains
    /// or loses a session, changes agent, model or permission mode, or the
    /// set of tabs changes — the single-chat build wrote its session id at
    /// the same moments, for the same reason: waiting for window close is
    /// how stale ids survive an unclean exit.
    fn persist(&self) {
        if !self.live.get() {
            return;
        }
        let (chats, active) = self.snapshot();
        let root = self.workspace.root().to_path_buf();
        // Never on the GTK thread: this reads and writes a file.
        crate::runtime::runtime().spawn_blocking(move || {
            let mut state = taste_core::state::load(&root);
            state.root = root.clone();
            state.open_chats = chats;
            state.active_chat = active;
            if let Err(e) = taste_core::state::save(&root, &state) {
                tracing::warn!("saving open chats failed: {e:#}");
            }
        });
    }
}

/// A readable, unused environment id for the chat with this ordinal.
///
/// Ordinals are never reused, so the first candidate is almost always
/// free; the walk exists because the clone directory — not any list the
/// window holds — is the inventory of record, and a name may be taken by
/// an environment restored from disk.
///
/// The fleet view's "New Environment" names its environments the same way
/// — one vocabulary, so an environment a chat made and one a person made
/// are told apart by what they hold, never by how they are spelled.
pub(crate) fn fresh_environment_id(
    ordinal: u32,
    taken: &[EnvironmentId],
) -> anyhow::Result<EnvironmentId> {
    let adjective = ENVIRONMENT_ADJECTIVES[ordinal as usize % ENVIRONMENT_ADJECTIVES.len()];
    let mut suffix = ordinal;
    // Bounded: a walk that cannot end is a hung click, and a saturating
    // suffix would otherwise retry the same taken name forever.
    for _ in 0..1000 {
        let candidate = format!("{adjective}-{suffix}");
        if !taken.iter().any(|id| id.as_str() == candidate) {
            return EnvironmentId::parse(candidate);
        }
        suffix = suffix.saturating_add(1);
    }
    anyhow::bail!("no free environment id near {adjective}-{ordinal}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(slug: &str) -> EnvironmentId {
        EnvironmentId::parse(slug).unwrap()
    }

    /// Generated ids have to survive `EnvironmentId`'s validation, because
    /// they land verbatim in container names, volume names and a socket
    /// path — every ordinal, not just the small ones.
    #[test]
    fn generated_ids_are_readable_and_always_valid() {
        assert_eq!(fresh_environment_id(1, &[]).unwrap().as_str(), "calm-1");
        for ordinal in [0, 1, 7, 12, 13, 99, 100_000, u32::MAX] {
            let id = fresh_environment_id(ordinal, &[]).unwrap();
            assert!(id.as_str().len() <= taste_core::environment::MAX_ID_LEN);
            assert!(!id.is_primary());
            // Round-trips through the validator that container names use.
            assert_eq!(EnvironmentId::parse(id.as_str()).unwrap(), id);
        }
    }

    /// The disk is the inventory of record, so a name may already be taken
    /// by an environment restored from a clone. Walk rather than collide:
    /// `registry.create` would refuse, and the user would see a failure
    /// with nothing they could do about it.
    #[test]
    fn a_taken_name_is_walked_past() {
        let taken = vec![env("calm-1"), env("calm-2"), env("primary")];
        assert_eq!(
            fresh_environment_id(1, &taken).unwrap().as_str(),
            "calm-3",
            "the first free suffix, not the first candidate"
        );
        // A different chat's adjective is unaffected by another's collisions.
        assert_eq!(
            fresh_environment_id(2, &taken).unwrap().as_str(),
            "clever-2"
        );
    }
}

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

use crate::chat::{BusyHook, ChatPane, PersistHook};
use crate::envstrip::PRIMARY_TITLE;

/// The stack page the empty state lives on. Environments name their own
/// pages by slug, and a slug can never be this (it has no dot).
const EMPTY_PAGE: &str = "no.chat";

/// How the column tells the MCP server where the orchestration tools go.
type OrchestratorHook = Rc<dyn Fn(Option<EnvironmentId>)>;

struct Chat {
    env: EnvironmentId,
    pane: Rc<ChatPane>,
}

pub struct Chats {
    pub widget: gtk::Box,
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
    on_orchestrator_changed: RefCell<Option<OrchestratorHook>>,
    /// "Something a panel row renders has changed" — a turn starting or
    /// ending, a permission request arriving. The rows are assembled
    /// elsewhere; this asks for that to happen again.
    on_activity: RefCell<Option<Rc<dyn Fn()>>>,
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
            .icon_name("taste-chat-symbolic")
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
            .build();
        stack.add_named(&empty, Some(EMPTY_PAGE));

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&stack);

        let chats = Rc::new(Self {
            widget,
            stack,
            empty,
            start_button: start_button.clone(),
            workspace,
            environments,
            bridge_command,
            chats: RefCell::new(Vec::new()),
            current: RefCell::new(EnvironmentId::primary()),
            live: Cell::new(false),
            on_orchestrator_changed: RefCell::new(None),
            on_activity: RefCell::new(None),
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

    fn show_current(self: &Rc<Self>) {
        let env = self.current.borrow().clone();
        let pane = self.pane_for(&env);
        // Only the chat on screen may raise window-level toasts, whose
        // actions route back to it.
        for chat in self.chats.borrow().iter() {
            chat.pane.set_selected(chat.env == env);
        }
        match pane {
            Some(pane) => {
                self.stack.set_visible_child(&pane.widget);
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
            let weak = Rc::downgrade(self);
            let weak_pane = Rc::downgrade(&pane);
            pane.set_on_role_changed(Rc::new(move |wanted: bool| {
                if let (Some(chats), Some(pane)) = (weak.upgrade(), weak_pane.upgrade()) {
                    chats.designate(pane, wanted);
                }
            }));
        }
        self.chats.borrow_mut().push(Chat {
            env: env.clone(),
            pane: pane.clone(),
        });
        pane
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
        if chat.pane.is_orchestrator() {
            self.announce_orchestrator();
        }
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
        // Every chat is armed now, so the role can be settled against the
        // whole set rather than against whichever one restored first.
        self.settle_role();
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
            attention: chat.pane.needs_attention(),
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

    // --- the orchestrator role -------------------------------------------

    /// How the column tells the MCP server where to serve orchestration.
    pub fn set_on_orchestrator_changed(&self, hook: impl Fn(Option<EnvironmentId>) + 'static) {
        *self.on_orchestrator_changed.borrow_mut() = Some(Rc::new(hook));
    }

    /// The orchestrator's environment, if a chat holds the role.
    pub fn orchestrator_environment(&self) -> Option<EnvironmentId> {
        self.chats
            .borrow()
            .iter()
            .find(|chat| chat.pane.is_orchestrator())
            .map(|chat| chat.env.clone())
    }

    /// Move (or clear) the orchestrator role.
    ///
    /// Simpler than it was, and for a structural reason: every chat now has
    /// an environment, so there is no clone to make first. What remains is
    /// the order that was always the correctness here — the old holder
    /// loses it before the new one gains it (a moment with two chats
    /// believing they orchestrate is a moment where "who may spawn agents"
    /// depends on timing), and the server learns before either agent
    /// re-lists, since ACP sends the tool list once per session.
    fn designate(self: &Rc<Self>, pane: Rc<ChatPane>, wanted: bool) {
        if !wanted {
            pane.set_orchestrator_role(false);
            self.announce_orchestrator();
            pane.respawn_keeping_conversation();
            self.persist();
            return;
        }
        // The primary's socket is the hub every unbound connection shares;
        // serving `chat_create` there would hand execution authority to
        // every one of them. The switch is insensitive on that chat, so
        // this is the second wall rather than the first.
        if pane.environment().is_primary() {
            pane.set_orchestrator_role(false);
            return;
        }
        let others: Vec<Rc<ChatPane>> = self
            .chats
            .borrow()
            .iter()
            .filter(|chat| chat.pane.is_orchestrator() && !Rc::ptr_eq(&chat.pane, &pane))
            .map(|chat| chat.pane.clone())
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
        self.persist();
    }

    fn announce_orchestrator(&self) {
        let hook = self.on_orchestrator_changed.borrow().clone();
        if let Some(hook) = hook {
            hook(self.orchestrator_environment());
        }
    }

    /// Settle the role after a restore: at most one holder, and never the
    /// primary's chat. A state file can claim otherwise — hand-edited, or
    /// written by a build with other ideas — and this decides it once,
    /// here, rather than leaving it to whichever path notices first.
    fn settle_role(self: &Rc<Self>) {
        let claimants: Vec<Rc<ChatPane>> = self
            .chats
            .borrow()
            .iter()
            .filter(|chat| chat.pane.is_orchestrator() && !chat.env.is_primary())
            .map(|chat| chat.pane.clone())
            .collect();
        let winner = claimants.first().cloned();
        let all: Vec<Rc<ChatPane>> = self
            .chats
            .borrow()
            .iter()
            .filter(|chat| chat.pane.is_orchestrator())
            .map(|chat| chat.pane.clone())
            .collect();
        for pane in all {
            let keeps = winner.as_ref().is_some_and(|w| Rc::ptr_eq(w, &pane));
            if !keeps {
                pane.set_orchestrator_role(false);
            }
        }
        self.announce_orchestrator();
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
                "Start one to work on {PRIMARY_TITLE} — your own checkout. \
                 It edits the files you are looking at."
            ),
        );
    }
    (
        "No Agent Here Yet".to_string(),
        format!(
            "Start one to work in {env} — its own clone of the workspace, \
             with its own devcontainer."
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

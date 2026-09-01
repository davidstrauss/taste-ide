//! The chat pane's tab strip: N chats in one pane.
//!
//! A tab IS a [`ChatPane`] — its session, transcript, composer and session
//! settings travel together, because they are all facets of one
//! conversation. This type owns the strip and nothing else: which chat is
//! selected, opening and closing them, and writing the list of them to
//! workspace state. Everything about *talking to an agent* stays in
//! `chat.rs`.
//!
//! Two rules worth keeping in mind when this grows environments (see
//! docs/ENVIRONMENTS.md phase 2):
//!
//! - **Tabs are lazy.** A restored tab arms its session id and connects on
//!   first selection, so five remembered chats cost five labels, not five
//!   agent processes.
//! - **The window addresses the SELECTED pane.** Sign-in completion, the
//!   destroy-session toast, commit-message suggestions and the ui_probe
//!   "chat" target all resolve through [`ChatTabs::selected`], so a
//!   background chat can never be answered on the user's behalf.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use taste_core::state::ChatEntry;
use taste_core::Workspace;

use crate::chat::{BusyHook, ChatPane, PersistHook};

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
    mcp_bridge: (String, Vec<String>),
    mcp_socket: PathBuf,
    tabs: RefCell<Vec<Tab>>,
    next_ordinal: Cell<u32>,
    /// Off until [`ChatTabs::start`]: while restoring (and forever, in a
    /// probe instance) tabs neither connect nor persist.
    live: Cell<bool>,
}

impl ChatTabs {
    pub fn new(
        workspace: Workspace,
        mcp_bridge: (String, Vec<String>),
        mcp_socket: PathBuf,
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
            mcp_bridge,
            mcp_socket,
            tabs: RefCell::new(Vec::new()),
            next_ordinal: Cell::new(1),
            live: Cell::new(false),
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
            self.mcp_bridge.clone(),
            self.mcp_socket.clone(),
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
            pane.set_hooks(persist, busy);
        }
        self.tabs.borrow_mut().push(Tab {
            page,
            pane: pane.clone(),
            ordinal,
        });
        pane
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
        }
        self.persist();
    }

    /// Tab labels: the agent's name, plus the chat's number once there is
    /// more than one to tell apart.
    fn retitle(&self) {
        let tabs = self.tabs.borrow();
        let numbered = tabs.len() > 1;
        for tab in tabs.iter() {
            let name = tab.pane.agent_name();
            tab.page.set_title(&if numbered {
                format!("{name} {}", tab.ordinal)
            } else {
                name.clone()
            });
            let entry = tab.pane.chat_entry();
            tab.page.set_tooltip(&match entry.session_id {
                Some(session) => format!("{name} · session {session}"),
                None => format!("{name} · no session yet"),
            });
        }
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

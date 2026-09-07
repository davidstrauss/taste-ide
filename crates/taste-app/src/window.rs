//! The one window arrangement: files left, editor center, console bottom,
//! AI chat right. Resizable and collapsible; never rearrangeable.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use taste_core::event::FlatpakStateEvent;
use taste_core::{Event, Workspace};
use taste_devcontainer::EnvironmentRegistry;
use taste_flatpak::Packager;
use taste_mcp::McpServer;

use crate::chats::Chats;
use crate::console::Console;
use crate::devcontainer_ui::DevcontainerBanner;
use crate::editor::{Editor, GraftedTab};
use crate::filetree::FileTree;
use crate::portview::PortFacts;
use crate::runtime::runtime;
use crate::tabfamily::Family;

/// The file-tree flank's width when a window opens, in pixels. Above the
/// flank's minimum, so it is what the user gets rather than a clamp; the
/// width every frame in docs/screenshots was taken at.
const FLANK_OPENING_WIDTH: i32 = 335;

/// The chat's opening width. The paned between the center and the chat
/// keeps the chat at this width as the window grows (resize goes to the
/// center), and it is set from the paned's REAL width once there is one —
/// a fixed start-child position would hand a wide display's surplus to the
/// chat, and the chat is the one pane that must never force or take width:
/// its prose reflows, and we cope with it narrow. The number is the chat
/// column's own natural width, so the pane asks for what it opens at.
const CHAT_OPENING_WIDTH: i32 = crate::chat_column::NATURAL_WIDTH;

pub fn build_window(app: &adw::Application, root: PathBuf) -> adw::ApplicationWindow {
    // A TASTE_PROBE_CHECK instance is scaffolding, not a session: it must
    // observe (render, measure, quit) without leaving a footprint. One of
    // these once saved its empty state over a real window's on the same
    // workspace — and worse, session/load'ed the user's LIVE conversation
    // from a throwaway process.
    let probe_mode = std::env::var("TASTE_PROBE_CHECK").is_ok();
    let workspace = Workspace::open(root.clone());

    // --- one folder, one supervisor --------------------------------------
    // N windows on N folders is the design (main.rs, NON_UNIQUE) and every
    // derived name is keyed by the folder, so they never meet. N windows on
    // ONE folder is the case keying cannot answer: the key IS the folder, so
    // both windows compute the same container names, the same volumes, the
    // same fleet socket, the same build staging directory. Two supervisors
    // then fight — one window's reload force-removes the container the other
    // is streaming, one window's staging wipe lands mid-build in the other.
    //
    // No arbitration makes two supervisors correct, so the first window to
    // open a folder supervises it and a second one edits. Everything with no
    // shared mutable state behind it — files, git, search, the editor — works
    // exactly as it always does, which is most of the IDE.
    //
    // A probe instance is scaffolding and claims nothing: taking the lock
    // would make a screenshot run demote the user's real window.
    let supervision = if probe_mode {
        None
    } else {
        Some(taste_core::instance::claim(&root))
    };
    let supervising = supervision.as_ref().is_none_or(|s| s.is_granted());

    // --- background services -------------------------------------------
    // This workspace's environments. The registry owns them all; the
    // primary — the main checkout — is the one the window's panes are aimed
    // at, which is a fact about the UI, not a privilege of that
    // environment. Aiming them elsewhere is phase 5's watching.
    //
    // The registry starts on the local host and learns its real substrate
    // in `reconcile`, on the runtime. Resolving here would mean booting a
    // VM on the GTK thread — up to twenty seconds of frozen window — and
    // there is nothing to resolve it *for* yet: environments are lazy, so
    // no container exists to be in the wrong place.
    let environments = EnvironmentRegistry::new(
        root.clone(),
        workspace.events.clone(),
        workspace.exec.clone(),
    );
    let supervisor = environments.primary();
    let primary_env = supervisor.id().clone();

    // Flatpak packaging: build/install/launch is a first-class, USER-
    // triggered task (agents get read-only status/logs over MCP).
    let packager = Packager::new(root.clone(), workspace.events.clone());

    // One MCP socket per environment, all served by this one server: the
    // socket an agent connects on is the environment it is in. Binding
    // follows the registry, so environments restored from their clones get
    // sockets as they are picked back up, and destroyed ones lose theirs.
    let server = McpServer::new(environments.clone(), packager.clone(), workspace.clone());
    runtime().spawn(server.clone().serve_all());

    // Search by meaning (`taste-semantic`), for the agents' one question
    // grep cannot answer. The server answers `ide_semantic_search` from it;
    // the keeper builds the primary checkout's index in the background and
    // keeps it current, fetching the pinned model first if this machine
    // has never had it.
    let semantic = taste_semantic::Semantic::new(&root);
    server.set_semantic(semantic.clone());

    // ...and the same server, plus the auth proxy, on the other route in:
    // the environment channels. An agent relocated into a devcontainer
    // cannot dial either socket the IDE bound — a confined container is
    // refused `connectto` on an unconfined listener's socket, on every
    // SELinux-enforcing host — so the endpoints live inside the container
    // and their traffic comes out over `podman exec` stdio. The supervisor
    // opens those channels; this is what it serves down them.
    //
    // Told to the registry rather than to each supervisor, so an
    // environment a chat creates for itself later inherits it.
    // Kept before the server is handed to the channel services: the chat
    // strip tells it which environment's socket serves the orchestration
    // tools, and the server is the only thing that can act on that.
    environments.set_channel_services(crate::env_channel::IdeChannelServices::new(server));

    // Agents reach the MCP server through our own binary's bridge mode.
    // The socket half is per environment, so the command is composed at
    // spawn time from the chat's binding (`taste_acp::AgentAim`).
    let bridge_command = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "taste-ide".into());

    // --- panes -----------------------------------------------------------
    let editor = Editor::new(workspace.clone());
    let filetree = FileTree::new(workspace.clone());
    {
        let editor = editor.clone();
        filetree.set_on_open(move |path, line| editor.open_at(&path, line));
    }
    {
        // Changed-list rows open as diffs: the tab lands on its Changes face.
        let editor = editor.clone();
        filetree.set_on_open_diff(move |path| editor.open_changes(&path));
    }
    {
        // ...and a REVIEW row opens the branch's two sides instead. Not the
        // same call with a different base: there is no file on disk behind
        // a branch's version of a file, and the working tree has no part in
        // it.
        let editor = editor.clone();
        filetree.set_on_open_review_diff(move |rel, branch, target| {
            editor.open_review_diff(&rel, &branch, &target)
        });
    }
    {
        // The review's tabs are the review's: leaving it takes them.
        let editor = editor.clone();
        filetree.set_on_review_ended(move || editor.close_review_tabs());
    }
    {
        // ...and the flank follows the strip: the tab in front is the row
        // selected, when one corresponds.
        let filetree = Rc::downgrade(&filetree);
        editor.set_on_focus_changed(move |focused| {
            if let Some(filetree) = filetree.upgrade() {
                filetree.select_for_editor(focused);
            }
        });
    }
    // What the window knows about each forwarded port (the tick's connect
    // probe, a tab's deeper look), keyed by environment and port. The tree's
    // rows and the port tabs both read it; only the probes write it.
    let port_facts: Rc<RefCell<HashMap<(taste_core::environment::EnvironmentId, u16), PortFacts>>> =
        Rc::new(RefCell::new(HashMap::new()));
    // Where the IDE-log follower has read to (`app_log::since`). Shared by
    // the open (which seeds from it) and the tick (which appends from it),
    // so a line is never shown twice.
    let ide_log_cursor: Rc<std::cell::Cell<u64>> = Rc::new(std::cell::Cell::new(0));
    // How much each log has been saying: the Logs rows' sparklines.
    let log_activity: Rc<crate::logview::LogActivity> =
        Rc::new(crate::logview::LogActivity::default());
    // Opening a log as a tab, seeded with what the log holds. Shared,
    // because two things ask for it: a Logs row in the tree, and
    // `Event::ShowDevcontainerLog` — the safe-mode banner's "View Log" and
    // anything else that wants a build watched while it runs. There is one
    // place a log is shown, and this is the way to it.
    let open_log: Rc<dyn Fn(taste_core::environment::EnvironmentId, crate::logview::LogKind)> = {
        let editor = editor.clone();
        let environments = environments.clone();
        let ide_log_cursor = ide_log_cursor.clone();
        Rc::new(move |env, kind| {
            // The log already on screen: a click steps through its hits.
            if editor.focused() == crate::editor::Focused::Log(env.clone(), kind)
                && editor.step_results()
            {
                return;
            }
            let seed = match kind {
                crate::logview::LogKind::Environment => environments
                    .get(&env)
                    .map(|supervisor| supervisor.logs_tail(5000))
                    .unwrap_or_default(),
                crate::logview::LogKind::Container => environments
                    .get(&env)
                    .map(|supervisor| supervisor.container_logs_tail(5000))
                    .unwrap_or_default(),
                crate::logview::LogKind::Ide => {
                    let (cursor, lines) = taste_core::app_log::since(0);
                    ide_log_cursor.set(cursor);
                    lines
                }
            };
            editor.open_log(&env, kind, seed);
        })
    };
    {
        let open_log = open_log.clone();
        filetree.set_on_open_log(move |env, kind| open_log(env, kind));
    }
    {
        // A Ports row opens the port as a tab, and takes the deeper look.
        let editor = editor.clone();
        let environments = environments.clone();
        let port_facts = port_facts.clone();
        filetree.set_on_open_port(move |env, port| {
            let spec = environments
                .get(&env)
                .and_then(|supervisor| supervisor.ports().into_iter().find(|p| p.port == port))
                .unwrap_or(taste_devcontainer::config::PortSpec {
                    port,
                    label: None,
                    protocol: None,
                });
            let facts = port_facts
                .borrow()
                .get(&(env.clone(), port))
                .cloned()
                .unwrap_or_default();
            editor.open_port(&env, spec.clone(), facts);
            probe_port(&editor, &environments, &port_facts, env, spec);
        });
    }
    let console = Console::new(workspace.clone(), environments.clone());
    // One chat per environment, and the pane shows the selected
    // environment's (see chats.rs). There is no tab strip: choosing a
    // conversation IS choosing an environment, and that choice belongs to
    // the panel under the file tree.
    let chats = Chats::new(workspace.clone(), environments.clone(), bridge_command);
    {
        // The ✨ button by the commit entry: staged diff → chat agent →
        // suggested message (the exchange stays visible in the transcript).
        let chats = chats.clone();
        filetree.set_commit_suggester(move |prompt, on_done| {
            // No agent in this environment yet: the ✨ button is asking a
            // conversation that does not exist. Saying so beats a button
            // that silently does nothing.
            match chats.selected() {
                Some(pane) => pane.request_text(prompt, on_done),
                None => on_done(String::new()),
            }
        });
    }
    {
        // The fleet's "bound chat" column: the strip is the authority on
        // which chat works where, and the console asks it at render time
        // rather than keeping a copy that could disagree.
        let chats = chats.clone();
        console.set_chat_lookup(move |env| chats.binding_for(env));
    }
    // The editor tells whose file a tab holds by asking the registry, which
    // is what makes a file from another environment open read-only and
    // badged — and what bounds an agent's mediated write by ITS checkout
    // rather than by the window's.
    editor.set_environments(environments.clone());

    // --- watching: one place decides where the panes are aimed ------------
    // ENVIRONMENTS.md → "Watching an environment". Three surfaces can ask
    // (a fleet row, a chat's environment row, the tree's way back), and all
    // three come through here, because the transition is four things at
    // once: the tree's target, the editor's notion of whose files these
    // are, the watcher that makes the agent's edits show up, and the fleet
    // row that must not claim to be showing something else. Watching is UI
    // state and is deliberately never persisted — a fresh IDE opens on the
    // user's own checkout.
    let watch_slot = std::rc::Rc::new(std::cell::RefCell::new(
        taste_core::watcher::WatchSlot::new(workspace.events.clone()),
    ));
    let aim_panes: std::rc::Rc<dyn Fn(Option<taste_core::environment::EnvironmentId>)> = {
        let filetree = filetree.clone();
        let editor = editor.clone();
        let console = console.clone();
        let chats = chats.clone();
        let environments = environments.clone();
        let watch_slot = watch_slot.clone();
        std::rc::Rc::new(move |env: Option<taste_core::environment::EnvironmentId>| {
            let env = env.unwrap_or_else(taste_core::environment::EnvironmentId::primary);
            let target = if env.is_primary() {
                None
            } else {
                match environments.get(&env) {
                    Some(supervisor) => Some((env.clone(), supervisor.root().to_path_buf())),
                    // An environment with no supervisor is one that does not
                    // exist. Refuse rather than quietly aiming at the
                    // primary: there is no fallback environment anywhere in
                    // this design, and a switch that silently landed
                    // somewhere else would move every pane — including which
                    // conversation is on screen — without saying so. Coming
                    // home when the environment being watched is DESTROYED is
                    // a different act, and `EnvironmentRemoved` asks for it
                    // by name.
                    None => return,
                }
            };
            // The clone gets a watcher WHILE it is watched and not a moment
            // longer: agent edits reload clean buffers, restyle the tree and
            // refresh git state, exactly as the user's own do — and going
            // back drops the watcher rather than accumulating one per
            // environment ever opened.
            watch_slot
                .borrow_mut()
                .aim(target.as_ref().map(|(_, root)| root.clone()));
            filetree.aim_at(target);
            // Each environment owns its editor tabs: switching stows the
            // ones on screen and brings back the ones this environment had,
            // scroll positions and unsaved buffers exactly as they were.
            editor.aim_at(&env);
            editor.sync_git_state();
            console.note_watching(&env);
            // ...and its conversation. The chat pane is a pane like the
            // rest: it renders the selected environment's chat, or offers
            // to start one.
            chats.show(&env);
        })
    };
    {
        // A chat that changed something a panel row renders (a turn
        // starting, a permission request arriving in an environment nobody
        // is looking at) asks for the rows to be re-assembled.
        //
        // ...and, while the window is narrow enough that the chat is a
        // tab rather than a column, lights that tab when the
        // conversation is stopped on the user. Same fact, said the way a
        // tab strip says it; a no-op at full width, where the chat is on
        // screen and the question is already visible.
        let console = console.clone();
        let editor_for_tab = editor.clone();
        let chats_for_tab = chats.clone();
        chats.set_on_activity(move || {
            console.refresh_fleet();
            editor_for_tab.set_chat_attention(
                chats_for_tab
                    .selected()
                    .is_some_and(|pane| pane.awaits_user()),
            );
        });
    }
    {
        // A grafted tab the user tried to close belongs to the pane that
        // handed it over, and that pane answers for it: the console's
        // sections refuse, and a terminal's tab closing is
        // how that shell ends — the same answers it gives in its own strip
        // at full width, because it is the same function.
        let console = console.clone();
        editor.set_on_close_grafted(move |view, page| {
            if console.owns_page(page) {
                return console.close_request(view, page);
            }
            // The chat's three faces are panes, not documents: they arrived
            // with the rung and they leave with it.
            view.close_page_finish(page, false);
            glib::Propagation::Stop
        });
    }
    {
        // How full this conversation is, said in the grafted tab's
        // indicator while the toggle that wears the same dot is not on
        // screen. One fact, one traffic dot, two slots.
        let editor_for_usage = editor.clone();
        chats.set_on_usage_severity(move |badge, tooltip| {
            // Second of the chat family: [chat] [usage] [settings].
            editor_for_usage.set_grafted_badge(Family::Chat, 1, badge, tooltip);
        });
    }
    {
        // The environment panel at the bottom of the file-tree pane: the
        // permanent context indicator, and the fourth surface that asks
        // for this transition. Returning home is the primary's own row —
        // `aim_panes` already reads the primary as "no environment".
        let aim_panes = aim_panes.clone();
        filetree.set_on_open_environment(move |env| aim_panes(Some(env)));
    }
    {
        // The editor asking to move the selection: it was told to open a
        // file that belongs to another environment (back/forward across a
        // stowed tab, or an agent pointing the user at its own work), and a
        // tab the user cannot see is not an open file.
        let aim_panes = aim_panes.clone();
        editor.set_on_open_environment(move |env| aim_panes(Some(env)));
    }
    // The title bar carries the search box (search.rs, docs/SEARCH.md):
    // one query for every surface, where a title used to sit. The window
    // still has its title for the shell; the workspace's name is the root
    // row of the tree, which is on screen.
    let search = crate::search::Search::new();
    // The index's keeper, now that the box exists to show its progress in.
    let semantic_keeper =
        crate::semantic::Keeper::start(semantic.clone(), workspace.clone(), Rc::downgrade(&search));
    // Every surface answers the one query; the panes tell the box which of
    // them focus is in, so Down from the box steps where the user was.
    filetree.attach_search(&search);
    editor.attach_search(&search);
    {
        // Results by meaning for the person at the box (David, 2026-09-07:
        // "I'd like to be able to use this for results for myself, too …
        // amend the current, literal hits with the ML/AI ones"). One
        // question to the index per settled query — a quarter second after
        // the last keystroke, since each costs an embedding — off the main
        // thread; the answer, if the query is still the one asked, goes to
        // the tree (files found by meaning, with ≈ badges) and the editor
        // (the file on screen's chunks, under "By meaning"). The toggle
        // beside the ghost, or an index that does not exist yet, means an
        // empty answer.
        let semantic = semantic.clone();
        let root = root.clone();
        let filetree = filetree.clone();
        let editor = editor.clone();
        let search_weak = Rc::downgrade(&search);
        let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        search.subscribe("meaning", move |query, generation| {
            if let Some(timer) = pending.borrow_mut().take() {
                timer.remove();
            }
            if query.is_empty() || !query.meaning || semantic.status(&root).is_none() {
                filetree.set_meaning_hits(Vec::new());
                editor.set_meaning_hits(Vec::new());
                return;
            }
            let text = query.text.clone();
            let (semantic, root, filetree, editor, search_weak) = (
                semantic.clone(),
                root.clone(),
                filetree.clone(),
                editor.clone(),
                search_weak.clone(),
            );
            let pending_for_timer = pending.clone();
            let timer =
                glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
                    pending_for_timer.borrow_mut().take();
                    let (ask, ask_root, ask_text) = (semantic.clone(), root.clone(), text.clone());
                    let handle = crate::runtime::runtime()
                        .spawn_blocking(move || ask.search(&ask_root, &ask_text, 40));
                    glib::spawn_future_local(async move {
                        let Ok(Ok(hits)) = handle.await else { return };
                        let Some(search) = search_weak.upgrade() else {
                            return;
                        };
                        if !search.is_current(generation) {
                            return;
                        }
                        let hits: Vec<crate::search::MeaningHit> = hits
                            .into_iter()
                            .filter(|hit| hit.score >= crate::search::MEANING_FLOOR)
                            .map(|hit| crate::search::MeaningHit {
                                path: root.join(&hit.path),
                                start_line: hit.start_line,
                                end_line: hit.end_line,
                                score: hit.score,
                                text: hit
                                    .text
                                    .lines()
                                    .find(|line| !line.trim().is_empty())
                                    .unwrap_or("")
                                    .trim()
                                    .to_string(),
                            })
                            .collect();
                        filetree.set_meaning_hits(hits.clone());
                        editor.set_meaning_hits(hits);
                    });
                });
            *pending.borrow_mut() = Some(timer);
        });
    }
    {
        // A click on a file with hits that is already on screen steps to
        // the next hit rather than reopening it at the first.
        let editor = editor.clone();
        filetree.set_on_step_file(move |path| editor.step_in(&path));
    }
    {
        // A click on the environment row the panes already aim at, while
        // it has hits inside: step the conversation's listing, or the
        // console's when the conversation has none.
        let chats = chats.clone();
        let console = console.clone();
        filetree.set_on_step_hits(move |_env| {
            if !chats.step_results() {
                console.step_results();
            }
        });
    }
    // The console and the chat answer with listings of their own, and both
    // count hits per environment; the backlog row wants the sum, so the two
    // maps are merged here and handed down together with the scrollback
    // scan's progress.
    {
        let inner: Rc<RefCell<(HashMap<String, usize>, HashMap<String, usize>)>> =
            Rc::new(RefCell::new((HashMap::new(), HashMap::new())));
        let publish = {
            let inner = inner.clone();
            let filetree = Rc::downgrade(&filetree);
            Rc::new(move |done: usize, total: usize| {
                let Some(filetree) = filetree.upgrade() else {
                    return;
                };
                let inner = inner.borrow();
                let mut merged = inner.0.clone();
                for (env, count) in &inner.1 {
                    *merged.entry(env.clone()).or_default() += count;
                }
                filetree.set_inner_hits(merged, done, total);
            })
        };
        {
            let inner = inner.clone();
            let publish = publish.clone();
            chats.attach_search(&search, move |counts| {
                inner.borrow_mut().0 = counts;
                publish(0, 0);
            });
        }
        {
            let inner = inner.clone();
            console.attach_search(&search, move |counts, done, total| {
                inner.borrow_mut().1 = counts;
                publish(done, total);
            });
        }
    }
    for (panel, widget) in [
        (
            crate::search::Panel::Tree,
            filetree.widget.clone().upcast::<gtk::Widget>(),
        ),
        (crate::search::Panel::Editor, editor.widget.clone().upcast()),
        (
            crate::search::Panel::Console,
            console.widget.clone().upcast(),
        ),
        (crate::search::Panel::Chat, chats.widget.clone().upcast()),
    ] {
        let focus = gtk::EventControllerFocus::new();
        let search_for_focus = search.clone();
        focus.connect_enter(move |_| search_for_focus.note_panel(panel));
        widget.add_controller(focus);
    }
    {
        // The backlog's environment actions all run the console's own, so
        // there is one way to stop a container, one to rebuild it, one
        // intervention that destroys a clone. The panel is where they are
        // reached from — the header for the selected row, the row's `⋮`
        // menu for the ones that only make sense pointed at one — and the
        // console is where they are done.
        let console_for_stop = console.clone();
        filetree.set_on_stop_environment(move |env| console_for_stop.stop_environment(env));
        let console_for_rebuild = console.clone();
        filetree
            .set_on_rebuild_environment(move |env| console_for_rebuild.rebuild_environment(env));
        let console_for_destroy = console.clone();
        filetree
            .set_on_destroy_environment(move |env| console_for_destroy.destroy_environment(env));
        let console_for_rename = console.clone();
        filetree.set_on_rename_environment(move |env| console_for_rename.rename_environment(env));
        let console_for_nuke = console.clone();
        filetree.set_on_nuke_environment(move |env| console_for_nuke.nuke_environment(env));
        let console_for_review = console.clone();
        filetree.set_on_open_review(move |env| console_for_review.open_review_for(&env));
        // Refresh: the whole off-thread pass, deep — branches, published
        // work, podman, and the directory walks the footprint needs.
        let console_for_refresh = console.clone();
        filetree.set_on_refresh_environments(move || {
            console_for_refresh.refresh_environment_data(true)
        });
    }
    {
        // The window has ONE intervention slot — the bottom panel in the
        // file tree's column — and the console's flows use it: rename, the
        // destroy confirmation that enumerates first, and reject's note.
        // Never a modal (ARCHITECTURE.md → the intervention convention).
        let tree_for_open = filetree.clone();
        let tree_for_close = filetree.clone();
        console.set_intervention_host(
            move |title| tree_for_open.open_named_intervention(title),
            move || tree_for_close.dismiss_named_intervention(),
        );
    }
    {
        // The judgment lives on the review tab, beside the diff it is
        // about: the console computes the mergedness and does the merging,
        // the editor draws it and asks.
        let editor_for_review = editor.clone();
        console.set_on_review_facts(move |facts| editor_for_review.set_review_facts(facts));
        let console_for_judgment = console.clone();
        editor.set_on_review_judgment(move |branch, action| {
            console_for_judgment.rule_on_review(&branch, action)
        });
    }
    {
        // Start, on an issue: the environment that IS that issue's — a
        // clone under the issue's id — a chat in it given the issue as its
        // first prompt, the store told who started it, and the panes aimed
        // there. The orchestrator's `issue_start` does the same through
        // `orchestration.rs`; this is the user's hand on the same lever.
        let environments = environments.clone();
        let chats = chats.clone();
        let workspace = workspace.clone();
        let aim_panes = aim_panes.clone();
        let console = console.clone();
        filetree.set_on_start_issue(move |issue| {
            start_issue(
                &environments,
                &chats,
                &workspace,
                &aim_panes,
                &console,
                issue,
            )
        });
    }
    {
        // The backlog under that panel. Two wires, and each one is the
        // queue meeting something that already exists rather than a
        // mechanism of its own:
        //
        // - a write asks the console to re-read the ref, because the
        //   console is where the off-thread git passes live;
        // - a refused write toasts, like every other action outcome.
        //
        // There is no third wire aiming the panes any more. A backlog row
        // used to select the environment holding it, which made every
        // claimed row a hidden jump; the panel above is where an
        // environment is chosen, and it is the one that now says what each
        // is working on.
        // The review band's Open Review: the console knows which branch,
        // the tree knows how to show one. The same `changed_since_base`
        // machinery the deleted Inbox filter used, which is why that
        // filter could be removed rather than replaced.
        let filetree_for_review = filetree.clone();
        console.set_on_open_review(move |branch, target| {
            filetree_for_review.open_review(branch, target)
        });
        // ...and a judgment that settles the environment takes the review
        // back off the panes, tabs included.
        let filetree_for_close = filetree.clone();
        console.set_on_close_review(move || filetree_for_close.close_review());
        let console = console.clone();
        filetree.set_on_backlog_changed(move || console.refresh_issues());
        let events = workspace.events.clone();
        filetree.set_on_backlog_error(move |message| {
            events.publish(Event::Toast(message));
        });
    }
    // The panel's tick re-renders the fleet: the assembly is cheap by
    // construction (no IO, no podman) and equality-guarded, and it is what
    // makes a chat that started streaming since the last fleet change show
    // its spinner. A permanent list has no open-moment to refresh on, so it
    // takes one every second. Registered where the ladder is retuned, which
    // rides on the same tick — see "the ladder's numbers".
    let banner = DevcontainerBanner::new(supervisor.clone(), workspace.events.clone());

    // shrink_*_child(false) everywhere: panes stop at their children's
    // minimum sizes instead of clipping their content.
    let center = gtk::Paned::builder()
        .orientation(gtk::Orientation::Vertical)
        .start_child(&editor.widget)
        .end_child(&console.widget)
        .resize_start_child(true)
        .resize_end_child(false)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .wide_handle(true)
        .position(560)
        .build();

    let center_and_chat = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&center)
        .end_child(&chats.widget)
        .resize_start_child(true)
        .resize_end_child(false)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .wide_handle(true)
        .build();
    // The divider is placed from the paned's own width at first
    // allocation, so the chat opens at CHAT_OPENING_WIDTH whatever the
    // display is. Once placed, GTK keeps the end child's size on resize.
    {
        let placed = std::rc::Rc::new(std::cell::Cell::new(false));
        let paned = center_and_chat.clone();
        center_and_chat.connect_map(move |_| {
            if placed.replace(true) {
                return;
            }
            let paned = paned.clone();
            glib::idle_add_local_once(move || {
                let width = paned.width();
                if width > CHAT_OPENING_WIDTH * 2 {
                    paned.set_position(width - CHAT_OPENING_WIDTH);
                }
            });
        });
    }

    let outer = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&filetree.widget)
        .end_child(&center_and_chat)
        .resize_start_child(false)
        .resize_end_child(true)
        // shrink stays false here too: shrinkable panes get allocated
        // below their minimum and CLIP (measured: the tree lost its left
        // edge, the chat its Send button). Tiling is enabled by keeping
        // the real minimums small instead — TASTE_MEASURE_MIN=1 audits
        // them.
        .shrink_start_child(false)
        .shrink_end_child(false)
        .wide_handle(true)
        // The flank's opening width, stated. It used to say 260, which was
        // under the flank's minimum, so the flank opened at whatever that
        // minimum happened to be — 335 for as long as the branch label
        // held a 14-character floor, and 280 the day it stopped. A width
        // that every frame in docs/screenshots has and nothing in the code
        // chose is a width to write down.
        .position(FLANK_OPENING_WIDTH)
        .build();

    let title = adw::WindowTitle::new(
        &root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "taste".into()),
        "Taste IDE",
    );
    // Opinionated chrome: no minimize button. An IDE session is something
    // you're in or you close; maximize and close remain.
    let header = adw::HeaderBar::builder()
        .title_widget(&search.widget)
        .decoration_layout(":maximize,close")
        .build();
    let app_icon = gtk::Image::from_icon_name(crate::APP_ID);
    app_icon.set_pixel_size(20);
    app_icon.set_margin_start(6);
    header.pack_start(&app_icon);
    // File navigation lives with the window chrome, right of the carrot.
    header.pack_start(&editor.back_button);
    header.pack_start(&editor.forward_button);
    // Primary menu — the HIG staple every GNOME window carries.
    let menu = gtk::gio::Menu::new();
    menu.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
    menu.append(Some("About Taste"), Some("win.about"));
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .primary(true)
        .tooltip_text("Main menu")
        .build();
    header.pack_end(&menu_button);

    // The deploy button: build the workspace's Flatpak, install it into the
    // user installation, launch it. Visible only when a manifest exists.
    let flatpak_button = gtk::Button::builder()
        .icon_name("package-x-generic-symbolic")
        .tooltip_text("Build, install, and run as Flatpak")
        .visible(packager.manifest().is_some())
        .build();
    {
        let packager = packager.clone();
        flatpak_button.connect_clicked(move |button| {
            button.set_sensitive(false);
            let spinner = gtk::Spinner::new();
            spinner.start();
            button.set_child(Some(&spinner));
            // Manifest may have been created since startup (e.g. a ghost).
            packager.rediscover();
            let packager = packager.clone();
            runtime().spawn(async move {
                // Failures surface as a toast via the FlatpakState event.
                let _ = packager.build_install_launch(true).await;
            });
        });
    }
    header.pack_end(&flatpak_button);

    // --- gadget mode: the window is the monitor ---------------------------
    // ENVIRONMENTS.md → "Gadget mode". The panes and the gadget's container
    // are two children of one stack, swapped by an AdwBreakpoint. A stack
    // rather than a rebuild because the panes must survive the trip: the
    // commitment is ONE window whose layout is never rearranged, and a
    // gadget that tore the editor down and put it back would be a
    // rearrangement with extra steps.
    //
    // The gadget draws nothing of its own any more: it is where the
    // environment panel and the backlog GO while the window is too small
    // for panes, moved rather than copied.
    let gadget = crate::gadget::Gadget::new();
    let surfaces = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .transition_duration(120)
        // NOT homogeneous, in either axis. A GtkStack defaults to
        // requesting enough room for every child at once, which would make
        // the window's minimum width the PANES' minimum even while the
        // card is showing — the window could then never be dragged small
        // enough to reach the breakpoint that shows the card, and the card
        // would be allocated below its own minimum and clipped. (Both
        // observed, in that order, under the Broadway probe.)
        .hhomogeneous(false)
        .vhomogeneous(false)
        .build();
    surfaces.add_named(&outer, Some("panes"));
    surfaces.add_named(&gadget.widget, Some("gadget"));

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.add_top_bar(&banner.widget);
    toolbar_view.set_content(Some(&surfaces));

    // Toasts: transient action outcomes (commit/push/sync failures and the
    // like) surface here via Event::Toast, never only in logs.
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&toolbar_view));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(1440)
        .default_height(900)
        .content(&toast_overlay)
        .build();

    // --- the responsive ladder --------------------------------------------
    // ENVIRONMENTS.md → the responsive ladder. Two breakpoints, and the
    // ORDER of these two blocks is load-bearing: libadwaita applies the
    // LAST breakpoint whose condition matches, and at 400sp BOTH of these
    // match. Added the other way round, the middle rung shadowed gadget
    // mode entirely — a window dragged into a corner kept its panes and
    // merely squeezed them. (Observed, under the probe, as a "gadget"
    // screenshot with an editor in it.)
    //
    // So: widest first, narrowest last. Only one applies at a time, which
    // is also why gadget mode does not inherit the middle rung's setters —
    // it does not need them, having replaced the panes outright.

    // --- the middle rung: one window, half a screen ------------------------
    // Between the full layout and the gadget there is a width where four
    // panes are still wanted and no longer fit as four *columns*: a window
    // tiled beside a browser. Exactly one thing gives way, and it is a
    // consolidation rather than a removal.
    //
    // **The chat column and the console pane stop being panes, and their
    // views become tabs at the end of the editor's strip**, so the window
    // has exactly ONE tab strip in it:
    //
    //   [file 1] … [chat] [usage] [settings] [environment] [resources]
    //   [terminal 1] [terminal 2]
    //
    // That is the whole rung, and the principle under it is **no nested tab
    // sets**: every leaf view is a first-class tab in its region's one
    // strip, and down here there is one region. The chat's own toggle strip
    // hides and its three views become three tabs; the console's tabs are
    // *transferred* pages, so a terminal's pty crosses the breakpoint
    // without noticing. Everything is reparented, never rebuilt — these
    // widgets hold a live transcript, a half-typed prompt and running
    // shells.
    //
    // Nothing is carried across BESIDE the pages. The console briefly had
    // a header above its strip — which environment this is, what it is
    // doing, the review band — which had to be reparented here by hand and
    // hidden again whenever a file was in front. It is deleted: the
    // Environments panel in the flank names the selected environment, and
    // the rest of those facts are the environment tab's own content, which
    // crosses with its page and is on screen exactly when that tab is.
    //
    // **The flank does not move.** It keeps its column, with the
    // Environments panel and the Backlog in it. An earlier version of this
    // rung collapsed it too, which turned the window into a stack of
    // full-width bands and took away the panel that says which environment
    // you are in, at exactly the width where there is least room to say it.
    //
    // Which families the strip carries at a rung is `tabfamily`'s to say,
    // and this applies it: one function for both directions, so growing
    // back is the same code path read the other way and cannot forget half
    // of what shrinking did.
    let set_rung: std::rc::Rc<dyn Fn(crate::tabfamily::Rung)> = {
        let editor = editor.clone();
        let chats = chats.clone();
        let console = console.clone();
        let paned = center_and_chat.clone();
        let center = center.clone();
        std::rc::Rc::new(move |rung| {
            let families = crate::tabfamily::strip_families(rung);
            let want_chat = families.contains(&Family::Chat);
            let want_console = families.contains(&Family::Console);
            // Guarded on what the strip already holds rather than on the
            // rung: AdwBreakpoint fires `apply` on the breakpoint being
            // added as well as on the window being resized.
            if want_chat && !editor.holds_family(Family::Chat) {
                // The pane is unparented HERE rather than in the editor,
                // because the editor does not know what it was a child of
                // and should not have to.
                let faces = chats.graft_faces();
                paned.set_end_child(gtk::Widget::NONE);
                editor.graft(
                    Family::Chat,
                    &[
                        GraftedTab {
                            widget: faces.chat,
                            title: "Chat".into(),
                            icon: "taste-chat-symbolic".into(),
                            tooltip: "The selected environment's conversation".into(),
                        },
                        GraftedTab {
                            widget: faces.usage,
                            title: "Usage".into(),
                            icon: "taste-utilization-symbolic".into(),
                            tooltip: "How much room is left in this conversation".into(),
                        },
                        GraftedTab {
                            widget: faces.settings,
                            title: "Agent".into(),
                            icon: "emblem-system-symbolic".into(),
                            tooltip: "This conversation's agent and session settings".into(),
                        },
                    ],
                );
                // ...and now that the tab exists, the tint on it is the
                // conversation's own rather than the graft's placeholder.
                chats.republish_usage_severity();
            } else if !want_chat && editor.holds_family(Family::Chat) {
                editor.ungraft(Family::Chat);
                chats.ungraft_faces();
                paned.set_end_child(Some(&chats.widget));
            }
            if want_console && !editor.holds_family(Family::Console) {
                // The console's pages move as PAGES: they already exist,
                // and one of them holds a running pty.
                // `begin_migration` takes the pins off the console's three
                // fixtures for the crossing itself, so a transfer never has
                // to have an opinion about which section a page is in;
                // `set_host` pins them again on arrival, because a pane's
                // tab is icon-only and unclosable in whichever strip it is
                // in, and pinning is how AdwTabBar renders that.
                console.begin_migration();
                let pages = console.strip_pages();
                let from = console.own_view();
                center.set_end_child(gtk::Widget::NONE);
                editor.graft_pages(Family::Console, &from, &pages);
                console.set_host(editor.tab_view());
                // The pages moved; the BAR FURNITURE did not, because it
                // cannot — an `AdwTabBar` action widget belongs to the bar,
                // and the console's bar stays behind with the pane. So New
                // Terminal is handed to the strip that is now drawing these
                // tabs, explicitly, here. Forget this half and the button
                // is simply gone below 960px, which is exactly how it ended
                // up buried in a page's content once before.
                editor.attach_end_action(&console.release_new_terminal_button());
            } else if !want_console && editor.holds_family(Family::Console) {
                console.begin_migration();
                editor.ungraft_pages(Family::Console, &console.own_view());
                console.set_host(&console.own_view());
                center.set_end_child(Some(&console.widget));
                editor.detach_end_action(&console.new_terminal_button());
                console.reclaim_new_terminal_button();
            }
        })
    };
    let consolidated_breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        crate::gadget::CONSOLIDATED_MAX_WIDTH_SP,
        adw::LengthUnit::Sp,
    ));
    {
        {
            let set_rung = set_rung.clone();
            consolidated_breakpoint
                .connect_apply(move |_| set_rung(crate::tabfamily::Rung::Consolidated));
        }
        {
            let set_rung = set_rung.clone();
            consolidated_breakpoint
                .connect_unapply(move |_| set_rung(crate::tabfamily::Rung::Full));
        }
        // The indexing gauge beside the box is the first thing this rung
        // has no width for: the title bar's minimum is the window's.
        consolidated_breakpoint.add_setter(
            search.indexing_box(),
            "visible",
            Some(&false.to_value()),
        );
        window.add_breakpoint(consolidated_breakpoint.clone());
    }

    // Below the breakpoint the panes give way to the card, the deploy
    // button and the safe-mode banner go with them (neither is a thing you
    // act on from a monitor), and the header says what is being watched.
    // Every setter is restored when the window grows back — that is
    // AdwBreakpoint's contract, and it is what makes "stretch back to the
    // IDE, nothing rearranged" true rather than aspirational.
    let gadget_breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        crate::gadget::GADGET_MAX_WIDTH_SP,
        adw::LengthUnit::Sp,
    ));
    {
        let breakpoint = gadget_breakpoint.clone();
        breakpoint.add_setter(&surfaces, "visible-child-name", Some(&"gadget".to_value()));
        breakpoint.add_setter(&banner.widget, "visible", Some(&false.to_value()));
        breakpoint.add_setter(&flatpak_button, "visible", Some(&false.to_value()));
        // File navigation belongs to the editor, and there is no editor
        // down here.
        breakpoint.add_setter(&editor.back_button, "visible", Some(&false.to_value()));
        breakpoint.add_setter(&editor.forward_button, "visible", Some(&false.to_value()));
        // The search summary's fixed width is what keeps the box still at
        // full size; down here it is the width the 400px window lacks.
        breakpoint.add_setter(search.summary(), "visible", Some(&false.to_value()));
        breakpoint.add_setter(search.indexing_box(), "visible", Some(&false.to_value()));
        breakpoint.add_setter(&title, "subtitle", Some(&"fleet monitor".to_value()));
        {
            // The two panels move house. Two `remove`/`append` pairs, no
            // rebuild, nothing touched on the filesystem — and the panels
            // keep their scroll, their filter text and their sparkline
            // history because the widgets are never taken apart.
            let gadget = gadget.clone();
            let filetree = filetree.clone();
            breakpoint.connect_apply(move |_| {
                if gadget.holding() {
                    return; // already here; AdwBreakpoint can fire twice
                }
                gadget.adopt(filetree.stow_panels());
            });
        }
        {
            let gadget = gadget.clone();
            let filetree = filetree.clone();
            breakpoint.connect_unapply(move |_| {
                filetree.restore_panels(gadget.release());
            });
        }
        window.add_breakpoint(breakpoint);
    }

    // --- the ladder's numbers are the layout's, not a taste ---------------
    // A breakpoint's width is a PROMISE: below this, the rung above is
    // gone. The promise is kept only if the rung above actually fits
    // everywhere it is still in force — and whether it fits is not a
    // constant. It is a sum of the panes' own minimums, and those move with
    // the workspace: the flank's floor carries a branch name and a git
    // status line, and both are as long as the project makes them.
    //
    // Measured, on this repo, with the walk below: the full layout needs
    // 863px against the screenshot fixture and 973px against a real
    // checkout (flank 335 + handle + centre 308 + handle + chat 320), while
    // the breakpoint handed over at 960sp. Between those two numbers is a
    // band the window can be sized into where neither rung fits — the
    // panes are allocated below their minimums and the last one in the row,
    // the chat, is cut off the right edge. That is the bug David reported,
    // and a hand-picked 960 could not have been right: nothing was checking
    // it against the arithmetic, so the two drifted apart silently.
    //
    // **The window's own minimum does not defend against this**, and that
    // is by design rather than a fault to find: a window with breakpoints
    // reports the minimum of its NARROWEST configuration (measured: 360px,
    // which is the gadget card's), because otherwise it could never be
    // dragged small enough to reach the rung that needs less room. So
    // "some rung fits at every width" cannot come from the minimum. It has
    // to come from each breakpoint handing over at or above the width its
    // own rung stops fitting at, which is what this does:
    //
    //   consolidate at max(960sp, the full layout's minimum)
    //   go gadget at   max(520sp, the consolidated layout's minimum)
    //
    // The constants stay as FLOORS — they are the taste in this, "half a
    // screen is where I stop wanting two columns" and "a corner of the
    // display is not an IDE" — and the arithmetic raises them when taste
    // and geometry disagree. It cannot lower them.
    //
    // Each rung's minimum is measured while that rung is in force, which is
    // the only time it is measurable: the chat's column is unparented when
    // it is a tab, so the paned's minimum IS the rung's. Both are learned
    // on the way down, in the order they are needed, and remembered.
    // What the two thresholds currently are, in px. Shared out so the
    // width walk can print the numbers it is checking against rather than
    // the constants they were derived from.
    let ladder_thresholds = std::rc::Rc::new(std::cell::Cell::new((0f64, 0f64)));
    {
        let ladder = std::rc::Rc::new(std::cell::Cell::new((0i32, 0i32)));
        let applied = ladder_thresholds.clone();
        let pending = std::rc::Rc::new(std::cell::Cell::new(false));
        let retune: std::rc::Rc<dyn Fn()> = {
            let outer = outer.clone();
            let surfaces = surfaces.clone();
            let editor = editor.clone();
            // The parts the rung below's minimum is predicted from, as
            // widgets: measuring them is all this needs them for.
            let filetree_measure: gtk::Widget = filetree.widget.clone().upcast();
            let center_and_chat_measure: gtk::Widget = center_and_chat.clone().upcast();
            let editor_measure: gtk::Widget = editor.widget.clone().upcast();
            let chat_measure: gtk::Widget = chats.widget.clone().upcast();
            let console_measure: gtk::Widget = console.widget.clone().upcast();
            let consolidated_breakpoint = consolidated_breakpoint.clone();
            let gadget_breakpoint = gadget_breakpoint.clone();
            let ladder = ladder.clone();
            let applied = applied.clone();
            let weak = window.downgrade();
            std::rc::Rc::new(move || {
                let Some(window) = weak.upgrade() else { return };
                // Which rung is in force is asked of the LAYOUT, not of the
                // width: the width is the input the thresholds are being
                // computed from, and reading the rung back out of it would
                // make this circular.
                let min = width_of(&outer);
                let (mut full, mut consolidated) = ladder.get();
                let at_gadget_rung = surfaces.visible_child_name().as_deref() == Some("gadget");
                if at_gadget_rung && (full, consolidated) != (0, 0) {
                    // Nothing to learn here that is not already known
                    // better. The panes are still parented and still
                    // measure — but the flank has lent its two panels to
                    // the gadget, so every number taken down here is short
                    // by whatever those panels' own floor contributed
                    // (measured: 6px). A window that has been wider knows
                    // the real figure and keeps it.
                } else if !at_gadget_rung && editor.holds_family(Family::Chat) {
                    consolidated = min;
                } else {
                    // The full layout, measured — either because it is in
                    // force, or because this is a window that opened
                    // straight into gadget mode and an estimate a few
                    // pixels short beats the constant it would otherwise
                    // use. Either way it is replaced by the real figure the
                    // moment the panes are whole.
                    full = min;
                    // ...and what the rung BELOW would need, before anyone
                    // has been there. Waiting to measure it until it is in
                    // force means the first step into it is taken blind,
                    // and a window arriving at that rung from underneath —
                    // growing out of gadget mode — lands one frame clipped
                    // before the number is known. (Seen in the walk.)
                    //
                    // It is predictable, and exactly: consolidating puts
                    // the chat's faces and the console's pages into the
                    // editor's tab view, and a tab view measures EVERY
                    // page, so its minimum is the widest of them. The flank
                    // does not move. The handle is the paned's own, taken
                    // from the arithmetic rather than from the theme.
                    let handle =
                        min - width_of(&filetree_measure) - width_of(&center_and_chat_measure);
                    consolidated = width_of(&filetree_measure)
                        + handle
                        + width_of(&editor_measure)
                            .max(width_of(&chat_measure))
                            .max(width_of(&console_measure));
                }
                ladder.set((full, consolidated));
                // `sp` is the unit the taste is expressed in — it means the
                // same thing on a HiDPI screen — and px is the unit a
                // measurement comes back in. The comparison has to happen
                // in one of them, so the constants are converted down.
                let settings = window.settings();
                let sp = |value: f64| adw::LengthUnit::Sp.to_px(value, Some(&settings));
                let at_consolidated = sp(crate::gadget::CONSOLIDATED_MAX_WIDTH_SP)
                    .max(f64::from(full))
                    .round();
                // Never above the rung it sits under: a middle rung with no
                // band left would be worse than one that is merely narrow.
                let at_gadget = sp(crate::gadget::GADGET_MAX_WIDTH_SP)
                    .max(f64::from(consolidated))
                    .round()
                    .min(at_consolidated);
                if applied.get() == (at_consolidated, at_gadget) {
                    return;
                }
                applied.set((at_consolidated, at_gadget));
                // Info, not debug: this is the one line that says WHY the
                // window changed rungs at the width it did, and it lands in
                // the app log (`ide_app_log`) where a report can quote it.
                // Guarded above, so a drag logs once per change, not per px.
                tracing::info!(
                    full_min = full,
                    consolidated_min = consolidated,
                    flank_min = width_of(&filetree_measure),
                    center_min = width_of(&center_and_chat_measure) - width_of(&chat_measure),
                    chat_min = width_of(&chat_measure),
                    at_consolidated,
                    at_gadget,
                    "responsive ladder retuned"
                );
                for (breakpoint, width) in [
                    (&consolidated_breakpoint, at_consolidated),
                    (&gadget_breakpoint, at_gadget),
                ] {
                    breakpoint.set_condition(Some(&adw::BreakpointCondition::new_length(
                        adw::BreakpointConditionLengthType::MaxWidth,
                        width,
                        // Px, because that is what the measurement is in.
                        adw::LengthUnit::Px,
                    )));
                }
                // Conditions are read on the next allocation, so ask for
                // one: without this a threshold that just moved past the
                // current width would not take effect until something else
                // happened to invalidate the layout.
                window.queue_resize();
            })
        };
        // Deferred, and by a short timer rather than an idle. The trigger
        // is `default-width`, which GTK sets when the size is ASKED for —
        // before the surface has been reconfigured, before the layout has
        // been allocated at the new size and therefore before the
        // breakpoint that the new size trips has applied. An idle wins that
        // race and measures the rung the window is leaving. (Measured: the
        // walk saw thresholds a step and a half stale, and the one width
        // that clipped was the one no retune had landed on.) A timer loses
        // it on purpose — and its cost is bounded by the guard below, which
        // is what keeps a drag from queueing one of these per pixel.
        let schedule: std::rc::Rc<dyn Fn()> = {
            let retune = retune.clone();
            let pending = pending.clone();
            std::rc::Rc::new(move || {
                if pending.replace(true) {
                    return; // one pass in flight is enough; a drag is many
                }
                let retune = retune.clone();
                let pending = pending.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(60), move || {
                    pending.set(false);
                    retune();
                });
            })
        };
        {
            let schedule = schedule.clone();
            // GTK4 keeps default-width in step with the real size, so this
            // is "the window changed width" — which is both when a rung may
            // have changed and when the answer matters.
            window.connect_default_width_notify(move |_| schedule());
        }
        {
            let schedule = schedule.clone();
            window.connect_map(move |_| schedule());
        }
        // ...and once a second, because a pane's minimum can also grow
        // while the window sits still: a checkout out to a branch with a
        // longer name is a wider flank without a resize to notice it. The
        // measurement is a cached size request on a clean layout, so this
        // costs a comparison; the panel's own refresh already ticks at this
        // rate and this rides with it rather than adding a second clock.
        {
            let schedule = schedule.clone();
            let console = console.clone();
            let editor = editor.clone();
            let environments = environments.clone();
            let port_facts = port_facts.clone();
            let ide_log_cursor = ide_log_cursor.clone();
            let log_activity = log_activity.clone();
            let search_for_tick = search.clone();
            let filetree_weak = Rc::downgrade(&filetree);
            let ticks = Rc::new(std::cell::Cell::new(0u32));
            filetree.set_on_panel_tick(move || {
                console.refresh_fleet();
                schedule();
                // The IDE's own log, to its tab if one is open: the ring
                // has no event, so its follower rides this clock.
                let (cursor, lines) = taste_core::app_log::since(ide_log_cursor.get());
                ide_log_cursor.set(cursor);
                if !lines.is_empty() {
                    log_activity.record(
                        &taste_core::environment::EnvironmentId::primary(),
                        crate::logview::LogKind::Ide,
                        lines.len(),
                    );
                    editor.append_log(
                        &taste_core::environment::EnvironmentId::primary(),
                        crate::logview::LogKind::Ide,
                        &lines,
                    );
                }
                // The Ports section: the selected environment's forwarded
                // ports, with what the last probe said. Every third tick,
                // the probe itself — one connect per port, off the thread.
                if probe_mode {
                    return; // the frames are posed (`seed_*_for_probe`)
                }
                let Some(filetree) = filetree_weak.upgrade() else {
                    return;
                };
                let env = filetree
                    .watching()
                    .unwrap_or_else(taste_core::environment::EnvironmentId::primary);
                // The Logs rows' badges: the query's hits in each log.
                {
                    let standing = search_for_tick.query();
                    let counts: Vec<usize> = if standing.is_empty() {
                        vec![0; crate::logview::LogKind::ALL.len()]
                    } else {
                        let count = |lines: Vec<String>| {
                            lines.iter().filter(|line| standing.matches(line)).count()
                        };
                        crate::logview::LogKind::ALL
                            .iter()
                            .map(|kind| match kind {
                                crate::logview::LogKind::Environment => environments
                                    .get(&env)
                                    .map(|s| count(s.logs_tail(5000)))
                                    .unwrap_or(0),
                                crate::logview::LogKind::Container => environments
                                    .get(&env)
                                    .map(|s| count(s.container_logs_tail(5000)))
                                    .unwrap_or(0),
                                crate::logview::LogKind::Ide => {
                                    count(taste_core::app_log::tail(2000))
                                }
                            })
                            .collect()
                    };
                    filetree.set_log_hits(&counts);
                }
                // The Logs rows' sparklines: the selected environment's
                // logs, and the IDE's own under the primary.
                let primary = taste_core::environment::EnvironmentId::primary();
                let samples: Vec<_> = crate::logview::LogKind::ALL
                    .iter()
                    .map(|kind| {
                        log_activity.samples(
                            if kind.per_environment() {
                                &env
                            } else {
                                &primary
                            },
                            *kind,
                        )
                    })
                    .collect();
                filetree.set_log_activity(&samples);
                let specs = environments
                    .get(&env)
                    .map(|supervisor| supervisor.ports())
                    .unwrap_or_default();
                filetree.set_ports(port_rows(&specs, &env, &port_facts.borrow()));
                let tick = ticks.get().wrapping_add(1);
                ticks.set(tick);
                if !tick.is_multiple_of(3) || specs.is_empty() {
                    return;
                }
                let ports: Vec<u16> = specs.iter().map(|spec| spec.port).collect();
                let filetree_weak = filetree_weak.clone();
                let editor = editor.clone();
                let port_facts = port_facts.clone();
                glib::spawn_future_local(async move {
                    let handle = crate::runtime::runtime().spawn_blocking(move || {
                        ports
                            .into_iter()
                            .map(|port| (port, crate::portview::is_listening(port)))
                            .collect::<Vec<_>>()
                    });
                    let Ok(results) = handle.await else { return };
                    {
                        let mut cache = port_facts.borrow_mut();
                        for (port, listening) in &results {
                            let facts = cache.entry((env.clone(), *port)).or_default();
                            facts.listening = Some(*listening);
                            if !listening {
                                // Nothing behind a closed port, whatever
                                // the last deep look said.
                                facts.process = None;
                                facts.server = None;
                                facts.content_type = None;
                            }
                        }
                    }
                    for (port, _) in &results {
                        if let Some(facts) = port_facts.borrow().get(&(env.clone(), *port)) {
                            editor.set_port_facts(&env, *port, facts);
                        }
                    }
                    if let Some(filetree) = filetree_weak.upgrade() {
                        filetree.set_ports(port_rows(&specs, &env, &port_facts.borrow()));
                    }
                });
            });
        }
    }

    // --- landing on a surface --------------------------------------------
    // Two things point here: a notification's default action, and a click
    // on a gadget row. Both mean the same thing — "take me to the thing
    // that wanted me" — so both go through one function, and the window
    // grows back to a size with panes in it first, because a surface you
    // cannot see is not somewhere you have landed.
    let restore_panes: std::rc::Rc<dyn Fn()> = {
        // Weak: this closure ends up owned by an application action, and
        // the application outlives the window.
        let weak = window.downgrade();
        std::rc::Rc::new(move || {
            let Some(window) = weak.upgrade() else { return };
            // GTK4 keeps default-width/height in step with the real size,
            // so this reads as "how big am I now". A maximized or tiled
            // window is never below the breakpoint and is left alone —
            // the compositor owns its size, not us.
            if crate::tabfamily::Rung::of_width(f64::from(window.default_width()))
                == crate::tabfamily::Rung::Gadget
            {
                window.set_default_size(
                    crate::gadget::RESTORED_WIDTH,
                    window.default_height().max(crate::gadget::RESTORED_HEIGHT),
                );
            }
            window.present();
        })
    };
    let route: std::rc::Rc<dyn Fn(&crate::notify::Surface)> = {
        let restore_panes = restore_panes.clone();
        let chats = chats.clone();
        let aim_for_notice = aim_panes.clone();
        std::rc::Rc::new(move |surface: &crate::notify::Surface| {
            restore_panes();
            match surface {
                // A notification can outlive the chat it came from — the
                // desktop keeps them, and hands the click back whenever.
                // Nothing found is nothing done, not a panic.
                crate::notify::Surface::Chat(key) => {
                    // A chat is reached by going to its environment —
                    // there is one selection, and this is a request to
                    // move it. Nothing found is nothing done: a
                    // notification can outlive the chat it came from.
                    if let Some(env) = chats.environment_for_key(key) {
                        aim_for_notice(Some(env));
                    }
                }
                // Both of the fleet-shaped notices land the same way: on
                // the environment, panes and all. There is nowhere else
                // for them to go now — the backlog row is what carries an
                // environment's light and its actions, and it is the row
                // the panes are aimed from, so "show me this environment"
                // and "select it" are the same gesture.
                crate::notify::Surface::Environment(env) | crate::notify::Surface::Review(env) => {
                    aim_for_notice(Some(env.clone()))
                }
            }
        })
    };
    {
        // The application action a notification's default action names.
        // Application-scoped because that is the only scope the desktop
        // can activate when the app is not running.
        let action =
            gtk::gio::SimpleAction::new(crate::notify::ACTION, Some(glib::VariantTy::STRING));
        let route = route.clone();
        action.connect_activate(move |_, target| {
            let Some(surface) = target
                .and_then(glib::Variant::str)
                .and_then(crate::notify::Surface::parse)
            else {
                return; // a target from a stale or foreign notification
            };
            route(&surface);
        });
        app.add_action(&action);
    }
    {
        // Choosing an environment from a window that has no panes in it is
        // a request for the panes back. The gadget shows the environment
        // panel itself now, so the row's own hook is the one that fires —
        // it is re-registered here, wrapped, because `restore_panes` needs
        // the window and the window did not exist when it was first set.
        //
        // Safe to call unconditionally: `restore_panes` grows a window
        // that is below the breakpoint and leaves every other one alone.
        let restore = restore_panes.clone();
        let aim_for_panel = aim_panes.clone();
        filetree.set_on_open_environment(move |env| {
            restore();
            aim_for_panel(Some(env));
        });
        // ...and nothing equivalent for the backlog: a queue row no longer
        // aims the panes anywhere. Choosing an environment is the panel's
        // job, and it is the surface that says what each one is doing.
    }

    // --- the fleet, published --------------------------------------------
    // The fleet's rows as JSON, refreshed on every publish: what
    // `issue_list` and `issue_status` answer with (see `orchestration.rs`).
    let fleet_rows = std::rc::Rc::new(std::cell::RefCell::new(
        serde_json::Value::Array(Vec::new()),
    ));

    // One assembly, three renderers. The console assembles (it is the one
    // that has the six sources in hand); gadget mode and the varlink
    // service take what comes out. A probe instance publishes to the card
    // but binds no socket: it is scaffolding, and stealing the real
    // window's socket is exactly the footprint it must not leave.
    let fleet_service = taste_fleetlink::FleetService::new(
        root.to_string_lossy().to_string(),
        taste_fleetlink::Snapshot::default(),
    );
    // ...and a window that does not supervise this folder does not answer
    // for it either. The socket path is derived from the folder, so the
    // other window is already bound there; the bind would refuse anyway,
    // and not attempting it keeps the log honest about why.
    if !probe_mode && supervising {
        let socket = taste_core::environment::fleet_socket_path(&root);
        let service = fleet_service.clone();
        runtime().spawn(async move {
            if let Err(e) = service.serve(socket).await {
                tracing::warn!("fleet service stopped: {e:#}");
            }
        });
    }
    {
        let workspace_name = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "workspace".into());
        // Which window a desktop notification came from. gio ids are per
        // APPLICATION and every taste-ide window is the same application,
        // so without this two windows' notifications replace each other in
        // the shell — see `notify::notification_id`.
        let notify_scope = taste_core::environment::workspace_key(&root);
        let service = fleet_service.clone();
        // What has already been said, so it is not said twice. The bus is
        // coarse on purpose and the fleet republishes freely; only changes
        // are news, and the first sighting of anything is a baseline.
        let digest = std::cell::RefCell::new(crate::notify::Digest::default());
        // Weak tree: the hook is owned by the console, which the tree's
        // column is not, but a strong handle to a pane inside this closure
        // is a cycle waiting to happen either way.
        let filetree_for_notice = std::rc::Rc::downgrade(&filetree);
        let window_for_notice = window.downgrade();
        let fleet_cache = fleet_rows.clone();
        let filetree_for_strip = filetree.clone();
        {
            // The queue itself, to the one surface that draws it. The
            // console reads the ref (that is where the off-thread git
            // passes are) and the backlog renders it, so there is one read
            // per change rather than one per surface.
            let filetree_for_backlog = filetree.clone();
            console.set_on_issues_changed(move |issues| filetree_for_backlog.set_issues(issues));
        }
        console.set_on_fleet_changed(move |rows, open_issues| {
            // The environment panel is a fifth renderer of the same rows:
            // its lights and its names come from the assembly, never from
            // a second read of podman and git.
            filetree_for_strip.set_fleet(rows);
            let snapshot = crate::fleet::snapshot(rows, &workspace_name, open_issues);
            service.publish(snapshot.clone());
            // ...and the same rows, for the agents' issue_list. One
            // assembly, four renderers now; the tools read what the user
            // reads rather than a fifth derivation of podman and git.
            *fleet_cache.borrow_mut() = serde_json::to_value(&snapshot.rows)
                .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));

            // The two fleet-shaped notifications, decided off the same
            // rows every other surface renders — not off a second read of
            // podman and git.
            let (Some(window), Some(filetree)) =
                (window_for_notice.upgrade(), filetree_for_notice.upgrade())
            else {
                return;
            };
            let Some(app) = window.application() else {
                return;
            };
            let attention = crate::notify::Attention {
                window_active: window.is_active(),
                fleet_on_screen: filetree.backlog_on_screen(),
                // No chat moments come through here.
                chat_on_screen: false,
            };
            let mut digest = digest.borrow_mut();
            let mut moments: Vec<crate::notify::Moment> = Vec::new();
            for row in &snapshot.rows {
                let Ok(env) = taste_core::environment::EnvironmentId::parse(&row.environment)
                else {
                    continue;
                };
                if digest.environment_moved(&env, &row.state) && row.state == "failed" {
                    moments.push(crate::notify::Moment::BuildFailed {
                        env,
                        name: row.name.clone(),
                        message: row.detail.clone(),
                    });
                }
            }
            // Environments that have flagged themselves since the last
            // assembly. Read off the FleetRows rather than off a branch
            // list: publishing is a checkpoint and flagging is the
            // submission, and only the second one is news.
            let flagged: Vec<taste_core::environment::EnvironmentId> = rows
                .iter()
                .filter(|row| row.review.flagged())
                .map(|row| row.env.clone())
                .collect();
            for env in digest.newly_flagged(&flagged) {
                let name = rows
                    .iter()
                    .find(|row| row.env == env)
                    .map(crate::backlog::title_of)
                    .unwrap_or_else(|| env.to_string());
                moments.push(crate::notify::Moment::ReadyForReview { env, name });
            }
            drop(digest);
            for moment in moments {
                if let Some(notice) = crate::notify::decide(&moment, &attention, &notify_scope) {
                    crate::notify::send(&app, &notice);
                }
            }
        });
        // What the whole fleet is spending out of, to the two places that
        // draw it: the panel header's gauge, and every chat's utilization
        // tab. One read of the proxy, in the console, as with spend.
        let filetree_for_pool = filetree.clone();
        let chats_for_pool = chats.clone();
        console.set_on_pool_changed(move |pool| {
            // The panel header shows the pool; the chats show the pool
            // and who drew on it. Same assembly, two depths.
            filetree_for_pool.set_quota(&pool.quota);
            chats_for_pool.set_pool(pool);
        });
        // The console already has rows; the hook was not there to hear
        // about them. This first pass is what primes the digest.
        console.republish_fleet();
    }
    {
        // The orchestrator's questions about other chats. The fleet
        // getter re-renders from the console's cached facts (no IO, no
        // podman call) so `issue_list` answers with what is on screen
        // rather than with whatever was last broadcast.
        let console = console.clone();
        let rows = fleet_rows.clone();
        let console_for_find = console.clone();
        let chats_for_find = chats.clone();
        crate::orchestration::attach(
            &workspace,
            chats.clone(),
            environments.clone(),
            std::rc::Rc::new(move || {
                console.republish_fleet();
                rows.borrow().clone()
            }),
            // `ide_find`'s inside half: the panes that hold scrollback and
            // transcripts answer, composed here.
            std::rc::Rc::new(move |query, scope| taste_core::orchestration::FoundInside {
                terminals: console_for_find.find_in_scrollback(query, scope),
                chats: chats_for_find.find_in_transcripts(query, scope),
            }),
        );
    }

    // Debug harness: TASTE_MEASURE_MIN=1 prints every pane's minimum width
    // after first map, then quits. Minimums decide whether GNOME will tile
    // the window to half a screen, so keep them measurable.
    if std::env::var("TASTE_MEASURE_MIN").is_ok() {
        let report: Vec<(&str, gtk::Widget)> = vec![
            ("window", window.clone().upcast()),
            ("filetree", filetree.widget.clone().upcast()),
            ("center(editor+console)", center.clone().upcast()),
            ("editor", editor.widget.clone().upcast()),
            ("console", console.widget.clone().upcast()),
            ("chat", chats.widget.clone().upcast()),
            // The gadget is a surface of its own, and its minimum is what a
            // 400px window is held to below the last breakpoint.
            ("gadget", gadget.widget.clone().upcast()),
        ];
        let app = app.clone();
        window.connect_map(move |_| {
            let report = report.clone();
            let app = app.clone();
            // `TASTE_MEASURE_DELAY_MS` moves the moment: a minimum that
            // grows only after a probe view has posed itself (the gadget
            // taking the backlog, a tab opened after the first frame) is
            // invisible at 400ms and plain at 3000.
            let delay = std::env::var("TASTE_MEASURE_DELAY_MS")
                .ok()
                .and_then(|d| d.parse().ok())
                .unwrap_or(400);
            glib::timeout_add_local_once(std::time::Duration::from_millis(delay), move || {
                for (name, widget) in &report {
                    let (min, natural, _, _) = widget.measure(gtk::Orientation::Horizontal, -1);
                    println!("min-width {name}: min={min} nat={natural}");
                }
                // ...and where each pane's number comes FROM. A pane's
                // minimum is a sum of somebody's floor plus a label that
                // does not ellipsize, and the only way to tell which is
                // which is to walk down and watch the number survive.
                // Every pane, not just the console: the flank's minimum is
                // in the same arithmetic that decides the breakpoints, and
                // it was only ever visible here by accident.
                // `TASTE_MEASURE_FLOOR` moves the reporting threshold.
                let floor: i32 = std::env::var("TASTE_MEASURE_FLOOR")
                    .ok()
                    .and_then(|f| f.parse().ok())
                    .unwrap_or(300);
                // TASTE_MEASURE_NAT=1 attributes the NATURAL width instead:
                // a window with no size of its own opens at its natural
                // width, so a wrapping label that reports its unwrapped
                // text as natural is what makes a fresh window 2000px wide.
                let natural = std::env::var("TASTE_MEASURE_NAT").is_ok();
                fn walk(widget: &gtk::Widget, depth: usize, floor: i32, natural: bool) {
                    let (min, nat, _, _) = widget.measure(gtk::Orientation::Horizontal, -1);
                    let reported = if natural { nat } else { min };
                    if reported >= floor {
                        let name = widget.widget_name();
                        let label = widget
                            .downcast_ref::<gtk::Label>()
                            .map(|l| {
                                format!(" \"{}\"", l.text().chars().take(60).collect::<String>())
                            })
                            .unwrap_or_default();
                        println!(
                            "{}{} [{name}] min={min} nat={nat}{label}",
                            "  ".repeat(depth),
                            widget.type_().name()
                        );
                    }
                    if depth < 14 {
                        let mut child = widget.first_child();
                        while let Some(current) = child {
                            walk(&current, depth + 1, floor, natural);
                            child = current.next_sibling();
                        }
                    }
                }
                for (name, widget) in &report {
                    if *name == "window" {
                        continue;
                    }
                    println!("--- {name}");
                    walk(widget, 0, floor, natural);
                }
                app.quit();
            });
        });
    }

    // Debug harness: TASTE_PROBE_CHECK=1 exercises the agents' UI probe
    // (ide_screenshot / ide_widget_geometry) through the REAL channel and
    // responder after first map, writes the PNGs under /tmp, prints the
    // geometry, then quits. Runs headless under gtk4-broadwayd — see
    // build-aux/headless/broadway-client.py for the full recipe.
    if probe_mode {
        // Colors only show against text: without this the composer
        // screenshot is an empty wash whatever the theme does.
        // The one view name the whole block agrees on, read once.
        let view = std::env::var("TASTE_PROBE_VIEW").unwrap_or_default();
        // Where the panes are aimed, for this view. `watching` is the
        // shot that is about the principle — every pane one environment's
        // — so all of them are aimed together, each through its own
        // stand-in: calm-1 has no clone on this disk, so what is
        // fabricated is the aim, while the locks, the badge, the tint and
        // the scoping are the real ones.
        let probe_env = match view.as_str() {
            "watching" => "i-0007",
            // The coordinator is the primary's chat, so its view is home.
            "orchestrator" => "primary",
            // The review shots are of a FLAGGED environment's work, so the
            // panes have to be aimed at one.
            "review" => "i-0002",
            // A review is read in the user's OWN checkout — that is where
            // published branches land — so this one stays home. The frame
            // says whose branch it is where it matters: the review list's
            // header, the tab's badge and the diff's comparison line.
            "review-diff" => "primary",
            _ => "primary",
        };
        // `TASTE_PROBE_CHAT=none` seeds NO chat, so the shot is of the
        // other face this pane has: an environment nobody has started an
        // agent in, and the invitation that is now the only way to start
        // one by hand.
        //
        // Seeding the chat is what binds it to `probe_env`, and it happens
        // before the transcript for a reason the permission card depends
        // on: that card names where an approval would land, and a card
        // built against an unbound chat cannot.
        if std::env::var("TASTE_PROBE_CHAT").as_deref() != Ok("none") {
            chats.seed_for_probe(probe_env);
        }
        if let Some(pane) = chats.selected() {
            // A half-typed follow-up while a turn is still running — which
            // is also why the send button reads "Queue" rather than "Send".
            pane.seed_composer_for_probe(
                "Also keep the Dirty filter's place while you are in there",
            );
            // A transcript with something in it: the plan/prompt/plan
            // sequence whose card count the geometry dump below is there to
            // check.
            pane.seed_transcript_for_probe();
            // ...and what the orchestrator looks like. The options shade
            // opens only for the view that is about the designation itself,
            // because the shade covers the transcript.
            if view == "orchestrator" {
                pane.seed_orchestrator_for_probe(true);
            }
            // The Utilization tab, which is two questions at once: how
            // much room is left in this conversation, and how much of the
            // subscription every conversation has left between them. The
            // second half is only ever as of the last turn, so the shot
            // has to show that it says so.
            //
            // The consolidated shots take the same numbers with the face
            // left closed: down there the utilization toggle is gone and
            // the only thing that can say a conversation is filling up is
            // the traffic dot on its grafted tab, so a frame of that rung
            // with an empty context window would be a frame of the one
            // state where the badge has nothing to draw.
            if view == "utilization" || view.starts_with("consolidated") {
                pane.seed_utilization_for_probe(view == "utilization");
            }
        }
        // What the file tree looks like aimed somewhere. TASTE_PROBE_VIEW
        // picks which of its multi-environment faces to shoot, because one
        // pane gets one screenshot: `watching` (the default — locks, the
        // panel tinted, git controls disabled), `review` (one environment's
        // branch against the merge base), or the views that leave it at
        // home.
        // `seed_watching_for_probe` aims the TREE directly, and in the
        // running app nothing does that: `aim_panes` moves the tree, the
        // editor and the console together. Seeding only half of it shot a
        // window whose panel said `calm-1` while the console's environment
        // tab still showed `Personal`'s state and log — two surfaces
        // disagreeing about where the panes are, which is the exact
        // failure that deleting the console's second listing (and, later,
        // its own header) was meant to make impossible. `probe_env` is the
        // one answer all of them are aimed with.
        match view.as_str() {
            // The review face of this pane — one environment's branch of
            // record against the merge target, which is where the console's
            // Open Review aims it — is seeded after the first frame instead
            // (see `connect_map` below): the pane's opening status pass
            // settles its filter toggles, and entering a filter is one of
            // the ways out of a review, so a review aimed here is one the
            // pane has already left by the time anything is photographed.
            "review" | "review-diff" => {}
            // The views that are about the primary checkout leave the tree
            // aimed where it starts: watching is a second thing the tree
            // does, not the state it is normally in. That includes
            // `backlog`, whose whole subject is the panel at home:
            // untinted, with "Personal" the selected row.
            "hero" | "backlog" | "backlog-composer" | "dirty" | "search" | "port" => {}
            view if view.starts_with("consolidated") => {}
            _ => filetree.seed_watching_for_probe(probe_env),
        }
        // The Dirty filter, on the checkout's own dirty files: the rows
        // with checkboxes, photographed beside the Logs and backlog rows so
        // the column's leading glyphs and text can be measured on one line.
        if view == "dirty" {
            filetree.seed_dirty_view_for_probe();
        }
        // An editor with code in it. "No Files Open" is an honest empty
        // state and a dishonest screenshot: the pane is the middle of the
        // window and every shot is of a session already under way.
        // Watching is mostly a set of refusals, and the editor's half of it
        // is the badged, read-only tab — so the view that is about watching
        // opens its file as one.
        if view == "watching" {
            editor.seed_watched_owner_for_probe(probe_env);
        }
        let probe_open = {
            let path = workspace.root().join(match view.as_str() {
                // The file the transcript is editing, so the shot reads as
                // one session rather than three unrelated panes.
                "hero" => "crates/taste-app/src/fleet.rs",
                _ => "crates/taste-app/src/filetree.rs",
            });
            path.exists().then(|| {
                // Opening now creates the tab; the jump is re-issued after
                // the first frame, because scrolling a view that has not
                // been realized yet lands on line 1 and stays there.
                editor.open_at(&path, Some(113));
                path
            })
        };
        // A live agent terminal: the console's half of live shells, now a
        // tab of its own in the strip rather than a roster row. Into the
        // environment the panes are aimed at: watching is "open an
        // environment and see its agent work", and a strip with no such
        // tab while the agent works next door is the shot contradicting
        // its own caption.
        //
        // NOT for the review shot, whose environment is flagged and
        // therefore STOPPED. Flagging stops the container; the agent lived
        // in it and died with it, and `Terminals::release_all` takes its
        // roster entry with it on the way out — so a real stopped
        // environment has no agent terminal to show, running or otherwise.
        // The fixture used to seed one anyway, and the frame said "stopped"
        // and "agent terminal · running" at once. A fixture that
        // contradicts the code is a fixture to fix.
        //
        // The review DIFF is not that shot. A review is read in the user's
        // OWN checkout — `probe_env` is "primary" for it — and the primary
        // checkout is running, which its environment tab's state line says.
        // Suppressing its terminals swapped one contradiction for the
        // mirror image of it: a state line reading "running" over a strip
        // with no terminal tab at all. The exclusion belongs to the
        // environment that is stopped, not to every view with "review" in
        // its name.
        if view != "review" {
            // The consolidated shots are where a terminal tab marked
            // exited-with-output gets posed, and the full-width ones are
            // where the agent-owned badge does — a tab cannot show both,
            // since `mark_tab_exited` overwrites the ownership indicator
            // (a dead command has no owner left to mark). So the two
            // facts take one frame each rather than a third being invented
            // for them: `watching` catches the agent's terminal running and
            // badged, `consolidated*` catches one that has ended.
            console.seed_agent_terminal_for_probe(
                &taste_core::environment::EnvironmentId::parse(probe_env)
                    .unwrap_or_else(|_| primary_env.clone()),
                view.starts_with("consolidated"),
            );
        }
        // And a fleet with something in it: one row per environment is
        // what the console's detail now is. The console gets more of
        // the window than it normally has, because a fleet of one row is
        // not what the screenshot is for.
        // The whole fabricated fleet — four environments, plus the
        // primary, which is one under the panel's six-row ceiling, so the
        // list photographs full and not yet scrolling.
        //
        // It is not truncated for any view any more. It used to be, for the
        // console's own list; that list is gone, and a claim whose
        // environment has been truncated out of the fleet renders as "this
        // workspace no longer has it" — an honest rendering of a dishonest
        // fixture.
        console.seed_fleet_for_probe(4);
        // The subscription pool behind that fleet. A probe has no account
        // and never makes a request, so without this every shot would
        // show the honest empty state — which is worth having a shot of,
        // but not in the shots that are about everything else.
        console.seed_quota_for_probe();
        // ...and the console follows the panes, exactly as `aim_panes`
        // makes it. After the fleet seed, because the header reads the row.
        if let Ok(env) = taste_core::environment::EnvironmentId::parse(probe_env) {
            console.note_watching(&env);
        }
        // ...with a terminal in front. Aiming the panes stows the previous
        // environment's shells, and a strip that loses its selected page
        // hands the selection to Resources — which for a fabricated
        // environment is one row about the IDE's own container, and not
        // what any caption about this pane is describing.
        console.select_terminal_for_probe();
        // A queue with something on it, always. It is the backlog panel
        // that draws it now, in the file-tree flank under the environment
        // panel, and it appears in every shot that frames that flank — so
        // there is no view that seeds it and no view that does not.
        console.seed_issues_for_probe();
        // ...and the shot that is ABOUT the backlog has a row's context
        // menu open on it. Reordering is a drag or this menu, and a drag
        // cannot be photographed mid-flight — so the frame would otherwise
        // be missing the half of this panel that does anything. The second
        // row, because it is the one where all four moves are available.
        if view == "backlog" {
            filetree.seed_backlog_actions_for_probe("i-0002");
        }
        // ...and the shot that is about WRITING one opens the composer,
        // which is the panel's other half and is never up by default. Its
        // two fields are the subject: they have to read as one form.
        if view == "backlog-composer" {
            // The New issue composer, half-written, in the slot under the
            // list. Edit opens the same slot with a Save pill, so one frame
            // says where both live.
            filetree.seed_backlog_composer_for_probe();
        }
        // The one query, posed: a word that is in file names, file contents,
        // definitions, the backlog and a branch, so every surface has
        // something to answer.
        if view == "search" {
            search.seed_for_probe("gauge");
            // ...and what the semantic index would add: the gauge mid-build
            // beside the box, and three places found by meaning — one in
            // the file on screen, two elsewhere — so the frame shows both
            // the ≈ badges and the "By meaning" group. Fixtures, because
            // the probe builds no index (`semantic::Keeper` stands down
            // under TASTE_PROBE_CHECK).
            search.seed_indexing_for_probe();
            let hit = |rel: &str, start: u32, end: u32, score: f32, text: &str| {
                crate::search::MeaningHit {
                    path: root.join(rel),
                    start_line: start,
                    end_line: end,
                    score,
                    text: text.to_string(),
                }
            };
            let hits = vec![
                hit(
                    "crates/taste-app/src/filetree.rs",
                    1171,
                    1210,
                    0.74,
                    "/// The header's count, gauge and actions",
                ),
                hit(
                    "crates/taste-app/src/coordinator.rs",
                    1,
                    40,
                    0.72,
                    "//! The coordinator's tools and what they cost the allowance",
                ),
                hit(
                    "docs/ENVIRONMENTS.md",
                    1141,
                    1180,
                    0.69,
                    "Model choice per level is ACP session config",
                ),
            ];
            filetree.set_meaning_hits(hits.clone());
            editor.set_meaning_hits(hits);
        }
        // The tree's Logs and Ports sections have rows in every frame; the
        // `port` view is the port tab itself, on its REST face, at work.
        filetree.seed_ports_for_probe();
        filetree.seed_log_activity_for_probe();
        if view == "port" {
            let primary = taste_core::environment::EnvironmentId::primary();
            editor.open_port(
                &primary,
                taste_devcontainer::config::PortSpec {
                    port: 3000,
                    label: Some("App".into()),
                    protocol: None,
                },
                PortFacts::default(),
            );
            editor.seed_port_for_probe(&primary, 3000);
        }
        // Pane geometry, per view. A probe window is smaller than a real one
        // and the panes' natural sizes do not divide it the way a person
        // would, so each shot says what it is of: the hero balances all four,
        // the fleet view gives the console the room a fleet needs to be a
        // list rather than a row and a half.
        // Editor/console split. The console is the fleet view here, and a
        // fleet of one visible row is not what that shot is for, so the
        // fleet view gives it the height a list needs; the hero keeps the
        // editor dominant and still clears three rows.
        center.set_position(match view.as_str() {
            // The review DIFF shot is of the editor: the comparison bar,
            // the judgment row under it, the badged tab and the hunks. That
            // wants the height, and the console has nothing to say about a
            // review any more.
            "review-diff" => 560,
            // The review LIST is the flank's, and the editor beside it is
            // where the reading happens; the console keeps a strip, because
            // a flagged environment is a stopped one with no terminals to
            // show and half a frame of that says nothing.
            "review" => 600,
            // The console used to be handed the height a LIST needs,
            // because it listed every environment. It does not any more —
            // the backlog enumerates them and the console is the machine
            // room — so the editor takes the room back rather than the
            // shot framing an empty half-pane.
            "hero" => 430,
            // The port tab is the editor's: a request, a schema and a
            // response want the height.
            "port" => 600,
            _ => 300,
        });
        // The horizontal dividers are deliberately NOT set: the tree's width
        // follows its own git columns and the chat pane has a minimum it
        // clips below, so a hand-picked position is a guess that goes wrong
        // the moment either changes. Letting the panes take their natural
        // widths is what a real window does anyway.
        // TASTE_PROBE_VIEW=gadget shrinks the window past the breakpoint
        // instead of forcing the stack's child, so what the screenshot
        // shows is the real transition and not a pose of it.
        let gadget_probe = view == "gadget";
        // Both backlog shots are of the same pane, and want the same
        // targets and the same geometry — they differ only in what is open
        // inside it.
        let backlog_probe = view == "backlog" || view == "backlog-composer" || view == "dirty";
        let review_probe = view == "review";
        let review_diff_probe = view == "review-diff";
        // The middle rung, shot at a real width rather than posed: the
        // window is made narrow enough to trip the breakpoint, and what
        // the frame shows is the transition the breakpoint actually
        // performs.
        // Two frames of the one strip, because it carries two families
        // and a tab shows one of them: `consolidated` is posed on the
        // chat, `consolidated-console` on the environment's sections,
        // where the environment tab's own content — state, actions, review
        // banner, log — is what the frame has to show.
        let consolidated_probe = view.starts_with("consolidated");
        // The utilization shot is of one pane, like the panel's own: a
        // window shot at this size cannot be read, and what has to be
        // legible here is a list of sentences.
        let utilization_probe = view == "utilization";
        if utilization_probe {
            center_and_chat.set_shrink_start_child(true);
            center_and_chat.set_position(180);
        }
        if gadget_probe {
            // Tall enough for the panels and no taller: the point of the
            // gadget is a window with nothing spare in it.
            window.set_default_size(400, 500);
        }
        if consolidated_probe {
            // Between the two breakpoints: below CONSOLIDATED_MAX_WIDTH_SP
            // so the chat and the console become tabs, and well clear of
            // GADGET_MAX_WIDTH_SP so every pane stays where it is.
            //
            // Inside the band rather than at the top of it: this used to be
            // shot at 955 because the centre ran off the right edge and a
            // wider frame lost less of it (`chat_column` has the
            // measurements). It fits now, so the shot can sit where the
            // rung is actually used — a window beside a browser — and 900
            // still clears the floor the three panes' own minimums put
            // under it (see the note on CONSOLIDATED_MAX_WIDTH_SP).
            window.set_default_size(900, 760);
        }
        // ...and any view can be posed at a width of the caller's choosing.
        // A breakpoint's rung is a BAND, not a width: what fits at 955 can
        // still run off the edge at 600, so checking one is walking it, and
        // a geometry dump at a single point in it proves nothing about the
        // rest. Probe-only, and unset in every recipe that takes a shot.
        if let Some(width) = std::env::var("TASTE_PROBE_WIDTH")
            .ok()
            .and_then(|w| w.parse::<i32>().ok())
        {
            let height = std::env::var("TASTE_PROBE_HEIGHT")
                .ok()
                .and_then(|h| h.parse::<i32>().ok())
                .unwrap_or_else(|| window.default_height());
            window.set_default_size(width, height);
        }
        // The orchestrator view is about the chat pane's own controls and
        // its tab strip, so it gets most of the width — by moving the
        // divider rather than by growing a window the display may not grant.
        if view == "orchestrator" {
            // Shrink first: the divider will not pass the editor+console
            // minimum otherwise, which is what pins the chat pane to its
            // own minimum in this harness.
            center_and_chat.set_shrink_start_child(true);
            center_and_chat.set_position(180);
        }
        let ui = workspace.ui.clone();
        let app = app.clone();
        let editor_for_probe = editor.clone();
        let view_for_open = view.clone();
        let filetree_for_probe = filetree.clone();
        let outer_for_probe = outer.clone();
        // The panes whose right edges have to land inside the frame, in the
        // order they sit in: see the fit check after the geometry dump.
        //
        // The chat column is in this list because it is the pane that was
        // reported clipped and the one this check was blind to: it sits at
        // the END of the row, which is where an overflowing layout puts its
        // overflow, so leaving it out checked every pane except the only one
        // that could fail.
        let panes_for_fit: Vec<(&'static str, gtk::Widget)> = vec![
            ("filetree", filetree.widget.clone().upcast()),
            ("editor", editor.widget.clone().upcast()),
            ("console", console.widget.clone().upcast()),
            ("chat", chats.widget.clone().upcast()),
        ];
        // `TASTE_PROBE_WALK=520-1500[:25]` walks the ladder instead of
        // shooting a view — see `width_walk`. It installs its own handler,
        // and the screenshot handler below stands down.
        let walking = std::env::var("TASTE_PROBE_WALK").ok();
        if let Some(spec) = &walking {
            width_walk(
                &window,
                panes_for_fit.clone(),
                outer.clone().upcast(),
                ladder_thresholds.clone(),
                spec,
                &app,
            );
        }
        let walking = walking.is_some();
        window.connect_map(move |window| {
            if walking {
                return; // the walk owns this window's size
            }
            let window = window.clone();
            let panes_for_fit = panes_for_fit.clone();
            let ui = ui.clone();
            let app = app.clone();
            let probe_open = probe_open.clone();
            let editor_for_probe = editor_for_probe.clone();
            let view_for_open = view_for_open.clone();
            let filetree_for_probe = filetree_for_probe.clone();
            let outer_for_probe = outer_for_probe.clone();
            // Nothing to open: the panel is permanent, which is the whole
            // point of the shot. It gets fabricated activity instead, so
            // the sparklines have five minutes of history a two-second-old
            // probe window could not have earned — in EVERY view that
            // frames the panel, not just the one that is about it. The
            // hero's panel drawn from a three-second-old process is three
            // rows and one tick, which photographs a feature in its
            // degenerate state; the fleet, the transcript and the agent
            // terminal beside it are fabricated for exactly this reason.
            {
                use crate::backlog::Shape;
                filetree_for_probe.seed_activity_for_probe(&[
                    // The user's own checkout: they have been editing, so
                    // it is alive but not the busiest thing on screen.
                    ("primary", Shape::Editing),
                    // An agent mid-task in a container that is up.
                    ("i-0007", Shape::Working),
                    // A container building: a burst per step, gaps between.
                    ("i-0005", Shape::Building),
                    // Stopped, and therefore silent. The row that proves a
                    // sparkline can be honestly empty.
                    ("i-0002", Shape::Silent),
                    // Up, and stopped on a question. It used to draw the
                    // same busy shape as calm-1, which made the panel two
                    // identical lines and left the hardest case — a row
                    // with almost nothing in it — out of every frame.
                    // Waiting is what the row's amber dot already says.
                    ("i-0004", Shape::Waiting),
                ]);
            }
            // `TASTE_PROBE_ROUNDTRIP=1`: pose the view at its width, let
            // the rung apply, then grow the window back and shoot THAT.
            //
            // "Stretch back to the IDE, nothing rearranged" is a
            // commitment, and the only way to check it is to make the trip:
            // the panes have to come back as panes, the chat's toggle strip
            // has to return, the console's tabs have to come home to their
            // own strip with the section the user was reading still
            // selected, and a half-typed prompt has to still be half-typed.
            // Early, so there are seconds of frames after the resize rather
            // than one — a window shot immediately after an X11 resize is a
            // paintable that has not been drawn into yet, which photographs
            // as a uniform slab.
            if std::env::var("TASTE_PROBE_ROUNDTRIP").is_ok() {
                let w = window.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(200), move || {
                    w.set_default_size(1440, 900)
                });
            }
            // Long enough for the FIRST frame, not just for the jump. On a
            // workspace with real git state the tree's index build pushes
            // that frame past a second, and WidgetPaintable serves the last
            // frame DRAWN — so a shot taken too early is not an error the
            // retry loop can see, it is a uniform slab of window background.
            // Generous rather than tight on purpose: this budget is only
            // ever spent by a harness that is about to quit, and at 1800ms
            // the hero came back blank about one run in three.
            glib::timeout_add_local_once(std::time::Duration::from_millis(2600), move || {
                // Once frames have rendered, the jump lands where it was
                // asked to: re-issuing on an already-open page only scrolls
                // it, and a view that has never been laid out scrolls to
                // line 1 and stays there.
                // The port view keeps its port tab in front: the file is
                // there to show a port tab is a tab among files, not to be
                // what the frame is of.
                if let Some(path) = probe_open.filter(|_| view_for_open != "port") {
                    editor_for_probe.open_at(&path, Some(113));
                }
                if view_for_open == "port" {
                    editor_for_probe.select_port_for_probe(
                        &taste_core::environment::EnvironmentId::primary(),
                        3000,
                    );
                }
                // The search frame shows a hit selected and highlighted in
                // the file: step once the re-open above has re-listed.
                if view_for_open == "search" {
                    let editor = editor_for_probe.clone();
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(200),
                        move || {
                            editor.step_results();
                        },
                    );
                }
                // ...and, for the shot that is about consolidation, the
                // chat tab in front. Opening the file above selected its
                // own tab, and a frame of the editor with a small unopened
                // icon beside it does not show what the icon IS.
                if view_for_open.starts_with("consolidated") {
                    if view_for_open == "consolidated-console" {
                        // The console family is [resources] [user shell]
                        // [agent terminal] at this rung as at every other,
                        // and the LAST of them is what the frame is judged
                        // on: the agent's terminal, marked exited, with its
                        // output still on screen. That is the fact this
                        // rung has to get right — a grafted page keeps
                        // everything it had — and Resources on a
                        // fabricated environment is an honest empty state
                        // and a poor frame.
                        for offset in [2, 1, 0] {
                            if editor_for_probe.select_console_tab(offset) {
                                break;
                            }
                        }
                    } else {
                        editor_for_probe.select_chat_tab();
                    }
                    // ...and the flank at the width it has at full size.
                    // Set here rather than at build time: a GtkPaned
                    // position asked for before the children are realized
                    // is recomputed from their natural sizes on the first
                    // allocation, which in a 900px window hands the flank
                    // nearly half the frame and pushes the tabbed area off
                    // the right edge. The point of the shot is that the
                    // flank is UNCHANGED and the middle gained the chat
                    // column's room.
                    // (This said 280 and was clamped up to the flank's
                    // minimum, which is how it ever matched the hero.)
                    outer_for_probe.set_position(FLANK_OPENING_WIDTH);
                }
                // The review is aimed HERE, not with the other seeds: the
                // pane's first status pass settles its filter toggles, and
                // entering a filter is one of the ways out of a review — so
                // a review opened before that lands is a review the pane
                // has already left by the time anything is photographed.
                if view_for_open == "review" || view_for_open == "review-diff" {
                    filetree_for_probe.seed_review_for_probe("agents/i-0002", "main");
                }
                // The hero frame also proves a rebuild keeps an open
                // folder open: `crates` is expanded the way a click does
                // it, then the tree is rebuilt the way a file change does
                // it, and the shot 700ms on shows whether it is still open.
                // Every rebuild used to start from a collapsed model. (It
                // rode the `fleet` view until that view's subject — the
                // console's environment detail — was dissolved.)
                if view_for_open == "hero" {
                    filetree_for_probe.expand_for_probe("crates");
                    let filetree = filetree_for_probe.clone();
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(300),
                        move || {
                            filetree.refresh_tree();
                        },
                    );
                }
                // ...and the diff one of its rows opens, in front, because
                // the file tab opened above took the selection.
                if view_for_open == "review-diff" {
                    editor_for_probe.open_review_diff(
                        std::path::Path::new("crates/taste-app/src/fleet.rs"),
                        "agents/i-0002",
                        "main",
                    );
                }
                glib::spawn_future_local(async move {
                    use taste_core::ui_probe::{UiReply, UiRequest};
                    // Let the jump above land before anything is shot.
                    glib::timeout_future(std::time::Duration::from_millis(700)).await;
                    let targets: &[&str] = if gadget_probe {
                        // One window, one layout: below the breakpoint
                        // there are no panes to shoot.
                        &["window", "gadget"]
                    } else if backlog_probe {
                        &["filetree", "filetree.backlog", "filetree.backlog-menu"]
                    } else if consolidated_probe {
                        // The whole window: the point of this one is what
                        // the LAYOUT does, and a pane out of it says
                        // nothing about that.
                        &["window"]
                    } else if review_probe {
                        // The flank, where the review's file list is, and
                        // the window — because the backlog row's accent
                        // rail on the same environment is the other half of
                        // it. Nothing about a review is in the console.
                        &["window", "filetree"]
                    } else if review_diff_probe {
                        // The whole window: the review list in the flank
                        // and the diff it opened are one gesture, and the
                        // editor alone would not show where the tab came
                        // from.
                        &["window", "editor"]
                    } else if utilization_probe {
                        &["chat"]
                    } else {
                        &[
                            "window",
                            "chat",
                            "chat.composer",
                            "filetree",
                            // The console, showing the seeded agent
                            // terminal: live shells are a console feature,
                            // and the window shot is too small to read a
                            // tab in.
                            "console",
                            "no-such-pane",
                        ]
                    };
                    for target in targets.iter().copied() {
                        // "Not drawn yet" is timing, not failure: retry the
                        // way an agent would, briefly.
                        for attempt in 0..10 {
                            let request = UiRequest::Screenshot {
                                target: target.into(),
                            };
                            match ui.request(request).await {
                                Ok(UiReply::Screenshot { png, width, height }) => {
                                    let path =
                                        format!("/tmp/probe-{}.png", target.replace('.', "-"));
                                    let _ = std::fs::write(&path, &png);
                                    println!(
                                        "screenshot {target}: {width}x{height}, {} bytes -> {path}",
                                        png.len()
                                    );
                                    break;
                                }
                                Ok(UiReply::Error(e)) if e.contains("not been drawn") => {
                                    if attempt == 9 {
                                        println!("screenshot {target}: ERROR {e}");
                                    } else {
                                        glib::timeout_future(std::time::Duration::from_millis(500))
                                            .await;
                                        continue;
                                    }
                                }
                                Ok(UiReply::Error(e)) => {
                                    println!("screenshot {target}: ERROR {e}");
                                    break;
                                }
                                Ok(_) => {
                                    println!("screenshot {target}: unexpected reply");
                                    break;
                                }
                                Err(e) => {
                                    println!("screenshot {target}: channel error {e}");
                                    break;
                                }
                            }
                        }
                    }
                    let geometry: &[&str] = if gadget_probe {
                        &["gadget"]
                    } else if backlog_probe {
                        &["filetree", "filetree.backlog", "filetree.backlog-menu"]
                    } else if consolidated_probe {
                        // What the middle rung claims: the flank is still
                        // there and still a column, the console is still
                        // under the editor, and the editor — now holding
                        // the chat and the console as tabs — has the rest. The
                        // window too, because those three only add up to
                        // the claim if they add up to IT.
                        &["window", "filetree", "editor", "console"]
                    } else {
                        &["chat.composer", "chat", "console"]
                    };
                    for target in geometry.iter().copied() {
                        let request = UiRequest::Geometry {
                            target: target.into(),
                        };
                        match ui.request(request).await {
                            Ok(UiReply::Geometry(value)) => {
                                println!(
                                    "geometry {target}:\n{}",
                                    serde_json::to_string_pretty(&value).unwrap_or_default()
                                );
                            }
                            Ok(UiReply::Error(e)) => println!("geometry {target}: ERROR {e}"),
                            Ok(_) => println!("geometry {target}: unexpected reply"),
                            Err(e) => println!("geometry {target}: channel error {e}"),
                        }
                    }
                    // Does it FIT? A pane whose right edge is past the
                    // window's is a pane the user cannot see the end of,
                    // and no screenshot of a rung is honest without the
                    // answer. Printed for every view, because the fault it
                    // catches — a pane demanding a width the window does
                    // not have — is a property of the panes, not of the
                    // rung, and the middle one is only where it showed
                    // first. `fit ... OFF-WINDOW` is a failure.
                    //
                    // The window's own bounds rather than its width: the
                    // two differ by the shadow, and the panes are measured
                    // in the coordinate space the bounds are in.
                    let frame = window
                        .compute_bounds(&window)
                        .map_or(f32::MAX, |bounds| bounds.x() + bounds.width());
                    for (name, pane) in &panes_for_fit {
                        // Below the gadget breakpoint the panes are not on
                        // screen at all, and a pane nobody can see keeps
                        // whatever allocation it had last.
                        if !pane.is_mapped() {
                            continue;
                        }
                        let Some(bounds) = pane.compute_bounds(&window) else {
                            continue;
                        };
                        let right = bounds.x() + bounds.width();
                        let verdict = if right <= frame + 0.5 {
                            "ok"
                        } else {
                            "OFF-WINDOW"
                        };
                        println!("fit {name}: right={right:.0} window={frame:.0} {verdict}");
                    }
                    app.quit();
                });
            });
        });
    }

    // Returning to the window clears informational notifications (turn
    // finished, disconnect); ones still awaiting a response (permission,
    // sign-in) stay until actually resolved.
    {
        let chats = chats.clone();
        window.connect_is_active_notify(move |window| {
            if window.is_active() {
                chats.withdraw_informational();
            }
        });
    }

    // Stock editor shortcuts: Ctrl+W closes the current tab, Ctrl+F
    // focuses find-in-project.
    {
        let shortcuts = gtk::ShortcutController::new();
        shortcuts.set_scope(gtk::ShortcutScope::Global);
        let editor_for_close = editor.clone();
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>w"),
            Some(gtk::CallbackAction::new(move |_, _| {
                editor_for_close.close_current();
                glib::Propagation::Stop
            })),
        ));
        // Voice, from anywhere: Ctrl+Shift+M dictates into the selected
        // chat's field, Ctrl+Shift+I into the backlog's new-issue field.
        // Each press toggles — start, then stop and transcribe — because a
        // shortcut has no release to hold.
        let chats_for_dictation = chats.clone();
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control><Shift>m"),
            Some(gtk::CallbackAction::new(move |_, _| {
                if let Some(pane) = chats_for_dictation.selected() {
                    pane.toggle_dictation();
                }
                glib::Propagation::Stop
            })),
        ));
        let filetree_for_dictation = filetree.clone();
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control><Shift>i"),
            Some(gtk::CallbackAction::new(move |_, _| {
                filetree_for_dictation.toggle_issue_dictation();
                glib::Propagation::Stop
            })),
        ));
        // Ctrl+F is the search box — and so is Ctrl+P, for the hand that
        // learned quick-open: file names are the first thing the one query
        // filters, and Enter on the tree opens the selected row.
        for chord in ["<Control>f", "<Control>p"] {
            let search_for_focus = search.clone();
            shortcuts.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(chord),
                Some(gtk::CallbackAction::new(move |_, _| {
                    search_for_focus.focus();
                    glib::Propagation::Stop
                })),
            ));
        }
        // Ctrl+Shift+E: the keyboard's way into the environment panel.
        // Nothing opens any more — the list is permanent — so this focuses
        // the row the panes are aimed at and walks down on repeat presses;
        // Enter is what switches. Shifted because a bare Ctrl+E is
        // readline's end-of-line and this controller is global — a terminal
        // tab would lose it.
        let filetree_for_envs = filetree.clone();
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control><Shift>e"),
            Some(gtk::CallbackAction::new(move |_, _| {
                filetree_for_envs.focus_environment_panel();
                glib::Propagation::Stop
            })),
        ));
        // Ctrl+Q: quit through the graceful close path.
        let window_for_quit = window.downgrade();
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>q"),
            Some(gtk::CallbackAction::new(move |_, _| {
                if let Some(window) = window_for_quit.upgrade() {
                    window.close();
                }
                glib::Propagation::Stop
            })),
        ));
        window.add_controller(shortcuts);
    }

    // Primary-menu actions.
    {
        let about = gtk::gio::SimpleAction::new("about", None);
        let window_ref = window.clone();
        about.connect_activate(move |_, _| {
            let dialog = adw::AboutDialog::builder()
                .application_name("Taste")
                .application_icon(crate::APP_ID)
                .version(env!("CARGO_PKG_VERSION"))
                .developer_name("David Strauss")
                .comments("In an era of AI software authoring, all that's left is taste.")
                .build();
            dialog.present(Some(&window_ref));
        });
        window.add_action(&about);

        let shortcuts_action = gtk::gio::SimpleAction::new("shortcuts", None);
        let window_ref = window.clone();
        shortcuts_action.connect_activate(move |_, _| {
            present_shortcuts_dialog(&window_ref);
        });
        window.add_action(&shortcuts_action);
    }

    // Display facts for ide_environment: which backend, and whether the
    // theme is dark — tracked live, because an agent reasoning about a
    // screenshot needs to know which palette it is looking at.
    {
        let ide = workspace.ide.clone();
        let style = adw::StyleManager::default();
        let publish = move |style: &adw::StyleManager| {
            // "GdkWaylandDisplay" → "wayland"; unknown backends pass
            // through verbatim rather than pretending to be known.
            let backend = gtk::gdk::Display::default()
                .map(|display| {
                    let name = display.type_().name().to_string();
                    name.strip_prefix("Gdk")
                        .and_then(|n| n.strip_suffix("Display"))
                        .map(str::to_lowercase)
                        .unwrap_or(name)
                })
                .unwrap_or_else(|| "none".into());
            ide.set_display(taste_core::ide_state::DisplayFacts {
                backend,
                dark: style.is_dark(),
            });
        };
        publish(&style);
        style.connect_dark_notify(publish);
    }

    // Agents' eyes and hands on the UI: the probe responder behind
    // ide_screenshot and ide_widget_geometry (pane names here are the
    // tools' contract), plus the editor's live buffers behind ACP
    // fs/read_text_file and fs/write_text_file — an agent reads what the
    // user SEES, unsaved edits included, and its writes land in the
    // buffer they are looking at rather than behind their back.
    crate::ui_probe::attach(
        &workspace,
        vec![
            ("window", window.clone().upcast()),
            ("filetree", filetree.widget.clone().upcast()),
            ("editor", editor.widget.clone().upcast()),
            ("console", console.widget.clone().upcast()),
            // The whole chat column, tab strip included — what the user
            // sees on the right. "chat.composer" and friends resolve inside
            // the SELECTED tab, because ui_probe searches what is mapped
            // before what is not.
            ("chat", chats.widget.clone().upcast()),
            // Gadget mode's card. Only mapped below the breakpoint, which
            // is exactly when a screenshot of it means anything.
            ("gadget", gadget.widget.clone().upcast()),
        ],
        {
            let editor = editor.clone();
            std::rc::Rc::new(move |path: &std::path::Path| editor.buffer_text(path))
        },
        {
            let editor = editor.clone();
            std::rc::Rc::new(move |path: &std::path::Path, text: &str| {
                editor.buffer_write(path, text)
            })
        },
    );

    // Agent URL bridge: sandboxed sign-in flows (e.g. Claude Code's OAuth)
    // can't open a browser themselves; their $BROWSER helper drops URLs
    // here, and we open them host-side after the user confirms.
    start_url_bridge(&window, &root);

    // --- workspace watcher: external edits become visible ----------------
    // Kept alive for the window's lifetime (leak is deliberate: one window,
    // one process).
    match taste_core::watcher::start(root.clone(), workspace.events.clone()) {
        Ok(watcher) => {
            Box::leak(Box::new(watcher));
        }
        Err(e) => tracing::warn!("workspace watcher failed to start: {e:#}"),
    }

    // Ctrl+C on the launching console (SIGINT) and container stop
    // (SIGTERM) close the window gracefully — the same path as the close
    // button, so state persists.
    {
        let events = workspace.events.clone();
        crate::runtime::runtime().spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let (Ok(mut int), Ok(mut term)) = (
                signal(SignalKind::interrupt()),
                signal(SignalKind::terminate()),
            ) else {
                return;
            };
            loop {
                tokio::select! {
                    _ = int.recv() => {}
                    _ = term.recv() => {}
                }
                events.publish(Event::QuitRequested);
            }
        });
    }

    // --- event pump: tokio-side services → GTK --------------------------
    let events = workspace.events.subscribe();
    {
        let filetree = filetree.clone();
        let console = console.clone();
        let banner = banner.clone();
        let editor = editor.clone();
        let chats = chats.clone();
        let packager = packager.clone();
        let root = root.clone();
        let aim_panes = aim_panes.clone();
        let workspace = workspace.clone();
        let open_log = open_log.clone();
        glib::spawn_future_local(async move {
            while let Ok(event) = events.recv().await {
                match event {
                    Event::GitStatusChanged => {
                        filetree.on_git_status_changed();
                        editor.sync_git_state();
                        semantic_keeper.schedule_refresh();
                        // The issue queue is git state in the user's own
                        // checkout, and every issue tool publishes this
                        // after it writes the ref. No second event, and no
                        // polling: an agent filing or claiming something
                        // moves the queue the user is looking at.
                        console.refresh_issues();
                    }
                    Event::FileChanged(path) => {
                        editor.on_file_changed(&path);
                        filetree.on_git_status_changed();
                        semantic_keeper.schedule_refresh();
                        // The dots are the USER's uncommitted files, read
                        // from the user's own checkout. While the panes are
                        // watching an environment there is a second watcher
                        // over that clone, and every file its agent writes
                        // used to run a full status over this repo — a
                        // question whose answer that file cannot change.
                        // Lexical on purpose: this is a "could this possibly
                        // matter" filter on the main thread, not a policy
                        // decision, and `sync_git_state` is what actually
                        // reads the repository.
                        if path.starts_with(&root) {
                            editor.sync_git_state();
                        }
                    }
                    Event::FileTreeChanged => {
                        filetree.refresh_tree();
                        filetree.rebuild_index();
                        editor.sync_git_state();
                        // A manifest may have appeared (ghost, agent, git
                        // pull): the deploy button follows reality.
                        flatpak_button.set_visible(packager.rediscover().is_some());
                    }
                    Event::OpenFileRequested { path, line } => {
                        editor.open_at(&path, line);
                    }
                    // The devcontainer events name their environment. The
                    // banner, the Containers tab and the file tree's
                    // read-only locks all speak for the PRIMARY environment
                    // — the one the panes are aimed at — so anything from
                    // another environment is dropped here rather than
                    // painted over the primary's. Phase 5 aims these
                    // surfaces at a chosen environment; until then, routing
                    // means filtering.
                    Event::DevcontainerPendingChanges { env, pending } => {
                        // Every environment's drift shows in its fleet row
                        // — the amber light and "needs rebuild" — and that
                        // is the whole announcement; the banner only makes
                        // sure it is not standing for the primary's drift.
                        console.refresh_fleet();
                        if env != primary_env {
                            continue;
                        }
                        banner.on_pending_changes(pending);
                    }
                    Event::DevcontainerState { env, state } => {
                        // Chats route on the environment they are BOUND to,
                        // not on the one the panes are aimed at: a chat in
                        // its own environment moves its agent into that
                        // environment's container when it comes up, and
                        // back out when it goes. This is the only
                        // subscriber here that is not primary-only.
                        chats.on_environment_state(&env, &state);
                        let running = matches!(
                            state,
                            taste_core::event::DevcontainerStateEvent::Running { .. }
                        );
                        // Every environment's row is live; only the primary
                        // drives the banner and the panes.
                        console.on_environment_state(&env, running);
                        if env != primary_env {
                            continue;
                        }
                        banner.on_state(&state);
                        // Mode may have flipped (safe ↔ container): restyle
                        // the tree's read-only locks.
                        filetree.on_git_status_changed();
                    }
                    // Each environment's build output goes to its own log
                    // buffer and its own lifecycle roster row; the panel
                    // shows whichever environment is selected.
                    Event::DevcontainerLog { env, line } => {
                        console.append_env_log(&env, &line);
                        log_activity.record(&env, crate::logview::LogKind::Environment, 1);
                        editor.append_log(
                            &env,
                            crate::logview::LogKind::Environment,
                            std::slice::from_ref(&line),
                        );
                    }
                    Event::ContainerOutput { env, line } => {
                        log_activity.record(&env, crate::logview::LogKind::Container, 1);
                        editor.append_log(
                            &env,
                            crate::logview::LogKind::Container,
                            std::slice::from_ref(&line),
                        );
                    }
                    Event::FlatpakLog(line) => console.append_flatpak_log(&line),
                    Event::FlatpakState(state) => {
                        // Re-arm the deploy button when the pipeline settles.
                        let done = matches!(
                            state,
                            FlatpakStateEvent::Succeeded | FlatpakStateEvent::Failed { .. }
                        );
                        if done {
                            flatpak_button.set_sensitive(true);
                            flatpak_button.set_icon_name("package-x-generic-symbolic");
                        }
                        match state {
                            FlatpakStateEvent::Failed { message } => {
                                console.append_flatpak_log(&format!("FAILED: {message}"));
                                toast_overlay
                                    .add_toast(adw::Toast::new(&format!("Flatpak: {message}")));
                            }
                            FlatpakStateEvent::Succeeded => {
                                toast_overlay
                                    .add_toast(adw::Toast::new("Flatpak installed and launched"));
                            }
                            _ => {}
                        }
                    }
                    Event::RunInTerminal {
                        title,
                        program,
                        args,
                        env,
                        wrapped,
                    } => {
                        console.add_command_tab(&title, &program, &args, &env, wrapped);
                    }
                    // "View Log": the environment's build and lifecycle
                    // stream, opened as the document it is — the same tab
                    // the tree's Logs section opens, tailing while the
                    // build runs.
                    Event::ShowDevcontainerLog => {
                        open_log(primary_env.clone(), crate::logview::LogKind::Environment)
                    }
                    // Coarse by design: the roster says "look again", and
                    // the console opens tabs for shells it has not seen.
                    // Output reaches an open tab through its own watcher,
                    // never through this bus.
                    Event::ShellRosterChanged { env } => console.sync_shell_roster(&env),
                    Event::CreateDevcontainerConfig => {
                        filetree.create_ghost(&root.join(".devcontainer/devcontainer.json"));
                    }
                    Event::CreateFileRequested { path, content } => {
                        editor.open_unsaved(&path, content);
                    }
                    Event::CommandTabExited { title, status } => {
                        if title == "Sign In" {
                            // The sign-in terminal was opened from the chat
                            // the user is in; credentials are per agent, so
                            // the other tabs pick them up on their next
                            // connection anyway.
                            if let Some(pane) = chats.selected() {
                                pane.on_sign_in_finished(status == 0);
                            }
                        }
                    }
                    Event::QuitRequested => {
                        if let Some(window) = editor.widget.root().and_downcast::<gtk::Window>() {
                            window.close();
                        }
                    }
                    Event::OpenUrlRequested(url) => {
                        if !(url.starts_with("https://") || url.starts_with("http://")) {
                            continue; // terminals print all sorts of things
                        }
                        open_url(&url, &toast_overlay);
                    }
                    Event::Toast(message) => {
                        // A probe is a screenshot rig, and the things it
                        // has to complain about are true of the rig
                        // rather than of the app: no podman machine
                        // inside the build container, no session
                        // bus. A banner about the harness across
                        // the bottom of every shot documents the harness.
                        if probe_mode {
                            tracing::debug!("probe: suppressed toast: {message}");
                            continue;
                        }
                        toast_overlay.add_toast(adw::Toast::new(&message));
                    }
                    Event::ToastAction {
                        message,
                        label,
                        action,
                    } => {
                        let toast = adw::Toast::new(&message);
                        toast.set_button_label(Some(&label));
                        if action == "chat-destroy-session" {
                            // Raised only by the selected chat (chat.rs
                            // holds that line), and answered by the
                            // selected chat.
                            let chats = chats.clone();
                            toast.connect_button_clicked(move |_| {
                                if let Some(pane) = chats.selected() {
                                    pane.destroy_stale_session();
                                }
                            });
                        }
                        toast_overlay.add_toast(toast);
                    }
                    // The MCP server binds and unbinds that environment's
                    // socket on these; the fleet view gains and loses a row.
                    Event::EnvironmentCreated { env } => {
                        tracing::info!("environment {env} is available");
                        console.refresh_environment_data(false);
                    }
                    Event::EnvironmentRemoved { env } => {
                        tracing::info!("environment {env} is gone");
                        // Watching something that no longer exists is a
                        // tree pointed at a deleted directory: come home.
                        if filetree.watching().as_ref() == Some(&env) {
                            aim_panes(None);
                        }
                        // ...and its conversation goes with it. A chat is
                        // an environment's; there is nowhere else for it.
                        chats.forget_environment(&env);
                        // As do the tabs it had stowed: they are views onto
                        // a checkout that is gone.
                        editor.forget_environment(&env);
                        console.refresh_environment_data(false);
                    }
                    // Flagged for review, merged, rejected, or back at work:
                    // the fleet row says which, and a flagged environment's
                    // container is on its way down.
                    Event::EnvironmentReviewChanged { env } => {
                        tracing::info!("environment {env} moved along the review arc");
                        console.refresh_environment_data(false);
                        // An environment flagged for review wakes the
                        // coordinator — the primary's chat — to look at it
                        // (David, 2026-09-06: "Wake up the chat to review the
                        // state of envs if they become available for
                        // review"). Only when there IS a coordinator to wake:
                        // no agent in the primary means the user reviews
                        // alone, as they always could. Mid-turn, the prompt
                        // queues like any other.
                        if workspace.review.state(&env).flagged() {
                            crate::coordinator::wake_for_review(&chats, &toast_overlay, &env);
                        }
                    }
                    Event::AgentSessionUpdate { .. } => {}
                }
            }
        });
    }

    // Reconciliation runs before anything can be started: it picks existing
    // environment clones back up, and removes the containers and images the
    // single-environment naming scheme left behind — which would otherwise
    // sit unmanaged holding this workspace's forwarded ports. It reports
    // itself once (toast + app log) rather than resetting silently.
    //
    // Only from the supervising window. Reconciliation force-removes
    // containers and picks clones back up; a second window on the same
    // folder doing it in parallel is two processes deciding the fate of one
    // set of containers, which is the collision this whole claim exists to
    // prevent.
    if supervising {
        let environments = environments.clone();
        runtime().spawn(async move {
            let report = environments.reconcile().await;
            if !report.restored.is_empty() {
                tracing::info!(
                    "restored environments: {}",
                    report
                        .restored
                        .iter()
                        .map(|id| id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        });
    }

    // Initial state check runs only now that the UI is subscribed, so the
    // safe-mode banner reflects reality from the first frame.
    if let Err(e) = supervisor.recheck() {
        tracing::warn!("devcontainer recheck failed: {e:#}");
    }
    if let Err(e) = supervisor.start_watching() {
        tracing::warn!("devcontainer watcher failed: {e:#}");
    }

    // --- restore what was last open (XDG state + ACP session/load) -------
    // Never from a probe instance: session/load ATTACHES to the user's
    // real conversation, and two clients on one session is a fork bomb
    // for its history.
    let (persisted, state_was_reset) = if probe_mode {
        (taste_core::state::WorkspaceState::default(), false)
    } else {
        taste_core::state::load_reporting(&root)
    };
    if state_was_reset {
        // The IDE is alpha and its state schema moves; a discarded file is
        // told to the user once rather than looking like data loss. Through
        // the bus, because the toast overlay now belongs to the event pump.
        workspace.events.publish(Event::Toast(
            "Workspace state was reset (alpha schema change)".into(),
        ));
    }
    // Said once, on the same route, and only to the window it is about. A
    // person who opened the same project on a second monitor has done
    // nothing wrong, so this names what still works rather than what does
    // not.
    if let Some(notice) = supervision.as_ref().and_then(|s| s.notice()) {
        taste_core::app_log::push("INFO", "supervision", &notice);
        workspace.events.publish(Event::Toast(notice));
    }
    // The fleet renders the names the user gave their environments, and
    // this is where the state file has just been read.
    console.set_workspace_state(persisted.clone());
    for path in &persisted.open_files {
        if path.is_file() {
            editor.open_at(path, None);
        }
    }
    if let Some(active) = &persisted.active_file {
        if active.is_file() {
            editor.open_at(active, None);
        }
    }
    editor.sync_git_state();
    if probe_mode {
        // No agent, no persistence: render, get probed, quit. The seeded
        // chat is there; it simply never connects.
    } else {
        // One armed chat per remembered environment; only the selected
        // environment's connects now, the rest when the user goes there.
        chats.start(persisted.chats());
    }

    // Persist on close: open files come from the shared IDE state, the
    // chats from the chat column. The conversations themselves live with
    // the agent (session/load); we keep only the handles.
    //
    // This closure is also where the supervision claim lives, and that is
    // deliberate rather than convenient: the claim must last exactly as long
    // as the window, and a handler owned by the window is the one thing in
    // this function that does. GTK drops it with the widget, the descriptor
    // closes, and the folder is free for the next window — including when
    // this process is killed, which is the whole reason the claim is an
    // flock and not a pid file.
    if !probe_mode {
        let workspace = workspace.clone();
        let chats = chats.clone();
        let root = root.clone();
        let supervision = supervision;
        window.connect_close_request(move |_| {
            // Restore state has one owner too, for the same reason the
            // containers do: two windows on one folder writing one file is
            // whichever closed last deciding what the other had open.
            if supervision.as_ref().is_none_or(|s| s.is_granted()) {
                let open = workspace.ide.open_files();
                // Update in place: fields owned elsewhere survive untouched.
                let mut state = taste_core::state::load(&root);
                state.root = root.clone();
                state.open_files = open.iter().map(|f| f.path.clone()).collect();
                state.active_file = open.iter().find(|f| f.active).map(|f| f.path.clone());
                state.set_chats(chats.snapshot());
                if let Err(e) = taste_core::state::save(&root, &state) {
                    tracing::warn!("saving workspace state failed: {e:#}");
                }
            }
            glib::Propagation::Proceed
        });
    }

    window
}

/// A widget's minimum width, which is the unit the responsive ladder's
/// arithmetic is done in.
fn width_of(widget: &impl IsA<gtk::Widget>) -> i32 {
    widget.as_ref().measure(gtk::Orientation::Horizontal, -1).0
}

/// `TASTE_PROBE_WALK=520-1500[:25]` — the responsive ladder checked as a
/// ladder, at every width in a range rather than at the one width a
/// screenshot happens to be posed at.
///
/// This exists because a rung is a BAND and every other tool here reports a
/// point. A geometry dump says the panes fit at 900; it says nothing about
/// 830, and the fault it has to catch lives at whichever width the rung in
/// force stops fitting — which is a different number from the one the
/// breakpoint hands over at, and the gap between those two numbers is a
/// window the user can size to where nothing fits.
///
/// So: pose the window at each width, let the breakpoints apply, and ask the
/// only question that matters — is every mapped pane's right edge inside the
/// frame? A `FAIL` here is a width a user can drag to and see a pane cut off
/// the edge of their window. It exits non-zero so a harness can gate on it.
///
/// The layout's own minimum is printed beside the verdict, because that is
/// the number a breakpoint has to be chosen from: `min(layout)` at a rung is
/// the narrowest window that rung fits in, and the breakpoint below it must
/// hand over at or above that.
fn width_walk(
    window: &adw::ApplicationWindow,
    panes: Vec<(&'static str, gtk::Widget)>,
    layout: gtk::Widget,
    thresholds: std::rc::Rc<std::cell::Cell<(f64, f64)>>,
    spec: &str,
    app: &adw::Application,
) {
    let (range, step) = match spec.split_once(':') {
        Some((range, step)) => (range, step.parse::<usize>().unwrap_or(25)),
        None => (spec, 25),
    };
    let (from, to) = match range.split_once('-') {
        Some((from, to)) => (
            from.trim().parse::<i32>().unwrap_or(520),
            to.trim().parse::<i32>().unwrap_or(1500),
        ),
        None => (520, 1500),
    };
    let app = app.clone();
    let window = window.clone();
    window.connect_map(move |mapped| {
        let window = mapped.clone();
        let panes = panes.clone();
        let layout = layout.clone();
        let thresholds = thresholds.clone();
        let app = app.clone();
        glib::spawn_future_local(async move {
            // The first frame has to land before anything is measured: a
            // window that has not been allocated reports the sizes it was
            // built with.
            glib::timeout_future(std::time::Duration::from_millis(900)).await;
            let mut failures: Vec<String> = Vec::new();
            let height = window.default_height();
            // Down as well as up: `1500-380` is the gesture the bug was
            // reported from — a window being dragged narrower — and the
            // rungs are learned in the opposite order going that way.
            let descending = from > to;
            let mut width = from;
            while if descending { width >= to } else { width <= to } {
                window.set_default_size(width, height);
                // Long enough for the resize to round-trip through the
                // display and for the breakpoint's apply to reparent
                // whatever it reparents — grafting the chat and the
                // console's pages is real widget work, not a setter.
                glib::timeout_future(std::time::Duration::from_millis(320)).await;
                let actual = window.width();
                let frame = window
                    .compute_bounds(&window)
                    .map_or(f32::MAX, |bounds| bounds.x() + bounds.width());
                let (layout_min, _, _, _) = layout.measure(gtk::Orientation::Horizontal, -1);
                let (window_min, _, _, _) = window.measure(gtk::Orientation::Horizontal, -1);
                // The rung named by the thresholds actually in force, which
                // is not the same as the one the CONSTANTS would name: the
                // whole point of deriving them is that they move.
                let (at_consolidated, at_gadget) = thresholds.get();
                let actual_f = f64::from(actual);
                let rung = if actual_f <= at_gadget {
                    "Gadget"
                } else if actual_f <= at_consolidated {
                    "Consolidated"
                } else {
                    "Full"
                };
                let mut verdicts = Vec::new();
                for (name, pane) in &panes {
                    if !pane.is_mapped() {
                        continue;
                    }
                    let Some(bounds) = pane.compute_bounds(&window) else {
                        continue;
                    };
                    let right = bounds.x() + bounds.width();
                    let (min, _, _, _) = pane.measure(gtk::Orientation::Horizontal, -1);
                    if right > frame + 0.5 {
                        let over = right - frame;
                        verdicts.push(format!("{name} OFF-WINDOW by {over:.0} (min={min})"));
                        failures.push(format!(
                            "w={actual} {rung}: {name} runs {over:.0}px past the frame"
                        ));
                    } else {
                        verdicts.push(format!("{name} ok (min={min})"));
                    }
                }
                println!(
                    "walk asked={width} actual={actual} rung={rung} \
                     min(layout)={layout_min} min(window)={window_min} \
                     at({at_consolidated:.0}/{at_gadget:.0}) :: {}",
                    verdicts.join(", ")
                );
                width += if descending {
                    -(step as i32)
                } else {
                    step as i32
                };
            }
            if failures.is_empty() {
                println!("WALK PASS: {from}-{to} step {step}, every rung fits");
                app.quit();
            } else {
                println!("WALK FAIL: {} width(s) do not fit", failures.len());
                for failure in &failures {
                    println!("  {failure}");
                }
                // Non-zero, and without going through the app's own quit:
                // this is a gate, and a gate that exits 0 is decoration.
                std::process::exit(1);
            }
        });
    });
}

/// Watch the sandbox URL drop directory; confirm and open each URL in the
/// user's browser. Untrusted input by definition (an agent wrote it), hence
/// the scheme check and the explicit confirmation.
/// The shortcuts reference, as a plain boxed list (AdwShortcutsDialog
/// needs a newer libadwaita than we target).
fn present_shortcuts_dialog(parent: &adw::ApplicationWindow) {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    for (accel, title) in [
        ("Ctrl+P", "Open a file by name"),
        ("Ctrl+F", "Find in project"),
        ("Ctrl+S", "Save the current file"),
        ("Ctrl+W", "Close the current tab"),
        ("Ctrl+Shift+E", "Switch environment"),
        ("Ctrl+Q", "Quit (state is saved)"),
        ("Ctrl+Shift+C / V", "Copy / paste in terminals"),
        ("Ctrl+Click", "Open a link from a terminal"),
        ("Tab / Esc", "Accept / dismiss an AI suggestion"),
        ("Enter / Shift+Enter", "Send prompt / new line"),
    ] {
        let row = adw::ActionRow::builder().title(title).build();
        row.add_suffix(
            &gtk::Label::builder()
                .label(accel)
                .css_classes(["dim-label", "numeric"])
                .build(),
        );
        list.append(&row);
    }
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .propagate_natural_height(true)
        .max_content_height(480)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroller));
    let dialog = adw::Dialog::builder()
        .title("Keyboard Shortcuts")
        .content_width(420)
        .build();
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(parent));
}

/// Open a URL the user asked for: through the bootstrap's host-side
/// opener when present (the container has no browser), else the portal
/// (packaged runs), else the clipboard.
fn open_url(url: &str, overlay: &adw::ToastOverlay) {
    if let Some((dir, token)) = crate::host_open_channel() {
        let path = dir.join(format!("{token}.{}", glib::monotonic_time()));
        let contents = url.to_string();
        let overlay = overlay.clone();
        let handle =
            crate::runtime::runtime().spawn_blocking(move || std::fs::write(&path, contents));
        glib::spawn_future_local(async move {
            let message = match handle.await {
                Ok(Ok(())) => "Opening in your browser…",
                _ => "Couldn't reach the host URL opener",
            };
            overlay.add_toast(adw::Toast::new(message));
        });
        return;
    }
    let overlay = overlay.clone();
    let fallback = url.to_string();
    gtk::UriLauncher::new(url).launch(
        None::<&gtk::Window>,
        gtk::gio::Cancellable::NONE,
        move |result| {
            if result.is_err() {
                if let Some(display) = gtk::gdk::Display::default() {
                    display.clipboard().set_text(&fallback);
                }
                overlay.add_toast(adw::Toast::new(
                    "No browser here — link copied, paste it on the host",
                ));
            }
        },
    );
}

fn start_url_bridge(window: &adw::ApplicationWindow, root: &std::path::Path) {
    use notify::Watcher;

    // This window's drop directory, and only this window's. The purge below
    // is why that matters as much as the watch: a shared directory meant
    // every window deleted every other window's pending sign-in URLs on
    // startup, and whichever window's dialog appeared first consumed one
    // that may have belonged to a project the user was not looking at.
    let dir = taste_acp::sandbox::url_bridge_dir(root);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("url bridge dir: {e}");
        return;
    }
    // Stale drops from previous runs must not pop dialogs at startup.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }

    let (tx, rx) = async_channel::unbounded::<std::path::PathBuf>();
    let watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        if let Ok(event) = result {
            // Create fires at open(O_CREAT), often BEFORE the helper's
            // printf writes the URL — Modify covers the completed write.
            if matches!(
                event.kind,
                notify::EventKind::Create(_) | notify::EventKind::Modify(_)
            ) {
                for path in event.paths {
                    let _ = tx.try_send(path);
                }
            }
        }
    });
    let mut watcher = match watcher {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("url bridge watcher: {e}");
            return;
        }
    };
    if let Err(e) = watcher.watch(&dir, notify::RecursiveMode::NonRecursive) {
        tracing::warn!("url bridge watch: {e}");
        return;
    }
    Box::leak(Box::new(watcher));

    let window = window.downgrade();
    glib::spawn_future_local(async move {
        while let Ok(path) = rx.recv().await {
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue; // already consumed by an earlier event
            };
            let url = raw.trim().to_string();
            if url.is_empty() {
                // Created but not yet written: leave the file; the Modify
                // event after the write will bring us back.
                continue;
            }
            let _ = std::fs::remove_file(&path);
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                tracing::warn!("url bridge: refusing non-http(s) url");
                continue;
            }
            let Some(window) = window.upgrade() else {
                break;
            };
            let dialog = adw::AlertDialog::new(
                Some("Open sign-in link?"),
                Some(&format!(
                    "An agent wants to open this in your browser:\n\n{url}"
                )),
            );
            // "Copy Link" matters in the self-hosting bootstrap: inside the
            // container there is no browser, so the user pastes it into the
            // host browser (OAuth callbacks work under --network=host).
            dialog.add_responses(&[
                ("deny", "Deny"),
                ("copy", "Copy Link"),
                ("open", "Open in Browser"),
            ]);
            dialog.set_response_appearance("open", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("open"));
            dialog.set_close_response("deny");
            let launch_url = url.clone();
            let launch_window = window.clone();
            dialog.connect_response(Some("open"), move |_, _| {
                // Same channel as user clicks: host opener → portal →
                // clipboard.
                if let Some(overlay) = launch_window.content().and_downcast::<adw::ToastOverlay>() {
                    open_url(&launch_url, &overlay);
                }
            });
            let copy_url = url.clone();
            let copy_window = window.clone();
            dialog.connect_response(Some("copy"), move |_, _| {
                copy_window.clipboard().set_text(&copy_url);
            });
            dialog.present(Some(&window));
        }
    });
}

/// The user's Start on an issue (see the wiring in `build_window`).
fn start_issue(
    environments: &std::sync::Arc<taste_devcontainer::EnvironmentRegistry>,
    chats: &std::rc::Rc<crate::chats::Chats>,
    workspace: &taste_core::Workspace,
    aim_panes: &std::rc::Rc<dyn Fn(Option<taste_core::environment::EnvironmentId>)>,
    console: &std::rc::Rc<crate::console::Console>,
    issue: crate::backlog::StartedIssue,
) {
    let env = match crate::environments::for_issue(&issue.id) {
        Ok(env) => env,
        Err(e) => {
            workspace
                .events
                .publish(taste_core::Event::Toast(format!("{e:#}")));
            return;
        }
    };
    if environments.get(&env).is_some() {
        // Already started here: go there rather than make a second.
        aim_panes(Some(env));
        return;
    }
    let events = workspace.events.clone();
    let root = workspace.root().to_path_buf();
    let chats = chats.clone();
    let aim_panes = aim_panes.clone();
    let console = console.clone();
    let registry = environments.clone();
    crate::environments::create(
        environments.clone(),
        env,
        Box::new(move |outcome| {
            let env = match outcome {
                Ok(env) => env,
                Err(e) => {
                    events.publish(taste_core::Event::Toast(e));
                    return;
                }
            };
            // Start means the environment RUNS. Build and bring up its
            // container now — `Supervisor::reload`, the one lifecycle run
            // the row's Rebuild and the chat's revival also call, so a
            // second start is never asked for. It used to be left to the
            // chat: the first prompt below, once the agent was ready,
            // queued for revival and that revival started the container.
            // An agent that never got ready — Copilot in a fresh
            // environment's home, waiting for a sign-in — meant a clone
            // with a chat asking to log in and no container, ever (David,
            // 2026-09-06: "I can't seem to start any issue/environment
            // other than my personal one").
            if let Some(supervisor) = registry.get(&env) {
                let events = events.clone();
                let started = env.clone();
                crate::runtime::runtime().spawn(async move {
                    if let Err(e) = supervisor.reload().await {
                        events.publish(taste_core::Event::Toast(format!("{started}: {e:#}")));
                    }
                });
            }
            // The chat next, so the record can say what it was started
            // with; the store records who started it, and under which
            // agent and model — off this thread, and after the clone
            // exists, so a failed clone records nothing.
            let pane = chats.start_agent_in(&env);
            let settings = pane.as_ref().map(|pane| {
                let facts = pane.chat_facts(env.clone());
                (pane.agent_id(), facts.model)
            });
            {
                let id = issue.id.clone();
                let events = events.clone();
                let handle = crate::runtime::runtime().spawn_blocking(move || {
                    let git = taste_git::GitWorkspace::discover(&root)
                        .ok_or_else(|| anyhow::anyhow!("this workspace is not a git repository"))?;
                    let (agent, model) = match &settings {
                        Some((agent, model)) => (Some(agent.as_str()), model.as_deref()),
                        None => (None, None),
                    };
                    git.issue_start_with(&id, &taste_git::starter_identity(), agent, model)
                        .map(|_| ())
                });
                glib::spawn_future_local(async move {
                    match handle.await {
                        Ok(Ok(())) => events.publish(taste_core::Event::GitStatusChanged),
                        Ok(Err(e)) => events.publish(taste_core::Event::Toast(format!(
                            "the environment exists, but recording the start failed: {e:#}"
                        ))),
                        Err(e) => events.publish(taste_core::Event::Toast(format!(
                            "recording the start did not finish: {e}"
                        ))),
                    }
                });
            }
            // The chat, prompted with the issue: what Start means.
            if let Some(pane) = pane {
                let prompt = issue_prompt(&issue);
                pane.activate();
                pane.on_ready_once(Box::new(move |pane| {
                    if let Err(e) = pane.submit_prompt(prompt) {
                        tracing::warn!("the issue's first prompt was not taken: {e}");
                    }
                }));
            }
            aim_panes(Some(env.clone()));
            console.refresh_environment_data(false);
            events.publish(taste_core::Event::Toast(format!(
                "Started {} — {}",
                env, issue.title
            )));
        }),
    );
}

/// An issue as a first prompt: the same words the orchestrator's
/// `issue_start` sends, so a user-started and an agent-started environment
/// begin from one brief.
fn issue_prompt(issue: &crate::backlog::StartedIssue) -> String {
    taste_core::orchestration::issue_brief(&issue.id, &issue.title, &issue.body)
}

/// The Ports section's rows for one environment: its forwarded ports and
/// what the probe cache says about each.
fn port_rows(
    specs: &[taste_devcontainer::config::PortSpec],
    env: &taste_core::environment::EnvironmentId,
    facts: &HashMap<(taste_core::environment::EnvironmentId, u16), PortFacts>,
) -> Vec<crate::filetree::PortRow> {
    specs
        .iter()
        .map(|spec| crate::filetree::PortRow {
            spec: spec.clone(),
            listening: facts
                .get(&(env.clone(), spec.port))
                .and_then(|facts| facts.listening),
        })
        .collect()
}

/// The deeper look a port tab takes when it opens: server, content type,
/// the process in the container. Off the main thread; the answer lands on
/// the tab and in the cache.
fn probe_port(
    editor: &Rc<Editor>,
    environments: &std::sync::Arc<EnvironmentRegistry>,
    cache: &Rc<RefCell<HashMap<(taste_core::environment::EnvironmentId, u16), PortFacts>>>,
    env: taste_core::environment::EnvironmentId,
    spec: taste_devcontainer::config::PortSpec,
) {
    let exec = environments
        .get(&env)
        .map(|supervisor| supervisor.exec().clone());
    let weak = Rc::downgrade(editor);
    let cache = cache.clone();
    glib::spawn_future_local(async move {
        let handle = crate::runtime::runtime().spawn(crate::portview::probe(exec, spec.clone()));
        let Ok(facts) = handle.await else { return };
        cache
            .borrow_mut()
            .insert((env.clone(), spec.port), facts.clone());
        if let Some(editor) = weak.upgrade() {
            editor.set_port_facts(&env, spec.port, &facts);
        }
    });
}

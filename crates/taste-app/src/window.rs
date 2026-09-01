//! The one window arrangement: files left, editor center, console bottom,
//! AI chat right. Resizable and collapsible; never rearrangeable.

use std::path::PathBuf;

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
use crate::editor::Editor;
use crate::filetree::FileTree;
use crate::runtime::runtime;

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
    let server_for_role = server.clone();
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
    {
        // The orchestrator role: the strip owns which chat holds it, the
        // MCP server owns which socket serves the tools, and this is the
        // one wire between them. Set before the strip restores its tabs,
        // so a remembered orchestrator's socket is already serving them
        // when its agent lists tools on first activation.
        let server = server_for_role.clone();
        chats.set_on_orchestrator_changed(move |env| server.set_orchestrator(env));
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
        let aim_panes = aim_panes.clone();
        console.set_on_open_environment(move |env| aim_panes(Some(env)));
    }
    {
        // A chat that changed something a panel row renders (a turn
        // starting, a permission request arriving in an environment nobody
        // is looking at) asks for the rows to be re-assembled.
        //
        // ...and, while the window is narrow enough that the chat is a
        // pinned tab rather than a column, lights that tab when the
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
    {
        // ...and the panel header's +, which is the fleet view's New
        // Environment button reached from where the switching happens.
        let console = console.clone();
        filetree.set_on_new_environment(move |button| console.create_environment(button));
    }
    {
        // The backlog under that panel. Three wires, and each one is the
        // queue meeting something that already exists rather than a
        // mechanism of its own:
        //
        // - a claimed row selects its environment, through the same
        //   `aim_panes` every other surface goes through;
        // - a write asks the console to re-read the ref, because the
        //   console is where the off-thread git passes live;
        // - a refused write toasts, like every other action outcome.
        let aim_panes = aim_panes.clone();
        filetree.set_on_open_claim(move |env| aim_panes(Some(env)));
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
    {
        // The panel's tick re-renders the fleet: the assembly is cheap by
        // construction (no IO, no podman) and equality-guarded, and it is
        // what makes a chat that started streaming since the last fleet
        // change show its spinner. A permanent list has no open-moment to
        // refresh on, so it takes one every second.
        let console = console.clone();
        filetree.set_on_strip_refresh(move || console.refresh_fleet());
    }
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
        .position(980)
        .build();

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
        .position(260)
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
        .title_widget(&title)
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
    // **The chat column becomes a PINNED tab in the editor's tab view**,
    // and that is the whole rung. The pane is reparented into the tab page,
    // its own header — identity, chat/utilization/settings — riding along,
    // so switching environments keeps working exactly as it does at full
    // width: the pinned tab always shows the selected environment's chat,
    // because it holds the same widget the column did. What it buys is that
    // whichever of the two the user is actually reading — the chat or a
    // file — gets the whole width instead of half of it.
    //
    // **Nothing else moves.** The flank stays, with the Environments panel
    // and the Backlog in it; the console stays under the editor. The
    // three-region geometry is identical to full width — flank, wide area,
    // console below — and only the number of columns in the middle changes,
    // from two to one. An earlier version of this rung also collapsed the
    // flank, which turned the window into a stack of full-width bands and
    // took away the panel that says which environment you are in, at
    // exactly the width where the console has less room to say it.
    {
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            crate::gadget::CONSOLIDATED_MAX_WIDTH_SP,
            adw::LengthUnit::Sp,
        ));
        {
            // The chat column moves into the editor's tab view. The Paned
            // is unparented HERE rather than in the editor, because the
            // editor does not know what the chat was a child of and should
            // not have to.
            let editor = editor.clone();
            let chats = chats.clone();
            let paned = center_and_chat.clone();
            breakpoint.connect_apply(move |_| {
                if editor.holds_chat() {
                    return; // AdwBreakpoint can fire apply twice
                }
                paned.set_end_child(gtk::Widget::NONE);
                editor.adopt_chat(chats.widget.clone().upcast_ref());
            });
        }
        {
            let editor = editor.clone();
            let paned = center_and_chat.clone();
            breakpoint.connect_unapply(move |_| {
                if let Some(chat) = editor.release_chat() {
                    paned.set_end_child(Some(&chat));
                }
            });
        }
        window.add_breakpoint(breakpoint);
    }

    // Below the breakpoint the panes give way to the card, the deploy
    // button and the safe-mode banner go with them (neither is a thing you
    // act on from a monitor), and the header says what is being watched.
    // Every setter is restored when the window grows back — that is
    // AdwBreakpoint's contract, and it is what makes "stretch back to the
    // IDE, nothing rearranged" true rather than aspirational.
    {
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            crate::gadget::GADGET_MAX_WIDTH_SP,
            adw::LengthUnit::Sp,
        ));
        breakpoint.add_setter(&surfaces, "visible-child-name", Some(&"gadget".to_value()));
        breakpoint.add_setter(&banner.widget, "visible", Some(&false.to_value()));
        breakpoint.add_setter(&flatpak_button, "visible", Some(&false.to_value()));
        // File navigation belongs to the editor, and there is no editor
        // down here.
        breakpoint.add_setter(&editor.back_button, "visible", Some(&false.to_value()));
        breakpoint.add_setter(&editor.forward_button, "visible", Some(&false.to_value()));
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
            if f64::from(window.default_width()) <= crate::gadget::GADGET_MAX_WIDTH_SP {
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
        let console = console.clone();
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
                // The fleet row, not the panes: a failed build is
                // something to look at, and re-aiming the tree and editor
                // at that environment is a bigger act than the user asked
                // for by clicking a notification.
                crate::notify::Surface::Environment(env) => console.reveal_environment(env),
                // A judgment is wanted, so land on the environment
                // properly — the console's review band is about the
                // SELECTED environment, and revealing a row without
                // selecting it would show the band for a different one.
                crate::notify::Surface::Review(env) => aim_for_notice(Some(env.clone())),
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
        let restore = restore_panes.clone();
        let aim_for_claim = aim_panes.clone();
        filetree.set_on_open_claim(move |env| {
            restore();
            aim_for_claim(Some(env));
        });
    }

    // --- the fleet, published --------------------------------------------
    // The fleet's rows as JSON, refreshed on every publish: what
    // `env_list` and `env_status` answer with (see `orchestration.rs`).
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
        // Weak console: the hook is owned BY the console, and a strong
        // handle back would be a cycle that never drops.
        let console_for_notice = std::rc::Rc::downgrade(&console);
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
            // ...and the same rows, for the orchestrator's env_list. One
            // assembly, four renderers now; the tools read what the user
            // reads rather than a fifth derivation of podman and git.
            *fleet_cache.borrow_mut() = serde_json::to_value(&snapshot.rows)
                .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));

            // The two fleet-shaped notifications, decided off the same
            // rows every other surface renders — not off a second read of
            // podman and git.
            let (Some(window), Some(console)) =
                (window_for_notice.upgrade(), console_for_notice.upgrade())
            else {
                return;
            };
            let Some(app) = window.application() else {
                return;
            };
            let attention = crate::notify::Attention {
                window_active: window.is_active(),
                fleet_on_screen: console.fleet_on_screen(),
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
                    .map(crate::envstrip::title_of)
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
        // podman call) so `env_list` answers with what is on screen
        // rather than with whatever was last broadcast.
        let console = console.clone();
        let rows = fleet_rows.clone();
        crate::orchestration::attach(
            &workspace,
            chats.clone(),
            environments.clone(),
            std::rc::Rc::new(move || {
                console.republish_fleet();
                rows.borrow().clone()
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
        ];
        let app = app.clone();
        window.connect_map(move |_| {
            let report = report.clone();
            let app = app.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
                for (name, widget) in &report {
                    let (min, natural, _, _) = widget.measure(gtk::Orientation::Horizontal, -1);
                    println!("min-width {name}: min={min} nat={natural}");
                }
                // Walk the console tree to attribute its minimum.
                fn walk(widget: &gtk::Widget, depth: usize) {
                    let (min, _, _, _) = widget.measure(gtk::Orientation::Horizontal, -1);
                    if min > 300 {
                        println!("{}{} min={min}", "  ".repeat(depth), widget.type_().name());
                    }
                    if depth < 8 {
                        let mut child = widget.first_child();
                        while let Some(current) = child {
                            walk(&current, depth + 1);
                            child = current.next_sibling();
                        }
                    }
                }
                if let Some((_, console)) = report.iter().find(|(n, _)| *n == "console") {
                    walk(console, 0);
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
            "watching" | "orchestrator" => "calm-1",
            // The review shots are of a FLAGGED environment's work, so the
            // panes have to be aimed at one.
            "review" => "wry-4",
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
            if view == "utilization" {
                pane.seed_utilization_for_probe();
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
        // window whose panel said `calm-1` while the console header still
        // said `Yours` — two surfaces disagreeing about where the panes
        // are, which is the exact failure that deleting the console's
        // second listing was meant to make impossible. `probe_env` is the
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
            // `envstrip`, whose whole subject is the panel at home:
            // untinted, with "Yours" the selected row.
            "hero" | "fleet" | "envstrip" | "backlog" | "consolidated" => {}
            _ => filetree.seed_watching_for_probe(probe_env),
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
        // A live agent terminal: the console's half of live shells.
        // Into the environment the panes are aimed at: watching is "open an
        // environment and see its agent work", and a roster that says
        // "nothing running here" while the agent works next door is the shot
        // contradicting its own caption.
        //
        // NOT for the review shot, whose environment is flagged and
        // therefore STOPPED. Flagging stops the container; the agent lived
        // in it and died with it, and `Terminals::release_all` takes its
        // rows off the roster on the way out — so a real stopped
        // environment has no agent terminal to show, running or otherwise.
        // The fixture used to seed one anyway, and the frame said "stopped"
        // and "agent terminal · running" at once. A fixture that
        // contradicts the code is a fixture to fix.
        if view != "review" && view != "review-diff" {
            console.seed_agent_terminal_for_probe(
                &taste_core::environment::EnvironmentId::parse(probe_env)
                    .unwrap_or_else(|_| primary_env.clone()),
            );
        }
        // And a fleet with something in it: one row per environment is
        // what the console's pinned tab now is. The console gets more of
        // the window than it normally has, because a fleet of one row is
        // not what the screenshot is for.
        console.seed_fleet_for_probe(match view.as_str() {
            // The console's list stops at four rows; the gadget's does not,
            // and a monitor with room to spare is the thing it is for.
            "fleet" => 3,
            // Everything else takes the whole fabricated fleet — four, plus
            // the primary, which is one under the panel's six-row ceiling,
            // so the panel photographs full and not yet scrolling.
            //
            // It used to be two for most views, and that was wrong the
            // moment the backlog arrived: a claim whose environment has
            // been truncated out of the fleet renders as "this workspace no
            // longer has it", which is an honest rendering of a dishonest
            // fixture. The flagged environment is the fourth, so the review
            // rail needs all of them too.
            _ => 4,
        });
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
        // A queue with something on it, always. It is the backlog panel
        // that draws it now, in the file-tree flank under the environment
        // panel, and it appears in every shot that frames that flank — so
        // there is no view that seeds it and no view that does not.
        console.seed_issues_for_probe();
        // The backlog folds away for the shot that is about something
        // above it: `envstrip` is the environment panel's own portrait,
        // and a queue hanging off the bottom of it would be half of one
        // photograph and half of another.
        filetree.set_backlog_expanded(view != "envstrip");
        // ...and the shot that is ABOUT the backlog shows a row wearing
        // its actions. They are hover-only, and a hover cannot be
        // photographed — so the frame would otherwise be missing the half
        // of this panel that does anything. The second row, because it is
        // the one with all four moves available.
        if view == "backlog" {
            filetree.seed_backlog_actions_for_probe("i-0002");
        }
        // The build log is empty on a probe — nothing here has been built.
        // The shell roster under it has the seeded agent terminal in it.
        console.seed_detail_page_for_probe("shells");
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
            // The review band leads the console's detail, and a band
            // clipped to its heading is not a shot of it.
            "review" => 300,
            // The review DIFF shot is of the editor: the comparison bar,
            // the badged tab and the hunks. The console keeps enough
            // height to show which environment this is a review of.
            "review-diff" => 520,
            // Both of these used to hand the console the height a LIST
            // needs, because the console listed every environment. It does
            // not any more — the file tree's panel enumerates them and the
            // console details the one you are in — so the editor takes the
            // room back rather than the shot framing an empty half-pane.
            "hero" => 430,
            "fleet" => 400,
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
        let envstrip_probe = view == "envstrip";
        let backlog_probe = view == "backlog";
        let review_probe = view == "review";
        let review_diff_probe = view == "review-diff";
        // The middle rung, shot at a real width rather than posed: the
        // window is made narrow enough to trip the breakpoint, and what
        // the frame shows is the transition the breakpoint actually
        // performs.
        let consolidated_probe = view == "consolidated";
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
            // so the chat becomes a pinned tab, and well clear of
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
        let panes_for_fit: Vec<(&'static str, gtk::Widget)> = vec![
            ("filetree", filetree.widget.clone().upcast()),
            ("editor", editor.widget.clone().upcast()),
            ("console", console.widget.clone().upcast()),
        ];
        window.connect_map(move |window| {
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
                use crate::envstrip::Shape;
                filetree_for_probe.seed_activity_for_probe(&[
                    // The user's own checkout: they have been editing, so
                    // it is alive but not the busiest thing on screen.
                    ("primary", Shape::Editing),
                    // An agent mid-task in a container that is up.
                    ("calm-1", Shape::Working),
                    // A container building: a burst per step, gaps between.
                    ("brisk-3", Shape::Building),
                    // Stopped, and therefore silent. The row that proves a
                    // sparkline can be honestly empty.
                    ("wry-4", Shape::Silent),
                    // Up, and stopped on a question — working right up to
                    // the moment it needed an answer.
                    ("spry-2", Shape::Working),
                ]);
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
                if let Some(path) = probe_open {
                    editor_for_probe.open_at(&path, Some(113));
                }
                // ...and, for the shot that is about consolidation, the
                // chat tab in front. Opening the file above selected its
                // own tab, and a frame of the editor with a small unopened
                // icon beside it does not show what the icon IS.
                if view_for_open == "consolidated" {
                    editor_for_probe.select_chat_tab();
                    // ...and the flank at the width it has at full size.
                    // Set here rather than at build time: a GtkPaned
                    // position asked for before the children are realized
                    // is recomputed from their natural sizes on the first
                    // allocation, which in a 900px window hands the flank
                    // nearly half the frame and pushes the tabbed area off
                    // the right edge. The point of the shot is that the
                    // flank is UNCHANGED and the middle gained the chat
                    // column's room.
                    outer_for_probe.set_position(280);
                }
                // The review is aimed HERE, not with the other seeds: the
                // pane's first status pass settles its filter toggles, and
                // entering a filter is one of the ways out of a review — so
                // a review opened before that lands is a review the pane
                // has already left by the time anything is photographed.
                if view_for_open == "review" || view_for_open == "review-diff" {
                    filetree_for_probe.seed_review_for_probe("agents/wry-4", "main");
                }
                // ...and the diff one of its rows opens, in front, because
                // the file tab opened above took the selection.
                if view_for_open == "review-diff" {
                    editor_for_probe.open_review_diff(
                        std::path::Path::new("crates/taste-app/src/fleet.rs"),
                        "agents/wry-4",
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
                    } else if envstrip_probe {
                        &["filetree", "filetree.envpanel"]
                    } else if backlog_probe {
                        &["filetree", "filetree.backlog"]
                    } else if consolidated_probe {
                        // The whole window: the point of this one is what
                        // the LAYOUT does, and a pane out of it says
                        // nothing about that.
                        &["window"]
                    } else if review_probe {
                        // The console's detail, where the band is. The
                        // window shot too, because the panel's accent rail
                        // on the same environment is the other half of it.
                        &["window", "console", "filetree"]
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
                        &["filetree", "filetree.backlog"]
                    } else if consolidated_probe {
                        // What the middle rung claims: the flank is still
                        // there and still a column, the console is still
                        // under the editor, and the editor — now holding
                        // the chat as a pinned tab — has the rest. The
                        // window too, because those three only add up to
                        // the claim if they add up to IT.
                        &["window", "filetree", "editor", "console"]
                    } else if envstrip_probe {
                        // The panel's own allocation: it is pinned below
                        // everything the pane can open and must stop at six
                        // rows, and the numbers are how both are checked
                        // rather than eyeballed.
                        &["filetree", "filetree.envpanel"]
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
        let filetree_for_search = filetree.clone();
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>f"),
            Some(gtk::CallbackAction::new(move |_, _| {
                filetree_for_search.focus_search();
                glib::Propagation::Stop
            })),
        ));
        // Ctrl+P: quick-open over the background file index.
        let filetree_for_open = filetree.clone();
        let editor_for_open = editor.clone();
        let window_for_open = window.clone();
        let root_for_open = root.clone();
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>p"),
            Some(gtk::CallbackAction::new(move |_, _| {
                present_quick_open(
                    &window_for_open,
                    &root_for_open,
                    filetree_for_open.index_files(),
                    editor_for_open.clone(),
                );
                glib::Propagation::Stop
            })),
        ));
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
        glib::spawn_future_local(async move {
            while let Ok(event) = events.recv().await {
                match event {
                    Event::GitStatusChanged => {
                        filetree.on_git_status_changed();
                        editor.sync_git_state();
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
                        editor.sync_git_state();
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
                        // Every environment's drift shows in its fleet row;
                        // the banner speaks for the primary alone, because
                        // it is the checkout the panes write to.
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
                    Event::ShowDevcontainerLog => console.show_devcontainer_log(&primary_env),
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
                    Event::ServiceSummary { total, failed } => {
                        console.update_service_summary(total, failed);
                    }
                    Event::ServicesUnavailable { systemd_missing } => {
                        console.set_services_unavailable(systemd_missing);
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
                        // inside the build container, no systemd, no
                        // session bus. A banner about the harness across
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

/// Quick-open: type-to-filter over the indexed file list, Enter opens the
/// top hit. Lean by design — the index already exists for search.
fn present_quick_open(
    parent: &adw::ApplicationWindow,
    root: &std::path::Path,
    index: Option<std::sync::Arc<Vec<std::path::PathBuf>>>,
    editor: std::rc::Rc<crate::editor::Editor>,
) {
    let entry = gtk::SearchEntry::builder()
        .placeholder_text("Type a file name…")
        .margin_top(8)
        .margin_start(8)
        .margin_end(8)
        .build();
    let results = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Browse)
        .css_classes(["navigation-sidebar"])
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&results)
        .vexpand(true)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.append(&entry);
    content.append(&scroller);
    let dialog = adw::Dialog::builder()
        .title("Open File")
        .content_width(520)
        .content_height(420)
        .build();
    dialog.set_child(Some(&content));

    let root = root.to_path_buf();
    let refresh = {
        let results = results.clone();
        let dialog = dialog.downgrade();
        let editor = editor.clone();
        let root = root.clone();
        move |query: &str| {
            while let Some(child) = results.first_child() {
                results.remove(&child);
            }
            let Some(index) = index.as_ref() else {
                results.append(
                    &gtk::Label::builder()
                        .label("Index still building — try again in a moment")
                        .css_classes(["dim-label"])
                        .margin_top(12)
                        .build(),
                );
                return;
            };
            let query = query.to_lowercase();
            // File-name hits first, then path hits; both bounded.
            let mut hits: Vec<&std::path::PathBuf> = Vec::new();
            for by_name in [true, false] {
                for path in index.iter() {
                    if hits.len() >= 50 {
                        break;
                    }
                    let rel = path.strip_prefix(&root).unwrap_or(path);
                    let hay = if by_name {
                        rel.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_lowercase()
                    } else {
                        rel.display().to_string().to_lowercase()
                    };
                    let already = hits.contains(&path);
                    if !already && hay.contains(&query) {
                        hits.push(path);
                    }
                }
            }
            for path in hits {
                let rel = path.strip_prefix(&root).unwrap_or(path);
                // Row titles are Pango markup: file names must be escaped.
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&name))
                    .subtitle(glib::markup_escape_text(&rel.display().to_string()))
                    .activatable(true)
                    .build();
                row.add_prefix(&gtk::Image::from_gicon(&crate::editor::file_type_icon(
                    path,
                )));
                let editor = editor.clone();
                let dialog = dialog.clone();
                let path = path.clone();
                row.connect_activated(move |_| {
                    editor.open_at(&path, None);
                    if let Some(dialog) = dialog.upgrade() {
                        dialog.close();
                    }
                });
                results.append(&row);
            }
            if let Some(first) = results.row_at_index(0) {
                results.select_row(Some(&first));
            }
        }
    };
    {
        let refresh = refresh.clone();
        entry.connect_search_changed(move |entry| refresh(&entry.text()));
    }
    {
        let results = results.clone();
        entry.connect_activate(move |_| {
            if let Some(row) = results.selected_row().or_else(|| results.row_at_index(0)) {
                row.emit_activate();
            }
        });
    }
    refresh("");
    dialog.present(Some(parent));
    entry.grab_focus();
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

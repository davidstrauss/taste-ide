//! Slash-command completion for the composer, through GtkSourceView's own
//! completion framework.
//!
//! The framework owns the popup, keyboard navigation, filtering-as-you-type,
//! scrolling, sizing and cursor-relative placement. All this module supplies is
//! the list and how to render and apply one entry — which is the whole reason
//! to use it rather than hand-rolling a popover.
//!
//! `GtkSourceCompletionProvider` is a GObject interface, and gtk-rs has no
//! closure-based adapter for interfaces, so the two types here are subclasses.
//! The proposal is a marker interface with no methods; the provider has four
//! that do real work.

use std::cell::RefCell;

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use sourceview5::subclass::prelude::*;

/// One completable command: the name typed after the slash, and the one-line
/// description shown beside it.
#[derive(Clone)]
pub struct Command {
    pub name: String,
    pub description: String,
}

mod proposal_imp {
    use super::*;

    #[derive(Default)]
    pub struct CommandProposal {
        pub name: RefCell<String>,
        pub description: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CommandProposal {
        const NAME: &'static str = "TasteCommandProposal";
        type Type = super::CommandProposal;
        type Interfaces = (sourceview5::CompletionProposal,);
    }

    impl ObjectImpl for CommandProposal {}
    impl CompletionProposalImpl for CommandProposal {}
}

glib::wrapper! {
    pub struct CommandProposal(ObjectSubclass<proposal_imp::CommandProposal>)
        @implements sourceview5::CompletionProposal;
}

impl CommandProposal {
    fn new(command: &Command) -> Self {
        let proposal: Self = glib::Object::new();
        proposal.imp().name.replace(command.name.clone());
        proposal
            .imp()
            .description
            .replace(command.description.clone());
        proposal
    }

    fn name(&self) -> String {
        self.imp().name.borrow().clone()
    }

    fn description(&self) -> String {
        self.imp().description.borrow().clone()
    }
}

mod provider_imp {
    use super::*;

    #[derive(Default)]
    pub struct CommandProvider {
        /// Held interior-mutably rather than baked in at construction: ACP
        /// models this as `AvailableCommandsUpdate`, an update the agent may
        /// resend mid-session, so a frozen list would go stale.
        pub commands: RefCell<Vec<Command>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CommandProvider {
        const NAME: &'static str = "TasteCommandProvider";
        type Type = super::CommandProvider;
        type Interfaces = (sourceview5::CompletionProvider,);
    }

    impl ObjectImpl for CommandProvider {}

    impl CompletionProviderImpl for CommandProvider {
        fn title(&self) -> Option<glib::GString> {
            Some(glib::GString::from("Commands"))
        }

        /// A slash opens the list only at the very start of the composer — a
        /// command is the whole prompt, not something embedded in one.
        ///
        /// `iter` is where the caret landed, which is one PAST the character
        /// just typed, so the slash is the one BEHIND it. Read as the slash's
        /// own position this asked for offset 0 and could never be true: by
        /// the time the framework asks, the caret is at 1. A bare slash
        /// therefore opened nothing at all, and the list only appeared once a
        /// letter followed it and the ordinary word path took over.
        fn is_trigger(&self, iter: &gtk::TextIter, c: char) -> bool {
            if c != '/' {
                return false;
            }
            let mut slash = *iter;
            slash.backward_char() && slash.char() == '/' && slash.offset() == 0
        }

        fn populate(
            &self,
            context: &sourceview5::CompletionContext,
        ) -> Result<gtk::gio::ListModel, glib::Error> {
            let matches = self.matching(context);
            if matches.n_items() == 0 {
                // An empty model is not "nothing" to the assistant: it is a
                // page, and the assistant presents it — zero proposals wide,
                // a gdk_popup_present CRITICAL on every ordinary keystroke
                // (interactive completion populates for ANY word, not just
                // slash commands). A zero-size popup is also a surface a
                // Wayland compositor may kill the whole app over. No
                // matches is an error here; that is the framework's word
                // for "this provider sits this one out".
                return Err(glib::Error::new(
                    gtk::gio::IOErrorEnum::NotFound,
                    "no matching commands",
                ));
            }
            Ok(matches.upcast())
        }

        /// Typing narrows the list. Handing back a freshly filtered model is
        /// what keeps the popup in step with the prefix — and handing back
        /// None (not an empty model) is what makes it close when the prefix
        /// stops matching anything.
        fn refilter(&self, context: &sourceview5::CompletionContext, _model: &gtk::gio::ListModel) {
            let matches = self.matching(context);
            if matches.n_items() == 0 {
                context.set_proposals_for_provider(&*self.obj(), None::<&gtk::gio::ListModel>);
            } else {
                context.set_proposals_for_provider(&*self.obj(), Some(&matches));
            }
        }

        fn display(
            &self,
            _context: &sourceview5::CompletionContext,
            proposal: &sourceview5::CompletionProposal,
            cell: &sourceview5::CompletionCell,
        ) {
            let Some(proposal) = proposal.downcast_ref::<super::CommandProposal>() else {
                return;
            };
            match cell.column() {
                sourceview5::CompletionColumn::TypedText => {
                    cell.set_text(Some(&format!("/{}", proposal.name())));
                }
                sourceview5::CompletionColumn::Comment | sourceview5::CompletionColumn::Details => {
                    cell.set_text(Some(&proposal.description()));
                }
                _ => cell.set_text(None),
            }
        }

        fn activate(
            &self,
            context: &sourceview5::CompletionContext,
            proposal: &sourceview5::CompletionProposal,
        ) {
            let Some(proposal) = proposal.downcast_ref::<super::CommandProposal>() else {
                return;
            };
            let (Some(buffer), Some((start, end))) = (context.buffer(), context.bounds()) else {
                return;
            };
            let (mut start, mut end) = (start, end);
            // Swallow the leading slash if the word bounds excluded it, so the
            // replacement is the whole token either way — whether `/` counts as
            // a word character is a buffer setting, not something to rely on.
            let mut probe = start;
            if probe.backward_char() && probe.char() == '/' {
                start = probe;
            }
            buffer.begin_user_action();
            buffer.delete(&mut start, &mut end);
            buffer.insert(&mut start, &format!("/{} ", proposal.name()));
            buffer.end_user_action();
        }

        /// Enter takes the highlighted command, as it would in any completion
        /// list. Stated explicitly rather than left to key-controller ordering
        /// between the framework and the composer's own Enter-sends handler.
        fn key_activates(
            &self,
            context: &sourceview5::CompletionContext,
            proposal: &sourceview5::CompletionProposal,
            keyval: gtk::gdk::Key,
            state: gtk::gdk::ModifierType,
        ) -> bool {
            if matches!(keyval, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter)
                && !state.contains(gtk::gdk::ModifierType::SHIFT_MASK)
            {
                return true;
            }
            self.parent_key_activates(context, proposal, keyval, state)
        }
    }

    impl CommandProvider {
        /// The commands whose name the typed prefix starts — when what is
        /// being typed is a command at all.
        fn matching(&self, context: &sourceview5::CompletionContext) -> gtk::gio::ListStore {
            let store = gtk::gio::ListStore::new::<super::CommandProposal>();
            let Some(prefix) = command_prefix(context) else {
                return store;
            };
            for command in self
                .commands
                .borrow()
                .iter()
                .filter(|command| command.name.starts_with(&prefix))
            {
                store.append(&super::CommandProposal::new(command));
            }
            store
        }
    }

    /// How much of a slash command has been typed, if the word the
    /// completion is asking about IS one: the same rule `is_trigger` states
    /// for the keystroke that opens the list — a slash at the very start of
    /// the box — applied to every keystroke after it, which is where it was
    /// missing.
    ///
    /// Interactive completion asks about EVERY word, not just the ones
    /// behind a slash, and this used to answer by trimming a leading slash
    /// that need not have been there: typing "container", "clear" or
    /// "resume" into an ordinary sentence pulled the command list up over
    /// it. Two faults in one. The list is wrong — a command is the whole
    /// prompt, never a word inside one — and each opening was a chance at
    /// `gdk_popup_present: assertion 'width > 0' failed`, the CRITICAL
    /// David reported once per typing burst (i-0023). A popup surface with
    /// no width is one a Wayland compositor may kill the whole app over.
    ///
    /// The earlier guard in `populate` — no matches is an error, not an
    /// empty page — covered the ordinary words that match NOTHING. It could
    /// not cover the ones that match something, and in English prose those
    /// are common.
    fn command_prefix(context: &sourceview5::CompletionContext) -> Option<String> {
        let (start, _) = context.bounds()?;
        let word = context.word();
        match word.strip_prefix('/') {
            // The slash came back as part of the word, so the word itself
            // has to be the first thing in the box.
            Some(rest) => (start.offset() == 0).then(|| rest.to_string()),
            // ...and when it did not — whether `/` counts as a word
            // character is a buffer setting, which is why `activate` probes
            // for it too — the character before the word has to be the
            // slash, and the slash has to be the first thing in the box.
            None => {
                let mut probe = start;
                (probe.backward_char() && probe.char() == '/' && probe.offset() == 0)
                    .then(|| word.to_string())
            }
        }
    }
}

glib::wrapper! {
    pub struct CommandProvider(ObjectSubclass<provider_imp::CommandProvider>)
        @implements sourceview5::CompletionProvider;
}

impl Default for CommandProvider {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl CommandProvider {
    /// Replace the offered commands. Called for every
    /// `AvailableCommandsUpdate`, including the ones after the first.
    pub fn set_commands(&self, commands: Vec<Command>) {
        self.imp().commands.replace(commands);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn commands() -> Vec<Command> {
        ["compact", "context", "clear", "review"]
            .into_iter()
            .map(|name| Command {
                name: name.to_string(),
                description: format!("the {name} command"),
            })
            .collect()
    }

    /// Run the main loop for roughly `ms`, the way the frame clock would
    /// between keystrokes: the completion is asynchronous and its popup is
    /// presented on the popover's own frame clock, so a burst typed without
    /// any turns of the loop exercises none of it.
    fn settle(ms: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        while std::time::Instant::now() < deadline {
            glib::MainContext::default().iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Type `text` into `view` one character at a time, letting the loop
    /// turn between each — one keystroke is one insertion of one character,
    /// which is what `is_single_char` in the completion asks for.
    fn type_into(view: &sourceview5::View, text: &str) {
        let buffer = view.buffer();
        for ch in text.chars() {
            let mut end = buffer.end_iter();
            buffer.insert(&mut end, &ch.to_string());
            settle(40);
        }
    }

    /// The composer, as the completion sees it: a mapped view in a mapped
    /// window, because `display_show` in GtkSourceCompletion refuses to
    /// present the popup over a view that is not mapped, and an unmapped
    /// view would pass this test by never showing anything.
    fn composer() -> (gtk::Window, sourceview5::View) {
        let view = sourceview5::View::new();
        let scroller = gtk::ScrolledWindow::builder().child(&view).build();
        let window = gtk::Window::builder()
            .default_width(600)
            .default_height(200)
            .child(&scroller)
            .build();
        window.present();
        settle(300);
        (window, view)
    }

    /// Collect every CRITICAL and WARNING GLib logs while the closure runs.
    /// This is `G_DEBUG=fatal-criticals` made assertable: the assertion that
    /// matters — `gdk_popup_present: assertion 'width > 0' failed` — is a
    /// log line and nothing else, so the only way a test can see it is to
    /// listen for it.
    fn criticals(body: impl FnOnce()) -> Vec<String> {
        // Arc, not Rc: glib hands the default handler out to whatever thread
        // logs, so it demands Send + Sync even though everything here is on
        // the main one.
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorder = seen.clone();
        glib::log_set_default_handler(move |domain, level, message| {
            if matches!(level, glib::LogLevel::Critical | glib::LogLevel::Warning) {
                if let Ok(mut seen) = recorder.lock() {
                    seen.push(format!("{}: {message}", domain.unwrap_or("(unknown)")));
                }
            }
        });
        body();
        glib::log_unset_default_handler();
        let collected = seen.lock().expect("log recorder").clone();
        collected
    }

    /// The completion's popup, if it is on screen: GtkSourceCompletion
    /// parents its list to the view as an "assistant", so it is a child
    /// widget like any other and there is no accessor for it. What comes
    /// back is its visibility and its width — which is exactly what the
    /// failing assertion is about.
    fn popup(view: &sourceview5::View) -> Option<(bool, i32)> {
        let mut child = view.first_child();
        while let Some(widget) = child {
            if widget.type_().name().contains("CompletionList") {
                let (_, natural, _, _) = widget.measure(gtk::Orientation::Horizontal, -1);
                return Some((widget.is_visible(), natural));
            }
            child = widget.next_sibling();
        }
        None
    }

    /// What the popup is actually offering: every label under it, which for
    /// this provider is a `/name` and its description per row. A width above
    /// zero says a popup was presented; this says WHAT — the difference
    /// between a list that opens and a list that opens with the right
    /// commands on it.
    fn popup_rows(view: &sourceview5::View) -> Vec<String> {
        fn labels(widget: &gtk::Widget, into: &mut Vec<String>) {
            if let Some(label) = widget.downcast_ref::<gtk::Label>() {
                let text = label.text().to_string();
                if !text.is_empty() {
                    into.push(text);
                }
            }
            let mut child = widget.first_child();
            while let Some(widget) = child {
                labels(&widget, into);
                child = widget.next_sibling();
            }
        }
        let mut found = Vec::new();
        let mut child = view.first_child();
        while let Some(widget) = child {
            if widget.type_().name().contains("CompletionList") {
                labels(&widget, &mut found);
            }
            child = widget.next_sibling();
        }
        found
    }

    /// Whether the popup is on screen, and how wide it says it wants to be
    /// — zero being the state the CRITICAL is about. Settled rather than
    /// sampled: populating is asynchronous, so the answer right after a
    /// keystroke is "not yet" whichever way it is going to land. This waits
    /// for `want`, and gives up — truthfully — if it never comes.
    fn showing(view: &sourceview5::View, want: bool) -> (bool, i32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
        loop {
            let state = popup(view).unwrap_or((false, 0));
            if state.0 == want || std::time::Instant::now() > deadline {
                return state;
            }
            settle(20);
        }
    }

    /// David, 2026-09-12: `gdk_popup_present: assertion 'width > 0' failed`,
    /// once per typing burst in Dispatch (i-0023). A popup surface with no
    /// width is one a Wayland compositor may kill the whole app over, so
    /// this is the reproduction, not a cosmetic check — and the two halves
    /// are inseparable, because the obvious way to stop presenting an empty
    /// popup is to stop presenting the full one too.
    ///
    /// Ignored by default, and one test rather than several, for the same
    /// reason the profiling harness in `filetree.rs` is: this needs a real
    /// display to present a popup on, GTK may only be initialized from one
    /// thread, and libtest gives every test function its own — so a second
    /// one in the same binary panics the first time a display is actually
    /// there. Run it alone:
    ///
    ///   Xvfb :9 -screen 0 1440x900x24 & DISPLAY=:9 GDK_BACKEND=x11 \
    ///     cargo test -p taste-app command_popup -- --ignored --nocapture
    ///
    /// `build-aux/headless/typing.sh` is the same check against the whole
    /// app, needs no arranging, and is the one to reach for.
    #[test]
    #[ignore]
    fn the_command_popup_opens_over_a_command_and_never_over_a_sentence() {
        crate::gtk_test::on_gtk_thread("command completion: no display — skipped", || {
            let (window, view) = composer();
            let provider = CommandProvider::default();
            provider.set_commands(commands());
            sourceview5::prelude::ViewExt::completion(&view).add_provider(&provider);

            let logged = criticals(|| {
                // A sentence first, and one whose words PREFIX command names:
                // "container", "clone" and "comes" all start `compact`,
                // `context` and `clear`. This is the burst that failed — the
                // command list came up over ordinary prose, and every opening
                // was a chance at the zero-width popup.
                for word in "Relocation waits for the container and the clone comes up".split(' ') {
                    type_into(&view, &format!("{word} "));
                    // Asking whether it became visible, so the wait is spent
                    // looking for the fault rather than past it.
                    let (visible, width) = showing(&view, true);
                    assert!(
                        !visible,
                        "the command list opened over the ordinary word {word:?} (width {width})"
                    );
                }

                // Now a command, which still has to open the list: a slash at
                // the start of the box is the one place one can be.
                view.buffer().set_text("");
                settle(100);
                type_into(&view, "/");
                let (visible, width) = showing(&view, true);
                assert!(
                    visible && width > 0,
                    "a bare slash opened nothing (width {width})"
                );
                let rows = popup_rows(&view);
                for command in ["/compact", "/context", "/clear", "/review"] {
                    assert!(
                        rows.iter().any(|row| row == command),
                        "the whole list should be up, and {command} is not on it: {rows:?}"
                    );
                }

                // ...and narrowing it keeps it up, with the two commands that
                // still match on it and the three that no longer do off it.
                type_into(&view, "co");
                let (visible, width) = showing(&view, true);
                assert!(
                    visible && width > 0,
                    "typing the prefix closed the list (width {width})"
                );
                let rows = popup_rows(&view);
                let on = |command: &str| rows.iter().any(|row| row == command);
                assert!(
                    on("/compact") && on("/context"),
                    "typing `co` dropped a command it matches: {rows:?}"
                );
                assert!(
                    !on("/clear") && !on("/review"),
                    "typing `co` kept a command it does not match: {rows:?}"
                );

                // ...and running past the last match closes it, rather than
                // shrinking it to nothing.
                type_into(&view, "zz");
                let (visible, _) = showing(&view, false);
                assert!(!visible, "the list stayed up with nothing matching");
            });
            window.destroy();
            settle(50);
            assert!(
                logged.is_empty(),
                "GTK complained while typing: {logged:#?}"
            );
        });
    }
}

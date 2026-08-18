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
        fn is_trigger(&self, iter: &gtk::TextIter, c: char) -> bool {
            c == '/' && iter.offset() == 0
        }

        fn populate(
            &self,
            context: &sourceview5::CompletionContext,
        ) -> Result<gtk::gio::ListModel, glib::Error> {
            Ok(self.matching(context).upcast())
        }

        /// Typing narrows the list. Handing back a freshly filtered model is
        /// what keeps the popup in step with the prefix.
        fn refilter(&self, context: &sourceview5::CompletionContext, _model: &gtk::gio::ListModel) {
            let matches = self.matching(context);
            context.set_proposals_for_provider(&*self.obj(), Some(&matches));
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
        /// The commands whose name the typed prefix starts.
        fn matching(&self, context: &sourceview5::CompletionContext) -> gtk::gio::ListStore {
            let store = gtk::gio::ListStore::new::<super::CommandProposal>();
            let word = context.word();
            let prefix = word.trim_start_matches('/');
            for command in self
                .commands
                .borrow()
                .iter()
                .filter(|command| command.name.starts_with(prefix))
            {
                store.append(&super::CommandProposal::new(command));
            }
            store
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

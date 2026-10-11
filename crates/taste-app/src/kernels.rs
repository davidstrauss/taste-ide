//! The notebooks' kernels as one Tasks entry per environment.
//!
//! Each open notebook runs its own kernel (`crate::notebook`,
//! `crate::kernel`), as every Jupyter does, in the container of the
//! environment it belongs to. An environment's are started, stopped, and
//! watched from one place: the entry the IDE adds at the top of that
//! environment's Tasks, which says how they stand and runs or stops them all
//! (David, 2026-10-11: "Move the kernel runner to an entry at the top of
//! Tasks -- just one auto-added by the IDE … I should see the status of
//! the kernel in the tasks listing"). A notebook says only that its kernel
//! is not running, with a button to start it, and nothing once it is.
//!
//! GTK-thread only: the sessions it holds are.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use taste_core::environment::EnvironmentId;

use crate::notebook::Session;

/// How a notebook's kernel stands, as the entry draws it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum KernelState {
    /// Not started, or stopped.
    Off,
    /// Running, waiting for a cell.
    Idle,
    /// Starting or restarting.
    Starting,
    /// Running a cell.
    Busy,
    /// It did not start, or died.
    Failed,
}

/// The entry's line: how the kernels stand together, and what to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelSummary {
    pub state: KernelState,
    /// The subtitle: the state in words and the notebook, or how many.
    pub detail: String,
}

/// Every open notebook that can run, and who wants to hear of a change.
#[derive(Default)]
pub struct Kernels {
    sessions: RefCell<Vec<Weak<Session>>>,
    listeners: RefCell<Vec<Rc<dyn Fn()>>>,
    notifying: std::cell::Cell<bool>,
}

thread_local! {
    static KERNELS: Rc<Kernels> = Rc::new(Kernels::default());
}

/// The registry, on the GTK thread.
pub fn kernels() -> Rc<Kernels> {
    KERNELS.with(Rc::clone)
}

impl Kernels {
    /// A notebook that runs, from now until it is dropped.
    pub fn register(&self, session: &Rc<Session>) {
        self.sessions.borrow_mut().push(Rc::downgrade(session));
        self.changed();
    }

    /// Hear of every change: a kernel's state, a notebook opened or closed.
    pub fn connect(&self, listener: impl Fn() + 'static) {
        self.listeners.borrow_mut().push(Rc::new(listener));
    }

    /// Something changed; the listeners hear of it once the main loop is
    /// next idle, so a burst (a run's busy and idle) is one redraw, and a
    /// session being dropped is gone by then.
    pub fn changed(&self) {
        if self.notifying.replace(true) {
            return;
        }
        gtk::glib::idle_add_local_once(|| {
            let kernels = kernels();
            kernels.notifying.set(false);
            let listeners: Vec<Rc<dyn Fn()>> = kernels.listeners.borrow().clone();
            for listener in listeners {
                listener();
            }
        });
    }

    fn live(&self, env: &EnvironmentId) -> Vec<Rc<Session>> {
        let mut sessions = self.sessions.borrow_mut();
        sessions.retain(|weak| weak.strong_count() > 0);
        sessions
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|session| session.env() == env)
            .collect()
    }

    /// `env`'s entry, or `None` with none of its notebooks open to run.
    pub fn summary(&self, env: &EnvironmentId) -> Option<KernelSummary> {
        let live = self.live(env);
        if live.is_empty() {
            return None;
        }
        let states: Vec<(KernelState, Option<String>, String)> = live
            .iter()
            .map(|session| {
                let (state, said) = session.kernel_status();
                (state, said, session.file_name())
            })
            .collect();
        Some(summarize(&states))
    }

    /// Start each of `env`'s open notebooks' kernels that is not running.
    pub fn start_all(&self, env: &EnvironmentId) {
        for session in self.live(env) {
            session.start_kernel();
        }
    }

    /// Stop each of `env`'s running kernels.
    pub fn stop_all(&self, env: &EnvironmentId) {
        for session in self.live(env) {
            session.stop_kernel();
        }
    }
}

/// The kernels' states together: the most pressing first — a failure, then
/// work in hand, then a running one — and the words for it.
fn summarize(states: &[(KernelState, Option<String>, String)]) -> KernelSummary {
    let state = states
        .iter()
        .map(|(state, _, _)| *state)
        .max()
        .unwrap_or(KernelState::Off);
    let words = |state: KernelState| match state {
        KernelState::Off => "not running",
        KernelState::Idle => "idle",
        KernelState::Starting => "starting",
        KernelState::Busy => "running a cell",
        KernelState::Failed => "did not start",
    };
    let detail = match states {
        [(state, said, name)] => match said {
            Some(said) if *state == KernelState::Failed => format!("{name}: {said}"),
            _ => format!("{} · {name}", words(*state)),
        },
        _ => {
            let failed = states
                .iter()
                .find(|(state, _, _)| *state == KernelState::Failed);
            match failed {
                Some((_, Some(said), name)) => format!("{name}: {said}"),
                _ => {
                    let running = states
                        .iter()
                        .filter(|(state, _, _)| *state != KernelState::Off)
                        .count();
                    format!(
                        "{} · {running} of {} notebooks running",
                        words(state),
                        states.len()
                    )
                }
            }
        }
    };
    KernelSummary { state, detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_says_the_most_pressing_state() {
        let one = summarize(&[(KernelState::Idle, None, "a.ipynb".into())]);
        assert_eq!(one.state, KernelState::Idle);
        assert_eq!(one.detail, "idle · a.ipynb");
        let failed = summarize(&[(
            KernelState::Failed,
            Some("no jupyter_client".into()),
            "a.ipynb".into(),
        )]);
        assert_eq!(failed.detail, "a.ipynb: no jupyter_client");
        let two = summarize(&[
            (KernelState::Off, None, "a.ipynb".into()),
            (KernelState::Busy, None, "b.ipynb".into()),
        ]);
        assert_eq!(two.state, KernelState::Busy);
        assert_eq!(two.detail, "running a cell · 1 of 2 notebooks running");
    }
}

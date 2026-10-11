//! Each environment's notebook kernel: the project's `jupyter:kernel` task.
//!
//! The kernel is the project's to run (`taste_core::kernel_task`): one per
//! environment, started by its `jupyter:kernel` task and shared by the
//! notebooks open in it, each of which connects to it (`crate::kernel`).
//! Its row in Tasks is that task's own, which says how the kernel stands
//! (David, 2026-10-11: "I should see the status of the kernel in the tasks
//! listing"; "rather than faking a task, we should instruct the agent to
//! define a task to run the kernel at an expected name"). A notebook says
//! only that its kernel is not running, with a button to start it, or that
//! the project does not define the task, with one to ask the agent for it.
//!
//! This is where the notebooks and the task meet: which environments
//! define the task, starting and restarting it, and telling the notebooks
//! when it starts and stops. GTK-thread only.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

use taste_core::environment::EnvironmentId;
use taste_core::kernel_task::TASK;
use taste_core::tasks::{RunState, TaskBoard};

use crate::notebook::Session;

/// How a notebook's connection to the kernel stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum KernelState {
    /// Not connected: the task is not running, or nothing has asked.
    Off,
    /// Connected, waiting for a cell.
    Idle,
    /// Connecting, or the task starting.
    Starting,
    /// Running a cell.
    Busy,
    /// It did not connect, or the task failed.
    Failed,
}

/// An environment's exec context, files, and checkout root: what listing
/// its tasks takes.
pub type Where = (
    taste_core::ExecContext,
    taste_core::files::Files,
    std::path::PathBuf,
);

/// The window's part: where an environment's tasks are listed, and the
/// board that runs them.
pub struct Hooks {
    /// `None` for an environment that is gone.
    pub lister: Rc<dyn Fn(&EnvironmentId) -> Option<Where>>,
    pub board: TaskBoard,
}

/// Every open notebook, and what is known of each environment's task.
#[derive(Default)]
pub struct Kernels {
    sessions: RefCell<Vec<Weak<Session>>>,
    listeners: RefCell<Vec<Rc<dyn Fn()>>>,
    notifying: Cell<bool>,
    hooks: RefCell<Option<Rc<Hooks>>>,
    /// Whether each environment's Taskfile defines the task, as last
    /// listed; absent until it has been.
    defined: RefCell<HashMap<EnvironmentId, bool>>,
    /// Environments being listed now.
    listing: RefCell<HashSet<EnvironmentId>>,
    /// Environments whose task is to be started again once it has stopped:
    /// a restart.
    restarting: RefCell<HashSet<EnvironmentId>>,
}

thread_local! {
    static KERNELS: Rc<Kernels> = Rc::new(Kernels::default());
}

/// The registry, on the GTK thread.
pub fn kernels() -> Rc<Kernels> {
    KERNELS.with(Rc::clone)
}

impl Kernels {
    pub fn set_hooks(&self, hooks: Hooks) {
        *self.hooks.borrow_mut() = Some(Rc::new(hooks));
    }

    /// A notebook that runs, from now until it is dropped.
    pub fn register(&self, session: &Rc<Session>) {
        self.sessions.borrow_mut().push(Rc::downgrade(session));
        self.check(session.env());
        self.changed();
    }

    /// Hear of every change: a kernel's state, a notebook opened or closed,
    /// the task found or lost.
    pub fn connect(&self, listener: impl Fn() + 'static) {
        self.listeners.borrow_mut().push(Rc::new(listener));
    }

    /// Something changed; the listeners hear of it once the main loop is
    /// next idle, so a burst (a run's busy and idle) is one redraw.
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

    /// Whether `env`'s Taskfile defines the task: `None` until listed.
    pub fn defined(&self, env: &EnvironmentId) -> Option<bool> {
        self.defined.borrow().get(env).copied()
    }

    /// What a listing of `env`'s tasks found: the window's own, for the
    /// environment the panes are aimed at, and this registry's.
    pub fn set_defined(&self, env: &EnvironmentId, defined: bool) {
        let before = self.defined.borrow_mut().insert(env.clone(), defined);
        if before != Some(defined) {
            for session in self.live(env) {
                session.task_changed();
            }
            self.changed();
        }
    }

    /// List `env`'s tasks again, off the main thread, for whether it
    /// defines the task.
    pub fn check(&self, env: &EnvironmentId) {
        let Some(hooks) = self.hooks.borrow().clone() else {
            return;
        };
        if !self.listing.borrow_mut().insert(env.clone()) {
            return;
        }
        let Some((exec, files, root)) = (hooks.lister)(env) else {
            self.listing.borrow_mut().remove(env);
            return;
        };
        let env = env.clone();
        gtk::glib::spawn_future_local(async move {
            let listed = crate::runtime::runtime()
                .spawn_blocking(move || crate::tasks::list(&exec, &files, &root))
                .await;
            let kernels = kernels();
            kernels.listing.borrow_mut().remove(&env);
            let defined = match listed {
                Ok(crate::tasks::Listing::Tasks { tasks, .. }) => {
                    tasks.iter().any(|task| task.name == TASK)
                }
                Ok(crate::tasks::Listing::NoTaskfile) => false,
                Err(_) => return,
            };
            kernels.set_defined(&env, defined);
        });
    }

    /// How `env`'s task stands.
    pub fn task_state(&self, env: &EnvironmentId) -> RunState {
        self.hooks
            .borrow()
            .as_ref()
            .map_or(RunState::Idle, |hooks| hooks.board.state(env, TASK))
    }

    /// What `env`'s task's last run said, for a failure's report.
    pub fn task_lines(&self, env: &EnvironmentId) -> Vec<String> {
        self.hooks
            .borrow()
            .as_ref()
            .map(|hooks| hooks.board.lines(env, TASK))
            .unwrap_or_default()
    }

    /// Start `env`'s task in `exec`, unless it is running. `Err` says why
    /// it was not.
    pub fn start_task(
        &self,
        env: &EnvironmentId,
        exec: &taste_core::ExecContext,
    ) -> Result<(), String> {
        let Some(hooks) = self.hooks.borrow().clone() else {
            return Err("the IDE cannot run tasks yet".into());
        };
        if hooks.board.state(env, TASK) == RunState::Running {
            return Ok(());
        }
        let _in = crate::runtime::runtime().enter();
        hooks.board.start(env, TASK, exec, false)
    }

    /// Restart `env`'s kernel: its task stopped, and started again once it
    /// has (`task_changed`); started at once when it is not running.
    pub fn restart(&self, env: &EnvironmentId) {
        let Some(hooks) = self.hooks.borrow().clone() else {
            return;
        };
        if hooks.board.state(env, TASK) == RunState::Running {
            self.restarting.borrow_mut().insert(env.clone());
            hooks.board.stop(env, TASK);
        } else {
            for session in self.live(env) {
                session.start_kernel();
            }
        }
    }

    /// A run of a task started or ended (`Event::TaskState`): the
    /// notebooks of its environment follow the kernel's task.
    pub fn task_changed(&self, env: &EnvironmentId, name: &str) {
        if name != TASK {
            return;
        }
        let running = self.task_state(env) == RunState::Running;
        let restart = !running && self.restarting.borrow_mut().remove(env);
        for session in self.live(env) {
            if restart {
                session.start_kernel();
            } else {
                session.task_changed();
            }
        }
        self.changed();
    }

    /// What the task's row adds to its subtitle: how the kernel stands with
    /// the notebooks connected to it; `None` with none of them open.
    pub fn row_note(&self, env: &EnvironmentId) -> Option<String> {
        let live = self.live(env);
        if live.is_empty() {
            return None;
        }
        let states: Vec<KernelState> = live
            .iter()
            .map(|session| session.kernel_status().0)
            .collect();
        Some(row_note(&states))
    }
}

/// The kernel's state in a few words, with how many notebooks are on it:
/// the busiest of them says it, since they share one kernel.
fn row_note(states: &[KernelState]) -> String {
    let connected = states
        .iter()
        .filter(|state| matches!(state, KernelState::Idle | KernelState::Busy))
        .count();
    let words = if states.contains(&KernelState::Busy) {
        "kernel running a cell"
    } else if connected > 0 {
        "kernel idle"
    } else if states.contains(&KernelState::Starting) {
        "kernel starting"
    } else if states.contains(&KernelState::Failed) {
        "kernel not answering"
    } else {
        "kernel not connected"
    };
    match connected {
        0 => words.to_string(),
        1 => format!("{words} · 1 notebook"),
        n => format!("{words} · {n} notebooks"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_task_row_says_how_the_shared_kernel_stands() {
        use KernelState as K;
        assert_eq!(row_note(&[K::Idle]), "kernel idle · 1 notebook");
        assert_eq!(
            row_note(&[K::Idle, K::Busy]),
            "kernel running a cell · 2 notebooks"
        );
        assert_eq!(row_note(&[K::Off]), "kernel not connected");
        assert_eq!(row_note(&[K::Starting, K::Off]), "kernel starting");
        assert_eq!(row_note(&[K::Failed]), "kernel not answering");
    }
}

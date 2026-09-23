//! One thread for every test that touches GTK.
//!
//! GTK is single-threaded, and the test harness is not: each test runs on
//! a thread of its own, several at once. Tests that each called
//! `gtk::init()` on their own thread raced GTK's initialisation against
//! itself — on CI, with no display, every one of them was inside
//! `gtk_init_check` at the same moment — and the test binary died of a
//! SIGSEGV now and then, with no test named (CI, 2026-09-23). So GTK is
//! initialised once, on a thread kept for it, and each test's GTK work is
//! run there in turn, its panic carried back to fail the test that asked.

use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};

type Job = Box<dyn FnOnce() + Send>;

/// The GTK thread's queue, and whether GTK came up on it.
struct GtkThread {
    jobs: Mutex<mpsc::Sender<Job>>,
    display: bool,
}

fn gtk_thread() -> &'static GtkThread {
    static THREAD: OnceLock<GtkThread> = OnceLock::new();
    THREAD.get_or_init(|| {
        let (jobs, queue) = mpsc::channel::<Job>();
        let (ready, answer) = mpsc::channel();
        std::thread::Builder::new()
            .name("gtk-tests".into())
            .spawn(move || {
                let _ = ready.send(gtk::init().is_ok());
                for job in queue {
                    job();
                }
            })
            .expect("starting the GTK test thread");
        GtkThread {
            jobs: Mutex::new(jobs),
            display: answer.recv().unwrap_or(false),
        }
    })
}

/// Run `test` on the GTK thread and wait for it; a panic in it fails the
/// caller. Without a display it is not run, and `skipped` is printed, as
/// each of these tests always said when it skipped itself.
pub fn on_gtk_thread(skipped: &str, test: impl FnOnce() + Send + 'static) {
    let thread = gtk_thread();
    if !thread.display {
        println!("{skipped}");
        return;
    }
    let (done, outcome) = mpsc::channel();
    let job: Job = Box::new(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test));
        let _ = done.send(result);
    });
    thread
        .jobs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .send(job)
        .expect("the GTK test thread is gone");
    match outcome.recv().expect("the GTK test thread dropped a test") {
        Ok(()) => {}
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

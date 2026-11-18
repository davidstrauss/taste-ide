//! A lock-order cycle found in the running IDE goes to the log rather than
//! panicking (docs/LOCKS.md).
//!
//! Every std lock in the workspace is tracing-mutex's, which in a debug
//! build panics where a lock taken would close a cycle in the order locks
//! have been taken in. That is what a test wants: the gate fails on the
//! order, without the deadlock ever having to happen. It is not what the
//! IDE the user is working in wants, whose debug build is the one run day
//! to day: a cycle is a deadlock that has not happened yet, and a panic
//! on a GTK callback aborts the process now. So the app reports it, with
//! the stack that closed it, and carries on.
//!
//! The handler runs inside the lock it reports, holding tracing-mutex's
//! own handler lock and whatever the thread already held, so it does
//! nothing that could take a lock of ours: the report is sent to a thread
//! that logs it, since logging pushes to `taste_core::app_log`, which is
//! locked. A release build's locks are std's, and the handler is never
//! called.

use std::sync::atomic::{AtomicUsize, Ordering};

/// How many cycles are reported in one run. A cycle on a hot path is taken
/// again at every acquisition, and each report resolves a backtrace.
const MOST: usize = 8;

pub fn report_cycles() {
    let (reports, received) = std::sync::mpsc::channel::<String>();
    let reported = AtomicUsize::new(0);
    tracing_mutex::util::set_cycle_handler(move |message| {
        if reported.fetch_add(1, Ordering::Relaxed) >= MOST {
            return;
        }
        let here = std::backtrace::Backtrace::force_capture();
        let _ = reports.send(format!("{message}\nThe lock that closed it:\n{here}"));
    });
    let logger = std::thread::Builder::new()
        .name("lock-order".into())
        .spawn(move || {
            for report in received {
                tracing::error!("lock order: {report}");
            }
        });
    if let Err(e) = logger {
        tracing::warn!("lock-order cycles will not be logged: {e}");
    }
}

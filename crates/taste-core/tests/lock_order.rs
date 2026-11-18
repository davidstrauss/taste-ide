//! The gate checks lock order (docs/LOCKS.md): a test that takes two of
//! the workspace's locks in opposite orders fails, whether or not the two
//! orders ever meet. This holds that true, so a build that stops tracing
//! (a test profile without debug assertions, a lock that is std's again)
//! fails here rather than letting every other test pass unchecked.

use std::sync::Arc;
use tracing_mutex::stdsync::Mutex;

/// `ide_exec`'s registry and one of its jobs, as they were before 80fe82e6:
/// one poll took the registry and then the job, another the job and then
/// the registry. Here the two run one after the other, on two threads, so
/// nothing can deadlock, and the second is refused all the same.
#[test]
fn opposite_orders_fail_without_meeting() {
    let registry = Arc::new(Mutex::new(()));
    let job = Arc::new(Mutex::new(()));

    let (r, j) = (registry.clone(), job.clone());
    std::thread::spawn(move || {
        let _registry = r.lock().unwrap();
        let _job = j.lock().unwrap();
    })
    .join()
    .expect("the first order is fine on its own");

    let (r, j) = (registry.clone(), job.clone());
    let refused = std::thread::spawn(move || {
        let _job = j.lock().unwrap();
        let _registry = r.lock().unwrap();
    })
    .join()
    .expect_err("the opposite order closes a cycle");
    let message = refused
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_default();
    assert!(message.contains("cycle"), "{message}");

    // The lock refused is not poisoned, since it was never taken; the one
    // the panicking thread held is, as any std lock is.
    assert!(!registry.is_poisoned());
    assert!(job.is_poisoned());
}

/// The same order taken again and again is no cycle.
#[test]
fn one_order_is_fine() {
    let outer = Mutex::new(0);
    let inner = Mutex::new(0);
    for _ in 0..3 {
        let mut a = outer.lock().unwrap();
        let mut b = inner.lock().unwrap();
        *a += 1;
        *b += 1;
    }
}

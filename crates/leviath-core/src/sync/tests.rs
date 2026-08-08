//! Tests for poison-recovering locks.
//!
//! A sibling file so the scaffolding stays out of the coverage report, matching
//! the layout used elsewhere in the workspace.

use super::*;

#[test]
fn an_ordinary_lock_yields_its_value() {
    let m = Mutex::new(vec![1, 2, 3]);
    assert_eq!(lock(&m).len(), 3);
    lock(&m).push(4);
    assert_eq!(*lock(&m), vec![1, 2, 3, 4]);
}

#[test]
fn a_poisoned_mutex_still_yields_its_value() {
    let m = std::sync::Arc::new(Mutex::new(vec![1, 2, 3]));
    let poisoner = std::sync::Arc::clone(&m);
    // Panic while holding the guard, which is what poisons it. The panic is
    // caught so the test process survives; the hook is silenced so the expected
    // panic does not look like a failure in the output.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(move || {
        let _guard = poisoner.lock().expect("first lock is clean");
        panic!("poison it");
    });
    std::panic::set_hook(hook);
    assert!(result.is_err(), "the closure should have panicked");
    assert!(m.is_poisoned(), "the mutex should now be poisoned");

    // The whole point: the value is still readable, and still intact, because
    // the section that panicked had not modified it.
    assert_eq!(*lock(&m), vec![1, 2, 3]);
    lock(&m).push(4);
    assert_eq!(*lock(&m), vec![1, 2, 3, 4]);
}

#[test]
fn try_lock_reports_contention_but_not_poisoning() {
    let m = Mutex::new(7);
    assert_eq!(try_lock(&m).map(|g| *g), Some(7));

    // Held elsewhere: `None`, and no blocking.
    let held = lock(&m);
    assert!(
        try_lock(&m).is_none(),
        "a held mutex should not be handed out"
    );
    drop(held);
    assert_eq!(try_lock(&m).map(|g| *g), Some(7));
}

#[test]
fn try_lock_recovers_a_poisoned_but_free_mutex() {
    let m = std::sync::Arc::new(Mutex::new(7));
    let poisoner = std::sync::Arc::clone(&m);
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let _ = std::panic::catch_unwind(move || {
        let _guard = poisoner.lock().expect("first lock is clean");
        panic!("poison it");
    });
    std::panic::set_hook(hook);
    assert!(m.is_poisoned());
    // Poisoned but nobody is holding it, so this is a hand-out, not a `None`.
    assert_eq!(try_lock(&m).map(|g| *g), Some(7));
}

//! Taking a lock without a panic path.
//!
//! `std::sync::Mutex::lock` returns a `Result` because a thread that panics
//! while holding the guard *poisons* it, and every later locker is told so.
//! Answering that with `.expect("...")` turns one unrelated panic into a
//! daemon-wide cascade: the first failure poisons a telemetry mutex, and the
//! next observation - on a healthy run, in a different subsystem - aborts the
//! process.
//!
//! # Why recovering is sound here, and would not be everywhere
//!
//! Poisoning is not noise. It reports a real condition: a writer stopped
//! mid-update, so the value may be half-written and its invariants may not hold.
//! Ignoring that in general is how a corrupt state gets read as a good one, and
//! a crate that reaches for [`lock`] to silence a poison it has not thought
//! about has made its data *less* trustworthy, not more.
//!
//! What makes it sound in this workspace is a property of the call sites rather
//! than of this function: **every critical section guarded this way is
//! panic-free.** They take the lock, do one bounded thing to a container - push,
//! clone, read a field, swap an `Option` - and drop it. No indexing, no
//! `unwrap`, no arithmetic that can overflow, no user input parsed, no callback
//! invoked. A section like that has no intermediate state to be caught in: a
//! `Vec::push` either happened or did not, and the `Vec` is a valid `Vec` either
//! way. Poisoning is therefore unreachable, and this function's recovery is not
//! papering over a risk - it is stating that the risk does not exist and
//! degrading gracefully if a future edit ever creates one.
//!
//! That invariant is the thing to protect. When adding a call, keep the section
//! short enough that "can anything in here panic?" is answerable by reading it.
//! If the answer is ever no, hoist the fallible work out of the lock rather than
//! reaching for this.
//!
//! Locks held across an `.await` are `tokio::sync::Mutex`, which has no
//! poisoning to begin with and is unaffected by any of this.

use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};

/// Take `mutex`, recovering the guard if a previous holder panicked.
///
/// See the module docs: this is only correct because the sections it guards
/// cannot panic, so the recovery branch is unreachable rather than merely
/// tolerated.
pub fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// [`lock`], but gives up rather than waiting when the mutex is already held.
///
/// `None` means "someone else has it right now", not "it is broken": a poisoned
/// but free mutex still yields its guard, for the reason [`lock`] documents.
pub fn try_lock<T: ?Sized>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

#[cfg(test)]
mod tests;

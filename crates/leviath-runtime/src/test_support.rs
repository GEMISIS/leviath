#![cfg(test)]
//! Crate-local names for the shared helpers in `leviath-testkit`, plus the ones
//! that only this crate needs.

pub(crate) use leviath_testkit::{PANIC_HOOK_LOCK, with_silenced_panics, with_tracing};

/// Silence the process panic hook for as long as the guard lives.
///
/// `with_silenced_panics` takes a synchronous closure, which is enough when the
/// panic happens inside it. The lane tests provoke a panic on a *tokio worker*,
/// some time after the spawning call returns, so the hook has to stay swapped
/// across an await - hence a guard rather than a closure.
pub(crate) struct SilentPanics {
    /// Held for the guard's lifetime: swapping the process-global hook has to be
    /// serialized against every other test that does it.
    _lock: std::sync::MutexGuard<'static, ()>,
    /// The hook to put back. `Option` because `Drop` only has `&mut self` and
    /// `set_hook` needs the box by value.
    previous: Option<PanicHook>,
}

/// What `std::panic::take_hook` hands back, named so the field above doesn't
/// trip `clippy::type_complexity`.
type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

impl SilentPanics {
    pub(crate) fn install() -> Self {
        let lock = PANIC_HOOK_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        Self {
            _lock: lock,
            previous: Some(previous),
        }
    }
}

impl Drop for SilentPanics {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            std::panic::set_hook(previous);
        }
    }
}

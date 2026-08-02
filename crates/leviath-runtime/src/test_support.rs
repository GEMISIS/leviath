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
    /// The hook to put back.
    previous: PanicHook,
}

/// What `std::panic::take_hook` hands back, named so the field above doesn't
/// trip `clippy::type_complexity`.
type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// The hook installed while the guard lives: print nothing. A named function
/// rather than two `|_| {}` closures so the silencer and the throwaway below are
/// the same (exercised) code.
fn swallow_panic(_: &std::panic::PanicHookInfo<'_>) {}

impl SilentPanics {
    pub(crate) fn install() -> Self {
        let lock = PANIC_HOOK_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(swallow_panic));
        Self {
            _lock: lock,
            previous,
        }
    }
}

impl Drop for SilentPanics {
    fn drop(&mut self) {
        // Swap a throwaway in so the real hook can be moved out of `&mut self`.
        // (An `Option` + `take` would work too, but its `None` arm can never be
        // reached, and the coverage gate counts that.)
        let previous = std::mem::replace(&mut self.previous, Box::new(swallow_panic));
        std::panic::set_hook(previous);
    }
}

/// The global end of the system-prompt hint cascade for a spawn under test,
/// where only the batch-tool hint is ever varied. The shell hint follows it so
/// the two never disagree in a test that only cares about one of them; a test
/// that does care builds the struct itself.
pub(crate) fn hints(on: bool) -> leviath_core::config::PromptHints {
    leviath_core::config::PromptHints {
        batch_tool: on,
        shell: on,
    }
}

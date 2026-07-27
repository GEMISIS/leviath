//! Rendering a caught panic payload as a human-readable message.
//!
//! Every place that recovers from a panic with [`std::panic::catch_unwind`]
//! needs the same three-way downcast, so it lives here once rather than being
//! re-derived per crate (`leviath-scripting`'s Rhai host-function guards and
//! `leviath-runtime`'s pipeline-tick isolation both use it).

use std::any::Any;

/// Text used when a panic payload is neither a `String` nor a `&'static str`
/// (e.g. `std::panic::panic_any(42)`).
const UNKNOWN: &str = "unknown panic";

/// Render a [`catch_unwind`](std::panic::catch_unwind) payload as a message.
///
/// `panic!("{x}")` yields a `String` payload and `panic!("literal")` a
/// `&'static str`; anything else (`panic_any`) has no text to show and renders
/// as `"unknown panic"`.
pub fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    UNKNOWN.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `f`, expecting it to panic, and return the rendered payload.
    fn caught(f: impl FnOnce()) -> String {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // silence the expected panic
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
            .expect_err("the closure must panic");
        std::panic::set_hook(prev);
        panic_message(payload.as_ref())
    }

    #[test]
    fn renders_all_three_payload_kinds() {
        // `panic!("{}", …)` → String payload.
        let formatted = "formatted message";
        assert_eq!(caught(|| panic!("{formatted}")), "formatted message");
        // `panic!("literal")` → &'static str payload.
        assert_eq!(caught(|| panic!("literal message")), "literal message");
        // Anything else has no text.
        assert_eq!(caught(|| std::panic::panic_any(42_i32)), UNKNOWN);
    }
}

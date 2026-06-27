//! Leviath functions exposed to Rhai scripts.

use rhai::Engine;

/// Register Leviath functions in the Rhai engine.
pub fn register_functions(engine: &mut Engine) {
    // String operations
    engine.register_fn("contains", |text: &str, pattern: &str| -> bool {
        text.contains(pattern)
    });

    engine.register_fn("join", |arr: rhai::Array, separator: &str| -> String {
        arr.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(separator)
    });

    engine.register_fn("split", |text: &str, separator: &str| -> rhai::Array {
        text.split(separator)
            .map(|s| rhai::Dynamic::from(s.to_string()))
            .collect()
    });

    // Token counting (approximate for now)
    engine.register_fn("count_tokens", |text: &str| -> i64 {
        ((text.len() + 3) / 4) as i64
    });

    // Content validation
    engine.register_fn("is_json", |text: &str| -> bool {
        serde_json::from_str::<serde_json::Value>(text).is_ok()
    });

    engine.register_fn("is_empty", |text: &str| -> bool {
        text.trim().is_empty()
    });
}

//! Leviath types registered in Rhai.

use rhai::Engine;

/// Register Leviath types in the Rhai engine.
pub fn register_types(engine: &mut Engine) {
    // Region kind constructors
    engine.register_fn("region_pinned", || -> String {
        "pinned".to_string()
    });

    engine.register_fn("region_temporary", || -> String {
        "temporary".to_string()
    });

    engine.register_fn("region_clearable", || -> String {
        "clearable".to_string()
    });

    engine.register_fn("region_sliding_window", |max_items: i64| -> rhai::Map {
        let mut map = rhai::Map::new();
        map.insert("kind".into(), rhai::Dynamic::from("sliding_window".to_string()));
        map.insert("max_items".into(), rhai::Dynamic::from(max_items));
        map
    });

    engine.register_fn("region_compacting", |threshold: i64| -> rhai::Map {
        let mut map = rhai::Map::new();
        map.insert("kind".into(), rhai::Dynamic::from("compacting".to_string()));
        map.insert(
            "threshold_tokens".into(),
            rhai::Dynamic::from(threshold),
        );
        map
    });

    // Region entry constructor
    engine.register_fn("region_entry", |content: String, tokens: i64| -> rhai::Map {
        let mut map = rhai::Map::new();
        map.insert("content".into(), rhai::Dynamic::from(content));
        map.insert("tokens".into(), rhai::Dynamic::from(tokens));
        map
    });

    // Content format validator
    engine.register_fn("content_format", |format: &str| -> String {
        match format {
            "text" | "json" | "mermaid" | "markdown" | "code" => format.to_string(),
            _ => "text".to_string(),
        }
    });

    // Token budget helpers
    engine.register_fn(
        "tokens_remaining",
        |max_tokens: i64, current_tokens: i64| -> i64 {
            max_tokens - current_tokens
        },
    );

    engine.register_fn(
        "usage_ratio",
        |max_tokens: i64, current_tokens: i64| -> f64 {
            if max_tokens == 0 {
                return 1.0;
            }
            current_tokens as f64 / max_tokens as f64
        },
    );

    engine.register_fn(
        "needs_eviction",
        |max_tokens: i64, current_tokens: i64, threshold: f64| -> bool {
            if max_tokens == 0 {
                return true;
            }
            (current_tokens as f64 / max_tokens as f64) >= threshold
        },
    );
}

//! Leviath types registered in Rhai.

use rhai::Engine;

/// Register Leviath types in the Rhai engine.
pub fn register_types(engine: &mut Engine) {
    // Region kind constructors
    engine.register_fn("region_pinned", || -> String { "pinned".to_string() });

    engine.register_fn("region_temporary", || -> String { "temporary".to_string() });

    engine.register_fn("region_clearable", || -> String { "clearable".to_string() });

    engine.register_fn("region_sliding_window", |max_items: i64| -> rhai::Map {
        let mut map = rhai::Map::new();
        map.insert(
            "kind".into(),
            rhai::Dynamic::from("sliding_window".to_string()),
        );
        map.insert("max_items".into(), rhai::Dynamic::from(max_items));
        map
    });

    engine.register_fn("region_compacting", |threshold: i64| -> rhai::Map {
        let mut map = rhai::Map::new();
        map.insert("kind".into(), rhai::Dynamic::from("compacting".to_string()));
        map.insert("threshold_tokens".into(), rhai::Dynamic::from(threshold));
        map
    });

    engine.register_fn(
        "region_custom",
        |script: String, persistent: bool| -> rhai::Map {
            let mut map = rhai::Map::new();
            map.insert("kind".into(), rhai::Dynamic::from("custom".to_string()));
            map.insert("script".into(), rhai::Dynamic::from(script));
            map.insert("persistent".into(), rhai::Dynamic::from(persistent));
            map
        },
    );

    // Region entry constructor
    engine.register_fn(
        "region_entry",
        |content: String, tokens: i64| -> rhai::Map {
            let mut map = rhai::Map::new();
            map.insert("content".into(), rhai::Dynamic::from(content));
            map.insert("tokens".into(), rhai::Dynamic::from(tokens));
            map
        },
    );

    // Content format validator
    engine.register_fn("content_format", |format: &str| -> String {
        match format {
            "text" | "json" | "mermaid" | "markdown" | "code" => format.to_string(),
            _ => "text".to_string(),
        }
    });

    // Token budget helpers. The operands come from a script (ultimately from
    // model output), so plain `-` would panic on overflow in a debug build -
    // and a panic inside a Rhai native fn aborts the process (#109).
    engine.register_fn(
        "tokens_remaining",
        |max_tokens: i64, current_tokens: i64| -> i64 { max_tokens.saturating_sub(current_tokens) },
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

#[cfg(test)]
mod tests {
    use super::*;
    use rhai::Engine;

    fn engine() -> Engine {
        let mut e = Engine::new();
        register_types(&mut e);
        e
    }

    // --- region constructors ---

    #[test]
    fn region_pinned_returns_pinned() {
        let e = engine();
        let result: String = e.eval("region_pinned()").unwrap();
        assert_eq!(result, "pinned");
    }

    #[test]
    fn region_temporary_returns_temporary() {
        let e = engine();
        let result: String = e.eval("region_temporary()").unwrap();
        assert_eq!(result, "temporary");
    }

    #[test]
    fn region_clearable_returns_clearable() {
        let e = engine();
        let result: String = e.eval("region_clearable()").unwrap();
        assert_eq!(result, "clearable");
    }

    // --- region_sliding_window ---

    #[test]
    fn region_sliding_window_returns_map_with_kind_and_max_items() {
        let e = engine();
        let result: rhai::Map = e.eval("region_sliding_window(10)").unwrap();
        assert_eq!(
            result.get("kind").unwrap().clone_cast::<String>(),
            "sliding_window"
        );
        assert_eq!(result.get("max_items").unwrap().clone_cast::<i64>(), 10);
    }

    #[test]
    fn region_sliding_window_zero() {
        let e = engine();
        let result: rhai::Map = e.eval("region_sliding_window(0)").unwrap();
        assert_eq!(result.get("max_items").unwrap().clone_cast::<i64>(), 0);
    }

    // --- region_compacting ---

    #[test]
    fn region_compacting_returns_map_with_kind_and_threshold() {
        let e = engine();
        let result: rhai::Map = e.eval("region_compacting(5000)").unwrap();
        assert_eq!(
            result.get("kind").unwrap().clone_cast::<String>(),
            "compacting"
        );
        assert_eq!(
            result.get("threshold_tokens").unwrap().clone_cast::<i64>(),
            5000
        );
    }

    // --- region_custom ---

    #[test]
    fn region_custom_returns_map_with_script_and_persistent() {
        let e = engine();
        let result: rhai::Map = e.eval(r#"region_custom("hooks/conv.rhai", true)"#).unwrap();
        assert_eq!(result.get("kind").unwrap().clone_cast::<String>(), "custom");
        assert_eq!(
            result.get("script").unwrap().clone_cast::<String>(),
            "hooks/conv.rhai"
        );
        assert!(result.get("persistent").unwrap().clone_cast::<bool>());
    }

    #[test]
    fn region_custom_non_persistent() {
        let e = engine();
        let result: rhai::Map = e.eval(r#"region_custom("r.rhai", false)"#).unwrap();
        assert!(!result.get("persistent").unwrap().clone_cast::<bool>());
    }

    // --- region_entry ---

    #[test]
    fn region_entry_returns_map_with_content_and_tokens() {
        let e = engine();
        let result: rhai::Map = e.eval(r#"region_entry("content", 42)"#).unwrap();
        assert_eq!(
            result.get("content").unwrap().clone_cast::<String>(),
            "content"
        );
        assert_eq!(result.get("tokens").unwrap().clone_cast::<i64>(), 42);
    }

    #[test]
    fn region_entry_empty_content() {
        let e = engine();
        let result: rhai::Map = e.eval(r#"region_entry("", 0)"#).unwrap();
        assert_eq!(result.get("content").unwrap().clone_cast::<String>(), "");
        assert_eq!(result.get("tokens").unwrap().clone_cast::<i64>(), 0);
    }

    // --- content_format ---

    #[test]
    fn content_format_valid_formats() {
        let e = engine();
        for fmt in &["text", "json", "mermaid", "markdown", "code"] {
            let script = format!(r#"content_format("{fmt}")"#);
            let result: String = e.eval(&script).unwrap();
            assert_eq!(result, *fmt);
        }
    }

    #[test]
    fn content_format_invalid_falls_back_to_text() {
        let e = engine();
        let result: String = e.eval(r#"content_format("invalid")"#).unwrap();
        assert_eq!(result, "text");
    }

    #[test]
    fn content_format_empty_falls_back_to_text() {
        let e = engine();
        let result: String = e.eval(r#"content_format("")"#).unwrap();
        assert_eq!(result, "text");
    }

    // --- tokens_remaining ---

    #[test]
    fn tokens_remaining_basic() {
        let e = engine();
        let result: i64 = e.eval("tokens_remaining(100, 30)").unwrap();
        assert_eq!(result, 70);
    }

    #[test]
    fn tokens_remaining_zero_used() {
        let e = engine();
        let result: i64 = e.eval("tokens_remaining(100, 0)").unwrap();
        assert_eq!(result, 100);
    }

    #[test]
    fn tokens_remaining_all_used() {
        let e = engine();
        let result: i64 = e.eval("tokens_remaining(100, 100)").unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn tokens_remaining_saturates_instead_of_overflowing() {
        // The operands come from a script, so extreme values must not panic -
        // a panic in a Rhai native fn aborts the daemon (issue #109).
        let e = engine();
        let low: i64 = e.eval("tokens_remaining(-9223372036854775808, 1)").unwrap();
        assert_eq!(low, i64::MIN);
        let high: i64 = e.eval("tokens_remaining(9223372036854775807, -1)").unwrap();
        assert_eq!(high, i64::MAX);
    }

    // --- usage_ratio ---

    #[test]
    fn usage_ratio_half() {
        let e = engine();
        let result: f64 = e.eval("usage_ratio(100, 50)").unwrap();
        assert!((result - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn usage_ratio_zero_max_returns_one() {
        let e = engine();
        let result: f64 = e.eval("usage_ratio(0, 50)").unwrap();
        assert!((result - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn usage_ratio_none_used() {
        let e = engine();
        let result: f64 = e.eval("usage_ratio(100, 0)").unwrap();
        assert!((result - 0.0).abs() < f64::EPSILON);
    }

    // --- needs_eviction ---

    #[test]
    fn needs_eviction_above_threshold() {
        let e = engine();
        let result: bool = e.eval("needs_eviction(100, 90, 0.8)").unwrap();
        assert!(result);
    }

    #[test]
    fn needs_eviction_below_threshold() {
        let e = engine();
        let result: bool = e.eval("needs_eviction(100, 50, 0.8)").unwrap();
        assert!(!result);
    }

    #[test]
    fn needs_eviction_at_exact_threshold() {
        let e = engine();
        let result: bool = e.eval("needs_eviction(100, 80, 0.8)").unwrap();
        assert!(result);
    }

    #[test]
    fn needs_eviction_zero_max_returns_true() {
        let e = engine();
        let result: bool = e.eval("needs_eviction(0, 0, 0.8)").unwrap();
        assert!(result);
    }
}

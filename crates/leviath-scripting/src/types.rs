//! The helpers every Rhai engine has besides its own surface: a content
//! format check and the budget arithmetic a region hook needs.

use rhai::Engine;

/// Register the content format check and the token budget helpers.
pub fn register_types(engine: &mut Engine) {
    // Content format validator
    engine.register_fn("content_format", |format: &str| -> String {
        match format {
            "text" | "json" | "mermaid" | "markdown" | "code" => format.to_string(),
            _ => "text".to_string(),
        }
    });

    // Token budget helpers. The operands come from a script (ultimately from
    // model output), so plain `-` would panic on overflow in a debug build -
    // and a panic inside a Rhai native fn aborts the process.
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
        let mut e = crate::sandboxed(1_000_000);
        register_types(&mut e);
        e
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
        // a panic in a Rhai native fn aborts the daemon.
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

//! Leviath functions exposed to Rhai scripts.

use rhai::Engine;

/// Register Leviath functions in the Rhai engine.
pub fn register_functions(engine: &mut Engine) {
    // String operations
    engine.register_fn("contains", |text: &str, pattern: &str| -> bool {
        text.contains(pattern)
    });

    engine.register_fn("starts_with", |text: &str, pattern: &str| -> bool {
        text.starts_with(pattern)
    });

    engine.register_fn("ends_with", |text: &str, pattern: &str| -> bool {
        text.ends_with(pattern)
    });

    engine.register_fn("trim", |text: &str| -> String { text.trim().to_string() });

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
        text.len().div_ceil(4) as i64
    });

    // Content validation
    engine.register_fn("is_json", |text: &str| -> bool {
        serde_json::from_str::<serde_json::Value>(text).is_ok()
    });

    engine.register_fn("is_mermaid", |text: &str| -> bool {
        text.contains("graph")
            || text.contains("sequenceDiagram")
            || text.contains("classDiagram")
            || text.contains("stateDiagram")
            || text.contains("erDiagram")
            || text.contains("flowchart")
    });

    engine.register_fn("is_markdown", |text: &str| -> bool {
        // Very permissive - just check for common markdown markers
        text.contains("##") || text.contains("**") || text.contains("```") || !text.is_empty()
    });

    engine.register_fn("is_empty", |text: &str| -> bool { text.trim().is_empty() });

    // Summarization placeholder (requires LLM provider, not available in scripting layer)
    // Users should implement this at the application level
    engine.register_fn("summarize", |text: &str, max_tokens: i64| -> String {
        // Simple truncation as placeholder
        let target_chars = (max_tokens * 4) as usize;
        if text.len() <= target_chars {
            text.to_string()
        } else {
            format!("{}...", &text[..target_chars])
        }
    });

    // Extract modified files/content
    // This is a placeholder - real implementation would parse structured data
    engine.register_fn("extract_modified", |content: &str| -> rhai::Array {
        // Look for lines starting with common markers
        content
            .lines()
            .filter(|line| {
                line.starts_with("modified:")
                    || line.starts_with("changed:")
                    || line.starts_with("updated:")
                    || line.starts_with("M ")
            })
            .map(|line| rhai::Dynamic::from(line.to_string()))
            .collect()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhai::Engine;

    fn engine() -> Engine {
        let mut e = Engine::new();
        register_functions(&mut e);
        e
    }

    // --- contains ---

    #[test]
    fn contains_returns_true_when_pattern_present() {
        let e = engine();
        let result: bool = e.eval(r#"contains("hello world", "world")"#).unwrap();
        assert!(result);
    }

    #[test]
    fn contains_returns_false_when_pattern_absent() {
        let e = engine();
        let result: bool = e.eval(r#"contains("hello", "xyz")"#).unwrap();
        assert!(!result);
    }

    #[test]
    fn contains_empty_pattern_always_matches() {
        let e = engine();
        let result: bool = e.eval(r#"contains("hello", "")"#).unwrap();
        assert!(result);
    }

    // --- starts_with ---

    #[test]
    fn starts_with_true() {
        let e = engine();
        let result: bool = e.eval(r#"starts_with("hello", "he")"#).unwrap();
        assert!(result);
    }

    #[test]
    fn starts_with_false() {
        let e = engine();
        let result: bool = e.eval(r#"starts_with("hello", "lo")"#).unwrap();
        assert!(!result);
    }

    // --- ends_with ---

    #[test]
    fn ends_with_true() {
        let e = engine();
        let result: bool = e.eval(r#"ends_with("hello", "lo")"#).unwrap();
        assert!(result);
    }

    #[test]
    fn ends_with_false() {
        let e = engine();
        let result: bool = e.eval(r#"ends_with("hello", "he")"#).unwrap();
        assert!(!result);
    }

    // --- trim ---

    #[test]
    fn trim_removes_whitespace() {
        let e = engine();
        let result: String = e.eval(r#"trim("  hi  ")"#).unwrap();
        assert_eq!(result, "hi");
    }

    #[test]
    fn trim_no_op_on_clean_string() {
        let e = engine();
        let result: String = e.eval(r#"trim("hi")"#).unwrap();
        assert_eq!(result, "hi");
    }

    // --- join ---

    #[test]
    fn join_with_comma() {
        let e = engine();
        let result: String = e.eval(r#"join(["a", "b", "c"], ",")"#).unwrap();
        assert_eq!(result, "a,b,c");
    }

    #[test]
    fn join_empty_array() {
        let e = engine();
        let result: String = e.eval(r#"join([], ",")"#).unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn join_single_element() {
        let e = engine();
        let result: String = e.eval(r#"join(["only"], "-")"#).unwrap();
        assert_eq!(result, "only");
    }

    // --- split ---

    #[test]
    fn split_by_comma() {
        let e = engine();
        let result: rhai::Array = e.eval(r#"split("a,b,c", ",")"#).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].clone_cast::<String>(), "a");
        assert_eq!(result[1].clone_cast::<String>(), "b");
        assert_eq!(result[2].clone_cast::<String>(), "c");
    }

    #[test]
    fn split_no_separator_found() {
        let e = engine();
        let result: rhai::Array = e.eval(r#"split("abc", ",")"#).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].clone_cast::<String>(), "abc");
    }

    // --- count_tokens ---

    #[test]
    fn count_tokens_approximate() {
        let e = engine();
        let result: i64 = e.eval(r#"count_tokens("hello world")"#).unwrap();
        // "hello world" is 11 chars, ceil(11/4) = 3
        assert_eq!(result, 3);
    }

    #[test]
    fn count_tokens_empty() {
        let e = engine();
        let result: i64 = e.eval(r#"count_tokens("")"#).unwrap();
        assert_eq!(result, 0);
    }

    // --- is_json ---

    #[test]
    fn is_json_valid_object() {
        let e = engine();
        let result: bool = e.eval(r#"is_json("{}")"#).unwrap();
        assert!(result);
    }

    #[test]
    fn is_json_valid_array() {
        let e = engine();
        let result: bool = e.eval(r#"is_json("[1,2,3]")"#).unwrap();
        assert!(result);
    }

    #[test]
    fn is_json_invalid() {
        let e = engine();
        let result: bool = e.eval(r#"is_json("not json")"#).unwrap();
        assert!(!result);
    }

    // --- is_mermaid ---

    #[test]
    fn is_mermaid_with_graph_keyword() {
        let e = engine();
        let result: bool = e.eval(r#"is_mermaid("graph TD; A-->B")"#).unwrap();
        assert!(result);
    }

    #[test]
    fn is_mermaid_with_sequence_diagram() {
        let e = engine();
        let result: bool = e
            .eval(r#"is_mermaid("sequenceDiagram\nA->>B: Hi")"#)
            .unwrap();
        assert!(result);
    }

    #[test]
    fn is_mermaid_with_flowchart() {
        let e = engine();
        let result: bool = e.eval(r#"is_mermaid("flowchart LR")"#).unwrap();
        assert!(result);
    }

    #[test]
    fn is_mermaid_false_for_plain_text() {
        let e = engine();
        let result: bool = e.eval(r#"is_mermaid("just some text")"#).unwrap();
        assert!(!result);
    }

    // --- is_markdown ---

    #[test]
    fn is_markdown_with_heading() {
        let e = engine();
        let script = "is_markdown(\"## Heading\")";
        let result: bool = e.eval(script).unwrap();
        assert!(result);
    }

    #[test]
    fn is_markdown_with_bold() {
        let e = engine();
        let result: bool = e.eval(r#"is_markdown("some **bold** text")"#).unwrap();
        assert!(result);
    }

    #[test]
    fn is_markdown_with_code_fence() {
        let e = engine();
        let result: bool = e.eval(r#"is_markdown("```code```")"#).unwrap();
        assert!(result);
    }

    // --- is_empty ---

    #[test]
    fn is_empty_true_for_empty() {
        let e = engine();
        let result: bool = e.eval(r#"is_empty("")"#).unwrap();
        assert!(result);
    }

    #[test]
    fn is_empty_true_for_whitespace_only() {
        let e = engine();
        let result: bool = e.eval(r#"is_empty("   ")"#).unwrap();
        assert!(result);
    }

    #[test]
    fn is_empty_false_for_content() {
        let e = engine();
        let result: bool = e.eval(r#"is_empty("hi")"#).unwrap();
        assert!(!result);
    }

    // --- summarize ---

    #[test]
    fn summarize_short_text_unchanged() {
        let e = engine();
        let result: String = e.eval(r#"summarize("hello", 100)"#).unwrap();
        assert_eq!(result, "hello");
    }

    #[test]
    fn summarize_long_text_truncated() {
        let e = engine();
        // max_tokens=2 => target_chars=8, "hello world" is 11 chars => truncated
        let result: String = e.eval(r#"summarize("hello world", 2)"#).unwrap();
        assert!(result.ends_with("..."));
        assert!(result.len() < "hello world".len() + 3);
    }

    // --- extract_modified ---

    #[test]
    fn extract_modified_finds_markers() {
        let e = engine();
        let result: rhai::Array = e
            .eval(r#"extract_modified("modified: a.rs\nother line\nchanged: b.rs\nM c.rs")"#)
            .unwrap();
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn extract_modified_no_matches() {
        let e = engine();
        let result: rhai::Array = e
            .eval(r#"extract_modified("no markers here\njust text")"#)
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn extract_modified_updated_marker() {
        let e = engine();
        let result: rhai::Array = e.eval(r#"extract_modified("updated: file.txt")"#).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].clone_cast::<String>(), "updated: file.txt");
    }
}

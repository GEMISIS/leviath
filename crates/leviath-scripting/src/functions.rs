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

    engine.register_fn("trim", |text: &str| -> String {
        text.trim().to_string()
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

    engine.register_fn("is_empty", |text: &str| -> bool {
        text.trim().is_empty()
    });

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
        content.lines()
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

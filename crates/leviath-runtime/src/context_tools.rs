//! Handling for the `context_*` self-management tools (`context_write`,
//! `context_append`, `context_read`, `context_delete`, `context_list`).
//!
//! These let an agent read and edit its own context-window regions. They operate
//! directly on the [`ContextWindow`], so in the shared world they are applied by
//! a pipeline system (which holds the ECS window) rather than by the async tool
//! lane - [`handle_context_tool`] is the window-level core it calls. Ported from
//! the imperative worker's `handle_context_tool`.

use leviath_core::RegionKind;

use crate::components::ContextWindow;

/// Whether a tool name is a context self-management tool this module handles.
pub fn is_context_tool(name: &str) -> bool {
    name.starts_with("context_")
}

/// Build the "section not found" error listing the writable regions.
fn region_not_found(name: &str, window: &ContextWindow) -> String {
    let available: Vec<&str> = window
        .regions
        .iter()
        .filter(|r| !matches!(r.kind, RegionKind::CompactHistory { .. }))
        .filter(|r| r.name != "conversation")
        .map(|r| r.name.as_str())
        .collect();
    format!(
        "[error] Section '{}' not found. Available sections: {}",
        name,
        available.join(", ")
    )
}

/// Apply one `context_*` tool call to `window`, returning the result text the
/// model sees. Unknown tools and missing arguments yield `[error] …` strings
/// (never a hard failure - the model reads and adjusts).
pub fn handle_context_tool(
    name: &str,
    args: &serde_json::Value,
    window: &mut ContextWindow,
) -> String {
    match name {
        "context_write" => {
            let Some(region_name) = args.get("region").and_then(|v| v.as_str()) else {
                return "[error] missing 'region' argument".to_string();
            };
            let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
                return "[error] missing 'content' argument".to_string();
            };
            let key = args.get("key").and_then(|v| v.as_str());
            let tokens = leviath_core::estimate_tokens(content);

            let is_hashmap = match window.get_region(region_name) {
                Some(r) => matches!(r.kind, RegionKind::HashMap { .. }),
                None => return region_not_found(region_name, window),
            };
            let region = window.get_region_mut(region_name).expect("region present");
            if is_hashmap {
                let Some(k) = key else {
                    return "[error] HashMap regions require a 'key' argument".to_string();
                };
                match region.upsert_by_key(k, content, tokens) {
                    Ok(()) => format!("Stored in '{region_name}' section under key '{k}'."),
                    Err(e) => format!("[error] {e}"),
                }
            } else {
                // Through the window method (not region.add_entry directly)
                // so a custom region's on_write hook sees the write.
                region.clear();
                window.current_tokens = window.calculate_tokens();
                match window.add_to_region(region_name, content.to_string(), tokens) {
                    Ok(()) => format!("Stored in '{region_name}' section."),
                    Err(e) => format!("[error] {e}"),
                }
            }
        }
        "context_append" => {
            let Some(region_name) = args.get("region").and_then(|v| v.as_str()) else {
                return "[error] missing 'region' argument".to_string();
            };
            let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
                return "[error] missing 'content' argument".to_string();
            };
            let key = args.get("key").and_then(|v| v.as_str());
            let tokens = leviath_core::estimate_tokens(content);

            let is_hashmap = match window.get_region(region_name) {
                Some(r) => matches!(r.kind, RegionKind::HashMap { .. }),
                None => return region_not_found(region_name, window),
            };
            let region = window.get_region_mut(region_name).expect("region present");
            if is_hashmap {
                let Some(k) = key else {
                    return "[error] HashMap regions require a 'key' argument for append"
                        .to_string();
                };
                if let Some(existing) = region.get_by_key(k) {
                    let new_content = format!("{}\n{}", existing.content(), content);
                    let new_tokens = leviath_core::estimate_tokens(&new_content);
                    // Upserting an already-present key updates in place with no
                    // budget check, so this cannot fail.
                    region
                        .upsert_by_key(k, new_content, new_tokens)
                        .expect("infallible: existing HashMap key updates in place");
                    format!("Appended to '{region_name}' section under key '{k}'.")
                } else {
                    match region.upsert_by_key(k, content, tokens) {
                        Ok(()) => {
                            format!("Created entry in '{region_name}' section under key '{k}'.")
                        }
                        Err(e) => format!("[error] {e}"),
                    }
                }
            } else {
                // Same routing rationale as context_write above.
                match window.add_to_region(region_name, content.to_string(), tokens) {
                    Ok(()) => format!("Appended to '{region_name}' section."),
                    Err(e) => format!("[error] {e}"),
                }
            }
        }
        "context_read" => {
            let Some(region_name) = args.get("region").and_then(|v| v.as_str()) else {
                return "[error] missing 'region' argument".to_string();
            };
            let key = args.get("key").and_then(|v| v.as_str());
            let region = match window.get_region(region_name) {
                Some(r) => r,
                None => return region_not_found(region_name, window),
            };
            if matches!(region.kind, RegionKind::HashMap { .. }) {
                if let Some(k) = key {
                    match region.get_by_key(k) {
                        Some(entry) => entry.content_owned(),
                        None => {
                            format!("[not found] No entry with key '{k}' in region '{region_name}'")
                        }
                    }
                } else {
                    let mut lines = Vec::new();
                    for entry in &region.content {
                        if let Some(k) = &entry.key {
                            lines.push(format!("  {} ({} tokens)", k, entry.tokens));
                        }
                    }
                    if lines.is_empty() {
                        format!("Section '{region_name}' is empty.")
                    } else {
                        format!("Section '{region_name}' entries:\n{}", lines.join("\n"))
                    }
                }
            } else {
                let text = region
                    .content
                    .iter()
                    .map(|e| e.content())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if text.is_empty() {
                    format!("Section '{region_name}' is empty.")
                } else {
                    text
                }
            }
        }
        "context_delete" => {
            let Some(region_name) = args.get("region").and_then(|v| v.as_str()) else {
                return "[error] missing 'region' argument".to_string();
            };
            let Some(key) = args.get("key").and_then(|v| v.as_str()) else {
                return "[error] missing 'key' argument".to_string();
            };
            if window.get_region(region_name).is_none() {
                return region_not_found(region_name, window);
            }
            let region = window.get_region_mut(region_name).expect("region present");
            if region.remove_by_key(key) {
                format!("Removed '{key}' from '{region_name}' section.")
            } else {
                format!("[not found] No entry with key '{key}' in region '{region_name}'")
            }
        }
        "context_list" => {
            let region_name = args.get("region").and_then(|v| v.as_str());
            if let Some(rname) = region_name {
                let region = match window.get_region(rname) {
                    Some(r) => r,
                    None => return region_not_found(rname, window),
                };
                let mut lines = Vec::new();
                for entry in &region.content {
                    match &entry.key {
                        Some(k) => lines.push(format!("  {} ({} tokens)", k, entry.tokens)),
                        None => lines.push(format!("  (entry, {} tokens)", entry.tokens)),
                    }
                }
                if lines.is_empty() {
                    format!("Section '{rname}' is empty.")
                } else {
                    format!(
                        "Region '{}' ({} entries, {} tokens):\n{}",
                        rname,
                        region.content.len(),
                        region.current_tokens,
                        lines.join("\n")
                    )
                }
            } else {
                let mut lines = Vec::new();
                for region in &window.regions {
                    let kind_str = match &region.kind {
                        RegionKind::Pinned => "permanent",
                        RegionKind::SlidingWindow { .. } => "conversation",
                        RegionKind::Temporary => "temporary",
                        RegionKind::Compacting { .. } => "summarized when full",
                        RegionKind::Clearable => "temporary",
                        RegionKind::CompactHistory { .. } => "summary archive",
                        RegionKind::HashMap { .. } => "key-value store",
                        RegionKind::Custom { .. } => "scripted",
                    };
                    lines.push(format!(
                        "  {} ({}): {} entries, {}/{} tokens",
                        region.name,
                        kind_str,
                        region.content.len(),
                        region.current_tokens,
                        region.max_tokens
                    ));
                }
                if lines.is_empty() {
                    "No context window sections configured.".to_string()
                } else {
                    format!("Context window sections:\n{}", lines.join("\n"))
                }
            }
        }
        _ => format!("[error] Unknown context tool: {name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::{EvictionStrategy, Region};
    use serde_json::json;

    /// A window with one region of each relevant kind.
    fn win() -> ContextWindow {
        let mut w = ContextWindow::new(100_000);
        w.add_region(Region::new("task".to_string(), RegionKind::Pinned, 5000));
        w.add_region(Region::new(
            "notes".to_string(),
            RegionKind::Clearable,
            5000,
        ));
        w.add_region(Region::new(
            "files".to_string(),
            RegionKind::HashMap { max_entries: None },
            5000,
        ));
        w.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 10,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            5000,
        ));
        w.add_region(Region::new(
            "archive".to_string(),
            RegionKind::CompactHistory {
                source_region: "notes".to_string(),
            },
            5000,
        ));
        w
    }

    fn call(w: &mut ContextWindow, name: &str, args: serde_json::Value) -> String {
        handle_context_tool(name, &args, w)
    }

    #[test]
    fn detects_context_tools() {
        assert!(is_context_tool("context_write"));
        assert!(!is_context_tool("read_file"));
    }

    // ── context_write ──
    #[test]
    fn write_branches() {
        let mut w = win();
        assert!(
            call(&mut w, "context_write", json!({"content": "x"})).contains("missing 'region'")
        );
        assert!(
            call(&mut w, "context_write", json!({"region": "notes"})).contains("missing 'content'")
        );
        // Region-not-found lists writable regions (excludes conversation + history).
        let nf = call(
            &mut w,
            "context_write",
            json!({"region": "ghost", "content": "c"}),
        );
        assert!(nf.contains("not found"));
        assert!(nf.contains("task") && nf.contains("files") && !nf.contains("conversation"));
        // HashMap without key.
        assert!(
            call(
                &mut w,
                "context_write",
                json!({"region": "files", "content": "c"})
            )
            .contains("require a 'key'")
        );
        // HashMap ok.
        assert!(
            call(
                &mut w,
                "context_write",
                json!({"region": "files", "content": "c", "key": "a.rs"})
            )
            .contains("under key 'a.rs'")
        );
        // Non-HashMap ok (replaces).
        assert!(
            call(
                &mut w,
                "context_write",
                json!({"region": "notes", "content": "hello"})
            )
            .contains("Stored in 'notes'")
        );
    }

    #[test]
    fn write_over_budget_errors() {
        let mut w = ContextWindow::new(100_000);
        w.add_region(Region::new("tiny".to_string(), RegionKind::Clearable, 2));
        w.add_region(Region::new(
            "tinymap".to_string(),
            RegionKind::HashMap { max_entries: None },
            2,
        ));
        let big = "x".repeat(400); // ~100 tokens > budget 2
        assert!(
            call(
                &mut w,
                "context_write",
                json!({"region": "tiny", "content": big.clone()})
            )
            .starts_with("[error]")
        );
        assert!(
            call(
                &mut w,
                "context_write",
                json!({"region": "tinymap", "content": big, "key": "k"})
            )
            .starts_with("[error]")
        );
    }

    // ── context_append ──
    #[test]
    fn append_branches() {
        let mut w = win();
        assert!(
            call(&mut w, "context_append", json!({"content": "x"})).contains("missing 'region'")
        );
        assert!(
            call(&mut w, "context_append", json!({"region": "notes"}))
                .contains("missing 'content'")
        );
        assert!(
            call(
                &mut w,
                "context_append",
                json!({"region": "ghost", "content": "c"})
            )
            .contains("not found")
        );
        // HashMap append without key.
        assert!(
            call(
                &mut w,
                "context_append",
                json!({"region": "files", "content": "c"})
            )
            .contains("require a 'key'")
        );
        // HashMap append new key.
        assert!(
            call(
                &mut w,
                "context_append",
                json!({"region": "files", "content": "c1", "key": "k"})
            )
            .contains("Created entry")
        );
        // HashMap append existing key.
        assert!(
            call(
                &mut w,
                "context_append",
                json!({"region": "files", "content": "c2", "key": "k"})
            )
            .contains("Appended to 'files'")
        );
        // Non-HashMap append.
        assert!(
            call(
                &mut w,
                "context_append",
                json!({"region": "notes", "content": "line"})
            )
            .contains("Appended to 'notes'")
        );
    }

    #[test]
    fn append_over_budget_errors() {
        let mut w = ContextWindow::new(100_000);
        w.add_region(Region::new("tiny".to_string(), RegionKind::Clearable, 2));
        w.add_region(Region::new(
            "tinymap".to_string(),
            RegionKind::HashMap { max_entries: None },
            2,
        ));
        let big = "x".repeat(400);
        assert!(
            call(
                &mut w,
                "context_append",
                json!({"region": "tiny", "content": big.clone()})
            )
            .starts_with("[error]")
        );
        // New key over budget.
        assert!(
            call(
                &mut w,
                "context_append",
                json!({"region": "tinymap", "content": big, "key": "k"})
            )
            .starts_with("[error]")
        );
    }

    // ── context_read ──
    #[test]
    fn read_branches() {
        let mut w = win();
        assert!(call(&mut w, "context_read", json!({})).contains("missing 'region'"));
        assert!(call(&mut w, "context_read", json!({"region": "ghost"})).contains("not found"));

        // HashMap empty (no key) + non-empty listing.
        assert!(
            call(&mut w, "context_read", json!({"region": "files"})).contains("files' is empty")
        );
        call(
            &mut w,
            "context_write",
            json!({"region": "files", "content": "c", "key": "a.rs"}),
        );
        // A stray keyless entry in a hashmap region is skipped by the listing.
        w.get_region_mut("files")
            .unwrap()
            .add_entry("keyless", 1)
            .unwrap();
        let listing = call(&mut w, "context_read", json!({"region": "files"}));
        assert!(listing.contains("a.rs") && !listing.contains("keyless"));
        // HashMap key found + not found.
        assert_eq!(
            call(
                &mut w,
                "context_read",
                json!({"region": "files", "key": "a.rs"})
            ),
            "c"
        );
        assert!(
            call(
                &mut w,
                "context_read",
                json!({"region": "files", "key": "z"})
            )
            .contains("[not found]")
        );

        // Non-HashMap empty + with content.
        assert!(call(&mut w, "context_read", json!({"region": "notes"})).contains("is empty"));
        call(
            &mut w,
            "context_write",
            json!({"region": "notes", "content": "body"}),
        );
        assert_eq!(
            call(&mut w, "context_read", json!({"region": "notes"})),
            "body"
        );
    }

    // ── context_delete ──
    #[test]
    fn delete_branches() {
        let mut w = win();
        assert!(call(&mut w, "context_delete", json!({})).contains("missing 'region'"));
        assert!(
            call(&mut w, "context_delete", json!({"region": "files"})).contains("missing 'key'")
        );
        assert!(
            call(
                &mut w,
                "context_delete",
                json!({"region": "ghost", "key": "k"})
            )
            .contains("not found")
        );
        // Not present.
        assert!(
            call(
                &mut w,
                "context_delete",
                json!({"region": "files", "key": "z"})
            )
            .contains("[not found]")
        );
        // Present.
        call(
            &mut w,
            "context_write",
            json!({"region": "files", "content": "c", "key": "k"}),
        );
        assert!(
            call(
                &mut w,
                "context_delete",
                json!({"region": "files", "key": "k"})
            )
            .contains("Removed 'k'")
        );
    }

    // ── context_list ──
    #[test]
    fn list_branches() {
        let mut w = win();
        // Specific region not found.
        assert!(call(&mut w, "context_list", json!({"region": "ghost"})).contains("not found"));
        // Specific region empty.
        assert!(call(&mut w, "context_list", json!({"region": "notes"})).contains("is empty"));
        // Specific region with a keyless entry (non-hashmap) and a keyed entry (hashmap).
        call(
            &mut w,
            "context_write",
            json!({"region": "notes", "content": "n"}),
        );
        assert!(call(&mut w, "context_list", json!({"region": "notes"})).contains("(entry,"));
        call(
            &mut w,
            "context_write",
            json!({"region": "files", "content": "c", "key": "a.rs"}),
        );
        assert!(call(&mut w, "context_list", json!({"region": "files"})).contains("a.rs ("));

        // All regions - covers every kind's label.
        let all = call(&mut w, "context_list", json!({}));
        for label in [
            "permanent",
            "conversation",
            "key-value store",
            "summary archive",
        ] {
            assert!(all.contains(label), "missing label {label} in: {all}");
        }
    }

    #[test]
    fn list_covers_temporary_and_compacting_and_empty() {
        let mut w = ContextWindow::new(100_000);
        w.add_region(Region::new("t".to_string(), RegionKind::Temporary, 100));
        w.add_region(Region::new(
            "c".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 5,
            },
            100,
        ));
        w.add_region(Region::new(
            "brain".to_string(),
            RegionKind::Custom {
                script: "b.rhai".to_string(),
                persistent: false,
            },
            100,
        ));
        let all = call(&mut w, "context_list", json!({}));
        assert!(all.contains("temporary") && all.contains("summarized when full"));
        assert!(all.contains("scripted"), "custom region label: {all}");

        // No regions configured.
        let mut empty = ContextWindow::new(100_000);
        assert!(call(&mut empty, "context_list", json!({})).contains("No context window sections"));
    }

    #[test]
    fn unknown_tool_errors() {
        let mut w = win();
        assert!(call(&mut w, "context_frobnicate", json!({})).contains("Unknown context tool"));
    }
}

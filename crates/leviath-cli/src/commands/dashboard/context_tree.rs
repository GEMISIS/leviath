//! The structured Context view: a collapsible region → entry tree instead of
//! one giant flat scroll.
//!
//! The old view flattened every region and every entry into a single wrapped
//! paragraph; exploring a large window meant scrolling through everything.
//! Here each region is a header row (token bar intact), its entries are
//! one-line stubs with a preview, and Enter/Space on a row folds or unfolds
//! it. The pure functions in this module compute the interactive row list and
//! the rendered lines; the cursor and expansion state live in
//! [`super::types::ContextTreeState`].

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::*;
use super::types::ContextTreeState;
use crate::commands::dashboard::helpers::format_tokens;
use crate::runstate::ContextSnapshot;

/// One interactive (cursor-addressable) row of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TreeRow {
    RegionHeader { region: String },
    EntryStub { region: String, index: usize },
}

/// The cursor-row sequence for `snap` under `tree`'s fold state: every region
/// header, then a stub per entry unless the region is folded. `searching`
/// forces everything visible so match navigation can reach any line.
pub(super) fn rows(
    snap: &ContextSnapshot,
    tree: &ContextTreeState,
    searching: bool,
) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    for region in &snap.regions {
        rows.push(TreeRow::RegionHeader {
            region: region.name.clone(),
        });
        if searching || !tree.collapsed_regions.contains(&region.name) {
            for index in 0..region.entries.len() {
                rows.push(TreeRow::EntryStub {
                    region: region.name.clone(),
                    index,
                });
            }
        }
    }
    rows
}

/// Whether an entry's full content should render: explicitly expanded, or a
/// search is active (matches must be reachable inside collapsed entries).
pub(super) fn entry_expanded(
    tree: &ContextTreeState,
    searching: bool,
    region: &str,
    index: usize,
) -> bool {
    searching || tree.expanded_entries.contains(&(region.to_string(), index))
}

/// The rendered tree plus, per interactive row, the index of its line in the
/// output - what the renderer uses to place the cursor and follow it.
pub(super) struct FlatTree {
    pub(super) lines: Vec<Line<'static>>,
    /// `cursor_lines[i]` = index into `lines` of interactive row `i`
    /// (parallel to [`rows`]).
    pub(super) cursor_lines: Vec<usize>,
}

/// Render the region tree. `cursor` highlights that interactive row.
pub(super) fn flatten(
    snap: &ContextSnapshot,
    tree: &ContextTreeState,
    cursor: usize,
    searching: bool,
    render_width: u16,
) -> FlatTree {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cursor_lines: Vec<usize> = Vec::new();
    let mut row_idx = 0usize;

    for region in &snap.regions {
        let folded = !searching && tree.collapsed_regions.contains(&region.name);
        let on_cursor = cursor == row_idx;
        cursor_lines.push(lines.len());
        row_idx += 1;

        let pct = (region.current_tokens * 100)
            .checked_div(region.max_tokens)
            .unwrap_or(0)
            .min(100);
        let bar_w = 16usize;
        let filled = bar_w * pct / 100;
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_w - filled));
        let bar_color = if pct >= 90 {
            C_ERROR
        } else if pct >= 70 {
            C_WARN
        } else if pct > 0 {
            C_SUCCESS
        } else {
            C_DIM
        };
        // Two spellings each for the sliding window and the compacted
        // history: the blueprint's, which is what a snapshot writes now, and
        // the short one older snapshots on this disk still carry.
        let kind_color = match region.kind.as_str() {
            "pinned" => C_ACCENT,
            "sliding_window" | "sliding" => C_SUCCESS,
            "compacting" | "compact_history" | "history" => C_WARN,
            "temporary" | "clearable" => C_MUTED,
            "custom" => C_SCRIPT,
            _ => C_DIM,
        };
        let name_style = if on_cursor {
            Style::default()
                .fg(C_WHITE)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)
        };
        lines.push(Line::from(vec![
            Span::styled(
                if folded { "▸ " } else { "▾ " },
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{:<16}", region.name), name_style),
            Span::styled(
                // 16, because `compact_history` is fifteen characters and a
                // kind wider than its column pushes that row's bar out of line
                // with every other row.
                format!("{:<16}", region.kind),
                Style::default().fg(kind_color),
            ),
            Span::styled(bar, Style::default().fg(bar_color)),
            Span::styled(
                format!(
                    "  {}/{}",
                    format_tokens(region.current_tokens),
                    format_tokens(region.max_tokens)
                ),
                Style::default().fg(C_DIM),
            ),
        ]));

        // What the blueprint says the region is for, under its header. Only
        // when the region is open: a folded row is a summary, and a sentence
        // of prose under it would defeat folding.
        if !folded && let Some(description) = region.description.as_deref() {
            lines.push(Line::from(Span::styled(
                format!("    {description}"),
                Style::default().fg(C_MUTED).add_modifier(Modifier::ITALIC),
            )));
        }

        if folded {
            if !region.entries.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("    ({} entries folded)", region.entries.len()),
                    Style::default().fg(C_DIM),
                )));
            }
            continue;
        }

        if region.entries.is_empty() {
            lines.push(Line::from(Span::styled(
                "    (empty)",
                Style::default().fg(C_DIM),
            )));
            continue;
        }

        for (index, entry) in region.entries.iter().enumerate() {
            let expanded = entry_expanded(tree, searching, &region.name, index);
            let on_cursor = cursor == row_idx;
            cursor_lines.push(lines.len());
            row_idx += 1;

            let preview: String = entry
                .content
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            let stub_style = if on_cursor {
                Style::default()
                    .fg(C_WHITE)
                    .add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(C_MUTED)
            };
            let mut spans = vec![
                Span::styled(
                    if expanded { "  ▾ " } else { "  ▸ " },
                    Style::default().fg(C_ACCENT),
                ),
                Span::styled(
                    format!("entry {} · {} tokens", index + 1, entry.tokens),
                    stub_style,
                ),
            ];
            if !expanded && !preview.is_empty() {
                spans.push(Span::styled(
                    format!("  {preview}"),
                    Style::default().fg(C_DIM),
                ));
            }
            lines.push(Line::from(spans));

            if expanded {
                let rendered =
                    crate::render::markdown_to_text(&entry.content, render_width.saturating_sub(4));
                for mut l in rendered.lines {
                    l.spans.insert(0, Span::raw("    "));
                    lines.push(l);
                }
            }
        }
        lines.push(Line::from(""));
    }

    FlatTree {
        lines,
        cursor_lines,
    }
}

/// The previous/next region-header row from `cursor` (for `[` / `]`).
pub(super) fn nearest_region_row(rows: &[TreeRow], cursor: usize, forward: bool) -> Option<usize> {
    if forward {
        rows.iter()
            .enumerate()
            .skip(cursor + 1)
            .find(|(_, r)| matches!(r, TreeRow::RegionHeader { .. }))
            .map(|(i, _)| i)
    } else {
        rows.iter()
            .enumerate()
            .take(cursor)
            .rev()
            .find(|(_, r)| matches!(r, TreeRow::RegionHeader { .. }))
            .map(|(i, _)| i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runstate::{ContextSnapshot, RegionSnapshot};
    use leviath_core::run_meta::RegionEntrySnapshot;

    fn entry(content: &str) -> RegionEntrySnapshot {
        RegionEntrySnapshot {
            content: content.to_string(),
            tokens: 5,
            kind: Default::default(),
            metadata: None,
            key: None,
            taint: Default::default(),
        }
    }

    fn snap() -> ContextSnapshot {
        ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 30,
            max_tokens: 100,
            regions: vec![
                RegionSnapshot {
                    name: "system".to_string(),
                    kind: "pinned".to_string(),
                    current_tokens: 10,
                    max_tokens: 50,
                    entries: vec![entry("first entry line\nmore"), entry("second")],
                    description: None,
                },
                RegionSnapshot {
                    name: "conversation".to_string(),
                    kind: "sliding".to_string(),
                    current_tokens: 20,
                    max_tokens: 50,
                    entries: vec![],
                    description: None,
                },
            ],
        }
    }

    /// Both spellings of a kind get the same colour and the same cell width.
    ///
    /// A `context.json` written by an older daemon says `sliding` where one
    /// written today says `sliding_window`, and nothing rewrites those files,
    /// so both are on disk and both get drawn. The width is the half that is
    /// easy to miss: the kind sits in a fixed column with the token bar after
    /// it, and a word too long for the column pushes that row's bar out of
    /// line with every other row.
    #[test]
    fn a_region_kind_keeps_its_colour_and_its_column() {
        let cell = |kind: &str| {
            let mut s = snap();
            s.regions[1].kind = kind.to_string();
            let span = flatten(&s, &ContextTreeState::default(), 0, false, 80)
                .lines
                .into_iter()
                .flat_map(|line| line.spans)
                .find(|span| span.content.trim() == kind)
                .expect("the kind cell");
            (span.style, span.content.chars().count())
        };
        for (old, new) in [
            ("sliding", "sliding_window"),
            ("history", "compact_history"),
        ] {
            assert_eq!(cell(old), cell(new));
        }
    }

    #[test]
    fn rows_list_headers_and_stubs_and_fold_hides_stubs() {
        let s = snap();
        let mut tree = ContextTreeState::default();
        let all = rows(&s, &tree, false);
        assert_eq!(
            all.len(),
            4,
            "two headers + two stubs (the empty region has none)"
        );
        assert!(matches!(&all[0], TreeRow::RegionHeader { region } if region == "system"));
        assert!(matches!(&all[1], TreeRow::EntryStub { region, index: 0 } if region == "system"));

        tree.collapsed_regions.insert("system".to_string());
        let folded = rows(&s, &tree, false);
        assert_eq!(folded.len(), 2, "the folded region's stubs are gone");

        // A live search overrides folds so matches stay reachable.
        assert_eq!(rows(&s, &tree, true).len(), 4);
    }

    /// A region's description belongs where the person is already looking at
    /// the region, not in the manifest they would otherwise have to go and open.
    #[test]
    fn a_region_description_is_shown_under_its_header_while_the_region_is_open() {
        let mut s = snap();
        s.regions[0].description = Some("Format: one line per source.".to_string());
        let mut tree = ContextTreeState::default();

        let text_of = |flat: &FlatTree| -> String {
            flat.lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|sp| sp.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let text = text_of(&flatten(&s, &tree, 0, false, 80));
        assert!(text.contains("Format: one line per source."));

        // Folded, it is gone: a folded row is a summary, and a sentence of
        // prose under it would defeat the fold.
        tree.collapsed_regions.insert("system".to_string());
        let folded = text_of(&flatten(&s, &tree, 0, false, 80));
        assert!(!folded.contains("Format: one line per source."));
        assert!(
            folded.contains("system"),
            "the region itself is still listed"
        );
    }

    #[test]
    fn flatten_marks_folds_previews_and_expansions() {
        let s = snap();
        let mut tree = ContextTreeState::default();

        let text_of = |flat: &FlatTree| -> String {
            flat.lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|sp| sp.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Default: stubs with previews, no full content, an (empty) marker.
        let flat = flatten(&s, &tree, 0, false, 80);
        let text = text_of(&flat);
        assert!(text.contains("entry 1 · 5 tokens"));
        assert!(text.contains("first entry line"), "preview shown");
        assert!(!text.contains("more"), "full content not rendered");
        assert!(text.contains("(empty)"));
        assert_eq!(flat.cursor_lines.len(), 4);

        // Expanding an entry renders its full content.
        tree.expanded_entries.insert(("system".to_string(), 0));
        let text = text_of(&flatten(&s, &tree, 1, false, 80));
        assert!(text.contains("more"), "expanded entry content rendered");

        // Folding a region replaces its entries with a count.
        tree.collapsed_regions.insert("system".to_string());
        let text = text_of(&flatten(&s, &tree, 0, false, 80));
        assert!(text.contains("(2 entries folded)"));
        assert!(!text.contains("entry 1"), "stubs folded away");

        // Search overrides both folds and collapsed entries.
        let text = text_of(&flatten(&s, &tree, 0, true, 80));
        assert!(text.contains("more"), "search sees inside entries");

        // Folding a region with no entries shows neither marker nor (empty).
        tree.collapsed_regions.insert("conversation".to_string());
        let text = text_of(&flatten(&s, &tree, 0, false, 80));
        assert!(!text.contains("(empty)"), "folded empty region: {text}");
    }

    #[test]
    fn region_jumps_find_the_neighboring_headers() {
        let s = snap();
        let tree = ContextTreeState::default();
        // rows: [header system, stub 0, stub 1, header conversation].
        let all = rows(&s, &tree, false);
        assert_eq!(all.len(), 4);
        assert_eq!(nearest_region_row(&all, 0, true), Some(3));
        assert_eq!(nearest_region_row(&all, 3, false), Some(0));
        assert_eq!(nearest_region_row(&all, 1, false), Some(0));
        assert_eq!(nearest_region_row(&all, 0, false), None);
        assert_eq!(nearest_region_row(&all, 3, true), None);
    }
}

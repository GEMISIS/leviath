//! Tests for the markdown renderer.
//!
//! Their own file: `mod.rs` carries the renderer, and a source file in
//! this workspace stops at 1200 lines.

use super::*;

#[test]
fn push_line_flushes_pending_spans_first() {
    // No current caller ever invokes `push_line` while `current_spans`
    // is non-empty (each call site flushes its own pending spans via a
    // separate check first) - but the method itself is directly
    // testable by constructing that state manually, without needing a
    // real caller to reach it.
    let mut r = Renderer::new(80);
    r.current_spans.push(Span::raw("pending"));
    r.push_line(Line::from("new line"));
    assert_eq!(r.lines.len(), 2);
    assert!(r.current_spans.is_empty());
    let flushed: String = r.lines[0]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(flushed, "pending");
}

#[test]
fn plain_text_renders_as_single_line() {
    let text = markdown_to_text("Hello, world!", 80);
    assert!(!text.lines.is_empty());
    // The content should contain our text somewhere
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(all.contains("Hello, world!"));
}

#[test]
fn heading_produces_lines() {
    let text = markdown_to_text("# My Heading\n\nSome paragraph.", 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert!(all.contains("My Heading"));
    assert!(all.contains("Some paragraph"));
}

#[test]
fn code_block_renders_with_border() {
    let md = "```rust\nfn main() {}\n```";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert!(all.contains("fn main() {}"));
    // Should have a border glyph. Code-block borders only ever render
    // with '╭' (never '│'), so checking a single glyph avoids a
    // redundant `||` whose right-hand side could never be reached.
    assert!(all.contains('╭'));
}

#[test]
fn mermaid_block_is_drawn_rather_than_dumped() {
    let md = "```mermaid\ngraph LR\n  A --> B\n```";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    // It used to print its own source and an errand ("install mmdc"). Now it
    // is a diagram: the ids are in boxes with an arrow between them.
    assert!(all.contains('┌'), "{all}");
    assert!(all.contains('▼'), "{all}");
    assert!(!all.contains("mmdc"), "the errand is gone: {all}");
}

#[test]
fn list_renders_bullets() {
    let md = "- item one\n- item two";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert!(all.contains("item one"));
    assert!(all.contains("item two"));
}

// ─── Empty input ───────────────────────────────────────────────────────

#[test]
fn empty_input_returns_empty() {
    let text = markdown_to_text("", 80);
    // Empty input produces zero lines (no content to render). Checked
    // directly (rather than via `.iter().all(|l| ...)`) since `all()`
    // short-circuits without invoking its predicate on an empty
    // iterator, which would otherwise leave that closure's body
    // permanently unreachable.
    assert!(text.lines.is_empty());
}

// ─── Inline styles ────────────────────────────────────────────────────

#[test]
fn bold_text_rendered() {
    let text = markdown_to_text("**bold text**", 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert!(all.contains("bold text"));
}

#[test]
fn italic_text_rendered() {
    let text = markdown_to_text("*italic text*", 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert!(all.contains("italic text"));
}

#[test]
fn strikethrough_text_rendered() {
    let text = markdown_to_text("~~deleted~~", 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert!(all.contains("deleted"));
}

/// `<u>` is the one HTML tag this renderer reads, because markdown has no
/// underline and the dashboard's long-form editor writes exactly this. The
/// tags themselves never reach the screen.
#[test]
fn the_underline_tag_underlines_and_does_not_print_itself() {
    let text = markdown_to_text("plain <u>marked</u> plain", 80);
    let spans: Vec<_> = text.lines.iter().flat_map(|l| l.spans.iter()).collect();
    let all: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(all.trim(), "plain marked plain", "{all:?}");
    assert!(
        spans
            .iter()
            .any(|s| s.content.contains("marked")
                && s.style.add_modifier.contains(Modifier::UNDERLINED)),
        "{spans:?}"
    );
    assert!(
        spans
            .iter()
            .any(|s| s.content.contains("plain")
                && !s.style.add_modifier.contains(Modifier::UNDERLINED)),
        "the underline did not stop: {spans:?}"
    );
}

/// Bold inside an underline keeps both, the way strikethrough-inside-bold
/// already did.
#[test]
fn bold_inside_an_underline_keeps_both() {
    let text = markdown_to_text("<u>a **b**</u>", 80);
    let bold = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .find(|s| s.content.contains('b') && s.style.add_modifier.contains(Modifier::BOLD))
        .expect("a bold span");
    assert!(bold.style.add_modifier.contains(Modifier::UNDERLINED));
}

/// Only `<u>` exactly. Every other tag stays ignored, `<ul>` included, and
/// a prefix test here would have swallowed it.
#[test]
fn only_the_underline_tag_is_read() {
    assert!(is_tag("<u>", "u"));
    assert!(is_tag("  <U>  ", "u"), "case and padding do not matter");
    assert!(is_tag("</u>", "/u"));
    assert!(!is_tag("<ul>", "u"));
    assert!(!is_tag("<user>", "u"));
    assert!(!is_tag("u", "u"), "not a tag at all");

    // `<ins>` is the other tag that means underline in HTML, and is
    // deliberately not read: this reads what the editor writes.
    let text = markdown_to_text("<ins>x</ins>", 80);
    let underlined = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .any(|s| s.style.add_modifier.contains(Modifier::UNDERLINED));
    assert!(!underlined);
}

#[test]
fn inline_code_rendered() {
    let text = markdown_to_text("use `println!()`", 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert!(all.contains("println!()"));
}

// ─── Headings ──────────────────────────────────────────────────────────

#[test]
fn h2_heading_rendered() {
    let text = markdown_to_text("## Second Level", 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert!(all.contains("Second Level"));
}

#[test]
fn h3_heading_rendered() {
    let text = markdown_to_text("### Third Level", 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert!(all.contains("Third Level"));
}

#[test]
fn h1_heading_produces_underline_rule() {
    let text = markdown_to_text("# Heading", 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    // H1 should produce a horizontal rule line underneath
    assert!(all.contains("\u{2500}"));
}

// ─── Horizontal rule ───────────────────────────────────────────────────

#[test]
fn horizontal_rule_rendered() {
    let text = markdown_to_text("above\n\n---\n\nbelow", 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert!(all.contains("above"));
    assert!(all.contains("below"));
    assert!(all.contains("\u{2500}"));
}

// ─── Blockquote ────────────────────────────────────────────────────────

#[test]
fn blockquote_rendered() {
    let text = markdown_to_text("> quoted text", 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert!(all.contains("quoted text"));
}

// ─── Ordered list ──────────────────────────────────────────────────────

#[test]
fn ordered_list_rendered() {
    let md = "1. first\n2. second\n3. third";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert!(all.contains("first"));
    assert!(all.contains("second"));
    assert!(all.contains("third"));
}

// ─── Code block without language ───────────────────────────────────────

#[test]
fn code_block_without_language() {
    let md = "```\nplain code\n```";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert!(all.contains("plain code"));
    // Should show "code" as the default language label
    assert!(all.contains("code"));
}

// ─── Link rendering ───────────────────────────────────────────────────

#[test]
fn link_rendered() {
    let md = "[click here](https://example.com)";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert!(all.contains("click here"));
}

#[test]
fn link_with_empty_url_skips_bracket_span() {
    // Exercises the `!url.is_empty()` false arm (a link whose destination
    // URL is empty), which skips pushing the trailing "[" span.
    let md = "[no url]()";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert!(all.contains("no url"));
}

// ─── Narrow width ──────────────────────────────────────────────────────

#[test]
fn narrow_width_does_not_panic() {
    // Very narrow width should not cause panics
    let text = markdown_to_text("# Heading\n\n---\n\nSome content", 5);
    assert!(!text.lines.is_empty());
}

#[test]
fn zero_width_does_not_panic() {
    let text = markdown_to_text("# Heading\n\n---", 0);
    assert!(!text.lines.is_empty());
}

// ─── InlineStyle ───────────────────────────────────────────────────────

#[test]
fn inline_style_default_produces_white_text() {
    let style = InlineStyle::default();
    let ratatui_style = style.to_ratatui_style();
    assert_eq!(ratatui_style.fg, Some(C_WHITE));
}

#[test]
fn inline_style_code_overrides_color() {
    let style = InlineStyle {
        code: true,
        ..Default::default()
    };
    let ratatui_style = style.to_ratatui_style();
    // Code should have a specific fg color, not white
    assert_ne!(ratatui_style.fg, Some(C_WHITE));
}

// ─── Multiple paragraphs ───────────────────────────────────────────────

#[test]
fn multiple_paragraphs_have_blank_lines() {
    let md = "First paragraph.\n\nSecond paragraph.";
    let text = markdown_to_text(md, 80);
    // Should have more than 2 lines (paragraphs + blank separators)
    assert!(text.lines.len() >= 3);
}

// ─── Additional heading levels (H4/H5/H6) ───────────────────────────────

#[test]
fn h4_h5_h6_headings_rendered() {
    let md = "#### Four\n\n##### Five\n\n###### Six";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(all.contains("Four"));
    assert!(all.contains("Five"));
    assert!(all.contains("Six"));
}

// ─── Nested inline styles inherit from parent ──────────────────────────

#[test]
fn bold_italic_nested_inherits_both_modifiers() {
    // ***text*** parses as Strong containing Emphasis (or vice versa) -
    // the inner style must inherit the outer's bold/italic/strikethrough.
    let md = "***bold italic***";
    let text = markdown_to_text(md, 80);
    let style = text.lines[0].spans[0].style;
    assert!(style.add_modifier.contains(Modifier::BOLD));
    assert!(style.add_modifier.contains(Modifier::ITALIC));
}

#[test]
fn strikethrough_inside_bold_inherits_bold() {
    let md = "**bold ~~and struck~~**";
    let text = markdown_to_text(md, 80);
    let all_styled_bold = text.lines[0].spans.iter().any(|s| {
        s.style
            .add_modifier
            .contains(Modifier::CROSSED_OUT | Modifier::BOLD)
    });
    assert!(all_styled_bold);
}

// ─── Blockquote with multiple lines flushes correctly ──────────────────

#[test]
fn blockquote_with_multiple_lines() {
    let md = "> line one\n> line two";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(all.contains("line one"));
    assert!(all.contains("line two"));
}

// ─── Nested / multi-item lists flush pending content between items ─────

#[test]
fn bullet_list_with_multiple_items() {
    let md = "- alpha\n- beta\n- gamma";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(all.contains("alpha"));
    assert!(all.contains("beta"));
    assert!(all.contains("gamma"));
    assert!(all.contains("\u{25cf}"));
}

#[test]
fn nested_list_indents() {
    let md = "- top\n  - nested\n- top2";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(all.contains("top"));
    assert!(all.contains("nested"));
}

// ─── Indented (non-fenced) code block ───────────────────────────────────

#[test]
fn indented_code_block_has_no_language_label_from_lang() {
    let md = "Normal text.\n\n    indented code line\n\nMore text.";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(all.contains("indented code line"));
}

// ─── Multi-line text event (embedded newline split) ────────────────────

#[test]
fn hard_break_splits_into_separate_lines() {
    // Two trailing spaces + newline = hard break in CommonMark.
    let md = "first line  \nsecond line";
    let text = markdown_to_text(md, 80);
    assert!(text.lines.len() >= 2);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(all.contains("first line"));
    assert!(all.contains("second line"));
}

#[test]
fn soft_break_becomes_space() {
    let md = "first\nsecond";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(all.contains("first"));
    assert!(all.contains("second"));
}

// ─── Rule with pending inline content before it ────────────────────────

#[test]
fn rule_flushes_pending_content_first() {
    // pulldown-cmark treats "text\n***" as a paragraph followed by a rule
    // only when properly separated; use explicit blank-line-free content
    // before a thematic break to exercise the pending-flush branch.
    let md = "above text\n\n---\nbelow text";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(all.contains("above text"));
    assert!(all.contains("below text"));
}

// ─── Table events fall through the catch-all arm ───────────────────────

#[test]
fn table_does_not_panic_and_renders_cell_text() {
    let md = "| A | B |\n|---|---|\n| 1 | 2 |";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    // Table structural events are ignored (catch-all arm), but the text
    // content inside cells still comes through as Text events. Both
    // substrings are always present together, so check each directly
    // rather than via a redundant `||` whose right-hand side could
    // never be reached.
    assert!(all.contains('1'));
    assert!(all.contains('A'));
}

// ─── Renderer state-machine edge cases ──────────────────────────────────
//
// These target `flush_line()`/inline-inheritance branches that only fire
// when `current_spans` (or the inline-style stack) is in a specific,
// non-default state at the moment a new block/inline event starts --
// found by probing actual `pulldown_cmark::Parser` event streams for
// each input (see git history) rather than guessing at markdown syntax.

#[test]
fn heading_as_first_content_of_list_item_flushes_pending_bullet_span() {
    // `Start(Item)` pushes the bullet marker into `current_spans`, then
    // `Start(Heading)` fires with no Text/other event in between --
    // exercises the `!self.current_spans.is_empty()` flush at heading
    // start.
    let md = "- # nested heading in item\n- item2";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(all.contains("nested heading in item"));
    assert!(all.contains("item2"));
}

#[test]
fn code_block_as_first_content_of_list_item_flushes_pending_bullet_span() {
    // Same shape as the heading case above, but for `Start(CodeBlock)`.
    let md = "- ```\ncode\n```";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(all.contains("code"));
}

#[test]
fn rule_directly_inside_blockquote_flushes_pending_quote_marker_span() {
    // `Start(BlockQuote)` pushes the "│ " marker into `current_spans`,
    // then `Event::Rule` fires with nothing else queued - exercises the
    // `!self.current_spans.is_empty()` flush at `Event::Rule` (every
    // other rule test has an empty `current_spans` at that point).
    let md = "> ---";
    let text = markdown_to_text(md, 80);
    assert!(!text.lines.is_empty());
}

#[test]
fn empty_blockquote_flushes_pending_quote_marker_span_at_end() {
    // `Start(BlockQuote)` pushes "│ " with no inner content at all
    // before `End(BlockQuote)` - exercises the flush at blockquote end
    // (every other blockquote test has real paragraph content, which
    // flushes `current_spans` via `End(Paragraph)` first).
    let md = ">";
    let text = markdown_to_text(md, 80);
    let all: String = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(all.contains('│'));
}

#[test]
fn nested_strong_inside_strikethrough_inherits_strikethrough_modifier() {
    // `push_inline` for the inner `Strong` tag inherits `strikethrough`
    // from the `Strikethrough` parent already on the stack - every
    // other strikethrough test only nests plain text, never another
    // inline tag, so `parent.strikethrough` was never true at push time.
    let md = "~~strike **bold inside** more~~";
    let text = markdown_to_text(md, 80);
    let has_strikethrough_bold = text.lines.iter().any(|l| {
        l.spans.iter().any(|s| {
            s.content.contains("bold inside")
                && s.style.add_modifier.contains(Modifier::CROSSED_OUT)
                && s.style.add_modifier.contains(Modifier::BOLD)
        })
    });
    assert!(has_strikethrough_bold);
}

// `push_line`'s own `if !self.current_spans.is_empty() { self.flush_line() }`
// guard is not reachable given how `push_line`/`blank_line` are actually
// called in this file: every call site (heading/paragraph/blockquote/list/
// rule end handlers) already flushes `current_spans` explicitly, via its
// own separate check, before ever calling `push_line`/`blank_line`.

// `Event::Start(Tag::Item)`'s `None => "● ".to_string()` arm (list_stack
// empty) is not covered and is not reachable: `Tag::Item` is only ever
// emitted by `pulldown_cmark` as a child of `Tag::List`, which always
// pushes onto `list_stack` first.

// ─── handle_text_content: embedded newline split (direct call) ────────────

#[test]
fn handle_text_content_with_embedded_newline_splits_lines() {
    // pulldown_cmark never produces a non-code Text event with `\n`, so
    // the newline-split path in handle_text_content is exercised here by
    // calling the method directly.
    let mut r = Renderer::new(80);
    r.handle_text_content("first\nsecond\nthird");
    // The first line is flushed for each embedded newline; at least 2 lines
    // should have been emitted.
    assert!(r.lines.len() >= 2);
    let all: String = r
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(all.contains("first"));
    assert!(all.contains("second"));
}

#[test]
fn handle_text_content_with_leading_newline_skips_empty_first_part() {
    // The empty part before the leading '\n' must not emit an empty span.
    let mut r = Renderer::new(80);
    r.handle_text_content("\nhello");
    // "hello" lands in current_spans (pending); lines get the flush for the
    // empty-first-part boundary, which produces an empty line.
    let pending: String = r.current_spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(pending.contains("hello"));
}

#[test]
fn handle_text_content_without_newline_emits_directly() {
    let mut r = Renderer::new(80);
    // Flush a real line into `r.lines` first (text ending in a newline
    // gets flushed), so the `r.lines` iteration below actually visits an
    // element instead of running over an always-empty `Vec` and leaving
    // its closure unreachable.
    r.handle_text_content("first line\n");
    r.handle_text_content("no newline here");
    let all: String = r
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>()
        + r.current_spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
            .as_str();
    assert!(all.contains("no newline here"));
    assert!(all.contains("first line"));
}

/// The trailing flush. No document reaches the end of the event stream with
/// spans still pending - every block closes itself - but the flush is what
/// makes that true rather than a hope, and it is directly testable.
#[test]
fn render_flushes_anything_still_pending_at_the_end() {
    let mut r = Renderer::new(80);
    r.current_spans.push(Span::raw("left over"));
    r.render("");
    let all: String = r
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert_eq!(all, "left over");
}

/// A pane with barely room for the frame gives every column one cell and
/// stops, rather than shrinking them away to nothing.
#[test]
fn a_table_in_a_pane_with_no_room_keeps_one_column_each() {
    let md = "| aaaa | bbbb | cccc |\n|---|---|---|\n| x | y | z |\n";
    let rows = rows_of(md, 8);
    assert!(rows.iter().any(|r| r.contains('┌')), "{rows:?}");
    for row in &rows {
        assert!(row.chars().count() <= 13, "{row:?}");
    }
}

/// A cell whose markup splits it into several spans truncates across them:
/// the first span fills the room and the rest contribute nothing.
#[test]
fn a_multi_span_cell_truncates_across_its_spans() {
    let md = "| a |\n|---|\n| **aaaaaaaaaaaaaaaa** and `bbbbbbbbbbbb` and more |\n";
    let rows = rows_of(md, 20);
    let body = rows.iter().find(|r| r.contains('a')).expect("a row");
    assert!(body.chars().count() <= 20, "{body:?}");
    assert!(rows.join("\n").contains('…'), "{rows:?}");
}

// ─── Mermaid ─────────────────────────────────────────────────────────────────

fn chart(source: &[&str], width: u16) -> Vec<String> {
    let lines: Vec<String> = source.iter().map(|s| s.to_string()).collect();
    super::mermaid::render(&lines, width)
        .expect("a flowchart")
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

/// A flowchart used to render as its own source with an errand attached
/// ("install mmdc"). It is a diagram now: boxes, and arrows between them.
#[test]
fn a_flowchart_is_drawn_as_boxes_and_arrows() {
    let rows = chart(
        &[
            "flowchart TD",
            "  A[Discover] --> B{Plan ok?}",
            "  B -->|yes| C[Build]",
            "  B -->|no| D[Rethink]",
            "  C --> E((Done))",
        ],
        60,
    );
    let all = rows.join("\n");
    assert!(all.contains("Discover"), "{all}");
    assert!(all.contains('┌') && all.contains('┘'), "no box:\n{all}");
    assert!(all.contains('▼'), "no arrow head:\n{all}");
    assert!(
        all.contains("yes") && all.contains("no"),
        "no labels:\n{all}"
    );

    // A node that branches is a tee, not an elbow: the line arrives from
    // above and leaves both ways.
    assert!(all.contains('┴'), "the fan-out is not a junction:\n{all}");
    // A decision is drawn differently from a step, without costing a row.
    assert!(all.contains("<Plan ok?>"), "{all}");
    assert!(all.contains("(Done)"), "{all}");
    // Nothing runs past the width it was given.
    for row in &rows {
        assert!(row.chars().count() <= 60, "too wide: {row:?}");
    }
}

/// An edge that goes backwards would need routing around the boxes between
/// its ends. It is named underneath instead of drawn through them.
#[test]
fn an_edge_that_does_not_go_straight_down_is_listed_instead() {
    let rows = chart(
        &[
            "flowchart TD",
            "  A[one] --> B[two]",
            "  B --> C[three]",
            "  C -->|retry| A",
        ],
        50,
    );
    let all = rows.join("\n");
    assert!(all.contains("──▶"), "the back edge is not listed:\n{all}");
    assert!(all.contains("(retry)"), "its label is missing:\n{all}");
}

/// Every shape and connector the subset covers parses, and a node named
/// before it is described still picks up its label.
#[test]
fn the_shapes_and_connectors_all_parse() {
    let rows = chart(
        &[
            "flowchart LR",
            "  %% a comment",
            "  subgraph outer",
            "  A --> B",
            "  end",
            "  B[Named later] -.-> C(round)",
            "  C ==> D{choice}",
            "  D --- E((end))",
        ],
        70,
    );
    let all = rows.join("\n");
    for label in ["Named later", "round", "choice", "end"] {
        assert!(all.contains(label), "{label} missing from:\n{all}");
    }
    // A dashed edge is drawn with a lighter stem than a solid one.
    assert!(all.contains('╎') || all.contains('╌'), "not dashed:\n{all}");
}

/// A chain writes an edge per link rather than one edge end to end.
#[test]
fn a_chained_statement_is_one_edge_per_link() {
    let rows = chart(&["flowchart TD", "  A --> B --> C"], 40);
    let all = rows.join("\n");
    assert!(
        all.contains('A') && all.contains('B') && all.contains('C'),
        "{all}"
    );
    // Three layers of boxes: A over B over C.
    assert_eq!(all.matches('┌').count(), 3, "{all}");
}

/// Mermaid is more than flowcharts, and a diagram this cannot draw is shown
/// as its source rather than as a wrong picture.
#[test]
fn a_diagram_that_is_not_a_flowchart_falls_back_to_its_source() {
    let source: Vec<String> = ["sequenceDiagram", "  Alice->>Bob: hi"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(super::mermaid::render(&source, 60).is_none());

    let md = "```mermaid\nsequenceDiagram\n  Alice->>Bob: hi\n```\n";
    let all = rows_of(md, 60).join("\n");
    assert!(all.contains("not a flowchart"), "{all}");
    assert!(all.contains("Alice->>Bob"), "the source is there: {all}");
}

/// An empty or headerless block is not a flowchart either.
#[test]
fn an_empty_block_is_not_a_flowchart() {
    assert!(super::mermaid::render(&[], 40).is_none());
    assert!(super::mermaid::render(&["flowchart TD".to_string()], 40).is_none());
}

/// A cycle must not send the layering off forever.
#[test]
fn a_cycle_still_terminates() {
    let rows = chart(&["flowchart TD", "  A --> B", "  B --> A"], 40);
    assert!(!rows.is_empty());
}

/// A line the parser cannot make sense of is skipped, not fatal: an agent
/// writing mermaid by hand will produce some of these, and losing the whole
/// diagram over one of them would be the wrong trade.
#[test]
fn a_line_that_does_not_parse_is_skipped() {
    let rows = chart(
        &[
            "flowchart TD",
            "  --> orphan", // no node before the arrow
            "  A B",        // no connector between them
            "  C -->",      // nothing after the arrow
            "  D --> E",    // and one that is fine
        ],
        40,
    );
    let all = rows.join("\n");
    assert!(all.contains('D') && all.contains('E'), "{all}");
}

/// A label in quotes loses them, because mermaid's quotes are how you write a
/// label with punctuation in it, not part of the label.
#[test]
fn a_quoted_label_loses_its_quotes() {
    let rows = chart(&["flowchart TD", "  A[\"Do the thing\"] --> B"], 40);
    let all = rows.join("\n");
    assert!(all.contains("Do the thing"), "{all}");
    assert!(!all.contains('"'), "the quotes came through: {all}");
}

/// A node reached two ways is visited once, and its second arrival is not a
/// back edge.
#[test]
fn a_diamond_joins_back_up_without_a_back_edge() {
    let rows = chart(
        &[
            "flowchart TD",
            "  A --> B",
            "  A --> C",
            "  B --> D",
            "  C --> D",
        ],
        50,
    );
    let all = rows.join("\n");
    assert!(!all.contains("──▶"), "nothing should be listed:\n{all}");
    // Two lines arrive at D, so its junction is a tee.
    assert!(all.contains('┬'), "no join:\n{all}");
}

/// A node with one edge straight down and another to the side turns through a
/// tee, not an elbow. Two sources over two targets, each reaching both, puts
/// one of each kind on the same routing row.
#[test]
fn an_edge_down_and_one_sideways_meet_at_a_tee() {
    let rows = chart(
        &[
            "flowchart TD",
            "  A --> C",
            "  A --> D",
            "  B --> C",
            "  B --> D",
        ],
        50,
    );
    let all = rows.join("\n");
    // Each source turns a tee, and because a target sits under each source the
    // two marks land on the same cells: a line down and a line sideways is a
    // crossing.
    assert!(all.contains('┼'), "no crossing:\n{all}");
}

/// A label longer than the pane is clipped at the edge rather than drawn off
/// the end of the canvas.
#[test]
fn a_label_wider_than_the_pane_is_clipped() {
    let rows = chart(
        &[
            "flowchart TD",
            "  A -->|a label far longer than this pane can hold| B",
        ],
        24,
    );
    for row in &rows {
        assert!(row.chars().count() <= 24, "ran off: {row:?}");
    }
}

// ─── Tables ──────────────────────────────────────────────────────────────────

/// The rendered lines, each as one string, for tests that care about shape.
fn rows_of(md: &str, width: u16) -> Vec<String> {
    markdown_to_text(md, width)
        .lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

/// A table used to arrive as its cells run together on one line. It is now a
/// framed grid, with the header ruled off and the columns aligned as the
/// delimiter row asked.
#[test]
fn a_table_is_drawn_as_a_grid() {
    let md = "| Name | Role |\n|:---|---:|\n| alpha | lead |\n| b | c |\n";
    let rows = rows_of(md, 40);
    let joined = rows.join("\n");
    assert!(joined.contains('┌'), "no frame:\n{joined}");
    assert!(joined.contains('┼'), "no header rule:\n{joined}");
    assert!(joined.contains('└'), "no bottom:\n{joined}");
    assert!(joined.contains("Name"), "{joined}");
    assert!(joined.contains("alpha"), "{joined}");

    // Right-aligned column: `lead` sits against the right of its cell.
    let body = rows.iter().find(|r| r.contains("alpha")).expect("a row");
    assert!(body.contains("lead │"), "not right aligned: {body:?}");
    // Left-aligned column: `alpha` sits against the left of its cell.
    assert!(body.contains("│ alpha"), "not left aligned: {body:?}");
}

#[test]
fn a_centred_column_is_centred() {
    let md = "| x |\n|:-:|\n| a |\n| longer |\n";
    let rows = rows_of(md, 40);
    let body = rows.iter().find(|r| r.contains(" a ")).expect("a row");
    assert!(body.contains("│   a"), "not centred: {body:?}");
}

/// A table wider than the pane loses content from its widest column rather
/// than running off the edge, and says so with an ellipsis.
#[test]
fn a_table_too_wide_for_the_pane_is_squeezed() {
    let md = "| short | a very long column of text indeed |\n|---|---|\n| x | y |\n";
    let rows = rows_of(md, 30);
    for row in &rows {
        assert!(
            row.chars().count() <= 30,
            "ran off the edge ({}): {row:?}",
            row.chars().count()
        );
    }
    assert!(rows.join("\n").contains('…'), "nothing marked the cut");
}

/// Cell markup still renders: a table is not a code block.
#[test]
fn a_cells_markup_is_still_markup() {
    let md = "| a |\n|---|\n| **loud** |\n";
    let text = markdown_to_text(md, 40);
    let bold = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .find(|s| s.content.contains("loud"))
        .expect("the cell");
    assert!(bold.style.add_modifier.contains(Modifier::BOLD), "{bold:?}");
}

/// A ragged table (a row with fewer cells than the header) still draws every
/// column, so the grid does not go crooked.
#[test]
fn a_row_with_missing_cells_still_fills_the_grid() {
    let md = "| a | b |\n|---|---|\n| only |\n";
    let rows = rows_of(md, 40);
    let widths: Vec<usize> = rows
        .iter()
        .filter(|r| r.starts_with('│') || r.starts_with('┌'))
        .map(|r| r.chars().count())
        .collect();
    assert!(
        widths.windows(2).all(|w| w[0] == w[1]),
        "rows disagree on width: {widths:?}"
    );
}

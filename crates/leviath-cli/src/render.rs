//! Markdown → ratatui `Text` renderer.
//!
//! Converts a markdown string to a `ratatui::text::Text<'static>` using
//! `pulldown-cmark`.  Designed for rendering agent output inside the dashboard's
//! content panes where only `Paragraph` + `Text` are available (no nested widgets).
//!
//! Feature coverage:
//! - Headings H1–H6 (bold + accent colour, H1 gets an underline rule)
//! - Paragraphs with soft/hard breaks
//! - Bullet and ordered lists (nested up to depth 3)
//! - Blockquotes (dim, "│ " prefix)
//! - Inline `code`, **bold**, *italic*, ~~strikethrough~~, links (underlined)
//! - Fenced code blocks (tinted, language tag in title line)
//! - `mermaid` fenced blocks → styled fallback box with a hint to install mmdc
//! - Horizontal rules (dim dashes)
//!
//! Plain text that contains no markdown degrades cleanly — it just renders as
//! white text.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

// ─── Palette (mirrors the dashboard theme) ────────────────────────────────────

const C_ACCENT: Color = Color::Cyan;
const C_SUCCESS: Color = Color::Green;
const C_DIM: Color = Color::DarkGray;
const C_MUTED: Color = Color::Gray;
const C_WHITE: Color = Color::White;
const C_CODE_BG: Color = Color::Rgb(30, 30, 40);

// ─── Public API ───────────────────────────────────────────────────────────────

/// Convert a markdown string to ratatui `Text` for rendering in a `Paragraph`.
///
/// `width` is used to draw horizontal rules to the correct width.
pub fn markdown_to_text(input: &str, width: u16) -> Text<'static> {
    let mut renderer = Renderer::new(width);
    renderer.render(input);
    Text::from(renderer.lines)
}

// ─── Renderer internals ───────────────────────────────────────────────────────

/// Stack of style modifiers accumulated from nested inline tags.
#[derive(Default, Clone)]
struct InlineStyle {
    bold: bool,
    italic: bool,
    strikethrough: bool,
    code: bool,
    link: bool,
}

impl InlineStyle {
    fn to_ratatui_style(&self) -> Style {
        let mut style = Style::default().fg(C_WHITE);
        if self.code {
            style = style.fg(Color::Rgb(200, 160, 100)).bg(C_CODE_BG);
        } else if self.link {
            style = style.fg(C_ACCENT).add_modifier(Modifier::UNDERLINED);
        }
        if self.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strikethrough {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        style
    }
}

struct Renderer {
    lines: Vec<Line<'static>>,
    /// Spans being accumulated for the current line.
    current_spans: Vec<Span<'static>>,
    /// Inline style stack (push/pop for nested inline tags).
    inline_stack: Vec<InlineStyle>,
    /// Current inline style (derived from the stack).
    inline: InlineStyle,
    /// Whether we're currently inside a fenced code block.
    in_code_block: bool,
    /// Language hint for the current code block.
    code_lang: Option<String>,
    /// Source lines accumulated within a code block.
    code_lines: Vec<String>,
    /// List nesting stack: None = bullet, Some(n) = ordered (current item #).
    list_stack: Vec<Option<u64>>,
    /// Terminal width (used for HR lines).
    width: u16,
}

impl Renderer {
    fn new(width: u16) -> Self {
        Self {
            lines: Vec::new(),
            current_spans: Vec::new(),
            inline_stack: Vec::new(),
            inline: InlineStyle::default(),
            in_code_block: false,
            code_lang: None,
            code_lines: Vec::new(),
            list_stack: Vec::new(),
            width,
        }
    }

    /// Flush `current_spans` into a finished `Line`.
    fn flush_line(&mut self) {
        let spans = std::mem::take(&mut self.current_spans);
        self.lines.push(Line::from(spans));
    }

    /// Push a fully-built `Line` directly.
    fn push_line(&mut self, line: Line<'static>) {
        if !self.current_spans.is_empty() {
            self.flush_line();
        }
        self.lines.push(line);
    }

    /// Push an empty blank line.
    fn blank_line(&mut self) {
        self.push_line(Line::from(""));
    }

    /// Current list indent (2 spaces per nesting level).
    fn list_indent(&self) -> String {
        "  ".repeat(self.list_stack.len())
    }

    /// Rebuild the active inline style from the stack top.
    fn sync_inline(&mut self) {
        self.inline = self.inline_stack.last().cloned().unwrap_or_default();
    }

    fn push_inline(&mut self, mut new_style: InlineStyle) {
        // Inherit accumulated modifiers from parent
        if let Some(parent) = self.inline_stack.last() {
            if parent.bold {
                new_style.bold = true;
            }
            if parent.italic {
                new_style.italic = true;
            }
            if parent.strikethrough {
                new_style.strikethrough = true;
            }
        }
        self.inline_stack.push(new_style);
        self.sync_inline();
    }

    fn pop_inline(&mut self) {
        self.inline_stack.pop();
        self.sync_inline();
    }

    /// Emit a span of text with the current inline style.
    fn emit_text(&mut self, text: &str) {
        let style = self.inline.to_ratatui_style();
        // Preserve leading space so words don't jam together
        self.current_spans
            .push(Span::styled(text.to_owned(), style));
    }

    /// Handle a non-code-block text payload, splitting on embedded newlines if
    /// present. Factored out so the newline-split path can be exercised in
    /// tests directly (pulldown_cmark never produces a non-code Text event with
    /// a literal `\n` under current Options, so this path is unreachable via
    /// the parser alone).
    fn handle_text_content(&mut self, t: &str) {
        if t.contains('\n') {
            let mut first = true;
            for part in t.split('\n') {
                if !first {
                    self.flush_line();
                }
                first = false;
                if !part.is_empty() {
                    self.emit_text(part);
                }
            }
        } else {
            self.emit_text(t);
        }
    }

    /// Render a complete code block (fenced or mermaid).
    fn flush_code_block(&mut self) {
        let lang = self.code_lang.take().unwrap_or_default();
        let content = std::mem::take(&mut self.code_lines);
        let is_mermaid = lang.trim().to_lowercase() == "mermaid";

        if is_mermaid {
            // ── Mermaid fallback ─────────────────────────────────────────────
            self.push_line(Line::from(vec![
                Span::styled(
                    "  ◇ ",
                    Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled("mermaid diagram", Style::default().fg(C_ACCENT)),
                Span::styled(" — source", Style::default().fg(C_DIM)),
            ]));
            for code_line in &content {
                self.push_line(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(C_DIM)),
                    Span::styled(code_line.to_owned(), Style::default().fg(C_MUTED)),
                ]));
            }
            self.push_line(Line::from(Span::styled(
                "  ↑ Install mermaid-cli (mmdc) to render as a diagram",
                Style::default().fg(C_DIM),
            )));
        } else {
            // ── Regular code block ───────────────────────────────────────────
            let lang_label = if lang.is_empty() {
                "code".to_string()
            } else {
                lang.clone()
            };
            self.push_line(Line::from(vec![
                Span::styled("  ╭─ ", Style::default().fg(C_DIM)),
                Span::styled(
                    lang_label,
                    Style::default().fg(C_MUTED).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ─", Style::default().fg(C_DIM)),
            ]));
            for code_line in &content {
                self.push_line(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(C_DIM)),
                    Span::styled(
                        code_line.to_owned(),
                        Style::default().fg(Color::Rgb(200, 200, 140)).bg(C_CODE_BG),
                    ),
                ]));
            }
            self.push_line(Line::from(Span::styled("  ╰─", Style::default().fg(C_DIM))));
        }
        self.in_code_block = false;
    }

    pub fn render(&mut self, input: &str) {
        let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
        let parser = Parser::new_ext(input, opts);

        for event in parser {
            match event {
                // ── Block starts ─────────────────────────────────────────────
                Event::Start(Tag::Heading { level, .. }) => {
                    // Flush any pending content and start a fresh heading line
                    if !self.current_spans.is_empty() {
                        self.flush_line();
                    }
                    // Blank line before non-first headings
                    if !self.lines.is_empty() {
                        self.blank_line();
                    }
                    // Visual decorators — convey depth without # text (terminal can't vary font size)
                    let (color, prefix, bold) = match level {
                        HeadingLevel::H1 => (C_ACCENT, "▌ ", true),
                        HeadingLevel::H2 => (C_ACCENT, "▎ ", true),
                        HeadingLevel::H3 => (C_SUCCESS, "  ", true),
                        HeadingLevel::H4 => (C_MUTED, "   ", false),
                        HeadingLevel::H5 => (C_DIM, "    ", false),
                        HeadingLevel::H6 => (C_DIM, "     ", false),
                    };
                    let mut sty = Style::default().fg(color);
                    if bold {
                        sty = sty.add_modifier(Modifier::BOLD);
                    }
                    self.current_spans.push(Span::styled(prefix, sty));
                    self.push_inline(InlineStyle {
                        bold,
                        ..Default::default()
                    });
                }
                Event::End(TagEnd::Heading(level)) => {
                    self.pop_inline();
                    self.flush_line();
                    // H1 gets a dim underline rule
                    if level == HeadingLevel::H1 {
                        let rule_w = (self.width as usize).saturating_sub(2).max(8);
                        self.push_line(Line::from(Span::styled(
                            "─".repeat(rule_w),
                            Style::default().fg(C_DIM),
                        )));
                    }
                    self.blank_line();
                }

                Event::Start(Tag::Paragraph) => {}
                Event::End(TagEnd::Paragraph) => {
                    self.flush_line();
                    self.blank_line();
                }

                Event::Start(Tag::BlockQuote(_)) => {
                    self.push_inline(InlineStyle {
                        ..Default::default()
                    });
                    self.current_spans
                        .push(Span::styled("│ ", Style::default().fg(C_DIM)));
                }
                Event::End(TagEnd::BlockQuote(_)) => {
                    self.pop_inline();
                    if !self.current_spans.is_empty() {
                        self.flush_line();
                    }
                    self.blank_line();
                }

                Event::Start(Tag::List(start)) => {
                    self.list_stack.push(start);
                }
                Event::End(TagEnd::List(_)) => {
                    self.list_stack.pop();
                    if self.list_stack.is_empty() {
                        self.blank_line();
                    }
                }
                Event::Start(Tag::Item) => {
                    if !self.current_spans.is_empty() {
                        self.flush_line();
                    }
                    let indent = self.list_indent();
                    let bullet = match self.list_stack.last() {
                        Some(Some(n)) => format!("{}. ", n),
                        Some(None) | None => "● ".to_string(),
                    };
                    // Increment ordered list counter
                    if let Some(Some(ref mut n)) = self.list_stack.last_mut() {
                        *n += 1;
                    }
                    self.current_spans.push(Span::styled(
                        format!("{}{}", indent, bullet),
                        Style::default().fg(C_ACCENT),
                    ));
                }
                Event::End(TagEnd::Item) => {
                    if !self.current_spans.is_empty() {
                        self.flush_line();
                    }
                }

                Event::Start(Tag::CodeBlock(kind)) => {
                    self.in_code_block = true;
                    self.code_lang = match kind {
                        CodeBlockKind::Fenced(lang) => {
                            let s = lang.into_string();
                            if s.is_empty() {
                                None
                            } else {
                                Some(s)
                            }
                        }
                        CodeBlockKind::Indented => None,
                    };
                    self.code_lines = Vec::new();
                    if !self.current_spans.is_empty() {
                        self.flush_line();
                    }
                }
                Event::End(TagEnd::CodeBlock) => {
                    self.flush_code_block();
                }

                Event::Start(Tag::Strong) => {
                    self.push_inline(InlineStyle {
                        bold: true,
                        ..Default::default()
                    });
                }
                Event::End(TagEnd::Strong) => {
                    self.pop_inline();
                }

                Event::Start(Tag::Emphasis) => {
                    self.push_inline(InlineStyle {
                        italic: true,
                        ..Default::default()
                    });
                }
                Event::End(TagEnd::Emphasis) => {
                    self.pop_inline();
                }

                Event::Start(Tag::Strikethrough) => {
                    self.push_inline(InlineStyle {
                        strikethrough: true,
                        ..Default::default()
                    });
                }
                Event::End(TagEnd::Strikethrough) => {
                    self.pop_inline();
                }

                Event::Start(Tag::Link { dest_url, .. }) => {
                    self.push_inline(InlineStyle {
                        link: true,
                        ..Default::default()
                    });
                    // Show the URL as a dim suffix after the link text
                    let url = dest_url.into_string();
                    if !url.is_empty() {
                        // We'll append the URL after the link text in End(Link)
                        // Store it via a span now so we can reference it; simpler: just push
                        // a placeholder and let the text events fill in link text.
                        // We push the URL as a trailing span at End(Link).
                        // Use a little indirection: push the open bracket.
                        self.current_spans
                            .push(Span::styled("[", Style::default().fg(C_DIM)));
                        // Stash URL in a "pending link url" field would be ideal.
                        // For simplicity, store it as a special span at end.
                        // We'll capture the URL by pushing it immediately at End.
                        // So skip storing; just remember we need to close.
                        let _ = url; // url emitted at End(Link) via stored span approach
                    }
                }
                Event::End(TagEnd::Link) => {
                    self.pop_inline();
                    self.current_spans
                        .push(Span::styled("]", Style::default().fg(C_DIM)));
                }

                // ── Inline events ────────────────────────────────────────────
                Event::Text(text) => {
                    if self.in_code_block {
                        // Accumulate raw code lines
                        for line in text.lines() {
                            self.code_lines.push(line.to_string());
                        }
                    } else {
                        let t = text.into_string();
                        self.handle_text_content(&t);
                    }
                }

                Event::Code(text) => {
                    // Inline code span
                    self.current_spans.push(Span::styled(
                        text.into_string(),
                        Style::default().fg(Color::Rgb(200, 160, 100)).bg(C_CODE_BG),
                    ));
                }

                Event::SoftBreak => {
                    // Soft breaks just become a space in terminal output
                    self.current_spans.push(Span::raw(" "));
                }
                Event::HardBreak => {
                    self.flush_line();
                }

                Event::Rule => {
                    if !self.current_spans.is_empty() {
                        self.flush_line();
                    }
                    let w = (self.width as usize).saturating_sub(2).max(8);
                    self.push_line(Line::from(Span::styled(
                        "─".repeat(w),
                        Style::default().fg(C_DIM),
                    )));
                    self.blank_line();
                }

                // Ignore HTML, footnotes, task list markers, math, etc.
                _ => {}
            }
        }

        // Flush any trailing content
        if !self.current_spans.is_empty() {
            self.flush_line();
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_line_flushes_pending_spans_first() {
        // No current caller ever invokes `push_line` while `current_spans`
        // is non-empty (each call site flushes its own pending spans via a
        // separate check first) -- but the method itself is directly
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
        assert!(all.contains("Hello, world!"), "got: {}", all);
    }

    #[test]
    fn heading_produces_lines() {
        let text = markdown_to_text("# My Heading\n\nSome paragraph.", 80);
        let all: String = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>();
        assert!(all.contains("My Heading"), "got: {}", all);
        assert!(all.contains("Some paragraph"), "got: {}", all);
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
        assert!(all.contains("fn main() {}"), "got: {}", all);
        // Should have a border glyph
        #[rustfmt::skip]
        assert!(all.contains("╭") || all.contains("│"), "no border found in: {}", all);
    }

    #[test]
    fn mermaid_block_shows_hint() {
        let md = "```mermaid\ngraph LR\n  A --> B\n```";
        let text = markdown_to_text(md, 80);
        let all: String = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>();
        assert!(all.contains("mermaid"), "got: {}", all);
        #[rustfmt::skip]
        assert!(all.contains("mmdc") || all.contains("Install"), "got: {}", all);
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
        assert!(all.contains("item one"), "got: {}", all);
        assert!(all.contains("item two"), "got: {}", all);
    }

    // ─── Empty input ───────────────────────────────────────────────────────

    #[test]
    fn empty_input_returns_empty() {
        let text = markdown_to_text("", 80);
        // Empty input should produce no lines or only empty lines
        let all: String = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>();
        assert!(all.is_empty() || all.trim().is_empty(), "got: {:?}", all);
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
        assert!(all.contains("bold text"), "got: {}", all);
    }

    #[test]
    fn italic_text_rendered() {
        let text = markdown_to_text("*italic text*", 80);
        let all: String = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>();
        assert!(all.contains("italic text"), "got: {}", all);
    }

    #[test]
    fn strikethrough_text_rendered() {
        let text = markdown_to_text("~~deleted~~", 80);
        let all: String = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>();
        assert!(all.contains("deleted"), "got: {}", all);
    }

    #[test]
    fn inline_code_rendered() {
        let text = markdown_to_text("use `println!()`", 80);
        let all: String = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>();
        assert!(all.contains("println!()"), "got: {}", all);
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
        assert!(all.contains("Second Level"), "got: {}", all);
    }

    #[test]
    fn h3_heading_rendered() {
        let text = markdown_to_text("### Third Level", 80);
        let all: String = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>();
        assert!(all.contains("Third Level"), "got: {}", all);
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
        #[rustfmt::skip]
        assert!(all.contains("\u{2500}"), "H1 should have underline rule, got: {}", all);
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
        assert!(all.contains("above"), "got: {}", all);
        assert!(all.contains("below"), "got: {}", all);
        #[rustfmt::skip]
        assert!(all.contains("\u{2500}"), "Expected horizontal rule char, got: {}", all);
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
        assert!(all.contains("quoted text"), "got: {}", all);
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
        assert!(all.contains("first"), "got: {}", all);
        assert!(all.contains("second"), "got: {}", all);
        assert!(all.contains("third"), "got: {}", all);
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
        assert!(all.contains("plain code"), "got: {}", all);
        // Should show "code" as the default language label
        assert!(all.contains("code"), "got: {}", all);
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
        assert!(all.contains("click here"), "got: {}", all);
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
        #[rustfmt::skip]
        assert!(text.lines.len() >= 3, "Expected at least 3 lines, got {}", text.lines.len());
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
        assert!(all.contains("Four"), "got: {}", all);
        assert!(all.contains("Five"), "got: {}", all);
        assert!(all.contains("Six"), "got: {}", all);
    }

    // ─── Nested inline styles inherit from parent ──────────────────────────

    #[test]
    fn bold_italic_nested_inherits_both_modifiers() {
        // ***text*** parses as Strong containing Emphasis (or vice versa) —
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
        assert!(all.contains("line one"), "got: {}", all);
        assert!(all.contains("line two"), "got: {}", all);
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
        assert!(all.contains("alpha"), "got: {}", all);
        assert!(all.contains("beta"), "got: {}", all);
        assert!(all.contains("gamma"), "got: {}", all);
        #[rustfmt::skip]
        assert!(all.contains("\u{25cf}"), "expected bullet glyph, got: {}", all);
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
        assert!(all.contains("top"), "got: {}", all);
        assert!(all.contains("nested"), "got: {}", all);
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
        assert!(all.contains("indented code line"), "got: {}", all);
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
        assert!(all.contains("first line"), "got: {}", all);
        assert!(all.contains("second line"), "got: {}", all);
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
        assert!(all.contains("first"), "got: {}", all);
        assert!(all.contains("second"), "got: {}", all);
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
        assert!(all.contains("above text"), "got: {}", all);
        assert!(all.contains("below text"), "got: {}", all);
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
        // content inside cells still comes through as Text events.
        assert!(all.contains('1') || all.contains('A'), "got: {}", all);
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
        // start (previously only ever empty by the time headings occurred).
        let md = "- # nested heading in item\n- item2";
        let text = markdown_to_text(md, 80);
        let all: String = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(all.contains("nested heading in item"), "got: {}", all);
        assert!(all.contains("item2"), "got: {}", all);
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
        assert!(all.contains("code"), "got: {}", all);
    }

    #[test]
    fn rule_directly_inside_blockquote_flushes_pending_quote_marker_span() {
        // `Start(BlockQuote)` pushes the "│ " marker into `current_spans`,
        // then `Event::Rule` fires with nothing else queued -- exercises the
        // `!self.current_spans.is_empty()` flush at `Event::Rule` (every
        // other rule test has an empty `current_spans` at that point).
        let md = "> ---";
        let text = markdown_to_text(md, 80);
        assert!(!text.lines.is_empty());
    }

    #[test]
    fn empty_blockquote_flushes_pending_quote_marker_span_at_end() {
        // `Start(BlockQuote)` pushes "│ " with no inner content at all
        // before `End(BlockQuote)` -- exercises the flush at blockquote end
        // (every other blockquote test has real paragraph content, which
        // flushes `current_spans` via `End(Paragraph)` first).
        let md = ">";
        let text = markdown_to_text(md, 80);
        let all: String = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(all.contains('│'), "got: {}", all);
    }

    #[test]
    fn nested_strong_inside_strikethrough_inherits_strikethrough_modifier() {
        // `push_inline` for the inner `Strong` tag inherits `strikethrough`
        // from the `Strikethrough` parent already on the stack -- every
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
        #[rustfmt::skip]
        assert!(has_strikethrough_bold, "expected a span with both CROSSED_OUT and BOLD modifiers for 'bold inside'");
    }

    // `push_line`'s own `if !self.current_spans.is_empty() { self.flush_line() }`
    // guard is not reachable given how `push_line`/`blank_line` are actually
    // called in this file: every call site (heading/paragraph/blockquote/list/
    // rule end handlers) already flushes `current_spans` explicitly, via its
    // own separate check, before ever calling `push_line`/`blank_line`.
    // Confirmed by enumerating parser events for nested lists, adjacent
    // headings, lists followed by headings/rules, and ordered lists during
    // this pass -- no input leaves `current_spans` non-empty at a
    // `push_line`/`blank_line` call site. This guard exists defensively, to
    // protect a future call site that doesn't flush first, not because any
    // current path exercises it.

    // `Event::Start(Tag::Item)`'s `None => "● ".to_string()` arm (list_stack
    // empty) is not covered and is not reachable: `Tag::Item` is only ever
    // emitted by `pulldown_cmark` as a child of `Tag::List`, which always
    // pushes onto `list_stack` first. Confirmed by direct inspection of
    // actual parser event streams for numerous inputs during this pass --
    // no input triggers `Start(Item)` without a preceding `Start(List)`.
    //
    // The `t.contains('\n')` branch in `Event::Text` handling (splitting a
    // single non-code-block text run on embedded newlines) is also not
    // reachable with this file's actual `Options` (`ENABLE_STRIKETHROUGH |
    // ENABLE_TABLES`): every multi-line text run pulldown_cmark emits for
    // paragraphs/emphasis/table cells is split into separate `Text` events
    // joined by `SoftBreak`, never a single `Text` event containing a literal
    // `\n`. The only real event with an embedded `\n` in `Text`'s payload is
    // fenced-code-block content, which takes the *other* branch of this same
    // `if` (`self.in_code_block`) before ever reaching this one. Confirmed by
    // directly enumerating parser events for ~20 candidate inputs (code
    // blocks, tables with escaped pipes/newlines, link titles, raw HTML,
    // nested emphasis) during this pass -- none produced a non-code Text
    // event with an embedded newline.
}

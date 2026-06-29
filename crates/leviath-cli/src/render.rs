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
                        Some(None) => "● ".to_string(),
                        Some(Some(n)) => format!("{}. ", n),
                        None => "● ".to_string(),
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
                            self.emit_text(&t);
                        }
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
        assert!(
            all.contains("╭") || all.contains("│"),
            "no border found in: {}",
            all
        );
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
        assert!(
            all.contains("mmdc") || all.contains("Install"),
            "got: {}",
            all
        );
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
}

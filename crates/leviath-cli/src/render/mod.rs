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
//! - Tables, framed and column-fitted (the `table` submodule)
//! - Blockquotes (dim, "│ " prefix)
//! - Inline `code`, **bold**, *italic*, ~~strikethrough~~, links (underlined)
//! - Fenced code blocks (tinted, language tag in title line)
//! - ```mermaid``` flowcharts, drawn as a diagram (the `mermaid` submodule)
//! - Horizontal rules (dim dashes)
//!
//! Plain text that contains no markdown degrades cleanly - it just renders as
//! white text.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

// ─── Palette ──────────────────────────────────────────────────────────────────
//
// Shared with every other Leviath terminal surface. Imported from the single
// definition in `tui::theme` rather than a hand-copied duplicate of the
// dashboard's palette, which is exactly the kind of thing that drifts.

use crate::tui::theme::{C_ACCENT, C_CODE_BG, C_CODE_FG, C_DIM, C_MUTED, C_SUCCESS, C_WHITE};

// ─── Public API ───────────────────────────────────────────────────────────────

/// Convert a markdown string to ratatui `Text` for rendering in a `Paragraph`.
///
/// `width` is what the drawn blocks are laid out to: horizontal rules,
/// tables, and mermaid diagrams.
pub(crate) fn markdown_to_text(input: &str, width: u16) -> Text<'static> {
    let mut renderer = Renderer::new(width);
    renderer.render(input);
    Text::from(renderer.lines)
}

// ─── Renderer internals ───────────────────────────────────────────────────────

/// Whether an inline-HTML event is exactly `<name>`, whatever case it was
/// written in and whatever whitespace it was padded with.
///
/// Deliberately exact: `<u>` underlines, `<ul>` is left to the catch-all that
/// ignores every other tag. A prefix test here would have swallowed it.
fn is_tag(html: &str, name: &str) -> bool {
    html.trim()
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
        .is_some_and(|inner| inner.trim().eq_ignore_ascii_case(name))
}

/// Stack of style modifiers accumulated from nested inline tags.
#[derive(Default, Clone)]
struct InlineStyle {
    bold: bool,
    italic: bool,
    strikethrough: bool,
    /// `<u>…</u>`. Markdown has no underline of its own, so this is the HTML
    /// tag every renderer understands, and the one the dashboard's long-form
    /// editor writes.
    underline: bool,
    code: bool,
    link: bool,
}

impl InlineStyle {
    fn to_ratatui_style(&self) -> Style {
        let mut style = Style::default().fg(C_WHITE);
        if self.code {
            style = style.fg(C_CODE_FG).bg(C_CODE_BG);
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
        if self.underline {
            style = style.add_modifier(Modifier::UNDERLINED);
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
    /// The destination of the link being rendered, held until its text has
    /// been emitted so the URL can follow it.
    pending_link_url: Option<String>,
    /// The table being accumulated. Always present rather than optional: the
    /// cell events only arrive between a `Table` start and end, so an "is
    /// there a table" check on each one is a branch nothing can take.
    table: table::TableBuilder,
    /// Terminal width, which rules, tables and diagrams are drawn to.
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
            pending_link_url: None,
            table: table::TableBuilder::default(),
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
            if parent.underline {
                new_style.underline = true;
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
            // A flowchart is boxes and arrows, and a terminal can draw those.
            // Anything else mermaid can express (a sequence diagram, a class
            // diagram) is shown as its source: a wrong diagram would be worse
            // than an honest listing.
            if let Some(diagram) = mermaid::render(&content, self.width) {
                for line in diagram {
                    self.push_line(line);
                }
            } else {
                self.push_line(Line::from(vec![
                    Span::styled(
                        "  ◇ ",
                        Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("mermaid", Style::default().fg(C_ACCENT)),
                    Span::styled(
                        " - not a flowchart, so here is the source",
                        Style::default().fg(C_DIM),
                    ),
                ]));
                for code_line in &content {
                    self.push_line(Line::from(vec![
                        Span::styled("  │ ", Style::default().fg(C_DIM)),
                        Span::styled(code_line.to_owned(), Style::default().fg(C_MUTED)),
                    ]));
                }
            }
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

    pub(crate) fn render(&mut self, input: &str) {
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
                    // Visual decorators - convey depth without # text (terminal can't vary font size)
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

                // ── Tables ───────────────────────────────────────────────
                //
                // A cell's content arrives as the same inline events as any
                // other text, so each cell is captured by taking the span
                // buffer at its end rather than by a second parser.
                Event::Start(Tag::Table(alignments)) => {
                    let alignments = alignments.iter().map(table::Align::of).collect();
                    self.table = table::TableBuilder::new(alignments);
                }
                Event::Start(Tag::TableHead) => self.table.start_head(),
                Event::End(TagEnd::TableHead) | Event::End(TagEnd::TableRow) => {
                    self.table.end_row();
                }
                Event::End(TagEnd::TableCell) => {
                    let cell = std::mem::take(&mut self.current_spans);
                    self.table.push_cell(cell);
                }
                Event::End(TagEnd::Table) => {
                    let width = self.width;
                    for line in std::mem::take(&mut self.table).draw(width) {
                        self.push_line(line);
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
                    if let Some(Some(n)) = self.list_stack.last_mut() {
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
                            if s.is_empty() { None } else { Some(s) }
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

                // The one HTML tag this renderer reads. Markdown has no
                // underline, `<u>` is what every other renderer takes for it,
                // and the dashboard's long-form editor writes exactly this - so
                // an agent's output and a prompt you typed underline the same
                // way. Every other tag stays ignored by the catch-all below.
                Event::InlineHtml(html) if is_tag(&html, "u") => {
                    self.push_inline(InlineStyle {
                        underline: true,
                        ..Default::default()
                    });
                }
                Event::InlineHtml(html) if is_tag(&html, "/u") => {
                    self.pop_inline();
                }

                // A terminal cannot make text clickable, so a rendered link
                // that shows only its text is a link whose destination the
                // reader has no way to learn. The text is styled, and the URL
                // follows it dim: both halves, which is the whole point of
                // reading the rendered form rather than the markdown.
                Event::Start(Tag::Link { dest_url, .. }) => {
                    self.push_inline(InlineStyle {
                        link: true,
                        ..Default::default()
                    });
                    let url = dest_url.into_string();
                    self.pending_link_url = (!url.is_empty()).then_some(url);
                }
                Event::End(TagEnd::Link) => {
                    self.pop_inline();
                    if let Some(url) = self.pending_link_url.take() {
                        self.current_spans.push(Span::styled(
                            format!(" ({url})"),
                            Style::default().fg(C_DIM),
                        ));
                    }
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
                        Style::default().fg(C_CODE_FG).bg(C_CODE_BG),
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

pub(crate) mod mermaid;
pub(crate) mod table;

#[cfg(test)]
mod tests;

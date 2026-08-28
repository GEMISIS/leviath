//! The crate's long-form text editor: soft-wrapped, markdown-aware, with a
//! toolbar and a rendered preview.
//!
//! Every multi-line box in the TUI (the new-run task, the response pane, a
//! stage's system and transition prompts) used a bare `ratatui-textarea` with
//! its default `WrapMode::None`, which scrolls sideways instead of wrapping.
//! A task longer than the pane is wide therefore ran off the edge, and the
//! text you had just typed was not on screen. This wraps at word boundaries,
//! falling back to splitting a word wider than the pane, so what you type
//! stays inside the box.
//!
//! On top of that it is what a person expects a text editor to be:
//!
//! * **A toolbar of real buttons.** Each one is a chip on a tinted strip, it
//!   lifts under the pointer, and its face is drawn in the style it applies:
//!   the bold button is bold, the strike button is struck through, the code
//!   button wears the colour code renders in. The bottom border names whatever
//!   the pointer is over, and its chord.
//! * **Two views.** `Markdown` shows what you are writing; `Preview` shows how
//!   it will read, through the very same renderer that draws an agent's output
//!   in the run view, so the two can never disagree. Which view you prefer is
//!   remembered between sessions by the host.
//! * **Chords for all of it**, resolving to the same [`MdAction`] the buttons
//!   do, so a binding and its button cannot drift apart.
//!
//! Short fields (an agent's name, a numeric limit) stay on
//! [`LineEdit`](super::line_edit::LineEdit). This one is for prose.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};

use super::line_edit::EditOutcome;
use super::popup::popup_frame;
mod actions;
mod prompts;

use actions::{GROUPS, Slot, chord_char};
pub(crate) use actions::{
    MODE_CHORD, MdAction, MdMode, MdOutcome, action_for, chord_label, shortcut_help,
};
use prompts::{LinkField, LinkPrompt, Prompt, TablePrompt};

use crate::tui::theme::{C_ACCENT, C_BORDER, C_CHROME_BG, C_CHROME_HOVER, C_DIM, C_MUTED, C_WHITE};

/// Heading prefixes, in the order the heading key cycles them.
const HEADINGS: [&str; 3] = ["# ", "## ", "### "];

/// How a [`MarkdownEdit`] is framed this frame.
pub(crate) struct MdEditView {
    /// The block's title.
    pub(crate) title: Line<'static>,
    /// Border colour.
    pub(crate) border: Color,
    /// Whether this box has the keys: it gets a visible cursor and a lit
    /// toolbar.
    pub(crate) focused: bool,
}

impl MdEditView {
    /// A framed box with a plain text title, styled to match its border.
    pub(crate) fn new(title: impl Into<String>, border: Color, focused: bool) -> Self {
        Self::titled(
            Line::from(Span::styled(
                title.into(),
                Style::default().fg(border).add_modifier(Modifier::BOLD),
            )),
            border,
            focused,
        )
    }

    /// The same, with a title the caller has already styled.
    pub(crate) fn titled(title: Line<'static>, border: Color, focused: bool) -> Self {
        Self {
            title,
            border,
            focused,
        }
    }
}

/// A soft-wrapping, markdown-aware multi-line editor with a rendered preview.
#[derive(Debug, Clone)]
pub(crate) struct MarkdownEdit {
    area: TextArea<'static>,
    mode: MdMode,
    /// How far the preview is scrolled, in display rows.
    preview_scroll: u16,
    /// Where each toolbar cell landed in the last frame, for hit testing.
    /// Empty until the first draw, and rewritten by every draw.
    slots: Vec<(Rect, Slot)>,
    /// The cell the pointer is over, if any.
    hovered: Option<Slot>,
    /// Where the text was drawn last frame, so a click in it can be turned
    /// into a cursor position.
    body: Option<Rect>,
    /// Which screen line of the wrapped document sat at the top of `body`.
    ///
    /// The textarea scrolls itself to keep the caret visible and does not say
    /// where it ended up, so this is read back off the drawn frame: the caret
    /// is the one cell wearing the cursor style, and its row plus the caret's
    /// own screen row give the offset.
    top_line: Option<usize>,
    /// The link popup, while it is up.
    prompt: Option<Prompt>,
}

impl Default for MarkdownEdit {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl MarkdownEdit {
    /// An editor over `lines`, cursor at the end of the text.
    pub(crate) fn new(lines: Vec<String>) -> Self {
        let mut area = TextArea::new(lines);
        // The whole point: a long line folds into the pane instead of running
        // off it. `WordOrGlyph` keeps words whole where it can and splits the
        // ones wider than the pane, which is the only way a pasted URL stays
        // visible.
        area.set_wrap_mode(WrapMode::WordOrGlyph);
        area.move_cursor(CursorMove::Bottom);
        area.move_cursor(CursorMove::End);
        Self {
            area,
            mode: MdMode::Source,
            preview_scroll: 0,
            slots: Vec::new(),
            hovered: None,
            body: None,
            top_line: None,
            prompt: None,
        }
    }

    /// An editor over `text`, split on newlines.
    pub(crate) fn from_text(text: &str) -> Self {
        Self::new(text.lines().map(str::to_string).collect())
    }

    /// Open in `mode`. Hosts seed a new box with the view the user last chose.
    pub(crate) fn in_mode(mut self, mode: MdMode) -> Self {
        self.mode = mode;
        self
    }

    /// Which view this box is showing.
    pub(crate) fn mode(&self) -> MdMode {
        self.mode
    }

    /// The lines, as the textarea holds them.
    pub(crate) fn lines(&self) -> &[String] {
        self.area.lines()
    }

    /// The whole buffer as one string.
    pub(crate) fn text(&self) -> String {
        self.area.lines().join("\n")
    }

    /// The greyed-out prompt shown while the buffer is empty.
    pub(crate) fn set_placeholder(&mut self, text: impl Into<String>) {
        self.area.set_placeholder_text(text);
        self.area.set_placeholder_style(Style::default().fg(C_DIM));
    }

    /// The wrapped textarea, for the few callers that need an operation this
    /// wrapper does not name (inserting a completed file path, say).
    pub(crate) fn area_mut(&mut self) -> &mut TextArea<'static> {
        &mut self.area
    }

    /// Whether a popup of this editor's own has the keys.
    ///
    /// Hosts match their own keys (Enter to submit, Esc to close) before
    /// delegating, so they have to ask: while the link popup is up, `Enter`
    /// means "insert the link", not "start the run".
    pub(crate) fn is_modal(&self) -> bool {
        self.prompt.is_some()
    }

    /// Feed a key to the editor.
    ///
    /// Callers take their own keys *before* calling this, except while
    /// [`Self::is_modal`] holds.
    ///
    /// `Preview` is a view of the text, not a lock on it: everything still
    /// edits, and the rendering re-runs as you type, so markup resolves the
    /// moment it is well formed. The strip under the preview shows the line
    /// the cursor is on, in the markdown you are actually typing.
    pub(crate) fn handle_key(&mut self, key: &KeyEvent) -> MdOutcome {
        if self.prompt.is_some() {
            return self.prompt_key(key);
        }
        if let Some(('p', _)) = chord_char(key) {
            return self.set_mode(self.mode.flipped());
        }
        self.apply_key(key)
    }

    /// A key in `Markdown` view: a formatting chord, undo/redo, or the
    /// textarea's own editing keys.
    fn apply_key(&mut self, key: &KeyEvent) -> MdOutcome {
        if let Some(action) = action_for(key) {
            self.apply(action);
            return MdOutcome::Edited;
        }
        // `Ctrl-U` is underline now, and it used to be the textarea's undo.
        // `Ctrl-Z` is what a person reaches for anyway, and it is already undo
        // in the agent editor underneath this overlay.
        match chord_char(key) {
            Some(('z', false)) => {
                self.area.undo();
                MdOutcome::Edited
            }
            Some(('z', true)) => {
                self.area.redo();
                MdOutcome::Edited
            }
            _ => match self.area.input(ratatui_textarea::Input::from(*key)) {
                true => MdOutcome::Edited,
                false => MdOutcome::Ignored,
            },
        }
    }

    /// Keys while a popup is up.
    ///
    /// Tab and the arrows move between the fields, Esc abandons it, and Enter
    /// is the confirmation - except on the first field, where it moves on
    /// rather than finishing before the important half has been typed.
    ///
    /// The popup is taken out and put back rather than borrowed in place, so
    /// there is one place that decides whether it survives the key.
    fn prompt_key(&mut self, key: &KeyEvent) -> MdOutcome {
        let mut prompt = self.prompt.take().expect("a popup, checked by the caller");
        match key.code {
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Down | KeyCode::Up => {
                match &mut prompt {
                    Prompt::Link(link) => link.focus = link.focus.flipped(),
                    Prompt::Table(table) => table.on_columns = !table.on_columns,
                }
                self.prompt = Some(prompt);
                return MdOutcome::Edited;
            }
            // Abandoned: the popup was taken above, so dropping it is enough.
            KeyCode::Esc => return MdOutcome::Edited,
            _ => {}
        }
        let outcome = match &mut prompt {
            Prompt::Link(link) => link.focused_mut().handle_key(key),
            Prompt::Table(table) => table.focused_mut().handle_key(key),
        };
        match outcome {
            EditOutcome::Commit => self.commit_prompt(prompt),
            // Esc is taken above, so the only thing left is ordinary typing.
            _ => {
                self.prompt = Some(prompt);
                MdOutcome::Edited
            }
        }
    }

    /// Enter on a popup: insert what it describes, or move on when there is
    /// still a field below.
    fn commit_prompt(&mut self, prompt: Prompt) -> MdOutcome {
        match prompt {
            Prompt::Link(link) if link.focus == LinkField::Text => {
                self.prompt = Some(Prompt::Link(LinkPrompt {
                    focus: LinkField::Url,
                    ..link
                }));
            }
            Prompt::Link(link) => self.insert_link(link.text.value(), link.url.value()),
            Prompt::Table(table) if table.on_columns => {
                self.prompt = Some(Prompt::Table(TablePrompt {
                    on_columns: false,
                    ..table
                }));
            }
            Prompt::Table(table) => {
                self.area.insert_str(table.markdown());
            }
        }
        MdOutcome::Edited
    }

    /// Write the finished link at the cursor.
    ///
    /// An empty URL still writes `[text]()`, so the markdown is there to
    /// finish rather than silently dropped; an empty text falls back to the
    /// URL, which is what a bare link means.
    fn insert_link(&mut self, text: &str, url: &str) {
        let label = match text.is_empty() {
            true => url,
            false => text,
        };
        self.area.insert_str(format!("[{label}]({url})"));
    }

    /// Switch views, reporting it so the host can remember the choice.
    fn set_mode(&mut self, mode: MdMode) -> MdOutcome {
        self.mode = mode;
        MdOutcome::ModeChanged(mode)
    }

    /// Run a formatting action over the selection, or at the cursor when
    /// nothing is selected.
    pub(crate) fn apply(&mut self, action: MdAction) {
        match action {
            MdAction::Bold => self.wrap_inline("**", "**"),
            MdAction::Italic => self.wrap_inline("*", "*"),
            MdAction::Strike => self.wrap_inline("~~", "~~"),
            MdAction::Underline => self.wrap_inline("<u>", "</u>"),
            MdAction::Code => self.wrap_inline("`", "`"),
            MdAction::CodeBlock => self.fence(),
            MdAction::Link => self.link(),
            MdAction::Heading => self.prefix_lines(next_heading),
            MdAction::Bullet => self.prefix_lines(|line| toggle_prefix(line, "- ")),
            MdAction::Ordered => self.prefix_lines(|line| toggle_prefix(line, "1. ")),
            MdAction::Quote => self.prefix_lines(|line| toggle_prefix(line, "> ")),
            MdAction::Table => self.prompt = Some(Prompt::Table(TablePrompt::default())),
            MdAction::Diagram => self.diagram(),
        }
    }

    /// A flowchart to fill in, rather than an empty fence: mermaid's own
    /// syntax is the part people look up, so the skeleton is the help.
    fn diagram(&mut self) {
        self.area.insert_str(
            "```mermaid\nflowchart TD\n    A[Start] --> B{Choice}\n    \
             B -->|yes| C[Do it]\n    B -->|no| D[Stop]\n```",
        );
    }

    /// Wrap the selection in `open`/`close`, or unwrap it when it is already
    /// wrapped. With no selection, insert an empty pair and park the cursor
    /// between the halves.
    fn wrap_inline(&mut self, open: &str, close: &str) {
        let Some(text) = self.take_selection() else {
            self.area.insert_str(format!("{open}{close}"));
            self.move_back(close.chars().count());
            return;
        };
        let unwrapped = text
            .strip_prefix(open)
            .and_then(|inner| inner.strip_suffix(close))
            .map(str::to_string);
        match unwrapped {
            Some(inner) => {
                self.area.insert_str(inner);
            }
            None => {
                self.area.insert_str(format!("{open}{text}{close}"));
            }
        }
    }

    /// Wrap the selection in a fenced block, or open an empty fence with the
    /// cursor on the line between the fences.
    fn fence(&mut self) {
        match self.take_selection() {
            Some(text) => {
                self.area.insert_str(format!("```\n{text}\n```"));
            }
            None => {
                self.area.insert_str("```\n\n```");
                self.area.move_cursor(CursorMove::Up);
                self.area.move_cursor(CursorMove::End);
            }
        }
    }

    /// Ask for the two halves of a link.
    ///
    /// A popup rather than `[]()` with the cursor parked inside it: the parts
    /// of a link are a caption and a URL, and typing them by walking a cursor
    /// over punctuation is the sort of thing an editor is supposed to spare
    /// you. It is also the only thing that can work in `Preview`, where there
    /// is no cursor to park.
    ///
    /// A selection becomes the caption, so "select the words, press the key"
    /// does what it does everywhere else.
    fn link(&mut self) {
        let text = self.take_selection().unwrap_or_default();
        self.prompt = Some(Prompt::Link(LinkPrompt::new(&text)));
    }

    /// Apply `rule` to the head of every line the selection touches, or to the
    /// cursor's line when nothing is selected.
    ///
    /// `rule` reports how many characters to strip and what to put in their
    /// place, so "toggle it off" and "cycle to the next level" are one walk.
    fn prefix_lines(&mut self, rule: fn(&str) -> (usize, &'static str)) {
        let (first, last) = match self.area.selection_range() {
            Some(((start, _), (end, _))) => (start, end),
            None => {
                let row = self.area.cursor().0;
                (row, row)
            }
        };
        self.area.cancel_selection();
        for row in first..=last {
            // `unwrap_or_default` rather than a guard: the rows come from the
            // cursor or a selection range, so they are always in the buffer,
            // and a guard here would be a branch no test could take.
            let line = self.area.lines().get(row).cloned().unwrap_or_default();
            let (strip, prefix) = rule(&line);
            self.area
                .move_cursor(CursorMove::Jump(row.min(u16::MAX as usize) as u16, 0));
            for _ in 0..strip {
                self.area.delete_next_char();
            }
            if !prefix.is_empty() {
                self.area.insert_str(prefix);
            }
        }
    }

    /// Cut the selection out and hand it back, or `None` when nothing is
    /// selected. The textarea's own yank buffer is how the text comes back.
    fn take_selection(&mut self) -> Option<String> {
        self.area.selection_range()?;
        self.area.cut();
        Some(self.area.yank_text())
    }

    fn move_back(&mut self, chars: usize) {
        for _ in 0..chars {
            self.area.move_cursor(CursorMove::Back);
        }
    }

    /// Draw the framed editor: title, toolbar, and either the text or its
    /// rendered preview.
    ///
    /// The toolbar is dropped when the box is too short to spare a row for it,
    /// and trailing groups are dropped when it is too narrow. A cramped pane
    /// loses buttons, never text.
    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect, view: &MdEditView) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(view.border))
            .title(view.title.clone())
            .title_bottom(self.footer());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        self.slots.clear();
        let body = match inner.height >= 3 {
            true => {
                self.draw_toolbar(frame, Rect { height: 1, ..inner }, view.focused);
                Rect {
                    y: inner.y + 1,
                    height: inner.height - 1,
                    ..inner
                }
            }
            false => inner,
        };

        match self.mode {
            MdMode::Preview => self.draw_preview(frame, body, view.focused),
            MdMode::Source => {
                self.area.set_style(Style::default().fg(C_WHITE));
                self.area.set_cursor_line_style(Style::default());
                self.area.set_cursor_style(match view.focused {
                    true => Style::default().fg(Color::Black).bg(C_ACCENT),
                    false => Style::default(),
                });
                frame.render_widget(&self.area, body);
                self.top_line = caret_offset(frame, body, &self.area);
            }
        }
        self.body = Some(body);
        // Last, and over everything: the popup is the thing with the keys.
        self.draw_prompt(frame, area);
    }

    /// The bottom border: what the pointer is over, or which view this is.
    ///
    /// This is where "what does that button do" is answered. A toolbar of
    /// glyphs with nowhere to read their names is a toolbar you have to guess
    /// at, and the border costs no rows.
    fn footer(&self) -> Line<'static> {
        let text = match self.hovered {
            Some(slot) => slot.hint(self.mode),
            None => match self.mode {
                MdMode::Source => format!("markdown · {MODE_CHORD} previews it"),
                MdMode::Preview => format!("preview · type to edit · {MODE_CHORD} for markdown"),
            },
        };
        Line::from(Span::styled(
            format!(" {text} "),
            Style::default().fg(C_MUTED),
        ))
    }

    /// The rendered view, through the same renderer the run view uses for an
    /// agent's output.
    ///
    /// The preview is not read-only. Typing goes into the buffer underneath
    /// and this re-renders every frame, so markup resolves the moment it is
    /// well formed - which is what makes it a view of the document rather than
    /// a mode you have to leave. The strip along the bottom carries the line
    /// the cursor is on, in the markdown you are typing, because a rendered
    /// document has nowhere to put a caret.
    fn draw_preview(&mut self, frame: &mut Frame, body: Rect, focused: bool) {
        // A focused box spends its last row on the source strip. An unfocused
        // one has no cursor worth showing, and keeps the row for the document.
        let (doc, strip) = match focused && body.height >= 3 {
            true => (
                Rect {
                    height: body.height - 1,
                    ..body
                },
                Some(Rect {
                    y: body.y + body.height - 1,
                    height: 1,
                    ..body
                }),
            ),
            false => (body, None),
        };

        let text = crate::render::markdown_to_text(&self.text(), doc.width);
        let rows = wrapped_rows(&text.lines, doc.width);
        let height = doc.height as usize;
        let max = rows.saturating_sub(height);
        // Follow the caret rather than keeping a scroll of its own: the line
        // you are editing is the line that has to be on screen, and one
        // offset that answers to the cursor cannot disagree with itself.
        let top = self
            .caret_display_row(doc.width)
            .saturating_sub(height)
            .min(max);
        self.preview_scroll = top.min(u16::MAX as usize) as u16;
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((self.preview_scroll, 0)),
            doc,
        );
        if let Some(strip) = strip {
            self.draw_source_strip(frame, strip);
        }
    }

    /// Which display row of the rendered document the caret's line ends on.
    ///
    /// Measured by rendering the document up to and including that line: the
    /// renderer is block-structured, so a prefix of the source renders as a
    /// prefix of the output, and this is exact wherever that holds. Where it
    /// does not (mid-fence, say) it is off by the height of the unclosed
    /// block, and the strip below still shows what you are typing.
    fn caret_display_row(&self, width: u16) -> usize {
        let row = self.area.cursor().0;
        let prefix = self
            .area
            .lines()
            .iter()
            .take(row + 1)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        wrapped_rows(
            &crate::render::markdown_to_text(&prefix, width).lines,
            width,
        )
    }

    /// The line under the cursor, as markdown, with the caret on it.
    ///
    /// Windowed on the caret so a long line keeps the part you are typing in
    /// view, and split on chars because the buffer holds whatever was typed.
    fn draw_source_strip(&self, frame: &mut Frame, strip: Rect) {
        let (row, col) = {
            let cursor = self.area.cursor();
            (cursor.0, cursor.1)
        };
        let line = self.area.lines().get(row).cloned().unwrap_or_default();
        let marker = Span::styled("▏", Style::default().fg(C_ACCENT));
        let room = strip.width.saturating_sub(1) as usize;
        // Keep the caret in the window: scroll only once it would fall off.
        let from = (col + 1).saturating_sub(room);
        let before: String = line.chars().skip(from).take(col - from).collect();
        let at: String = line.chars().skip(col).take(1).collect();
        let after: String = line.chars().skip(col + 1).collect();
        let plain = Style::default().fg(C_MUTED).bg(C_CHROME_BG);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                marker,
                Span::styled(before, plain),
                Span::styled(
                    match at.is_empty() {
                        true => " ".to_string(),
                        false => at,
                    },
                    Style::default().fg(Color::Black).bg(C_ACCENT),
                ),
                Span::styled(after, plain),
            ]))
            .style(Style::default().bg(C_CHROME_BG)),
            strip,
        );
    }

    /// The popup, drawn over the box that owns it.
    fn draw_prompt(&self, frame: &mut Frame, area: Rect) {
        let Some(prompt) = self.prompt.as_ref() else {
            return;
        };
        let width = area.width.saturating_sub(4).clamp(12, 56);
        let height = 6u16.min(area.height);
        let popup = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };
        frame.render_widget(ratatui::widgets::Clear, popup);
        let (title, fields) = match prompt {
            Prompt::Link(link) => (
                "Link",
                vec![
                    ("Text  ", &link.text, link.focus == LinkField::Text),
                    ("URL   ", &link.url, link.focus == LinkField::Url),
                ],
            ),
            Prompt::Table(table) => (
                "Table",
                vec![
                    ("Columns  ", &table.columns, table.on_columns),
                    ("Rows     ", &table.rows, !table.on_columns),
                ],
            ),
        };
        let inner = popup_frame(frame, popup, title, C_ACCENT);
        let mut lines: Vec<Line<'static>> = fields
            .into_iter()
            .map(|(label, edit, focused)| {
                let colour = match focused {
                    true => C_ACCENT,
                    false => C_DIM,
                };
                let mut spans = vec![Span::styled(label, Style::default().fg(colour))];
                spans.extend(edit.display_spans(true).spans);
                Line::from(spans)
            })
            .collect();
        lines.push(Line::from(Span::styled(
            "Tab switches · Enter inserts · Esc cancels",
            Style::default().fg(C_MUTED),
        )));
        frame.render_widget(Paragraph::new(lines), inner);
    }

    /// The button row, and the registry that makes it clickable.
    fn draw_toolbar(&mut self, frame: &mut Frame, row: Rect, focused: bool) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut x = row.x;
        for group in GROUPS {
            let sep = u16::from(x > row.x);
            let width: u16 = group.iter().map(|slot| slot.width()).sum();
            if x + sep + width > row.x + row.width {
                break;
            }
            if sep > 0 {
                spans.push(Span::styled(
                    "│",
                    Style::default().fg(C_BORDER).bg(C_CHROME_BG),
                ));
                x += 1;
            }
            for slot in group {
                spans.push(Span::styled(
                    slot.face(self.mode),
                    self.chip_style(*slot, focused),
                ));
                self.slots.push((
                    Rect {
                        x,
                        y: row.y,
                        width: slot.width(),
                        height: 1,
                    },
                    *slot,
                ));
                x += slot.width();
            }
        }
        // The tint goes on the paragraph rather than a leading blank span: a
        // span the width of the row would consume it, and every button after
        // it would be clipped away. Each chip's own background paints over
        // this one.
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(C_CHROME_BG)),
            row,
        );
    }

    /// A chip: filled when it is the view you are in, lifted under the
    /// pointer, and otherwise wearing the style it applies.
    fn chip_style(&self, slot: Slot, focused: bool) -> Style {
        // The switch is always filled: it is the one control that says what
        // the box currently is, and a button that reports state has to look
        // different from the ones that do something to the text.
        if slot == Slot::ViewSwitch {
            let bg = match self.hovered == Some(slot) {
                true => C_CHROME_HOVER,
                false => C_ACCENT,
            };
            let fg = match self.hovered == Some(slot) {
                true => C_ACCENT,
                false => Color::Black,
            };
            return Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD);
        }
        let base = match slot {
            Slot::Format(action) if focused => action.face(),
            // An unfocused box still shows its toolbar, so you can see it is
            // there, but greyed rather than competing with the box that has
            // the keys. (The switch never reaches here: it returned above.)
            _ => Style::default().fg(C_DIM),
        };
        match self.hovered == Some(slot) {
            true => base.bg(C_CHROME_HOVER),
            false => base.bg(C_CHROME_BG),
        }
    }

    /// The cell at `(column, row)`, in absolute screen coordinates.
    fn slot_at(&self, column: u16, row: u16) -> Option<Slot> {
        self.slots
            .iter()
            .find(|(rect, _)| rect.contains(Position::new(column, row)))
            .map(|(_, slot)| *slot)
    }

    /// Note where the pointer is, so the button under it lifts and the bottom
    /// border names it. Reports whether anything changed.
    pub(crate) fn hover(&mut self, column: u16, row: u16) -> bool {
        let was = self.hovered;
        self.hovered = self.slot_at(column, row);
        was != self.hovered
    }

    /// Put the caret where the pointer is, when the click landed in the text.
    ///
    /// Only in `Edit`, and only while the box is focused: the rendered view
    /// has no invertible map back to a source position, and an unfocused box
    /// has no caret drawn to measure the scroll from.
    ///
    /// The data position is found by asking the textarea where each candidate
    /// would sit on screen - the row first, then the column within it - which
    /// costs a walk of the lines rather than a second copy of its wrapping
    /// rules. A copy is the thing that would quietly disagree.
    fn place_caret(&mut self, column: u16, row: u16) -> bool {
        let (Some(body), Some(top)) = (self.body, self.top_line) else {
            return false;
        };
        if self.mode != MdMode::Source || !body.contains(Position::new(column, row)) {
            return false;
        }
        let want_row = top + (row - body.y) as usize;
        let want_col = (column - body.x) as usize;

        let at = |area: &mut TextArea<'static>, r: usize, c: usize| {
            area.move_cursor(CursorMove::Jump(
                r.min(u16::MAX as usize) as u16,
                c.min(u16::MAX as usize) as u16,
            ));
            let screen = area.screen_cursor();
            (screen.row, screen.col)
        };

        // The last line whose first screen row is at or above the click.
        let mut line = 0usize;
        for r in 0..self.area.lines().len() {
            if at(&mut self.area, r, 0).0 <= want_row {
                line = r;
            } else {
                break;
            }
        }
        // Then the last column of that line still at or before the click.
        let len = self.area.lines()[line].chars().count();
        let mut best = 0usize;
        for c in 0..=len {
            let (sr, sc) = at(&mut self.area, line, c);
            if sr < want_row || (sr == want_row && sc <= want_col) {
                best = c;
            } else {
                break;
            }
        }
        self.area.cancel_selection();
        at(&mut self.area, line, best);
        true
    }

    /// Act on a click at `(column, row)`. Coordinates are absolute, as
    /// crossterm reports them.
    pub(crate) fn click(&mut self, column: u16, row: u16) -> MdOutcome {
        match self.slot_at(column, row) {
            Some(Slot::ViewSwitch) => self.set_mode(self.mode.flipped()),
            Some(Slot::Format(action)) => {
                // No mode to leave: the preview renders the buffer as it is,
                // so formatting from there shows up where you are looking.
                self.apply(action);
                MdOutcome::Edited
            }
            None => match self.place_caret(column, row) {
                true => MdOutcome::Edited,
                false => MdOutcome::Ignored,
            },
        }
    }
}

/// Which screen line of the wrapped document sits at the top of `body`.
///
/// Read off the frame that was just drawn: the caret is the one cell in the
/// body wearing the cursor's background, and the textarea will say which
/// screen row the caret is on, so the difference is the scroll offset.
fn caret_offset(frame: &mut Frame, body: Rect, area: &TextArea<'static>) -> Option<usize> {
    let want = area.cursor_style().bg?;
    let buffer = frame.buffer_mut();
    let caret_y = (body.y..body.y + body.height).find(|y| {
        (body.x..body.x + body.width).any(|x| {
            buffer
                .cell((x, *y))
                .is_some_and(|c| c.style().bg == Some(want))
        })
    })?;
    Some(
        area.screen_cursor()
            .row
            .saturating_sub((caret_y - body.y) as usize),
    )
}

/// How many display rows `lines` take at `width`, which is what a scroll
/// offset has to be clamped against once the paragraph wraps.
fn wrapped_rows(lines: &[Line<'static>], width: u16) -> usize {
    let width = width.max(1) as usize;
    lines
        .iter()
        .map(|line| line.width().div_ceil(width).max(1))
        .sum()
}

/// `# ` → `## ` → `### ` → nothing → `# `.
fn next_heading(line: &str) -> (usize, &'static str) {
    match HEADINGS.iter().position(|level| line.starts_with(level)) {
        Some(i) => (
            HEADINGS[i].chars().count(),
            HEADINGS.get(i + 1).copied().unwrap_or(""),
        ),
        None => (0, HEADINGS[0]),
    }
}

/// Strip `prefix` when the line already has it, add it when it does not.
///
/// A numbered list also matches any other number, so toggling `2. ` off works
/// as well as toggling `1. ` off: markdown renumbers a list itself, and a
/// bullet key that only recognised the number it wrote is a bullet key that
/// stops working on the second line.
fn toggle_prefix(line: &str, prefix: &'static str) -> (usize, &'static str) {
    if line.starts_with(prefix) {
        return (prefix.chars().count(), "");
    }
    if prefix == "1. " {
        let digits = line.chars().take_while(char::is_ascii_digit).count();
        // Char-wise rather than `line[digits..]`: this crate denies byte-index
        // slicing of user text, and the line under the cursor is user text.
        let after: String = line.chars().skip(digits).take(2).collect();
        if digits > 0 && after == ". " {
            return (digits + 2, "");
        }
    }
    (0, prefix)
}
#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::actions::{ACTIONS, CHORD_LABELS};

    use super::super::line_edit::LineEdit;
    use super::*;
    use crate::tui::theme::C_CODE_FG;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn chord(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn plain(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty())
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn edit(text: &str) -> MarkdownEdit {
        MarkdownEdit::from_text(text)
    }

    /// Select `chars` characters back from the cursor, the way Shift+← does.
    fn select_back(md: &mut MarkdownEdit, chars: usize) {
        md.area_mut().start_selection();
        for _ in 0..chars {
            md.area_mut().move_cursor(CursorMove::Back);
        }
    }

    /// Draw into a `width` x `height` terminal and return every row as its own
    /// string, so a test can say "this word is on screen" without caring which
    /// row wrapping put it on.
    fn draw(md: &mut MarkdownEdit, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| {
                let view = MdEditView::new(" T ", C_ACCENT, true);
                md.render(f, f.area(), &view);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()).to_string())
                    .collect()
            })
            .collect()
    }

    // ── The bug this component exists for ──────────────────────────────────

    /// The regression: a task longer than the pane is wide used to scroll
    /// sideways, so with the cursor at the end the *start* of what you had
    /// written was off screen. Both ends have to be visible at once.
    #[test]
    fn a_line_wider_than_the_pane_wraps_instead_of_scrolling_off_the_edge() {
        let mut md = edit("the quick brown fox jumps over the lazy dog");
        let rows = draw(&mut md, 24, 8).join("\n");
        assert!(rows.contains("quick"), "the head is off screen:\n{rows}");
        assert!(rows.contains("dog"), "the tail is off screen:\n{rows}");
    }

    /// A single word with no space in it (a pasted URL is the real case) still
    /// has to fold, which is why the mode is `WordOrGlyph` and not `Word`.
    #[test]
    fn a_word_wider_than_the_pane_is_split_rather_than_hidden() {
        let mut md = edit("https://example.com/a/very/long/path/that/never/breaks");
        let rows = draw(&mut md, 24, 8).join("\n");
        assert!(rows.contains("https"), "the head is off screen:\n{rows}");
        assert!(rows.contains("breaks"), "the tail is off screen:\n{rows}");
    }

    // ── Chords ─────────────────────────────────────────────────────────────

    #[test]
    fn every_toolbar_action_has_a_chord_and_a_label() {
        assert_eq!(ACTIONS.len(), CHORD_LABELS.len());
        let help = shortcut_help();
        assert_eq!(help.len(), ACTIONS.len());
        for (i, action) in ACTIONS.iter().enumerate() {
            assert_eq!(chord_label(*action), CHORD_LABELS[i]);
            assert_eq!(help[i], (CHORD_LABELS[i], action.name()));
            assert!(!action.label().is_empty());
        }
    }

    #[test]
    fn the_control_chords_resolve_to_their_actions() {
        for (c, action) in [
            ('b', MdAction::Bold),
            ('i', MdAction::Italic),
            ('d', MdAction::Strike),
            ('e', MdAction::Code),
            ('k', MdAction::Link),
            ('h', MdAction::Heading),
            ('l', MdAction::Bullet),
            ('o', MdAction::Ordered),
            ('.', MdAction::Quote),
        ] {
            assert_eq!(action_for(&chord(c)), Some(action), "ctrl-{c}");
        }
    }

    /// macOS terminals that forward Command send `SUPER`, and the same
    /// bindings have to answer to it.
    #[test]
    fn the_command_modifier_resolves_the_same_actions() {
        let key = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::SUPER);
        assert_eq!(action_for(&key), Some(MdAction::Bold));
    }

    /// Shift reaches us as the flag, as an uppercased char, or both, and the
    /// fence has to answer to all three spellings while plain `e` does not.
    #[test]
    fn shift_e_is_the_code_fence_however_the_terminal_spells_it() {
        let flag = KeyEvent::new(
            KeyCode::Char('e'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        let upper = KeyEvent::new(KeyCode::Char('E'), KeyModifiers::CONTROL);
        let both = KeyEvent::new(
            KeyCode::Char('E'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        for key in [flag, upper, both] {
            assert_eq!(action_for(&key), Some(MdAction::CodeBlock), "{key:?}");
        }
        assert_eq!(action_for(&chord('e')), Some(MdAction::Code));
    }

    #[test]
    fn a_plain_key_an_unbound_chord_and_a_non_character_are_not_formatting() {
        assert_eq!(action_for(&plain('b')), None);
        assert_eq!(action_for(&chord('z')), None);
        assert_eq!(
            action_for(&KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL)),
            None
        );
    }

    // ── Inline wrapping ────────────────────────────────────────────────────

    #[test]
    fn a_chord_with_nothing_selected_opens_an_empty_pair_around_the_cursor() {
        let mut md = edit("");
        assert_eq!(md.handle_key(&chord('b')), MdOutcome::Edited);
        assert_eq!(md.text(), "****");
        // The cursor sits between the halves, so typing lands inside them.
        let _ = md.handle_key(&plain('h'));
        let _ = md.handle_key(&plain('i'));
        assert_eq!(md.text(), "**hi**");
    }

    #[test]
    fn each_inline_chord_uses_its_own_marker() {
        for (c, wrapped) in [
            ('b', "**word**"),
            ('i', "*word*"),
            ('d', "~~word~~"),
            ('e', "`word`"),
        ] {
            let mut md = edit("word");
            select_back(&mut md, 4);
            let _ = md.handle_key(&chord(c));
            assert_eq!(md.text(), wrapped, "ctrl-{c}");
        }
    }

    #[test]
    fn wrapping_a_selection_that_is_already_wrapped_takes_the_markers_off() {
        let mut md = edit("**word**");
        select_back(&mut md, 8);
        md.apply(MdAction::Bold);
        assert_eq!(md.text(), "word");
    }

    #[test]
    fn multibyte_text_survives_being_wrapped_and_unwrapped() {
        let mut md = edit("héllo wörld");
        select_back(&mut md, 11);
        md.apply(MdAction::Italic);
        assert_eq!(md.text(), "*héllo wörld*");
        select_back(&mut md, 13);
        md.apply(MdAction::Italic);
        assert_eq!(md.text(), "héllo wörld");
    }

    // ── Links and fences ───────────────────────────────────────────────────

    /// A selection becomes the caption, so only the URL is left to type, and
    /// the popup opens on that field.
    #[test]
    fn a_link_takes_the_selection_as_its_caption_and_asks_for_the_url() {
        let mut md = edit("docs");
        select_back(&mut md, 4);
        md.apply(MdAction::Link);
        assert!(md.is_modal(), "the popup has the keys");
        assert_eq!(md.text(), "", "the selection was lifted into the popup");

        for c in "https://x".chars() {
            let _ = md.handle_key(&plain(c));
        }
        let _ = md.handle_key(&press(KeyCode::Enter));
        assert!(!md.is_modal());
        assert_eq!(md.text(), "[docs](https://x)");
    }

    /// With nothing selected there are two things to type, and Enter on the
    /// caption moves to the URL rather than inserting half a link.
    #[test]
    fn a_link_with_nothing_selected_asks_for_both_halves() {
        let mut md = edit("");
        md.apply(MdAction::Link);
        assert!(md.is_modal());
        for c in "docs".chars() {
            let _ = md.handle_key(&plain(c));
        }
        let _ = md.handle_key(&press(KeyCode::Enter));
        assert!(md.is_modal(), "Enter moved on to the URL");
        assert_eq!(md.text(), "");
        let _ = md.handle_key(&plain('u'));
        let _ = md.handle_key(&press(KeyCode::Enter));
        assert_eq!(md.text(), "[docs](u)");
    }

    #[test]
    fn the_link_popup_switches_fields_and_can_be_abandoned() {
        let mut md = edit("");
        md.apply(MdAction::Link);
        // Tab lands on the URL; typing there fills it, not the caption.
        let _ = md.handle_key(&press(KeyCode::Tab));
        let _ = md.handle_key(&plain('u'));
        let _ = md.handle_key(&press(KeyCode::BackTab));
        let _ = md.handle_key(&plain('t'));
        let _ = md.handle_key(&press(KeyCode::Down));
        let _ = md.handle_key(&press(KeyCode::Enter));
        assert_eq!(md.text(), "[t](u)");

        // Esc abandons it, leaving the buffer alone.
        let mut md = edit("keep");
        md.apply(MdAction::Link);
        let _ = md.handle_key(&press(KeyCode::Esc));
        assert!(!md.is_modal());
        assert_eq!(md.text(), "keep");
    }

    /// A URL with no caption is a bare link: the URL reads as its own text.
    #[test]
    fn a_link_with_no_caption_uses_the_url_as_the_caption() {
        let mut md = edit("");
        md.apply(MdAction::Link);
        let _ = md.handle_key(&press(KeyCode::Tab));
        for c in "https://x".chars() {
            let _ = md.handle_key(&plain(c));
        }
        let _ = md.handle_key(&press(KeyCode::Enter));
        assert_eq!(md.text(), "[https://x](https://x)");
    }

    /// The popup is drawn over the box, with both fields on it.
    #[test]
    fn the_link_popup_shows_both_halves() {
        let mut md = edit("");
        md.apply(MdAction::Link);
        let framed = draw(&mut md, 60, 12).join("\n");
        assert!(framed.contains("Link"), "{framed}");
        assert!(framed.contains("Text"), "{framed}");
        assert!(framed.contains("URL"), "{framed}");
        assert!(framed.contains("Esc cancels"), "{framed}");
    }

    #[test]
    fn a_fence_wraps_the_selection_or_opens_an_empty_block() {
        let mut md = edit("let x = 1;");
        select_back(&mut md, 10);
        md.apply(MdAction::CodeBlock);
        assert_eq!(md.lines(), ["```", "let x = 1;", "```"]);

        let mut empty = edit("");
        empty.apply(MdAction::CodeBlock);
        assert_eq!(empty.lines(), ["```", "", "```"]);
        // The cursor is on the blank line between the fences.
        let _ = empty.handle_key(&plain('y'));
        assert_eq!(empty.lines(), ["```", "y", "```"]);
    }

    // ── Line prefixes ──────────────────────────────────────────────────────

    #[test]
    fn the_heading_key_cycles_the_levels_and_then_clears_them() {
        let mut md = edit("title");
        for expected in ["# title", "## title", "### title", "title", "# title"] {
            md.apply(MdAction::Heading);
            assert_eq!(md.text(), expected);
        }
    }

    #[test]
    fn the_list_and_quote_keys_toggle_their_own_prefix() {
        for (action, prefixed) in [
            (MdAction::Bullet, "- item"),
            (MdAction::Ordered, "1. item"),
            (MdAction::Quote, "> item"),
        ] {
            let mut md = edit("item");
            md.apply(action);
            assert_eq!(md.text(), prefixed);
            md.apply(action);
            assert_eq!(md.text(), "item", "{prefixed} did not toggle off");
        }
    }

    /// Markdown renumbers a list itself, so the second line of one is `2. `.
    /// A numbering key that only recognised `1. ` would refuse to clear it.
    #[test]
    fn the_numbering_key_clears_a_number_it_did_not_write() {
        let mut md = edit("12. item");
        md.apply(MdAction::Ordered);
        assert_eq!(md.text(), "item");

        // A bare number with no ". " after it is ordinary text.
        let mut plain_number = edit("2001 was a year");
        plain_number.apply(MdAction::Ordered);
        assert_eq!(plain_number.text(), "1. 2001 was a year");
    }

    #[test]
    fn a_prefix_applies_to_every_line_the_selection_touches() {
        let mut md = edit("one\ntwo\nthree");
        select_back(&mut md, 13);
        md.apply(MdAction::Bullet);
        assert_eq!(md.lines(), ["- one", "- two", "- three"]);
    }

    // ── Falling through to the textarea ────────────────────────────────────

    #[test]
    fn keys_that_are_not_chords_still_edit_the_text() {
        let mut md = edit("ab");
        let _ = md.handle_key(&plain('c'));
        let _ = md.handle_key(&KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()));
        let _ = md.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        let _ = md.handle_key(&plain('z'));
        assert_eq!(md.lines(), ["ab", "z"]);
    }

    #[test]
    fn the_buffer_round_trips_through_text_and_lines() {
        let md = edit("first\nsecond");
        assert_eq!(md.text(), "first\nsecond");
        assert_eq!(md.lines(), ["first", "second"]);
        assert_eq!(MarkdownEdit::default().text(), "");
    }

    // ── The toolbar ────────────────────────────────────────────────────────

    #[test]
    fn the_toolbar_draws_every_button_when_there_is_room() {
        let mut md = edit("");
        let bar = draw(&mut md, 70, 6).remove(1);
        for action in ACTIONS {
            let label = action.label();
            assert!(bar.contains(label), "{label} is missing from {bar}");
        }
        // The switch says which view you are in, and says the other one once
        // you press it.
        assert!(bar.contains(MdMode::Source.label()), "{bar}");
        let _ = md.handle_key(&chord('p'));
        let flipped = draw(&mut md, 70, 6).remove(1);
        assert!(flipped.contains(MdMode::Preview.label()), "{flipped}");
        assert!(!flipped.contains(MdMode::Source.label()), "{flipped}");
    }

    /// The whole bar needs 61 columns, inside what the task pane has on a
    /// 120-column terminal. This is the number to check against when a box
    /// turns out to be dropping groups.
    #[test]
    fn the_full_toolbar_fits_in_sixty_one_columns() {
        let separators = GROUPS.len() as u16 - 1;
        let buttons: u16 = GROUPS
            .iter()
            .flat_map(|group| group.iter())
            .map(|slot| slot.width())
            .sum();
        assert_eq!(buttons + separators, 61);
    }

    /// A cramped pane drops buttons off the end rather than drawing outside
    /// its own rect, and a box with no room for a toolbar row keeps all of its
    /// height for text.
    #[test]
    fn a_cramped_box_drops_buttons_and_then_the_whole_toolbar() {
        let mut narrow = edit("");
        let bar = draw(&mut narrow, 24, 6).remove(1);
        assert!(bar.contains("Edit"), "the view switch survives: {bar}");
        assert!(!bar.contains("```"), "the later groups do not: {bar}");

        let mut short = edit("text");
        let first = draw(&mut short, 40, 3).remove(1);
        assert!(first.contains("text"), "{first}");
        // Row 1 is the text now, not a button row: a press there moves the
        // caret and formats nothing.
        assert_eq!(short.click(2, 1), MdOutcome::Edited);
        assert_eq!(short.text(), "text");
        // And the border is nobody's.
        assert_eq!(short.click(0, 0), MdOutcome::Ignored);
    }

    #[test]
    fn the_placeholder_shows_only_while_the_box_is_empty() {
        let mut md = MarkdownEdit::default();
        md.set_placeholder("say something");
        let rows = draw(&mut md, 40, 6).join("\n");
        assert!(rows.contains("say something"), "{rows}");

        let _ = md.handle_key(&plain('x'));
        let rows = draw(&mut md, 40, 6).join("\n");
        assert!(!rows.contains("say something"), "{rows}");
    }

    /// Where the `B` button lands, now that the view switch leads the bar.
    fn bold_button(md: &mut MarkdownEdit) -> u16 {
        let bar = draw(md, 70, 8).remove(1);
        // Every glyph on the row is one column wide, so a char index is a
        // column, and nothing before the bold button contains a `B`.
        bar.chars().position(|c| c == 'B').expect("a bold button") as u16
    }

    #[test]
    fn clicking_a_button_runs_its_action_and_clicking_elsewhere_does_not() {
        let mut md = edit("");
        let bold = bold_button(&mut md);
        assert_eq!(md.click(bold, 1), MdOutcome::Edited);
        assert_eq!(md.text(), "****");

        assert_eq!(md.click(69, 1), MdOutcome::Ignored, "past the last button");
        // The text is not a button: a press there moves the caret and leaves
        // the buffer as it was.
        assert_eq!(md.click(bold, 4), MdOutcome::Edited);
        assert_eq!(md.text(), "****");
    }

    /// An unfocused box still shows its toolbar (that is how you learn it is
    /// there) but draws no cursor cell over the text.
    #[test]
    fn an_unfocused_box_draws_a_toolbar_and_no_cursor() {
        let mut md = edit("hi");
        let mut terminal = Terminal::new(TestBackend::new(40, 6)).unwrap();
        terminal
            .draw(|f| {
                let view = MdEditView::titled(Line::from("t"), C_BORDER, false);
                md.render(f, f.area(), &view);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let toolbar: String = (0..40)
            .map(|x| buf.cell((x, 1)).map_or(" ", |c| c.symbol()).to_string())
            .collect();
        assert!(toolbar.contains("Edit"), "{toolbar}");
        // The filled chip on the toolbar row is the view you are in; below it,
        // an unfocused box paints no cursor.
        let body_has_cursor = (2..6).any(|y| {
            (0..40).any(|x| {
                buf.cell((x, y))
                    .is_some_and(|c| c.style().bg == Some(C_ACCENT))
            })
        });
        assert!(!body_has_cursor, "an unfocused box drew a cursor cell");
    }

    // ── The two views ──────────────────────────────────────────────────────

    /// The preview is the point: markers stop being text and start being
    /// style, through the same renderer the run view uses.
    #[test]
    fn the_preview_renders_the_markup_instead_of_showing_it() {
        let mut md = edit("**loud** and ~~gone~~ and <u>under</u>");
        let source = draw(&mut md, 50, 8).join("\n");
        assert!(source.contains("**loud**"), "markdown view: {source}");

        assert_eq!(
            md.handle_key(&chord('p')),
            MdOutcome::ModeChanged(MdMode::Preview)
        );
        let mut terminal = Terminal::new(TestBackend::new(50, 8)).unwrap();
        terminal
            .draw(|f| {
                let view = MdEditView::new(" t ", C_ACCENT, true);
                md.render(f, f.area(), &view);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        // Rows 2..6 are the rendered document: the toolbar is above it and the
        // source strip, which shows the markdown on purpose, is below.
        let document: String = (2..6)
            .flat_map(|y| (0..50).map(move |x| (x, y)))
            .map(|(x, y)| buf.cell((x, y)).map_or(" ", |c| c.symbol()).to_string())
            .collect();
        assert!(!document.contains("**loud**"), "still raw: {document}");
        assert!(document.contains("loud"), "{document}");

        // And the words carry the styles the markers asked for.
        let styled = |needle: char, modifier: Modifier| {
            buf.content()
                .iter()
                .any(|c| c.symbol() == needle.to_string() && c.style().add_modifier == modifier)
        };
        assert!(styled('l', Modifier::BOLD), "bold is not bold");
        assert!(styled('g', Modifier::CROSSED_OUT), "strike is not struck");
        assert!(
            styled('u', Modifier::UNDERLINED),
            "underline is not underlined"
        );
    }

    #[test]
    fn the_view_chord_toggles_and_reports_the_choice_both_ways() {
        let mut md = edit("x");
        assert_eq!(md.mode(), MdMode::Source);
        assert_eq!(
            md.handle_key(&chord('p')),
            MdOutcome::ModeChanged(MdMode::Preview)
        );
        assert_eq!(md.mode(), MdMode::Preview);
        assert_eq!(
            md.handle_key(&chord('p')),
            MdOutcome::ModeChanged(MdMode::Source)
        );
        assert_eq!(md.mode(), MdMode::Source);

        // A host can also open a box already in the remembered view.
        assert_eq!(edit("x").in_mode(MdMode::Preview).mode(), MdMode::Preview);
    }

    /// The preview is a view of the document, not a lock on it: typing goes
    /// in, the box stays where it is, and the rendering resolves the markup as
    /// soon as it is well formed.
    #[test]
    fn typing_in_the_preview_edits_it_and_the_rendering_keeps_up() {
        let mut md = edit("ab").in_mode(MdMode::Preview);
        assert_eq!(md.handle_key(&plain('c')), MdOutcome::Edited);
        assert_eq!(md.mode(), MdMode::Preview, "still the rendered view");
        assert_eq!(md.text(), "abc");

        // Half-written markup is still text; finishing it renders.
        let mut md = edit("").in_mode(MdMode::Preview);
        for c in "**loud".chars() {
            let _ = md.handle_key(&plain(c));
        }
        let half = draw(&mut md, 40, 8).join("\n");
        assert!(half.contains("**loud"), "unclosed markup is text: {half}");
        for c in "**".chars() {
            let _ = md.handle_key(&plain(c));
        }
        let whole = draw(&mut md, 40, 8);
        // The last two rows are the source strip and the border; the document
        // is what is above them.
        let body = whole[..whole.len() - 2].join("\n");
        assert!(!body.contains("**loud"), "it resolved: {body}");
        assert!(body.contains("loud"), "{body}");

        // A formatting chord works there too, without leaving.
        let mut md = edit("").in_mode(MdMode::Preview);
        let _ = md.handle_key(&chord('b'));
        assert_eq!(md.mode(), MdMode::Preview);
        assert_eq!(md.text(), "****");
    }

    /// The rendered document has nowhere to put a caret, so the strip under it
    /// carries the line the cursor is on, as markdown.
    #[test]
    fn the_preview_shows_the_line_being_typed_underneath_it() {
        let mut md = edit("# Title\n\n**loud**").in_mode(MdMode::Preview);
        // Mid-line, so the caret sits on a character rather than past the end.
        for _ in 0..3 {
            let _ = md.handle_key(&press(KeyCode::Left));
        }
        let rows = draw(&mut md, 40, 10);
        let strip = rows[rows.len() - 2].clone();
        assert!(strip.contains("**loud**"), "the raw line: {strip:?}");
        // And the document above it is rendered rather than raw.
        let body = rows[..rows.len() - 2].join("\n");
        assert!(body.contains("Title"), "{body}");

        // An unfocused box has no cursor worth showing and keeps the row.
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|f| {
                let view = MdEditView::titled(Line::from("t"), C_BORDER, false);
                md.render(f, f.area(), &view);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let last: String = (0..40)
            .map(|x| buf.cell((x, 8)).map_or(" ", |c| c.symbol()).to_string())
            .collect();
        assert!(
            !last.contains("**loud**"),
            "no strip when unfocused: {last}"
        );
    }

    /// The preview follows the caret rather than keeping a scroll of its own,
    /// so moving through a long document brings the rendering with you.
    #[test]
    fn the_preview_follows_the_caret_through_a_long_document() {
        let long: String = (0..60).map(|i| format!("line {i}\n")).collect();
        let mut md = edit(&long).in_mode(MdMode::Preview);

        // The cursor starts at the end, so the end is what is on screen.
        let rows = draw(&mut md, 40, 12).join("\n");
        assert!(rows.contains("line 59"), "{rows}");
        assert!(!rows.contains("line 0"), "{rows}");

        // Walking it back to the top brings the top into view.
        for _ in 0..80 {
            let _ = md.handle_key(&press(KeyCode::Up));
        }
        let rows = draw(&mut md, 40, 12).join("\n");
        assert!(rows.contains("line 0"), "{rows}");
        assert_eq!(md.mode(), MdMode::Preview);
    }

    #[test]
    fn the_view_switch_is_clickable_and_a_format_button_works_from_the_preview() {
        let mut md = edit("x");
        let bar = draw(&mut md, 70, 8).remove(1);
        // The switch is the one cell with the flip glyph on it.
        let switch = bar.chars().position(|c| c == '⇄').expect("a switch") as u16;
        assert_eq!(md.click(switch, 1), MdOutcome::ModeChanged(MdMode::Preview));
        assert_eq!(md.mode(), MdMode::Preview);
        // And back again: one button, both ways.
        assert_eq!(md.click(switch, 1), MdOutcome::ModeChanged(MdMode::Source));
        assert_eq!(md.mode(), MdMode::Source);
        let _ = md.click(switch, 1);

        // A format button pressed from the preview formats in place: the
        // rendering keeps up, so there is nothing to leave for.
        let bold = bold_button(&mut md);
        assert_eq!(md.click(bold, 1), MdOutcome::Edited);
        assert_eq!(md.mode(), MdMode::Preview);
        assert_eq!(md.text(), "x****");
    }

    // ── Naming the buttons ─────────────────────────────────────────────────

    /// The answer to "what does that button do": the bottom border says so
    /// while the pointer is over it.
    #[test]
    fn the_bottom_border_names_the_button_under_the_pointer() {
        let mut md = edit("");
        let bold = bold_button(&mut md);

        assert!(md.hover(bold, 1), "moving onto a button is a change");
        assert!(!md.hover(bold, 1), "and staying on it is not");
        let framed = draw(&mut md, 70, 8).join("\n");
        assert!(framed.contains("bold"), "{framed}");
        assert!(framed.contains(chord_label(MdAction::Bold)), "{framed}");

        // Off the toolbar it goes back to naming the view.
        assert!(md.hover(bold, 5));
        let framed = draw(&mut md, 70, 8).join("\n");
        assert!(framed.contains("markdown"), "{framed}");
        assert!(framed.contains(MODE_CHORD), "{framed}");
    }

    #[test]
    fn every_button_has_something_to_say_about_itself() {
        for group in GROUPS {
            for slot in group {
                assert!(!slot.hint(MdMode::Source).is_empty(), "{slot:?}");
                assert!(slot.width() > 2, "{slot:?}");
            }
        }
        // The switch says something different depending on which way it will
        // go, so hovering it is worth doing in either view.
        assert_ne!(
            Slot::ViewSwitch.hint(MdMode::Source),
            Slot::ViewSwitch.hint(MdMode::Preview)
        );
        // And it keeps its width when its label changes, so the buttons beside
        // it do not jump when you press it.
        assert_eq!(
            Slot::ViewSwitch.face(MdMode::Source).chars().count(),
            Slot::ViewSwitch.face(MdMode::Preview).chars().count()
        );
    }

    #[test]
    fn a_buttons_face_carries_the_style_it_stands_for() {
        assert!(MdAction::Bold.face().add_modifier.contains(Modifier::BOLD));
        assert!(
            MdAction::Italic
                .face()
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert!(
            MdAction::Strike
                .face()
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
        assert!(
            MdAction::Underline
                .face()
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert_eq!(MdAction::Code.face().fg, Some(C_CODE_FG));
        assert_eq!(MdAction::Heading.face().fg, Some(C_ACCENT));
        assert_eq!(MdAction::Quote.face().fg, Some(C_DIM));
        assert_eq!(MdAction::Bullet.face().fg, Some(C_WHITE));
    }

    /// The chip tells you which view you are in, and lifts under the pointer.
    #[test]
    fn a_chip_is_filled_for_the_current_view_and_lifted_under_the_pointer() {
        let mut md = edit("");
        // The switch is filled whichever view it is reporting: it is the
        // control that says what the box is.
        assert_eq!(md.chip_style(Slot::ViewSwitch, true).bg, Some(C_ACCENT));

        // The switch lifts under the pointer like anything else, swapping its
        // fill for the hover tint so it is plainly the thing you are about to
        // press.
        let _ = draw(&mut md, 70, 8);
        let switch = draw(&mut md, 70, 8)
            .remove(1)
            .chars()
            .position(|c| c == '⇄')
            .expect("a switch") as u16;
        md.hover(switch, 1);
        assert_eq!(
            md.chip_style(Slot::ViewSwitch, true).bg,
            Some(C_CHROME_HOVER)
        );
        md.hover(0, 0);

        let bold = Slot::Format(MdAction::Bold);
        assert_eq!(md.chip_style(bold, true).bg, Some(C_CHROME_BG));
        let _ = draw(&mut md, 70, 8);
        let at = bold_button(&mut md);
        md.hover(at, 1);
        assert_eq!(md.chip_style(bold, true).bg, Some(C_CHROME_HOVER));
        // An unfocused box greys its buttons rather than competing.
        assert_eq!(md.chip_style(bold, false).fg, Some(C_DIM));
    }

    // ── Underline, undo ────────────────────────────────────────────────────

    #[test]
    fn underline_writes_the_html_tag_markdown_does_not_have() {
        let mut md = edit("word");
        select_back(&mut md, 4);
        let _ = md.handle_key(&chord('u'));
        assert_eq!(md.text(), "<u>word</u>");
        select_back(&mut md, 11);
        md.apply(MdAction::Underline);
        assert_eq!(md.text(), "word");
    }

    /// `Ctrl-U` is underline now, so undo moved to the chord people press.
    #[test]
    fn ctrl_z_undoes_and_ctrl_shift_z_redoes() {
        let mut md = edit("");
        let _ = md.handle_key(&plain('a'));
        let _ = md.handle_key(&plain('b'));
        assert_eq!(md.text(), "ab");

        let _ = md.handle_key(&chord('z'));
        assert_ne!(md.text(), "ab", "nothing was undone");

        let redo = KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        let _ = md.handle_key(&redo);
        assert_eq!(md.text(), "ab");
    }

    // ── Tables and diagrams ────────────────────────────────────────────────

    /// The table button asks for a shape rather than making you count pipes,
    /// and writes a grid the preview can render.
    #[test]
    fn the_table_button_asks_for_a_shape_and_writes_a_grid() {
        let mut md = edit("");
        md.apply(MdAction::Table);
        assert!(md.is_modal());

        // Two columns, one row. Enter on the first field moves on rather than
        // finishing early.
        let _ = md.handle_key(&press(KeyCode::Backspace));
        let _ = md.handle_key(&plain('2'));
        let _ = md.handle_key(&press(KeyCode::Enter));
        assert!(md.is_modal(), "Enter on the first field moved on");
        let _ = md.handle_key(&press(KeyCode::Backspace));
        let _ = md.handle_key(&plain('1'));
        let _ = md.handle_key(&press(KeyCode::Enter));
        assert!(!md.is_modal());

        let lines = md.lines();
        assert_eq!(lines[0], "| Column 1 | Column 2 |", "{lines:?}");
        assert_eq!(lines[1], "|---|---|", "{lines:?}");
        assert_eq!(lines[2], "|   |   |", "{lines:?}");
    }

    /// Tab moves between the two numbers, the same as on the link popup.
    #[test]
    fn the_table_popup_switches_fields() {
        let mut md = edit("");
        md.apply(MdAction::Table);
        let _ = md.handle_key(&press(KeyCode::Tab));
        // The rows field has the keys now, so typing lands there.
        let _ = md.handle_key(&press(KeyCode::Backspace));
        let _ = md.handle_key(&plain('1'));
        let _ = md.handle_key(&press(KeyCode::Enter));
        assert!(!md.is_modal(), "Enter on the second field inserted");
        // Header, alignment row, one body row, and the newline it ends on.
        let lines = md.lines();
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert_eq!(lines[2], "|   |   |   |", "{lines:?}");

        // And it can be abandoned like any other popup.
        let mut md = edit("keep");
        md.apply(MdAction::Table);
        let framed = draw(&mut md, 60, 12).join("\n");
        assert!(framed.contains("Table"), "{framed}");
        assert!(framed.contains("Columns"), "{framed}");
        assert!(framed.contains("Rows"), "{framed}");
        let _ = md.handle_key(&press(KeyCode::Esc));
        assert_eq!(md.text(), "keep");
    }

    /// A field that is not a number, or a silly one, still gives a table.
    #[test]
    fn a_tables_size_is_clamped_rather_than_refused() {
        let mut table = TablePrompt {
            columns: LineEdit::new("banana", false),
            rows: LineEdit::new("900", false),
            on_columns: true,
        };
        assert_eq!(table.size(), (3, 12), "fell back, then clamped");
        table.columns = LineEdit::new("0", false);
        assert_eq!(table.size().0, 1, "a table has at least one column");
    }

    /// The diagram button writes a flowchart worth editing, not an empty
    /// fence: mermaid's syntax is the part people look up.
    #[test]
    fn the_diagram_button_writes_a_flowchart_to_edit() {
        let mut md = edit("");
        let _ = md.handle_key(&chord('g'));
        let text = md.text();
        assert!(text.starts_with("```mermaid"), "{text}");
        assert!(text.contains("flowchart TD"), "{text}");
        assert!(text.trim_end().ends_with("```"), "{text}");

        // And the preview draws it, rather than showing the source back.
        let _ = md.handle_key(&chord('p'));
        let rows = draw(&mut md, 60, 20);
        let body = rows[..rows.len() - 2].join("\n");
        assert!(!body.contains("flowchart TD"), "still source: {body}");
    }

    #[test]
    fn the_new_buttons_are_on_the_toolbar_with_the_rest() {
        let mut md = edit("");
        let bar = draw(&mut md, 70, 6).remove(1);
        assert!(bar.contains('▦'), "no table button: {bar}");
        assert!(bar.contains('◇'), "no diagram button: {bar}");
        assert_eq!(action_for(&chord('t')), Some(MdAction::Table));
        assert_eq!(action_for(&chord('g')), Some(MdAction::Diagram));
    }

    // ── The pointer ────────────────────────────────────────────────────────

    /// Clicking in the text puts the caret there, which is the one thing a
    /// mouse is for in an editor.
    #[test]
    fn clicking_in_the_text_moves_the_caret_there() {
        let mut md = edit("hello world\nsecond line");
        // Drawn once so the widget knows where its text landed and how far it
        // has scrolled; a click before any draw has nothing to go on.
        let _ = draw(&mut md, 40, 10);
        assert_eq!(md.area_mut().cursor(), (1, 11), "starts at the end");

        // Row 2 of the frame is the first line of text; column 4 is its `l`.
        assert_eq!(md.click(4, 2), MdOutcome::Edited);
        assert_eq!(md.area_mut().cursor(), (0, 3));

        // Typing lands where the click put it.
        let _ = md.handle_key(&plain('X'));
        assert_eq!(md.lines()[0], "helXlo world");
    }

    /// A click past the end of a line lands on its last character rather than
    /// somewhere else entirely.
    #[test]
    fn clicking_past_the_end_of_a_line_lands_on_its_end() {
        let mut md = edit("ab\nlonger line");
        let _ = draw(&mut md, 40, 10);
        assert_eq!(md.click(30, 2), MdOutcome::Edited);
        assert_eq!(md.area_mut().cursor(), (0, 2), "the end of `ab`");
    }

    /// The rendered view has no map back to a source position, so a press
    /// there is not a caret move.
    #[test]
    fn clicking_in_the_preview_does_not_move_the_caret() {
        let mut md = edit("hello").in_mode(MdMode::Preview);
        let _ = draw(&mut md, 40, 10);
        let before = md.area_mut().cursor();
        assert_eq!(md.click(4, 2), MdOutcome::Ignored);
        assert_eq!(md.area_mut().cursor(), before);
    }

    /// A box with no room to draw text has no caret to measure the scroll
    /// from, so a click in it is not a caret move.
    #[test]
    fn a_box_with_no_room_for_text_takes_no_clicks() {
        let mut md = edit("text");
        let _ = draw(&mut md, 40, 2);
        assert_eq!(md.click(1, 1), MdOutcome::Ignored);
    }

    #[test]
    fn display_rows_count_the_wrapping_a_scroll_has_to_clear() {
        let width = 10;
        let lines = vec![Line::from("12345678901234567890"), Line::from("")];
        assert_eq!(
            wrapped_rows(&lines, width),
            3,
            "two wrapped rows and a blank"
        );
        // A zero-width pane must not divide by zero on the way to nowhere.
        assert_eq!(wrapped_rows(&lines, 0), 21);
    }

    /// The prompt popup clamps its width to at least 12 columns, so in a
    /// narrower terminal `area.width - width` went below zero - a panic with
    /// `overflow-checks` on, which is how release builds ship.
    #[test]
    fn the_link_prompt_draws_in_a_ten_column_terminal() {
        let mut md = edit("hi");
        md.prompt = Some(Prompt::Link(LinkPrompt::new("hi")));
        let _ = draw(&mut md, 10, 8);
    }
}

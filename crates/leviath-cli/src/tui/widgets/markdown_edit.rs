//! The crate's long-form text editor: soft-wrapped, markdown-aware, with a
//! clickable formatting toolbar.
//!
//! Every multi-line box in the TUI (the new-run task, the response pane, a
//! stage's system and transition prompts) used a bare `ratatui-textarea` with
//! its default `WrapMode::None`, which scrolls sideways instead of wrapping.
//! A task longer than the pane is wide therefore ran off the edge, and the
//! text you had just typed was not on screen. This wraps at word boundaries,
//! falling back to splitting a word wider than the pane, so what you type
//! stays inside the box.
//!
//! On top of that it adds what a person expects from a text editor: chords for
//! bold / italic / strikethrough / code / links, and a row of buttons that do
//! the same thing with the mouse. Both go through [`MdAction`], so a binding
//! and its button cannot drift apart.
//!
//! Short fields (an agent's name, a numeric limit) stay on
//! [`LineEdit`](super::line_edit::LineEdit). This one is for prose.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};

use crate::tui::theme::{C_ACCENT, C_BORDER, C_DIM, C_MUTED, C_WHITE};

/// Each [`TOOLBAR`] action's chord, spelled the way this platform's users
/// write it. Paired with `TOOLBAR` by index, and a test holds the two the same
/// length.
///
/// macOS says Command, everybody else says Control. Only the *label* differs:
/// both modifiers are accepted everywhere (see [`action_for`]), because most
/// terminal emulators never forward Command to the program at all, and an
/// editor that listened only for the platform-correct one would be an editor
/// with no working chords on macOS Terminal.
///
/// `cfg` rather than a runtime branch, so there is no arm a platform's own
/// coverage run can never reach.
#[cfg(target_os = "macos")]
const CHORD_LABELS: [&str; 10] = ["⌘B", "⌘I", "⌘D", "⌘E", "⌘⇧E", "⌘K", "⌘H", "⌘L", "⌘O", "⌘."];
/// Each [`TOOLBAR`] action's chord, spelled for this platform.
#[cfg(not(target_os = "macos"))]
const CHORD_LABELS: [&str; 10] = [
    "ctrl-b",
    "ctrl-i",
    "ctrl-d",
    "ctrl-e",
    "ctrl-shift-e",
    "ctrl-k",
    "ctrl-h",
    "ctrl-l",
    "ctrl-o",
    "ctrl-.",
];

/// The chord that runs `action`, as a person reads it here.
pub(crate) fn chord_label(action: MdAction) -> &'static str {
    // `unwrap_or` rather than a guard: every action is in `TOOLBAR`, and a
    // branch that cannot be taken is a branch no test can cover.
    let i = TOOLBAR.iter().position(|a| *a == action).unwrap_or(0);
    CHORD_LABELS[i]
}

/// Every chord and what it does, ready to drop into a help overlay.
pub(crate) fn shortcut_help() -> Vec<(&'static str, &'static str)> {
    TOOLBAR
        .iter()
        .zip(CHORD_LABELS)
        .map(|(action, chord)| (chord, action.name()))
        .collect()
}

/// One formatting command. A chord and a toolbar button both resolve to this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MdAction {
    /// `**bold**`.
    Bold,
    /// `*italic*`.
    Italic,
    /// `~~struck through~~`.
    Strike,
    /// `` `inline code` ``.
    Code,
    /// A fenced code block.
    CodeBlock,
    /// `[text](url)`.
    Link,
    /// Cycle the line between `#`, `##`, `###` and no heading.
    Heading,
    /// Toggle a `- ` bullet on the line.
    Bullet,
    /// Toggle a `1. ` number on the line.
    Ordered,
    /// Toggle a `> ` quote on the line.
    Quote,
}

impl MdAction {
    /// The button's face. Short on purpose: ten of these share one row.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Bold => "B",
            Self::Italic => "I",
            Self::Strike => "S",
            Self::Code => "<>",
            Self::CodeBlock => "```",
            Self::Link => "[]",
            Self::Heading => "H",
            Self::Bullet => "•",
            Self::Ordered => "1.",
            Self::Quote => ">",
        }
    }

    /// What the button does, in words.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Bold => "bold",
            Self::Italic => "italic",
            Self::Strike => "strikethrough",
            Self::Code => "inline code",
            Self::CodeBlock => "code block",
            Self::Link => "link",
            Self::Heading => "heading",
            Self::Bullet => "bullet list",
            Self::Ordered => "numbered list",
            Self::Quote => "quote",
        }
    }
}

/// The toolbar, left to right.
pub(crate) const TOOLBAR: [MdAction; 10] = [
    MdAction::Bold,
    MdAction::Italic,
    MdAction::Strike,
    MdAction::Code,
    MdAction::CodeBlock,
    MdAction::Link,
    MdAction::Heading,
    MdAction::Bullet,
    MdAction::Ordered,
    MdAction::Quote,
];

/// Heading prefixes, in the order the heading key cycles them.
const HEADINGS: [&str; 3] = ["# ", "## ", "### "];

/// Resolve a key event to a formatting action.
///
/// Both `Ctrl` and `Super` count as the chord modifier (see [`CHORD_LABELS`]
/// for why both). Terminals that cannot report a chord simply never produce
/// one, and the toolbar is there for exactly that case.
///
/// Note what is deliberately load-bearing here: in a terminal without the
/// kitty keyboard protocol, `Ctrl-I` arrives as `Tab` and `Ctrl-H` as
/// `Backspace`, so those two chords only reach us where the terminal can tell
/// them apart. Nothing regresses when it cannot; Tab and Backspace keep their
/// meaning and the buttons still work.
pub(crate) fn action_for(key: &KeyEvent) -> Option<MdAction> {
    let mods = key.modifiers;
    if !mods.contains(KeyModifiers::CONTROL) && !mods.contains(KeyModifiers::SUPER) {
        return None;
    }
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    // A terminal may report Shift as the flag, as an uppercased char, or both.
    let shift = mods.contains(KeyModifiers::SHIFT) || c.is_uppercase();
    match (c.to_ascii_lowercase(), shift) {
        ('b', _) => Some(MdAction::Bold),
        ('i', _) => Some(MdAction::Italic),
        ('d', _) => Some(MdAction::Strike),
        ('e', false) => Some(MdAction::Code),
        // `Ctrl-E` is already "hand this to $EDITOR" in the blueprint editor's
        // prompt overlay, which takes its own keys before delegating here.
        // `Ctrl-T` is the alias that works in every box.
        ('t', _) => Some(MdAction::Code),
        ('e', true) => Some(MdAction::CodeBlock),
        ('k', _) => Some(MdAction::Link),
        ('h', _) => Some(MdAction::Heading),
        ('l', _) => Some(MdAction::Bullet),
        ('o', _) => Some(MdAction::Ordered),
        ('.', _) => Some(MdAction::Quote),
        _ => None,
    }
}

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

/// A soft-wrapping, markdown-aware multi-line editor.
#[derive(Debug, Clone)]
pub(crate) struct MarkdownEdit {
    area: TextArea<'static>,
    /// Where each toolbar button landed in the last frame, for hit testing.
    /// Empty until the first draw, and rewritten by every draw.
    buttons: Vec<(Rect, MdAction)>,
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
            buttons: Vec::new(),
        }
    }

    /// An editor over `text`, split on newlines.
    pub(crate) fn from_text(text: &str) -> Self {
        Self::new(text.lines().map(str::to_string).collect())
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

    /// Feed a key to the editor: a formatting chord if it is one, otherwise
    /// the textarea's own editing keys. Reports whether the text changed.
    ///
    /// Callers take their own keys (Enter to submit, Esc to close) *before*
    /// calling this, exactly as they did with the bare textarea.
    pub(crate) fn handle_key(&mut self, key: &KeyEvent) -> bool {
        match action_for(key) {
            Some(action) => {
                self.apply(action);
                true
            }
            None => self.area.input(ratatui_textarea::Input::from(*key)),
        }
    }

    /// Run a formatting action over the selection, or at the cursor when
    /// nothing is selected.
    pub(crate) fn apply(&mut self, action: MdAction) {
        match action {
            MdAction::Bold => self.wrap_inline("**"),
            MdAction::Italic => self.wrap_inline("*"),
            MdAction::Strike => self.wrap_inline("~~"),
            MdAction::Code => self.wrap_inline("`"),
            MdAction::CodeBlock => self.fence(),
            MdAction::Link => self.link(),
            MdAction::Heading => self.prefix_lines(next_heading),
            MdAction::Bullet => self.prefix_lines(|line| toggle_prefix(line, "- ")),
            MdAction::Ordered => self.prefix_lines(|line| toggle_prefix(line, "1. ")),
            MdAction::Quote => self.prefix_lines(|line| toggle_prefix(line, "> ")),
        }
    }

    /// Wrap the selection in `marker`, or unwrap it when it is already
    /// wrapped. With no selection, insert an empty pair and park the cursor
    /// between the halves.
    fn wrap_inline(&mut self, marker: &str) {
        let Some(text) = self.take_selection() else {
            self.area.insert_str(format!("{marker}{marker}"));
            self.move_back(marker.chars().count());
            return;
        };
        let unwrapped = text
            .strip_prefix(marker)
            .and_then(|inner| inner.strip_suffix(marker))
            .map(str::to_string);
        match unwrapped {
            Some(inner) => {
                self.area.insert_str(inner);
            }
            None => {
                self.area.insert_str(format!("{marker}{text}{marker}"));
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

    /// `[selection]()` with the cursor between the parentheses, ready for a
    /// URL, or `[]()` with the cursor in the brackets when nothing is
    /// selected.
    fn link(&mut self) {
        match self.take_selection() {
            Some(text) => {
                self.area.insert_str(format!("[{text}]()"));
                self.move_back(1);
            }
            None => {
                self.area.insert_str("[]()");
                self.move_back(3);
            }
        }
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

    /// Draw the framed editor: title, toolbar, wrapped text.
    ///
    /// The toolbar is dropped when the box is too short to spare a row for it,
    /// and trailing buttons are dropped when it is too narrow. A cramped pane
    /// loses buttons, never text.
    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect, view: &MdEditView) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(view.border))
            .title(view.title.clone());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        self.buttons.clear();
        let text_area = match inner.height >= 3 {
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

        self.area.set_style(Style::default().fg(C_WHITE));
        self.area.set_cursor_line_style(Style::default());
        self.area.set_cursor_style(match view.focused {
            true => Style::default().fg(Color::Black).bg(C_ACCENT),
            false => Style::default(),
        });
        frame.render_widget(&self.area, text_area);
    }

    /// The button row, and the registry that makes it clickable.
    fn draw_toolbar(&mut self, frame: &mut Frame, row: Rect, focused: bool) {
        let face = match focused {
            true => C_ACCENT,
            false => C_MUTED,
        };
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut x = row.x;
        for (i, action) in TOOLBAR.iter().enumerate() {
            let cell = action.label().chars().count() as u16 + 2;
            let sep = u16::from(i > 0);
            if x + sep + cell > row.x + row.width {
                break;
            }
            if sep > 0 {
                spans.push(Span::styled("│", Style::default().fg(C_BORDER)));
                x += 1;
            }
            spans.push(Span::styled(
                format!(" {} ", action.label()),
                Style::default().fg(face).add_modifier(emphasis(*action)),
            ));
            self.buttons.push((
                Rect {
                    x,
                    y: row.y,
                    width: cell,
                    height: 1,
                },
                *action,
            ));
            x += cell;
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), row);
    }

    /// Act on a click at `(column, row)`, reporting whether a button was under
    /// it. Coordinates are absolute, as crossterm reports them.
    pub(crate) fn click(&mut self, column: u16, row: u16) -> bool {
        let hit = self
            .buttons
            .iter()
            .find(|(rect, _)| rect.contains(Position::new(column, row)))
            .map(|(_, action)| *action);
        match hit {
            Some(action) => {
                self.apply(action);
                true
            }
            None => false,
        }
    }
}

/// The style a button's own face carries, so `B` reads as bold and `S` reads
/// as struck through without a legend next to it.
fn emphasis(action: MdAction) -> Modifier {
    match action {
        MdAction::Bold => Modifier::BOLD,
        MdAction::Italic => Modifier::ITALIC,
        MdAction::Strike => Modifier::CROSSED_OUT,
        _ => Modifier::empty(),
    }
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
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn chord(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn plain(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty())
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
        assert_eq!(TOOLBAR.len(), CHORD_LABELS.len());
        let help = shortcut_help();
        assert_eq!(help.len(), TOOLBAR.len());
        for (i, action) in TOOLBAR.iter().enumerate() {
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
            ('t', MdAction::Code),
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
        md.handle_key(&chord('b'));
        assert_eq!(md.text(), "****");
        // The cursor sits between the halves, so typing lands inside them.
        md.handle_key(&plain('h'));
        md.handle_key(&plain('i'));
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
            md.handle_key(&chord(c));
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

    #[test]
    fn a_link_keeps_the_selection_as_its_text_and_waits_for_the_url() {
        let mut md = edit("docs");
        select_back(&mut md, 4);
        md.apply(MdAction::Link);
        assert_eq!(md.text(), "[docs]()");
        md.handle_key(&plain('x'));
        assert_eq!(md.text(), "[docs](x)");
    }

    #[test]
    fn a_link_with_nothing_selected_waits_for_the_text_first() {
        let mut md = edit("");
        md.apply(MdAction::Link);
        assert_eq!(md.text(), "[]()");
        md.handle_key(&plain('a'));
        assert_eq!(md.text(), "[a]()");
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
        empty.handle_key(&plain('y'));
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
        md.handle_key(&plain('c'));
        md.handle_key(&KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()));
        md.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        md.handle_key(&plain('z'));
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
        let bar = draw(&mut md, 60, 6).remove(1);
        for action in TOOLBAR {
            let label = action.label();
            assert!(bar.contains(label), "{label} is missing from {bar}");
        }
    }

    /// A cramped pane drops buttons off the end rather than drawing outside
    /// its own rect, and a box with no room for a toolbar row keeps all of its
    /// height for text.
    #[test]
    fn a_cramped_box_drops_buttons_and_then_the_whole_toolbar() {
        let mut narrow = edit("");
        let bar = draw(&mut narrow, 16, 6).remove(1);
        assert!(bar.contains('B'), "{bar}");
        assert!(!bar.contains("```"), "{bar}");

        let mut short = edit("text");
        let first = draw(&mut short, 40, 3).remove(1);
        assert!(first.contains("text"), "{first}");
        assert!(!short.click(2, 1), "no toolbar means nothing to click");
    }

    #[test]
    fn clicking_a_button_runs_its_action_and_clicking_elsewhere_does_not() {
        let mut md = edit("");
        let bar = draw(&mut md, 60, 6).remove(1);
        // The first button is ` B `, drawn just inside the left border, so
        // column 2 of row 1 is the `B` itself.
        assert_eq!(bar.chars().nth(2), Some('B'), "{bar}");
        assert!(md.click(2, 1));
        assert_eq!(md.text(), "****");

        assert!(!md.click(2, 4), "the text area is not a button");
        assert!(!md.click(59, 1), "past the last button");
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
        assert!(toolbar.contains('B'), "{toolbar}");
        assert!(
            !buf.content().iter().any(|c| c.style().bg == Some(C_ACCENT)),
            "an unfocused box drew a cursor cell"
        );
    }

    #[test]
    fn the_placeholder_shows_only_while_the_box_is_empty() {
        let mut md = MarkdownEdit::default();
        md.set_placeholder("say something");
        let rows = draw(&mut md, 40, 6).join("\n");
        assert!(rows.contains("say something"), "{rows}");

        md.handle_key(&plain('x'));
        let rows = draw(&mut md, 40, 6).join("\n");
        assert!(!rows.contains("say something"), "{rows}");
    }

    #[test]
    fn a_buttons_face_carries_the_style_it_stands_for() {
        assert_eq!(emphasis(MdAction::Bold), Modifier::BOLD);
        assert_eq!(emphasis(MdAction::Italic), Modifier::ITALIC);
        assert_eq!(emphasis(MdAction::Strike), Modifier::CROSSED_OUT);
        assert_eq!(emphasis(MdAction::Link), Modifier::empty());
    }
}

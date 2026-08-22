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

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};

use crate::tui::theme::{
    C_ACCENT, C_BORDER, C_CHROME_BG, C_CHROME_HOVER, C_CODE_FG, C_DIM, C_MUTED, C_WHITE,
};

/// Which view a long-form box is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MdMode {
    /// The markdown you are writing, with a cursor.
    Source,
    /// How it will read, rendered.
    Preview,
}

impl MdMode {
    /// The word on the button.
    fn label(self) -> &'static str {
        match self {
            Self::Source => "Markdown",
            Self::Preview => "Preview",
        }
    }

    /// The other one, for the key that toggles.
    fn flipped(self) -> Self {
        match self {
            Self::Source => Self::Preview,
            Self::Preview => Self::Source,
        }
    }
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
    /// `<u>underlined</u>`: markdown has no underline, so this is the HTML tag
    /// every renderer takes for it, [`crate::render`] included.
    Underline,
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

/// Every action, in the order the toolbar and the help table list them.
pub(crate) const ACTIONS: [MdAction; 11] = [
    MdAction::Bold,
    MdAction::Italic,
    MdAction::Strike,
    MdAction::Underline,
    MdAction::Code,
    MdAction::CodeBlock,
    MdAction::Link,
    MdAction::Heading,
    MdAction::Bullet,
    MdAction::Ordered,
    MdAction::Quote,
];

/// Each [`ACTIONS`] entry's chord, spelled the way this platform's users write
/// it. Paired with `ACTIONS` by index, and a test holds the two the same
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
const CHORD_LABELS: [&str; 11] = [
    "⌘B", "⌘I", "⌘D", "⌘U", "⌘E", "⌘⇧E", "⌘K", "⌘H", "⌘L", "⌘O", "⌘.",
];
/// Each [`ACTIONS`] entry's chord, spelled for this platform.
#[cfg(not(target_os = "macos"))]
const CHORD_LABELS: [&str; 11] = [
    "ctrl-b",
    "ctrl-i",
    "ctrl-d",
    "ctrl-u",
    "ctrl-e",
    "ctrl-shift-e",
    "ctrl-k",
    "ctrl-h",
    "ctrl-l",
    "ctrl-o",
    "ctrl-.",
];

/// The chord that switches between the two views, spelled for this platform.
#[cfg(target_os = "macos")]
pub(crate) const MODE_CHORD: &str = "⌘P";
/// The chord that switches between the two views.
#[cfg(not(target_os = "macos"))]
pub(crate) const MODE_CHORD: &str = "ctrl-p";

/// The chord that runs `action`, as a person reads it here.
pub(crate) fn chord_label(action: MdAction) -> &'static str {
    // `unwrap_or` rather than a guard: every action is in `ACTIONS`, and a
    // branch that cannot be taken is a branch no test can cover.
    let i = ACTIONS.iter().position(|a| *a == action).unwrap_or(0);
    CHORD_LABELS[i]
}

/// Every chord and what it does, ready to drop into a help overlay.
pub(crate) fn shortcut_help() -> Vec<(&'static str, &'static str)> {
    ACTIONS
        .iter()
        .zip(CHORD_LABELS)
        .map(|(action, chord)| (chord, action.name()))
        .collect()
}

impl MdAction {
    /// The button's face. Short on purpose: eleven of these share one row.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Bold => "B",
            Self::Italic => "i",
            Self::Strike => "S",
            Self::Underline => "U",
            Self::Code => "<>",
            Self::CodeBlock => "```",
            Self::Link => "[]",
            Self::Heading => "H",
            Self::Bullet => "•",
            Self::Ordered => "1.",
            Self::Quote => ">",
        }
    }

    /// What the button does, in words. Shown on the bottom border while the
    /// pointer is over it, and in the help overlay.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Bold => "bold",
            Self::Italic => "italic",
            Self::Strike => "strikethrough",
            Self::Underline => "underline",
            Self::Code => "inline code",
            Self::CodeBlock => "code block",
            Self::Link => "link",
            Self::Heading => "heading",
            Self::Bullet => "bullet list",
            Self::Ordered => "numbered list",
            Self::Quote => "quote",
        }
    }

    /// The style the button's own face wears, which is the style the action
    /// applies. `B` is bold, `S` is struck through, `<>` is the colour code
    /// renders in: the face *is* the label, so there is nothing to look up.
    fn face(self) -> Style {
        let plain = Style::default().fg(C_WHITE);
        match self {
            Self::Bold => plain.add_modifier(Modifier::BOLD),
            Self::Italic => plain.add_modifier(Modifier::ITALIC),
            Self::Strike => plain.add_modifier(Modifier::CROSSED_OUT),
            Self::Underline => plain.add_modifier(Modifier::UNDERLINED),
            Self::Code | Self::CodeBlock => Style::default().fg(C_CODE_FG),
            Self::Link => Style::default()
                .fg(C_ACCENT)
                .add_modifier(Modifier::UNDERLINED),
            Self::Heading => Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            Self::Bullet | Self::Ordered => plain,
            Self::Quote => Style::default().fg(C_DIM),
        }
    }
}

/// One cell of the toolbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// Half of the view switch. The one you are in is filled.
    Mode(MdMode),
    /// A formatting button.
    Format(MdAction),
}

impl Slot {
    fn label(self) -> &'static str {
        match self {
            Self::Mode(mode) => mode.label(),
            Self::Format(action) => action.label(),
        }
    }

    /// Columns this cell occupies: the label plus a space either side.
    fn width(self) -> u16 {
        self.label().chars().count() as u16 + 2
    }

    /// What the bottom border says while the pointer is over it.
    fn hint(self) -> String {
        match self {
            Self::Mode(MdMode::Source) => format!("the markdown you write · {MODE_CHORD}"),
            Self::Mode(MdMode::Preview) => format!("how it will read · {MODE_CHORD}"),
            Self::Format(action) => format!("{} · {}", action.name(), chord_label(action)),
        }
    }
}

/// The toolbar, in groups. A group is drawn whole or not at all, so a cramped
/// box loses a coherent set of buttons from the right rather than half of one.
/// The view switch leads, because it is the control that has to survive being
/// narrow.
const GROUPS: [&[Slot]; 4] = [
    &[Slot::Mode(MdMode::Source), Slot::Mode(MdMode::Preview)],
    &[
        Slot::Format(MdAction::Bold),
        Slot::Format(MdAction::Italic),
        Slot::Format(MdAction::Strike),
        Slot::Format(MdAction::Underline),
    ],
    &[
        Slot::Format(MdAction::Code),
        Slot::Format(MdAction::CodeBlock),
        Slot::Format(MdAction::Link),
    ],
    &[
        Slot::Format(MdAction::Heading),
        Slot::Format(MdAction::Bullet),
        Slot::Format(MdAction::Ordered),
        Slot::Format(MdAction::Quote),
    ],
];

/// Heading prefixes, in the order the heading key cycles them.
const HEADINGS: [&str; 3] = ["# ", "## ", "### "];

/// What a key or a click did, for a host that has to remember the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) enum MdOutcome {
    /// Nothing here answered to it.
    Ignored,
    /// The buffer, the cursor, or the preview's scroll moved.
    Edited,
    /// The view switched. The host stores this as the user's preference.
    ModeChanged(MdMode),
}

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
    let (c, shift) = chord_char(key)?;
    match (c, shift) {
        ('b', _) => Some(MdAction::Bold),
        ('i', _) => Some(MdAction::Italic),
        ('d', _) => Some(MdAction::Strike),
        ('u', _) => Some(MdAction::Underline),
        ('e', false) => Some(MdAction::Code),
        ('e', true) => Some(MdAction::CodeBlock),
        ('k', _) => Some(MdAction::Link),
        ('h', _) => Some(MdAction::Heading),
        ('l', _) => Some(MdAction::Bullet),
        ('o', _) => Some(MdAction::Ordered),
        ('.', _) => Some(MdAction::Quote),
        _ => None,
    }
}

/// The character of a chord and whether Shift was on it, or `None` when the
/// key is not a chord at all.
fn chord_char(key: &KeyEvent) -> Option<(char, bool)> {
    let mods = key.modifiers;
    if !mods.contains(KeyModifiers::CONTROL) && !mods.contains(KeyModifiers::SUPER) {
        return None;
    }
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    // A terminal may report Shift as the flag, as an uppercased char, or both.
    let shift = mods.contains(KeyModifiers::SHIFT) || c.is_uppercase();
    Some((c.to_ascii_lowercase(), shift))
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

    /// Feed a key to the editor.
    ///
    /// Callers take their own keys (Enter to submit, Esc to close) *before*
    /// calling this, exactly as they did with the bare textarea.
    ///
    /// In `Preview` only the arrows and the page keys stay in the preview,
    /// scrolling it. Anything else that would edit drops back to `Markdown`
    /// first and then does what it was going to do, because a key that appears
    /// to do nothing reads as a broken editor rather than as a mode.
    pub(crate) fn handle_key(&mut self, key: &KeyEvent) -> MdOutcome {
        if let Some(('p', _)) = chord_char(key) {
            return self.set_mode(self.mode.flipped());
        }
        if self.mode == MdMode::Preview {
            if let Some(outcome) = self.scroll_preview(key) {
                return outcome;
            }
            let changed = self.set_mode(MdMode::Source);
            let _ = self.apply_key(key);
            return changed;
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

    /// The keys that move the preview rather than leaving it.
    fn scroll_preview(&mut self, key: &KeyEvent) -> Option<MdOutcome> {
        let by: i16 = match key.code {
            KeyCode::Up => -1,
            KeyCode::Down => 1,
            KeyCode::PageUp => -10,
            KeyCode::PageDown => 10,
            KeyCode::Home => i16::MIN,
            KeyCode::End => i16::MAX,
            _ => return None,
        };
        // The draw clamps to the document's last row; this only has to stay
        // inside the type.
        self.preview_scroll = self.preview_scroll.saturating_add_signed(by);
        Some(MdOutcome::Edited)
    }

    /// Switch views, reporting it so the host can remember the choice.
    fn set_mode(&mut self, mode: MdMode) -> MdOutcome {
        self.mode = mode;
        self.preview_scroll = 0;
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
        }
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
            MdMode::Preview => self.draw_preview(frame, body),
            MdMode::Source => {
                self.area.set_style(Style::default().fg(C_WHITE));
                self.area.set_cursor_line_style(Style::default());
                self.area.set_cursor_style(match view.focused {
                    true => Style::default().fg(Color::Black).bg(C_ACCENT),
                    false => Style::default(),
                });
                frame.render_widget(&self.area, body);
            }
        }
    }

    /// The bottom border: what the pointer is over, or which view this is.
    ///
    /// This is where "what does that button do" is answered. A toolbar of
    /// glyphs with nowhere to read their names is a toolbar you have to guess
    /// at, and the border costs no rows.
    fn footer(&self) -> Line<'static> {
        let text = match self.hovered {
            Some(slot) => slot.hint(),
            None => match self.mode {
                MdMode::Source => format!("markdown · {MODE_CHORD} previews it"),
                MdMode::Preview => format!("preview · {MODE_CHORD} edits it"),
            },
        };
        Line::from(Span::styled(
            format!(" {text} "),
            Style::default().fg(C_MUTED),
        ))
    }

    /// The rendered view, through the same renderer the run view uses for an
    /// agent's output. Scrolling is clamped to the document's last row,
    /// counted in *display* rows, so the bottom is genuinely reachable.
    fn draw_preview(&mut self, frame: &mut Frame, body: Rect) {
        let text = crate::render::markdown_to_text(&self.text(), body.width);
        let rows = wrapped_rows(&text.lines, body.width);
        let max = rows.saturating_sub(body.height as usize);
        self.preview_scroll = self.preview_scroll.min(max.min(u16::MAX as usize) as u16);
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((self.preview_scroll, 0)),
            body,
        );
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
                    format!(" {} ", slot.label()),
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
        if slot == Slot::Mode(self.mode) {
            return Style::default()
                .fg(Color::Black)
                .bg(C_ACCENT)
                .add_modifier(Modifier::BOLD);
        }
        let base = match slot {
            Slot::Mode(_) => Style::default().fg(C_MUTED),
            Slot::Format(action) if focused => action.face(),
            // An unfocused box still shows its toolbar, so you can see it is
            // there, but greyed rather than competing with the box that has
            // the keys.
            Slot::Format(_) => Style::default().fg(C_DIM),
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

    /// Act on a click at `(column, row)`. Coordinates are absolute, as
    /// crossterm reports them.
    ///
    /// A formatting button pressed while the preview is up switches back to
    /// `Markdown` first, on the same reasoning as the keys: the press has to
    /// do something you can see.
    pub(crate) fn click(&mut self, column: u16, row: u16) -> MdOutcome {
        match self.slot_at(column, row) {
            Some(Slot::Mode(mode)) => self.set_mode(mode),
            Some(Slot::Format(action)) => {
                let left_preview = self.mode == MdMode::Preview;
                if left_preview {
                    self.mode = MdMode::Source;
                    self.preview_scroll = 0;
                }
                self.apply(action);
                match left_preview {
                    true => MdOutcome::ModeChanged(MdMode::Source),
                    false => MdOutcome::Edited,
                }
            }
            None => MdOutcome::Ignored,
        }
    }
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

    #[test]
    fn a_link_keeps_the_selection_as_its_text_and_waits_for_the_url() {
        let mut md = edit("docs");
        select_back(&mut md, 4);
        md.apply(MdAction::Link);
        assert_eq!(md.text(), "[docs]()");
        let _ = md.handle_key(&plain('x'));
        assert_eq!(md.text(), "[docs](x)");
    }

    #[test]
    fn a_link_with_nothing_selected_waits_for_the_text_first() {
        let mut md = edit("");
        md.apply(MdAction::Link);
        assert_eq!(md.text(), "[]()");
        let _ = md.handle_key(&plain('a'));
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
        for mode in [MdMode::Source, MdMode::Preview] {
            let label = mode.label();
            assert!(bar.contains(label), "{label} is missing from {bar}");
        }
    }

    /// The whole bar needs 60 columns. Every box that draws one is wider than
    /// that on a normal terminal, and this is the number to check against when
    /// one of them turns out not to be.
    #[test]
    fn the_full_toolbar_fits_in_sixty_columns() {
        let separators = GROUPS.len() as u16 - 1;
        let buttons: u16 = GROUPS
            .iter()
            .flat_map(|group| group.iter())
            .map(|slot| slot.width())
            .sum();
        assert_eq!(buttons + separators, 60);
    }

    /// A cramped pane drops buttons off the end rather than drawing outside
    /// its own rect, and a box with no room for a toolbar row keeps all of its
    /// height for text.
    #[test]
    fn a_cramped_box_drops_buttons_and_then_the_whole_toolbar() {
        let mut narrow = edit("");
        let bar = draw(&mut narrow, 24, 6).remove(1);
        assert!(bar.contains("Markdown"), "the view switch survives: {bar}");
        assert!(!bar.contains("```"), "the later groups do not: {bar}");

        let mut short = edit("text");
        let first = draw(&mut short, 40, 3).remove(1);
        assert!(first.contains("text"), "{first}");
        assert_eq!(
            short.click(2, 1),
            MdOutcome::Ignored,
            "no toolbar means nothing to click"
        );
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

        assert_eq!(
            md.click(bold, 4),
            MdOutcome::Ignored,
            "the text area is not a button"
        );
        assert_eq!(md.click(69, 1), MdOutcome::Ignored, "past the last button");
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
        assert!(toolbar.contains("Markdown"), "{toolbar}");
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
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(!text.contains("**loud**"), "still raw: {text}");
        assert!(text.contains("loud"), "{text}");

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

    /// A key that would edit must not silently do nothing just because the
    /// preview is up: it drops back to the markdown and then does it.
    #[test]
    fn typing_in_the_preview_returns_to_the_markdown_and_types() {
        let mut md = edit("ab").in_mode(MdMode::Preview);
        assert_eq!(
            md.handle_key(&plain('c')),
            MdOutcome::ModeChanged(MdMode::Source)
        );
        assert_eq!(md.mode(), MdMode::Source);
        assert_eq!(md.text(), "abc");

        // A formatting chord does the same.
        let mut md = edit("").in_mode(MdMode::Preview);
        let _ = md.handle_key(&chord('b'));
        assert_eq!(md.mode(), MdMode::Source);
        assert_eq!(md.text(), "****");
    }

    /// The arrows and page keys are the exception: they move the preview
    /// rather than leaving it, and the draw clamps them to the document.
    #[test]
    fn the_arrows_scroll_the_preview_and_stay_in_it() {
        let long: String = (0..40).map(|i| format!("line {i}\n")).collect();
        let mut md = edit(&long).in_mode(MdMode::Preview);

        for code in [KeyCode::Down, KeyCode::PageDown, KeyCode::End] {
            let key = KeyEvent::new(code, KeyModifiers::empty());
            assert_eq!(md.handle_key(&key), MdOutcome::Edited, "{code:?}");
            assert_eq!(md.mode(), MdMode::Preview, "{code:?}");
        }
        // Drawing clamps to the last row, so `End` cannot leave the document
        // behind: scrolling back up from there reaches the top again.
        let _ = draw(&mut md, 40, 10);
        for code in [KeyCode::Up, KeyCode::PageUp, KeyCode::Home] {
            let key = KeyEvent::new(code, KeyModifiers::empty());
            assert_eq!(md.handle_key(&key), MdOutcome::Edited, "{code:?}");
        }
        let rows = draw(&mut md, 40, 10).join("\n");
        assert!(rows.contains("line 0"), "back at the top: {rows}");
    }

    #[test]
    fn the_view_switch_is_clickable_and_a_format_button_leaves_the_preview() {
        let mut md = edit("x");
        let bar = draw(&mut md, 70, 8).remove(1);
        let preview = bar.find("Preview").expect("a Preview chip") as u16;
        assert_eq!(
            md.click(preview, 1),
            MdOutcome::ModeChanged(MdMode::Preview)
        );
        assert_eq!(md.mode(), MdMode::Preview);

        // A format button pressed from the preview has to do something you can
        // see, so it comes back to the markdown first.
        let bold = bold_button(&mut md);
        assert_eq!(md.click(bold, 1), MdOutcome::ModeChanged(MdMode::Source));
        assert_eq!(md.mode(), MdMode::Source);
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
                assert!(!slot.hint().is_empty(), "{slot:?}");
                assert!(slot.width() > 2, "{slot:?}");
            }
        }
        // The two halves of the switch describe themselves differently, so
        // hovering either one is worth doing.
        assert_ne!(
            Slot::Mode(MdMode::Source).hint(),
            Slot::Mode(MdMode::Preview).hint()
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
        assert_eq!(
            md.chip_style(Slot::Mode(MdMode::Source), true).bg,
            Some(C_ACCENT)
        );
        assert_eq!(
            md.chip_style(Slot::Mode(MdMode::Preview), true).bg,
            Some(C_CHROME_BG)
        );

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
}

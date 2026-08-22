//! What a long-form box can do, and how each of those is asked for.
//!
//! The vocabulary lives apart from the widget: a chord, a button and a help
//! entry all resolve to the same [`MdAction`], and keeping the three tables
//! that spell them in one file is what stops a binding drifting from its
//! button.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};

use crate::tui::theme::{C_ACCENT, C_BORDER, C_CODE_FG, C_DIM, C_WHITE};

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
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Source => "Edit",
            Self::Preview => "Preview",
        }
    }

    /// The other one, for the key that toggles.
    pub(super) fn flipped(self) -> Self {
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
    /// A table, sized by a popup and written as a markdown grid.
    Table,
    /// A ```mermaid``` flowchart, which the preview draws as a diagram.
    Diagram,
}

/// Every action, in the order the toolbar and the help table list them.
pub(crate) const ACTIONS: [MdAction; 13] = [
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
    MdAction::Table,
    MdAction::Diagram,
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
pub(super) const CHORD_LABELS: [&str; 13] = [
    "⌘B", "⌘I", "⌘D", "⌘U", "⌘E", "⌘⇧E", "⌘K", "⌘H", "⌘L", "⌘O", "⌘.", "⌘T", "⌘G",
];
/// Each [`ACTIONS`] entry's chord, spelled for this platform.
#[cfg(not(target_os = "macos"))]
pub(super) const CHORD_LABELS: [&str; 13] = [
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
    "ctrl-t",
    "ctrl-g",
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
            Self::Table => "▦",
            Self::Diagram => "◇",
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
            Self::Table => "table",
            Self::Diagram => "mermaid diagram",
        }
    }

    /// The style the button's own face wears, which is the style the action
    /// applies. `B` is bold, `S` is struck through, `<>` is the colour code
    /// renders in: the face *is* the label, so there is nothing to look up.
    pub(super) fn face(self) -> Style {
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
            // The two that draw something: the colours the preview draws them
            // in, so the button looks like what it makes.
            Self::Table => Style::default().fg(C_BORDER),
            Self::Diagram => Style::default().fg(C_ACCENT),
        }
    }
}

/// One cell of the toolbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Slot {
    /// Half of the view switch. The one you are in is filled.
    Mode(MdMode),
    /// A formatting button.
    Format(MdAction),
}

impl Slot {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Mode(mode) => mode.label(),
            Self::Format(action) => action.label(),
        }
    }

    /// Columns this cell occupies: the label plus a space either side.
    pub(super) fn width(self) -> u16 {
        self.label().chars().count() as u16 + 2
    }

    /// What the bottom border says while the pointer is over it.
    pub(super) fn hint(self) -> String {
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
pub(super) const GROUPS: [&[Slot]; 5] = [
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
        Slot::Format(MdAction::Table),
        Slot::Format(MdAction::Diagram),
    ],
    &[
        Slot::Format(MdAction::Heading),
        Slot::Format(MdAction::Bullet),
        Slot::Format(MdAction::Ordered),
        Slot::Format(MdAction::Quote),
    ],
];

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
        ('t', _) => Some(MdAction::Table),
        ('g', _) => Some(MdAction::Diagram),
        _ => None,
    }
}

/// The character of a chord and whether Shift was on it, or `None` when the
/// key is not a chord at all.
pub(super) fn chord_char(key: &KeyEvent) -> Option<(char, bool)> {
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

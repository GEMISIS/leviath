//! The two popups a long-form box can put up: a link, and a table.
//!
//! Both are a pair of fields and a confirmation, which is why they share a
//! shape. They live apart from the widget because what they collect is the
//! *content* of an insertion, and the widget only cares that something came
//! back.

use super::super::line_edit::LineEdit;

/// Which half of the link popup has the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LinkField {
    /// The caption, the part that is read.
    Text,
    /// The destination.
    Url,
}

impl LinkField {
    pub(super) fn flipped(self) -> Self {
        match self {
            Self::Text => Self::Url,
            Self::Url => Self::Text,
        }
    }
}

/// The link popup: a caption and a URL, and which of them you are typing in.
#[derive(Debug, Clone)]
pub(super) struct LinkPrompt {
    pub(super) text: LineEdit,
    pub(super) url: LineEdit,
    pub(super) focus: LinkField,
}

impl LinkPrompt {
    /// Open on `text`, which is whatever was selected. With a caption already
    /// filled in there is only the URL left to type, so start there.
    pub(super) fn new(text: &str) -> Self {
        Self {
            text: LineEdit::new(text, false),
            url: LineEdit::new(String::new(), false),
            focus: match text.is_empty() {
                true => LinkField::Text,
                false => LinkField::Url,
            },
        }
    }

    pub(super) fn focused_mut(&mut self) -> &mut LineEdit {
        match self.focus {
            LinkField::Text => &mut self.text,
            LinkField::Url => &mut self.url,
        }
    }
}

/// The table popup: how many columns and how many body rows.
#[derive(Debug, Clone)]
pub(super) struct TablePrompt {
    pub(super) columns: LineEdit,
    pub(super) rows: LineEdit,
    /// True while the columns field has the keys.
    pub(super) on_columns: bool,
}

impl Default for TablePrompt {
    /// Three by three, which is the table somebody who has not thought about
    /// it yet wants, and small enough to fit any pane.
    fn default() -> Self {
        Self {
            columns: LineEdit::new("3", false),
            rows: LineEdit::new("3", false),
            on_columns: true,
        }
    }
}

impl TablePrompt {
    pub(super) fn focused_mut(&mut self) -> &mut LineEdit {
        match self.on_columns {
            true => &mut self.columns,
            false => &mut self.rows,
        }
    }

    /// The two numbers, clamped to something that can be drawn. A field that
    /// is not a number at all falls back rather than refusing: the popup is
    /// for choosing a size, not for validating arithmetic.
    pub(super) fn size(&self) -> (usize, usize) {
        let read = |edit: &LineEdit, fallback: usize| {
            edit.value()
                .trim()
                .parse::<usize>()
                .unwrap_or(fallback)
                .clamp(1, 12)
        };
        (read(&self.columns, 3), read(&self.rows, 3))
    }

    /// The markdown grid, with a header row and the alignment row under it.
    pub(super) fn markdown(&self) -> String {
        let (columns, rows) = self.size();
        let header: Vec<String> = (1..=columns).map(|i| format!("Column {i}")).collect();
        let mut out = format!("| {} |\n", header.join(" | "));
        out.push_str(&format!("|{}|\n", vec!["---"; columns].join("|")));
        for _ in 0..rows {
            out.push_str(&format!("|{}|\n", vec!["   "; columns].join("|")));
        }
        out
    }
}

/// Whichever popup a long-form box has up.
#[derive(Debug, Clone)]
pub(super) enum Prompt {
    /// Ask for a caption and a URL.
    Link(LinkPrompt),
    /// Ask for a table's shape.
    Table(TablePrompt),
}

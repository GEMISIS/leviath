//! A stage's prompts, edited full screen: what the stage is told to do
//! (`system_prompt`) and how it decides where to go next
//! (`transition_prompt`). `Tab` moves between the two, `Ctrl-S` or `Esc`
//! applies and closes, `Ctrl-Q` closes without applying, and `F2` hands
//! the focused text to `$EDITOR`: the dashboard leaves the terminal, the
//! editor runs, and the text comes back into the box.
//!
//! `$EDITOR` was on `Ctrl-E` until the boxes became
//! [`MarkdownEdit`](crate::tui::widgets::markdown_edit::MarkdownEdit)s, where
//! `Ctrl-E` is inline code in every other long-form box in the dashboard. One
//! chord meaning two things depending on which box you are in is worse than
//! moving the rarer of the two, and a function key is the one thing here no
//! terminal mangles and no text field can swallow.

use std::path::PathBuf;

use crate::tui::widgets::markdown_edit::{MarkdownEdit, MdMode, MdOutcome};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::super::state::Dashboard;
use super::editor::Overlay;
use crate::blueprint_edit::StageText;

/// Which of the two prompts has the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::commands::dashboard) enum PromptFocus {
    /// `system_prompt`.
    System,
    /// `transition_prompt`.
    Transition,
}

/// The overlay's state.
#[derive(Debug, Clone)]
pub(in crate::commands::dashboard) struct PromptsEditor {
    /// The stage whose prompts these are.
    pub(in crate::commands::dashboard) stage: String,
    pub(in crate::commands::dashboard) system: MarkdownEdit,
    pub(in crate::commands::dashboard) transition: MarkdownEdit,
    pub(in crate::commands::dashboard) focus: PromptFocus,
}

impl PromptsEditor {
    fn new(stage: &str, system: &str, transition: &str, mode: MdMode) -> Self {
        Self {
            stage: stage.to_string(),
            system: MarkdownEdit::from_text(system).in_mode(mode),
            transition: MarkdownEdit::from_text(transition).in_mode(mode),
            focus: PromptFocus::System,
        }
    }

    /// The focused box.
    fn focused_mut(&mut self) -> &mut MarkdownEdit {
        match self.focus {
            PromptFocus::System => &mut self.system,
            PromptFocus::Transition => &mut self.transition,
        }
    }

    /// The text of a box, without the trailing newline a textarea adds.
    fn text_of(area: &MarkdownEdit) -> String {
        let mut text = area.text();
        // One trailing newline is how a multi-line prompt ends in the file;
        // a single line has none.
        if text.contains('\n') && !text.ends_with('\n') {
            text.push('\n');
        }
        text
    }
}

/// Text handed to `$EDITOR`, and where it goes when it comes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::commands::dashboard) struct ExternalEdit {
    /// The temp file the editor opens.
    pub(in crate::commands::dashboard) path: PathBuf,
    /// Which prompt of the open overlay takes the text.
    pub(in crate::commands::dashboard) target: PromptFocus,
}

impl Dashboard {
    /// Open the prompts overlay on the panel's stage.
    pub(super) fn editor_open_prompts(&mut self) {
        let stage = self.editor().panel_stage().expect("a stage field");
        let Some(view) = self.editor().doc.stage(&stage) else {
            return;
        };
        let mode = self.md_mode();
        self.editor().overlay = Some(Overlay::Prompts(Box::new(PromptsEditor::new(
            &stage,
            &view.system_prompt,
            &view.transition_prompt,
            mode,
        ))));
    }

    /// Keys on the prompts overlay.
    pub(super) fn editor_prompts_key(&mut self, key: &KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(Overlay::Prompts(prompts)) = self.editor().overlay.as_mut() else {
            return;
        };
        // A prompt box's own popup outranks the overlay's keys: Esc while it
        // is up closes the popup, not the whole overlay.
        if prompts.focused_mut().is_modal() {
            let outcome = prompts.focused_mut().handle_key(key);
            self.remember_md_mode(outcome);
            return;
        }
        match (key.code, ctrl) {
            (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
                prompts.focus = match prompts.focus {
                    PromptFocus::System => PromptFocus::Transition,
                    PromptFocus::Transition => PromptFocus::System,
                };
            }
            (KeyCode::Esc, _) | (KeyCode::Char('s'), true) => self.editor_apply_prompts(),
            (KeyCode::Char('q'), true) => self.editor().overlay = None,
            (KeyCode::F(2), _) => self.editor_request_external_edit(),
            // F1 rather than `?`, which is a question mark inside a prompt.
            (KeyCode::F(1), _) => self.show_help = true,
            _ => {
                let outcome = prompts.focused_mut().handle_key(key);
                self.remember_md_mode(outcome);
            }
        }
    }

    /// A press on one of the two prompt boxes' formatting toolbars.
    ///
    /// Lives here rather than in `click.rs` because [`Overlay`] and
    /// [`PromptFocus`] are private to the agents screen, and reaching into an
    /// overlay from outside it is how the two would drift.
    pub(in crate::commands::dashboard) fn prompts_toolbar_click(
        &mut self,
        column: u16,
        row: u16,
    ) -> bool {
        let Some(screen) = self.agent_builder.as_deref_mut() else {
            return false;
        };
        let Some(editor) = screen.editor.as_mut() else {
            return false;
        };
        let Some(Overlay::Prompts(prompts)) = editor.overlay.as_mut() else {
            return false;
        };
        let (outcome, focus) = match prompts.system.click(column, row) {
            MdOutcome::Ignored => (
                prompts.transition.click(column, row),
                PromptFocus::Transition,
            ),
            hit => (hit, PromptFocus::System),
        };
        if outcome == MdOutcome::Ignored {
            return false;
        }
        prompts.focus = focus;
        self.remember_md_mode(outcome)
    }

    /// The pointer moving over either prompt box's toolbar.
    pub(in crate::commands::dashboard) fn prompts_toolbar_hover(&mut self, column: u16, row: u16) {
        let Some(screen) = self.agent_builder.as_deref_mut() else {
            return;
        };
        let Some(editor) = screen.editor.as_mut() else {
            return;
        };
        let Some(Overlay::Prompts(prompts)) = editor.overlay.as_mut() else {
            return;
        };
        prompts.system.hover(column, row);
        prompts.transition.hover(column, row);
    }

    /// Write both prompts back and close the overlay.
    pub(super) fn editor_apply_prompts(&mut self) {
        let Some(Overlay::Prompts(prompts)) = self.editor().overlay.take() else {
            return;
        };
        let stage = prompts.stage.clone();
        let system = PromptsEditor::text_of(&prompts.system);
        let transition = PromptsEditor::text_of(&prompts.transition);
        self.editor_mutate(|d| {
            d.set_stage_text(&stage, StageText::SystemPrompt, &system)
                .and_then(|()| d.set_stage_text(&stage, StageText::TransitionPrompt, &transition))
        });
    }

    /// F2: write the focused text to a temp file and ask the loop to run
    /// `$EDITOR` on it.
    pub(super) fn editor_request_external_edit(&mut self) {
        let Some(Overlay::Prompts(prompts)) = self.editor().overlay.as_mut() else {
            return;
        };
        let target = prompts.focus;
        let text = PromptsEditor::text_of(match target {
            PromptFocus::System => &prompts.system,
            PromptFocus::Transition => &prompts.transition,
        });
        match self.write_handoff_file(&text) {
            Ok(path) => self.pending_external_edit = Some(ExternalEdit { path, target }),
            Err(e) => {
                self.editor().message = Some(format!("Could not hand the prompt to an editor: {e}"))
            }
        }
    }

    /// Write `text` to a fresh, owner-only file in this dashboard's private
    /// scratch directory and return its path.
    ///
    /// The directory is made on the first handoff (`tempdir_in` under
    /// `external_edit_dir`, which is the system temp directory in
    /// production) and the file's name is random, so neither is a path anyone
    /// else could have prepared. The stage's name is not part of it.
    fn write_handoff_file(&mut self, text: &str) -> std::io::Result<std::path::PathBuf> {
        use std::io::Write as _;
        if self.external_edit_scratch.is_none() {
            // Chained rather than two `?`s: the parent is the system temp
            // directory in production, and the one failure a test can
            // arrange (a file where the directory should be) fails the
            // first step and short-circuits the second.
            let dir = &self.external_edit_dir;
            let scratch = std::fs::create_dir_all(dir)
                .and_then(|()| {
                    tempfile::Builder::new()
                        .prefix("leviath-dash-")
                        .tempdir_in(dir)
                })
                // `tempdir_in` makes the directory at the umask; the files in
                // it are owner-only, and so is the directory that lists them.
                .and_then(|scratch| {
                    leviath_sys::secure_dir_perms(scratch.path()).map(|()| scratch)
                })?;
            self.external_edit_scratch = Some(scratch);
        }
        let dir = self
            .external_edit_scratch
            .as_ref()
            .expect("made above")
            .path();
        // A fresh file created for writing has no reachable failure past its
        // creation, so the steps are chained the same way.
        tempfile::Builder::new()
            .prefix("prompt-")
            .suffix(".md")
            .tempfile_in(dir)
            .and_then(|mut file| file.write_all(text.as_bytes()).map(|()| file))
            .and_then(|file| file.keep().map(|(_, path)| path).map_err(|e| e.error))
    }

    /// Whether a prompt is waiting for `$EDITOR`: the loop hands the terminal
    /// over when this is true.
    pub(in crate::commands::dashboard) fn has_external_edit(&self) -> bool {
        self.pending_external_edit.is_some()
    }

    /// The file waiting for `$EDITOR`, taken so the loop runs it once.
    pub(in crate::commands::dashboard) fn take_external_edit(&mut self) -> Option<ExternalEdit> {
        self.pending_external_edit.take()
    }

    /// The editor came back: read the file into the prompt it was for, or
    /// say why the editor failed or the file could not be read.
    pub(in crate::commands::dashboard) fn finish_external_edit(
        &mut self,
        edit: ExternalEdit,
        ran: std::io::Result<()>,
    ) {
        let text = ran.and_then(|()| std::fs::read_to_string(&edit.path));
        let _ = std::fs::remove_file(&edit.path);
        let Some(screen) = self.agent_builder.as_deref_mut() else {
            return;
        };
        let Some(editor) = screen.editor.as_mut() else {
            return;
        };
        match text {
            Ok(text) => {
                if let Some(Overlay::Prompts(prompts)) = editor.overlay.as_mut() {
                    let area = match edit.target {
                        PromptFocus::System => &mut prompts.system,
                        PromptFocus::Transition => &mut prompts.transition,
                    };
                    let mode = area.mode();
                    *area = MarkdownEdit::from_text(&text).in_mode(mode);
                }
                editor.message = Some("Prompt updated from the editor".to_string());
            }
            Err(e) => {
                editor.message = Some(format!("The editor did not hand the prompt back: {e}"));
            }
        }
    }
}

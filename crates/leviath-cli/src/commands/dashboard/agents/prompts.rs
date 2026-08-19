//! A stage's prompts, edited full screen: what the stage is told to do
//! (`system_prompt`) and how it decides where to go next
//! (`transition_prompt`). `Tab` moves between the two, `Ctrl-S` or `Esc`
//! applies and closes, `Ctrl-Q` closes without applying, and `Ctrl-E` hands
//! the focused text to `$EDITOR`: the dashboard leaves the terminal, the
//! editor runs, and the text comes back into the box.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::TextArea;

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
    pub(in crate::commands::dashboard) system: TextArea<'static>,
    pub(in crate::commands::dashboard) transition: TextArea<'static>,
    pub(in crate::commands::dashboard) focus: PromptFocus,
}

impl PromptsEditor {
    fn new(stage: &str, system: &str, transition: &str) -> Self {
        Self {
            stage: stage.to_string(),
            system: textarea(system),
            transition: textarea(transition),
            focus: PromptFocus::System,
        }
    }

    /// The focused box.
    fn focused_mut(&mut self) -> &mut TextArea<'static> {
        match self.focus {
            PromptFocus::System => &mut self.system,
            PromptFocus::Transition => &mut self.transition,
        }
    }

    /// The text of a box, without the trailing newline a textarea adds.
    fn text_of(area: &TextArea<'static>) -> String {
        let mut text = area.lines().join("\n");
        // One trailing newline is how a multi-line prompt ends in the file;
        // a single line has none.
        if text.contains('\n') && !text.ends_with('\n') {
            text.push('\n');
        }
        text
    }
}

fn textarea(text: &str) -> TextArea<'static> {
    let mut area = TextArea::new(text.lines().map(str::to_string).collect());
    area.move_cursor(ratatui_textarea::CursorMove::Bottom);
    area.move_cursor(ratatui_textarea::CursorMove::End);
    area
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
        self.editor().overlay = Some(Overlay::Prompts(Box::new(PromptsEditor::new(
            &stage,
            &view.system_prompt,
            &view.transition_prompt,
        ))));
    }

    /// Keys on the prompts overlay.
    pub(super) fn editor_prompts_key(&mut self, key: &KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(Overlay::Prompts(prompts)) = self.editor().overlay.as_mut() else {
            return;
        };
        match (key.code, ctrl) {
            (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
                prompts.focus = match prompts.focus {
                    PromptFocus::System => PromptFocus::Transition,
                    PromptFocus::Transition => PromptFocus::System,
                };
            }
            (KeyCode::Esc, _) | (KeyCode::Char('s'), true) => self.editor_apply_prompts(),
            (KeyCode::Char('q'), true) => self.editor().overlay = None,
            (KeyCode::Char('e'), true) => self.editor_request_external_edit(),
            _ => {
                prompts
                    .focused_mut()
                    .input(ratatui_textarea::Input::from(*key));
            }
        }
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

    /// Ctrl-E: write the focused text to a temp file and ask the loop to
    /// run `$EDITOR` on it.
    pub(super) fn editor_request_external_edit(&mut self) {
        let Some(Overlay::Prompts(prompts)) = self.editor().overlay.as_mut() else {
            return;
        };
        let target = prompts.focus;
        let text = PromptsEditor::text_of(match target {
            PromptFocus::System => &prompts.system,
            PromptFocus::Transition => &prompts.transition,
        });
        let stage = prompts.stage.clone();
        let dir = self.external_edit_dir.clone();
        let path = dir.join(format!(
            "{stage}-{}.md",
            match target {
                PromptFocus::System => "system",
                PromptFocus::Transition => "transition",
            }
        ));
        let written = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, &text));
        match written {
            Ok(()) => self.pending_external_edit = Some(ExternalEdit { path, target }),
            Err(e) => {
                self.editor().message = Some(format!("Could not hand the prompt to an editor: {e}"))
            }
        }
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
                    *area = textarea(&text);
                }
                editor.message = Some("Prompt updated from the editor".to_string());
            }
            Err(e) => {
                editor.message = Some(format!("The editor did not hand the prompt back: {e}"));
            }
        }
    }
}

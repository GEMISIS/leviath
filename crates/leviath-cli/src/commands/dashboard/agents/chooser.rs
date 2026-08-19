//! Where a new agent starts: "Start simple", or a copy of an agent from the
//! catalog, under a name of your choosing.

use crossterm::event::{KeyCode, KeyEvent};

use super::super::state::Dashboard;
use super::super::types::ToastLevel;
use crate::blueprint_edit::catalog::CatalogEntry;
use crate::blueprint_edit::{is_valid_name, templates};
use crate::tui::widgets::line_edit::{EditOutcome, LineEdit};

/// One row of the chooser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::commands::dashboard) struct Template {
    /// What the row says.
    pub(in crate::commands::dashboard) label: String,
    /// What it is, in a few words.
    pub(in crate::commands::dashboard) detail: String,
    /// The name suggested for the new agent.
    pub(in crate::commands::dashboard) suggested: String,
    /// The manifest to start from; `None` is the two-stage starter.
    pub(in crate::commands::dashboard) manifest: Option<String>,
    /// The bundled agent it is a copy of, whose scripts come along.
    pub(in crate::commands::dashboard) bundled_from: Option<String>,
}

/// The chooser's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::commands::dashboard) struct Chooser {
    pub(in crate::commands::dashboard) templates: Vec<Template>,
    pub(in crate::commands::dashboard) cursor: usize,
    /// The name field. Follows the row's suggestion until the user types
    /// into it.
    pub(in crate::commands::dashboard) name: LineEdit,
    pub(in crate::commands::dashboard) name_touched: bool,
    /// Why the name will not do, on a reserved line under the field.
    pub(in crate::commands::dashboard) problem: Option<String>,
}

impl Chooser {
    /// The rows: the starter first, then every agent in the catalog.
    pub(in crate::commands::dashboard) fn new(entries: &[CatalogEntry]) -> Self {
        let mut templates = vec![Template {
            label: "Start simple".to_string(),
            detail: "two stages, work then finish, with placeholder prompts".to_string(),
            suggested: "my-agent".to_string(),
            manifest: None,
            bundled_from: None,
        }];
        for entry in entries.iter().filter(|e| e.manifest.is_some()) {
            templates.push(Template {
                label: format!("Clone {}", entry.name),
                detail: format!("{} · {}", entry.source.as_str(), entry.description),
                suggested: format!("my-{}", entry.name),
                manifest: entry.manifest.clone(),
                bundled_from: (entry.source == crate::blueprint_edit::catalog::Source::Bundled
                    || entry.bundled)
                    .then(|| entry.name.clone()),
            });
        }
        Self {
            templates,
            cursor: 0,
            name: LineEdit::new("my-agent", false),
            name_touched: false,
            problem: None,
        }
    }

    /// The row under the cursor.
    pub(in crate::commands::dashboard) fn selected(&self) -> &Template {
        &self.templates[self.cursor.min(self.templates.len() - 1)]
    }

    fn move_by(&mut self, delta: isize) {
        let len = self.templates.len() as isize;
        self.cursor = (self.cursor as isize + delta).clamp(0, len - 1) as usize;
        if !self.name_touched {
            self.name = LineEdit::new(self.selected().suggested.clone(), false);
        }
    }

    /// Whether the name will do, and if not why. `taken` are the names
    /// already in the catalog.
    pub(in crate::commands::dashboard) fn check_name(&mut self, taken: &[String]) -> bool {
        let name = self.name.value().to_string();
        self.problem = if !is_valid_name(&name) {
            Some("Letters, digits, `.`, `_` and `-` only.".to_string())
        } else if taken.contains(&name) {
            Some(format!("An agent named {name} already exists."))
        } else {
            None
        };
        self.problem.is_none()
    }
}

impl Dashboard {
    /// `n` on the catalog: open the chooser.
    pub(in crate::commands::dashboard) fn open_chooser(&mut self) {
        let chooser = Chooser::new(&self.agents().catalog.entries);
        self.agents().chooser = Some(chooser);
    }

    /// Keys on the chooser: the arrows pick a template, anything else types
    /// the name; Enter opens the editor on it, Esc closes the chooser.
    pub(in crate::commands::dashboard) fn handle_chooser_key(&mut self, key: KeyEvent) {
        let taken: Vec<String> = self
            .agents()
            .catalog
            .entries
            .iter()
            .map(|e| e.name.clone())
            .collect();
        let chooser = self.agents().chooser.as_mut().expect("callers check");
        match key.code {
            KeyCode::Up => chooser.move_by(-1),
            KeyCode::Down => chooser.move_by(1),
            _ => {
                let was = chooser.name.value().to_string();
                match chooser.name.handle_key(&key) {
                    EditOutcome::Cancel => {
                        self.agents().chooser = None;
                        return;
                    }
                    EditOutcome::Commit => {
                        if chooser.check_name(&taken) {
                            let template = chooser.selected().clone();
                            let name = chooser.name.value().to_string();
                            self.agents().chooser = None;
                            self.start_from_template(&template, &name);
                        }
                        return;
                    }
                    EditOutcome::Pending => {}
                }
                if chooser.name.value() != was {
                    chooser.name_touched = true;
                    chooser.check_name(&taken);
                }
            }
        }
    }

    /// Open the editor on a fresh, unsaved agent from a template.
    fn start_from_template(&mut self, template: &Template, name: &str) {
        let text = match &template.manifest {
            Some(text) => templates::clone_of(text, name),
            None => templates::empty_blueprint(name),
        };
        match text {
            Ok(text) => self.open_editor(
                super::editor::EditTarget::New {
                    name: name.to_string(),
                    bundled_from: template.bundled_from.clone(),
                },
                &text,
            ),
            Err(e) => self.toast(
                format!("Could not start from that template: {e}"),
                ToastLevel::Error,
            ),
        }
    }
}

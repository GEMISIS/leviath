//! The catalog half of the Agents screen: the list, its filter, the preview,
//! and the actions on the selected agent.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::Line;

use super::super::state::Dashboard;
use super::super::types::*;
use crate::blueprint_edit::catalog::{self, CatalogEntry, Source};
use crate::config::Config;
use crate::tui::flowgraph::{FlowView, StageGraph};
use crate::tui::widgets::confirm::Confirm;

/// The catalog's state.
#[derive(Default)]
pub(in crate::commands::dashboard) struct Catalog {
    /// Every agent found, name-sorted.
    pub(in crate::commands::dashboard) entries: Vec<CatalogEntry>,
    /// The filter text; matched against name and description.
    pub(in crate::commands::dashboard) filter: String,
    /// `/` opened the filter and letters go into it until Enter or Esc.
    pub(in crate::commands::dashboard) filtering: bool,
    /// Cursor into the filtered list.
    pub(in crate::commands::dashboard) selected: usize,
    /// The graph of the entry the cursor is on, keyed by its name so it is
    /// rebuilt only when the cursor moves to another agent.
    pub(in crate::commands::dashboard) preview: Option<(String, Result<FlowView, String>)>,
}

impl Catalog {
    /// Read the catalog again, keeping the cursor on the same agent when
    /// it is still there.
    pub(in crate::commands::dashboard) fn refresh(&mut self, ctx: &NewRunContext, config: &Config) {
        let keep = self.selected_entry().map(|e| e.name.clone());
        self.entries = catalog::discover(&ctx.agents_dir, &ctx.workdir, config);
        if let Some(name) = keep
            && let Some(at) = self
                .visible()
                .iter()
                .position(|i| self.entries[*i].name == name)
        {
            self.selected = at;
        }
        self.clamp();
    }

    /// Indices into `entries` that pass the filter, in list order.
    pub(in crate::commands::dashboard) fn visible(&self) -> Vec<usize> {
        let query = self.filter.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                query.is_empty()
                    || e.name.to_lowercase().contains(&query)
                    || e.description.to_lowercase().contains(&query)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// The entry under the cursor.
    pub(in crate::commands::dashboard) fn selected_entry(&self) -> Option<&CatalogEntry> {
        self.visible()
            .get(self.selected)
            .and_then(|i| self.entries.get(*i))
    }

    /// Keep the cursor inside the filtered list.
    pub(in crate::commands::dashboard) fn clamp(&mut self) {
        let len = self.visible().len();
        self.selected = self.selected.min(len.saturating_sub(1));
    }

    /// Move the cursor, clamped.
    pub(in crate::commands::dashboard) fn move_by(&mut self, delta: isize) {
        let len = self.visible().len() as isize;
        let next = (self.selected as isize + delta).clamp(0, (len - 1).max(0));
        self.selected = next as usize;
    }

    /// Bring the preview in line with the cursor: build the selected agent's
    /// graph when it is not the one built.
    pub(in crate::commands::dashboard) fn sync_preview(&mut self) {
        let Some(entry) = self.selected_entry() else {
            self.preview = None;
            return;
        };
        if self
            .preview
            .as_ref()
            .is_some_and(|(name, _)| *name == entry.name)
        {
            return;
        }
        let name = entry.name.clone();
        let view = match entry.manifest.as_deref() {
            Some(text) => leviath_core::manifest::parse_manifest(text)
                .map(|bp| FlowView::new(Arc::new(StageGraph::from_blueprint(&bp)), true))
                .map_err(|e| e.to_string()),
            None => Err("the manifest could not be read".to_string()),
        };
        self.preview = Some((name, view));
    }
}

impl Dashboard {
    /// Keys on the catalog.
    pub(in crate::commands::dashboard) fn handle_catalog_key(&mut self, key: KeyEvent) {
        let code = key.code;
        let filtering = self.agents().catalog.filtering;
        if filtering {
            let catalog = &mut self.agents().catalog;
            match code {
                KeyCode::Esc => {
                    catalog.filter.clear();
                    catalog.filtering = false;
                }
                KeyCode::Enter => catalog.filtering = false,
                KeyCode::Backspace => {
                    catalog.filter.pop();
                }
                KeyCode::Char(c) => catalog.filter.push(c),
                _ => {}
            }
            catalog.clamp();
            return;
        }
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                let catalog = &mut self.agents().catalog;
                if catalog.filter.is_empty() {
                    self.close_agents_screen();
                } else {
                    catalog.filter.clear();
                    catalog.clamp();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.agents().catalog.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.agents().catalog.move_by(1),
            KeyCode::Home => self.agents().catalog.selected = 0,
            KeyCode::End => self.agents().catalog.move_by(isize::MAX / 2),
            KeyCode::PageUp => self.agents().catalog.move_by(-8),
            KeyCode::PageDown => self.agents().catalog.move_by(8),
            KeyCode::Char('/') => self.agents().catalog.filtering = true,
            KeyCode::Enter | KeyCode::Char('e') => self.edit_selected_agent(),
            KeyCode::Char('n') => self.open_chooser(),
            KeyCode::Char('d') => self.request_agent_delete(),
            KeyCode::Char('r') => self.request_agent_reset(),
            KeyCode::Char('l') => self.launch_selected_agent(),
            KeyCode::Char('?') | KeyCode::F(1) => self.show_help = true,
            _ => {}
        }
    }

    /// Open the editor on the agent under the cursor. A bundled agent not
    /// installed opens from its embedded copy and installs when saved.
    fn edit_selected_agent(&mut self) {
        let Some(entry) = self.agents().catalog.selected_entry().cloned() else {
            return;
        };
        let Some(text) = entry.manifest.clone() else {
            self.toast("That agent's manifest could not be read", ToastLevel::Error);
            return;
        };
        self.open_editor(
            super::editor::EditTarget::Existing {
                name: entry.name.clone(),
                dir: entry.dir.clone(),
                bundled_from: entry.bundled.then(|| entry.name.clone()),
            },
            &text,
        );
    }

    /// The wheel over the catalog list moves the cursor, as it does over the
    /// run list. Returns whether the event was the wheel over the list.
    pub(in crate::commands::dashboard) fn catalog_wheel(
        &mut self,
        event: crossterm::event::MouseEvent,
    ) -> bool {
        use crossterm::event::MouseEventKind;
        let screen = self.agents();
        if screen.editor.is_some() || screen.chooser.is_some() {
            return false;
        }
        let delta = match event.kind {
            MouseEventKind::ScrollUp => -1,
            MouseEventKind::ScrollDown => 1,
            _ => return false,
        };
        let area = screen.list_area;
        let inside = event.column >= area.x
            && event.column < area.x + area.width
            && event.row >= area.y
            && event.row < area.y + area.height;
        if !inside {
            return false;
        }
        screen.catalog.move_by(delta);
        true
    }

    /// `d`: confirm deleting the selected agent, when it can be deleted from
    /// here.
    fn request_agent_delete(&mut self) {
        let Some(entry) = self.agents().catalog.selected_entry().cloned() else {
            return;
        };
        if !entry.deletable() {
            let where_ = match entry.source {
                Source::Bundled => "It is not installed; there is nothing to delete.",
                _ => "It lives outside the agents directory; delete it where it is.",
            };
            self.toast(
                format!("{} is not deletable from here. {where_}", entry.name),
                ToastLevel::Info,
            );
            return;
        }
        let dialog = Confirm::new(
            "Delete agent?",
            vec![Line::from(format!(
                "Delete '{}' and everything in its directory? This cannot be undone.",
                entry.name
            ))],
            "Delete",
            "Cancel",
        )
        .danger();
        self.pending_confirm = Some((ConfirmAction::AgentDelete { name: entry.name }, dialog));
    }

    /// The confirmed delete.
    pub(in crate::commands::dashboard) fn perform_agent_delete(&mut self, name: &str) {
        let dir = self.new_run_ctx.agents_dir.clone();
        match catalog::delete_agent(&dir, name) {
            Ok(()) => {
                self.toast(format!("Deleted {name}"), ToastLevel::Info);
                let mut store = self.layout_store();
                store.forget(name);
                let _ = store.save();
            }
            Err(e) => self.toast(format!("Could not delete {name}: {e}"), ToastLevel::Error),
        }
        self.refresh_catalog();
    }

    /// `r`: confirm putting a bundled agent's embedded copy back.
    fn request_agent_reset(&mut self) {
        let Some(entry) = self.agents().catalog.selected_entry().cloned() else {
            return;
        };
        if !entry.bundled || !entry.differs_from_bundled {
            self.toast(
                format!(
                    "{} is not an edited bundled agent; nothing to reset",
                    entry.name
                ),
                ToastLevel::Info,
            );
            return;
        }
        let dialog = Confirm::new(
            "Reset to original?",
            vec![Line::from(format!(
                "Replace '{}' with the copy bundled in this binary? Your edits to it are lost.",
                entry.name
            ))],
            "Reset",
            "Cancel",
        )
        .danger();
        self.pending_confirm = Some((ConfirmAction::AgentReset { name: entry.name }, dialog));
    }

    /// The confirmed reset.
    pub(in crate::commands::dashboard) fn perform_agent_reset(&mut self, name: &str) {
        let dir = self.new_run_ctx.agents_dir.clone();
        match catalog::reset_bundled(&dir, name) {
            Ok(()) => self.toast(
                format!("Reset {name} to the bundled copy"),
                ToastLevel::Info,
            ),
            Err(e) => self.toast(format!("Could not reset {name}: {e}"), ToastLevel::Error),
        }
        self.refresh_catalog();
    }

    /// `l`: leave for the new-run screen with this agent picked.
    fn launch_selected_agent(&mut self) {
        let Some(name) = self
            .agents()
            .catalog
            .selected_entry()
            .map(|e| e.name.clone())
        else {
            return;
        };
        self.close_agents_screen();
        self.open_new_run_screen();
        // The new-run picker lists the same catalog, so the agent is there.
        self.new_run_selected = self
            .new_run_agents
            .iter()
            .position(|a| a.name == name)
            .unwrap_or(0);
    }

    /// Read the catalog again after something changed on disk, and keep the
    /// preview honest.
    pub(in crate::commands::dashboard) fn refresh_catalog(&mut self) {
        let config = self.agents_config();
        let ctx = NewRunContext {
            agents_dir: self.new_run_ctx.agents_dir.clone(),
            config_path: self.new_run_ctx.config_path.clone(),
            workdir: self.new_run_ctx.workdir.clone(),
        };
        let catalog = &mut self.agents().catalog;
        catalog.refresh(&ctx, &config);
        catalog.preview = None;
        // The new-run picker shows the same agents.
        self.refresh_new_run_agents();
    }

    /// The layout store, opened at the configured path (or in memory when
    /// there is none).
    pub(in crate::commands::dashboard) fn layout_store(
        &self,
    ) -> crate::blueprint_edit::LayoutStore {
        match &self.layout_store_path {
            Some(path) => crate::blueprint_edit::LayoutStore::open(path.clone()),
            None => crate::blueprint_edit::LayoutStore::in_memory(),
        }
    }
}

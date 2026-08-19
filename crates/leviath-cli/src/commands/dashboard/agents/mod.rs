//! The Agents screen (`a` from the run list): the catalog of agents this
//! dashboard can open, and the editor that builds one.
//!
//! The catalog is the same list `lev list` and the new-run picker show, with
//! the selected agent's graph, description and stages on the right. From
//! it: `Enter` edits, `n` starts a new agent from a template, `d` deletes an
//! installed one, `r` puts a bundled one's embedded copy back, `l` launches
//! it. The editor is a full screen of its own: the graph canvas on the
//! left, an inspector on the right showing whatever is selected, the same
//! shape as The Lair's editor.

mod catalog;
mod chooser;
mod editor;
mod editor_keys;
mod inspector;
mod render_catalog;
mod render_editor;
#[cfg(test)]
mod tests;

use ratatui::layout::Rect;

pub(in crate::commands::dashboard) use catalog::Catalog;
pub(in crate::commands::dashboard) use chooser::Chooser;
pub(in crate::commands::dashboard) use editor::Editor;

use super::state::Dashboard;
use crate::config::Config;

/// The screen's state while it is open.
pub(in crate::commands::dashboard) struct AgentsScreen {
    /// The list, the filter and the preview.
    pub(in crate::commands::dashboard) catalog: Catalog,
    /// The template chooser, when `n` opened it.
    pub(in crate::commands::dashboard) chooser: Option<Chooser>,
    /// The editor, when an agent is open in it.
    pub(in crate::commands::dashboard) editor: Option<Editor>,
    /// The whole terminal at the last draw, for overlays that take the mouse.
    pub(in crate::commands::dashboard) last_area: Rect,
}

impl Dashboard {
    /// Open the Agents screen on the catalog.
    pub(in crate::commands::dashboard) fn open_agents_screen(&mut self) {
        let mut catalog = Catalog::default();
        let config = self.agents_config();
        catalog.refresh(&self.new_run_ctx, &config);
        self.agent_builder = Some(Box::new(AgentsScreen {
            catalog,
            chooser: None,
            editor: None,
            last_area: Rect::default(),
        }));
    }

    /// Close the Agents screen, whatever it was showing.
    pub(in crate::commands::dashboard) fn close_agents_screen(&mut self) {
        self.agent_builder = None;
    }

    /// The config the catalog reads `agent_paths` from.
    pub(in crate::commands::dashboard) fn agents_config(&self) -> Config {
        Config::load_from_path_public(&self.new_run_ctx.config_path).unwrap_or_default()
    }

    /// The open screen, for the modules that drive it.
    pub(in crate::commands::dashboard) fn agents(&mut self) -> &mut AgentsScreen {
        self.agent_builder
            .as_deref_mut()
            .expect("callers check the screen is open")
    }

    /// Keys while the Agents screen is open. The editor takes them when it
    /// is on; the chooser when it is; the catalog otherwise. Help is a
    /// dashboard-wide overlay and closes through the dashboard.
    pub(in crate::commands::dashboard) fn handle_agents_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) {
        if self.agents().editor.is_some() {
            self.handle_editor_key(key);
        } else if self.agents().chooser.is_some() {
            self.handle_chooser_key(key);
        } else {
            self.handle_catalog_key(key);
        }
    }

    /// The mouse while the Agents screen is open: the editor's chooser
    /// takes it when open; the graph panes take it as everywhere else,
    /// and the editor then acts on what the canvas did.
    pub(in crate::commands::dashboard) fn handle_agents_mouse(
        &mut self,
        event: crossterm::event::MouseEvent,
    ) -> bool {
        let area = self.agents().last_area;
        if self.editor_picker_mouse(event, area) {
            return true;
        }
        if self.route_mouse_to_graph(event) {
            self.editor_drain_canvas();
            return true;
        }
        false
    }
}

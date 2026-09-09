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
mod choices;
mod chooser;
mod context_menu;
mod editor;
mod editor_keys;
mod editor_panels;
mod inspector;
#[cfg(test)]
mod menu_tests;
#[cfg(test)]
mod panel_tests;
mod prompts;
mod render_catalog;
mod render_editor;
mod render_prompts;
#[cfg(test)]
mod tests;

use ratatui::layout::Rect;

pub(in crate::commands::dashboard) use catalog::Catalog;
pub(in crate::commands::dashboard) use chooser::Chooser;
pub(in crate::commands::dashboard) use editor::Editor;
pub(in crate::commands::dashboard) use prompts::ExternalEdit;
#[cfg(test)]
pub(in crate::commands::dashboard) use prompts::PromptFocus;

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
    /// Where the last frame drew the catalog list, for the wheel.
    pub(in crate::commands::dashboard) list_area: Rect,
    /// `provider/model` ids the providers reported, once the background
    /// listing comes back; empty until then.
    pub(in crate::commands::dashboard) model_catalog: Vec<String>,
    /// The listing's channel, drained each tick.
    pub(in crate::commands::dashboard) models_rx:
        Option<tokio::sync::mpsc::UnboundedReceiver<Vec<String>>>,
    /// Every MCP server the tools chooser knows, with the tools it
    /// advertises once it has answered: the config's servers from the
    /// moment the screen opens, an agent's own from the moment its editor
    /// does.
    pub(in crate::commands::dashboard) mcp: McpCatalog,
    /// The channel the servers' answers arrive on, drained each tick.
    pub(in crate::commands::dashboard) mcp_rx: Option<McpFeed>,
    /// The sender the listings answer on, kept so an editor opened later
    /// can ask about the servers its blueprint declares.
    pub(in crate::commands::dashboard) mcp_tx:
        Option<tokio::sync::mpsc::UnboundedSender<McpAnswer>>,
}

/// What the tools chooser knows about one MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::commands::dashboard) enum McpServerTools {
    /// Asked, not yet answered.
    Pending,
    /// The tools it advertised.
    Listed(Vec<String>),
    /// Why it could not be asked.
    Failed(String),
}

/// Every MCP server the chooser offers, by name.
pub(in crate::commands::dashboard) type McpCatalog =
    std::collections::BTreeMap<String, McpServerTools>;

/// One server's answer: its name and its tools, or why not.
pub(in crate::commands::dashboard) type McpAnswer = (String, Result<Vec<String>, String>);

/// The channel the answers arrive on.
pub(in crate::commands::dashboard) type McpFeed = tokio::sync::mpsc::UnboundedReceiver<McpAnswer>;

impl Dashboard {
    /// Open the Agents screen on the catalog.
    pub(in crate::commands::dashboard) fn open_agents_screen(&mut self) {
        let mut catalog = Catalog::default();
        let config = self.agents_config();
        catalog.refresh(&self.new_run_ctx, &config);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (mcp_tx, mcp_rx) = tokio::sync::mpsc::unbounded_channel();
        self.agent_builder = Some(Box::new(AgentsScreen {
            catalog,
            chooser: None,
            editor: None,
            last_area: Rect::default(),
            list_area: Rect::default(),
            model_catalog: Vec::new(),
            models_rx: Some(rx),
            mcp: McpCatalog::new(),
            mcp_rx: Some(mcp_rx),
            mcp_tx: Some(mcp_tx),
        }));
        // And every MCP server the config names for its tools, the same way.
        self.ask_mcp_servers(&config.mcp_servers);
        // Ask every configured provider for its models, off the UI loop; the
        // chooser offers the closed catalog until they land.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let config = self.agents_config();
            handle.spawn(async move {
                let models = crate::commands::serve::list_model_ids(
                    &config,
                    &leviath_providers::provider::build_http_client,
                )
                .await;
                let _ = tx.send(models);
            });
        }
    }

    /// Ask each of `servers` for its tools, off the UI loop, unless the
    /// chooser already knows it. The chooser offers the server itself (as a
    /// connector) at once and its tools when the answer lands.
    pub(in crate::commands::dashboard) fn ask_mcp_servers(
        &mut self,
        servers: &[leviath_mcp::MCPServerConfig],
    ) {
        let config = self.agents_config();
        let screen = self.agents();
        for server in servers {
            if screen.mcp.contains_key(&server.name) {
                continue;
            }
            screen
                .mcp
                .insert(server.name.clone(), McpServerTools::Pending);
            // Without a runtime (a test's dashboard) the server stays
            // pending, which the chooser shows as such.
            if let (Some(tx), Ok(handle)) =
                (screen.mcp_tx.clone(), tokio::runtime::Handle::try_current())
            {
                let config = config.clone();
                let server = server.clone();
                handle.spawn(async move {
                    let name = server.name.clone();
                    let tools = crate::commands::serve::list_mcp_tools(config, server).await;
                    let _ = tx.send((name, tools));
                });
            }
        }
        if let Some(editor) = screen.editor.as_mut() {
            editor.rebuild_tools(&screen.mcp);
        }
    }

    /// Take the models the providers reported and the tools the MCP servers
    /// advertised, when they have.
    pub(in crate::commands::dashboard) fn drain_agents_models(&mut self) {
        let Some(screen) = self.agent_builder.as_deref_mut() else {
            return;
        };
        if let Some(rx) = screen.models_rx.as_mut() {
            loop {
                match rx.try_recv() {
                    Ok(models) => {
                        screen.model_catalog = models;
                        if let Some(editor) = screen.editor.as_mut() {
                            editor.models.extend(screen.model_catalog.iter().cloned());
                            editor.models.sort();
                            editor.models.dedup();
                        }
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    // The task has answered and gone: nothing more will come.
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        screen.models_rx = None;
                        break;
                    }
                }
            }
        }
        let Some(rx) = screen.mcp_rx.as_mut() else {
            return;
        };
        let mut changed = false;
        loop {
            match rx.try_recv() {
                Ok((name, answer)) => {
                    let state = match answer {
                        Ok(tools) => McpServerTools::Listed(tools),
                        Err(e) => McpServerTools::Failed(e),
                    };
                    screen.mcp.insert(name, state);
                    changed = true;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                // Every sender is gone, which the screen holds one of for
                // its whole life; a test that dropped it says nothing more
                // will come.
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    screen.mcp_rx = None;
                    break;
                }
            }
        }
        if changed && let Some(editor) = screen.editor.as_mut() {
            editor.rebuild_tools(&screen.mcp);
        }
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
        if self.editor_menu_mouse(event) {
            return true;
        }
        if self.editor_picker_mouse(event, area) {
            return true;
        }
        if self.catalog_wheel(event) {
            return true;
        }
        if self.editor_inspector_mouse(event) {
            return true;
        }
        if self.route_mouse_to_graph(event) {
            self.editor_drain_canvas();
            return true;
        }
        false
    }
}

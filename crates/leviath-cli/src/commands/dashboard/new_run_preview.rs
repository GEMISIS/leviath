//! The new-run screen's blueprint preview: the selected agent's stage graph,
//! drawn above the task editor while you pick.
//!
//! The picker row says a name and a description; the preview says what the
//! agent will do: how many stages, in what order, where it loops. It is a
//! viewer (drag pans, nothing selects) and it follows the picker: moving the
//! selection swaps the graph, and a blueprint whose manifest cannot be read
//! says so here, before `lev run` would have refused it.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use super::graph::{bundled_stage_graph, load_stage_graph};
use super::state::Dashboard;
use super::theme::*;
use super::types::*;
use crate::tui::flowgraph::{FlowView, NodeStyle};

/// The rows the task editor keeps for itself; the preview only appears
/// when the pane has this many left over for it.
pub(super) const TASK_MIN_HEIGHT: u16 = 8;
/// The preview's smallest useful height (a border, two rows of boxes, a lane).
pub(super) const PREVIEW_MIN_HEIGHT: u16 = 8;
/// And its largest: more rows than this only show empty canvas.
pub(super) const PREVIEW_MAX_HEIGHT: u16 = 14;

/// The preview canvas for one catalog row, kept between frames.
#[derive(Debug)]
pub(super) struct BlueprintPreview {
    /// The row it was built for: `NewRunAgent::path`.
    pub(super) key: String,
    /// The graph, or why there is none.
    pub(super) view: Result<FlowView, String>,
}

/// How many rows the preview takes out of a right pane `height` rows tall:
/// none when the task editor would drop below its minimum.
pub(super) fn preview_height(pane_height: u16) -> u16 {
    if pane_height < TASK_MIN_HEIGHT + PREVIEW_MIN_HEIGHT {
        return 0;
    }
    (pane_height * 2 / 5).clamp(PREVIEW_MIN_HEIGHT, PREVIEW_MAX_HEIGHT)
}

impl Dashboard {
    /// Make the preview match the picker's selection: build it when the
    /// selected row changed, keep it otherwise. Parsing a manifest per frame
    /// would be wasteful; per selection change it is nothing.
    pub(super) fn sync_new_run_preview(&mut self) {
        let Some(agent) = self.new_run_selected_agent() else {
            self.new_run_preview = None;
            return;
        };
        if self
            .new_run_preview
            .as_ref()
            .is_some_and(|p| p.key == agent.path)
        {
            return;
        }
        let key = agent.path.clone();
        let graph = if agent.source == "bundled" {
            bundled_stage_graph(&agent.name)
        } else {
            load_stage_graph(&agent.path)
        };
        let view = match graph {
            Some(graph) => Ok(FlowView::new(graph, NodeStyle::Compact, true)),
            None => Err(format!(
                "no preview: could not read a blueprint at {}",
                agent.path
            )),
        };
        self.new_run_preview = Some(BlueprintPreview { key, view });
    }

    /// Draw the preview into `area`.
    pub(super) fn draw_new_run_preview(&mut self, frame: &mut Frame, area: Rect) {
        self.sync_new_run_preview();
        let Some(preview) = self.new_run_preview.as_mut() else {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    " Pick an agent blueprint to see its stages.",
                    Style::default().fg(C_DIM),
                )))
                .block(preview_block(" Blueprint ".to_string())),
                area,
            );
            return;
        };
        match &mut preview.view {
            Ok(view) => {
                let graph = view.graph().clone();
                // The key is a directory for a discovered blueprint and the
                // bare name for a bundled one; either way the last component
                // is the name.
                let name = std::path::Path::new(&preview.key)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or(preview.key.clone());
                let title = format!(
                    " {name} · {} stage{} · entry {} · drag to pan ",
                    graph.stage_count(),
                    if graph.stage_count() == 1 { "" } else { "s" },
                    graph.entry
                );
                let canvas = view.render(frame, area, Some(preview_block(title)));
                self.pane_rects.push((PaneId::NewRunPreview, canvas));
            }
            Err(why) => {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        format!(" {why}"),
                        Style::default().fg(C_WARN),
                    )))
                    .block(preview_block(" Blueprint ".to_string())),
                    area,
                );
            }
        }
    }
}

fn preview_block(title: String) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_BORDER))
        .title(Span::styled(title, Style::default().fg(C_DIM)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::{make_test_dashboard, rendered_buffer};
    use crate::test_support::write_test_agent;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::Path;

    /// A dashboard whose new-run catalog is read from `dir`, with the screen
    /// open.
    fn dash_at(dir: &Path) -> Dashboard {
        let mut dash = make_test_dashboard();
        dash.new_run_ctx = NewRunContext {
            agents_dir: dir.join("agents"),
            config_path: dir.join("config.toml"),
            workdir: dir.to_path_buf(),
        };
        dash.open_new_run_screen();
        dash
    }

    fn write_agent(dir: &Path, name: &str, body: &str) {
        let agent_dir = dir.join("agents").join(name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        write_test_agent(
            &agent_dir,
            format!("[agent]\nname = \"{name}\"\ndescription = \"d\"\n{body}"),
        );
    }

    fn draw(dash: &mut Dashboard, w: u16, h: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| dash.draw(f)).unwrap();
        terminal
    }

    fn select(dash: &mut Dashboard, name: &str) {
        let index = dash
            .filtered_new_run_agents()
            .into_iter()
            .position(|i| dash.new_run_agents[i].name == name)
            .expect("the agent is in the catalog");
        dash.new_run_selected = index;
    }

    #[test]
    fn preview_height_leaves_the_task_editor_its_rows() {
        assert_eq!(preview_height(10), 0);
        assert_eq!(
            preview_height(TASK_MIN_HEIGHT + PREVIEW_MIN_HEIGHT),
            PREVIEW_MIN_HEIGHT
        );
        assert_eq!(preview_height(30), 12);
        assert_eq!(preview_height(80), PREVIEW_MAX_HEIGHT);
    }

    #[test]
    fn the_preview_follows_the_selected_agent_and_says_why_when_it_cannot_draw() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "alpha",
            "[stages.gather]\n[stages.gather.transitions.write]\n[stages.write]\n[stages.write.transitions]\n",
        );
        let mut dash = dash_at(dir.path());
        select(&mut dash, "alpha");
        let terminal = draw(&mut dash, 160, 40);
        let text = rendered_buffer(&terminal);
        assert!(text.contains("alpha · 2 stages · entry gather"), "{text}");
        assert!(
            text.contains(" gather ") && text.contains(" write "),
            "{text}"
        );
        assert!(
            dash.pane_rects
                .iter()
                .any(|(id, _)| *id == PaneId::NewRunPreview)
        );
        // Drawing again with the same selection keeps the same canvas.
        let key = dash.new_run_preview.as_ref().unwrap().key.clone();
        draw(&mut dash, 160, 40);
        assert_eq!(dash.new_run_preview.as_ref().unwrap().key, key);

        // A row whose manifest cannot be read (the catalog lists what parsed
        // when it was built; a file can go away after) says so instead of
        // drawing.
        dash.new_run_agents.push(NewRunAgent {
            name: "broken".to_string(),
            source: "local".to_string(),
            description: String::new(),
            path: dir
                .path()
                .join("agents/gone")
                .to_string_lossy()
                .into_owned(),
        });
        select(&mut dash, "broken");
        let text = rendered_buffer(&draw(&mut dash, 160, 40));
        assert!(text.contains("no preview"), "{text}");
        assert!(text.contains("could not read a blueprint"), "{text}");
        assert!(dash.new_run_preview.as_ref().unwrap().view.is_err());

        // A bundled blueprint that is not installed previews from the binary.
        let bundled = dash
            .new_run_agents
            .iter()
            .position(|a| a.source == "bundled")
            .expect("the catalog lists the bundled blueprints");
        dash.new_run_filter.clear();
        dash.new_run_selected = bundled;
        let text = rendered_buffer(&draw(&mut dash, 200, 50));
        assert!(text.contains("stages · entry"), "{text}");
        assert!(dash.new_run_preview.as_ref().unwrap().view.is_ok());

        // Filtering everything out empties the preview.
        dash.new_run_filter = "zzz-nothing".to_string();
        let text = rendered_buffer(&draw(&mut dash, 160, 40));
        assert!(text.contains("Pick an agent blueprint"), "{text}");
        assert!(dash.new_run_preview.is_none());
    }

    #[test]
    fn a_short_screen_skips_the_preview_and_the_popup_still_anchors_to_the_task_pane() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(dir.path(), "alpha", "[stages.only]\n");
        std::fs::write(dir.path().join("notes.md"), "x").unwrap();
        let mut dash = dash_at(dir.path());
        select(&mut dash, "alpha");
        let text = rendered_buffer(&draw(&mut dash, 100, 12));
        assert!(!text.contains("entry only"), "{text}");
        assert!(text.contains("Task for alpha"), "{text}");
        // The `@` completion floats at the bottom of the task pane, whether or
        // not the preview is above it.
        dash.new_run_focus = NewRunPane::Task;
        dash.new_run_file_ref = true;
        dash.new_run_file_query.clear();
        let text = rendered_buffer(&draw(&mut dash, 100, 12));
        assert!(text.contains(" files "), "{text}");
        let text = rendered_buffer(&draw(&mut dash, 160, 40));
        assert!(
            text.contains(" files ") && text.contains("entry only"),
            "{text}"
        );
    }

    #[test]
    fn the_preview_takes_the_mouse_and_ticks() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "alpha",
            "[stages.a]\n[stages.a.transitions.b]\n[stages.b]\n[stages.b.transitions.a]\n[stages.b.transitions.c]\n[stages.c]\n[stages.c.transitions]\n",
        );
        let mut dash = dash_at(dir.path());
        select(&mut dash, "alpha");
        draw(&mut dash, 160, 40);
        let canvas = dash
            .pane_rects
            .iter()
            .find(|(id, _)| *id == PaneId::NewRunPreview)
            .map(|(_, r)| *r)
            .expect("the preview registered its canvas");
        let view = |d: &Dashboard| {
            d.new_run_preview
                .as_ref()
                .unwrap()
                .view
                .as_ref()
                .ok()
                .unwrap()
                .pan()
        };
        let pan = view(&dash);
        let (x, y) = (canvas.x + canvas.width - 3, canvas.y + 1);
        let mouse = |kind, x, y| MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
        dash.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), x - 10, y));
        dash.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x - 10, y));
        assert_ne!(view(&dash), pan);
        dash.tick_graphs(std::time::Duration::from_millis(100));
        // With the screen closed the preview canvas is not a mouse target.
        dash.close_new_run_screen();
        assert!(dash.graph_view_mut(PaneId::NewRunPreview).is_none());
    }
}

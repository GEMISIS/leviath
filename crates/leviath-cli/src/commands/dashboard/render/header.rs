//! Header breadcrumb and info strip rendering.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};

use crate::commands::dashboard::helpers::{
    elapsed_str, elapsed_str_until, format_tokens, truncate,
};
use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::theme::*;
use crate::commands::dashboard::types::*;

impl Dashboard {
    pub(in crate::commands::dashboard) fn render_header_breadcrumb(
        &self,
        frame: &mut Frame,
        hdr_area: Rect,
        agent: &DashboardAgent,
    ) {
        let effective_start = agent.started_at + agent.waiting_secs as i64;
        let elapsed = if let Some(until) = agent.active_until {
            elapsed_str_until(effective_start, until)
        } else {
            elapsed_str(effective_start)
        };
        let status_color = agent.status.color();
        let spinner_frame = SPINNER[(self.tick_count as usize) % SPINNER.len()];
        let status_span = match &agent.status {
            AgentDisplayStatus::Active => Span::styled(
                format!("{} {} ", spinner_frame, agent.status),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
            _ => Span::styled(
                format!("{} ", agent.status),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
        };
        let raw_title = agent.title.as_deref().unwrap_or(&agent.blueprint_name);
        let title_text = raw_title.trim_start_matches('#').trim();
        let hdr_line = Line::from(vec![
            Span::styled(
                format!(" {} ", truncate(title_text, 28)),
                Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
            ),
            Span::styled("· ", Style::default().fg(C_DIM)),
            status_span,
            Span::styled("· ", Style::default().fg(C_DIM)),
            Span::styled(
                format!("{}↑", format_tokens(agent.tokens_in)),
                Style::default().fg(C_DIM),
            ),
            Span::styled(
                format!(" {}↓", format_tokens(agent.tokens_out)),
                Style::default().fg(C_DIM),
            ),
            Span::styled(format!(" · {} ", elapsed), Style::default().fg(C_DIM)),
            if agent.accepts_messages {
                Span::styled("💬 ", Style::default().fg(C_SUCCESS))
            } else {
                Span::styled("🔇 ", Style::default().fg(C_DIM))
            },
            Span::styled("· ", Style::default().fg(C_DIM)),
            Span::styled(agent.id.clone(), Style::default().fg(C_DIM)),
        ]);
        frame.render_widget(
            Paragraph::new(hdr_line).style(Style::default().bg(Color::Rgb(20, 20, 30))),
            hdr_area,
        );
    }

    pub(in crate::commands::dashboard) fn render_info_strip(
        &self,
        frame: &mut Frame,
        info_area: Rect,
        agent: &DashboardAgent,
        _area_width: u16,
    ) {
        // Task line: truncated original prompt
        let max_task = (info_area.width as usize).saturating_sub(10);
        let task_display = truncate(&agent.task, max_task);
        let task_line = Line::from(vec![
            Span::styled(" task  ", Style::default().fg(C_DIM)),
            Span::styled(task_display, Style::default().fg(C_MUTED)),
        ]);

        // Stats line: workdir · per-stage tokens · total tokens [· model]
        let home = dirs::home_dir()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_default();
        let workdir_display = if !home.is_empty() && agent.workdir.starts_with(&home) {
            format!("~{}", &agent.workdir[home.len()..])
        } else {
            agent.workdir.clone()
        };
        let workdir_truncated = truncate(&workdir_display, 42);

        let stage_tok_part = agent
            .stages
            .get(self.selected_stage)
            .filter(|s| s.prompt_tokens > 0 || s.completion_tokens > 0)
            .map(|s| {
                format!(
                    "  ·  stage {}↑ {}↓",
                    format_tokens(s.prompt_tokens),
                    format_tokens(s.completion_tokens)
                )
            })
            .unwrap_or_default();

        let total_tok_part = if agent.tokens_in > 0 || agent.tokens_out > 0 {
            let cache_part = if agent.cached_tokens > 0 && agent.tokens_in > 0 {
                let pct = (agent.cached_tokens as f64 / agent.tokens_in as f64) * 100.0;
                format!("  cache {:.0}%", pct)
            } else {
                String::new()
            };
            format!(
                "  ·  total {}↑ {}↓{}",
                format_tokens(agent.tokens_in),
                format_tokens(agent.tokens_out),
                cache_part,
            )
        } else {
            String::new()
        };

        let model_part = agent
            .model
            .as_deref()
            .map(|m| format!("  ·  {}", truncate(m, 24)))
            .unwrap_or_default();

        let stats_line = Line::from(vec![
            Span::styled(" dir   ", Style::default().fg(C_DIM)),
            Span::styled(workdir_truncated, Style::default().fg(C_MUTED)),
            Span::styled(stage_tok_part, Style::default().fg(C_DIM)),
            Span::styled(total_tok_part, Style::default().fg(C_DIM)),
            Span::styled(model_part, Style::default().fg(C_MUTED)),
        ]);

        frame.render_widget(
            Paragraph::new(vec![task_line, stats_line]).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(C_BORDER))
                    .padding(Padding::horizontal(1)),
            ),
            info_area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::make_test_dashboard;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn make_test_agent(id: &str, status: AgentDisplayStatus) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "test-agent".to_string(),
            agent_path: "/path".to_string(),
            stage: "main".to_string(),
            stage_index: 0,
            num_stages: 2,
            status,
            tokens_in: 100,
            tokens_out: 50,
            cached_tokens: 10,
            context_tokens: (500, 8000),
            iteration: 3,
            waiting_prompt: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            entity: bevy_ecs::prelude::Entity::from_raw(0),
            is_run_state: true,
            pid: 0,
            workdir: "/tmp/test".to_string(),
            task: "test task".to_string(),
            title: Some("My Test".to_string()),
            model: Some("claude-sonnet-4-20250514".to_string()),
            parent_id: None,
            depth: 0,
            started_at: chrono::Utc::now().timestamp() - 60,
            active_until: None,
            waiting_secs: 0,
            graph_info: None,
            accepts_messages: true,
            taint_summary: vec![],
        }
    }

    #[test]
    fn render_header_breadcrumb_active() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-abc-123", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.render_header_breadcrumb(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn render_header_breadcrumb_waiting() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-w", AgentDisplayStatus::Waiting);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.render_header_breadcrumb(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn render_header_breadcrumb_complete() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-c", AgentDisplayStatus::Complete);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.render_header_breadcrumb(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn render_header_breadcrumb_error() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-e", AgentDisplayStatus::Error("boom".to_string()));
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.render_header_breadcrumb(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn render_header_breadcrumb_cancelled() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-x", AgentDisplayStatus::Cancelled);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.render_header_breadcrumb(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn render_header_breadcrumb_without_title() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-no-title", AgentDisplayStatus::Active);
        agent.title = None;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.render_header_breadcrumb(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn render_header_breadcrumb_accepts_messages_false() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-no-msg", AgentDisplayStatus::Active);
        agent.accepts_messages = false;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.render_header_breadcrumb(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn render_header_breadcrumb_with_active_until() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-frozen", AgentDisplayStatus::Waiting);
        agent.active_until = Some(chrono::Utc::now().timestamp() - 30);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.render_header_breadcrumb(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn render_info_strip_basic() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-info", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 4);
                dash.render_info_strip(f, area, &agent, 120);
            })
            .unwrap();
    }

    #[test]
    fn render_info_strip_with_stage_tokens() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-stok", AgentDisplayStatus::Active);
        agent.stages.push(crate::runstate::StageRecord {
            name: "main".to_string(),
            index: 0,
            status: crate::runstate::StageRunStatus::Active,
            prompt_tokens: 200,
            completion_tokens: 80,
            cached_tokens: 0,
            started_at: Some(chrono::Utc::now().timestamp() - 30),
            ended_at: None,
        });
        dash.selected_stage = 0;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 4);
                dash.render_info_strip(f, area, &agent, 120);
            })
            .unwrap();
    }

    #[test]
    fn render_info_strip_with_model() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-model", AgentDisplayStatus::Active);
        agent.model = Some("claude-sonnet-4-20250514".to_string());
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 4);
                dash.render_info_strip(f, area, &agent, 120);
            })
            .unwrap();
    }

    #[test]
    fn render_info_strip_no_model() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-nomodel", AgentDisplayStatus::Active);
        agent.model = None;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 4);
                dash.render_info_strip(f, area, &agent, 120);
            })
            .unwrap();
    }

    #[test]
    fn render_info_strip_home_dir_substitution() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-home", AgentDisplayStatus::Active);
        let home = dirs::home_dir().unwrap_or(std::path::PathBuf::from("/home/user"));
        agent.workdir = format!("{}/projects/test", home.to_string_lossy());
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 4);
                dash.render_info_strip(f, area, &agent, 120);
            })
            .unwrap();
    }

    #[test]
    fn render_info_strip_zero_tokens() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-zero", AgentDisplayStatus::Active);
        agent.tokens_in = 0;
        agent.tokens_out = 0;
        agent.cached_tokens = 0;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 4);
                dash.render_info_strip(f, area, &agent, 120);
            })
            .unwrap();
    }

    #[test]
    fn render_info_strip_tokens_present_without_cache() {
        // tokens_in > 0 but cached_tokens == 0: total_tok_part is shown but
        // the cache-percentage sub-part must stay empty.
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-no-cache", AgentDisplayStatus::Active);
        agent.tokens_in = 100;
        agent.tokens_out = 50;
        agent.cached_tokens = 0;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 4);
                dash.render_info_strip(f, area, &agent, 120);
            })
            .unwrap();
    }
}

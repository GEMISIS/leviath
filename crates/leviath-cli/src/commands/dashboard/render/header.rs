//! Header breadcrumb and info strip rendering.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
use ratatui::Frame;

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

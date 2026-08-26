//! Header breadcrumb and info strip rendering.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};

use crate::commands::dashboard::helpers::{format_tokens, truncate};
use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::theme::*;
use crate::commands::dashboard::types::*;
use leviath_core::duration;

impl Dashboard {
    pub(in crate::commands::dashboard) fn render_header_breadcrumb(
        &self,
        frame: &mut Frame,
        hdr_area: Rect,
        agent: &DashboardAgent,
    ) {
        // Both spans, because they answer different questions and a run can look
        // very different under each: how long it has been working, and how long
        // it has existed. A run that has sat paused since yesterday reads `12m
        // work · 19h old`, which is the whole story in two figures.
        let elapsed = duration::precise(agent.runtime_secs);
        let age = duration::compact(duration::between(agent.started_at, agent.clock_now));
        let status_color = agent.status.color();
        let spinner_frame = SPINNER[(self.tick_count as usize) % SPINNER.len()];
        let status_span = match &agent.status {
            AgentDisplayStatus::Active => Span::styled(
                format!("{} {} ", spinner_frame, agent.status),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
            // Cap the status text so a long error message (the full text lives in
            // the Output pane) can't consume the whole line and push the run id
            // off the right edge.
            _ => Span::styled(
                format!("{} ", truncate(&agent.status.to_string(), 28)),
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
            Span::styled(format!(" · {} work", elapsed), Style::default().fg(C_DIM)),
            Span::styled(format!(" · {} old ", age), Style::default().fg(C_DIM)),
            if agent.accepts_messages {
                Span::styled("💬 ", Style::default().fg(C_SUCCESS))
            } else {
                Span::styled("🔇 ", Style::default().fg(C_DIM))
            },
            Span::styled("· ", Style::default().fg(C_DIM)),
            Span::styled(agent.id.clone(), Style::default().fg(C_DIM)),
            // A run whose script could not be used looks exactly like a healthy
            // one otherwise: a broken output validator is skipped rather than
            // fatal, so the run completes and reports success. This is the only
            // place that says the result went unchecked.
            match agent.broken_scripts.len() {
                0 => Span::raw(""),
                n => Span::styled(
                    format!(" · ⚠ {n} broken script{}", if n == 1 { "" } else { "s" }),
                    Style::default().fg(C_WARN).add_modifier(Modifier::BOLD),
                ),
            },
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
        // Display-only `~` abbreviation of the OS home directory; deliberately
        // NOT the LEVIATH_HOME-aware resolver, which points at Leviath's data
        // root rather than the home the user reads paths against.
        let home = dirs::home_dir()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_default();
        // One operation rather than a guard plus a byte offset, so the test and
        // the cut cannot disagree about where the prefix ended.
        let workdir_display = match agent.workdir.strip_prefix(&home) {
            Some(rest) if !home.is_empty() => format!("~{rest}"),
            _ => agent.workdir.clone(),
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
    use crate::commands::dashboard::test_support::{make_test_dashboard, rendered_buffer};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn make_test_agent(id: &str, status: AgentDisplayStatus) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "test-agent".to_string(),
            stage: "main".to_string(),
            stage_index: 0,
            num_stages: 2,
            status,
            tokens_in: 100,
            tokens_out: 50,
            cached_tokens: 10,
            iteration: 3,
            broken_scripts: Vec::new(),
            waiting_prompt: None,
            wait_reason: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            workdir: "/tmp/test".to_string(),
            task: "test task".to_string(),
            title: Some("My Test".to_string()),
            model: Some("claude-sonnet-4-20250514".to_string()),
            parent_id: None,
            depth: 0,
            started_at: chrono::Utc::now().timestamp() - 60,
            last_progress_at: None,
            runtime_secs: 0,
            clock_now: 0,
            graph: None,
            accepts_messages: true,
            taint_summary: vec![],
        }
    }

    /// A run whose script could not be used looks exactly like a healthy one
    /// otherwise - it completes and reports success - so this badge is the only
    /// thing on screen that says the result went unchecked.
    #[test]
    fn render_header_breadcrumb_flags_a_broken_script() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-broken", AgentDisplayStatus::Complete);
        agent.broken_scripts = vec!["shape.rhai".to_string()];
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.render_header_breadcrumb(f, area, &agent);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("1 broken script"), "{buf}");
        assert!(!buf.contains("scripts"), "singular for one: {buf}");
    }

    /// Two of them read as two.
    #[test]
    fn render_header_breadcrumb_pluralises_broken_scripts() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-broken", AgentDisplayStatus::Complete);
        agent.broken_scripts = vec!["a.rhai".to_string(), "b.rhai".to_string()];
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.render_header_breadcrumb(f, area, &agent);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("2 broken scripts"), "{buf}");
    }

    /// And a healthy run carries no badge at all.
    #[test]
    fn render_header_breadcrumb_is_quiet_when_nothing_is_broken() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-ok", AgentDisplayStatus::Complete);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.render_header_breadcrumb(f, area, &agent);
            })
            .unwrap();
        assert!(!rendered_buffer(&terminal).contains("broken"));
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("ACTIVE"), "{buf}");
        assert!(!buf.contains("WAITING"), "{buf}");
        assert!(!buf.contains("COMPLETE"), "{buf}");
        assert!(!buf.contains("CANCEL"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("WAITING"), "{buf}");
        assert!(!buf.contains("ACTIVE"), "{buf}");
        assert!(!buf.contains("COMPLETE"), "{buf}");
        assert!(!buf.contains("CANCEL"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("COMPLETE"), "{buf}");
        assert!(!buf.contains("ACTIVE"), "{buf}");
        assert!(!buf.contains("WAITING"), "{buf}");
        assert!(!buf.contains("CANCEL"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("ERROR: boom"), "{buf}");
        assert!(!buf.contains("ACTIVE"), "{buf}");
        assert!(!buf.contains("WAITING"), "{buf}");
        assert!(!buf.contains("COMPLETE"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("CANCEL"), "{buf}");
        assert!(!buf.contains("ACTIVE"), "{buf}");
        assert!(!buf.contains("WAITING"), "{buf}");
        assert!(!buf.contains("COMPLETE"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("test-agent"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("ACTIVE"), "{buf}");
    }

    #[test]
    fn render_header_breadcrumb_shows_working_time_not_age() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-frozen", AgentDisplayStatus::Waiting);
        // Alive for an hour, at work for 30 seconds of it.
        agent.started_at = chrono::Utc::now().timestamp() - 3600;
        agent.runtime_secs = 30;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.render_header_breadcrumb(f, area, &agent);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("WAITING"), "{buf}");
        assert!(buf.contains("30s"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("test task"), "{buf}");
    }

    #[test]
    fn render_info_strip_with_stage_tokens() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-stok", AgentDisplayStatus::Active);
        agent.stages.push(crate::runstate::StageRecord {
            active: Default::default(),
            name: "main".to_string(),
            index: 0,
            status: crate::runstate::StageRunStatus::Active,
            entered: true,
            prompt_tokens: 200,
            completion_tokens: 80,
            cached_tokens: 0,
            cache_write_tokens: 0,
            region_tokens: Default::default(),
            first_call_prompt_tokens: None,
            runaway_warned: false,
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("test task"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("claude-sonnet-4"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("test task"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("~/projects/test"), "{buf}");
        // The point of the substitution: the real home path is gone, not just
        // accompanied by a tilde somewhere on the line.
        assert!(!buf.contains(&*home.to_string_lossy()), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("test task"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("test task"), "{buf}");
    }
}

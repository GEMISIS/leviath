//! Header breadcrumb and info strip rendering.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::commands::dashboard::helpers::{fit_parts, format_tokens, truncate};
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
        let status_style = Style::default()
            .fg(agent.status.color())
            .add_modifier(Modifier::BOLD);
        let spinner_frame = SPINNER[(self.tick_count as usize) % SPINNER.len()];
        let status_text = match &agent.status {
            AgentDisplayStatus::Active => format!("{} {}", spinner_frame, agent.status),
            other => other.to_string(),
        };
        let raw_title = agent.title.as_deref().unwrap_or(&agent.blueprint_name);
        let title_text = raw_title.trim_start_matches('#').trim();
        let dim = Style::default().fg(C_DIM);
        // A run whose script could not be used looks exactly like a healthy
        // one otherwise: a broken output validator is skipped rather than
        // fatal, so the run completes and reports success. This is the only
        // place that says the result went unchecked.
        let broken = match agent.broken_scripts.len() {
            0 => String::new(),
            n => format!(" · ⚠ {n} broken script{}", if n == 1 { "" } else { "s" }),
        };
        let (msg_glyph, msg_style) = if agent.accepts_messages {
            ("💬 ", Style::default().fg(C_SUCCESS))
        } else {
            ("🔇 ", dim)
        };
        // The line as text, then fitted to the row: the title gives way first,
        // then the status (a long error message lives in full in the Output
        // pane), and nothing else moves, so the run id stays on screen.
        let parts: Vec<(String, Style)> = vec![
            (" ".to_string(), dim),
            (
                title_text.to_string(),
                Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
            ),
            (" · ".to_string(), dim),
            (status_text, status_style),
            (" · ".to_string(), dim),
            (format!("{}↑", format_tokens(agent.tokens_in)), dim),
            (format!(" {}↓", format_tokens(agent.tokens_out)), dim),
            (format!(" · {} work", elapsed), dim),
            (format!(" · {} old ", age), dim),
            (msg_glyph.to_string(), msg_style),
            ("· ".to_string(), dim),
            (agent.id.clone(), dim),
            (
                broken,
                Style::default().fg(C_WARN).add_modifier(Modifier::BOLD),
            ),
        ];
        let texts: Vec<String> = parts.iter().map(|(t, _)| t.clone()).collect();
        let spans: Vec<Span> = fit_parts(&texts, hdr_area.width as usize, &[1, 3])
            .into_iter()
            .zip(parts.iter().map(|(_, style)| *style))
            .map(|(text, style)| Span::styled(text, style))
            .collect();
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Rgb(20, 20, 30))),
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
        // The room inside the border and its one column of padding each side.
        let inner_width = (info_area.width as usize).saturating_sub(4);

        // Task line: the original prompt, cut only where it does not fit
        // after its seven-column label.
        let task_display = truncate(&agent.task, inner_width.saturating_sub(7));
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
            // `tokens_in` is the fresh input only: every provider is
            // normalised to Anthropic's convention, where cache reads are a
            // separate count and not part of it (see
            // `TokenUsage::prompt_tokens`). The share of the prompt served
            // from cache is therefore reads over fresh plus reads, which
            // cannot pass 100%; over fresh alone it read 164% on a run that
            // was mostly cache hits.
            let cache_part = if agent.cached_tokens > 0 {
                let prompt = agent.tokens_in.saturating_add(agent.cached_tokens);
                let pct = (agent.cached_tokens as f64 / prompt as f64) * 100.0;
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

        let (model_sep, model_part) = match agent.model.as_deref() {
            Some(m) => ("  ·  ".to_string(), m.to_string()),
            None => (String::new(), String::new()),
        };

        // Everything shown in full when the row has the room. When it does
        // not, the model gives way first and the workdir second; the token
        // figures are the numbers the line exists for and never shrink.
        let mut parts = vec![
            " dir   ".to_string(),
            workdir_display,
            stage_tok_part,
            total_tok_part,
            model_sep,
            model_part,
        ];
        let mut fitted = fit_parts(&parts, inner_width, &[5, 1]);
        // A row too narrow for both at their floors drops the model rather
        // than clip the token figures off the right edge, and the workdir
        // gets the room back.
        if fitted.iter().map(|p| p.width()).sum::<usize>() > inner_width {
            parts[4].clear();
            parts[5].clear();
            fitted = fit_parts(&parts, inner_width, &[1]);
        }
        let colors = [C_DIM, C_MUTED, C_DIM, C_DIM, C_MUTED, C_MUTED];
        let spans: Vec<Span> = fitted
            .into_iter()
            .zip(colors)
            .map(|(text, color)| Span::styled(text, Style::default().fg(color)))
            .collect();
        let stats_line = Line::from(spans);

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
            started_at: chrono::Utc::now().timestamp() - 60,
            last_progress_at: None,
            runtime_secs: 0,
            clock_now: 0,
            graph: None,
            accepts_messages: true,
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
            status: crate::runstate::StageRunStatus::Active,
            entered: true,
            prompt_tokens: 200,
            completion_tokens: 80,
            started_at: Some(chrono::Utc::now().timestamp() - 30),
            ..crate::runstate::StageRecord::new("main".to_string(), 0)
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

    // ── Fitting to the row ─────────────────────────────────────────────────

    const LONG_MODEL: &str = "openrouter/z-ai/glm-5.3-preview-long-name";
    const LONG_WORKDIR: &str = "/srv/projects/ai/personal/leviath/worktrees/fit-to-width";

    fn wide_agent() -> DashboardAgent {
        let mut agent = make_test_agent("run-wide", AgentDisplayStatus::Active);
        agent.model = Some(LONG_MODEL.to_string());
        agent.workdir = LONG_WORKDIR.to_string();
        agent.tokens_in = 1_000_000;
        agent.tokens_out = 25_000;
        agent.cached_tokens = 410_000;
        agent
    }

    fn info_strip_at(width: u16, agent: &DashboardAgent) -> String {
        let backend = TestBackend::new(width, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, width, 4);
                dash.render_info_strip(f, area, agent, width);
            })
            .unwrap();
        rendered_buffer(&terminal)
    }

    /// The defect: a run that is mostly served from cache showed a cache
    /// figure over 100%. Every provider is normalised so that `tokens_in` is
    /// the FRESH input only (see `TokenUsage::prompt_tokens`), so dividing
    /// the cache reads by it is not a share of anything. The numbers are a
    /// real researcher run's `meta.json`: 555,075 fresh, 909,343 cached,
    /// which the strip showed as `cache 164%`.
    #[test]
    fn render_info_strip_cache_share_is_a_share_of_the_whole_prompt() {
        let mut agent = wide_agent();
        agent.tokens_in = 555_075;
        agent.tokens_out = 58_267;
        agent.cached_tokens = 909_343;
        let buf = info_strip_at(200, &agent);
        assert!(buf.contains("total 555k↑ 58k↓  cache 62%"), "{buf}");
        assert!(!buf.contains("164%"), "{buf}");
        // Everything cached is the ceiling, not a division by zero.
        agent.tokens_in = 0;
        agent.cached_tokens = 4_000;
        let buf = info_strip_at(200, &agent);
        assert!(buf.contains("total 0↑ 58k↓  cache 100%"), "{buf}");
    }

    /// The defect: the model was cut to 24 characters on a 200-column
    /// terminal with most of the row empty.
    #[test]
    fn render_info_strip_wide_shows_the_whole_model_and_workdir() {
        let buf = info_strip_at(200, &wide_agent());
        assert!(buf.contains(LONG_MODEL), "{buf}");
        assert!(buf.contains(LONG_WORKDIR), "{buf}");
        assert!(buf.contains("total 1M↑ 25k↓  cache 29%"), "{buf}");
        assert!(!buf.contains('…'), "{buf}");
    }

    /// Narrow: the model gives way first, the workdir keeps as much as the
    /// model's floor leaves it, and the token figures stay whole.
    #[test]
    fn render_info_strip_narrow_shrinks_model_before_workdir_and_keeps_tokens() {
        let buf = info_strip_at(70, &wide_agent());
        assert!(buf.contains("total 1M↑ 25k↓  cache 29%"), "{buf}");
        assert!(!buf.contains(LONG_MODEL), "{buf}");
        assert!(!buf.contains(LONG_WORKDIR), "{buf}");
        // The model sits at the floor (seven characters and the ellipsis).
        assert!(buf.contains("·  openrou…"), "{buf}");
        // The workdir gave up the rest and kept its head.
        assert!(buf.contains("dir   /srv/projects/a…  ·  total"), "{buf}");
    }

    /// Narrower than both floors allow: the model is dropped altogether
    /// rather than clipping the token figures, and the workdir takes the
    /// room the model's floor was holding.
    #[test]
    fn render_info_strip_below_the_floors_drops_the_model_and_keeps_the_tokens() {
        // 60 wide: 56 inside, 7 for the label and 30 for the totals leaves
        // the workdir 19 columns.
        let buf = info_strip_at(60, &wide_agent());
        assert!(
            buf.contains("dir   /srv/projects/ai/p…  ·  total 1M↑ 25k↓  cache 29%"),
            "{buf}"
        );
        assert!(!buf.contains("openro"), "{buf}");
        // The mid case, where the model at its floor would have fitted the
        // arithmetic of the parts but not the row: 50 wide drops it too.
        let buf = info_strip_at(50, &wide_agent());
        assert!(buf.contains("total 1M↑ 25k↓  cache 29%"), "{buf}");
        assert!(!buf.contains("openro"), "{buf}");
    }

    /// With a little less room than the whole line, only the model is cut and
    /// the workdir is untouched: the shrink order is model, then workdir.
    #[test]
    fn render_info_strip_slightly_narrow_cuts_only_the_model() {
        let buf = info_strip_at(120, &wide_agent());
        assert!(buf.contains(LONG_WORKDIR), "{buf}");
        assert!(buf.contains("openrouter/z-ai/g…"), "{buf}");
        assert!(!buf.contains(LONG_MODEL), "{buf}");
    }

    /// The task line gets the whole inner width too.
    #[test]
    fn render_info_strip_task_uses_the_row() {
        let mut agent = wide_agent();
        agent.task = "t".repeat(150);
        let buf = info_strip_at(200, &agent);
        assert!(buf.contains(&"t".repeat(150)), "{buf}");
        let narrow = info_strip_at(60, &agent);
        // 60 wide, 4 for border and padding, 7 for the label: 49 columns.
        assert!(narrow.contains(&format!("{}…", "t".repeat(48))), "{narrow}");
        assert!(!narrow.contains(&"t".repeat(49)), "{narrow}");
    }

    /// The header row: at 200 columns a long title and the run id are both
    /// whole; at 80 the title gives way and the id stays.
    #[test]
    fn render_header_breadcrumb_fits_the_title_to_the_row() {
        let long_title = "Add adversarial and cross-company risk analysis to the blueprint";
        let mut agent = make_test_agent("run-abc-123456", AgentDisplayStatus::Complete);
        agent.title = Some(long_title.to_string());

        let backend = TestBackend::new(200, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        terminal
            .draw(|f| dash.render_header_breadcrumb(f, Rect::new(0, 0, 200, 1), &agent))
            .unwrap();
        let wide = rendered_buffer(&terminal);
        assert!(wide.contains(long_title), "{wide}");
        assert!(wide.contains("run-abc-123456"), "{wide}");
        assert!(!wide.contains('…'), "{wide}");

        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| dash.render_header_breadcrumb(f, Rect::new(0, 0, 80, 1), &agent))
            .unwrap();
        let narrow = rendered_buffer(&terminal);
        assert!(!narrow.contains(long_title), "{narrow}");
        assert!(narrow.contains("Add adversarial"), "{narrow}");
        assert!(narrow.contains('…'), "{narrow}");
        assert!(narrow.contains("run-abc-123456"), "{narrow}");
        assert!(narrow.contains("COMPLETE"), "{narrow}");
    }

    /// A long error status is the second thing to give way, after the title.
    #[test]
    fn render_header_breadcrumb_cuts_a_long_error_after_the_title() {
        let mut agent = make_test_agent(
            "run-err-1",
            AgentDisplayStatus::Error(
                "provider returned 502 after three attempts and the breaker opened".to_string(),
            ),
        );
        agent.title = Some("Short".to_string());
        let backend = TestBackend::new(70, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        terminal
            .draw(|f| dash.render_header_breadcrumb(f, Rect::new(0, 0, 70, 1), &agent))
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(
            buf.contains("Short"),
            "title is already at the floor: {buf}"
        );
        assert!(buf.contains("ERROR: "), "{buf}");
        assert!(!buf.contains("breaker opened"), "{buf}");
        assert!(buf.contains("run-err-1"), "{buf}");
    }
}

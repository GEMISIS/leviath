//! Toast notifications, help overlay, and confirmation popup rendering.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::commands::dashboard::helpers::truncate;
use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::theme::*;
use crate::commands::dashboard::types::MainPane;
use crate::tui::widgets::help::{HelpSection, draw_help};
use crate::tui::widgets::markdown_edit::{MODE_CHORD, shortcut_help};

impl Dashboard {
    pub(in crate::commands::dashboard) fn draw_toasts(&self, frame: &mut Frame) {
        if self.toasts.is_empty() {
            return;
        }
        let area = frame.area();
        let toast_w: u16 = 40;
        let toast_h: u16 = self.toasts.len() as u16;
        let x = area.width.saturating_sub(toast_w + 1);
        let y: u16 = 1;
        let toast_area = Rect {
            x,
            y,
            width: toast_w,
            height: toast_h,
        };
        frame.render_widget(Clear, toast_area);
        for (i, toast) in self.toasts.iter().enumerate() {
            let color = match toast.level {
                super::super::types::ToastLevel::Info => C_SUCCESS,
                super::super::types::ToastLevel::Warning => C_WARN,
                super::super::types::ToastLevel::Error => C_ERROR,
            };
            let icon = match toast.level {
                super::super::types::ToastLevel::Info => "✓",
                super::super::types::ToastLevel::Warning => "⏸",
                super::super::types::ToastLevel::Error => "✗",
            };
            let msg = truncate(&toast.message, (toast_w - 4) as usize);
            let line = Line::from(vec![
                Span::styled(
                    format!(" {} ", icon),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(msg, Style::default().fg(C_WHITE)),
            ]);
            let row = Rect {
                x,
                y: y + i as u16,
                width: toast_w,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(line).style(Style::default().bg(Color::Rgb(30, 30, 30))),
                row,
            );
        }
    }

    /// The page-scoped help overlay, built from `(key, description)` tables
    /// via the shared builder so it cannot drift from the bindings.
    ///
    /// Every mode ends with the same three shared sections. Dialogs and the
    /// mouse are not one screen's business, and `Ctrl-C` is the only key that
    /// works in all of them, which is worth saying where somebody is looking
    /// rather than only where it happens to have been listed.
    pub(in crate::commands::dashboard) fn draw_help_overlay(&self, frame: &mut Frame) {
        let mut sections: Vec<HelpSection> = if let Some(screen) = self.agent_builder.as_deref() {
            if screen.editor.is_some() {
                agent_editor_sections()
            } else {
                agents_sections()
            }
        } else if self.new_run_screen {
            new_run_sections()
        } else if self.mcp_screen {
            mcp_sections()
        } else if self.detail_view {
            detail_sections()
        } else if self.main_focus == MainPane::LogPane {
            log_pane_sections()
        } else {
            run_list_sections()
        };
        sections.extend(shared_sections());
        draw_help(frame, frame.area(), &sections, &self.help_scroll);
    }
}

/// The Agents screen's catalog.
fn agents_sections() -> Vec<HelpSection> {
    vec![
        HelpSection {
            title: "Agents (a)",
            entries: vec![
                ("↑ ↓ / k j", "select an agent"),
                ("home / end, pgup / pgdn", "first / last, a page at a time"),
                ("wheel", "over the list: move; over the preview: zoom"),
                ("enter / e", "edit it (a bundled one installs when saved)"),
                ("n", "new agent: start simple, or clone one"),
                ("l", "launch it: the new-run screen with it picked"),
                (
                    "r",
                    "rename an installed agent: its directory and its manifest's name",
                ),
                ("d", "delete an installed agent (asks first)"),
                (
                    "R",
                    "reset an edited bundled agent to the bundled copy (asks first)",
                ),
                (
                    "/",
                    "filter by name or description; enter keeps it, esc clears it",
                ),
                ("esc / q", "back to the run list"),
                ("? / F1", "this help"),
            ],
        },
        HelpSection {
            title: "New agent (n)",
            entries: vec![
                (
                    "↑ ↓",
                    "pick the template; the name follows until you type one",
                ),
                ("enter", "open the editor on it (not saved until you save)"),
                ("esc", "back to the catalog"),
            ],
        },
    ]
}

/// The agent editor.
fn agent_editor_sections() -> Vec<HelpSection> {
    vec![
        HelpSection {
            title: "Agent editor: everywhere",
            entries: vec![
                (
                    "ctrl-s",
                    "save (checks first; errors block it and open the problems)",
                ),
                ("tab", "move the keys between the canvas and the inspector"),
                (
                    "ctrl-z / ctrl-y",
                    "undo / redo an edit (ctrl-shift-z redoes too)",
                ),
                (
                    "v",
                    "the definition: the exact file that will be saved (y copies it)",
                ),
                ("p", "open / close the problems list under the canvas"),
                (
                    "esc",
                    "on the canvas: close (asks when there are unsaved edits)",
                ),
            ],
        },
        HelpSection {
            title: "Agent editor: canvas",
            entries: vec![
                ("↑ ↓ ← → / h j k l", "select a stage in that direction"),
                ("[ / ]", "previous / next stage in file order"),
                ("enter", "edit the selected stage or path in the inspector"),
                ("a", "add a stage after the selected one (asks its name)"),
                ("c", "connect the selected stage to another (or to itself)"),
                ("x / delete", "delete the selected stage (asks) or path"),
                ("+ / - / 0, f, r", "zoom, fit, turn the graph"),
                ("drag a box", "move it (the arrangement is kept per agent)"),
                ("drag a ●", "connect two stages with the mouse"),
                (
                    "click",
                    "select a stage or a path; on empty canvas, select nothing",
                ),
                (
                    "right-click",
                    "a menu for the stage, path or canvas under the pointer",
                ),
            ],
        },
        HelpSection {
            title: "Agent editor: inspector",
            entries: vec![
                ("↑ ↓ / k j, home / end", "move between rows"),
                (
                    "enter",
                    "edit the row: type, choose, flip, open, or press the button",
                ),
                (
                    "← → / h l",
                    "change the row in place: cycle a choice, step a number, flip a toggle",
                ),
                ("1 2 3", "a stage's tabs: behaviour, model & tools, context"),
                (
                    "x / backspace",
                    "remove the row: a model from the chain, a routing rule",
                ),
                ("← → on a model", "move it earlier or later in the chain"),
                (
                    "esc",
                    "back: a region or a loop's path returns to where it was opened from, otherwise the canvas",
                ),
                (
                    "click",
                    "pick a row (again to open it); click a tab to switch",
                ),
            ],
        },
        HelpSection {
            title: "Agent editor: choosers and the definition",
            entries: vec![
                (
                    "type",
                    "search the list; ↑ ↓ move; enter chooses, esc cancels",
                ),
                (
                    "space",
                    "in the tools chooser: pick or drop a row; enter keeps the picks",
                ),
                ("↑ ↓, y", "in the definition: scroll, copy it"),
            ],
        },
        HelpSection {
            title: "Agent editor: prompts",
            entries: vec![
                (
                    "tab",
                    "move between the system prompt and the transition prompt",
                ),
                ("ctrl-s / esc", "apply both and close"),
                ("ctrl-q", "close without applying"),
                (
                    "F2",
                    "open the focused prompt in $EDITOR (the dashboard waits for it)",
                ),
                ("F1", "this help (? types a question mark in a prompt)"),
            ],
        },
        formatting_section(),
    ]
}

/// The long-form editor's formatting chords.
///
/// Shown on every screen that has one of those boxes, because the box is the
/// same component in all of them. Every chord has a button on the box's
/// toolbar as well, which is the path that still works on a terminal that
/// cannot report the chord at all.
fn formatting_section() -> HelpSection {
    let mut entries = vec![(
        MODE_CHORD,
        "switch between the markdown you write and how it will read",
    )];
    entries.extend(shortcut_help());
    entries.push((
        "click",
        "the same actions, from the button row along the top of the box",
    ));
    entries.push((
        "hover",
        "the bottom border names the button under the pointer",
    ));
    entries.push((
        "shift + ← →",
        "select first, to wrap text that is already written",
    ));
    entries.push(("ctrl-z", "undo; ctrl-shift-z or ctrl-r redoes"));
    HelpSection {
        title: "Formatting a long-form box",
        entries,
    }
}

/// The run list: the screen `lev dash` opens on.
fn run_list_sections() -> Vec<HelpSection> {
    vec![HelpSection {
        title: "Agent run list",
        entries: vec![
            ("↑ ↓ / k j", "select a run"),
            ("home / end (g / G)", "first / last run"),
            ("enter", "open the detail view"),
            (
                "← →",
                "fold / unfold a run's sub-agents (the ▸ ▾ arrow is clickable too)",
            ),
            (
                "",
                "on a run with none: ← goes up to the parent, → down to the first child",
            ),
            ("n", "start a new run"),
            ("tab / shift-tab", "focus the log panel"),
            ("/", "filter runs; enter keeps it, esc clears it"),
            ("s", "cycle sort: started / activity / status"),
            ("p / r", "pause / resume the selected run"),
            ("x", "kill the run (asks first)"),
            ("d", "delete the run and its files (asks first)"),
            (
                "space",
                "mark/unmark the run, then move down (marked runs are killed/deleted together)",
            ),
            ("m", "manage MCP servers"),
            ("a", "agents: the catalog and the editor"),
            ("esc", "clear the filter, then the marks"),
            ("q", "quit"),
        ],
    }]
}

/// The log panel, once Tab has given it the keys.
fn log_pane_sections() -> Vec<HelpSection> {
    vec![HelpSection {
        title: "Log panel",
        entries: vec![
            ("↑ ↓ / k j", "scroll a line"),
            ("pgup / pgdn", "scroll a screen"),
            ("home / g", "oldest line"),
            ("end / G", "newest line, and follow again"),
            ("tab / shift-tab / esc", "back to the run list"),
            ("q", "quit"),
        ],
    }]
}

/// The detail view, its Context sub-view, and the response editor it opens.
fn detail_sections() -> Vec<HelpSection> {
    vec![
        HelpSection {
            title: "Detail view",
            entries: vec![
                ("← →", "switch stage tab"),
                ("1-9", "jump to a stage by number"),
                ("l / o / c", "logs / output / context"),
                ("↑ ↓ / k j", "scroll a line"),
                ("pgup / pgdn", "scroll ten lines"),
                ("home / end (b / e)", "top / bottom"),
                ("/", "search; n / N step through matches"),
                ("y", "copy this pane to the clipboard"),
                ("i", "respond, or send a message to a running agent run"),
                ("g", "the stage graph explorer"),
                (", / .", "older / newer context point; opens Context"),
                ("x", "kill the run (asks first)"),
                ("p / r", "pause / resume"),
                ("esc", "clear the search, then back to the list"),
                ("ctrl-c", "quit; q does nothing here"),
            ],
        },
        HelpSection {
            title: "Context view (c)",
            entries: vec![
                ("↑ ↓ / k j", "move the tree cursor, not the scroll"),
                ("enter / space", "fold or unfold a row"),
                ("click", "the same, on the row under the pointer"),
                ("[ / ]", "previous / next region"),
            ],
        },
        HelpSection {
            title: "Stage explorer (g)",
            entries: vec![
                ("tab / shift-tab", "graph / timeline"),
                (
                    "← → ↑ ↓ / h j k l",
                    "graph: select a stage in that direction",
                ),
                ("[ / ]", "graph: previous / next stage in blueprint order"),
                (
                    "enter",
                    "graph: open the stage's tab; timeline: open the visit's context",
                ),
                ("+ / - / 0", "zoom in / out / back to 100%"),
                ("f", "fit the whole graph on screen"),
                ("r", "turn the graph: left to right / top to bottom"),
                (
                    "t",
                    "the whole graph, or only the path taken and what comes next",
                ),
                ("e", "show or hide the escape edges (error, dead_end, ...)"),
                ("↑ ↓ / k j", "timeline: step through visits"),
                ("drag", "move a box; on empty canvas, pan"),
                ("wheel / click", "zoom / select on the canvas"),
                ("esc / g", "close the explorer"),
                (
                    "",
                    "the detail view's keys are off while it is open (e, c, l, o, b included)",
                ),
            ],
        },
        HelpSection {
            title: "Writing a response (i)",
            entries: vec![
                ("enter", "send"),
                ("alt+enter", "insert a newline"),
                ("pgup / pgdn", "scroll the document above the prompt"),
                ("esc", "cancel"),
                ("/quit or /exit", "end the conversation without answering"),
            ],
        },
        HelpSection {
            title: "Choosing an option (i)",
            entries: vec![
                ("↑ ↓", "choose"),
                ("enter", "send the highlighted choice"),
                ("pgup / pgdn / home / end", "scroll the document"),
                ("esc", "cancel"),
            ],
        },
        formatting_section(),
    ]
}

/// The new-run screen: two panes and a completion menu.
fn new_run_sections() -> Vec<HelpSection> {
    vec![
        HelpSection {
            title: "New run: agent blueprints",
            entries: vec![
                ("↑ ↓", "select an agent"),
                ("any letter", "filter the list"),
                ("backspace", "shorten the filter"),
                ("tab / enter", "move to the task"),
                ("esc", "clear the filter, then close"),
                ("F1", "this help (? types a question mark here)"),
            ],
        },
        HelpSection {
            title: "New run: task",
            entries: vec![
                ("enter", "start the run"),
                ("alt+enter", "insert a newline"),
                ("@", "reference a file from the working directory"),
                ("tab / esc", "back to the agent list"),
            ],
        },
        HelpSection {
            title: "New run: @ files",
            entries: vec![
                ("↑ ↓", "choose a path"),
                ("enter / tab", "insert it"),
                ("backspace", "shorten; over the @ it ends the reference"),
                ("esc", "dismiss, keeping what you typed"),
            ],
        },
        HelpSection {
            title: "New run: unattended",
            entries: vec![
                ("ctrl-y", "run without being asked to approve tool calls"),
                (
                    "",
                    "off every time this screen opens; warns before the first",
                ),
                (
                    "",
                    "it does not skip checkpoints a blueprint asks a person for",
                ),
            ],
        },
        formatting_section(),
    ]
}

/// The MCP screen and its add form.
fn mcp_sections() -> Vec<HelpSection> {
    vec![
        HelpSection {
            title: "MCP servers",
            entries: vec![
                ("↑ ↓ / k j", "move"),
                ("a", "add a server"),
                ("d", "remove (asks first)"),
                ("l", "browser login"),
                ("t", "test the connection"),
                ("r", "refresh the list"),
                ("esc", "back to the run list"),
                ("q", "quit"),
            ],
        },
        HelpSection {
            title: "Adding a server (a)",
            entries: vec![
                ("", "type: <name> <url-or-command> [args…]"),
                ("enter", "add it"),
                ("backspace", "delete a character"),
                ("esc", "cancel"),
                ("F1", "this help (? types a question mark here)"),
            ],
        },
    ]
}

/// Sections appended to every mode.
fn shared_sections() -> Vec<HelpSection> {
    vec![
        HelpSection {
            title: "Questions",
            entries: vec![
                ("← → / tab", "choose a button"),
                ("enter", "the focused button; it starts on the safe one"),
                ("y / n", "answer without moving first"),
                ("space", "tick \"don't ask again\", where offered"),
                ("esc", "decline"),
            ],
        },
        HelpSection {
            title: "Mouse",
            entries: vec![
                ("wheel", "scrolls whichever pane is under the pointer"),
                ("wheel on the run list", "moves the selection"),
                ("drag", "select text; releasing copies it"),
                (
                    "click a run",
                    "select it; the ▸ ▾ arrow folds its sub-agents",
                ),
                ("double click a run", "open its detail view"),
                ("click the log panel", "move the keyboard there, like tab"),
                ("click a stage tab", "open that stage"),
                (
                    "click [l] [o] [c]",
                    "switch the content pane, like the letter",
                ),
                ("click a context row", "fold or unfold it"),
            ],
        },
        HelpSection {
            title: "Everywhere",
            entries: vec![
                ("ctrl-c", "quit, from any screen including dialogs"),
                ("? or F1", "this help (F1 where ? types text)"),
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use crate::commands::dashboard::test_support::{make_test_dashboard, rendered_buffer};
    use crate::commands::dashboard::types::*;
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn make_test_agent(id: &str, status: AgentDisplayStatus) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "test-agent".to_string(),
            stage: "main".to_string(),
            stage_index: 0,
            num_stages: 1,
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
            model: None,
            parent_id: None,
            depth: 0,
            started_at: chrono::Utc::now().timestamp() - 60,
            last_progress_at: None,
            active_until: None,
            waiting_secs: 0,
            graph: None,
            accepts_messages: true,
            taint_summary: vec![],
        }
    }

    #[test]
    fn draw_toasts_empty() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        terminal
            .draw(|f| {
                dash.draw_toasts(f);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(
            buf.trim().is_empty(),
            "no toasts should draw nothing: {buf}"
        );
    }

    #[test]
    fn draw_toasts_info() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.toasts.push(Toast {
            message: "Agent completed".to_string(),
            remaining_ticks: 25,
            level: ToastLevel::Info,
        });
        terminal
            .draw(|f| {
                dash.draw_toasts(f);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Agent completed"), "{buf}");
    }

    #[test]
    fn draw_toasts_warning_and_error() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.toasts.push(Toast {
            message: "Needs input".to_string(),
            remaining_ticks: 25,
            level: ToastLevel::Warning,
        });
        dash.toasts.push(Toast {
            message: "Agent failed".to_string(),
            remaining_ticks: 25,
            level: ToastLevel::Error,
        });
        terminal
            .draw(|f| {
                dash.draw_toasts(f);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Needs input"), "{buf}");
        assert!(buf.contains("Agent failed"), "{buf}");
    }

    #[test]
    fn draw_help_overlay_renders() {
        let backend = TestBackend::new(120, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        terminal
            .draw(|f| {
                dash.draw_help_overlay(f);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Agent run list"), "{buf}");
    }

    #[test]
    fn draw_help_overlay_small_terminal() {
        let backend = TestBackend::new(40, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        terminal
            .draw(|f| {
                dash.draw_help_overlay(f);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Agent run list"), "{buf}");
    }

    #[test]
    fn the_delete_dialog_renders_over_the_frame() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-del-123", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        dash.request_delete();
        let (_, dialog) = dash.pending_confirm.as_ref().expect("dialog open").clone();
        terminal
            .draw(|f| {
                dialog.draw(f, f.area());
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Delete run?"));
        assert!(text.contains("[ Cancel ]"));
    }

    // ─── Regression: help overlay must scope to the current page ──────────
    //
    // draw_help_overlay() must scope its sections to `self.detail_view`:
    // rendering both the "Main list" and "Detail view"/"Input" sections
    // regardless of page would show the user keybindings for a page they
    // aren't on.

    #[test]
    fn draw_help_overlay_main_list_omits_detail_view_section() {
        let backend = TestBackend::new(120, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.detail_view = false;
        terminal
            .draw(|f| {
                dash.draw_help_overlay(f);
            })
            .unwrap();

        let rendered = rendered_buffer(&terminal);
        assert!(rendered.contains("Agent run list"));
        assert!(rendered.contains("cycle sort"));
        assert!(rendered.contains("kill the run (asks first)"));
        assert!(!rendered.contains("Detail view"));
        assert!(!rendered.contains("switch stage tab"));
    }

    #[test]
    fn draw_help_overlay_detail_view_omits_main_list_section() {
        let backend = TestBackend::new(120, 70);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.detail_view = true;
        terminal
            .draw(|f| {
                dash.draw_help_overlay(f);
            })
            .unwrap();

        let rendered = rendered_buffer(&terminal);
        assert!(rendered.contains("Detail view"));
        assert!(rendered.contains("switch stage tab"));
        assert!(rendered.contains("the stage graph explorer"), "{rendered}");
        assert!(rendered.contains("Stage explorer (g)"), "{rendered}");
        assert!(rendered.contains("tab / shift-tab"), "{rendered}");
        assert!(rendered.contains("escape edges"), "{rendered}");
        assert!(rendered.contains("logs / output / context"), "{rendered}");
        // The response editor's keys are listed here rather than in a mode of
        // their own: `?` is unbound once you are typing, so the only place to
        // read them is before you press `i`.
        assert!(rendered.contains("Writing a response"));
        assert!(!rendered.contains("Agent run list"));
        assert!(!rendered.contains("cycle sort"));
    }

    #[test]
    fn draw_help_overlay_mcp_screen_shows_its_own_keys() {
        let backend = TestBackend::new(120, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.mcp_screen = true;
        terminal
            .draw(|f| {
                dash.draw_help_overlay(f);
            })
            .unwrap();

        let rendered = rendered_buffer(&terminal);
        assert!(rendered.contains("MCP servers"));
        assert!(rendered.contains("remove (asks first)"));
    }

    fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    /// The run list's help has to name the key that opens the newest screen.
    /// It did not, which is how a whole feature stayed undiscoverable.
    #[test]
    fn draw_help_overlay_run_list_names_the_new_run_key() {
        let backend = TestBackend::new(120, 60);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        terminal.draw(|f| dash.draw_help_overlay(f)).unwrap();

        let rendered = rendered_buffer(&terminal);
        assert!(rendered.contains("start a new run"), "{rendered}");
        // And the shared tail every mode ends with.
        assert!(rendered.contains("Everywhere"));
        assert!(rendered.contains("Mouse"));
    }

    /// The new-run screen has its own help, reachable with F1 because `?` is a
    /// question mark in both of its panes.
    #[test]
    fn draw_help_overlay_new_run_screen_documents_its_panes() {
        let backend = TestBackend::new(120, 60);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.new_run_screen = true;
        terminal.draw(|f| dash.draw_help_overlay(f)).unwrap();

        let rendered = rendered_buffer(&terminal);
        assert!(rendered.contains("New run: agent blueprints"), "{rendered}");
        assert!(rendered.contains("New run: task"));
        assert!(rendered.contains("reference a file"));
        assert!(rendered.contains("New run: unattended"));
    }

    /// The log panel had no section at all: the run list only said Tab focuses
    /// it, and nothing said what the keys did once it had focus.
    #[test]
    fn draw_help_overlay_log_pane_has_its_own_section() {
        let backend = TestBackend::new(120, 60);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.main_focus = MainPane::LogPane;
        terminal.draw(|f| dash.draw_help_overlay(f)).unwrap();

        let rendered = rendered_buffer(&terminal);
        assert!(rendered.contains("Log panel"), "{rendered}");
        assert!(rendered.contains("follow again"));
    }

    /// Help longer than the window scrolls rather than stopping silently,
    /// which is what hid the end of the detail view's list.
    #[test]
    fn the_help_overlay_scrolls_when_it_does_not_fit() {
        let mut dash = make_test_dashboard();
        dash.detail_view = true;
        // The overlay is open, so the keys below reach it rather than the
        // screen underneath.
        dash.show_help = true;
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|f| dash.draw_help_overlay(f)).unwrap();
        let top = rendered_buffer(&terminal);
        assert!(top.contains("switch stage tab"), "{top}");
        assert!(!top.contains("Everywhere"), "the tail is below the fold");
        assert!(top.contains("scroll"), "and it says the overlay scrolls");

        // Scrolling to the end brings the tail into view, and the offset is
        // clamped to something real rather than left past the end.
        for _ in 0..40 {
            dash.handle_key(key(KeyCode::PageDown));
        }
        terminal.draw(|f| dash.draw_help_overlay(f)).unwrap();
        assert!(rendered_buffer(&terminal).contains("Everywhere"));

        let at_end = dash.help_scroll.get();
        dash.handle_key(key(KeyCode::Up));
        assert_eq!(
            dash.help_scroll.get(),
            at_end - 1,
            "one press up moves one line, not nothing"
        );
    }

    /// Closing resets the scroll, so the next open starts at the top rather
    /// than wherever the last reader left it.
    #[test]
    fn closing_the_help_overlay_resets_its_scroll() {
        let mut dash = make_test_dashboard();
        dash.show_help = true;
        dash.handle_key(key(KeyCode::PageDown));
        assert!(dash.help_scroll.get() > 0);

        dash.handle_key(key(KeyCode::Esc));
        assert!(!dash.show_help);
        assert_eq!(dash.help_scroll.get(), 0);
    }

    /// F1 opens it wherever `?` does, and on the screens where `?` is text.
    #[test]
    fn f1_opens_the_help_overlay() {
        let mut dash = make_test_dashboard();
        dash.handle_key(key(KeyCode::F(1)));
        assert!(dash.show_help, "from the run list");

        dash.handle_key(key(KeyCode::Esc));
        dash.new_run_screen = true;
        dash.handle_key(key(KeyCode::F(1)));
        assert!(dash.show_help, "and from the screen that types text");

        // `?` there is a question mark, as it must be.
        dash.handle_key(key(KeyCode::Esc));
        dash.handle_key(key(KeyCode::Char('?')));
        assert!(!dash.show_help);
        assert_eq!(dash.new_run_filter, "?");
    }
}

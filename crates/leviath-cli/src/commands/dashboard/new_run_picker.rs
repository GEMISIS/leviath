//! The new-run screen's file picker: a modal over the Inputs pane that lists
//! the files under the working directory a caller-input region will take, so a
//! file reaches a region by being chosen from a list, never by the user typing
//! its name or an `@` in front of it.
//!
//! The list is the working directory only, which is the root a run's file
//! tools are confined to, so the picker can never offer a file the run could
//! not read. It is filtered to the region's `accepts`: a `pictures` region of
//! `image/*` shows images, not the run's `notes.md`. Files are toggled on and
//! off and each one's token cost is shown, so the limit is the region's token
//! budget (its share of the model's context window), not an arbitrary count; a
//! blueprint may still set a hard `max_stored` cap, which the picker honours.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent};
use leviath_core::mime::MimeRegistry;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use super::state::Dashboard;
use super::theme::*;
use crate::commands::run::attach::cli_registry;

/// The most files the picker will walk the working directory for. A menu is a
/// convenience; past a few thousand entries a list stops being one, and the
/// type filter is what narrows it anyway.
const FILE_CAP: usize = 2000;

/// One candidate file under the working directory.
#[derive(Debug, Clone)]
struct PickerFile {
    /// Workdir-relative path, what the row stores and the part is read from.
    rel: PathBuf,
    /// The type the file's extension resolves to, shown dim beside it. `?`
    /// when the registry does not know the extension, in which case the file
    /// is offered anyway rather than hidden.
    type_label: String,
    /// The estimated token cost of this file, shown on the row and summed into
    /// the running total as it is chosen.
    tokens: usize,
}

/// A modal that fills one Inputs row by choosing files under the workdir.
#[derive(Debug)]
pub(super) struct FilePicker {
    /// The Inputs row this fills.
    row: usize,
    /// The region's name, for the title.
    region: String,
    /// A hard count cap the region declares, when it does; `None` means the
    /// only limit is the token budget.
    max_stored: Option<usize>,
    /// The region's token budget, resolved against the entry model's window.
    /// `0` means the region declares none, so no token limit is enforced.
    budget: usize,
    /// Every candidate, already filtered to the region's `accepts` and costed.
    files: Vec<PickerFile>,
    /// Indices into `files` matching the current name filter, in order.
    filtered: Vec<usize>,
    /// The name filter typed so far.
    query: String,
    /// Highlighted row of `filtered`.
    selected: usize,
    /// The files chosen so far, each with its estimated token cost.
    chosen: BTreeMap<PathBuf, usize>,
    /// A one-line reason the last pick was refused, shown until the next key.
    warn: Option<String>,
}

impl FilePicker {
    /// Rebuild `filtered` from the query (a case-insensitive substring of the
    /// path) and put the highlight back on the top.
    fn refilter(&mut self) {
        let needle = self.query.to_lowercase();
        self.filtered = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                needle.is_empty() || f.rel.to_string_lossy().to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect();
        self.selected = 0;
    }

    /// The candidate under the highlight, if any.
    fn highlighted(&self) -> Option<&PickerFile> {
        self.filtered.get(self.selected).map(|&i| &self.files[i])
    }

    /// The tokens the choice costs so far.
    fn chosen_tokens(&self) -> usize {
        self.chosen.values().sum()
    }

    /// Toggle the highlighted file in or out of the choice. A one-file region
    /// (`max_stored = 1`) swaps rather than adds. Otherwise the only limit is
    /// the token budget, plus any hard count cap the region declares; a pick
    /// that would exceed either is refused with the reason.
    fn toggle_highlighted(&mut self) {
        self.warn = None;
        let Some((path, tokens)) = self.highlighted().map(|f| (f.rel.clone(), f.tokens)) else {
            return;
        };
        if self.chosen.remove(&path).is_some() {
            return;
        }
        match self.max_stored {
            // A single-file region swaps the one it holds.
            Some(1) => self.chosen.clear(),
            // A region with a hard cap refuses past it.
            Some(n) if self.chosen.len() >= n => {
                self.warn = Some(format!("holds at most {n} files"));
                return;
            }
            // Room under a cap, or no cap at all.
            _ => {}
        }
        if self.budget > 0 && self.chosen_tokens() + tokens > self.budget {
            let left = self.budget.saturating_sub(self.chosen_tokens());
            self.warn = Some(format!(
                "no room: ~{} tokens, {} left of {}",
                tokens, left, self.budget
            ));
            return;
        }
        self.chosen.insert(path, tokens);
    }

    /// The choice as an ordered list.
    fn chosen_paths(&self) -> Vec<PathBuf> {
        self.chosen.keys().cloned().collect()
    }
}

/// A token estimate for a workdir file, offline: its type's rule applied to the
/// file's size, with image dimensions and audio duration read from the header
/// when the rule needs them. It matches what the daemon charges the part at
/// ingest closely enough to keep a budget honest, without reading the whole
/// file, and errs high (an unprobed image falls to the per-pixel cap), which is
/// the safe direction for a budget.
pub(super) fn estimate_file_tokens(path: &Path, registry: &MimeRegistry) -> usize {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let name = path.file_name().and_then(|n| n.to_str());
    let head = read_head(path, 64 * 1024);
    let mime_type = registry.resolve(None, name, &head);
    let info = registry.info(&mime_type);
    let dims = leviath_core::mime::probe::dimensions(&mime_type, &head);
    let duration = leviath_core::mime::probe::duration_ms(&mime_type, &head);
    info.tokens.estimate(size, dims, duration)
}

/// The first `cap` bytes of `path`, enough for the header probes; empty if it
/// cannot be opened or read.
fn read_head(path: &Path, cap: usize) -> Vec<u8> {
    use std::io::Read;
    let mut buf = vec![0u8; cap];
    let read = std::fs::File::open(path)
        .and_then(|mut file| file.read(&mut buf))
        .unwrap_or(0);
    buf.truncate(read);
    buf
}

/// Whether a workdir file belongs in a picker for `accepts`, and the type
/// label to show. `None` means a known type that does not match, so the file
/// is hidden; an unknown extension is kept (`?`) rather than ruled out.
fn candidate(
    rel: &str,
    registry: &leviath_core::mime::MimeRegistry,
    accepts: &[String],
) -> Option<PickerFile> {
    let ext = Path::new(rel)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let resolved = registry.from_extension(ext);
    let label = resolved
        .as_ref()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| "?".to_string());
    let keep = accepts.is_empty()
        || match &resolved {
            Some(m) => m.matches_any(accepts),
            None => true,
        };
    keep.then(|| PickerFile {
        rel: PathBuf::from(rel),
        type_label: label,
        // Filled by the caller once the file is kept, so a rejected candidate
        // is never read.
        tokens: 0,
    })
}

impl Dashboard {
    /// Open the picker over Inputs row `row`, listing the workdir files the
    /// region takes, with the files already on the row pre-chosen.
    pub(super) fn open_new_run_picker(&mut self, row: usize) {
        let Some(slot) = self.new_run_inputs.get(row) else {
            return;
        };
        let region = slot.region.clone();
        let accepts = slot.accepts.clone();
        let max_stored = slot.max_stored;
        let budget = slot.max_tokens;
        let workdir = self.new_run_ctx.workdir.clone();
        let registry = cli_registry();
        // The files already on the row start chosen, each with its estimate.
        let chosen: BTreeMap<PathBuf, usize> = slot
            .files
            .iter()
            .map(|p| {
                let tokens = estimate_file_tokens(&workdir.join(p), &registry);
                (p.clone(), tokens)
            })
            .collect();
        // Each candidate is typed by extension, then costed once so the row can
        // show its tokens and the running total is a sum, not a re-read.
        let files: Vec<PickerFile> = super::new_run::collect_workdir_files(&workdir, FILE_CAP)
            .iter()
            .filter_map(|name| candidate(name, &registry, &accepts))
            .map(|mut file| {
                file.tokens = estimate_file_tokens(&workdir.join(&file.rel), &registry);
                file
            })
            .collect();
        let mut picker = FilePicker {
            row,
            region,
            max_stored,
            budget,
            files,
            filtered: Vec::new(),
            query: String::new(),
            selected: 0,
            chosen,
            warn: None,
        };
        picker.refilter();
        self.new_run_picker = Some(picker);
    }

    /// Whether the picker modal is up and holds the keys.
    pub(super) fn new_run_picker_open(&self) -> bool {
        self.new_run_picker.is_some()
    }

    /// Keys while the picker is up: `↑`/`↓` move, `Space` toggles a file (or
    /// swaps it, for a one-file region), `Enter` confirms, `Esc` cancels, and
    /// anything typed filters by name.
    pub(super) fn handle_new_run_picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = self.new_run_picker.as_mut() else {
            return;
        };
        // Any key dismisses a refusal reason; a Space that is refused again
        // sets a fresh one.
        picker.warn = None;
        match key.code {
            KeyCode::Esc => self.new_run_picker = None,
            KeyCode::Enter => {
                // Enter only confirms what Space chose. It never selects the
                // highlighted file on its own, so a file toggled on can be
                // toggled back off and the choice left empty.
                let row = picker.row;
                let files = picker.chosen_paths();
                self.new_run_picker = None;
                // `row` was a valid index when the picker opened, and no key
                // rebuilds the input rows while the picker holds them all, so it
                // is still valid.
                self.new_run_inputs[row].files = files;
            }
            KeyCode::Char(' ') => picker.toggle_highlighted(),
            KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Down => {
                if picker.selected + 1 < picker.filtered.len() {
                    picker.selected += 1;
                }
            }
            KeyCode::Backspace => {
                picker.query.pop();
                picker.refilter();
            }
            KeyCode::Char(c) => {
                picker.query.push(c);
                picker.refilter();
            }
            _ => {}
        }
    }

    /// Draw the picker, centred over the screen, when it is open.
    pub(super) fn draw_new_run_picker(&self, frame: &mut Frame, area: Rect) {
        let Some(picker) = self.new_run_picker.as_ref() else {
            return;
        };
        let popup = centred(area, 64, 70);
        let inner = Rect {
            x: popup.x + 1,
            y: popup.y + 1,
            width: popup.width.saturating_sub(2),
            height: popup.height.saturating_sub(2),
        };
        // The count cap is named only when the region declares one; otherwise
        // the token budget is the only limit and the title says so by omission.
        let cap_note = match picker.max_stored {
            Some(1) => "one file, ".to_string(),
            Some(n) => format!("up to {n}, "),
            None => String::new(),
        };
        // The token line is the honest answer to "how many files fit": the
        // budget is the region's share of the model's context window.
        let budget_note = match picker.budget > 0 {
            true => format!(", ≈{}/{} tokens", picker.chosen_tokens(), picker.budget),
            false => String::new(),
        };
        let title = format!(
            " Choose files for {} ({}{} chosen{}) ",
            picker.region,
            cap_note,
            picker.chosen.len(),
            budget_note,
        );
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(C_BORDER_FOCUS))
                .title(Span::styled(
                    title,
                    Style::default()
                        .fg(C_BORDER_FOCUS)
                        .add_modifier(Modifier::BOLD),
                )),
            popup,
        );

        let mut lines: Vec<Line<'static>> = Vec::new();
        let query = match picker.query.is_empty() {
            true => "type to filter".to_string(),
            false => picker.query.clone(),
        };
        lines.push(Line::from(vec![
            Span::styled("filter: ", Style::default().fg(C_MUTED)),
            Span::styled(
                query,
                match picker.query.is_empty() {
                    true => Style::default().fg(C_DIM),
                    false => Style::default().fg(C_ACTIVE),
                },
            ),
        ]));
        lines.push(Line::from(""));

        // Room for the two-line header and the one-line footer.
        let list_rows = inner.height.saturating_sub(3) as usize;
        if picker.filtered.is_empty() {
            lines.push(Line::from(Span::styled(
                "  no matching file in the working directory",
                Style::default().fg(C_DIM),
            )));
        } else {
            let start = picker.selected.saturating_sub(list_rows.saturating_sub(1));
            for &fi in picker.filtered.iter().skip(start).take(list_rows) {
                let file = &picker.files[fi];
                let on = Some(fi) == picker.filtered.get(picker.selected).copied();
                let ticked = picker.chosen.contains_key(&file.rel);
                let mark = match ticked {
                    true => "[x] ",
                    false => "[ ] ",
                };
                let name = file.rel.to_string_lossy().to_string();
                lines.push(Line::from(vec![
                    Span::styled(if on { "› " } else { "  " }, Style::default().fg(C_ACCENT)),
                    Span::styled(
                        mark,
                        match ticked {
                            true => Style::default().fg(C_SUCCESS),
                            false => Style::default().fg(C_MUTED),
                        },
                    ),
                    Span::styled(
                        name,
                        match on {
                            true => Style::default().fg(C_ACTIVE).add_modifier(Modifier::BOLD),
                            false => Style::default().fg(C_MUTED),
                        },
                    ),
                    Span::styled(
                        format!(
                            "  {}  ~{} tok",
                            file.type_label,
                            super::new_run_inputs::compact_count(file.tokens)
                        ),
                        Style::default().fg(C_DIM),
                    ),
                ]));
            }
        }
        frame.render_widget(Paragraph::new(lines), inner);

        // A refused pick replaces the key hint with its reason until the next
        // key, so the person sees why nothing happened.
        let footer = match &picker.warn {
            Some(reason) => Line::from(Span::styled(
                format!(" {reason} "),
                Style::default().fg(C_WARN).add_modifier(Modifier::BOLD),
            )),
            None => Line::from(Span::styled(
                match picker.max_stored {
                    Some(1) => " Space choose (swaps) · Enter done · Esc cancel · type to filter ",
                    _ => " Space add/remove · Enter done · Esc cancel · type to filter ",
                },
                Style::default().fg(C_MUTED),
            )),
        };
        let footer_area = Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(1),
            width: inner.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(footer), footer_area);
    }
}

/// A rectangle `pct_w` × `pct_h` percent of `area`, centred in it.
fn centred(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let width = (area.width * pct_w / 100).clamp(20, area.width);
    let height = (area.height * pct_h / 100).clamp(6, area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::make_test_dashboard;
    use crate::commands::dashboard::types::{NewRunContext, NewRunPane};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// An agent with a one-image slot, a many-image slot, and a text slot, so
    /// the picker's single, multi, and no-picker cases all have a row.
    fn write_agent(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("agent.leviath"),
            "[agent]\nname = \"looker\"\nversion = \"0.1.0\"\ndescription = \"looks\"\n\n\
             [stages.main]\nmode = \"autonomous\"\n\n\
             [stages.main.model]\nprovider = \"anthropic\"\nmodel = \"claude-sonnet-5\"\n\n\
             [context.regions]\n\
             task = { kind = \"pinned\", max_tokens = 1000, seed = \"task\" }\n\
             cover = { kind = \"pinned\", max_tokens = 100000, seed = \"input\", accepts = [\"image/*\"], max_stored = 1 }\n\
             gallery = { kind = \"pinned\", max_tokens = 100000, seed = \"input\", accepts = [\"image/*\"], max_stored = 3 }\n\
             notes = { kind = \"pinned\", max_tokens = 1000, seed = \"input\", accepts = [\"text/*\"] }\n\
             tight = { kind = \"pinned\", max_tokens = 1000, seed = \"input\", accepts = [\"image/*\"], max_stored = 4 }\n\
             stream = { kind = \"pinned\", max_tokens = 100000, seed = \"input\", accepts = [\"image/*\"] }\n\
             conversation = { kind = \"sliding_window\", max_items = 20, max_tokens = 10000 }\n",
        )
        .unwrap();
    }

    fn dash_at(dir: &Path) -> Dashboard {
        let mut dash = make_test_dashboard();
        dash.new_run_ctx = NewRunContext {
            agents_dir: dir.join("agents"),
            config_path: dir.join("config.toml"),
            workdir: dir.join("work"),
        };
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("hero.png"), b"\x89PNG\r\n\x1a\na").unwrap();
        std::fs::write(work.join("villain.png"), b"\x89PNG\r\n\x1a\nb").unwrap();
        std::fs::write(work.join("readme.md"), "text").unwrap();
        dash.last_launched_agent = Some("looker".to_string());
        dash
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// The whole screen rendered to text, for asserting on the modal.
    fn draw(dash: &mut Dashboard) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal.draw(|f| dash.draw(f)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect()
    }

    /// A file-only row opens the picker on Enter; it lists the images and not
    /// the markdown, and choosing one puts it on the row.
    #[test]
    fn a_file_row_picks_from_the_workdir() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        // The rows are cover, gallery, notes.
        assert_eq!(dash.new_run_inputs[0].region, "cover");
        dash.new_run_focus = NewRunPane::Inputs;
        dash.new_run_input_selected = 0;
        dash.handle_new_run_key(key(KeyCode::Enter));
        assert!(dash.new_run_picker_open(), "Enter opens the picker");
        let picker = dash.new_run_picker.as_ref().unwrap();
        let names: Vec<String> = picker
            .files
            .iter()
            .map(|f| f.rel.to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"hero.png".to_string()));
        assert!(names.contains(&"villain.png".to_string()));
        assert!(
            !names.contains(&"readme.md".to_string()),
            "text is filtered out"
        );
        // Space chooses the highlighted file; Enter alone chooses nothing, so
        // a file can be left unselected.
        dash.handle_new_run_key(key(KeyCode::Char(' ')));
        dash.handle_new_run_key(key(KeyCode::Enter));
        assert!(!dash.new_run_picker_open());
        assert_eq!(dash.new_run_inputs[0].files.len(), 1);
        // The chosen file resolves to a part on its region when the run starts,
        // with no `@` anywhere.
        let resolved = dash.new_run_input_values().unwrap();
        assert_eq!(resolved.parts.len(), 1);
        assert_eq!(resolved.parts[0].region.as_deref(), Some("cover"));
    }

    /// A many-file row toggles files with Space up to its cap, and Enter keeps
    /// what was toggled.
    #[test]
    fn a_multi_file_row_toggles_up_to_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        let gallery = dash
            .new_run_inputs
            .iter()
            .position(|s| s.region == "gallery")
            .unwrap();
        dash.new_run_focus = NewRunPane::Inputs;
        dash.new_run_input_selected = gallery;
        dash.handle_new_run_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(dash.new_run_picker_open(), "Ctrl+O opens it");
        assert_eq!(dash.new_run_picker.as_ref().unwrap().max_stored, Some(3));
        // Toggle both files on, then confirm.
        dash.handle_new_run_key(key(KeyCode::Char(' ')));
        dash.handle_new_run_key(key(KeyCode::Down));
        dash.handle_new_run_key(key(KeyCode::Char(' ')));
        assert_eq!(dash.new_run_picker.as_ref().unwrap().chosen.len(), 2);
        dash.handle_new_run_key(key(KeyCode::Enter));
        assert!(!dash.new_run_picker_open());
        assert_eq!(dash.new_run_inputs[gallery].files.len(), 2);
        // Re-opening pre-checks the chosen files; toggling one off removes it.
        dash.handle_new_run_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert_eq!(dash.new_run_picker.as_ref().unwrap().chosen.len(), 2);
        dash.handle_new_run_key(key(KeyCode::Char(' ')));
        dash.handle_new_run_key(key(KeyCode::Enter));
        assert_eq!(dash.new_run_inputs[gallery].files.len(), 1);
    }

    /// The cap holds: a fourth pick on a three-file region is ignored, and a
    /// one-file region swaps rather than stacks.
    #[test]
    fn the_cap_and_the_single_swap_hold() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        std::fs::write(
            dir.path().join("work").join("extra.png"),
            b"\x89PNG\r\n\x1a\nc",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("work").join("more.png"),
            b"\x89PNG\r\n\x1a\nd",
        )
        .unwrap();
        dash.open_new_run_screen();
        // Single region: two picks leave one.
        let cover = dash
            .new_run_inputs
            .iter()
            .position(|s| s.region == "cover")
            .unwrap();
        dash.open_new_run_picker(cover);
        let p = dash.new_run_picker.as_mut().unwrap();
        p.toggle_highlighted();
        p.selected = 1;
        p.toggle_highlighted();
        assert_eq!(p.chosen.len(), 1, "one-file region swaps");

        // Many region caps at three however many are toggled.
        let gallery = dash
            .new_run_inputs
            .iter()
            .position(|s| s.region == "gallery")
            .unwrap();
        dash.open_new_run_picker(gallery);
        let p = dash.new_run_picker.as_mut().unwrap();
        for i in 0..p.filtered.len() {
            p.selected = i;
            p.toggle_highlighted();
        }
        assert_eq!(p.chosen.len(), 3, "capped at max_stored");
    }

    /// The modal draws its title, the tick boxes and the footer.
    #[test]
    fn the_picker_draws() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        dash.open_new_run_picker(0);
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal.draw(|f| dash.draw(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("Choose files for cover"), "{text}");
        assert!(text.contains("hero.png"), "{text}");
        assert!(text.contains("Space choose"), "{text}");
        assert!(text.contains("Enter done"), "{text}");
        // Filtering narrows the list.
        dash.handle_new_run_key(key(KeyCode::Char('v')));
        assert_eq!(dash.new_run_picker.as_ref().unwrap().filtered.len(), 1);
    }

    /// Esc closes without touching the row.
    #[test]
    fn esc_cancels() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        dash.open_new_run_picker(0);
        dash.handle_new_run_key(key(KeyCode::Char(' ')));
        dash.handle_new_run_key(key(KeyCode::Esc));
        assert!(!dash.new_run_picker_open());
        assert!(
            dash.new_run_inputs[0].files.is_empty(),
            "cancel keeps the row empty"
        );
    }

    /// The picker's other keys: move down and back up, filter and un-filter,
    /// and ignore a key it has no use for. A key sent when no picker is open
    /// does nothing.
    #[test]
    fn the_other_keys_move_filter_and_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        // No picker: a key is a no-op.
        dash.handle_new_run_picker_key(key(KeyCode::Down));
        assert!(!dash.new_run_picker_open());
        dash.open_new_run_picker(0);
        dash.handle_new_run_key(key(KeyCode::Down));
        assert_eq!(dash.new_run_picker.as_ref().unwrap().selected, 1);
        // Down at the last row stays put.
        dash.handle_new_run_key(key(KeyCode::Down));
        assert_eq!(dash.new_run_picker.as_ref().unwrap().selected, 1);
        dash.handle_new_run_key(key(KeyCode::Up));
        assert_eq!(dash.new_run_picker.as_ref().unwrap().selected, 0);
        dash.handle_new_run_key(key(KeyCode::Char('v')));
        assert_eq!(dash.new_run_picker.as_ref().unwrap().query, "v");
        dash.handle_new_run_key(key(KeyCode::Backspace));
        assert_eq!(dash.new_run_picker.as_ref().unwrap().query, "");
        // A key the picker has no use for leaves it as it was.
        dash.handle_new_run_key(key(KeyCode::Tab));
        assert!(dash.new_run_picker_open());
        // Toggling with nothing under the highlight (the list filtered to
        // empty) is a no-op.
        dash.handle_new_run_key(key(KeyCode::Char('z')));
        assert!(dash.new_run_picker.as_ref().unwrap().filtered.is_empty());
        dash.handle_new_run_key(key(KeyCode::Char(' ')));
        assert!(dash.new_run_picker.as_ref().unwrap().chosen.is_empty());
    }

    /// A multi-file picker draws its tick boxes, its cap in the title, the
    /// active filter, and its footer; an empty match says so.
    #[test]
    fn the_multi_picker_draws_every_part() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        let gallery = dash
            .new_run_inputs
            .iter()
            .position(|s| s.region == "gallery")
            .unwrap();
        dash.open_new_run_picker(gallery);
        // Choose one, leave one, and render: [x] and [ ] both appear, with the
        // cap in the title and the multi-file footer.
        dash.handle_new_run_key(key(KeyCode::Char(' ')));
        let text = draw(&mut dash);
        assert!(text.contains("Choose files for gallery"), "{text}");
        assert!(text.contains("up to 3"), "{text}");
        assert!(text.contains("[x]"), "{text}");
        assert!(text.contains("[ ]"), "{text}");
        assert!(text.contains("Space add/remove"), "{text}");
        assert!(text.contains("type to filter"), "{text}");
        // A filter that matches something shows the query; one that matches
        // nothing says so.
        dash.handle_new_run_key(key(KeyCode::Char('h')));
        let text = draw(&mut dash);
        assert!(text.contains("filter: h"), "{text}");
        dash.handle_new_run_key(key(KeyCode::Char('z')));
        let text = draw(&mut dash);
        assert!(text.contains("no matching file"), "{text}");
    }

    /// A file whose extension the registry does not know is offered anyway,
    /// labelled `?`, rather than hidden.
    #[test]
    fn an_unknown_extension_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        std::fs::write(dir.path().join("work").join("scene.weird"), b"data").unwrap();
        dash.open_new_run_screen();
        // The cover row takes image/*; an unknown type cannot be ruled out.
        dash.open_new_run_picker(0);
        let picker = dash.new_run_picker.as_ref().unwrap();
        let weird = picker
            .files
            .iter()
            .find(|f| f.rel.to_string_lossy() == "scene.weird")
            .expect("the unknown-type file is offered");
        assert_eq!(weird.type_label, "?");
    }

    /// Enter never selects on its own: with nothing toggled the row stays
    /// empty, and a file toggled on can be toggled back off.
    #[test]
    fn enter_does_not_auto_select() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        // Enter with nothing chosen leaves the row empty.
        dash.open_new_run_picker(0);
        dash.handle_new_run_key(key(KeyCode::Enter));
        assert!(dash.new_run_inputs[0].files.is_empty());
        // Toggle a file on, then off, then confirm: still empty.
        dash.open_new_run_picker(0);
        dash.handle_new_run_key(key(KeyCode::Char(' ')));
        dash.handle_new_run_key(key(KeyCode::Char(' ')));
        dash.handle_new_run_key(key(KeyCode::Enter));
        assert!(dash.new_run_inputs[0].files.is_empty());
    }

    /// A region whose budget an image would blow refuses the pick, says why in
    /// the footer, and shows the running tokens against the budget in the title.
    #[test]
    fn a_tight_budget_refuses_a_file() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        let tight = dash
            .new_run_inputs
            .iter()
            .position(|s| s.region == "tight")
            .unwrap();
        assert_eq!(dash.new_run_inputs[tight].max_tokens, 1000);
        dash.open_new_run_picker(tight);
        // The title shows the budget.
        let text = draw(&mut dash);
        assert!(text.contains("/1000 tokens"), "{text}");
        // A ~1600-token image does not fit 1000: refused, with a reason.
        dash.handle_new_run_key(key(KeyCode::Char(' ')));
        assert!(dash.new_run_picker.as_ref().unwrap().chosen.is_empty());
        assert!(
            dash.new_run_picker.as_ref().unwrap().warn.is_some(),
            "a refusal is recorded"
        );
        let text = draw(&mut dash);
        assert!(text.contains("no room"), "{text}");
    }

    /// A region with no token budget (0) enforces no token limit and shows no
    /// token line.
    #[test]
    fn no_budget_means_no_token_limit() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        // Force the cover slot to declare no budget.
        dash.new_run_inputs[0].max_tokens = 0;
        dash.open_new_run_picker(0);
        let text = draw(&mut dash);
        assert!(
            !text.contains("tokens)"),
            "no token line in the title: {text}"
        );
        // Toggling still works with no budget to check against.
        dash.handle_new_run_key(key(KeyCode::Char(' ')));
        assert_eq!(dash.new_run_picker.as_ref().unwrap().chosen.len(), 1);
    }

    /// The token estimate handles a file that is not there: no size, no header,
    /// and a zero-ish estimate rather than a panic.
    #[test]
    fn estimate_handles_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let reg = crate::commands::run::attach::cli_registry();
        // A missing file: metadata and the header read both fail, so the
        // estimate falls to the size-0 case (a token floor, not a panic).
        let tokens = estimate_file_tokens(&dir.path().join("gone.bin"), &reg);
        assert!(tokens <= 1, "a missing file costs about nothing: {tokens}");
    }

    /// A region with no count cap takes as many files as the budget allows, and
    /// its title names no count, only the token line. Each row shows its tokens.
    #[test]
    fn no_count_cap_is_bounded_only_by_tokens() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        let stream = dash
            .new_run_inputs
            .iter()
            .position(|s| s.region == "stream")
            .unwrap();
        assert_eq!(dash.new_run_inputs[stream].max_stored, None);
        dash.open_new_run_picker(stream);
        // Toggle both images on: no count cap stops them.
        dash.handle_new_run_key(key(KeyCode::Char(' ')));
        dash.handle_new_run_key(key(KeyCode::Down));
        dash.handle_new_run_key(key(KeyCode::Char(' ')));
        assert_eq!(dash.new_run_picker.as_ref().unwrap().chosen.len(), 2);
        let text = draw(&mut dash);
        // The title names the tokens, not a count cap.
        assert!(text.contains("/100000 tokens"), "{text}");
        assert!(!text.contains("up to"), "{text}");
        assert!(!text.contains("one file"), "{text}");
        // Each row shows its own token estimate.
        assert!(text.contains("tok"), "{text}");
    }

    /// Opening on a row that is not there does nothing.
    #[test]
    fn open_ignores_a_missing_row() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        dash.open_new_run_picker(999);
        assert!(!dash.new_run_picker_open());
    }
}

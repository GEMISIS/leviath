//! Shared test-only fixtures for the dashboard command modules.
//!
//! `make_test_dashboard` is shared here. Per-module `make_test_agent` helpers
//! stay local because each bakes in module-specific field values that its own
//! tests assert on (token counts, stage counts, titles, `is_run_state`, etc.).

use crate::commands::dashboard::state::Dashboard;
use tokio::sync::mpsc;

/// Build a `Dashboard` wired to a throwaway command channel. `Dashboard::new`
/// already points the activity log at a shared temp file and uses a no-op
/// clipboard, so `add_log`/`y` never touch the real `~/.leviath/dashboard.log`,
/// the system clipboard, or the TTY.
pub(crate) fn make_test_dashboard() -> Dashboard {
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
    Dashboard::new(cmd_tx)
}

/// Seed a run under the current runs dir whose answer arrived through
/// `submit_output`: the `final_output` descriptor in `meta.json` plus the
/// sidecar beside it, submitted by `stage` under the `markdown` format, with
/// no `output.log` anywhere. Callers run inside
/// `runstate::with_isolated_runs_dir`, so the home runs dir is never touched.
pub(crate) fn seed_run_with_final_output(run_id: &str, stage: &str, content: &str) {
    use crate::runstate;
    // A stale directory from an earlier panicked test must not leak in.
    let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
    let answer = leviath_core::output::FinalOutput::new(
        content,
        Some("markdown".to_string()),
        stage.to_string(),
        42,
    );
    let mut meta = runstate::RunMeta::new(
        run_id.to_string(),
        "agent".to_string(),
        "/p".to_string(),
        "task".to_string(),
        None,
        "/tmp".to_string(),
        1,
    );
    meta.final_output = Some(answer.descriptor());
    runstate::create_run(&meta).unwrap();
    runstate::write_final_output(&runstate::run_dir(run_id), &answer.content).unwrap();
}

/// Every cell of a rendered frame, concatenated row by row.
///
/// The cheapest honest assertion a render test can make: that the thing the
/// case exists to distinguish actually reached the screen. Deliberately not a
/// full-frame snapshot, which would fail on any cosmetic layout change and
/// teach the next person to regenerate it without reading.
///
/// Note there are no row separators, so a string long enough to wrap is split
/// across the join. Assert on a fragment short enough to sit on one row.
pub(crate) fn rendered_buffer(
    terminal: &ratatui::Terminal<ratatui::backend::TestBackend>,
) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect()
}

/// The style of the cell where `needle` first appears in a rendered frame,
/// so a test can say "the current stage is drawn in the active colour"
/// rather than only "the current stage is drawn". Panics when the text is
/// not on screen: that is the assertion failing, said plainly.
pub(crate) fn style_at_text(
    terminal: &ratatui::Terminal<ratatui::backend::TestBackend>,
    needle: &str,
) -> ratatui::style::Style {
    let buf = terminal.backend().buffer();
    let width = buf.area.width as usize;
    let chars: Vec<char> = rendered_buffer(terminal).chars().collect();
    let needle: Vec<char> = needle.chars().collect();
    let missing = format!("{:?} is not on screen", needle.iter().collect::<String>());
    let idx = (0..chars.len())
        .find(|&i| chars[i..].starts_with(&needle))
        .expect(&missing);
    let (x, y) = ((idx % width) as u16, (idx / width) as u16);
    buf.cell((buf.area.x + x, buf.area.y + y))
        .expect("the index came from this buffer")
        .style()
}

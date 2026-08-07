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

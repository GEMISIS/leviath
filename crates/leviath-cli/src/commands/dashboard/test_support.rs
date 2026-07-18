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

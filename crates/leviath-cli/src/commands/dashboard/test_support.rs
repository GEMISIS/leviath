//! Shared test-only fixtures for the dashboard command modules.
//!
//! `make_test_dashboard` was previously duplicated verbatim in ~10 sibling
//! modules (`mod.rs`, `input.rs`, `state.rs`, and the `render/*` files); it
//! now lives here once. Per-module `make_test_agent` helpers stay local
//! because each bakes in module-specific field values that its own tests
//! assert on (token counts, stage counts, titles, `is_run_state`, etc.).

use crate::commands::dashboard::state::Dashboard;
use tokio::sync::mpsc;

/// Build a `Dashboard` wired to a throwaway command channel.
pub(crate) fn make_test_dashboard() -> Dashboard {
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
    Dashboard::new(cmd_tx)
}

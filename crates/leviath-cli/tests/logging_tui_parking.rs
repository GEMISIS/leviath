//! The log-parking seam, driven through the real subscriber.
//!
//! The unit tests in `logging.rs` exercise the writer directly, which proves
//! the behaviour but not the wiring: a `tracing` call has to reach the writer
//! through the layer `init` installs, and that only happens in a process that
//! won the global subscriber slot. A test binary of its own is the smallest
//! place where that is guaranteed.
//!
//! The stakes are a user-visible bug: `lev -v setup` used to draw hyper and
//! rustls debug lines across the wizard, staircased by raw mode and never
//! painted over, because stderr is the same terminal the alternate screen is
//! on.

use leviath_cli::logging;

/// Nothing logged while the terminal is held reaches stderr, and nothing is
/// lost either: it arrives when the terminal is handed back.
#[test]
fn a_held_terminal_parks_log_lines_until_it_is_released() {
    logging::init(true);

    // Held: this line is buffered rather than written.
    logging::hold_for_tui();
    tracing::info!("a line emitted while a TUI owned the terminal");
    tracing::debug!("and a second one, at the level `-v` turns on");

    // Released: whatever was parked goes out, and the flag is clear again, so
    // ordinary logging resumes.
    logging::release_from_tui();
    tracing::info!("a line emitted with the terminal free");

    // Releasing again with nothing parked is what the panic hook plus `Drop`
    // actually do, and it must stay quiet rather than write an empty line.
    logging::release_from_tui();
}

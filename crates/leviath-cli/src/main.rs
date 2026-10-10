//! Leviath CLI - `lev` command-line interface (binary entry point).
//!
//! All of `lev` is [`leviath_cli::run`]; what this file adds is the allocator,
//! which is a binary's decision and not a library's. A fork that wraps lev in
//! its own binary writes a `main` like this one.

/// mimalloc instead of the platform allocator. The daemon's workload is a
/// stream of large, variably-sized, short-lived allocations (assembled
/// inference requests, context snapshots, tool results) interleaved with
/// long-lived small ones; the system allocator strands the freed spans behind
/// the live objects and RSS never comes back down (measured: a 22 MB live
/// footprint under 293 MB of retained RSS after a five-agent burst). mimalloc
/// returns freed pages to the OS aggressively, so RSS tracks what the process
/// actually holds.
#[cfg(not(feature = "system-allocator"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> std::process::ExitCode {
    // Purge freed memory at free time (leviath-alloc has the full why): an
    // idle daemon otherwise parks its burst memory as unflagged freed pages
    // the OS keeps charging to it. Applied in-binary so every lev process
    // behaves the same however it was started; a user-exported
    // MIMALLOC_PURGE_DELAY always wins.
    #[cfg(not(feature = "system-allocator"))]
    leviath_alloc::use_purge_at_free_unless_overridden();

    leviath_cli::run()
}

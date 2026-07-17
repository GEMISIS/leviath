//! Platform/OS-specific system calls for Leviath, isolated behind one
//! cross-platform API.
//!
//! Every raw `libc` call, `#[cfg(unix)]`/`#[cfg(windows)]` branch, and
//! platform permission/signal/TTY primitive used anywhere in the workspace
//! lives here — nowhere else. Callers use the plain functions re-exported at
//! the crate root and never write a `#[cfg]` of their own.
//!
//! ## Why this crate exists
//!
//! 1. **De-duplication.** The same `setsid`/`pre_exec` detach, `libc::kill`,
//!    and `Permissions::from_mode(0o600)` logic was previously copy-pasted
//!    across a dozen call sites in `leviath-cli`. There is now exactly one
//!    implementation of each.
//! 2. **Per-OS coverage correctness.** Because all platform code is gathered
//!    into cfg-gated submodules ([`platform`]), the non-target
//!    implementations (`#[cfg(windows)]` on a Linux CI run) are simply not
//!    compiled, so the coverage tool never sees them as gaps. The
//!    Linux-visible code paths are all reachable from real unit tests.
//! 3. **Testability.** The genuinely-untestable leaf syscalls (installing a
//!    `pre_exec` hook, opening `/dev/tty`, killing a live pid) keep a
//!    `#[cfg(not(test))]` real / `#[cfg(test)]` no-op "twin" — but that twin
//!    now lives *once*, inside this crate, so downstream crates stay clean and
//!    fully coverable.

mod platform;

pub mod perms;
pub mod process;

pub use perms::{ensure_file_private, secure_dir_perms, secure_file_perms};
pub use process::{configure_detached, terminate};

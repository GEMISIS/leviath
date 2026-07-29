//! Platform/OS-specific system calls for Leviath, isolated behind one
//! cross-platform API.
//!
//! Every `#[cfg(unix)]`/`#[cfg(windows)]` branch and platform
//! permission/signal/TTY primitive used anywhere in the workspace lives here -
//! nowhere else. The unavoidable OS-level `unsafe` is delegated to audited
//! crates (`nix`) and safe std APIs, so this crate itself contains no `unsafe`.
//! Callers use the plain functions re-exported at the crate root and never
//! write a `#[cfg]` of their own.
//!
//! ## Why this crate exists
//!
//! 1. **De-duplication.** The `process_group` detach and
//!    `Permissions::from_mode(0o600)` logic each has exactly one implementation
//!    here, rather than being spread across call sites in `leviath-cli`.
//! 2. **Per-OS coverage correctness.** Because all platform code is gathered
//!    into cfg-gated submodules (`platform`), the non-target
//!    implementations (`#[cfg(windows)]` on a Linux CI run) are simply not
//!    compiled, so the coverage tool never sees them as gaps. The
//!    Linux-visible code paths are all reachable from real unit tests.
//! 3. **Testability.** Every function in this crate is exercised by real unit
//!    tests - the syscalls here are either safe to call under test (`chmod` on a
//!    tempfile) or split behind an
//!    injected seam ([`tty::osc52_write_via`]
//!    takes the tty opener + sink; `perms`' hardening op is a `fn` pointer). The
//!    only genuinely-untestable real-I/O leaves (opening `/dev/tty`, writing the
//!    real `stdout()`) are composed in the CLI binary, not here - so this crate
//!    contains no `#[cfg(not(test))]`.

mod platform;

pub mod browser;
pub mod keychain;
pub mod perms;
pub mod process;
pub mod sandbox;
pub mod tty;

pub use browser::open_url;
pub use perms::{ensure_file_private, secure_dir_perms, secure_file_perms, write_private};
#[cfg(unix)]
pub use process::peer_uid;
pub use process::{configure_detached, current_uid, kill_process_group};
pub use sandbox::{
    ContainerRunSpec, container_exec_argv, container_rm_argv, container_run_argv,
    detect_container_engine, namespace_argv, namespace_supported,
};
pub use tty::osc52_write_via;

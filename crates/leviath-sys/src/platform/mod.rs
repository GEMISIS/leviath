//! Internal platform dispatch.
//!
//! Each public topic module at the crate root (`perms`, and later `process`,
//! `tty`, `exe`) calls into the functions re-exported here. The `#[cfg]`
//! selection of the real implementation happens in this one place, so the
//! public API stays platform-agnostic and free of conditional compilation.
//!
//! On a Linux coverage run, only [`unix`] is compiled; the [`fallback`] module
//! (`#[cfg(not(unix))]`) is never emitted and therefore never counts as a
//! coverage gap.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(not(unix))]
mod fallback;
#[cfg(not(unix))]
pub(crate) use fallback::*;

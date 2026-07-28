//! Internal platform dispatch.
//!
//! Each public topic module at the crate root (`perms`, and later `process`,
//! `tty`, `exe`) calls into the functions re-exported here. The `#[cfg]`
//! selection of the real implementation happens in this one place, so the
//! public API stays platform-agnostic and free of conditional compilation.
//!
//! Exactly one of these is compiled per target, so the others are never emitted
//! and never count as a coverage gap. [`fallback`] covers targets that are
//! neither — it applies no protection, and says so rather than pretending.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;

#[cfg(not(any(unix, windows)))]
mod fallback;
#[cfg(not(any(unix, windows)))]
pub(crate) use fallback::*;

//! Deciding whether an agent may write, and how much.
//!
//! Three questions, deliberately answered separately because they are not the
//! same kind of thing (issue #252):
//!
//! 1. **Will this fill the disk?** Always asked, not configurable away. A run
//!    that filled `C:` took the machine down with it, and every other process
//!    on it. Nobody wants that outcome, so nothing offers it.
//! 2. **Is one call writing an absurd amount?** Off unless configured. The
//!    reported incident was a single shell call appending in a loop until the
//!    60-second timeout - about 14 GB - and a per-call ceiling is what would
//!    have caught it.
//! 3. **Is the whole run writing an absurd amount?** Also off unless
//!    configured. Three calls of 12-14 GB each is the shape 2 alone misses.
//!
//! **2 and 3 default to off in code and on in a fresh config.** How much an
//! agent should be allowed to write is a judgement about what the user is doing
//! with it, not something this crate can know - so the code imposes nothing.
//! `lev setup` writes concrete values into `config.toml`, where they are
//! visible and can be deleted outright by anyone who wants no ceiling.

use serde::{Deserialize, Serialize};

/// How much free space must remain before a write is refused.
///
/// Chosen to leave a machine usable rather than merely alive: below a gigabyte,
/// a desktop OS starts failing at things a person notices - swap, browser
/// caches, save dialogs - well before the disk is literally full. Refusing the
/// agent's write at that point costs one tool call; not refusing it costs the
/// session.
pub const MIN_FREE_BYTES: u64 = 1024 * 1024 * 1024;

/// The ceilings in effect for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WriteLimits {
    /// Most one tool call may write. `None` is unlimited.
    pub per_call: Option<u64>,
    /// Most the whole run may write. `None` is unlimited.
    pub per_run: Option<u64>,
}

/// Why a write was refused, or that it was not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteVerdict {
    /// Nothing objected.
    Allow,
    /// The filesystem is nearly full.
    OutOfSpace {
        /// Bytes still writable.
        available: u64,
        /// Bytes that must remain.
        required: u64,
    },
    /// This one call is over the per-call ceiling.
    CallTooLarge {
        /// Bytes this call would write.
        bytes: u64,
        /// The ceiling.
        limit: u64,
    },
    /// The run has spent its budget.
    RunTooLarge {
        /// Bytes written so far, including this call.
        written: u64,
        /// The ceiling.
        limit: u64,
    },
}

impl WriteVerdict {
    /// The message an agent reads, or `None` when it was allowed.
    ///
    /// Each names the number that was exceeded, because "write refused" with no
    /// figure leaves a model guessing whether to retry smaller or stop - and
    /// the retry is what turns one refusal into a loop.
    pub fn refusal(&self) -> Option<String> {
        match self {
            Self::Allow => None,
            Self::OutOfSpace {
                available,
                required,
            } => Some(format!(
                "[denied] Refusing to write: only {available} bytes are free on this filesystem \
                 and {required} must remain. This is not a limit you can raise - the machine is \
                 nearly out of disk. Free some space, or write somewhere with room."
            )),
            Self::CallTooLarge { bytes, limit } => Some(format!(
                "[denied] This call would write {bytes} bytes, over the {limit}-byte per-call \
                 limit. Write less in one go, or raise `[limits] max_tool_call_write_bytes` in \
                 the Leviath config (deleting the line removes the limit)."
            )),
            Self::RunTooLarge { written, limit } => Some(format!(
                "[denied] This run has written {written} bytes, over its {limit}-byte budget. \
                 Raise `[limits] max_run_write_bytes` in the Leviath config, or delete the line \
                 to remove the limit."
            )),
        }
    }
}

/// Whether a write of `bytes` may proceed.
///
/// `available` is what the filesystem reports, or `None` when it could not be
/// measured. An unmeasurable filesystem **allows** the write: a guard that
/// cannot see has nothing to say, and refusing on it would block every write on
/// any filesystem the probe cannot read. The other two ceilings still apply.
///
/// `already_written` counts the run so far, *excluding* this call.
///
/// Checked in that order on purpose. Running out of disk is the only one that
/// harms anything outside this run, so it is reported first when more than one
/// applies - a user reading "over the per-call limit" would go raise the limit,
/// which is exactly wrong when the real problem is a full disk.
pub fn check_write(
    limits: WriteLimits,
    already_written: u64,
    bytes: u64,
    available: Option<u64>,
) -> WriteVerdict {
    if let Some(available) = available
        && available.saturating_sub(bytes) < MIN_FREE_BYTES
    {
        return WriteVerdict::OutOfSpace {
            available,
            required: MIN_FREE_BYTES,
        };
    }
    if let Some(limit) = limits.per_call
        && bytes > limit
    {
        return WriteVerdict::CallTooLarge { bytes, limit };
    }
    let total = already_written.saturating_add(bytes);
    if let Some(limit) = limits.per_run
        && total > limit
    {
        return WriteVerdict::RunTooLarge {
            written: total,
            limit,
        };
    }
    WriteVerdict::Allow
}

#[cfg(test)]
mod tests;

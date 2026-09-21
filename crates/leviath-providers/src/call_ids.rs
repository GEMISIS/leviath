//! Tool-call ids for the providers that send none.
//!
//! Most providers name every tool call themselves (`toolu_…`, `call_…`, a
//! `toolUseId`) and those names travel through Leviath untouched. A few send
//! nothing, or send a reply with the id missing, and then the id has to be
//! minted here.
//!
//! It is minted from one place so that every provider that needs one gets the
//! same guarantee, which is stronger than "unique in this response" and
//! stronger than "unique for this provider object":
//!
//! * Unique across the conversation, because `drop_unpaired_tool_turns` pairs a
//!   call with its result *by id* to keep a window that evicted half a pair
//!   from putting a malformed conversation on the wire. With every id equal,
//!   every call looks answered and every result looks called, the guard removes
//!   nothing, and a result stranded by eviction survives at the head of the
//!   conversation where it suppresses the inserted user turn.
//! * Unique across the process, because one daemon runs many runs at once and
//!   the ids meet each other: an interaction request is named after the call it
//!   is about, and the hub that holds open requests is one per daemon.
//! * Unique across a restart, because a run outlives the daemon: a pause and
//!   resume restores a window full of ids minted before, and a counter that
//!   starts again at zero collides with them.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// A tool-call id nothing else in this process will mint.
///
/// `<label>_<prefix>_<sequence>`: the sequence is process-wide rather than
/// per-provider or per-response, and the prefix is minted once per process, so
/// two provider objects cannot hand out the same id and neither can two
/// processes. The sequence is monotonic, so a transcript still reads in call
/// order.
pub(crate) fn mint(label: &str) -> String {
    static PREFIX: OnceLock<String> = OnceLock::new();
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let prefix = PREFIX.get_or_init(|| {
        use rand::RngExt as _;
        format!("{:08x}", rand::rng().random::<u32>())
    });
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{label}_{prefix}_{sequence}")
}

#[cfg(test)]
#[path = "call_ids_tests.rs"]
mod tests;

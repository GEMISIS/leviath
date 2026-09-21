//! Execution identity: what a run did, told apart from what it was asked to do.
//!
//! A tool call is an intended operation: a tool name and its arguments. An
//! execution is one attempt to carry that out. The two were the same thing here
//! until now, because the journal used the provider's own call id as identity,
//! and a provider is under no obligation to make those unique: a retried or
//! reissued call can arrive with the id an earlier one had. Two attempts sharing
//! an id cannot be told apart afterwards, which is exactly the question a run
//! debugger exists to answer.
//!
//! So an execution gets an id this crate mints, and the provider's id travels
//! beside it as correlation. Every record about one attempt carries the
//! execution id; anything a provider says about it is matched through the
//! correlation id, which may repeat.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// How one attempt to execute a tool call ended.
///
/// The unknown case is a state rather than an absence. A run that crashed
/// between dispatch and completion left a call whose outcome nobody observed,
/// and a missing completion is not evidence of success or of safe retry: it is
/// the one fact there is, and recording it is what lets a person decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    /// The tool ran and answered.
    Succeeded,
    /// The tool ran and failed.
    Failed,
    /// A gate refused it before it ran: the taint gate, a permission rule.
    Blocked,
    /// A person refused it.
    Denied,
    /// Nobody observed how it ended. A crash between dispatch and completion
    /// leaves this, and no amount of reading the logs turns it into one of the
    /// four above.
    Indeterminate,
}

impl ToolOutcome {
    /// The word this outcome goes on the wire as.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Denied => "denied",
            Self::Indeterminate => "indeterminate",
        }
    }
}

/// Distinguishes two executions minted inside one nanosecond.
static MINTED: AtomicU64 = AtomicU64::new(0);

/// Mint an id for one execution attempt.
///
/// Unique within a machine's lifetime, and ordered: the clock leads, so the ids
/// sort into the order they were minted and a person reading two of them can tell
/// which came first. The counter is what keeps two minted inside one nanosecond
/// apart, which is a thing that happens when a batch dispatches.
///
/// Not a hash of the call: two attempts at the same call are two executions, and
/// an id derived from the call would make them one again.
pub fn mint_execution_id() -> String {
    format!("x{}", minted_suffix())
}

/// The clock and the counter, in a form that sorts.
///
/// Both parts are fixed width and separated. Concatenating two hex numbers of
/// whatever width they happened to need does not sort: the sixteenth id of a
/// nanosecond reads as `10` and lands before the fifteenth's `f`, and the two
/// halves cannot be told apart again afterwards either.
fn minted_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default();
    let seq = MINTED.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:016x}-{seq:08x}")
}

/// Mint an id for one stay in a stage.
///
/// The correlation key for everything that happened during that stay. It has to
/// be minted rather than taken from a visit's position in the stage ledger,
/// because that list is capped: the hundred and twenty-ninth visit would take
/// the first one's identity, and everything correlated to it would move.
pub fn mint_visit_id() -> String {
    format!("v{}", minted_suffix())
}

/// Mint an id for one trip to a provider.
///
/// What the answer's consequences name it by. A tool batch records the attempt
/// whose answer asked for it, and neither the stage nor the attempt number can
/// serve: a stage makes hundreds of trips, and the number restarts at every
/// call.
pub fn mint_attempt_id() -> String {
    format!("a{}", minted_suffix())
}

#[cfg(test)]
mod tests {
    use super::{ToolOutcome, mint_execution_id, mint_visit_id};

    /// Two ids minted in a row are different, and they sort into the order they
    /// were minted.
    ///
    /// A batch mints several inside one nanosecond, which is what the counter is
    /// for: without it a dispatch of four calls could hand two of them one id.
    #[test]
    fn minted_ids_are_unique_and_ordered() {
        let ids: Vec<String> = (0..64).map(|_| mint_execution_id()).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "no two alike: {ids:?}");
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(sorted, ids, "minted in order, so they read in order");
        // Both halves stay readable: a person comparing two ids can see which
        // nanosecond each came from and which of that nanosecond's it was.
        let (clock, seq) = ids[0]
            .strip_prefix('x')
            .expect("an execution id says which kind it is")
            .split_once('-')
            .expect("two parts");
        assert_eq!(clock.len(), 16, "{clock}");
        assert_eq!(seq.len(), 8, "{seq}");
    }

    /// Each kind of id is told apart by its first character, so one pasted
    /// where another belongs is visibly wrong.
    #[test]
    fn the_kinds_of_id_are_told_apart_on_sight() {
        assert!(mint_execution_id().starts_with('x'));
        assert!(mint_visit_id().starts_with('v'));
        assert!(super::mint_attempt_id().starts_with('a'));
    }

    /// Every outcome has its own word.
    #[test]
    fn each_outcome_has_its_own_word() {
        assert_eq!(ToolOutcome::Succeeded.wire(), "succeeded");
        assert_eq!(ToolOutcome::Failed.wire(), "failed");
        assert_eq!(ToolOutcome::Blocked.wire(), "blocked");
        assert_eq!(ToolOutcome::Denied.wire(), "denied");
        assert_eq!(ToolOutcome::Indeterminate.wire(), "indeterminate");
    }

    /// The outcome round-trips through the journal's own encoding.
    #[test]
    fn an_outcome_round_trips_as_its_word() {
        for outcome in [
            ToolOutcome::Succeeded,
            ToolOutcome::Failed,
            ToolOutcome::Blocked,
            ToolOutcome::Denied,
            ToolOutcome::Indeterminate,
        ] {
            let json = serde_json::to_string(&outcome).expect("it serializes");
            assert_eq!(json, format!("\"{}\"", outcome.wire()));
            let back: ToolOutcome = serde_json::from_str(&json).expect("it reads back");
            assert_eq!(back, outcome);
        }
    }
}

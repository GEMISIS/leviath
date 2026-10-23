//! The loop guard: a model that keeps making calls it has already made, and
//! keeps getting the answers it already has, is nudged once, then its stage is
//! ended.
//!
//! Two shapes of loop are counted. The plain one is the same batch of calls,
//! batch after batch. The other goes round: `list_dir .`, `read_file
//! README.md`, `list_dir .` again, each batch different from the one before
//! it, so a guard that only compares neighbours never fires, while nothing
//! new comes back. That second count waits until a call has been answered the
//! same way twice before, so reading a file again after it left the context
//! is not a loop.

use std::collections::HashMap;

/// What the loop guard has decided about a stage, pending action.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum LoopTrip {
    /// Nothing to do.
    #[default]
    None,
    /// A loop reached [`LOOP_REPEAT_LIMIT`]: nudge the model, naming the tool.
    Nudge(String),
    /// The model went on looping after the nudge: end the stage.
    Stuck(String),
}

/// What the loop guard has seen in a stage.
#[derive(Debug, Clone, Default)]
pub(crate) struct LoopGuard {
    /// A hash of the last batch of calls and what they returned, while the
    /// batches since the last different one have all been alike.
    pub last: Option<u64>,
    /// How many batches in a row were that batch.
    pub streak: usize,
    /// How many times each call (tool, arguments and result) has been made
    /// since the stage began or last changed a file.
    pub seen: HashMap<u64, usize>,
    /// How many batches in a row held only calls already answered the same
    /// way at least [`STALE_AFTER`] times.
    pub stale: usize,
    /// Whether the guard has already nudged this stage. It nudges once, and
    /// only counts a nudge that was delivered.
    pub nudged: bool,
    /// What it found in the last batch, waiting for the next `ReadyToInfer`
    /// tick to act on: the nudge goes in after the tool results, never between
    /// a call and its result.
    pub trip: LoopTrip,
}

/// How many batches in a row may loop before the model is nudged to stop. One
/// more after the nudge ends the stage.
pub(crate) const LOOP_REPEAT_LIMIT: usize = 3;

/// How many times a call must already have been answered the same way before
/// making it again counts toward a loop that goes round. Once is allowed: a
/// result that left the context may be read again.
pub(crate) const STALE_AFTER: usize = 2;

/// Tools whose repeated, identical call is the point: waiting on another run,
/// polling a status, and the fan-out retries a stage makes by design.
fn polls(name: &str) -> bool {
    matches!(name, "wait_for_agent" | "fan_out")
        || name.starts_with("wait_")
        || name.ends_with("_status")
        || name.contains("poll")
}

/// A hash of `value`.
fn hash_of(value: &impl std::hash::Hash) -> u64 {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

impl LoopGuard {
    /// Look at one batch of tool calls and what they returned.
    ///
    /// Only a loop is counted: the same calls with the same results, batch
    /// after batch, or batches that only make calls already answered the same
    /// way [`STALE_AFTER`] times in this stage. A batch with anything new in it
    /// starts the count again, and a batch that polls or that names a tool
    /// able to change files forgets everything seen, since the same command
    /// run after an edit is not the same question. Calls repeated inside one
    /// batch are one call. Sets [`Self::trip`] when either count reaches
    /// [`LOOP_REPEAT_LIMIT`] (nudge) or goes on after the nudge was delivered
    /// (stuck).
    pub(crate) fn observe(
        &mut self,
        calls: &[crate::components::ToolCall],
        results: &[crate::tool_bridge::ToolResult],
        modifying: &[String],
    ) {
        let exempt = calls.iter().any(|c| {
            let name = leviath_tools::canonical_tool_name(&c.name);
            polls(name) || modifying.iter().any(|m| m == name)
        });
        if calls.is_empty() || exempt {
            self.last = None;
            self.streak = 0;
            self.seen.clear();
            self.stale = 0;
            return;
        }
        let mut ones: Vec<u64> = calls
            .iter()
            .zip(results.iter())
            .map(|(call, (_id, result))| {
                hash_of(&(
                    call.name.as_str(),
                    call.arguments.to_string(),
                    result.as_str(),
                ))
            })
            .collect();
        ones.sort_unstable();
        ones.dedup();
        let batch = hash_of(&ones);
        self.streak = match self.last == Some(batch) {
            true => self.streak + 1,
            false => 1,
        };
        self.last = Some(batch);
        let stale = ones
            .iter()
            .all(|one| self.seen.get(one).is_some_and(|n| *n >= STALE_AFTER));
        self.stale = match stale {
            true => self.stale + 1,
            false => 0,
        };
        for one in ones {
            *self.seen.entry(one).or_default() += 1;
        }
        let name = calls[0].name.as_str();
        let count = self.streak.max(self.stale);
        if self.nudged && count > LOOP_REPEAT_LIMIT {
            self.trip = LoopTrip::Stuck(match self.streak >= self.stale {
                true => format!(
                    "`{name}` was called {count} times in a row with identical arguments and \
                     results"
                ),
                false => format!(
                    "{count} batches in a row, ending with `{name}`, only repeated calls whose \
                     results the stage already had"
                ),
            });
        } else if !self.nudged && count == LOOP_REPEAT_LIMIT {
            self.trip = LoopTrip::Nudge(name.to_string());
        }
    }
}

/// The nudge sent when the loop guard first trips.
pub(crate) fn loop_nudge(tool: &str) -> String {
    format!(
        "Your last {LOOP_REPEAT_LIMIT} batches of tool calls, ending with `{tool}`, only repeated \
         calls you had already made and got the same results you already have. Repeating them \
         will not change the answer. Use what you already have: do the stage's actual work now \
         (write the regions or output it asks for), or change your approach."
    )
}

/// The path a tool call targets, for per-stage edit-churn tracking. Only the two
/// mutating file tools count: both carry the path in their `path` argument. A
/// call without a string `path` (or any other tool) contributes nothing.
pub(crate) fn edited_path(call: &crate::components::ToolCall) -> Option<&str> {
    matches!(call.name.as_str(), "write_file" | "edit_file")
        .then(|| call.arguments.get("path").and_then(|v| v.as_str()))
        .flatten()
}

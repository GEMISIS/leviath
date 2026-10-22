//! The loop guard: a model that makes the identical tool call over and over
//! is nudged once, then its stage is ended.

/// What the loop guard has decided about a stage, pending action.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum LoopTrip {
    /// Nothing to do.
    #[default]
    None,
    /// An identical call hit [`LOOP_REPEAT_LIMIT`]: nudge the model, naming the tool.
    Nudge(String),
    /// The model repeated the call after the nudge: end the stage.
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
    /// Whether the guard has already nudged this stage. It nudges once, and
    /// only counts a nudge that was delivered.
    pub nudged: bool,
    /// What it found in the last batch, waiting for the next `ReadyToInfer`
    /// tick to act on: the nudge goes in after the tool results, never between
    /// a call and its result.
    pub trip: LoopTrip,
}

/// How many batches in a row may be the same calls with the same results
/// before the model is nudged to stop. One more after the nudge ends the stage.
pub(crate) const LOOP_REPEAT_LIMIT: usize = 3;

/// Tools whose repeated, identical call is the point: waiting on another run,
/// polling a status, and the fan-out retries a stage makes by design.
fn polls(name: &str) -> bool {
    matches!(name, "wait_for_agent" | "fan_out")
        || name.starts_with("wait_")
        || name.ends_with("_status")
        || name.contains("poll")
}

impl LoopGuard {
    /// Look at one batch of tool calls and what they returned.
    ///
    /// Only a loop is counted: the same calls with the same results, batch
    /// after batch. Any different batch starts the count again, and so does
    /// a batch that polls or that names a tool able to change files, since
    /// the same command run after an edit is not the same question. Calls
    /// repeated inside one batch are one call. Sets [`Self::trip`] when the
    /// streak reaches [`LOOP_REPEAT_LIMIT`] (nudge) or goes on after the
    /// nudge was delivered (stuck).
    pub(crate) fn observe(
        &mut self,
        calls: &[crate::components::ToolCall],
        results: &[crate::tool_bridge::ToolResult],
        modifying: &[String],
    ) {
        use std::hash::{Hash, Hasher};
        let exempt = calls.iter().any(|c| {
            let name = leviath_tools::canonical_tool_name(&c.name);
            polls(name) || modifying.iter().any(|m| m == name)
        });
        if calls.is_empty() || exempt {
            self.last = None;
            self.streak = 0;
            return;
        }
        let mut seen = std::collections::HashSet::new();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (call, (_id, result)) in calls.iter().zip(results.iter()) {
            let one = (
                call.name.as_str(),
                call.arguments.to_string(),
                result.as_str(),
            );
            if !seen.contains(&one) {
                one.hash(&mut hasher);
                seen.insert(one);
            }
        }
        let batch = hasher.finish();
        self.streak = match self.last == Some(batch) {
            true => self.streak + 1,
            false => 1,
        };
        self.last = Some(batch);
        let name = calls[0].name.as_str();
        if self.nudged && self.streak > LOOP_REPEAT_LIMIT {
            self.trip = LoopTrip::Stuck(format!(
                "`{name}` was called {} times in a row with identical arguments and results",
                self.streak
            ));
        } else if !self.nudged && self.streak == LOOP_REPEAT_LIMIT {
            self.trip = LoopTrip::Nudge(name.to_string());
        }
    }
}

/// The nudge sent when the loop guard first trips.
pub(crate) fn loop_nudge(tool: &str) -> String {
    format!(
        "You have called `{tool}` with the same arguments {LOOP_REPEAT_LIMIT} times and got the \
         same result each time. Repeating it will not change the answer. Use what you already \
         have: do the stage's actual work now (write the regions or output it asks for), or \
         change your approach."
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

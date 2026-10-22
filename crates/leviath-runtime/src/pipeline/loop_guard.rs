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
    /// How many times each distinct tool call (name and arguments) has been
    /// requested in this stage.
    pub repeats: std::collections::HashMap<String, usize>,
    /// Whether the guard has already nudged this stage. It nudges once.
    pub nudged: bool,
    /// What it found in the last reply, waiting for the next `ReadyToInfer`
    /// tick to act on: the nudge goes in after the tool results, never between
    /// a call and its result.
    pub trip: LoopTrip,
}

/// How many times the same tool call (same tool, same arguments) may be made in
/// one stage before the model is nudged to stop. One more repeat after the
/// nudge ends the stage.
pub(crate) const LOOP_REPEAT_LIMIT: usize = 3;

/// Count a requested call and set [`LoopGuard::trip`] when it has been
/// made [`LOOP_REPEAT_LIMIT`] times (nudge) or once more after the nudge (stuck).
pub(crate) fn record_call_repeat(guard: &mut LoopGuard, call: &crate::components::ToolCall) {
    let key = format!("{}\u{0}{}", call.name, call.arguments);
    let count = guard.repeats.entry(key).or_insert(0);
    *count += 1;
    let count = *count;
    if guard.nudged && count > LOOP_REPEAT_LIMIT {
        guard.trip = LoopTrip::Stuck(format!(
            "`{}` was called {count} times with identical arguments",
            call.name
        ));
    } else if !guard.nudged && count >= LOOP_REPEAT_LIMIT {
        guard.nudged = true;
        guard.trip = LoopTrip::Nudge(call.name.clone());
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

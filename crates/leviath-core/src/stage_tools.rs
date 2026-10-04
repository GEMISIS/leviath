//! Names of the stage-control tools the engine itself handles.
//!
//! They live at the bottom of the stack because the tools crate, the run graph
//! and the manifest parser all need them, and the tools crate sits below the
//! runtime that owns the graph.

/// The tool an agent calls to hand back the run's final output.
pub const SUBMIT_OUTPUT_TOOL: &str = "submit_output";

/// The tool that starts a fan-out: one worker per item, all at once.
///
/// The single entry point to the fan-out engine. A `mode = "fan_out"` stage is
/// sugar that grants this tool and transitions to its `merge_stage` once the
/// call returns; any other stage can grant it directly and fan out in the
/// middle of its own work, as many times as it needs. A structured tool call
/// carries the work items, so they are never parsed out of prose.
pub const FAN_OUT_TOOL: &str = "fan_out";

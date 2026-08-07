//! The `[limits]` step, and the values a fresh install is given.
//!
//! The one place in the wizard where an empty field does **not** mean "keep the
//! default": the two write ceilings default to unset in code and are written
//! with concrete numbers here, so deleting the line in `config.toml` is how a
//! user removes the limit rather than how they accept one.

use super::*;

/// The Limits screen's fields, seeded from a config.
pub(super) fn limits_fields(config: &Config) -> Vec<Field> {
    vec![
        Field {
            label: "Max concurrent inferences",
            help: "How many model calls run at once across all agents.",
            value: FieldValue::Number(config.limits.max_concurrent_inferences.map(|n| n as u64)),
        },
        Field {
            label: "Max concurrent tools",
            help: "How many tool calls run at once within one batch.",
            value: FieldValue::Number(Some(config.limits.max_concurrent_tools as u64)),
        },
        Field {
            label: "Default max iterations",
            help: "Per-stage ceiling when a blueprint sets none.",
            value: FieldValue::Number(config.limits.default_max_iterations.map(|n| n as u64)),
        },
        Field {
            label: "Exact token counting",
            help: "Ask the provider for real token counts instead of estimating. Slower.",
            value: FieldValue::Bool(config.limits.exact_token_counting),
        },
        Field {
            label: "Batch tool-call hint",
            help: "Nudge models to request several tools in one turn.",
            value: FieldValue::Bool(config.batch_tool_hint),
        },
        Field {
            label: "Platform shell hint",
            help: "Tell models what shell they get. Only says anything on Windows (cmd.exe).",
            value: FieldValue::Bool(config.shell_hint),
        },
        Field {
            label: "Stall timeout (seconds)",
            help: "Fail a run that can never dispatch (unconfigured provider). 0 waits forever.",
            value: FieldValue::Number(Some(config.limits.stall_timeout_secs)),
        },
        Field {
            label: "Dead cycles before relief",
            help: "Widen the tool lane after this many 30s cycles with work queued and nothing moving. 0 never does.",
            value: FieldValue::Number(Some(config.limits.dead_cycles_before_relief as u64)),
        },
        Field {
            label: "Finished run retention (seconds)",
            help: "Keep a run in `lev ps` this long after it ends, so a script polling on an interval sees how it ended. 0 drops it at once.",
            value: FieldValue::Number(Some(config.limits.finished_retention_secs)),
        },
        Field {
            label: "Wedge timeout (seconds)",
            help: "Fail a run nothing in the engine can reach any more. 0 is off; 300 is a sensible value.",
            value: FieldValue::Number(Some(config.limits.wedge_timeout_secs)),
        },
        Field {
            label: "Interaction timeout (seconds)",
            help: "Resolve a prompt nobody answered after this long, so the run carries on. 0 waits for ever.",
            value: FieldValue::Number(Some(config.limits.interaction_timeout_secs)),
        },
        // The two write ceilings are unset in code and offered with a value
        // here, so a fresh install has one written down where it can be seen
        // and cleared. Clearing the field stores nothing, which is unlimited.
        Field {
            label: "Max bytes one tool call may write",
            help: "Stops a single command that writes until the disk fills. Clear it for no limit.",
            value: FieldValue::Number(Some(
                config
                    .limits
                    .max_tool_call_write_bytes
                    .unwrap_or(SUGGESTED_CALL_WRITE_BYTES),
            )),
        },
        Field {
            label: "Max bytes one run may write",
            help: "The same ceiling across a whole run, which several ordinary-looking calls can reach. Clear it for no limit.",
            value: FieldValue::Number(Some(
                config
                    .limits
                    .max_run_write_bytes
                    .unwrap_or(SUGGESTED_RUN_WRITE_BYTES),
            )),
        },
    ]
}

/// What `lev setup` offers as a per-call write ceiling.
///
/// 2 GiB is far above anything a tool call legitimately writes - a large build
/// log, a full database dump - and far below the 14 GB a single runaway append
/// reached in the incident this exists for. The gap between those two numbers
/// is wide enough that the value does not need to be right, only present.
const SUGGESTED_CALL_WRITE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// What `lev setup` offers as a per-run write ceiling.
///
/// Five times the per-call figure, so a run doing genuinely heavy work has room
/// for several large writes while three runaway calls still stop.
const SUGGESTED_RUN_WRITE_BYTES: u64 = 5 * SUGGESTED_CALL_WRITE_BYTES;

/// Write the Limits screen's fields back into a config.
pub(super) fn apply_limits_fields(config: &mut Config, fields: &[Field]) {
    for (index, field) in fields.iter().enumerate() {
        match (index, &field.value) {
            (0, FieldValue::Number(n)) => {
                config.limits.max_concurrent_inferences = n.map(|n| n as usize)
            }
            // A zero here would deadlock every tool batch, so an explicit unset
            // or 0 falls back to the default rather than being stored.
            (1, FieldValue::Number(n)) => {
                config.limits.max_concurrent_tools = n
                    .filter(|n| *n > 0)
                    .map(|n| n as usize)
                    .unwrap_or(Config::default().limits.max_concurrent_tools)
            }
            (2, FieldValue::Number(n)) => {
                config.limits.default_max_iterations = n.map(|n| n as usize)
            }
            (3, FieldValue::Bool(b)) => config.limits.exact_token_counting = *b,
            (4, FieldValue::Bool(b)) => config.batch_tool_hint = *b,
            (5, FieldValue::Bool(b)) => config.shell_hint = *b,
            // Unset means "leave the watchdog at its default", not "disable it";
            // disabling is an explicit 0.
            (6, FieldValue::Number(n)) => {
                config.limits.stall_timeout_secs =
                    n.unwrap_or(Config::default().limits.stall_timeout_secs)
            }
            // Same rule: unset keeps the default, 0 is an explicit "never".
            (7, FieldValue::Number(n)) => {
                config.limits.dead_cycles_before_relief = n
                    .map(|n| n as u32)
                    .unwrap_or(Config::default().limits.dead_cycles_before_relief)
            }
            // And again: unset keeps the default, 0 means keep nothing.
            (8, FieldValue::Number(n)) => {
                config.limits.finished_retention_secs =
                    n.unwrap_or(Config::default().limits.finished_retention_secs)
            }
            // Same rule once more, and here the default is itself 0 (off).
            (9, FieldValue::Number(n)) => {
                config.limits.wedge_timeout_secs =
                    n.unwrap_or(Config::default().limits.wedge_timeout_secs)
            }
            // Same rule again: unset keeps the default hour, 0 is an explicit
            // "wait for a person however long it takes".
            (10, FieldValue::Number(n)) => {
                config.limits.interaction_timeout_secs =
                    n.unwrap_or(Config::default().limits.interaction_timeout_secs)
            }
            // The write ceilings break the rule above, and deliberately: here
            // an unset field means *no limit*, not "keep the default". They are
            // the only two settings whose absence is a real choice a user makes
            // - deleting the line is how you say "let it write" - so unset
            // stores `None` rather than reinstating a number they just removed.
            (11, FieldValue::Number(n)) => config.limits.max_tool_call_write_bytes = *n,
            (12, FieldValue::Number(n)) => config.limits.max_run_write_bytes = *n,
            _ => {}
        }
    }
}

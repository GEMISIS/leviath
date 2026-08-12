//! What still stops a `--yolo` run for a person.
//!
//! `--yolo` means "run without me", so a run that stops anyway looks like a
//! hang. It is not: a blueprint can declare that a particular checkpoint needs a
//! person however the run was launched, and the flagship `coder`
//! does exactly that for its plan approval, because everything after that gate
//! writes code.
//!
//! Two mechanisms say it, and they answer different questions, so they are not
//! merged. `unattended = "ask"` on an interaction point is a checkpoint the
//! framework *always* raises at a stage boundary. `required_tools` keeps a
//! blocking tool the model *may choose* to call. A verification agent that needs
//! "here is a fact, is it right?" to be guaranteed uses the first; one happy to
//! let the model decide when to ask uses the second.
//!
//! This module reports both, so the wait is announced before the run starts
//! rather than discovered twenty minutes later.

use leviath_core::Blueprint;
use leviath_core::blueprint::{StageMode, UnattendedPolicy};

/// One thing in a blueprint that will still stop a `--yolo` run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    /// The stage it belongs to.
    pub stage: String,
    /// The interaction point's name, or the tool's.
    pub name: String,
}

/// Every interaction point declaring `unattended = "ask"`, in stage order.
pub fn held_points(blueprint: &Blueprint) -> Vec<Held> {
    blueprint
        .stages
        .iter()
        .flat_map(|stage| {
            let points = match &stage.mode {
                StageMode::InteractivePoints { points } => points.as_slice(),
                _ => &[],
            };
            points
                .iter()
                .filter(|p| p.unattended == UnattendedPolicy::Ask)
                .map(|p| Held {
                    stage: stage.name.clone(),
                    name: p.name.clone(),
                })
        })
        .collect()
}

/// Every blocking human tool a stage keeps through `required_tools`.
///
/// Canonicalised, because the runtime matches on the name the model calls and a
/// manifest may write either spelling.
pub fn held_tools(blueprint: &Blueprint) -> Vec<Held> {
    blueprint
        .stages
        .iter()
        .flat_map(|stage| {
            stage
                .required_tools
                .iter()
                .filter(|t| {
                    leviath_runtime::dynamic_interaction::BLOCKING_INTERACTION_TOOLS
                        .contains(&leviath_tools::canonical_tool_name(t))
                })
                .map(|tool| Held {
                    stage: stage.name.clone(),
                    name: tool.clone(),
                })
        })
        .collect()
}

/// Render `secs` as the operator would write it, so the wait is a duration
/// rather than a number to divide.
fn human_timeout(secs: u64) -> String {
    match secs {
        0 => "indefinitely".to_string(),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// The stderr block for a `--yolo` spawn. Empty when nothing holds.
///
/// Pure, so the wording is testable without a daemon or a manifest on disk.
pub fn preflight_lines(blueprint: &Blueprint, timeout_secs: u64) -> Vec<String> {
    let points = held_points(blueprint);
    let tools = held_tools(blueprint);
    if points.is_empty() && tools.is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::new();
    let total = points.len() + tools.len();
    let plural = if total == 1 { "" } else { "s" };
    lines.push(format!(
        "--yolo will still stop for a person at {total} checkpoint{plural}:"
    ));
    for p in &points {
        lines.push(format!("  {}: {}", p.stage, p.name));
    }
    for t in &tools {
        lines.push(format!("  {}: {} (if the model calls it)", t.stage, t.name));
    }
    lines.push(match timeout_secs {
        0 => "  nothing expires these; the run waits until somebody answers".to_string(),
        secs => format!(
            "  unanswered after {}, the run stops with an error; `lev respond` lists them",
            human_timeout(secs)
        ),
    });
    lines
}

#[cfg(test)]
mod tests;

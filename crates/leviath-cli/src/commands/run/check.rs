//! `lev run --check`: ask the daemon what a run would be, without starting
//! it.
//!
//! The daemon resolves the request the whole way, the same as for a spawn,
//! and answers with a summary of the run it would start or with every problem
//! it found. The summary names the stage the run starts in, each stage's
//! model and tools, the checked inputs and what the run is trusted with; a
//! problem is printed one per line with its path.

use anyhow::bail;
use leviath_runtime::control_socket::{ControlClient, ControlResponse};
use leviath_runtime::spec::inputs::InputValue;
use leviath_runtime::spec::launch::Unattended;
use leviath_runtime::spec::run_spec::SpecOrigin;
use leviath_runtime::spec::summary::SpawnSummary;

use super::request::issues_report;
use crate::daemon::client::LocalRun;

/// Ask the daemon to validate `run`, and print what it would be: the
/// summary, or (as JSON with `json`) the summary or the issues. A refusal is
/// an error naming every problem, one per line.
pub async fn send_check(client: &ControlClient, run: &LocalRun, json: bool) -> anyhow::Result<()> {
    match client.validate_spawn(run.request.clone()).await {
        Ok(ControlResponse::Valid { summary }) => {
            println!("{}", check_report(&summary, json));
            Ok(())
        }
        Ok(ControlResponse::Rejected { issues }) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&issues).expect("issues serialize")
                );
            }
            bail!(issues_report(&issues))
        }
        Ok(ControlResponse::Error { message }) => bail!("the check failed: {message}"),
        Ok(other) => bail!("unexpected daemon response: {other:?}"),
        Err(e) => bail!("the leviath daemon is not reachable ({e}); start it with `lev daemon`"),
    }
}

/// The summary as printed: JSON, or a short block a person reads.
pub(crate) fn check_report(summary: &SpawnSummary, json: bool) -> String {
    if json {
        return serde_json::to_string_pretty(summary).expect("a summary serializes");
    }
    let mut lines = vec![format!(
        "{} would run ({})",
        summary.title,
        origin(&summary.origin)
    )];
    lines.push(format!("  starts in: {}", summary.entry_stage));
    lines.push(format!("  workdir:   {}", summary.workdir.display()));
    let launch = &summary.launch;
    let unattended = match &launch.unattended {
        Unattended::Off => "no".to_string(),
        Unattended::All => "yes".to_string(),
        Unattended::Profile(name) => format!("under the '{name}' profile"),
    };
    lines.push(format!("  unattended: {unattended}"));
    if !launch.allow.is_empty() {
        let allow: Vec<&str> = launch.allow.iter().map(|t| t.as_str()).collect();
        lines.push(format!("  allowed:   {}", allow.join(", ")));
    }
    lines.push(format!("  max depth: {}", launch.max_depth));
    if !summary.inputs.0.is_empty() {
        lines.push("  inputs:".to_string());
        for (name, value) in &summary.inputs.0 {
            lines.push(format!("    {name} = {}", shown(value)));
        }
    }
    lines.push("  stages:".to_string());
    for stage in &summary.stages {
        let tools = match stage.tools.is_empty() {
            true => "no tools".to_string(),
            false => {
                let names: Vec<&str> = stage.tools.iter().map(|t| t.as_str()).collect();
                format!("tools: {}", names.join(", "))
            }
        };
        lines.push(format!(
            "    {}  {}/{}  {tools}",
            stage.stage, stage.provider, stage.model
        ));
    }
    lines.join("\n")
}

/// Where a run's graph comes from, in a few words.
fn origin(origin: &SpecOrigin) -> String {
    match origin {
        SpecOrigin::Blueprint { blueprint, version } => {
            format!("blueprint {blueprint}, version {version}")
        }
        SpecOrigin::BlueprintFile { path, version, .. } => {
            format!("blueprint in {path}, version {version}")
        }
        SpecOrigin::Raw => "a graph from the request".to_string(),
    }
}

/// An input's value on one line: text quoted and cut short, anything else as
/// it renders.
fn shown(value: &InputValue) -> String {
    let text = value.render_text().replace('\n', " ");
    let short: String = text.chars().take(60).collect();
    let cut = match short.len() < text.len() {
        true => "...",
        false => "",
    };
    match value {
        InputValue::Text(_) => format!("{short:?}{cut}"),
        _ => format!("{short}{cut}"),
    }
}

#[cfg(test)]
#[path = "check_tests.rs"]
mod tests;

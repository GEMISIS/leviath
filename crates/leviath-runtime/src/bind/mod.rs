//! Binding: the live handles a resolved run needs, checked against the
//! machine it is about to run on.
//!
//! A [`RunSpec`] was decided once, when the run was resolved, and it records
//! what it relied on from the machine in its [`EnvFingerprint`]. Binding
//! never decides anything again. It asks the host whether every provider and
//! MCP server the run used is still there and still the same, checks that the
//! run file holds every piece of code the spec names, and only then asks the
//! host ([`BindEnv::bind`]) for the live components.
//!
//! Every problem is reported at once, each at the place in the spec that
//! depends on it, so a resume that cannot go ahead says everything that
//! changed in one answer.
//!
//! An empty fingerprint map means the run did not record that half of its
//! machine (a run converted from an older format), so there is nothing to
//! compare and the check is skipped. A run that uses no provider or no MCP
//! server records an empty map too, and skipping is the right answer there as
//! well.
//!
//! [`EnvFingerprint`]: crate::spec::run_spec::EnvFingerprint

use std::collections::{BTreeMap, BTreeSet};

use crate::spec::env::{BindEnv, Bindings, CodeFiles};
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::names::{Digest, McpServerName, ProviderName};
use crate::spec::run_spec::{RunSpec, ToolDef, ToolSource};

pub mod host;
pub mod scripts;

pub use scripts::{RegionScripts, code_key};

/// Bind a resolved run: check the machine still matches what the run
/// recorded, then build the host's live components for it.
pub async fn bind(
    spec: &RunSpec,
    code: &CodeFiles,
    env: &dyn BindEnv,
) -> Result<Bindings, SpawnIssues> {
    let mut issues = SpawnIssues::new();
    check_providers(spec, env, &mut issues);
    check_mcp_servers(spec, env, &mut issues);
    check_code(spec, code, &mut issues);
    if !issues.is_empty() {
        return Err(issues);
    }
    env.bind(spec, code).await
}

/// Every place in the spec a provider is used: each stage that runs on it,
/// and each stage that fails over to it. A provider nothing names any more
/// (the compaction model's, say) is reported against the fingerprint itself.
fn provider_paths(spec: &RunSpec, provider: &ProviderName) -> Vec<SpecPath> {
    let mut paths = Vec::new();
    for plan in &spec.stages {
        let stage = SpecPath::root().field("stages").key(plan.stage.as_str());
        if plan.provider == *provider {
            paths.push(stage.field("provider"));
        }
        let falls_back = plan
            .fallbacks
            .iter()
            .any(|f| f.provider.as_ref() == Some(provider));
        if falls_back {
            paths.push(stage.field("fallbacks"));
        }
    }
    if paths.is_empty() {
        paths.push(
            SpecPath::root()
                .field("env")
                .field("providers")
                .key(provider.as_str()),
        );
    }
    paths
}

fn check_providers(spec: &RunSpec, env: &dyn BindEnv, issues: &mut SpawnIssues) {
    let configured = env.providers_now();
    for (provider, recorded) in &spec.env.providers {
        let issue = match env.provider_fingerprint(provider) {
            None => (
                IssueCode::Unavailable,
                format!("provider '{provider}' is no longer configured on this machine"),
                format!("configure '{provider}' again, or start a new run"),
            ),
            Some(now) if now == *recorded => continue,
            Some(_) => (
                IssueCode::Changed,
                format!(
                    "provider '{provider}' is configured differently from when the run \
                     started: the run was started against a different configuration"
                ),
                format!(
                    "put '{provider}' back the way it was (its kind, base URL and model \
                     list), or start a new run"
                ),
            ),
        };
        let (code, message, hint) = issue;
        for path in provider_paths(spec, provider) {
            issues.push(
                SpawnIssue::new(path, code, message.clone())
                    .hint(hint.clone())
                    .known(configured.iter()),
            );
        }
    }
}

/// The MCP tools the run's stages were given from `server`, by the tool's
/// own name on that server, with the stages that got each one.
fn recorded_mcp_tools<'a>(
    spec: &'a RunSpec,
    server: &McpServerName,
) -> BTreeMap<&'a str, (&'a ToolDef, Vec<&'a str>)> {
    let mut tools: BTreeMap<&str, (&ToolDef, Vec<&str>)> = BTreeMap::new();
    for plan in &spec.stages {
        for def in &plan.tools {
            if let ToolSource::Mcp { server: s, tool } = &def.source
                && s == server
            {
                tools
                    .entry(tool.as_str())
                    .or_insert((def, Vec::new()))
                    .1
                    .push(plan.stage.as_str());
            }
        }
    }
    tools
}

/// What differs between the tools the run recorded and the ones the server
/// offers now, as a clause per kind of difference.
fn mcp_differences(
    recorded: &BTreeMap<&str, (&ToolDef, Vec<&str>)>,
    now: &[ToolDef],
) -> Vec<String> {
    let now: BTreeMap<&str, &ToolDef> = now
        .iter()
        .filter_map(|def| match &def.source {
            ToolSource::Mcp { tool, .. } => Some((tool.as_str(), def)),
            _ => None,
        })
        .collect();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    for (tool, (def, _)) in recorded {
        match now.get(tool) {
            None => removed.push(*tool),
            Some(current) => {
                if current.description != def.description || current.schema != def.schema {
                    changed.push(*tool);
                }
            }
        }
    }
    let added: Vec<&str> = now
        .keys()
        .filter(|t| !recorded.contains_key(*t))
        .copied()
        .collect();
    [("removed", removed), ("changed", changed), ("added", added)]
        .into_iter()
        .filter(|(_, tools)| !tools.is_empty())
        .map(|(what, tools)| format!("{what}: {}", tools.join(", ")))
        .collect()
}

fn check_mcp_servers(spec: &RunSpec, env: &dyn BindEnv, issues: &mut SpawnIssues) {
    let configured = env.mcp_servers_now();
    for (server, recorded) in &spec.env.mcp_servers {
        let tools = recorded_mcp_tools(spec, server);
        let (code, message) = match env.mcp_fingerprint(server) {
            None => (
                IssueCode::Unavailable,
                format!("MCP server '{server}' is no longer configured or connected"),
            ),
            Some(now) if now == *recorded => continue,
            Some(_) => {
                let detail = env
                    .mcp_tools(server)
                    .map(|now| mcp_differences(&tools, &now))
                    .filter(|d| !d.is_empty())
                    .map(|d| format!(" ({})", d.join("; ")))
                    .unwrap_or_default();
                (
                    IssueCode::Changed,
                    format!(
                        "MCP server '{server}' offers a different tool list from when the \
                         run started{detail}"
                    ),
                )
            }
        };
        let stages: BTreeSet<&str> = tools
            .values()
            .flat_map(|(_, stages)| stages.iter().copied())
            .collect();
        let paths: Vec<SpecPath> = match stages.is_empty() {
            true => vec![
                SpecPath::root()
                    .field("env")
                    .field("mcp_servers")
                    .key(server.as_str()),
            ],
            false => stages
                .into_iter()
                .map(|s| SpecPath::root().field("stages").key(s).field("tools"))
                .collect(),
        };
        for path in paths {
            issues.push(
                SpawnIssue::new(path, code, message.clone())
                    .hint(format!(
                        "restore '{server}' as it was when the run started, or start a new run"
                    ))
                    .known(configured.iter()),
            );
        }
    }
}

fn check_code(spec: &RunSpec, code: &CodeFiles, issues: &mut SpawnIssues) {
    for (i, (reference, digest)) in spec.code.iter().enumerate() {
        let path = SpecPath::root().field("code").index(i);
        let named = match reference {
            crate::spec::graph::CodeRef::File(file) => format!("'{file}'"),
            crate::spec::graph::CodeRef::Inline(_) => "inline code".to_string(),
        };
        match code.get(digest) {
            None => issues.push(
                SpawnIssue::new(
                    path,
                    IssueCode::Missing,
                    format!("the run file holds no code for {named}"),
                )
                .expected(format!("code with digest {digest}"))
                .hint("the run file is incomplete; start a new run"),
            ),
            Some(bytes) if Digest::of(bytes) != *digest => issues.push(
                SpawnIssue::new(
                    path,
                    IssueCode::Changed,
                    format!("the code stored for {named} does not match its digest"),
                )
                .expected(format!("digest {digest}"))
                .got(format!("digest {}", Digest::of(bytes)))
                .hint("the run file is damaged; start a new run"),
            ),
            Some(_) => {}
        }
    }
}

#[cfg(test)]
mod tests;

//! Binding: the live handles a resolved run needs, checked against the
//! machine it is about to run on.
//!
//! A [`RunSpec`] was decided once, when the run was resolved, and it records
//! what it relied on from the machine in its [`EnvFingerprint`]. Binding
//! never decides anything again. It asks the host whether every provider and
//! MCP server the run used is still there and still the same, and whether
//! each stage's tools still run in the kind of sandbox they ran in, checks
//! that the run file holds every piece of code the spec names, and only then
//! asks the host ([`BindEnv::bind`]) for the live components.
//!
//! Every problem is reported at once, each at the place in the spec that
//! depends on it, so a resume that cannot go ahead says everything that
//! changed in one answer.
//!
//! An empty fingerprint map means the run did not record that part of its
//! machine (a run converted from an older format), so there is nothing to
//! compare and the check is skipped. A run that uses no provider or no MCP
//! server records an empty map too, and skipping is the right answer there as
//! well. A run's sandboxes are recorded by a host that runs them; one with a
//! sandbox on a host that runs none now is held, and one that ran on the
//! machine itself carries on there.
//!
//! [`EnvFingerprint`]: crate::spec::run_spec::EnvFingerprint

use std::collections::{BTreeMap, BTreeSet};

use crate::spec::env::{BindEnv, Bindings, CodeFiles};
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::names::{Digest, McpServerName, ProviderName};
use crate::spec::run_spec::{RunSpec, ToolDef, ToolSource};
use leviath_core::sandbox::SandboxKind;

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
    check_sandbox(spec, env, &mut issues);
    check_code(spec, code, &mut issues);
    check_secret(spec, env, &mut issues);
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

/// How a sandbox kind is written in settings.
fn kind_name(kind: SandboxKind) -> &'static str {
    match kind {
        SandboxKind::None => "none",
        SandboxKind::Namespace => "namespace",
        SandboxKind::Container => "container",
    }
}

/// Each stage's tools run in the kind of sandbox they ran in when the run
/// was resolved: a run never carries on less isolated, or isolated another
/// way, than it started without a person saying so.
fn check_sandbox(spec: &RunSpec, env: &dyn BindEnv, issues: &mut SpawnIssues) {
    for (stage, recorded) in &spec.env.sandbox {
        let Some(def) = spec.graph.stages.iter().find(|s| s.name == *stage) else {
            continue;
        };
        let (code, now) = match env.sandbox_kind(&spec.graph, def) {
            Some(now) if now == *recorded => continue,
            None if *recorded == SandboxKind::None => continue,
            Some(now) => (IssueCode::Changed, kind_name(now)),
            None => (
                IssueCode::Unavailable,
                "none, on a host that runs no sandboxes",
            ),
        };
        let message = format!(
            "stage '{stage}' ran its tools in a '{}' sandbox, and its sandbox here is {now}",
            kind_name(*recorded)
        );
        let path = SpecPath::root()
            .field("env")
            .field("sandbox")
            .key(stage.as_str());
        issues.push(SpawnIssue::new(path, code, message).hint(format!(
            "set the sandbox back to '{}' (the `[sandbox]` settings), or start a new run",
            kind_name(*recorded)
        )));
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

/// The webhook's signing secret, which the spec names by where the machine
/// keeps it: a run whose secret is gone would post its webhook unsigned, or
/// not at all, so it is not taken back until the secret is.
fn check_secret(spec: &RunSpec, env: &dyn BindEnv, issues: &mut SpawnIssues) {
    if let Some(secret) = spec.delivery.callback_secret()
        && !env.holds_secret(secret)
    {
        issues.push(
            SpawnIssue::new(
                SpecPath::root()
                    .field("delivery")
                    .field("callback")
                    .field("signed_with"),
                IssueCode::Unavailable,
                "the webhook's signing secret is no longer in this machine's secret store",
            )
            .expected(format!("the secret kept as '{secret}'"))
            .hint(format!(
                "put the file '{secret}' back in the secret store (the `{}` directory beside \
                 the runs directory), or cancel the run and start a new one",
                crate::secret_store::SECRETS_DIR
            )),
        );
    }
}

#[cfg(test)]
mod tests;

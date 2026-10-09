//! Answers every host gives the same way.
//!
//! The daemon and an embedder know different things about their machine, but
//! several of the questions [`ResolveEnv`] asks have one right answer given
//! what they know: which model a stage runs on over a provider registry, which
//! of a catalog's tools a stage gets, whether some code compiles for its use,
//! what type some bytes are, and how a provider's configuration or a tool list
//! is fingerprinted. Both hosts call these so they cannot drift apart.
//!
//! [`ResolveEnv`]: crate::spec::env::ResolveEnv

use std::path::Path;

use leviath_core::JsonDoc;
use leviath_providers::Tool;

use crate::pipeline::{
    ModelDefaults, ToolCatalog, ToolOwners, expand_connector_grants, filter_tools_for_stage,
    resolve_stage_route,
};
use crate::provider_creds::ProviderCreds;
use crate::providers::ProviderRegistry;
use crate::spec::env::{CodeUse, ModelPlan};
use crate::spec::graph::{CodeRef, StageDef, ToolSelector};
use crate::spec::inputs::PathKind;
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::names::{
    Digest, McpServerName, MimePattern, ModelId, ModelRef, ProviderName, ToolName, WorkdirPath,
};
use crate::spec::run_spec::{ChosenModel, ToolDef, ToolSource};

/// The window assumed for a model whose provider cannot say, matching the
/// pipeline's own fallback for percentage budgets.
const FALLBACK_WINDOW: u32 = crate::pipeline::DEFAULT_CONTEXT_WINDOW_TOKENS as u32;

/// Read the code a graph names. A file is read from `base`, the directory of
/// the blueprint that named it, and never from outside it: code is logic a
/// blueprint ships, so a path that climbs out is refused rather than read.
pub fn read_code(code: &CodeRef, base: Option<&Path>) -> Result<Vec<u8>, String> {
    match code {
        CodeRef::Inline(source) => Ok(source.clone().into_bytes()),
        CodeRef::File(file) => {
            let base = base.ok_or_else(|| {
                format!(
                    "'{file}' names a file, and only a blueprint has a directory to read \
                     one from; put the code inline instead"
                )
            })?;
            let full = base.join(file);
            if !leviath_core::resolves_within(&full, base) {
                return Err(format!(
                    "'{file}' resolves outside the blueprint's directory ({}); code must \
                     live beside the blueprint that names it",
                    base.display()
                ));
            }
            std::fs::read(&full).map_err(|e| format!("cannot read '{}': {e}", full.display()))
        }
    }
}

/// Check that some code compiles and has the entry points `used_as` calls.
///
/// A seed is only checked for being text here: running it is the check, and
/// the host that runs it compiles it first.
pub fn check_code(code: &[u8], used_as: CodeUse) -> Result<(), String> {
    let source =
        std::str::from_utf8(code).map_err(|e| format!("the code is not UTF-8 text: {e}"))?;
    let label = "code";
    let checked = match used_as {
        CodeUse::Hook => leviath_scripting::stage_hook::compile(label, source, &[]).map(drop),
        CodeUse::Validator => leviath_scripting::output_validator::compile(label, source).map(drop),
        CodeUse::Region => leviath_scripting::region_hook::compile(label, source).map(drop),
        CodeUse::MimeCheck => leviath_scripting::mime_check::compile(label, source).map(drop),
        CodeUse::DependencyCheck => {
            leviath_scripting::dependency_check::compile(label, source).map(drop)
        }
        CodeUse::Install => leviath_scripting::dependency_check::compile_install(label, source),
        CodeUse::Tool => leviath_scripting::tool::check_source(label, source).map(drop),
        CodeUse::Seed => Ok(()),
    };
    checked.map_err(|e| e.to_string())
}

/// Choose a stage's provider and model over `registry`, the way every spawn
/// has chosen one: the stage's own models, then the operator's override and
/// fallback models and failover chain, over the providers they prefer.
///
/// The choice is made by the stage resolver every spawn uses, so a gateway's
/// unread model list, a provider that refuses the model and a model that
/// cannot run with zero data retention are refused here. The model is chosen
/// for the mime types the stage's own `input_accepts` names. An issue sits at
/// the root, which is the stage's model: the resolver puts the stage's path
/// in front of it.
pub fn choose_model(
    stage: &StageDef,
    requested: Option<&ModelRef>,
    defaults: &ModelDefaults,
    registry: &ProviderRegistry,
) -> Result<ModelPlan, Box<SpawnIssue>> {
    let requested_text = requested.map(ToString::to_string);
    let unresolvable = |message: String| {
        SpawnIssue::new(SpecPath::root(), IssueCode::Unresolvable, message)
            .known(registry.resolvable_names())
    };
    let needs: Vec<String> = stage
        .input_accepts
        .iter()
        .map(ToString::to_string)
        .collect();
    let resolved = resolve_stage_route(
        stage.name.as_str(),
        &stage.model,
        &needs,
        requested_text.as_deref(),
        defaults,
        registry,
    )
    .map_err(unresolvable)?;
    let provider = ProviderName::new(&resolved.provider_name)
        .map_err(|e| unresolvable(format!("the chosen provider: {e}")))?;
    let model = ModelId::new(&resolved.model)
        .map_err(|e| unresolvable(format!("the chosen model: {e}")))?;
    let context_window = registry
        .get(provider.as_str())
        .map(|p| u32::try_from(p.max_context_tokens(model.as_str())).unwrap_or(u32::MAX))
        .unwrap_or(FALLBACK_WINDOW);
    let max_output_tokens = registry
        .get(provider.as_str())
        .map(|p| p.capabilities(model.as_str()).max_output_tokens)
        .unwrap_or(leviath_providers::ModelCapabilities::default().max_output_tokens);
    let max_output_tokens = u32::try_from(max_output_tokens).unwrap_or(u32::MAX);
    Ok(ModelPlan {
        model: ChosenModel {
            provider,
            id: model,
            context_window,
            fallbacks: resolved.fallbacks,
        },
        max_output_tokens,
        notes: resolved.notes,
    })
}

/// Whether a compaction model may be sent a run's context over `registry`:
/// refused when its provider would keep what it is sent and the operator asked
/// for zero retention, as a stage's model is. A model whose provider is not
/// registered (or that names none) is never called, so it is not judged.
pub fn compaction_model(
    model: &ModelRef,
    defaults: &ModelDefaults,
    registry: &ProviderRegistry,
) -> Result<(), String> {
    let Some(provider) = model.provider.as_ref().filter(|p| registry.has(p.as_str())) else {
        return Ok(());
    };
    match registry.retention_refusal_with(
        &defaults.retention,
        provider.as_str(),
        model.model.as_str(),
    ) {
        Some(refusal) => Err(refusal),
        None => Ok(()),
    }
}

/// A graph's mime rows as the registry layers them: the TOML table a
/// `[mime_types]` block is written as. A check named inline is keyed by the
/// digest of its code, the way compiled code is filed.
pub fn mime_table(rows: &crate::spec::graph::MimeRows) -> toml::Table {
    use crate::spec::graph::TokenRule;
    use leviath_core::mime::TokenRule as Core;
    rows.iter()
        .map(|(pattern, row)| {
            let row = leviath_core::mime::registry::MimeRow {
                family: row.family.clone(),
                text: row.text,
                tokens: row.tokens.map(|t| match t {
                    TokenRule::PerByte(r) => Core::PerByte(r),
                    TokenRule::PerPixel { divisor, max } => Core::PerPixel {
                        divisor,
                        max: max as usize,
                    },
                    TokenRule::PerSecond(n) => Core::PerSecond(n),
                    TokenRule::PerPage(n) => Core::PerPage(n as usize),
                    TokenRule::Fixed(n) => Core::Fixed(n as usize),
                }),
                extensions: row.extensions.clone(),
                magic: row.magic.clone(),
                stand_in: row.stand_in.clone(),
                check: row.check.as_ref().map(check_key),
            };
            let value = toml::Value::try_from(row).expect("a row of plain values writes as TOML");
            (pattern.to_string(), value)
        })
        .collect()
}

/// The key a mime check's code is filed under in a registry row.
fn check_key(code: &CodeRef) -> String {
    match code {
        CodeRef::File(path) => path.clone(),
        CodeRef::Inline(source) => format!("inline:{}", Digest::of(source.as_bytes())),
    }
}

/// `base` with a graph's mime rows layered on top, as the run's registry.
pub fn run_registry(
    base: &leviath_core::mime::MimeRegistry,
    rows: &crate::spec::graph::MimeRows,
) -> Result<leviath_core::mime::MimeRegistry, String> {
    base.layered(&mime_table(rows), "blueprint")
        .map_err(|e| e.to_string())
}

/// A tool as the model is offered it, with where it comes from.
///
/// `None` for a tool whose name is not one a provider accepts, which a
/// catalog leaves out rather than offering something no model could call.
pub fn tool_def(tool: &Tool, source: ToolSource) -> Option<ToolDef> {
    let name = ToolName::new(&tool.name).ok()?;
    Some(ToolDef {
        name,
        description: tool.description.clone(),
        schema: JsonDoc::new(tool.parameters.clone()),
        source,
    })
}

/// Built-in tool definitions, with the stage-control tools marked as such.
pub fn builtin_defs(tools: &[Tool]) -> Vec<ToolDef> {
    tools
        .iter()
        .filter_map(|t| {
            let source = match leviath_tools::STAGE_CONTROL_TOOLS.contains(&t.name.as_str()) {
                true => ToolSource::StageControl,
                false => ToolSource::Builtin,
            };
            tool_def(t, source)
        })
        .collect()
}

/// An MCP server's advertised tools, as the model is offered them. The tool's
/// own name on the server is the advertised name with the server's prefix
/// taken off.
pub fn mcp_defs(server: &McpServerName, tools: &[Tool]) -> Vec<ToolDef> {
    let prefix = format!("{server}__");
    tools
        .iter()
        .filter_map(|t| {
            let tool = t.name.strip_prefix(&prefix).unwrap_or(&t.name).to_string();
            tool_def(
                t,
                ToolSource::Mcp {
                    server: server.clone(),
                    tool,
                },
            )
        })
        .collect()
}

/// The tools of `catalog` a stage gets: the ones it names (an MCP tool may be
/// named without its server when only one server offers it), every tool of
/// each group it names, and every tool of each MCP server it connects to.
///
/// A named MCP tool (`server__tool`) the catalog lacks is left out, since a
/// blueprint may name tools of servers this machine does not have. Any other
/// name the catalog lacks is an issue: it is a typo, or a tool the blueprint
/// expected to ship and does not. So is a tool the stage requires and cannot
/// have, since without it the stage cannot do its job. Each issue's path is
/// relative to the stage.
pub fn select_tools(catalog: &[ToolDef], stage: &StageDef) -> Result<Vec<ToolDef>, SpawnIssues> {
    let defs: Vec<Tool> = catalog
        .iter()
        .map(|d| Tool {
            name: d.name.to_string(),
            description: d.description.clone(),
            parameters: d.schema.value().clone(),
        })
        .collect();
    let owners: ToolOwners = catalog
        .iter()
        .filter_map(|d| match &d.source {
            ToolSource::Mcp { server, .. } => Some((d.name.to_string(), server.to_string())),
            _ => None,
        })
        .collect();
    let required: Vec<String> = stage
        .required_tools
        .iter()
        .map(ToString::to_string)
        .collect();
    let granted = stage_grants(stage, &owners);
    let picked = filter_tools_for_stage(
        ToolCatalog {
            defs: &defs,
            owners: &owners,
        },
        &granted,
        &required,
        false,
    );
    let chosen: Vec<ToolDef> = catalog
        .iter()
        .filter(|d| picked.iter().any(|t| t.name == d.name.as_str()))
        .cloned()
        .collect();
    let mut issues = SpawnIssues::new();
    for (i, selector) in stage.tools.iter().enumerate() {
        let ToolSelector::Tool(name) = selector else {
            continue;
        };
        if name.as_str().contains("__") || offers(catalog, name.as_str()) {
            continue;
        }
        issues.push(
            SpawnIssue::new(
                SpecPath::root().field("tools").index(i),
                IssueCode::Unknown,
                format!("no tool is named '{name}' here"),
            )
            .hint(
                "check the spelling, or ship the tool in the blueprint's tools/ directory; \
                 an MCP tool is named server__tool",
            )
            .known(catalog.iter().map(|d| d.name.to_string())),
        );
    }
    for (i, name) in stage.required_tools.iter().enumerate() {
        let canonical = leviath_tools::canonical_tool_name(name.as_str());
        if !chosen.iter().any(|d| d.name.as_str() == canonical) {
            issues.push(
                SpawnIssue::new(
                    SpecPath::root().field("required_tools").index(i),
                    IssueCode::Unresolvable,
                    format!("the stage requires '{name}', and it is not available to it here"),
                )
                .hint("grant it in the stage's tools, or install what provides it")
                .known(catalog.iter().map(|d| d.name.to_string())),
            );
        }
    }
    issues.into_result(chosen)
}

/// Whether `catalog` has a tool `name` refers to: by any spelling of the
/// name, or as an MCP tool's own name on its server.
fn offers(catalog: &[ToolDef], name: &str) -> bool {
    catalog.iter().any(|d| {
        leviath_tools::tool_name_spellings(name).any(|n| n == d.name.as_str())
            || matches!(&d.source, ToolSource::Mcp { tool, .. } if tool == name)
    })
}

/// What a stage's tool list grants, as the tool filter reads it: each named
/// tool, each group by its token, and every tool of each MCP server the
/// stage connects to (by `owners`).
pub fn stage_grants(stage: &StageDef, owners: &ToolOwners) -> Vec<String> {
    let available: Vec<String> = stage
        .tools
        .iter()
        .map(|s| match s {
            ToolSelector::Tool(name) => name.to_string(),
            ToolSelector::Group(group) => group.token().to_string(),
        })
        .collect();
    let connectors: Vec<String> = stage.connectors.iter().map(ToString::to_string).collect();
    expand_connector_grants(&available, &connectors, owners)
}

/// The type of attached bytes, by `registry`.
///
/// A declared type is kept when the bytes do not contradict it (their magic
/// says something else) and the registry's check for it passes. A declared
/// pattern (`image/*`) is a constraint rather than a type: the type is found
/// from the bytes and the name, and must match it. With nothing declared, the
/// type is found from the bytes and the name.
pub fn sniff(
    registry: &leviath_core::mime::MimeRegistry,
    name: &str,
    bytes: &[u8],
    declared: Option<&MimePattern>,
) -> Result<String, String> {
    let found = || registry.resolve(None, Some(name), bytes);
    let Some(declared) = declared else {
        return Ok(found().as_str().to_string());
    };
    let exact = (!declared.as_str().contains('*'))
        .then(|| leviath_core::mime::MimeType::parse(declared.as_str()).ok())
        .flatten();
    let Some(t) = exact else {
        let t = found();
        return match t.matches(declared.as_str()) {
            true => Ok(t.as_str().to_string()),
            false => Err(format!(
                "'{name}' is {}, which is not {declared}",
                t.as_str()
            )),
        };
    };
    match registry.sniff(bytes) {
        Some(magic) if magic != t => {
            return Err(format!(
                "'{name}' was declared {declared}, and its bytes are {}",
                magic.as_str()
            ));
        }
        // A type the registry knows the opening bytes of must open with them.
        None if registry.row(t.as_str()).is_some_and(|r| r.magic.is_some()) => {
            return Err(format!(
                "'{name}' was declared {declared}, and its bytes do not begin the way \
                 {declared} files do"
            ));
        }
        _ => {}
    }
    registry
        .verify(&t, bytes)
        .map_err(|e| format!("'{name}' is not a valid {declared}: {e}"))?;
    Ok(t.as_str().to_string())
}

/// Whether `path` names something of `kind` inside `workdir`, following
/// links only as far as they stay inside it.
pub fn path_exists(workdir: &Path, path: &WorkdirPath, kind: PathKind) -> bool {
    let full = workdir.join(path.as_str());
    leviath_core::resolves_within(&full, workdir)
        && std::fs::metadata(&full).is_ok_and(|m| match kind {
            PathKind::File => m.is_file(),
            PathKind::Dir => m.is_dir(),
            PathKind::Any => true,
        })
}

/// Run a dependency check written inline: `Ok` when it is satisfied, the
/// check's own remedy or failure otherwise.
pub fn run_inline_check(source: &str) -> Result<(), String> {
    use leviath_scripting::dependency_check::{Verdict, compile, run};
    let check = compile("check", source).map_err(|e| e.to_string())?;
    match run(&check) {
        Verdict::Satisfied => Ok(()),
        Verdict::Unmet(why) | Verdict::Unusable(why) => Err(why),
    }
}

/// Run a dependency check from the run's own copy of its code, however the
/// graph named it: `Ok` when it is satisfied, the check's own remedy or
/// failure otherwise.
pub fn run_check(code: Option<&[u8]>) -> Result<(), String> {
    let code = code.ok_or("the run holds no code for this check")?;
    let source =
        std::str::from_utf8(code).map_err(|e| format!("the check is not UTF-8 text: {e}"))?;
    run_inline_check(source)
}

/// A digest of a provider's configuration, credentials left out.
///
/// It covers what decides where requests go and what they may ask for: the
/// provider's name, its base URL and its options (its kind, its model list,
/// its region, which headers it sends). Header values are left out with the
/// key, because a header is where a gateway's token travels.
pub fn provider_fingerprint(creds: &ProviderCreds) -> Digest {
    let mut lines = vec![
        format!("name={}", creds.name),
        format!("base_url={}", creds.base_url.as_deref().unwrap_or("")),
    ];
    let mut options: Vec<String> = creds
        .options
        .iter()
        .map(|(k, v)| match k.starts_with("header:") {
            true => format!("option:{k}"),
            false => format!("option:{k}={v}"),
        })
        .collect();
    options.sort();
    lines.extend(options);
    Digest::of(lines.join("\n").as_bytes())
}

/// A digest of a provider registered without configuration (an embedder's
/// own implementation, or a script provider): it stands only for the name.
pub fn registered_fingerprint(provider: &str) -> Digest {
    Digest::of(format!("registered={provider}").as_bytes())
}

/// A digest of a tool list: every tool's name, description and schema, in
/// name order, so the same tools in another order digest the same.
pub fn tools_fingerprint(tools: &[ToolDef]) -> Digest {
    let mut lines: Vec<String> = tools
        .iter()
        .map(|t| format!("{}\n{}\n{}", t.name, t.description, t.schema.to_text()))
        .collect();
    lines.sort();
    Digest::of(lines.join("\n\n").as_bytes())
}

#[cfg(test)]
#[path = "host_tests.rs"]
pub(crate) mod tests;

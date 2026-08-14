//! Read and write the Rhai scripts an agent runs.
//!
//! Four extension points share one API because they share one editor: a script
//! tool, a region hook, a stage hook and an output validator are all a `.rhai`
//! file in the agent's directory, and only the `kind` says which compiler has to
//! accept it.
//!
//! # Where each kind actually lives
//!
//! Only **script tools** have a directory convention: `<agent>/tools/*.rhai`,
//! scanned at spawn, plus the global `~/.leviath/tools/` every agent gets. The
//! other three are named by path in the manifest and resolved relative to the
//! agent's own directory, with `resolve_*_scripts` in the daemon refusing any
//! that lands outside it. There is no `hooks/` or `validators/` directory to
//! list, so the listing derives those three from what the manifest declares,
//! and the read/write routes address them at `<agent>/<name>.rhai` - the path a
//! bare declared filename already resolves to. There is likewise no global
//! directory for them: a hook only ever loads from beside the agent that
//! declares it.
//!
//! # Why the write half is gated
//!
//! A blueprint is declarative. A `.rhai` file is executable code every agent
//! then runs, so `PUT` and `DELETE` are the same category of act as adding an
//! MCP server, and they are mounted the same way: absent entirely, 404, unless
//! `lev serve --allow-admin`. The `GET` routes stay open so an editor degrades
//! to read-only instead of disappearing.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};

use axum::extract::{Path as AxumPath, Query};
use axum::http::StatusCode;
use axum::response::Json;
use serde::{Deserialize, Serialize};

use super::tools::{ApiError, agent_dir};
use super::types::err;

/// Which extension point a script plugs into, and so which compiler decides
/// whether it is valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ScriptKind {
    /// A tool the model may call, discovered from a `tools/` directory.
    Tool,
    /// A custom region's `render`/`on_write`/`on_overflow`.
    RegionHook,
    /// A stage lifecycle hook.
    StageHook,
    /// A validator that decides whether an agent's output may be handed back.
    OutputValidator,
}

impl ScriptKind {
    /// The wire spelling, which is also the `{kind}` path segment.
    fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::RegionHook => "region_hook",
            Self::StageHook => "stage_hook",
            Self::OutputValidator => "output_validator",
        }
    }

    /// Parse a `{kind}` segment. `None` for anything else, which the routes turn
    /// into a 400 rather than guessing a kind and compiling with the wrong rules.
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "tool" => Some(Self::Tool),
            "region_hook" => Some(Self::RegionHook),
            "stage_hook" => Some(Self::StageHook),
            "output_validator" => Some(Self::OutputValidator),
            _ => None,
        }
    }
}

/// The global drop-in directory, `~/.leviath/tools`.
///
/// Empty when no home resolves. That is deliberately not a special case: an
/// empty base cannot be shown to contain anything, so [`guard`] refuses it, and
/// a write lands nowhere rather than in the process's working directory.
fn global_tools_dir() -> PathBuf {
    leviath_core::tools_dir().unwrap_or_default()
}

/// One resolved script file: which directory owns it, what it is called there,
/// and whose it is.
#[derive(Debug, Clone)]
struct Target {
    kind: ScriptKind,
    /// The directory the file must live in, and nothing below it.
    dir: PathBuf,
    /// `<dir>/<name>.rhai`.
    path: PathBuf,
    /// `agent` or `global`.
    scope: &'static str,
    /// The agent that owns it, absent for the global directory.
    agent: Option<String>,
}

/// Resolve `{kind}/{name}` plus an optional `?agent=` into the one file the
/// request may touch.
///
/// Both `name` and `agent` arrive in a URL and are joined onto a directory, so
/// both go through [`leviath_core::is_safe_path_component`] first, the way
/// `blueprint_dir` does in `blueprints.rs`: `Path::join` neither normalizes
/// `..` nor resists an absolute path, and the bug that check exists for was a
/// REST route writing attacker-chosen content outside the directory it meant to
/// and then recursively deleting whatever it had landed on.
///
/// The `.rhai` extension is fixed here rather than taken from the caller, so no
/// request can ask for a `.toml`, a manifest, or anything else in the agent's
/// directory. A `name` that already carries the extension is accepted and means
/// the same file, since that is how the listing spells a path back.
fn resolve(kind: &str, name: &str, agent: Option<&str>) -> Result<Target, ApiError> {
    let Some(kind) = ScriptKind::parse(kind) else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!(
                "Unknown script kind '{kind}': expected tool, region_hook, stage_hook \
                 or output_validator"
            ),
        ));
    };
    let stem = name.strip_suffix(".rhai").unwrap_or(name);
    if !leviath_core::is_safe_path_component(stem) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!(
                "Invalid script name '{name}': names may contain only letters, digits, \
                 '.', '_' and '-'"
            ),
        ));
    }

    let (dir, scope, owner) = match agent {
        Some(agent) => {
            let base = agent_dir(agent)?;
            // Tools are scanned out of `tools/`; a hook or validator is named by
            // a manifest path that resolves against the agent's own directory.
            let dir = match kind {
                ScriptKind::Tool => base.join("tools"),
                _ => base,
            };
            (dir, "agent", Some(agent.to_string()))
        }
        None => match kind {
            ScriptKind::Tool => (global_tools_dir(), "global", None),
            _ => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "A {} is only ever loaded from beside the agent that declares it, \
                         so this route needs ?agent=<name>",
                        kind.as_str()
                    ),
                ));
            }
        },
    };

    let path = dir.join(format!("{stem}.rhai"));
    Ok(Target {
        kind,
        dir,
        path,
        scope,
        agent: owner,
    })
}

/// Whether the target has to exist already.
#[derive(Debug, Clone, Copy)]
enum Presence {
    /// A read or a delete: no file, no request.
    Required,
    /// A write: the file may be new.
    Optional,
}

/// The containment gate every read and write passes through.
///
/// The name is already a single safe component joined onto a fixed directory,
/// so nothing the caller wrote can carry the path elsewhere. What is left is
/// what the *filesystem* can do: a symlink planted at that exact name would
/// send a read or a write straight through it, outside the directory this route
/// is fenced to. `symlink_metadata` does not follow, so anything that is not a
/// plain file is refused before it is opened, and the resolved path is then
/// checked against the directory it came from.
///
/// The existence check runs first on purpose. Containment canonicalizes, and a
/// directory that does not exist yet cannot be canonicalized, so asking that
/// question about a missing file would answer "forbidden" on the platforms
/// where the temporary directory is itself a symlink and "not found" everywhere
/// else.
fn guard(target: &Target, presence: Presence) -> Result<(), ApiError> {
    match (std::fs::symlink_metadata(&target.path), presence) {
        (Ok(meta), _) if !meta.is_file() => {
            return Err(err(
                StatusCode::FORBIDDEN,
                format!(
                    "'{}' is not a plain file, so it will not be read or written through",
                    target.path.display()
                ),
            ));
        }
        (Err(_), Presence::Required) => {
            return Err(err(
                StatusCode::NOT_FOUND,
                format!("No such script: {}", target.path.display()),
            ));
        }
        _ => {}
    }
    match leviath_core::resolves_within(&target.path, &target.dir) {
        true => Ok(()),
        false => Err(err(
            StatusCode::FORBIDDEN,
            format!(
                "'{}' does not resolve inside {}",
                target.path.display(),
                target.dir.display()
            ),
        )),
    }
}

/// Compile `content` as `kind` and say only whether it worked.
///
/// Each arm is the same compiler the daemon uses at spawn, so a script this
/// accepts is one a run will accept. `hooks` is what a stage-hook file was named
/// for; an empty slice asks only that it compiles, which is all that can be
/// asked of a file no manifest points at yet.
fn compile_status(
    kind: ScriptKind,
    label: &str,
    content: &str,
    hooks: &[&str],
) -> Result<(), String> {
    let outcome = match kind {
        ScriptKind::Tool => leviath_scripting::tool::check_source(label, content).map(drop),
        ScriptKind::RegionHook => leviath_scripting::region_hook::compile(label, content).map(drop),
        ScriptKind::StageHook => {
            leviath_scripting::stage_hook::compile(label, content, hooks).map(drop)
        }
        ScriptKind::OutputValidator => {
            leviath_scripting::output_validator::compile(label, content).map(drop)
        }
    };
    match outcome {
        Ok(()) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Flatten a compile outcome into the pair the wire types carry.
fn status_pair(status: Result<(), String>) -> (bool, Option<String>) {
    match status {
        Ok(()) => (true, None),
        Err(reason) => (false, Some(reason)),
    }
}

// ─── Wire types ─────────────────────────────────────────────────────────────

/// The `?agent=` every script route takes.
#[derive(Debug, Deserialize)]
pub(super) struct ScriptQuery {
    /// Scope to this agent's own directory. Absent means the global tools
    /// directory, which only holds script tools.
    agent: Option<String>,
}

/// One script in the listing.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ScriptItem {
    /// `tool`, `region_hook`, `stage_hook` or `output_validator`.
    pub(super) kind: String,
    /// The `{name}` the read and write routes address it by.
    pub(super) name: String,
    /// `agent` when it belongs to one agent, `global` when every agent gets it.
    pub(super) source: String,
    /// Which agent, for an agent-scoped script.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) agent: Option<String>,
    /// The file on disk.
    pub(super) path: String,
    /// Whether it compiles right now.
    pub(super) compiles: bool,
    /// Why not, when it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
}

/// The body of `GET /api/scripts`.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ScriptsResp {
    /// Every script this scope can see, agent-owned first.
    pub(super) scripts: Vec<ScriptItem>,
}

/// The body of `GET /api/scripts/{kind}/{name}`.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ScriptSource {
    /// Which extension point this file plugs into.
    pub(super) kind: String,
    /// The `{name}` it is addressed by.
    pub(super) name: String,
    /// `agent` or `global`.
    pub(super) source: String,
    /// Which agent, for an agent-scoped script.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) agent: Option<String>,
    /// The file on disk.
    pub(super) path: String,
    /// The script text.
    pub(super) content: String,
    /// Whether it compiles right now.
    pub(super) compiles: bool,
    /// Why not, when it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
}

/// The body of `PUT /api/scripts/{kind}/{name}`.
#[derive(Debug, Deserialize)]
pub(super) struct WriteScriptReq {
    /// The script text to write.
    content: String,
}

/// What a write reports back.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ScriptWritten {
    /// Where it landed.
    pub(super) path: String,
    /// Whether what was saved compiles. A file that does not is still written:
    /// an editor is allowed to save work in progress, and a tool that does not
    /// compile is skipped at spawn rather than breaking the agent.
    pub(super) compiles: bool,
    /// Why not, when it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
}

/// The body of `POST /api/scripts/validate`.
#[derive(Debug, Deserialize)]
pub(super) struct ValidateScriptReq {
    /// Which extension point to compile it as.
    kind: String,
    /// The script text.
    content: String,
    /// For a stage hook, the hook names the blueprint would name this file for.
    /// A file that does not define one it is named for is a spawn error, so an
    /// editor can ask about that here rather than at the start of a run.
    #[serde(default)]
    hooks: Vec<String>,
}

/// What `POST /api/scripts/validate` reports.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ValidateScriptResp {
    /// Whether the compiler for this kind accepts the text.
    pub(super) valid: bool,
    /// The compiler's complaint, when it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
}

// ─── Listing ────────────────────────────────────────────────────────────────

/// The `{name}` these routes would address a declared script by, or `None` when
/// the manifest declared something they cannot address.
///
/// A manifest may name any path inside the agent's directory, a subdirectory
/// included. These routes address `<agent>/<name>.rhai`: one component, one
/// fixed extension. Anything else is left out of the listing rather than listed
/// under a name that would fetch a different file or none at all.
fn addressable_name(declared: &str) -> Option<String> {
    let mut components = Path::new(declared).components();
    let (Some(Component::Normal(only)), None) = (components.next(), components.next()) else {
        return None;
    };
    let file = only.to_string_lossy();
    let stem = file.strip_suffix(".rhai")?;
    match leviath_core::is_safe_path_component(stem) {
        true => Some(stem.to_string()),
        false => None,
    }
}

/// Every hook and validator the manifest declares, keyed by kind and declared
/// path, carrying the stage-hook names each file was named for.
///
/// This is the only way to know a `.rhai` beside a manifest is a region hook
/// rather than a stage hook: nothing about the file says so, the declaration
/// does. A file written but not yet declared is therefore not listed, which is
/// the honest answer - nothing would load it either.
fn declared_scripts(bp: &leviath_core::Blueprint) -> BTreeMap<(ScriptKind, String), Vec<String>> {
    let mut declared: BTreeMap<(ScriptKind, String), Vec<String>> = BTreeMap::new();

    let layouts = std::iter::once(&bp.context_layout).chain(
        bp.stages
            .iter()
            .filter_map(|stage| stage.context_layout.as_ref()),
    );
    for layout in layouts {
        for region in &layout.regions {
            if let leviath_core::RegionKind::Custom { script, .. } = &region.kind {
                declared
                    .entry((ScriptKind::RegionHook, script.clone()))
                    .or_default();
            }
        }
    }

    for stage in &bp.stages {
        for (hook, path) in stage.hooks.declared() {
            declared
                .entry((ScriptKind::StageHook, path.to_string()))
                .or_default()
                .push(hook.to_string());
        }
    }

    let outputs = bp
        .output
        .iter()
        .chain(bp.stages.iter().filter_map(|stage| stage.output.as_ref()));
    for spec in outputs {
        if let Some(validator) = spec.validator.as_deref() {
            declared
                .entry((ScriptKind::OutputValidator, validator.to_string()))
                .or_default();
        }
    }

    declared
}

/// List the `.rhai` files in one tools directory, with the compile verdict
/// `discover` already reached for each.
///
/// `discover` is what the daemon runs at spawn, so a file it skips is a file no
/// agent will get. Running it once over the directory and matching by path is
/// what makes the reported status the real one, `tool.toml` overrides included.
fn collect_tools(dir: &Path, scope: &'static str, agent: Option<&str>, out: &mut Vec<ScriptItem>) {
    let (_, failed) = leviath_scripting::ScriptToolSet::discover(&[dir.to_path_buf()]);
    let reasons: HashMap<PathBuf, String> =
        failed.into_iter().map(|f| (f.path, f.reason)).collect();

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "rhai"))
        .collect();
    paths.sort();

    for path in paths {
        let (compiles, error) = match reasons.get(&path) {
            Some(reason) => (false, Some(reason.clone())),
            None => (true, None),
        };
        out.push(ScriptItem {
            kind: ScriptKind::Tool.as_str().to_string(),
            name: path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into(),
            source: scope.to_string(),
            agent: agent.map(str::to_string),
            path: path.display().to_string(),
            compiles,
            error,
        });
    }
}

/// List the hooks and validators the agent's manifest declares.
///
/// An agent with no manifest, or one that will not parse, contributes nothing:
/// the blueprint routes are where a broken manifest is reported, and a script
/// listing that failed outright would take the tools down with it.
fn collect_declared(dir: &Path, agent: &str, out: &mut Vec<ScriptItem>) {
    let Ok(text) = std::fs::read_to_string(dir.join("agent.leviath")) else {
        return;
    };
    let Ok(bp) = leviath_core::manifest::parse_manifest(&text) else {
        return;
    };

    for ((kind, declared), hooks) in declared_scripts(&bp) {
        let Some(name) = addressable_name(&declared) else {
            continue;
        };
        // `name` is a single safe component, so this join cannot leave `dir`.
        let path = dir.join(format!("{name}.rhai"));
        let hook_refs: Vec<&str> = hooks.iter().map(String::as_str).collect();
        let (compiles, error) = match std::fs::read_to_string(&path) {
            Ok(content) => status_pair(compile_status(kind, &declared, &content, &hook_refs)),
            Err(e) => (
                false,
                Some(format!("cannot read '{}': {e}", path.display())),
            ),
        };
        out.push(ScriptItem {
            kind: kind.as_str().to_string(),
            name,
            source: "agent".to_string(),
            agent: Some(agent.to_string()),
            path: path.display().to_string(),
            compiles,
            error,
        });
    }
}

// ─── Handlers ───────────────────────────────────────────────────────────────

/// `GET /api/scripts[?agent=<name>]`: every script this scope can see.
///
/// With an agent, that is the agent's own `tools/`, the hooks and validators its
/// manifest declares, and the global tools directory. Without one, the global
/// directory alone. `source` is what separates "this agent has it" from
/// "everything on this machine has it".
pub(super) async fn list_scripts(
    Query(q): Query<ScriptQuery>,
) -> Result<Json<ScriptsResp>, ApiError> {
    let mut scripts = Vec::new();
    if let Some(name) = q.agent.as_deref() {
        let dir = agent_dir(name)?;
        collect_tools(&dir.join("tools"), "agent", Some(name), &mut scripts);
        collect_declared(&dir, name, &mut scripts);
    }
    collect_tools(&global_tools_dir(), "global", None, &mut scripts);
    Ok(Json(ScriptsResp { scripts }))
}

/// `GET /api/scripts/{kind}/{name}[?agent=<name>]`: the source text.
pub(super) async fn get_script(
    AxumPath((kind, name)): AxumPath<(String, String)>,
    Query(q): Query<ScriptQuery>,
) -> Result<Json<ScriptSource>, ApiError> {
    let target = resolve(&kind, &name, q.agent.as_deref())?;
    guard(&target, Presence::Required)?;
    read_script(&target)
}

/// Read a script that [`guard`] has already accepted.
fn read_script(target: &Target) -> Result<Json<ScriptSource>, ApiError> {
    let content = match std::fs::read_to_string(&target.path) {
        Ok(content) => content,
        Err(e) => {
            return Err(err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot read '{}': {e}", target.path.display()),
            ));
        }
    };
    let label = target.path.display().to_string();
    let (compiles, error) = status_pair(compile_status(target.kind, &label, &content, &[]));
    Ok(Json(ScriptSource {
        kind: target.kind.as_str().to_string(),
        name: stem_of(&target.path),
        source: target.scope.to_string(),
        agent: target.agent.clone(),
        path: label,
        content,
        compiles,
        error,
    }))
}

/// The `{name}` a resolved path is addressed by. The path was built by joining
/// that name onto a directory, so the stem is always there to take back.
fn stem_of(path: &Path) -> String {
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into()
}

/// `PUT /api/scripts/{kind}/{name}[?agent=<name>]` (admin only): write it.
///
/// Mounted only under `--allow-admin`, because what this writes is code every
/// agent on the machine may then execute.
pub(super) async fn put_script(
    AxumPath((kind, name)): AxumPath<(String, String)>,
    Query(q): Query<ScriptQuery>,
    Json(body): Json<WriteScriptReq>,
) -> Result<Json<ScriptWritten>, ApiError> {
    let target = resolve(&kind, &name, q.agent.as_deref())?;
    if let Err(e) = std::fs::create_dir_all(&target.dir) {
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot create '{}': {e}", target.dir.display()),
        ));
    }
    // After the directory exists, so containment is asked of a path that can be
    // canonicalized rather than of one that merely might be.
    guard(&target, Presence::Optional)?;
    write_script(&target, &body.content)
}

/// Write a script that [`guard`] has already accepted.
fn write_script(target: &Target, content: &str) -> Result<Json<ScriptWritten>, ApiError> {
    if let Err(e) = std::fs::write(&target.path, content) {
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot write '{}': {e}", target.path.display()),
        ));
    }
    let label = target.path.display().to_string();
    let (compiles, error) = status_pair(compile_status(target.kind, &label, content, &[]));
    Ok(Json(ScriptWritten {
        path: label,
        compiles,
        error,
    }))
}

/// `DELETE /api/scripts/{kind}/{name}[?agent=<name>]` (admin only): remove it.
pub(super) async fn delete_script(
    AxumPath((kind, name)): AxumPath<(String, String)>,
    Query(q): Query<ScriptQuery>,
) -> Result<StatusCode, ApiError> {
    let target = resolve(&kind, &name, q.agent.as_deref())?;
    guard(&target, Presence::Required)?;
    remove_script(&target)
}

/// Remove a script that [`guard`] has already accepted.
fn remove_script(target: &Target) -> Result<StatusCode, ApiError> {
    match std::fs::remove_file(&target.path) {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot delete '{}': {e}", target.path.display()),
        )),
    }
}

/// `POST /api/scripts/validate`: compile without writing.
///
/// The only other way to find out whether a script compiles was to save it and
/// wait for an agent to fail, which is not much of an improvement on editing the
/// file over SSH. Ungated: compiling text in memory writes nothing and runs
/// nothing, since every compiler here stops at the AST.
pub(super) async fn validate_script(
    Json(body): Json<ValidateScriptReq>,
) -> Result<Json<ValidateScriptResp>, ApiError> {
    let Some(kind) = ScriptKind::parse(&body.kind) else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!(
                "Unknown script kind '{}': expected tool, region_hook, stage_hook \
                 or output_validator",
                body.kind
            ),
        ));
    };
    let hooks: Vec<&str> = body.hooks.iter().map(String::as_str).collect();
    let (valid, error) = status_pair(compile_status(kind, "script", &body.content, &hooks));
    Ok(Json(ValidateScriptResp { valid, error }))
}

#[cfg(test)]
#[path = "scripts_tests.rs"]
mod tests;

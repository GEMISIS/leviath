//! Read and write the Rhai scripts a machine runs.
//!
//! Five extension points share one API because they share one editor: a script
//! tool, a region hook, a stage hook, an output validator and a model provider
//! are all a `.rhai` file somewhere under the home directory, and only the
//! `kind` says which compiler has to accept it.
//!
//! # Where each kind actually lives
//!
//! Only **script tools** have a directory convention an agent owns:
//! `<agent>/tools/*.rhai`, scanned at spawn, plus the global `~/.leviath/tools/`
//! every agent gets. The hooks and the validator are named by path in the
//! manifest and resolved relative to the agent's own directory, with
//! `resolve_*_scripts` in the daemon refusing any that lands outside it. There
//! is no `hooks/` or `validators/` directory to list, so the listing derives
//! those three from what the manifest declares, and the read/write routes
//! address them at `<agent>/<name>.rhai` - subdirectories included, since a
//! manifest may declare `validators/a2ui.rhai` and a route that could not open
//! it would be a listing entry nobody could act on. There is likewise no global
//! directory for them: a hook only ever loads from beside the agent that
//! declares it.
//!
//! What the manifest declares is not what a person is choosing between, though.
//! An editor offering a validator picker needs the files that *could* be named,
//! and a file is only declared once somebody has named it. `?include=candidates`
//! is that half: the `.rhai` files under the agent's directory that nothing
//! declares, reported as a `kind` of `unknown` and `declared: false`, under a
//! bounded walk that never leaves the agent's own directory.
//!
//! A **provider** is the inverse: `~/.leviath/providers/<name>.rhai` and nothing
//! else. No agent owns one, and a stage reaches it by name rather than by path,
//! so this route takes no `?agent=` and refuses one rather than inventing a
//! per-agent layout that nothing would load.
//!
//! # Why the write half is gated
//!
//! A blueprint is declarative. A `.rhai` file is executable code every agent
//! then runs, so `PUT` and `DELETE` are the same category of act as adding an
//! MCP server, and they are mounted the same way: absent entirely, 404, unless
//! `lev serve --allow-admin`. The `GET` routes stay open so an editor degrades
//! to read-only instead of disappearing.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::{Deserialize, Serialize};

use super::tools::agent_dir;
use super::types::{ApiError, AppState, err};

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
    /// A drop-in model provider, global to the machine.
    Provider,
}

impl ScriptKind {
    /// The wire spelling, which is also the `{kind}` path segment.
    fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::RegionHook => "region_hook",
            Self::StageHook => "stage_hook",
            Self::OutputValidator => "output_validator",
            Self::Provider => "provider",
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
            "provider" => Some(Self::Provider),
            _ => None,
        }
    }
}

/// The kinds, spelled the way the 400s list them.
const KIND_LIST: &str = "tool, region_hook, stage_hook, output_validator or provider";

/// The global drop-in directory, `~/.leviath/tools`.
///
/// Empty when no home resolves. That is deliberately not a special case: an
/// empty base cannot be shown to contain anything, so [`guard`] refuses it, and
/// a write lands nowhere rather than in the process's working directory.
fn global_tools_dir() -> PathBuf {
    leviath_core::tools_dir().unwrap_or_default()
}

/// The drop-in provider directory, `~/.leviath/providers`.
///
/// Empty when no home resolves, for the same reason [`global_tools_dir`] is.
fn global_providers_dir() -> PathBuf {
    leviath_core::providers_dir().unwrap_or_default()
}

/// One resolved script file: which directory owns it, what it is called there,
/// and whose it is.
#[derive(Debug, Clone)]
struct Target {
    kind: ScriptKind,
    /// The directory the file must resolve inside. A `{name}` may name a
    /// subdirectory of it, and nothing above or outside it.
    dir: PathBuf,
    /// The directory the file itself sits in: `dir`, or a subdirectory of it
    /// when the name carries one. Kept apart from `dir` because `dir` is the
    /// fence and this is only where a write has to create directories.
    file_dir: PathBuf,
    /// `<file_dir>/<file>.rhai`.
    path: PathBuf,
    /// The `{name}` this was addressed by, normalized: `/`-separated, no
    /// `.rhai`. Spelled back so a response never renames what it was asked for.
    name: String,
    /// `agent` or `global`.
    scope: &'static str,
    /// The agent that owns it, absent for the global directory.
    agent: Option<String>,
}

/// A `.rhai` file addressed relative to some directory.
///
/// One shape for the two places a relative script path is read: a `{name}` off
/// the URL, and a path a manifest declares. Both have to end up as the same
/// three answers - what to call it, where it sits, and what to write into a
/// manifest - and having them computed twice is how the two disagreed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Addressed {
    /// The `{name}` the routes address it by: `/`-separated, `.rhai` stripped.
    name: String,
    /// The same file relative to the base directory, extension included, always
    /// with `/` separators because that is what goes into a manifest.
    relative: String,
    /// The directory components between the base and the file, in order.
    dirs: Vec<String>,
    /// The file name, `.rhai` included.
    file: String,
}

impl Addressed {
    /// Where the file sits under `base`.
    fn dir_in(&self, base: &Path) -> PathBuf {
        self.dirs
            .iter()
            .fold(base.to_path_buf(), |acc, d| acc.join(d))
    }

    /// The file itself under `base`.
    fn path_in(&self, base: &Path) -> PathBuf {
        self.dir_in(base).join(&self.file)
    }
}

/// Read a `{name}` as a path relative to a script directory, or `None` when it
/// is not one these routes can address.
///
/// A name may be a single component (`check`) or a `/`-separated relative path
/// (`validators/a2ui`), because that is the shape a manifest declares a hook or
/// a validator in and the shape the listing has to report back. Every component
/// goes through [`leviath_core::is_safe_path_component`], which is what keeps
/// `..`, an absolute path, a Windows `\` separator and an empty segment out:
/// `Path::join` normalizes none of them. Containment is still checked
/// afterwards by [`guard`], because a component that is safe to spell can still
/// be a symlink pointing elsewhere.
///
/// The `.rhai` extension is fixed here rather than taken from the caller, so no
/// request can ask for a `.toml`, a manifest, or anything else in the agent's
/// directory. A name that already carries the extension means the same file,
/// since that is how a manifest and this listing both spell one.
fn addressed_path(name: &str) -> Option<Addressed> {
    let stem = name.strip_suffix(".rhai").unwrap_or(name);
    let mut dirs: Vec<String> = Vec::new();
    let mut file = String::new();
    for part in stem.split('/') {
        if !leviath_core::is_safe_path_component(part) {
            return None;
        }
        // Which component is the file is only known once the walk ends, so
        // whatever was holding that place becomes a directory as soon as
        // another component arrives. An empty `file` is the first turn.
        if !file.is_empty() {
            dirs.push(std::mem::take(&mut file));
        }
        file = part.to_string();
    }
    Some(Addressed {
        name: stem.to_string(),
        relative: format!("{stem}.rhai"),
        file: format!("{file}.rhai"),
        dirs,
    })
}

/// Resolve `{kind}/{name}` plus an optional `?agent=` into the one file the
/// request may touch.
///
/// Both `name` and `agent` arrive in a URL and are joined onto a directory, so
/// every component of both goes through
/// [`leviath_core::is_safe_path_component`] first, the way `blueprint_dir` does
/// in `blueprints.rs`: `Path::join` neither normalizes `..` nor resists an
/// absolute path, and the bug that check exists for was a REST route writing
/// attacker-chosen content outside the directory it meant to and then
/// recursively deleting whatever it had landed on.
///
/// `name` may address a subdirectory (`validators/a2ui`, percent-encoded in the
/// URL as `validators%2Fa2ui`), because a manifest names a hook or a validator
/// by a path relative to the agent's directory and a route that could only
/// address one component could not open the file the listing had just reported.
/// [`guard`] still checks where the result lands.
fn resolve(
    config: &crate::config::Config,
    kind: &str,
    name: &str,
    agent: Option<&str>,
) -> Result<Target, ApiError> {
    let Some(kind) = ScriptKind::parse(kind) else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("Unknown script kind '{kind}': expected {KIND_LIST}"),
        ));
    };
    let Some(addressed) = addressed_path(name) else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!(
                "Invalid script name '{name}': each '/'-separated part may contain only \
                 letters, digits, '.', '_' and '-'"
            ),
        ));
    };

    let (dir, scope, owner) = match (agent, kind) {
        // A provider is machine-wide. Scoping one to an agent would write a file
        // nothing loads, so the agent is refused rather than ignored.
        (Some(_), ScriptKind::Provider) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "A provider is global to the machine and lives in ~/.leviath/providers, \
                 so this route takes no ?agent="
                    .to_string(),
            ));
        }
        (Some(agent), _) => {
            let base = agent_dir(config, agent)?;
            // Tools are scanned out of `tools/`; a hook or validator is named by
            // a manifest path that resolves against the agent's own directory.
            let dir = match kind {
                ScriptKind::Tool => base.join("tools"),
                _ => base,
            };
            (dir, "agent", Some(agent.to_string()))
        }
        (None, ScriptKind::Tool) => (global_tools_dir(), "global", None),
        (None, ScriptKind::Provider) => (global_providers_dir(), "global", None),
        (None, _) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!(
                    "A {} is only ever loaded from beside the agent that declares it, \
                     so this route needs ?agent=<name>",
                    kind.as_str()
                ),
            ));
        }
    };

    let file_dir = addressed.dir_in(&dir);
    let path = addressed.path_in(&dir);
    Ok(Target {
        kind,
        dir,
        file_dir,
        path,
        name: addressed.name,
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
/// The name is already safe components joined onto a fixed directory, so
/// nothing the caller wrote can carry the path elsewhere lexically. What is left
/// is what the *filesystem* can do: a symlink planted at that exact name, or at
/// a directory along the way, would send a read or a write straight through it,
/// outside the directory this route is fenced to. `symlink_metadata` does not
/// follow, so anything that is not a plain file is refused before it is opened,
/// and the resolved path is then checked against the directory it came from -
/// `resolves_within` canonicalizes, so a link at any component is caught, not
/// only one at the last.
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
/// Each arm is the same compiler the daemon uses when it loads the script, so a
/// script this accepts is one a run will accept. `hooks` is what a stage-hook
/// file was named for; an empty slice asks only that it compiles, which is all
/// that can be asked of a file no manifest points at yet.
///
/// Every arm stops at the AST. That is what lets `POST /api/scripts/validate`
/// stay ungated: a provider's `initialize` is script code, and `check_source`
/// reads it off the compiled AST rather than running it.
fn compile_status(
    kind: ScriptKind,
    label: &str,
    content: &str,
    hooks: &[&str],
) -> Result<(), String> {
    match kind {
        ScriptKind::Tool => leviath_scripting::tool::check_source(label, content)
            .map(drop)
            .map_err(|e| e.to_string()),
        ScriptKind::RegionHook => leviath_scripting::region_hook::compile(label, content)
            .map(drop)
            .map_err(|e| e.to_string()),
        ScriptKind::StageHook => leviath_scripting::stage_hook::compile(label, content, hooks)
            .map(drop)
            .map_err(|e| e.to_string()),
        ScriptKind::OutputValidator => leviath_scripting::output_validator::compile(label, content)
            .map(drop)
            .map_err(|e| e.to_string()),
        ScriptKind::Provider => leviath_providers::rhai_provider::check_source(label, content)
            .map(drop)
            .map_err(|e| e.to_string()),
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

/// What the listing takes: an `?agent=`, and what else to put in the answer.
///
/// Its own type rather than an `include` bolted onto [`ScriptQuery`], so the
/// read and write routes do not appear to take a parameter they would ignore.
#[derive(Debug, Deserialize)]
pub(super) struct ListScriptsQuery {
    /// Scope to this agent's own directory.
    agent: Option<String>,
    /// Comma-separated extras. `candidates` adds the `.rhai` files beside the
    /// agent that nothing declares. Only meaningful with `agent`: the global
    /// directories are already listed file by file from disk, so there is
    /// nothing undeclared left there to add.
    include: Option<String>,
}

/// What a provider script's leading `// @` comments declare.
///
/// Carried as one nested object rather than as six more optional fields on
/// every script: only a provider has any of it, and a shape whose fields are
/// absent for four kinds out of five is a shape nobody can read. A console
/// needs it to show which model a provider defaults to and how big a context it
/// claims, without fetching and re-parsing the source.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ProviderScriptMeta {
    /// `// @provider`: the name the script claims, which is informational.
    /// Selection goes by the filename stem, or by the `[model_providers]` key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) provider: Option<String>,
    /// `// @description`, empty when the script declares none.
    pub(super) description: String,
    /// `// @default_model`, used to fill an empty stage model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) default_model: Option<String>,
    /// `// @max_context_tokens`, defaulted when unset.
    pub(super) max_context_tokens: usize,
    /// `// @max_output_tokens`, defaulted when unset.
    pub(super) max_output_tokens: usize,
    /// `// @supports_streaming`. Advisory: real streaming follows whether the
    /// script defines `stream`.
    pub(super) supports_streaming: bool,
}

impl From<leviath_providers::rhai_provider::ProviderMeta> for ProviderScriptMeta {
    fn from(meta: leviath_providers::rhai_provider::ProviderMeta) -> Self {
        Self {
            provider: meta.provider,
            description: meta.description,
            default_model: meta.default_model,
            max_context_tokens: meta.max_context_tokens,
            max_output_tokens: meta.max_output_tokens,
            supports_streaming: meta.supports_streaming,
        }
    }
}

/// A provider script's annotations, for a script that is one.
///
/// Parsing never fails - an unrecognized or unparseable directive is ignored
/// and the default kept - so this is independent of whether the script
/// compiles, and a draft that does not compile still reports what it declares.
fn provider_meta(kind: ScriptKind, content: &str) -> Option<ProviderScriptMeta> {
    match kind {
        ScriptKind::Provider => {
            Some(leviath_providers::rhai_provider::parse_provider_annotations(content).into())
        }
        _ => None,
    }
}

/// One script in the listing.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ScriptItem {
    /// `tool`, `region_hook`, `stage_hook`, `output_validator` or `provider`,
    /// or [`CANDIDATE_KIND`] for a file nothing has claimed yet.
    pub(super) kind: String,
    /// The `{name}` the read and write routes address it by, once a caller has
    /// picked a `kind` for it. `/`-separated for a file in a subdirectory.
    pub(super) name: String,
    /// `agent` when it belongs to one agent, `global` when every agent gets it.
    pub(super) source: String,
    /// Which agent, for an agent-scoped script.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) agent: Option<String>,
    /// The file on disk.
    pub(super) path: String,
    /// The same file relative to the agent's own directory, which is the
    /// spelling a manifest wants (`validators/a2ui.rhai`). Absent for a
    /// machine-wide script, which no blueprint contains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) relative_path: Option<String>,
    /// Whether something loads this file as this `kind`: a `tools/` directory
    /// the daemon scans, or a manifest that names it. `false` for a candidate.
    pub(super) declared: bool,
    /// Whether it compiles right now. Absent for a candidate: nothing says
    /// which of the five compilers such a file is for, and picking one would be
    /// a claim about the file rather than a fact about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) compiles: Option<bool>,
    /// Why not, when it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
    /// What the script declares about itself, for `kind: "provider"` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) provider: Option<ProviderScriptMeta>,
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
    /// What the script declares about itself, for `kind: "provider"` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) provider: Option<ProviderScriptMeta>,
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

/// How a manifest-declared path is addressed, or `None` when the manifest
/// declared something these routes cannot address.
///
/// A manifest may name any path inside the agent's directory, a subdirectory
/// included, and [`addressed_path`] handles those. What it does not handle is a
/// declaration that is not a `.rhai` file at all: `addressed_path` appends the
/// extension, so a declared `notes.txt` would be reported as `notes.txt.rhai`,
/// a different file. Requiring the suffix here keeps the listing off it.
fn declared_address(declared: &str) -> Option<Addressed> {
    let stem = declared.strip_suffix(".rhai")?;
    addressed_path(stem)
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

/// The `.rhai` files one drop-in directory holds, sorted.
///
/// Sorted because a listing that reorders itself between two calls is a listing
/// nobody can edit against, and `read_dir` order is whatever the filesystem
/// feels like. A directory that cannot be read is an empty one: the tools and
/// providers directories are both optional, and neither missing one is an
/// error to report.
fn rhai_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "rhai"))
        .collect();
    paths.sort();
    paths
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

    for path in rhai_files(dir) {
        let (compiles, error) = match reasons.get(&path) {
            Some(reason) => (false, Some(reason.clone())),
            None => (true, None),
        };
        let name = stem_of(&path);
        out.push(ScriptItem {
            kind: ScriptKind::Tool.as_str().to_string(),
            // A tool sits in `tools/`, so its manifest-relative path carries
            // that directory even though the route addresses it without one.
            relative_path: agent.map(|_| format!("tools/{name}.rhai")),
            name,
            source: scope.to_string(),
            agent: agent.map(str::to_string),
            path: path.display().to_string(),
            declared: true,
            compiles: Some(compiles),
            error,
            provider: None,
        });
    }
}

/// List the `.rhai` files in the providers directory.
///
/// There is no `discover` to lean on the way [`collect_tools`] does: nothing
/// scans this directory ahead of time, because a provider script is compiled
/// only when an agent first names it. So each file is checked here, with the
/// same `check_source` a write and a validate use.
///
/// Every entry is `global`. A provider belongs to the machine, not to an agent,
/// which is why this runs whether or not the request named one.
fn collect_providers(dir: &Path, out: &mut Vec<ScriptItem>) {
    for path in rhai_files(dir) {
        let label = path.display().to_string();
        let (compiles, error, meta) = match std::fs::read_to_string(&path) {
            Ok(content) => {
                let (compiles, error) =
                    status_pair(compile_status(ScriptKind::Provider, &label, &content, &[]));
                (
                    compiles,
                    error,
                    provider_meta(ScriptKind::Provider, &content),
                )
            }
            Err(e) => (false, Some(format!("cannot read '{label}': {e}")), None),
        };
        out.push(ScriptItem {
            kind: ScriptKind::Provider.as_str().to_string(),
            name: stem_of(&path),
            source: "global".to_string(),
            agent: None,
            path: label,
            relative_path: None,
            declared: true,
            compiles: Some(compiles),
            error,
            provider: meta,
        });
    }
}

/// List the hooks and validators the agent's manifest declares.
///
/// An agent with no manifest, or one that will not parse, contributes nothing:
/// the blueprint routes are where a broken manifest is reported, and a script
/// listing that failed outright would take the tools down with it.
fn collect_declared(dir: &Path, agent: &str, out: &mut Vec<ScriptItem>) {
    let Ok(text) = std::fs::read_to_string(dir.join(leviath_core::files::MANIFEST_FILENAME)) else {
        return;
    };
    let Ok(bp) = leviath_core::manifest::parse_manifest(&text) else {
        return;
    };

    for ((kind, declared), hooks) in declared_scripts(&bp) {
        let Some(addressed) = declared_address(&declared) else {
            continue;
        };
        // Every component is a safe one, so this join cannot leave `dir`
        // lexically; a symlink at one of them still can, which is what the
        // read route's `guard` is for. Nothing is opened here but the file.
        let path = addressed.path_in(dir);
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
            name: addressed.name,
            source: "agent".to_string(),
            agent: Some(agent.to_string()),
            path: path.display().to_string(),
            relative_path: Some(addressed.relative),
            declared: true,
            compiles: Some(compiles),
            error,
            provider: None,
        });
    }
}

// ─── Candidates ─────────────────────────────────────────────────────────────

/// The `kind` a candidate is reported under.
///
/// Not a lie about the file: a `.rhai` beside an agent that nothing declares
/// could be a region hook, a stage hook, a validator or a tool, and the file
/// itself does not say which. The declaration does, which is the thing that has
/// not happened yet. `ScriptKind::parse` rejects this spelling, so a client
/// cannot hand it back as a kind either.
const CANDIDATE_KIND: &str = "unknown";

/// How deep below the agent's own directory the candidate scan walks.
///
/// The convention the docs use puts a script one level down
/// (`validators/a2ui.rhai`), and nothing anybody writes goes past two. Four
/// leaves room and still stops the walk from descending a whole source tree,
/// which is what `[agent_paths]` pointed at a working checkout would be.
const CANDIDATE_MAX_DEPTH: usize = 4;

/// How many candidate files one listing reports. A picker shows a list a person
/// reads, and past a couple of hundred the honest answer is not a longer list.
const CANDIDATE_MAX_FILES: usize = 256;

/// How many directories one scan opens. Depth alone does not bound a *wide*
/// tree, and this walk runs on a request anybody holding the token can repeat.
const CANDIDATE_MAX_DIRS: usize = 128;

/// Every `.rhai` file under the agent's own directory that nothing already
/// listed, so a picker can offer a validator no manifest names yet.
///
/// The manifest-derived listing is circular for a picker: a file appears once
/// something declares it, and declaring it is what the picker is for. This is
/// the other half - what *could* be named - which is why it is kept behind
/// `?include=candidates`: a client reading the listing as "what will load"
/// still gets exactly that.
///
/// Bounded three ways, by [`CANDIDATE_MAX_DEPTH`], [`CANDIDATE_MAX_DIRS`] and
/// [`CANDIDATE_MAX_FILES`], because the directory being walked is one a config
/// file chose and a request anybody can repeat.
///
/// Containment is asked per entry rather than assumed from where the walk
/// started: the agents directory itself may be a symlink, and a link inside the
/// agent's directory pointing out of it is followed by `read_dir` and by
/// nothing here. A name that is not a safe path component is skipped rather
/// than guessed at - a file this cannot spell back is one no manifest could
/// name and no route could fetch.
fn collect_candidates(base: &Path, agent: &str, out: &mut Vec<ScriptItem>) {
    let seen: HashSet<String> = out.iter().map(|item| item.path.clone()).collect();
    let mut found = 0usize;
    let mut opened = 0usize;
    // Breadth first, so when a cap cuts the walk short what survives is the
    // shallow half - `validators/` rather than `vendor/a/b/c/`.
    let mut queue: std::collections::VecDeque<(Vec<String>, usize)> =
        std::collections::VecDeque::from([(Vec::new(), 0usize)]);

    while let Some((dirs, depth)) = queue.pop_front() {
        if opened == CANDIDATE_MAX_DIRS {
            return;
        }
        opened += 1;
        let here = dirs.iter().fold(base.to_path_buf(), |acc, d| acc.join(d));
        let Ok(entries) = std::fs::read_dir(&here) else {
            continue;
        };
        // Sorted for the same reason `rhai_files` sorts: a listing that
        // reorders itself between two calls is one nobody can edit against.
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            // Lossy on purpose: a name that is not UTF-8 comes back with a
            // replacement character, which `is_safe_path_component` then
            // refuses along with every other name this could not spell back.
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| leviath_core::is_safe_path_component(name))
            .collect();
        names.sort();

        for name in names {
            let path = here.join(&name);
            if !leviath_core::resolves_within(&path, base) {
                continue;
            }
            // `is_dir` follows symlinks, which the check above has already
            // confined to this agent's directory. A dangling link is not a
            // directory and is reported as the file the directory says is
            // there; reading it is where it fails, honestly and with a reason.
            if path.is_dir() {
                if depth < CANDIDATE_MAX_DEPTH {
                    let mut below = dirs.clone();
                    below.push(name);
                    queue.push_back((below, depth + 1));
                }
                continue;
            }
            let Some(stem) = name.strip_suffix(".rhai") else {
                continue;
            };
            // A file called exactly `.rhai` has no name left to address it by.
            if stem.is_empty() {
                continue;
            }
            if seen.contains(&path.display().to_string()) {
                continue;
            }
            if found == CANDIDATE_MAX_FILES {
                return;
            }
            found += 1;
            let mut parts = dirs.clone();
            parts.push(stem.to_string());
            let addressed = parts.join("/");
            out.push(ScriptItem {
                kind: CANDIDATE_KIND.to_string(),
                name: addressed.clone(),
                source: "agent".to_string(),
                agent: Some(agent.to_string()),
                path: path.display().to_string(),
                relative_path: Some(format!("{addressed}.rhai")),
                declared: false,
                compiles: None,
                error: None,
                provider: None,
            });
        }
    }
}

/// The one `?include=` token the listing recognizes.
const INCLUDE_CANDIDATES: &str = "candidates";

/// Whether `?include=` asked for candidates.
///
/// A comma-separated list, so a later listing can add a second thing to include
/// without a second parameter. An unrecognized token is a 400 rather than a
/// silent no-op: the whole point of the parameter is that its absence and its
/// presence give different answers, so a client that misspells it would read a
/// short listing as "this agent has no files".
fn wants_candidates(include: Option<&str>) -> Result<bool, ApiError> {
    let Some(raw) = include else {
        return Ok(false);
    };
    let mut wanted = false;
    for token in raw.split(',') {
        match token.trim() {
            // `?include=` with nothing after it asks for nothing, which is what
            // a console sends when its checkbox is clear.
            "" => {}
            INCLUDE_CANDIDATES => wanted = true,
            other => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    format!("Unknown include '{other}': expected {INCLUDE_CANDIDATES}"),
                ));
            }
        }
    }
    Ok(wanted)
}

// ─── Handlers ───────────────────────────────────────────────────────────────

/// `GET /api/scripts[?agent=<name>]`: every script this scope can see.
///
/// With an agent, that is the agent's own `tools/`, the hooks and validators its
/// manifest declares, and the machine's global scripts. Without one, the global
/// scripts alone. `source` is what separates "this agent has it" from
/// "everything on this machine has it".
///
/// Providers are listed either way. Nothing scopes one to an agent, so leaving
/// them out of the agent view would only mean a console had to make a second
/// request to draw the same page.
///
/// `?include=candidates` adds the `.rhai` files beside the agent that nothing
/// declares, marked `declared: false` under [`CANDIDATE_KIND`]. Only the agent
/// half of the answer grows: every file in the two global directories is
/// already listed from disk, so "undeclared" describes nothing there.
pub(super) async fn list_scripts(
    State(state): State<AppState>,
    Query(q): Query<ListScriptsQuery>,
) -> Result<Json<ScriptsResp>, ApiError> {
    let candidates = wants_candidates(q.include.as_deref())?;
    let mut scripts = Vec::new();
    if let Some(name) = q.agent.as_deref() {
        let dir = agent_dir(&state.current_config(), name)?;
        collect_tools(&dir.join("tools"), "agent", Some(name), &mut scripts);
        collect_declared(&dir, name, &mut scripts);
        if candidates {
            collect_candidates(&dir, name, &mut scripts);
        }
    }
    collect_tools(&global_tools_dir(), "global", None, &mut scripts);
    collect_providers(&global_providers_dir(), &mut scripts);
    Ok(Json(ScriptsResp { scripts }))
}

/// `GET /api/scripts/{kind}/{name}[?agent=<name>]`: the source text.
pub(super) async fn get_script(
    State(state): State<AppState>,
    AxumPath((kind, name)): AxumPath<(String, String)>,
    Query(q): Query<ScriptQuery>,
) -> Result<Json<ScriptSource>, ApiError> {
    let target = resolve(&state.current_config(), &kind, &name, q.agent.as_deref())?;
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
    let meta = provider_meta(target.kind, &content);
    // Returned verbatim. A provider takes its key from `initialize(config)`,
    // which `/api/config` already redacts to a boolean and a list of key names,
    // or from `env_var`, which reads the daemon's environment and never the
    // file. Nothing puts a secret into the source that the author did not type
    // there, so there is nothing here that redacting would protect and a great
    // deal that it would break: an editor that saved what it was shown would
    // write the redaction back over the real script.
    Ok(Json(ScriptSource {
        kind: target.kind.as_str().to_string(),
        name: target.name.clone(),
        source: target.scope.to_string(),
        agent: target.agent.clone(),
        path: label,
        content,
        compiles,
        error,
        provider: meta,
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
    State(state): State<AppState>,
    AxumPath((kind, name)): AxumPath<(String, String)>,
    Query(q): Query<ScriptQuery>,
    Json(body): Json<WriteScriptReq>,
) -> Result<Json<ScriptWritten>, ApiError> {
    let target = resolve(&state.current_config(), &kind, &name, q.agent.as_deref())?;
    if let Err(e) = std::fs::create_dir_all(&target.dir) {
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot create '{}': {e}", target.dir.display()),
        ));
    }
    // After the directory exists, so containment is asked of a path that can be
    // canonicalized rather than of one that merely might be.
    guard(&target, Presence::Optional)?;
    // Only now: a name may carry a subdirectory, and creating one before the
    // containment check would let a symlink planted at an intermediate name
    // make this `mkdir` outside the agent's directory. `guard` has just proved
    // the whole path resolves inside it.
    if let Err(e) = std::fs::create_dir_all(&target.file_dir) {
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot create '{}': {e}", target.file_dir.display()),
        ));
    }
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
    State(state): State<AppState>,
    AxumPath((kind, name)): AxumPath<(String, String)>,
    Query(q): Query<ScriptQuery>,
) -> Result<StatusCode, ApiError> {
    let target = resolve(&state.current_config(), &kind, &name, q.agent.as_deref())?;
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
            format!("Unknown script kind '{}': expected {KIND_LIST}", body.kind),
        ));
    };
    let hooks: Vec<&str> = body.hooks.iter().map(String::as_str).collect();
    let (valid, error) = status_pair(compile_status(kind, "script", &body.content, &hooks));
    Ok(Json(ValidateScriptResp { valid, error }))
}

#[cfg(test)]
#[path = "scripts_tests.rs"]
mod tests;

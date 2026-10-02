//! Reading blueprints: the graph a run executed, and the one installed now.
//!
//! These are two different questions, and answering the first with the second
//! is how "what did this run do" became unanswerable. A run's file carries the
//! graph it was resolved to, so it answers for itself even after the
//! installed `agent.toml` is edited or deleted.
//!
//! Parsing an installed file is cached by digest. Five hundred listings of one
//! blueprint parse it once.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use leviath_blueprint::BlueprintFile;
use leviath_runtime::spec::graph::RunGraph;
use leviath_runtime::spec::run_spec::RunSpec;

use super::error::ServeError;

/// Where a blueprint was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlueprintSource {
    /// The run's own file: the graph the run was resolved to and executed.
    Snapshot,
    /// The installed `agent.toml`.
    Installed,
}

/// A blueprint read: what it is called, its version, and its graph.
///
/// A run's graph may come from a graph its caller wrote rather than from an
/// installed blueprint, and such a graph's title need not be a valid
/// blueprint name, so the name here is plain text.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParsedBlueprint {
    /// What it is called: `[blueprint] name`, or for a run, the name it was
    /// spawned under.
    pub(crate) name: String,
    /// `[blueprint] version`. Empty for a run of a graph its caller wrote.
    pub(crate) version: String,
    /// Its graph, with the title and description `[blueprint]` gives it.
    pub(crate) graph: RunGraph,
}

impl ParsedBlueprint {
    /// What the blueprint says it does. Empty when it says nothing.
    pub(crate) fn description(&self) -> &str {
        self.graph.description.as_deref().unwrap_or_default()
    }

    /// The blueprint in a parsed `agent.toml`.
    pub(crate) fn of_file(file: &BlueprintFile) -> Self {
        Self {
            name: file.blueprint.name.to_string(),
            version: file.blueprint.version.clone(),
            graph: file.run_graph(),
        }
    }

    /// The graph a run was resolved to, named the way its runs are listed.
    pub(crate) fn of_run(spec: &RunSpec) -> Self {
        let version = match &spec.origin {
            leviath_runtime::spec::run_spec::SpecOrigin::Blueprint { version, .. }
            | leviath_runtime::spec::run_spec::SpecOrigin::BlueprintFile { version, .. } => {
                version.clone()
            }
            leviath_runtime::spec::run_spec::SpecOrigin::Raw
            | leviath_runtime::spec::run_spec::SpecOrigin::Recorded { .. } => String::new(),
        };
        let name = spec
            .origin
            .blueprint_name()
            .map(str::to_string)
            .or_else(|| spec.graph.title.clone())
            .unwrap_or_default();
        Self {
            name,
            version,
            graph: spec.graph.clone(),
        }
    }
}

/// A blueprint as served: the parse, its identity, and where it was read.
#[derive(Debug, Clone)]
pub(crate) struct ReadBlueprint {
    /// The blueprint.
    pub(crate) parsed: Arc<ParsedBlueprint>,
    /// Lowercase hex SHA-256: of the file's bytes for an installed blueprint,
    /// of the graph's JSON for a run's.
    pub(crate) digest: String,
    /// Which it was read from.
    pub(crate) source: BlueprintSource,
}

/// The blueprint a run executed: the graph in its run file.
///
/// That graph has the run's inputs applied (a stage's model, an iteration
/// cap), so it can differ from the installed revision it came from even when
/// the file is unchanged. Its digest is therefore of the graph itself, and
/// `Run.blueprintDigest` is what names the installed revision.
pub(crate) fn blueprint_for_run(run_id: &str) -> Result<ReadBlueprint, ServeError> {
    let reader = super::run_file::require(run_id)?;
    let spec = reader.spec();
    Ok(ReadBlueprint {
        digest: graph_digest(&spec.graph),
        parsed: Arc::new(ParsedBlueprint::of_run(spec)),
        source: BlueprintSource::Snapshot,
    })
}

/// The digest of a graph: SHA-256 of its JSON.
fn graph_digest(graph: &RunGraph) -> String {
    let json = serde_json::to_vec(graph).expect("a run graph always serializes to JSON");
    leviath_core::mime::store::sha256_hex(&json)
}

/// An installed `agent.toml`, as read, before it is parsed.
#[derive(Debug)]
pub(crate) struct ManifestText {
    /// The file's text, verbatim.
    pub(crate) text: String,
    /// Lowercase hex SHA-256 of `text`: the blueprint revision's identity.
    pub(crate) digest: String,
}

impl ManifestText {
    /// An installed blueprint's text.
    ///
    /// The blueprint catalogue already holds the text it read while walking
    /// the agent directories, so this takes that text rather than reading the
    /// file a second time.
    pub(crate) fn installed(text: String) -> Self {
        Self {
            digest: digest_of(&text),
            text,
        }
    }
}

/// The identity of a blueprint file: the SHA-256 of its bytes, lowercase hex.
///
/// The same digest a spawn pins the installed revision to, so a digest
/// computed here and `Run.blueprintDigest` are comparable. That comparison is
/// how a client tells "this run executed what is installed now" from "this
/// run executed something else".
pub(crate) fn digest_of(text: &str) -> String {
    leviath_core::mime::store::sha256_hex(text.as_bytes())
}

/// `text` as an `agent.toml`: itself when it is one, or the `agent.toml` an
/// `agent.leviath` converts to, which is what older clients still send. An
/// `agent.leviath` that does not convert is every problem the conversion
/// found. `Ok(true)` says the text was converted.
pub(crate) fn as_agent_toml(text: &str) -> Result<(String, bool), Vec<String>> {
    let old = toml::from_str::<toml::Table>(text)
        .is_ok_and(|table| table.contains_key("agent") && !table.contains_key("blueprint"));
    match old {
        true => crate::commands::blueprint::convert(text)
            .map(|converted| (converted, true))
            .map_err(|problems| {
                problems
                    .into_iter()
                    .map(|p| format!("agent.leviath does not convert to agent.toml: {p}"))
                    .collect()
            }),
        false => Ok((text.to_string(), false)),
    }
}

/// What a client is told when the blueprint it sent was an `agent.leviath`.
pub(crate) const CONVERTED_NOTE: &str =
    "this is an agent.leviath blueprint; it is saved as the agent.toml it converts to";

/// Parse an `agent.toml` and check that its graph holds together, the way
/// `lev validate` and a spawn do. Each problem is one entry, naming the key it
/// is at.
pub(crate) fn parse_blueprint(text: &str) -> Result<ParsedBlueprint, Vec<String>> {
    let file = BlueprintFile::parse(text).map_err(|e| vec![e])?;
    let parsed = ParsedBlueprint::of_file(&file);
    let at = leviath_runtime::spec::issues::SpecPath::root().field("graph");
    parsed
        .graph
        .validate(&at)
        .map_err(|issues| issues.iter().map(ToString::to_string).collect::<Vec<_>>())?;
    Ok(parsed)
}

/// Parsed blueprints, kept by digest.
///
/// Content-addressed rather than keyed by name or path, so an edited
/// blueprint is a different key rather than a stale entry.
#[derive(Clone, Default)]
pub(crate) struct BlueprintCache {
    parsed: Arc<Mutex<HashMap<String, Arc<ParsedBlueprint>>>>,
}

/// How many parsed blueprints one server keeps.
///
/// Beyond this the cache is cleared rather than trimmed by age. A blueprint is
/// small, the bound exists so a machine with thousands of distinct files
/// cannot grow this without limit, and "clear and refill" costs one parse per
/// blueprint actually in use rather than the bookkeeping a proper eviction
/// order would need.
const MAX_PARSED: usize = 256;

impl BlueprintCache {
    /// The parsed blueprint for this file, parsing it the first time.
    ///
    /// A file that will not parse is reported rather than cached: the failure
    /// is about this document, and caching it would answer the same way after
    /// the file is fixed.
    pub(crate) fn parse(
        &self,
        manifest: &ManifestText,
    ) -> Result<Arc<ParsedBlueprint>, ServeError> {
        if let Some(hit) = self.lookup(&manifest.digest) {
            return Ok(hit);
        }
        // Internal, not a bad request: whoever asked did not write this file.
        let parsed = parse_blueprint(&manifest.text).map_err(|e| {
            ServeError::Internal(format!("Blueprint will not parse: {}", e.join("; ")))
        })?;
        let parsed = Arc::new(parsed);
        self.store(manifest.digest.clone(), Arc::clone(&parsed));
        Ok(parsed)
    }

    /// The cached parse for a digest, if this server holds one.
    fn lookup(&self, digest: &str) -> Option<Arc<ParsedBlueprint>> {
        leviath_core::sync::lock(&self.parsed).get(digest).cloned()
    }

    /// Remember one parse, clearing the cache when it has grown past its
    /// bound.
    fn store(&self, digest: String, parsed: Arc<ParsedBlueprint>) {
        let mut cache = leviath_core::sync::lock(&self.parsed);
        if cache.len() >= MAX_PARSED {
            cache.clear();
        }
        cache.insert(digest, parsed);
    }
}

#[cfg(test)]
#[path = "blueprints_tests.rs"]
mod tests;

/// Where an installed blueprint's directory is.
///
/// The name arrives from a client, and `Path::join` resists neither `..` nor an
/// absolute path, so it is checked before it is joined: this is the gate
/// between "install an agent" and "write a file anywhere".
pub(crate) fn blueprint_dir(name: &str) -> Result<PathBuf, ServeError> {
    if !leviath_core::is_safe_path_component(name) {
        return Err(ServeError::BadRequest(format!(
            "Invalid blueprint name '{name}': names may contain only letters, digits, \
             '.', '_' and '-'"
        )));
    }
    Ok(super::super::blueprints::agents_dir().join(name))
}

/// A blueprint as it was written to disk.
pub(crate) struct WrittenBlueprint {
    /// Its directory.
    pub(crate) dir: PathBuf,
    /// The file's text, as written.
    pub(crate) manifest: ManifestText,
    /// The parse of it.
    pub(crate) parsed: Arc<ParsedBlueprint>,
}

/// Install a blueprint, or replace the one under that name.
///
/// `replacing` decides which way a name that is already taken goes: a create
/// refuses it, and an edit requires it. Saying so here rather than at each call
/// site is what keeps "create" from quietly overwriting somebody's agent.
///
/// The file must call itself by the name it is installed under: a daemon
/// finds an installed blueprint by its directory and refuses one whose
/// `[blueprint] name` says otherwise, so writing one would install an agent
/// nothing can run.
pub(crate) fn write_blueprint(
    name: &str,
    manifest: String,
    replacing: bool,
) -> Result<WrittenBlueprint, ServeError> {
    let (manifest, _) = as_agent_toml(&manifest).map_err(|problems| {
        ServeError::BadRequest(format!("Invalid blueprint: {}", problems.join("; ")))
    })?;
    let parsed = parse_blueprint(&manifest)
        .map_err(|e| ServeError::BadRequest(format!("Invalid blueprint: {}", e.join("; "))))?;
    let dir = blueprint_dir(name)?;
    if parsed.name != name {
        return Err(ServeError::BadRequest(format!(
            "The blueprint calls itself '{}' in [blueprint] name; install it as '{}' or \
             rename it to '{name}'",
            parsed.name, parsed.name
        )));
    }
    let path = dir.join(leviath_blueprint::FILE_NAME);
    // `is_file`, not `exists`: a *directory* at the file's path is not a
    // blueprint, and reporting one as "already installed" would hide the write
    // failure that is actually coming.
    match (replacing, path.is_file()) {
        (true, false) => {
            return Err(ServeError::NotFound(format!(
                "Blueprint '{name}' not found"
            )));
        }
        (false, true) => {
            return Err(ServeError::Conflict(format!(
                "Blueprint '{name}' already exists; edit it instead of creating it again"
            )));
        }
        _ => {}
    }
    std::fs::create_dir_all(&dir)
        .map_err(|e| ServeError::Internal(format!("Failed to create directory: {e}")))?;
    std::fs::write(&path, &manifest)
        .map_err(|e| ServeError::Internal(format!("Failed to write blueprint: {e}")))?;
    Ok(WrittenBlueprint {
        dir,
        manifest: ManifestText::installed(manifest),
        parsed: Arc::new(parsed),
    })
}

/// Uninstall a blueprint.
///
/// Runs that used it keep the graph they ran in their own files, so removing
/// the installed copy does not take their history with it.
pub(crate) fn remove_blueprint(name: &str) -> Result<(), ServeError> {
    let dir = blueprint_dir(name)?;
    if !dir.exists() {
        return Err(ServeError::NotFound(format!(
            "Blueprint '{name}' not found"
        )));
    }
    std::fs::remove_dir_all(&dir)
        .map_err(|e| ServeError::Internal(format!("Failed to delete blueprint: {e}")))
}

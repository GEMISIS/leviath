//! Compiling a run's code into the components that run it.
//!
//! A run's hooks, output validators and custom regions name their code by
//! [`CodeRef`], and the run file holds that code by digest. Binding compiles
//! each piece once from the run file's own copy (never from disk, which may
//! have changed since) and places the compiled set on the run's entity:
//! [`StageHookScripts`] and [`OutputValidators`] as the pipeline reads them,
//! and [`RegionScripts`] for insertion to hand to the context window.
//!
//! Each compiled piece is keyed by [`code_key`]: the path a file was named
//! by, which is how the pipeline has always looked hooks up, or
//! `inline:<digest>` for code written inline.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use bevy_ecs::component::Component;
use leviath_scripting::region_hook::RegionScript;

use crate::components::{OutputValidators, StageHookScripts};
use crate::spec::env::{Bindings, CodeFiles};
use crate::spec::graph::{CodeRef, OutputDef, RegionKind, RegionLayoutDef};
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::names::Digest;
use crate::spec::run_spec::RunSpec;

/// The compiled custom-region scripts of a run, by [`code_key`]. Placed by
/// binding for insertion to move onto the run's context window.
#[derive(Component, Debug, Clone, Default)]
pub struct RegionScripts(pub HashMap<String, Arc<RegionScript>>);

/// The key compiled code is filed under: the path a file was named by, or
/// `inline:<digest>` for inline code.
pub fn code_key(code: &CodeRef, digest: &Digest) -> String {
    match code {
        CodeRef::File(path) => path.clone(),
        CodeRef::Inline(_) => format!("inline:{digest}"),
    }
}

/// The source of some code the spec names, from the run file's copy.
fn source<'a>(
    spec: &RunSpec,
    code: &'a CodeFiles,
    reference: &CodeRef,
    path: &SpecPath,
) -> Result<(String, &'a str), SpawnIssues> {
    let missing = |why: &str| {
        SpawnIssues::from(SpawnIssue::new(
            path.clone(),
            IssueCode::Missing,
            why.to_string(),
        ))
    };
    let digest = spec
        .code_digest(reference)
        .ok_or_else(|| missing("the run records no code for this"))?;
    let bytes = code
        .get(digest)
        .ok_or_else(|| missing("the run file holds no code for this"))?;
    let text = std::str::from_utf8(bytes).map_err(|e| {
        SpawnIssues::from(SpawnIssue::new(
            path.clone(),
            IssueCode::Invalid,
            format!("the code is not UTF-8 text: {e}"),
        ))
    })?;
    Ok((code_key(reference, digest), text))
}

fn compile_failed(path: &SpecPath, error: impl ToString) -> SpawnIssues {
    SpawnIssue::new(path.clone(), IssueCode::Invalid, error.to_string()).into()
}

/// Compile every hook, validator and custom region the spec names from
/// `code`, reporting every one that will not compile.
pub fn compile(spec: &RunSpec, code: &CodeFiles) -> Result<Bindings, SpawnIssues> {
    let mut issues = SpawnIssues::new();
    let hooks = issues.take(hooks(spec, code));
    let validators = issues.take(validators(spec, code));
    let regions = issues.take(regions(spec, code));
    let (Some(hooks), Some(validators), Some(regions)) = (hooks, validators, regions) else {
        return Err(issues);
    };
    let mut bindings = Bindings::new();
    if !hooks.is_empty() {
        bindings = bindings.with(StageHookScripts(hooks));
    }
    if !validators.is_empty() {
        bindings = bindings.with(OutputValidators::new(validators));
    }
    if !regions.is_empty() {
        bindings = bindings.with(RegionScripts(regions));
    }
    Ok(bindings)
}

type Compiled<T> = Result<HashMap<String, Arc<T>>, SpawnIssues>;

fn hooks(spec: &RunSpec, code: &CodeFiles) -> Compiled<leviath_scripting::stage_hook::HookScript> {
    // What each piece of code is wanted for, so code backing two hooks is
    // checked for both and compiled once.
    let mut wanted: BTreeMap<String, (String, SpecPath, Vec<&str>)> = BTreeMap::new();
    let mut issues = SpawnIssues::new();
    for stage in &spec.graph.stages {
        for (hook, reference) in stage.hooks.iter() {
            let path = SpecPath::root()
                .field("stages")
                .key(stage.name.as_str())
                .field("hooks")
                .field(hook);
            match source(spec, code, reference, &path) {
                Ok((key, text)) => {
                    wanted
                        .entry(key)
                        .or_insert((text.to_string(), path, Vec::new()))
                        .2
                        .push(hook);
                }
                Err(issue) => issues.absorb(issue),
            }
        }
    }
    let mut compiled = HashMap::new();
    for (key, (text, path, hooks)) in wanted {
        match leviath_scripting::stage_hook::compile(&key, &text, &hooks) {
            Ok(script) => {
                compiled.insert(key, Arc::new(script));
            }
            Err(e) => issues.absorb(compile_failed(&path, e)),
        }
    }
    issues.into_result(compiled)
}

fn validators(
    spec: &RunSpec,
    code: &CodeFiles,
) -> Compiled<leviath_scripting::output_validator::OutputValidator> {
    let outputs: Vec<(SpecPath, &OutputDef)> = spec
        .graph
        .output
        .iter()
        .map(|o| (SpecPath::root().field("output"), o))
        .chain(spec.graph.stages.iter().filter_map(|s| {
            let path = SpecPath::root()
                .field("stages")
                .key(s.name.as_str())
                .field("output");
            s.output.as_ref().map(|o| (path, o))
        }))
        .collect();
    let mut issues = SpawnIssues::new();
    let mut compiled = HashMap::new();
    for (path, output) in outputs {
        let Some(reference) = &output.validator else {
            continue;
        };
        let path = path.field("validator");
        let compiled_one = source(spec, code, reference, &path).and_then(|(key, text)| {
            match compiled.contains_key(&key) {
                true => Ok(None),
                false => leviath_scripting::output_validator::compile(&key, text)
                    .map(|v| Some((key, v)))
                    .map_err(|e| compile_failed(&path, e)),
            }
        });
        match compiled_one {
            Ok(Some((key, validator))) => {
                compiled.insert(key, Arc::new(validator));
            }
            Ok(None) => {}
            Err(issue) => issues.absorb(issue),
        }
    }
    issues.into_result(compiled)
}

fn regions(spec: &RunSpec, code: &CodeFiles) -> Compiled<RegionScript> {
    let layouts: Vec<(SpecPath, &RegionLayoutDef)> =
        std::iter::once((SpecPath::root().field("layout"), &spec.graph.layout))
            .chain(spec.graph.stages.iter().filter_map(|s| {
                let path = SpecPath::root()
                    .field("stages")
                    .key(s.name.as_str())
                    .field("layout");
                s.layout.as_ref().map(|l| (path, l))
            }))
            .collect();
    let mut issues = SpawnIssues::new();
    let mut compiled = HashMap::new();
    for (path, layout) in layouts {
        for region in &layout.regions {
            let RegionKind::Custom {
                code: reference, ..
            } = &region.kind
            else {
                continue;
            };
            let path = path
                .field("regions")
                .key(region.name.as_str())
                .field("kind");
            let compiled_one = source(spec, code, reference, &path).and_then(|(key, text)| {
                match compiled.contains_key(&key) {
                    true => Ok(None),
                    false => leviath_scripting::region_hook::compile(&key, text)
                        .map(|s| Some((key, s)))
                        .map_err(|e| compile_failed(&path, e)),
                }
            });
            match compiled_one {
                Ok(Some((key, script))) => {
                    compiled.insert(key, Arc::new(script));
                }
                Ok(None) => {}
                Err(issue) => issues.absorb(issue),
            }
        }
    }
    issues.into_result(compiled)
}

#[cfg(test)]
#[path = "scripts_tests.rs"]
mod tests;

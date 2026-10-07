//! Step 7: every piece of code the graph names, read once and checked for
//! each use.
//!
//! A hook, a validator, a custom region, a code seed, a mime check, a
//! dependency check and an install script all name code the same way, by a
//! [`CodeRef`]. Each distinct reference is read once, checked for every use
//! the graph makes of it (a file backing two hooks is checked as both), and
//! stored by digest, so the run file carries the exact bytes the run ran.
//!
//! Only a blueprint can name code by file: its files sit beside it and ship
//! with it. A graph the caller wrote has no directory, so its code is inline.

use super::Source;
use super::regions::layouts;
use crate::spec::env::{CodeFiles, CodeUse, ResolveEnv};
use crate::spec::graph::{CodeRef, Needs, RegionKind, RunGraph, Seed};
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::names::Digest;
use crate::spec::request::SpawnRequest;

/// The run's code, read.
#[derive(Debug, Clone, Default)]
pub(super) struct Code {
    /// The bytes, by digest.
    pub(super) files: CodeFiles,
    /// Each reference the graph makes, and the digest it read as.
    pub(super) refs: Vec<(CodeRef, Digest)>,
}

impl Code {
    /// The bytes a reference read as, when it was read.
    pub(super) fn bytes(&self, code: &CodeRef) -> Option<&[u8]> {
        self.refs
            .iter()
            .find(|(c, _)| c == code)
            .and_then(|(_, digest)| self.files.get(digest))
            .map(Vec::as_slice)
    }

    /// Add code the host found for the run's tools, each reference once.
    pub(super) fn add_found(&mut self, found: Vec<(CodeRef, Vec<u8>)>) {
        for (code, bytes) in found {
            if self.refs.iter().any(|(c, _)| *c == code) {
                continue;
            }
            let digest = Digest::of(&bytes);
            self.refs.push((code, digest.clone()));
            self.files.insert(digest, bytes);
        }
    }
}

/// One reference and every place it is used.
struct Wanted {
    code: CodeRef,
    uses: Vec<(CodeUse, SpecPath)>,
}

fn want(all: &mut Vec<Wanted>, code: &CodeRef, used_as: CodeUse, at: SpecPath) {
    let index = match all.iter().position(|w| w.code == *code) {
        Some(index) => index,
        None => {
            all.push(Wanted {
                code: code.clone(),
                uses: Vec::new(),
            });
            all.len() - 1
        }
    };
    let uses = &mut all[index].uses;
    if !uses.iter().any(|(u, _)| *u == used_as) {
        uses.push((used_as, at));
    }
}

/// Every reference the graph and the request make, with its use and path.
fn collect(graph: &RunGraph, request: &SpawnRequest, at: &SpecPath) -> Vec<Wanted> {
    let mut all = Vec::new();
    let validator =
        |o: &Option<crate::spec::graph::OutputDef>| o.as_ref().and_then(|o| o.validator.clone());
    if let Some(code) = validator(&request.output) {
        let path = SpecPath::root().field("output").field("validator");
        want(&mut all, &code, CodeUse::Validator, path);
    }
    if let Some(code) = validator(&graph.output) {
        want(
            &mut all,
            &code,
            CodeUse::Validator,
            at.field("output").field("validator"),
        );
    }
    for stage in &graph.stages {
        let sat = at.field("stages").key(stage.name.as_str());
        for (hook, code) in stage.hooks.iter() {
            want(
                &mut all,
                code,
                CodeUse::Hook,
                sat.field("hooks").field(hook),
            );
        }
        if let Some(code) = validator(&stage.output) {
            want(
                &mut all,
                &code,
                CodeUse::Validator,
                sat.field("output").field("validator"),
            );
        }
    }
    for (layout, path) in layouts(graph, at) {
        for (i, region) in layout.regions.iter().enumerate() {
            let rat = path.field("regions").index(i);
            if let RegionKind::Custom { code, .. } = &region.kind {
                want(&mut all, code, CodeUse::Region, rat.field("kind"));
            }
            if let Some(Seed::Code(code)) = &region.seed {
                want(&mut all, code, CodeUse::Seed, rat.field("seed"));
            }
        }
    }
    let checks = graph
        .mime_types
        .iter()
        .filter_map(|(pattern, row)| row.check.as_ref().map(|code| (pattern, code)));
    for (pattern, code) in checks {
        let path = at.field("mime_types").key(pattern.as_str()).field("check");
        want(&mut all, code, CodeUse::MimeCheck, path);
    }
    for (i, dependency) in graph.dependencies.iter().enumerate() {
        let dat = at.field("dependencies").index(i);
        if let Needs::Check(code) = &dependency.needs {
            want(&mut all, code, CodeUse::DependencyCheck, dat.field("needs"));
        }
        let script = dependency.install.as_ref().and_then(|i| i.script.as_ref());
        if let Some(code) = script {
            let path = dat.field("install").field("script");
            want(&mut all, code, CodeUse::Install, path);
        }
    }
    all
}

/// Read and check every piece of code the run names.
pub(super) async fn read_all(
    graph: &RunGraph,
    request: &SpawnRequest,
    src: &Source,
    env: &dyn ResolveEnv,
    issues: &mut SpawnIssues,
) -> Code {
    let mut code = Code::default();
    for wanted in collect(graph, request, &src.at) {
        if let (CodeRef::File(file), None) = (&wanted.code, &src.base) {
            for (_, at) in &wanted.uses {
                issues.push(
                    SpawnIssue::new(
                        at.clone(),
                        IssueCode::NotAllowed,
                        "a graph sent in a request cannot name code by file",
                    )
                    .got(format!("the file \"{file}\""))
                    .hint(
                        "put the code itself in the request as {\"inline\": \"...\"}; only an \
                         installed blueprint can name files that ship beside it",
                    ),
                );
            }
            continue;
        }
        let bytes = match env.code(&wanted.code, src.base.as_deref()).await {
            Ok(bytes) => bytes,
            Err(message) => {
                let at = wanted.uses[0].1.clone();
                issues.push(SpawnIssue::new(at, IssueCode::Unresolvable, message));
                continue;
            }
        };
        for (used_as, at) in &wanted.uses {
            if let Err(message) = env.check_code(&bytes, *used_as) {
                issues.push(
                    SpawnIssue::new(at.clone(), IssueCode::Invalid, message)
                        .hint("fix the code so it compiles and defines what this use calls"),
                );
            }
        }
        let digest = Digest::of(&bytes);
        code.refs.push((wanted.code, digest.clone()));
        code.files.insert(digest, bytes);
    }
    code
}

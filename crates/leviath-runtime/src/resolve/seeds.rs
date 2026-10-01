//! Step 10: the spawn-time seeds.
//!
//! A seed that only reads (files, a glob, literal text) runs in both modes.
//! A seed with side effects (a shell command, code, tool calls) runs only for
//! a real spawn, and only when nothing else about the spawn is wrong: a
//! refused spawn never runs a command on the way to being refused. Otherwise
//! it is checked to be allowed and well formed, and the region is recorded as
//! not judged.
//!
//! A seed that fails is an issue when its region is `required`, and a note on
//! the run otherwise, so an optional discovery nicety never sinks a run.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::ResolveMode;
use super::regions::layouts;
use crate::spec::env::{CodeFiles, ResolveEnv, SeedCx};
use crate::spec::graph::{CodeRef, RegionDef, RunGraph, Seed};
use crate::spec::inputs::InputValues;
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::launch::LaunchPolicy;
use crate::spec::names::{Digest, RegionName, RunId};
use crate::spec::run_spec::SeededContent;

/// Everything the seeds of one run see.
pub(super) struct SeedRun<'a> {
    pub(super) run_id: &'a RunId,
    pub(super) agent: &'a str,
    pub(super) graph: &'a RunGraph,
    pub(super) at: &'a SpecPath,
    pub(super) workdir: &'a Path,
    pub(super) launch: &'a LaunchPolicy,
    pub(super) code: &'a CodeFiles,
    pub(super) code_refs: &'a [(CodeRef, Digest)],
    pub(super) inputs: &'a InputValues,
    pub(super) mode: ResolveMode,
}

/// What the seeds produced, by region.
#[derive(Debug, Clone, Default)]
pub(super) struct Seeded {
    /// Each seeded region's content.
    pub(super) regions: BTreeMap<RegionName, SeededContent>,
    /// Regions the required-region check leaves alone: their seed was
    /// checked but not run, so what they would hold is not known, or it was
    /// refused or failed, which is already an issue of its own.
    pub(super) excused: BTreeSet<RegionName>,
}

/// Run (or check) every region's seed, in layout order. A region declared in
/// more than one layout is seeded once, from its first declaration.
pub(super) async fn run_all(
    run: SeedRun<'_>,
    env: &dyn ResolveEnv,
    issues: &mut SpawnIssues,
    notes: &mut Vec<String>,
) -> Seeded {
    let effects_allowed = run.mode == ResolveMode::Spawn && issues.is_empty();
    let mut seeded = Seeded::default();
    let mut seen = BTreeSet::new();
    for (layout, path) in layouts(run.graph, run.at) {
        for (i, region) in layout.regions.iter().enumerate() {
            let Some(seed) = &region.seed else { continue };
            if !seen.insert(region.name.clone()) {
                continue;
            }
            let at = path.field("regions").index(i).field("seed");
            if let Seed::Command(_) = seed
                && !run.launch.seed_commands
            {
                refuse_command(region, at, issues, notes);
                seeded.excused.insert(region.name.clone());
                continue;
            }
            let effects = matches!(seed, Seed::Command(_) | Seed::Code(_) | Seed::Tools { .. });
            if effects && !effects_allowed {
                check_only(seed, &at, issues);
                seeded.excused.insert(region.name.clone());
                continue;
            }
            let cx = SeedCx {
                run_id: run.run_id,
                agent: run.agent,
                graph: run.graph,
                launch: run.launch,
                workdir: run.workdir,
                commands_allowed: run.launch.seed_commands,
                code: run.code,
                code_refs: run.code_refs,
                inputs: run.inputs,
            };
            match env.seed(seed, cx).await {
                Ok(content) => {
                    seeded.regions.insert(region.name.clone(), content);
                }
                Err(message) if region.required => {
                    issues.push(
                        SpawnIssue::new(
                            at,
                            IssueCode::Unresolvable,
                            format!(
                                "the seed for required region \"{}\" failed: {message}",
                                region.name
                            ),
                        )
                        .hint("fix the seed, or fill the region from an input instead"),
                    );
                    seeded.excused.insert(region.name.clone());
                }
                Err(message) => notes.push(format!(
                    "region '{}': seed failed, left empty: {message}",
                    region.name
                )),
            }
        }
    }
    seeded
}

/// A command seed on a run that may not run commands: refused for a required
/// region, skipped with a note for any other.
fn refuse_command(
    region: &RegionDef,
    at: SpecPath,
    issues: &mut SpawnIssues,
    notes: &mut Vec<String>,
) {
    match region.required {
        true => issues.push(
            SpawnIssue::new(
                at,
                IssueCode::NotAllowed,
                format!(
                    "required region \"{}\" is seeded by a shell command, and command seeds are off for this run",
                    region.name
                ),
            )
            .hint(
                "the operator's `allow_seed_commands`, the request's `launch.seed_commands`, \
                 or a parent run turned them off",
            ),
        ),
        false => notes.push(format!(
            "region '{}': command seed skipped: command seeds are off for this run",
            region.name
        )),
    }
}

/// The checks a seed with side effects gets when it is not run: a command is
/// not blank, and each tool call's arguments are a JSON object, as a model
/// would write them. Code seeds are checked with the rest of the run's code.
fn check_only(seed: &Seed, at: &SpecPath, issues: &mut SpawnIssues) {
    match seed {
        Seed::Command(command) if command.trim().is_empty() => issues.push(SpawnIssue::new(
            at.clone(),
            IssueCode::Invalid,
            "a command seed needs a command",
        )),
        Seed::Tools { calls, .. } => {
            for (j, call) in calls.iter().enumerate() {
                if !call.args.value().is_object() {
                    issues.push(
                        SpawnIssue::new(
                            at.field("tools").field("calls").index(j).field("args"),
                            IssueCode::WrongType,
                            format!("the arguments for \"{}\" are not a JSON object", call.tool),
                        )
                        .expected("a JSON object")
                        .got(call.args.to_text()),
                    );
                }
            }
        }
        _ => {}
    }
}

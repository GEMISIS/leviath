//! Re-running a `refresh = "each_stage"` tool seed when a stage is entered.
//!
//! A seed normally resolves once, at spawn, in the daemon's spawner. That is
//! right for a region filled from files or a literal, and wrong for one filled
//! from a clock: a run that spends an hour in `gather` and then enters `analyze`
//! should date the second stage from when it started, not from when the run did.
//!
//! # Why this lives here and not in the spawner
//!
//! Only the runtime knows a stage was entered, and only the tool lane can run a
//! call without blocking the tick - an MCP tool may take seconds, and the tick
//! loop drives every other agent in the daemon. So the calls go out on the same
//! lane a model's tool calls use, and land on a later tick.
//!
//! # Holding the stage back
//!
//! Stage entry sets `ReadyToInfer`. A refreshed region has to be in place
//! *before* the stage's first request is built, or the stage would run its first
//! turn against the previous stage's values and the refresh would be pointless.
//! So [`start_stage_seeds`] takes `ReadyToInfer` away and [`apply_stage_seeds`]
//! puts it back - the same hold-and-release the interaction-point lane uses.
//!
//! A failed call leaves the region as it was rather than blanking it: the
//! previous value is merely stale, and stale beats absent.

use bevy_ecs::prelude::*;
use leviath_core::layout::{RegionSeed, SeedRefresh, SeedToolCall};

use crate::components::ContextWindow;
use crate::pipeline::{AgentBlueprint, ReadyToInfer, StageJustEntered, ToolServiceRes, ToolStage};

/// One dispatched stage-entry seed call, and where its answer belongs.
///
/// The lane answers with `(id, result)` pairs and has no notion of a region or
/// a heading, so both travel with the id rather than being recovered from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedCallSite {
    /// The tool-call id the lane will answer under.
    pub id: String,
    /// The region this answer fills.
    pub region: String,
    /// The tool called, which is also the heading its block carries.
    pub tool: String,
}

/// An agent whose stage-entry seed calls are out on the tool lane.
#[derive(Component, Debug, Clone)]
pub struct PendingStageSeeds {
    /// The dispatched calls, in order.
    pub sites: Vec<SeedCallSite>,
}

/// The regions of `blueprint` that re-seed on every stage entry, with their
/// calls.
///
/// Pure over the blueprint so the selection is testable without a world.
pub fn refreshing_regions(blueprint: &leviath_core::Blueprint) -> Vec<(&str, &[SeedToolCall])> {
    blueprint
        .context_layout
        .regions
        .iter()
        .filter_map(|r| match &r.seed {
            Some(RegionSeed::Tools {
                calls,
                refresh: SeedRefresh::EachStage,
            }) => Some((r.name.as_str(), calls.as_slice())),
            _ => None,
        })
        .collect()
}

/// The call sites for one stage entry, numbered in dispatch order.
///
/// Ids are prefixed so they cannot collide with a provider-minted one, and
/// numbered because one region can make several calls and the lane answers with
/// a flat list.
pub fn call_sites(regions: &[(&str, &[SeedToolCall])]) -> Vec<SeedCallSite> {
    let mut sites = Vec::new();
    for (region, calls) in regions {
        for call in *calls {
            sites.push(SeedCallSite {
                id: format!("stage-seed-{}", sites.len()),
                region: (*region).to_string(),
                tool: call.name.clone(),
            });
        }
    }
    sites
}

/// The agents [`start_stage_seeds`] considers: those that just entered a stage
/// and do not already have a batch out.
///
/// A named alias rather than the type inline, because it is long enough that
/// the workspace's `type_complexity` lint refuses it at a call site.
type StageSeedQuery<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static AgentBlueprint),
    (With<StageJustEntered>, Without<PendingStageSeeds>),
>;

/// Dispatch every refreshing region's calls for an agent that just entered a
/// stage, and hold the stage until they land.
///
/// Ordered with the other `StageJustEntered` systems, before `sync_tool_stages`
/// consumes that marker.
pub fn start_stage_seeds(
    agents: StageSeedQuery,
    service: Res<ToolServiceRes>,
    stage: Res<ToolStage>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, blueprint) in agents.iter() {
        crate::tick_scope::enter(entity);
        let regions = refreshing_regions(&blueprint.0);
        let sites = call_sites(&regions);
        if sites.is_empty() {
            continue;
        }
        let calls = regions
            .iter()
            .flat_map(|(_, calls)| calls.iter())
            .zip(sites.iter())
            .map(|(call, site)| leviath_providers::ToolCall {
                id: site.id.clone(),
                name: call.name.clone(),
                arguments: call.args.clone(),
                thought_signature: None,
            })
            .collect();
        let exec = service
            .0
            .exec_for(entity, calls, crate::pipeline::noop_progress());
        stage.stats.enqueued();
        // A failed send means the lane is gone, which only happens at shutdown.
        // The hold stays on: releasing it would start the stage against a region
        // that was never refreshed, and a shutting-down daemon has nowhere to
        // run the stage anyway.
        let _ = stage.jobs.send(crate::tool_bridge::ToolJob {
            entity,
            exec,
            cancel: crate::cancel::CancelToken::new(),
        });
        commands
            .entity(entity)
            .remove::<ReadyToInfer>()
            .insert(PendingStageSeeds { sites });
    }
}

/// Group one landed batch into `(region, content)` pairs.
///
/// Pure over the sites and the results, so the grouping is testable without a
/// lane. A region whose every call failed is absent from the result, which is
/// what leaves its previous content in place.
pub fn seeded_content(
    sites: &[SeedCallSite],
    results: &[(String, String)],
) -> Vec<(String, String)> {
    let mut per_region: Vec<(String, Vec<String>)> = Vec::new();
    for site in sites {
        let Some((_, text)) = results.iter().find(|(id, _)| *id == site.id) else {
            continue;
        };
        // The tool layer reports refusal and failure in-band and prefixed - the
        // same contract the spawn-time seed reads. Neither is data, so neither
        // gets a heading claiming the tool answered.
        if text.trim().is_empty() || text.starts_with("[error]") || text.starts_with("[denied]") {
            continue;
        }
        let block = format!("--- {} ---\n{}", site.tool, text.trim_end());
        match per_region.iter_mut().find(|(r, _)| *r == site.region) {
            Some((_, blocks)) => blocks.push(block),
            None => per_region.push((site.region.clone(), vec![block])),
        }
    }
    per_region
        .into_iter()
        .map(|(region, blocks)| (region, blocks.join("\n\n")))
        .collect()
}

/// Apply a landed stage-entry seed batch and release the stage.
pub fn apply_stage_seeds(
    entity: Entity,
    pending: &PendingStageSeeds,
    results: &[(String, String)],
    window: &mut ContextWindow,
    commands: &mut Commands,
) {
    for (region, content) in seeded_content(&pending.sites, results) {
        let tokens = leviath_core::estimate_tokens(&content);
        window.replace_region(&region, content, tokens);
    }
    commands
        .entity(entity)
        .remove::<PendingStageSeeds>()
        .insert(ReadyToInfer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::layout::{ContextLayout, RegionDefinition};
    use leviath_core::{RegionKind, Stage};

    fn blueprint_with(seeds: Vec<(&str, Option<RegionSeed>)>) -> leviath_core::Blueprint {
        let regions = seeds
            .into_iter()
            .map(|(name, seed)| {
                let mut r = RegionDefinition::new(name.to_string(), RegionKind::Pinned, 1000);
                r.seed = seed;
                r
            })
            .collect();
        let layout = ContextLayout::new(regions, 10_000);
        let stages = vec![Stage::new(
            "main".to_string(),
            leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
        )];
        leviath_core::Blueprint::new("t".to_string(), "d".to_string(), stages, layout)
    }

    fn tools(names: &[&str], refresh: SeedRefresh) -> RegionSeed {
        RegionSeed::Tools {
            calls: names.iter().map(|n| SeedToolCall::new(*n)).collect(),
            refresh,
        }
    }

    /// Only `each_stage` seeds re-run. A `once` tool seed, and every other seed
    /// kind, resolved at spawn and must not be called again on every entry -
    /// that would be a tool call per stage for the life of the run.
    #[test]
    fn only_each_stage_tool_seeds_are_refreshed() {
        let bp = blueprint_with(vec![
            (
                "clock",
                Some(tools(&["current_time"], SeedRefresh::EachStage)),
            ),
            ("machine", Some(tools(&["system_info"], SeedRefresh::Once))),
            (
                "readme",
                Some(RegionSeed::Files {
                    paths: vec!["README.md".to_string()],
                }),
            ),
            ("notes", None),
        ]);
        let picked = refreshing_regions(&bp);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].0, "clock");
        assert_eq!(picked[0].1.len(), 1);
        assert_eq!(picked[0].1[0].name, "current_time");
    }

    #[test]
    fn a_blueprint_with_no_refreshing_region_dispatches_nothing() {
        let bp = blueprint_with(vec![(
            "machine",
            Some(tools(&["system_info"], SeedRefresh::Once)),
        )]);
        assert!(refreshing_regions(&bp).is_empty());
        assert!(call_sites(&refreshing_regions(&bp)).is_empty());
    }

    /// Ids are unique across the whole batch, not per region: the lane answers
    /// with one flat list, and two regions calling the same tool would otherwise
    /// collide and file both answers under whichever matched first.
    #[test]
    fn call_sites_are_numbered_across_the_whole_batch() {
        let bp = blueprint_with(vec![
            (
                "a",
                Some(tools(
                    &["current_time", "system_info"],
                    SeedRefresh::EachStage,
                )),
            ),
            ("b", Some(tools(&["current_time"], SeedRefresh::EachStage))),
        ]);
        let sites = call_sites(&refreshing_regions(&bp));
        assert_eq!(
            sites,
            vec![
                SeedCallSite {
                    id: "stage-seed-0".into(),
                    region: "a".into(),
                    tool: "current_time".into()
                },
                SeedCallSite {
                    id: "stage-seed-1".into(),
                    region: "a".into(),
                    tool: "system_info".into()
                },
                SeedCallSite {
                    id: "stage-seed-2".into(),
                    region: "b".into(),
                    tool: "current_time".into()
                },
            ]
        );
        // The ids really are distinct, which is the property the grouping needs.
        let unique: std::collections::HashSet<&String> = sites.iter().map(|s| &s.id).collect();
        assert_eq!(unique.len(), sites.len());
    }

    fn sites_for(pairs: &[(&str, &str, &str)]) -> Vec<SeedCallSite> {
        pairs
            .iter()
            .map(|(id, region, tool)| SeedCallSite {
                id: (*id).to_string(),
                region: (*region).to_string(),
                tool: (*tool).to_string(),
            })
            .collect()
    }

    #[test]
    fn results_are_grouped_per_region_under_their_tool_headings() {
        let sites = sites_for(&[
            ("s0", "env", "current_time"),
            ("s1", "env", "locale_info"),
            ("s2", "tools", "which_command"),
        ]);
        let results = vec![
            ("s0".to_string(), "{\"date\":\"2026-08-18\"}".to_string()),
            ("s1".to_string(), "{\"locale\":\"en-US\"}".to_string()),
            ("s2".to_string(), "{\"found\":true}".to_string()),
        ];
        let grouped = seeded_content(&sites, &results);
        assert_eq!(
            grouped,
            vec![
                (
                    "env".to_string(),
                    "--- current_time ---\n{\"date\":\"2026-08-18\"}\n\n\
                     --- locale_info ---\n{\"locale\":\"en-US\"}"
                        .to_string()
                ),
                (
                    "tools".to_string(),
                    "--- which_command ---\n{\"found\":true}".to_string()
                ),
            ]
        );
    }

    /// A refused or failed call is not data. The region keeps whatever it had,
    /// which is stale rather than wrong - and a heading over `[error] ...` would
    /// read to the model as the tool having answered that.
    #[test]
    fn refused_failed_and_empty_answers_contribute_nothing() {
        let sites = sites_for(&[
            ("s0", "env", "current_time"),
            ("s1", "env", "system_info"),
            ("s2", "env", "locale_info"),
            ("s3", "env", "which_command"),
        ]);
        let results = vec![
            ("s0".to_string(), "[error] tool error: nope".to_string()),
            ("s1".to_string(), "[denied] not allowed".to_string()),
            ("s2".to_string(), "   \n".to_string()),
            ("s3".to_string(), "found".to_string()),
        ];
        let grouped = seeded_content(&sites, &results);
        assert_eq!(
            grouped,
            vec![(
                "env".to_string(),
                "--- which_command ---\nfound".to_string()
            )]
        );
    }

    /// Every call failing leaves the region out of the result entirely, which is
    /// what stops `apply_stage_seeds` from replacing good content with nothing.
    #[test]
    fn a_region_whose_every_call_failed_is_left_alone() {
        let sites = sites_for(&[("s0", "env", "current_time")]);
        let results = vec![("s0".to_string(), "[error] nope".to_string())];
        assert!(seeded_content(&sites, &results).is_empty());
    }

    /// A call the lane never answered - cancelled mid-batch - is skipped rather
    /// than filed as an empty block.
    #[test]
    fn a_call_with_no_answer_is_skipped() {
        let sites = sites_for(&[("s0", "env", "current_time"), ("s1", "env", "locale_info")]);
        let results = vec![("s1".to_string(), "en-US".to_string())];
        assert_eq!(
            seeded_content(&sites, &results),
            vec![("env".to_string(), "--- locale_info ---\nen-US".to_string())]
        );
    }
}

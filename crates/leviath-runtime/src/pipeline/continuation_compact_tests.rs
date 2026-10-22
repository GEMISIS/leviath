//! A continuation's summaries are not best effort: they wait for a free
//! slot, and a region whose summary cannot be written is emptied rather
//! than carried into the new segment as it was.

use super::*;
use crate::continuation::ContinuationCompact;

fn asked(regions: &[&str], started: bool) -> ContinuationCompact {
    ContinuationCompact {
        regions: regions.iter().map(ToString::to_string).collect(),
        started,
    }
}

fn tokens(world: &World, e: Entity, region: &str) -> usize {
    world
        .get::<ContextWindow>(e)
        .unwrap()
        .get_region(region)
        .unwrap()
        .current_tokens
}

/// A continuation's summary starts like an edge's, marked as started.
#[tokio::test]
async fn a_continuation_summary_starts_and_says_so() {
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let e = world
        .spawn((
            scratch_window(),
            PendingEdgeCompact(vec!["scratch".to_string()]),
            asked(&["scratch"], false),
            compaction_settings("cfg", "m"),
            agent_state(),
            ReadyToInfer,
        ))
        .id();
    run_dispatch_edge_compact(&mut world);
    assert!(world.get::<AwaitingCompaction>(e).is_some());
    assert_eq!(
        world.get::<ContinuationCompact>(e),
        Some(&asked(&["scratch"], true))
    );
}

/// With every slot taken, the summary is asked for again on a later tick
/// rather than dropped, and the region is left for it.
#[tokio::test]
async fn a_continuation_summary_waits_for_a_free_slot() {
    let mut cfg = InferencePoolConfig::new();
    cfg.set_limit("m", 0);
    let (mut world, _rx) = build_world(InferencePools::new(cfg));
    let e = world
        .spawn((
            scratch_window(),
            PendingEdgeCompact(vec!["scratch".to_string()]),
            asked(&["scratch"], false),
            compaction_settings("cfg", "m"),
            agent_state(),
            ReadyToInfer,
        ))
        .id();
    run_dispatch_edge_compact(&mut world);
    assert!(world.get::<PendingEdgeCompact>(e).is_some());
    assert!(world.get::<ContinuationCompact>(e).is_some());
    assert!(world.get::<AwaitingCompaction>(e).is_none());
    assert!(tokens(&world, e, "scratch") > 0);
}

/// With no route to summarize on, or no compaction at all, the region is
/// emptied and the run goes on.
#[tokio::test]
async fn a_continuation_summary_with_no_route_empties_the_region() {
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let ghost = world
        .spawn((
            scratch_window(),
            PendingEdgeCompact(vec!["scratch".to_string()]),
            asked(&["scratch"], false),
            compaction_settings("ghost", "m"),
            agent_state(),
            ReadyToInfer,
        ))
        .id();
    let unset = world
        .spawn((
            scratch_window(),
            PendingEdgeCompact(vec!["scratch".to_string()]),
            asked(&["scratch"], false),
            agent_state(),
            ReadyToInfer,
        ))
        .id();
    run_dispatch_edge_compact(&mut world);
    for e in [ghost, unset] {
        assert_eq!(tokens(&world, e, "scratch"), 0);
        assert!(world.get::<PendingEdgeCompact>(e).is_none());
        assert!(world.get::<ContinuationCompact>(e).is_none());
        assert!(world.get::<ReadyToInfer>(e).is_some());
    }
}

fn outcome(e: Entity, result: Result<Vec<(String, String)>, String>) -> CompactionOutcome {
    CompactionOutcome {
        entity: e,
        usage: Vec::new(),
        provider_name: "p".to_string(),
        model: "m".to_string(),
        result: result.map_err(leviath_providers::ProviderError::Other),
        pricing: None,
    }
}

/// A summary that fails or comes back blank empties the region it was for;
/// one that lands replaces it as any summary does.
#[test]
fn a_continuation_region_with_no_summary_to_show_is_emptied() {
    let (mut world, tx) = world_with_compaction_results();
    let failed = world
        .spawn((
            compacting_window(),
            AwaitingCompaction,
            asked(&["conv"], true),
        ))
        .id();
    let blank = world
        .spawn((
            compacting_window(),
            AwaitingCompaction,
            asked(&["conv"], true),
        ))
        .id();
    let landed = world
        .spawn((
            compacting_window(),
            AwaitingCompaction,
            asked(&["conv"], true),
        ))
        .id();
    tx.send(outcome(failed, Err("boom".to_string()))).unwrap();
    tx.send(outcome(blank, Ok(vec![("conv".into(), " ".into())])))
        .unwrap();
    tx.send(outcome(
        landed,
        Ok(vec![("conv".into(), "the summary".into())]),
    ))
    .unwrap();
    run_collect_compaction(&mut world);
    for e in [failed, blank, landed] {
        assert_eq!(tokens(&world, e, "conv"), 0);
        assert!(world.get::<ContinuationCompact>(e).is_none());
        assert!(world.get::<ReadyToInfer>(e).is_some());
    }
    assert_eq!(tokens(&world, failed, "history"), 0);
    assert!(tokens(&world, landed, "history") > 0);
}

/// Another compaction landing before the continuation's has started leaves
/// the continuation's ask where it is.
#[test]
fn another_compaction_leaves_a_continuation_waiting_alone() {
    let (mut world, tx) = world_with_compaction_results();
    let e = world
        .spawn((
            compacting_window(),
            AwaitingCompaction,
            asked(&["conv"], false),
        ))
        .id();
    let before = tokens(&world, e, "conv");
    tx.send(outcome(e, Err("boom".to_string()))).unwrap();
    run_collect_compaction(&mut world);
    assert_eq!(tokens(&world, e, "conv"), before);
    assert!(world.get::<ContinuationCompact>(e).is_some());
}

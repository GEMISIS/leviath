//! A run's path as a graph of its own: one node per stage *visit*, in the
//! order the run walked them.
//!
//! The blueprint says what a run *could* do. Painting a run onto it answers
//! that question, not "what did this run do": the stages it visited keep
//! their blueprint layer and slot, so the path reads as a sparse slice of a
//! big picture, and three passes through `implement` collapse into one box
//! with a `×3` badge, losing the order they happened in.
//!
//! So the path gets its own graph. It unrolls the loops: a stage entered
//! three times is three nodes, `implement`, `implement (2)`, `implement (3)`,
//! chained in visit order with nothing to branch to. Laid out by
//! [`super::layout::snake`] it stays compact and grows a row at a time while
//! the run is still going. The Lair's run view draws the same picture, from
//! the same data, for the same reasons.
//!
//! Free of ratatui and of the dashboard's own types: visits come in as
//! [`Visit`], which the caller maps from whatever it reads off disk.

use leviath_core::TransitionCondition;

use super::content::RunPhase;
use super::model::{EdgeClass, NodeKind, StageEdge, StageGraph, StageKind, StageNode};
use super::view::{LiveOverlay, StageLive};

/// One stay in a stage, as the path cares about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Visit {
    pub(crate) stage: String,
    /// `HH:MM:SS` it was entered, for the box's badge row.
    pub(crate) at: Option<String>,
    /// Iterations taken during the visit.
    pub(crate) iterations: usize,
}

/// The visits a path draws: what the archive recorded, plus the stage the run
/// is in now when the archive has not caught up with it.
///
/// A run that has just started has written no context yet, so without the
/// second half its path would be empty until the first checkpoint lands - a
/// blank band for the first few seconds of every run.
pub(crate) fn path_visits(visits: &[Visit], current: &str) -> Vec<Visit> {
    let mut out = visits.to_vec();
    let already = out.last().is_some_and(|v| v.stage == current);
    if !current.is_empty() && !already {
        out.push(Visit {
            stage: current.to_string(),
            at: None,
            iterations: 0,
        });
    }
    out
}

/// The node id of each visit: the stage name for the first pass through it,
/// then `name (2)`, `name (3)`, ... . The id is what the box shows, the way
/// the Lair labels its own path nodes.
fn visit_ids(visits: &[Visit]) -> Vec<String> {
    let mut seen: Vec<(&str, usize)> = Vec::new();
    visits
        .iter()
        .map(|visit| {
            let pass = match seen.iter_mut().find(|(name, _)| *name == visit.stage) {
                Some((_, count)) => {
                    *count += 1;
                    *count
                }
                None => {
                    seen.push((visit.stage.as_str(), 1));
                    1
                }
            };
            if pass == 1 {
                visit.stage.clone()
            } else {
                format!("{} ({pass})", visit.stage)
            }
        })
        .collect()
}

/// The stage name a path node id was made from, undoing [`visit_ids`].
///
/// The ordinal is only stripped when what is left is a stage `blueprint`
/// knows, so a blueprint that really does have a stage called `review (2)`
/// still resolves to itself.
pub(crate) fn stage_of(id: &str, blueprint: Option<&StageGraph>) -> String {
    if blueprint.is_some_and(|g| g.node(id).is_some()) {
        return id.to_string();
    }
    id.rsplit_once(" (")
        .and_then(|(head, tail)| {
            let digits = tail.strip_suffix(')')?;
            (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(head)
        })
        .unwrap_or(id)
        .to_string()
}

/// The path `visits` walked, as a graph: a chain of one node per visit.
///
/// `blueprint` lends each node what the stage is (its mode, its description,
/// its iteration ceiling); a run whose manifest could not be read still gets
/// its path, drawn as plain autonomous stages.
pub(crate) fn run_path(blueprint: Option<&StageGraph>, visits: &[Visit]) -> StageGraph {
    let ids = visit_ids(visits);
    let nodes: Vec<StageNode> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            let stage = blueprint.and_then(|g| g.node(&visits[index].stage));
            StageNode {
                id: id.clone(),
                kind: stage
                    .map(|s| s.kind.clone())
                    .unwrap_or(NodeKind::Stage(StageKind::Autonomous)),
                is_entry: index == 0,
                // Whether a stage may end the run is the blueprint's to say,
                // not the path's. Marking the last box terminal because it is
                // last would badge the stage a running run happens to be in
                // with "⏹ can end" whether or not it can; where the path
                // stops is already said by the box the run is in.
                is_terminal: stage.is_some_and(|s| s.is_terminal),
                allow_complete: stage.is_some_and(|s| s.allow_complete),
                // The path has already unrolled every loop; a box that
                // pointed at itself would be claiming a revisit that is
                // drawn as the next box along.
                self_loop: false,
                max_iterations: stage.and_then(|s| s.max_iterations),
                max_revisits: None,
                description: stage.and_then(|s| s.description.clone()),
            }
        })
        .collect();
    let edges: Vec<StageEdge> = ids
        .windows(2)
        .map(|pair| StageEdge {
            from: pair[0].clone(),
            to: pair[1].clone(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: "direct",
            class: EdgeClass::Primary,
            back_edge: false,
        })
        .collect();
    StageGraph {
        entry: ids.first().cloned().unwrap_or_default(),
        nodes,
        edges,
        // Nothing on a path branches: it is what happened, not what could.
        is_branching: false,
    }
}

/// Paint the run onto its own path: every node visited, every edge taken,
/// the last one the stage the run is in.
///
/// `errored` names the stages whose ledger says they ended in an error; the
/// mark lands on the *last* pass through such a stage, since an earlier pass
/// that was followed by another one plainly did not end the run.
pub(crate) fn path_overlay(
    visits: &[Visit],
    errored: &[String],
    run: RunPhase,
    tick: u64,
) -> LiveOverlay {
    let ids = visit_ids(visits);
    let stages: Vec<StageLive> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            let stage = &visits[index].stage;
            let last_pass = !visits[index + 1..].iter().any(|v| &v.stage == stage);
            StageLive {
                name: id.clone(),
                entered: true,
                errored: last_pass && errored.iter().any(|e| e == stage),
                // One box is one visit, so the `×n` badge has nothing to
                // count - the `(n)` in the name already says which pass this
                // is.
                visits: 1,
                last_seen: visits[index].at.clone(),
                iterations: Some(visits[index].iterations),
            }
        })
        .collect();
    let taken: Vec<(String, String)> = ids
        .windows(2)
        .map(|pair| (pair[0].clone(), pair[1].clone()))
        .collect();
    LiveOverlay {
        current: ids.last().cloned(),
        run: Some(run),
        iteration: visits.last().map(|v| v.iterations).unwrap_or(0),
        stages,
        workers: None,
        last_transition: taken.last().cloned(),
        taken,
        tick,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::manifest::parse_manifest;

    fn blueprint() -> StageGraph {
        StageGraph::from_blueprint(
            &parse_manifest(
                r#"
[agent]
name = "grapher"
[stages.plan]
description = "think first"
max_iterations = 4
[stages.plan.transitions.implement]
[stages.implement]
mode = "interactive"
[stages.implement.transitions.review]
[stages.review]
[stages.review.transitions.implement]
condition = "llm_choice"
[stages.review.transitions.done]
[stages.done]
[stages.done.transitions]
"#,
            )
            .unwrap(),
        )
    }

    fn visits(stages: &[&str]) -> Vec<Visit> {
        stages
            .iter()
            .enumerate()
            .map(|(i, stage)| Visit {
                stage: (*stage).to_string(),
                at: Some(format!("10:00:0{i}")),
                iterations: i,
            })
            .collect()
    }

    fn ids(graph: &StageGraph) -> Vec<&str> {
        graph.ids().collect()
    }

    #[test]
    fn every_visit_is_its_own_box_and_later_passes_are_numbered() {
        let v = visits(&["plan", "implement", "review", "implement", "review"]);
        let graph = run_path(Some(&blueprint()), &v);
        assert_eq!(
            ids(&graph),
            ["plan", "implement", "review", "implement (2)", "review (2)"]
        );
        // A chain: each box leads to the next and nothing else.
        assert_eq!(graph.edges.len(), 4);
        assert_eq!(graph.edges[3].from, "implement (2)");
        assert_eq!(graph.edges[3].to, "review (2)");
        assert!(graph.edges.iter().all(|e| e.class == EdgeClass::Primary));
        assert!(graph.edges.iter().all(|e| !e.back_edge));
        assert!(!graph.is_branching);
        assert_eq!(graph.entry, "plan");
    }

    #[test]
    fn the_path_starts_where_it_started_and_only_ends_where_it_ended() {
        let v = visits(&["plan", "implement", "review", "implement"]);
        let graph = run_path(Some(&blueprint()), &v);
        let node = |id: &str| graph.node(id).expect("on the path");
        assert!(node("plan").is_entry);
        assert!(!node("implement").is_entry);
        // Whether a stage can end the run is the blueprint's answer, the
        // same for every pass through it - not "this box is last".
        assert!(!node("implement").is_terminal);
        assert!(!node("implement (2)").is_terminal);
        let ends = run_path(Some(&blueprint()), &visits(&["review", "done"]));
        assert!(ends.node("done").expect("on the path").is_terminal);
        // The loop is already unrolled, so no box points at itself.
        assert!(graph.nodes.iter().all(|n| !n.self_loop));
        // What the stage is comes from the blueprint, for every pass.
        assert_eq!(node("plan").description.as_deref(), Some("think first"));
        assert_eq!(node("plan").max_iterations, Some(4));
        assert_eq!(node("implement (2)").kind_label(), "interactive");
    }

    #[test]
    fn a_path_without_a_blueprint_is_still_a_path() {
        let graph = run_path(None, &visits(&["plan", "plan"]));
        assert_eq!(ids(&graph), ["plan", "plan (2)"]);
        assert_eq!(graph.node("plan").unwrap().kind_label(), "autonomous");
        assert_eq!(graph.node("plan").unwrap().description, None);
        // Nothing at all: an empty graph, not a panic.
        let empty = run_path(None, &[]);
        assert!(empty.nodes.is_empty());
        assert!(empty.edges.is_empty());
        assert_eq!(empty.entry, "");
    }

    #[test]
    fn the_stage_the_run_is_in_is_appended_when_the_archive_lags() {
        // A run that has just moved on: the archive has not caught up, so
        // the box for where it is now would otherwise be missing.
        let v = path_visits(&visits(&["plan", "implement"]), "review");
        assert_eq!(v.len(), 3);
        assert_eq!(v[2].stage, "review");
        assert_eq!(v[2].at, None);
        // Already the last visit: nothing to add.
        let v = path_visits(&visits(&["plan", "implement"]), "implement");
        assert_eq!(v.len(), 2);
        // A run that has written nothing yet is still one box.
        assert_eq!(path_visits(&[], "plan").len(), 1);
        // And a run that is nowhere at all is none.
        assert!(path_visits(&[], "").is_empty());
    }

    #[test]
    fn stage_of_undoes_the_ordinal_unless_the_blueprint_really_uses_one() {
        assert_eq!(stage_of("review", None), "review");
        assert_eq!(stage_of("review (2)", None), "review");
        assert_eq!(stage_of("review (12)", None), "review");
        // Only a trailing ` (<digits>)` is an ordinal.
        assert_eq!(stage_of("review (draft)", None), "review (draft)");
        assert_eq!(stage_of("review ()", None), "review ()");
        assert_eq!(stage_of("review (2", None), "review (2");
        // A blueprint that really has a stage by that name wins.
        let odd = run_path(None, &visits(&["review (2)"]));
        assert_eq!(stage_of("review (2)", Some(&odd)), "review (2)");
        assert_eq!(stage_of("review (2)", Some(&blueprint())), "review");
    }

    #[test]
    fn the_overlay_paints_every_box_visited_and_the_last_one_current() {
        let v = visits(&["plan", "implement", "review", "implement"]);
        let live = path_overlay(&v, &[], RunPhase::Active, 7);
        assert_eq!(live.current.as_deref(), Some("implement (2)"));
        assert_eq!(live.run, Some(RunPhase::Active));
        assert_eq!(live.tick, 7);
        assert_eq!(live.iteration, 3, "the last visit's own count");
        assert!(live.stages.iter().all(|s| s.entered));
        // One box is one visit, so nothing carries a `×n` badge, and each
        // says how long it took on its own.
        assert!(live.stages.iter().all(|s| s.visits == 1));
        assert_eq!(live.stages[1].iterations, Some(1));
        assert_eq!(live.stages[1].last_seen.as_deref(), Some("10:00:01"));
        // Every hop was taken, and the newest one animates.
        assert_eq!(
            live.taken,
            vec![
                ("plan".to_string(), "implement".to_string()),
                ("implement".to_string(), "review".to_string()),
                ("review".to_string(), "implement (2)".to_string()),
            ]
        );
        assert_eq!(
            live.last_transition,
            Some(("review".to_string(), "implement (2)".to_string()))
        );
        assert_eq!(live.workers, None);
        // An empty path has nothing to be current.
        assert_eq!(path_overlay(&[], &[], RunPhase::Idle, 0).current, None);
        assert_eq!(path_overlay(&[], &[], RunPhase::Idle, 0).iteration, 0);
    }

    #[test]
    fn an_error_marks_the_last_pass_through_the_stage_not_an_earlier_one() {
        let v = visits(&["implement", "review", "implement"]);
        let live = path_overlay(&v, &["implement".to_string()], RunPhase::Error, 0);
        let errored = |name: &str| {
            live.stages
                .iter()
                .find(|s| s.name == name)
                .expect("on the path")
                .errored
        };
        // The first pass was followed by another one, so it plainly did not
        // end the run.
        assert!(!errored("implement"));
        assert!(errored("implement (2)"));
        assert!(!errored("review"));
    }
}

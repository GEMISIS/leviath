use super::super::tests::{edge, minimal, stage};
use super::super::*;
use crate::spec::issues::SpecPath;
use crate::spec::names::StageName;

fn warned(graph: &RunGraph) -> Vec<String> {
    graph
        .warnings(&SpecPath::root().field("graph"))
        .iter()
        .map(ToString::to_string)
        .collect()
}

/// Two stages that only hand the run to each other: the scenario 0.6.4
/// refused with "no terminal path exists from entry stage". The run is
/// allowed to start, and the warning names both stages.
#[test]
fn a_loop_with_no_way_out_is_named_stage_by_stage() {
    let mut g = minimal();
    g.edges.push(edge("back", "build", "plan"));
    assert_eq!(
        warned(&g),
        vec![
            "graph.edges: may never finish: this run can never finish: no stage it can reach \
             ends the run, so it loops through 'plan', 'build' until it is stopped. give one of \
             these stages a way out: a stage with no `always` or `llm_choice` edge leaving it, \
             or allow_complete = true. Known: plan, build"
                .to_string()
        ]
    );
    let issues = g.warnings(&SpecPath::root());
    assert_eq!(issues.0[0].known, vec!["plan", "build"]);
    // A graph with warnings is still a valid graph.
    assert!(g.validate(&SpecPath::root()).is_ok());
    // allow_complete is a way out, and so is a stage only an error edge
    // leaves.
    g.stages[1].allow_complete = true;
    assert_eq!(warned(&g), Vec::<String>::new());
    g.stages[1].allow_complete = false;
    g.edges[1].when = EdgeCondition::Error;
    assert_eq!(warned(&g), Vec::<String>::new());
}

/// A run that can end, but has a corner it never leaves once it goes in.
#[test]
fn a_trap_beside_a_way_out_names_only_the_trap() {
    let mut g = minimal();
    g.stages.push(stage("fix"));
    g.stages.push(stage("done"));
    g.edges = vec![
        edge("build", "plan", "build"),
        edge("done", "plan", "done"),
        edge("fix", "build", "fix"),
        edge("build", "fix", "build"),
    ];
    let got = warned(&g);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].contains("this run may never finish: once it enters 'build', 'fix', no path"),
        "{got:?}"
    );
}

/// A fan-out with a merge stage ends where its merge stage does, and a
/// stage no run can reach is no trap.
#[test]
fn a_fan_out_ends_through_its_merge_stage() {
    let mut g = minimal();
    let mut fan = FanOutDef::same_graph(StageName::new("build").unwrap());
    fan.merge_stage = Some(StageName::new("merge").unwrap());
    g.stages[0].mode = StageMode::FanOut(fan);
    g.edges.clear();
    g.stages.push(stage("merge"));
    g.stages.push(stage("island"));
    g.edges.push(edge("back", "island", "island"));
    g.stages[3].max_revisits = Some(2);
    assert_eq!(warned(&g), Vec::<String>::new());
    // A merge stage that only goes back to the split is a loop.
    g.edges.push(edge("again", "merge", "plan"));
    let got = warned(&g);
    assert!(got[0].contains("'plan', 'merge'"), "{got:?}");
}

/// A stage that loops on itself with no cap is warned about whether or not
/// the run can also end: nothing bounds that loop. One warning per stage,
/// however many edges it has back to itself.
#[test]
fn a_self_loop_without_max_revisits_is_warned_about_once() {
    let mut g = minimal();
    g.edges.push(edge("again", "plan", "plan"));
    g.edges.push(edge("retry", "plan", "plan"));
    assert_eq!(
        warned(&g),
        vec![
            "graph.stages.plan: may never finish: stage 'plan' has an edge back to itself and \
             no max_revisits, so nothing stops it looping. set max_revisits on the stage so the \
             loop has to end"
                .to_string()
        ]
    );
    g.stages[0].max_revisits = Some(3);
    assert_eq!(warned(&g), Vec::<String>::new());
}

/// Names that point nowhere are the validator's to refuse; the warnings
/// pass over them rather than guess.
#[test]
fn dangling_names_and_empty_graphs_warn_of_nothing() {
    let mut g = minimal();
    g.edges.push(edge("ghost", "nowhere", "nowhere"));
    g.edges.push(edge("gone", "plan", "nowhere"));
    assert_eq!(warned(&g), Vec::<String>::new());
    g.stages.clear();
    assert_eq!(warned(&g), Vec::<String>::new());
}

use super::*;
use crate::spec::graph::{RegionKind, Seed};
use crate::spec::launch::{LaunchPolicy, Unattended};
use crate::spec::names::{ProfileName, StageName, ToolName};

fn parent_policy(depth: u8) -> LaunchPolicy {
    LaunchPolicy {
        unattended: Unattended::Profile(n::<ProfileName>("safe")),
        allow: vec![n::<ToolName>("bash")],
        max_depth: depth,
        seed_commands: true,
        capture_model_input: false,
    }
}

fn child(depth: u8) -> Caller {
    Caller::Child {
        parent: n("parent-1"),
        policy: parent_policy(depth),
        depth: 1,
    }
}

#[tokio::test]
async fn a_top_level_run_takes_the_graphs_depth_when_it_names_none() {
    let mut g = graph();
    g.max_child_depth = Some(5);
    let resolved = spawn(&raw(g.clone()), &Fake::default()).await.unwrap();
    assert_eq!(resolved.spec.launch.max_depth, 5);
    let mut request = raw(g);
    request.launch.max_depth = Some(1);
    let resolved = spawn(&request, &Fake::default()).await.unwrap();
    assert_eq!(resolved.spec.launch.max_depth, 1);
}

#[tokio::test]
async fn a_child_is_narrowed_by_its_parent() {
    let mut request = raw(graph());
    request.launch.unattended = Unattended::All;
    request.launch.allow = vec![n("bash"), n("write_file")];
    request.launch.max_depth = Some(9);
    let resolved = resolve(&request, &child(2), &Fake::default(), ResolveMode::Spawn)
        .await
        .unwrap();
    let spec = resolved.spec;
    assert_eq!(spec.launch.unattended, Unattended::Profile(n("safe")));
    assert_eq!(spec.launch.allow, vec![n::<ToolName>("bash")]);
    assert_eq!(spec.launch.max_depth, 1);
    assert_eq!(spec.placement.parent, Some(n("parent-1")));
    assert_eq!(spec.placement.depth, 2);
    assert_eq!(spec.placement.worker_stage, None);
}

#[tokio::test]
async fn a_child_takes_the_graphs_depth_under_its_parents() {
    let mut g = graph();
    g.max_child_depth = Some(0);
    let resolved = resolve(&raw(g), &child(3), &Fake::default(), ResolveMode::Spawn)
        .await
        .unwrap();
    assert_eq!(resolved.spec.launch.max_depth, 0);
}

#[tokio::test]
async fn a_parent_with_no_depth_left_cannot_start_a_child() {
    let issues = resolve(
        &raw(graph()),
        &child(0),
        &Fake::default(),
        ResolveMode::Spawn,
    )
    .await
    .unwrap_err();
    assert_eq!(found(&issues), ["launch.max_depth NotAllowed"]);
    assert!(
        issues.0[0].message.contains("parent-1"),
        "{}",
        issues.0[0].message
    );
}

#[tokio::test]
async fn the_operator_can_turn_seed_commands_off_for_children_too() {
    let env = Fake {
        limits: SpawnLimits {
            seed_commands_allowed: false,
            ..Fake::default().limits
        },
        ..Fake::default()
    };
    let resolved = resolve(&raw(graph()), &child(2), &env, ResolveMode::Spawn)
        .await
        .unwrap();
    assert!(!resolved.spec.launch.seed_commands);
}

fn worker(stage: Option<&str>) -> Caller {
    Caller::Worker {
        parent: n("parent-1"),
        policy: parent_policy(2),
        depth: 0,
        stage: stage.map(n::<StageName>),
    }
}

#[tokio::test]
async fn a_worker_runs_a_stage_of_the_graph() {
    let resolved = resolve(
        &raw(graph()),
        &worker(Some("build")),
        &Fake::default(),
        ResolveMode::Spawn,
    )
    .await
    .unwrap();
    assert_eq!(resolved.spec.placement.worker_stage, Some(n("build")));
    assert_eq!(resolved.spec.placement.depth, 1);

    let issues = resolve(
        &raw(graph()),
        &worker(Some("gone")),
        &Fake::default(),
        ResolveMode::Spawn,
    )
    .await
    .unwrap_err();
    assert_eq!(found(&issues), ["(request) Dangling"]);
    assert_eq!(issues.0[0].known, ["plan", "build"]);
}

/// A worker's work item arrives in its task, and the regions a caller must
/// fill were filled by the caller of its parent.
#[tokio::test]
async fn a_worker_is_not_held_to_required_regions_or_a_task() {
    let mut g = graph();
    g.layout.regions[0].required = true;
    let request =
        SpawnRequest::new(SpawnSource::Raw(Box::new(g))).input("task", RawInput::Text(" ".into()));
    resolve(
        &request,
        &worker(None),
        &Fake::default(),
        ResolveMode::Spawn,
    )
    .await
    .unwrap();
    let issues = spawn(&request, &Fake::default()).await.unwrap_err();
    assert_eq!(found(&issues), ["inputs.task Missing"]);
}

#[tokio::test]
async fn a_parent_that_turned_commands_off_turns_them_off_for_its_child() {
    let mut g = graph();
    g.layout.regions[0].seed = Some(Seed::Command("git log".into()));
    g.layout.regions[0].required = true;
    g.layout.regions[0].kind = RegionKind::Pinned;
    let mut policy = parent_policy(2);
    policy.seed_commands = false;
    let caller = Caller::Child {
        parent: n("p"),
        policy,
        depth: 0,
    };
    let env = Fake::default();
    let issues = resolve(&raw(g), &caller, &env, ResolveMode::Spawn)
        .await
        .unwrap_err();
    assert_eq!(
        found(&issues),
        ["source.raw.layout.regions[0].seed NotAllowed"]
    );
    assert!(env.seeds_run().is_empty());
}

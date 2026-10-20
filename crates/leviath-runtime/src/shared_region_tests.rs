use std::sync::Arc;

use bevy_ecs::prelude::*;
use leviath_core::{Region, RegionKind};
use serde_json::json;

use super::*;
use crate::components::{AgentState, AgentStatus, ContextWindow};
use crate::insert::RunSpecC;
use crate::spec::issues::SpecPath;

fn keyed(budget: usize, cap: Option<usize>) -> Region {
    Region::new(
        SHARED_REGION.to_string(),
        RegionKind::HashMap { max_entries: cap },
        budget,
    )
}

fn window_with(region: Region) -> ContextWindow {
    let mut w = ContextWindow::new(100_000);
    w.add_region(region);
    w
}

fn shared(w: &ContextWindow) -> Vec<(String, String)> {
    w.get_region(SHARED_REGION)
        .unwrap()
        .content
        .iter()
        .map(|e| {
            (
                e.key.clone().unwrap_or_default(),
                e.content.as_str().to_string(),
            )
        })
        .collect()
}

fn call(w: &mut ContextWindow, name: &str, args: serde_json::Value) -> String {
    handle_context_tool(name, &args, "r-a", w)
}

fn record(run: &str, content: &str, tokens: usize) -> Record {
    Record {
        run: run.to_string(),
        content: content.to_string(),
        tokens,
        taint: leviath_core::TaintLevel::Public,
    }
}

fn laid_out(laid: &[Laid]) -> Vec<(String, String)> {
    laid.iter()
        .map(|(e, _)| {
            (
                e.key.clone().unwrap_or_default(),
                e.content.as_str().to_string(),
            )
        })
        .collect()
}

fn spec_in(scope: Option<&str>) -> RunSpecC {
    let mut spec = crate::spec::run_spec::tests::spec();
    if let Some(scope) = scope {
        spec.delivery
            .metadata
            .insert(SCOPE_KEY.to_string(), scope.to_string());
    }
    RunSpecC(Arc::new(spec))
}

fn state(id: &str, status: AgentStatus) -> AgentState {
    AgentState {
        agent_id: id.to_string(),
        current_visit: String::new(),
        current_stage: "s".to_string(),
        iteration: 0,
        status,
        spawned_children_ids: vec![],
        pending_wait: None,
        accepts_messages: true,
    }
}

#[test]
fn a_scope_is_named_in_the_delivery_metadata() {
    assert_eq!(scope_of(&spec_in(None).0), None);
    assert_eq!(scope_of(&spec_in(Some("  ")).0), None);
    assert_eq!(scope_of(&spec_in(Some(" proj ")).0), Some("proj"));
}

#[test]
fn other_regions_are_written_as_they_always_were() {
    let mut w = window_with(Region::new(
        "notes".to_string(),
        RegionKind::HashMap { max_entries: None },
        1000,
    ));
    let out = call(
        &mut w,
        "context_write",
        json!({"region": "notes", "key": "anything", "content": "x"}),
    );
    assert_eq!(out, "Stored in 'notes' section under key 'anything'.");
}

#[test]
fn a_write_lands_under_the_runs_own_key() {
    let mut w = window_with(keyed(1000, None));
    let out = call(
        &mut w,
        "context_write",
        json!({"region": SHARED_REGION, "content": "edit App.tsx"}),
    );
    assert_eq!(
        out, "Stored in 'shared_intents' section under key 'r-a'.",
        "{out}"
    );
    let out = call(
        &mut w,
        "context_write",
        json!({"region": SHARED_REGION, "key": "r-a", "content": "edit App.tsx header"}),
    );
    assert!(out.starts_with("Stored"), "{out}");
    assert_eq!(
        shared(&w),
        vec![("r-a".to_string(), "edit App.tsx header".to_string())]
    );
}

#[test]
fn another_runs_record_cannot_be_written_or_deleted() {
    let mut w = window_with(keyed(1000, None));
    w.get_region_mut(SHARED_REGION)
        .unwrap()
        .upsert_by_key("r-b", "theirs", 1)
        .unwrap();
    for (name, args) in [
        (
            "context_write",
            json!({"region": SHARED_REGION, "key": "r-b", "content": "x"}),
        ),
        (
            "context_append",
            json!({"region": SHARED_REGION, "key": "r-b", "content": "x"}),
        ),
        (
            "context_delete",
            json!({"region": SHARED_REGION, "key": "r-b"}),
        ),
        (
            "context_delete",
            json!({"region": SHARED_REGION, "index": 0}),
        ),
        (
            "context_delete",
            json!({"region": SHARED_REGION, "oldest": 1}),
        ),
    ] {
        let out = call(&mut w, name, args);
        assert!(out.contains("read-only") && out.contains("'r-a'"), "{out}");
    }
    assert_eq!(shared(&w), vec![("r-b".to_string(), "theirs".to_string())]);
    // Reading another run's record is what the region is for.
    let out = call(
        &mut w,
        "context_read",
        json!({"region": SHARED_REGION, "key": "r-b"}),
    );
    assert_eq!(out, "theirs");
}

#[test]
fn a_delete_clears_the_runs_own_record() {
    let mut w = window_with(keyed(1000, None));
    call(
        &mut w,
        "context_write",
        json!({"region": SHARED_REGION, "content": "plan"}),
    );
    let out = call(&mut w, "context_delete", json!({"region": SHARED_REGION}));
    assert_eq!(out, "Released 'r-a' from 'shared_intents'.");
    assert!(shared(&w).is_empty());
}

#[test]
fn an_append_extends_the_runs_own_record() {
    let mut w = window_with(keyed(1000, None));
    let out = call(
        &mut w,
        "context_append",
        json!({"region": SHARED_REGION, "content": "a.rs"}),
    );
    assert!(out.starts_with("Created entry"), "{out}");
    let out = call(
        &mut w,
        "context_append",
        json!({"region": SHARED_REGION, "content": "b.rs"}),
    );
    assert!(out.starts_with("Appended"), "{out}");
    assert_eq!(
        shared(&w),
        vec![("r-a".to_string(), "a.rs\nb.rs".to_string())]
    );
}

#[test]
fn a_record_bigger_than_the_region_is_refused() {
    let mut w = window_with(keyed(5, None));
    let big = "word ".repeat(10);
    let out = call(
        &mut w,
        "context_write",
        json!({"region": SHARED_REGION, "content": big}),
    );
    assert!(out.contains("holds at most 5"), "{out}");
    call(
        &mut w,
        "context_write",
        json!({"region": SHARED_REGION, "content": "short"}),
    );
    // Growing in place is held to the same size.
    let out = call(
        &mut w,
        "context_append",
        json!({"region": SHARED_REGION, "content": "and much longer than that"}),
    );
    assert!(out.contains("holds at most 5"), "{out}");
    assert_eq!(shared(&w), vec![("r-a".to_string(), "short".to_string())]);
}

#[test]
fn a_window_without_the_region_says_so() {
    let mut w = ContextWindow::new(1000);
    let out = call(
        &mut w,
        "context_write",
        json!({"region": SHARED_REGION, "content": "x"}),
    );
    assert!(out.contains("not found"), "{out}");
    let out = call(&mut w, "context_list", json!({"region": SHARED_REGION}));
    assert!(out.contains("not found"), "{out}");
}

#[test]
fn a_plan_adds_what_fits_beside_the_runs_own_record() {
    let mut w = window_with(keyed(10, Some(3)));
    w.get_region_mut(SHARED_REGION)
        .unwrap()
        .upsert_by_key("r-a", "mine", 4)
        .unwrap();
    w.add_to_region(SHARED_REGION, "note".to_string(), 1)
        .unwrap();
    let region = w.get_region(SHARED_REGION).unwrap();
    let (b, c, d) = (
        record("r-b", "b", 3),
        record("r-c", "c", 3),
        record("r-d", "d", 1),
    );
    // 4 + 1 + 3 = 8, so c does not fit; d would, but the cap is three
    // entries and mine, the note and b are three.
    let laid = plan(region, "r-a", &[&b, &c, &d]).unwrap();
    assert_eq!(
        laid_out(&laid),
        vec![pair("r-a", "mine"), pair("", "note"), pair("r-b", "b")]
    );
    // Planned against a region that already holds them, there is nothing to do.
    apply(&mut w, laid);
    let region = w.get_region(SHARED_REGION).unwrap();
    assert!(plan(region, "r-a", &[&b, &c, &d]).is_none());
}

#[test]
fn a_region_with_no_entry_cap_takes_what_fits() {
    let region = Region::new(SHARED_REGION.to_string(), RegionKind::Pinned, 10);
    let (b, c) = (record("r-b", "b", 6), record("r-c", "c", 4));
    let laid = plan(&region, "r-a", &[&b, &c]).unwrap();
    assert_eq!(laid_out(&laid), vec![pair("r-b", "b"), pair("r-c", "c")]);
}

#[test]
fn apply_replaces_the_other_runs_records_and_keeps_its_own() {
    let mut w = window_with(keyed(100, None));
    let region = w.get_region_mut(SHARED_REGION).unwrap();
    region.upsert_by_key("r-a", "mine", 1).unwrap();
    region.upsert_by_key("r-gone", "old", 1).unwrap();
    let laid = plan(
        w.get_region(SHARED_REGION).unwrap(),
        "r-a",
        &[&record("r-b", "new", 1)],
    )
    .unwrap();
    apply(&mut w, laid);
    assert_eq!(
        shared(&w),
        vec![
            ("r-a".to_string(), "mine".to_string()),
            ("r-b".to_string(), "new".to_string()),
        ]
    );
    assert_eq!(w.current_tokens, 2);
}

fn spawn(world: &mut World, id: &str, scope: Option<&str>, region: bool) -> Entity {
    let w = match region {
        true => window_with(keyed(1000, None)),
        false => ContextWindow::new(1000),
    };
    world
        .spawn((state(id, AgentStatus::Active), spec_in(scope), w))
        .id()
}

fn write(world: &mut World, e: Entity, id: &str, text: &str) {
    let mut w = world.get_mut::<ContextWindow>(e).unwrap();
    handle_context_tool(
        "context_write",
        &json!({"region": SHARED_REGION, "content": text}),
        id,
        &mut w,
    );
}

fn seen(world: &World, e: Entity) -> Vec<(String, String)> {
    shared(world.get::<ContextWindow>(e).unwrap())
}

fn pair(k: &str, v: &str) -> (String, String) {
    (k.to_string(), v.to_string())
}

#[test]
fn runs_in_one_scope_see_each_others_records_and_no_one_elses() {
    use bevy_ecs::system::RunSystemOnce;
    let mut world = World::new();
    let a = spawn(&mut world, "r-a", Some("proj"), true);
    let b = spawn(&mut world, "r-b", Some("proj"), true);
    let other = spawn(&mut world, "r-x", Some("elsewhere"), true);
    let alone = spawn(&mut world, "r-n", None, true);
    // In the scope, but its graph has no shared region: it neither reads
    // nor writes one.
    let blind = spawn(&mut world, "r-z", Some("proj"), false);
    for (e, id) in [(a, "r-a"), (b, "r-b"), (other, "r-x"), (alone, "r-n")] {
        write(&mut world, e, id, &format!("{id} plans"));
    }
    world.run_system_once(sync_shared_regions).unwrap();
    assert_eq!(
        seen(&world, a),
        vec![pair("r-a", "r-a plans"), pair("r-b", "r-b plans")]
    );
    assert_eq!(
        seen(&world, b),
        vec![pair("r-b", "r-b plans"), pair("r-a", "r-a plans")]
    );
    assert_eq!(seen(&world, other), vec![pair("r-x", "r-x plans")]);
    assert_eq!(seen(&world, alone), vec![pair("r-n", "r-n plans")]);
    assert!(
        world
            .get::<ContextWindow>(blind)
            .unwrap()
            .regions
            .is_empty()
    );

    // A changed record reaches the others; a cleared one leaves them.
    write(&mut world, a, "r-a", "r-a changed its mind");
    world.run_system_once(sync_shared_regions).unwrap();
    assert_eq!(
        seen(&world, b),
        vec![
            pair("r-b", "r-b plans"),
            pair("r-a", "r-a changed its mind")
        ]
    );
    {
        let mut w = world.get_mut::<ContextWindow>(a).unwrap();
        handle_context_tool(
            "context_delete",
            &json!({"region": SHARED_REGION}),
            "r-a",
            &mut w,
        );
    }
    world.run_system_once(sync_shared_regions).unwrap();
    assert_eq!(seen(&world, b), vec![pair("r-b", "r-b plans")]);
    assert_eq!(seen(&world, a), vec![pair("r-b", "r-b plans")]);
}

#[test]
fn a_run_that_ends_leaves_its_scope() {
    use bevy_ecs::system::RunSystemOnce;
    let mut world = World::new();
    let a = spawn(&mut world, "r-a", Some("proj"), true);
    let b = spawn(&mut world, "r-b", Some("proj"), true);
    write(&mut world, a, "r-a", "a");
    write(&mut world, b, "r-b", "b");
    world.run_system_once(sync_shared_regions).unwrap();
    assert_eq!(seen(&world, b), vec![pair("r-b", "b"), pair("r-a", "a")]);
    world.get_mut::<AgentState>(a).unwrap().status = AgentStatus::Complete;
    world.run_system_once(sync_shared_regions).unwrap();
    assert_eq!(seen(&world, b), vec![pair("r-b", "b")]);
    // The finished run's own window is left as it ended.
    assert_eq!(seen(&world, a), vec![pair("r-a", "a"), pair("r-b", "b")]);
    // And a run unloaded from the world is gone from the scope as well.
    let c = spawn(&mut world, "r-c", Some("proj"), true);
    write(&mut world, c, "r-c", "c");
    world.run_system_once(sync_shared_regions).unwrap();
    assert_eq!(seen(&world, b), vec![pair("r-b", "b"), pair("r-c", "c")]);
    world.despawn(c);
    world.run_system_once(sync_shared_regions).unwrap();
    assert_eq!(seen(&world, b), vec![pair("r-b", "b")]);
}

#[test]
fn the_shared_region_has_to_be_keyed() {
    use crate::spec::graph::tests::{minimal, region};
    let mut g = minimal();
    g.layout.regions.push(region(SHARED_REGION));
    let issues = g
        .validate(&SpecPath::root().field("graph"))
        .unwrap_err()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    assert_eq!(
        issues,
        vec![
            "graph.layout.regions[2].kind: wrong type: region \"shared_intents\" holds one \
             record per run sharing it, so it has to be keyed. declare it with kind = \"keyed\""
                .to_string()
        ]
    );
    g.layout.regions[2].kind = crate::spec::graph::RegionKind::Keyed { max_entries: None };
    assert!(g.validate(&SpecPath::root().field("graph")).is_ok());
}

#[test]
fn a_key_that_names_no_other_run_lands_as_the_runs_own() {
    // A keyed region asks for a key, and a model cannot know its run id, so
    // whatever it calls its record is its record.
    let mut w = window_with(keyed(1000, None));
    let out = call(
        &mut w,
        "context_write",
        json!({"region": SHARED_REGION, "key": "plan", "content": "edit App.tsx"}),
    );
    assert_eq!(
        out, "Stored in 'shared_intents' section under key 'r-a'.",
        "{out}"
    );
    let out = call(
        &mut w,
        "context_append",
        json!({"region": SHARED_REGION, "key": "plan", "content": "and Header.tsx"}),
    );
    assert!(out.starts_with("Appended"), "{out}");
    assert_eq!(
        shared(&w),
        vec![pair("r-a", "edit App.tsx\nand Header.tsx")]
    );
    let out = call(
        &mut w,
        "context_delete",
        json!({"region": SHARED_REGION, "key": "plan"}),
    );
    assert_eq!(out, "Released 'r-a' from 'shared_intents'.");
}

fn tainted(world: &mut World, e: Entity, level: leviath_core::TaintLevel) {
    let mut w = world.get_mut::<ContextWindow>(e).unwrap();
    let mut secrets = Region::new("secrets".to_string(), RegionKind::Pinned, 1000);
    secrets.enable_taint_tracking();
    secrets
        .add_typed_tainted_entry("a secret", 1, leviath_core::EntryKind::default(), level)
        .unwrap();
    w.add_region(secrets);
}

fn shared_taint(world: &World, e: Entity) -> Option<leviath_core::TaintLevel> {
    world
        .get::<ContextWindow>(e)
        .unwrap()
        .get_region(SHARED_REGION)
        .unwrap()
        .taint_level()
}

#[test]
fn a_record_carries_the_taint_of_the_run_that_wrote_it() {
    use bevy_ecs::system::RunSystemOnce;
    use leviath_core::TaintLevel;
    let mut world = World::new();
    let a = spawn(&mut world, "r-a", Some("proj"), true);
    let b = spawn(&mut world, "r-b", Some("proj"), true);
    let untracked = spawn(&mut world, "r-u", Some("proj"), true);
    tainted(&mut world, a, TaintLevel::Private);
    world
        .get_mut::<ContextWindow>(b)
        .unwrap()
        .enable_taint_tracking();
    write(&mut world, a, "r-a", "what a read in private");
    world.run_system_once(sync_shared_regions).unwrap();
    assert_eq!(shared_taint(&world, b), Some(TaintLevel::Private));
    // A run that tracks no taint has no gate for it to raise.
    assert_eq!(shared_taint(&world, untracked), None);
    // Once the private run leaves, so does its taint.
    world.get_mut::<AgentState>(a).unwrap().status = AgentStatus::Complete;
    world.run_system_once(sync_shared_regions).unwrap();
    assert_eq!(shared_taint(&world, b), Some(TaintLevel::Public));
}

#[test]
fn the_runs_own_record_comes_first() {
    use bevy_ecs::system::RunSystemOnce;
    let mut world = World::new();
    let a = spawn(&mut world, "r-a", Some("proj"), true);
    let b = spawn(&mut world, "r-b", Some("proj"), true);
    write(&mut world, a, "r-a", "a");
    world.run_system_once(sync_shared_regions).unwrap();
    // b writes after a's record reached it.
    write(&mut world, b, "r-b", "b");
    world.run_system_once(sync_shared_regions).unwrap();
    assert_eq!(seen(&world, b), vec![pair("r-b", "b"), pair("r-a", "a")]);
}

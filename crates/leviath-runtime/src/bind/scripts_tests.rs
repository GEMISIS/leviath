use super::*;
use crate::spec::run_spec::tests::spec;

const HOOK: &str = "fn on_stage_enter(ctx) { () }\nfn on_stage_exit(ctx) { () }";
const VALIDATOR: &str = "fn validate(content) { () }";
const REGION: &str = "fn render(ctx) { \"\" }";

/// A spec with no code, and a run file holding nothing.
fn bare() -> (RunSpec, CodeFiles) {
    let mut s = spec();
    s.code.clear();
    (s, CodeFiles::new())
}

/// Record `text` in the spec and the run file under `reference`.
fn add(s: &mut RunSpec, code: &mut CodeFiles, reference: CodeRef, text: &str) {
    let digest = Digest::of(text.as_bytes());
    s.code.push((reference, digest.clone()));
    code.insert(digest, text.as_bytes().to_vec());
}

fn file(path: &str) -> CodeRef {
    CodeRef::File(path.into())
}

fn paths(issues: &SpawnIssues) -> Vec<String> {
    issues.iter().map(|i| i.path.to_string()).collect()
}

fn place(bindings: Bindings) -> (bevy_ecs::world::World, bevy_ecs::entity::Entity) {
    let mut world = bevy_ecs::world::World::new();
    let e = crate::insert::insert(
        &mut world,
        Arc::new(spec()),
        bindings,
        &crate::state::RunState::initial(
            crate::spec::names::StageName::new("plan").unwrap(),
            Default::default(),
            true,
        ),
    );
    (world, e)
}

#[test]
fn a_run_with_no_code_gets_no_script_components() {
    let (s, code) = bare();
    assert!(compile(&s, &code).unwrap().is_empty());
}

#[test]
fn hooks_validators_and_regions_compile_once_each_from_the_run_file() {
    let (mut s, mut code) = bare();
    add(&mut s, &mut code, file("hooks/h.rhai"), HOOK);
    add(&mut s, &mut code, file("check.rhai"), VALIDATOR);
    add(&mut s, &mut code, CodeRef::Inline(REGION.into()), REGION);
    s.graph.stages[0].hooks.on_stage_enter = Some(file("hooks/h.rhai"));
    s.graph.stages[0].hooks.on_stage_exit = Some(file("hooks/h.rhai"));
    s.graph.stages[1].hooks.on_stage_enter = Some(file("hooks/h.rhai"));
    s.graph.output = Some(OutputDef {
        validator: Some(file("check.rhai")),
        ..Default::default()
    });
    s.graph.stages[1].output = Some(OutputDef {
        validator: Some(file("check.rhai")),
        ..Default::default()
    });
    s.graph.stages[0].output = Some(OutputDef::default());
    let custom = RegionKind::Custom {
        code: CodeRef::Inline(REGION.into()),
        pinned: false,
    };
    s.graph.layout.regions[1].kind = custom.clone();
    let mut own = s.graph.layout.clone();
    own.regions[0].kind = custom;
    s.graph.stages[1].layout = Some(own);

    let bindings = compile(&s, &code).unwrap();
    assert_eq!(bindings.len(), 3);
    let (world, e) = place(bindings);
    let hooks = world.get::<StageHookScripts>(e).unwrap();
    assert_eq!(hooks.0.keys().collect::<Vec<_>>(), ["hooks/h.rhai"]);
    let validators = world.get::<OutputValidators>(e).unwrap();
    assert_eq!(
        validators.compiled.keys().collect::<Vec<_>>(),
        ["check.rhai"]
    );
    let regions = world.get::<RegionScripts>(e).unwrap();
    let key = format!("inline:{}", Digest::of(REGION.as_bytes()));
    assert_eq!(regions.0.keys().collect::<Vec<_>>(), [&key]);
}

#[test]
fn code_the_spec_or_the_run_file_lacks_is_missing_where_it_is_named() {
    let (mut s, mut code) = bare();
    s.graph.stages[0].hooks.on_stage_enter = Some(file("unrecorded.rhai"));
    add(&mut s, &mut code, file("gone.rhai"), VALIDATOR);
    code.clear();
    s.graph.output = Some(OutputDef {
        validator: Some(file("gone.rhai")),
        ..Default::default()
    });
    s.graph.layout.regions[0].kind = RegionKind::Custom {
        code: file("unrecorded.rhai"),
        pinned: true,
    };
    let issues = compile(&s, &code).unwrap_err();
    assert_eq!(
        paths(&issues),
        [
            "stages.plan.hooks.on_stage_enter",
            "output.validator",
            "layout.regions.system.kind"
        ]
    );
    assert!(issues.iter().all(|i| i.code == IssueCode::Missing));
    assert_eq!(issues.0[0].message, "the run records no code for this");
    assert_eq!(issues.0[1].message, "the run file holds no code for this");
}

#[test]
fn code_that_will_not_compile_is_invalid_where_it_is_named() {
    let (mut s, mut code) = bare();
    add(&mut s, &mut code, file("h.rhai"), "fn nothing() {}");
    add(&mut s, &mut code, file("v.rhai"), "fn nothing() {}");
    add(&mut s, &mut code, file("r.rhai"), "fn nothing() {}");
    let digest = Digest::of(&[0xff]);
    s.code.push((file("bin.rhai"), digest.clone()));
    code.insert(digest, vec![0xff]);
    s.graph.stages[0].hooks.on_stage_enter = Some(file("h.rhai"));
    s.graph.stages[1].hooks.on_error = Some(file("bin.rhai"));
    s.graph.stages[1].output = Some(OutputDef {
        validator: Some(file("v.rhai")),
        ..Default::default()
    });
    let mut own = s.graph.layout.clone();
    own.regions[0].kind = RegionKind::Custom {
        code: file("r.rhai"),
        pinned: false,
    };
    s.graph.stages[0].layout = Some(own);
    let issues = compile(&s, &code).unwrap_err();
    assert_eq!(
        paths(&issues),
        [
            "stages.build.hooks.on_error",
            "stages.plan.hooks.on_stage_enter",
            "stages.build.output.validator",
            "stages.plan.layout.regions.system.kind"
        ]
    );
    assert!(issues.0[0].message.contains("not UTF-8"), "{}", issues.0[0]);
    assert!(
        issues.0[1].message.contains("on_stage_enter"),
        "{}",
        issues.0[1]
    );
    assert!(issues.iter().all(|i| i.code == IssueCode::Invalid));
}

use super::*;
use crate::spec::graph::{SeedRefresh, SeedToolCall};

/// The graph with its `system` region seeded by `seed`.
fn seeded(seed: Seed, required: bool) -> RunGraph {
    let mut g = graph();
    g.layout.regions[0].seed = Some(seed);
    g.layout.regions[0].required = required;
    g
}

#[tokio::test]
async fn a_spawn_runs_every_seed_and_seeds_come_before_inputs() {
    let mut g = seeded(Seed::Command("git log".into()), false);
    g.layout.regions[1].seed = Some(Seed::Literal("Read this first.".into()));
    let env = Fake::default();
    let resolved = spawn(&raw(g), &env).await.unwrap();
    assert_eq!(env.seeds_run().len(), 2);
    let seeded = &resolved.spec.seeded;
    assert_eq!(seeded["system"].text, "ran git log in /work");
    assert_eq!(seeded["task"].text, "Read this first.\n\ndo the thing");
}

#[tokio::test]
async fn a_dry_run_never_runs_a_seed_with_side_effects() {
    let mut g = seeded(Seed::Command("rm -rf build".into()), true);
    g.layout.regions[1].seed = Some(Seed::Glob("docs/*.md".into()));
    let mut own = g.layout.clone();
    own.regions[0].seed = Some(Seed::Code(CodeRef::Inline("fn seed() {}".into())));
    own.regions[0].name = n("notes");
    own.regions[0].required = true;
    g.stages[1].layout = Some(own);
    let env = Fake::default();
    let resolved = resolve(&raw(g), &Caller::TopLevel, &env, ResolveMode::Check)
        .await
        .unwrap();
    assert_eq!(env.seeds_run(), [Seed::Glob("docs/*.md".into())]);
    assert_eq!(
        resolved.spec.seeded["task"].text,
        "glob docs/*.md\n\ndo the thing"
    );
    assert!(!resolved.spec.seeded.contains_key("system"));
    assert!(!resolved.spec.seeded.contains_key("notes"));
}

#[tokio::test]
async fn a_refused_spawn_never_runs_a_command_on_the_way() {
    let g = seeded(Seed::Command("deploy".into()), false);
    let env = Fake::default();
    let request = raw(g).input("typo", RawInput::Int(1));
    let issues = spawn(&request, &env).await.unwrap_err();
    assert_eq!(found(&issues), ["inputs.typo Unknown"]);
    assert!(env.seeds_run().is_empty());
}

#[tokio::test]
async fn a_dry_run_still_checks_commands_and_tool_calls_are_well_formed() {
    let mut g = seeded(Seed::Command("  ".into()), false);
    g.layout.regions[1].seed = Some(Seed::Tools {
        calls: vec![
            SeedToolCall {
                tool: n("read_file"),
                args: leviath_core::JsonDoc::new(serde_json::json!({"path": "a"})),
            },
            SeedToolCall {
                tool: n("list_dir"),
                args: leviath_core::JsonDoc::new(serde_json::json!(["."])),
            },
        ],
        refresh: SeedRefresh::Once,
    });
    let env = Fake::default();
    let issues = resolve(&raw(g), &Caller::TopLevel, &env, ResolveMode::Check)
        .await
        .unwrap_err();
    assert_eq!(
        found(&issues),
        [
            "source.raw.layout.regions[0].seed Invalid",
            "source.raw.layout.regions[1].seed.tools.calls[1].args WrongType",
        ]
    );
    assert_eq!(issues.0[1].got.as_deref(), Some("[\".\"]"));
    assert!(env.seeds_run().is_empty());
}

#[tokio::test]
async fn commands_off_refuse_a_required_region_and_skip_an_optional_one() {
    let env = Fake {
        limits: SpawnLimits {
            seed_commands_allowed: false,
            ..Fake::default().limits
        },
        ..Fake::default()
    };
    let issues = spawn(&raw(seeded(Seed::Command("ls".into()), true)), &env)
        .await
        .unwrap_err();
    assert_eq!(
        found(&issues),
        ["source.raw.layout.regions[0].seed NotAllowed"]
    );

    let mut request = raw(seeded(Seed::Command("ls".into()), false));
    request.launch.seed_commands = true;
    let resolved = spawn(&request, &env).await.unwrap();
    assert_eq!(
        resolved.spec.stages[0].notes,
        ["region 'system': command seed skipped: command seeds are off for this run"]
    );
    assert!(env.seeds_run().is_empty());
}

#[tokio::test]
async fn a_failed_seed_sinks_only_a_required_region() {
    let env = Fake::default();
    let issues = spawn(&raw(seeded(Seed::Command("fail".into()), true)), &env)
        .await
        .unwrap_err();
    assert_eq!(
        found(&issues),
        ["source.raw.layout.regions[0].seed Unresolvable"]
    );
    assert!(issues.0[0].message.contains("exit 1"));

    let resolved = spawn(&raw(seeded(Seed::Command("fail".into()), false)), &env)
        .await
        .unwrap();
    assert_eq!(
        resolved.spec.stages[0].notes,
        ["region 'system': seed failed, left empty: exit 1"]
    );
}

#[tokio::test]
async fn code_and_tool_seeds_run_on_a_spawn_and_see_the_inputs() {
    let mut g = seeded(Seed::Code(CodeRef::Inline("fn seed() {}".into())), true);
    g.layout.regions[1].seed = Some(Seed::Files(vec![n("a.md"), n("b.md")]));
    let mut own = g.layout.clone();
    own.regions[0].seed = Some(Seed::Tools {
        calls: vec![],
        refresh: SeedRefresh::EachStage,
    });
    g.stages[0].layout = Some(own);
    let env = Fake::default();
    let resolved = spawn(&raw(g), &env).await.unwrap();
    let seeded = &resolved.spec.seeded;
    assert_eq!(seeded["system"].text, "code fn seed() {} saw 1 inputs");
    assert_eq!(seeded["task"].text, "2 files\n\ndo the thing");
    assert_eq!(
        env.seeds_run().len(),
        2,
        "a region in two layouts is seeded once"
    );
}

#[tokio::test]
async fn no_workdir_means_no_seeds() {
    let env = Fake {
        workdir: Err("gone".into()),
        ..Fake::default()
    };
    let issues = spawn(&raw(seeded(Seed::Literal("x".into()), false)), &env)
        .await
        .unwrap_err();
    assert_eq!(found(&issues), ["workdir Unresolvable"]);
    assert!(env.seeds_run().is_empty());
}

#[tokio::test]
async fn a_seed_that_writes_nothing_leaves_its_region_out() {
    let g = seeded(Seed::Literal("   ".into()), false);
    let resolved = spawn(&raw(g), &Fake::default()).await.unwrap();
    assert!(!resolved.spec.seeded.contains_key("system"));
}

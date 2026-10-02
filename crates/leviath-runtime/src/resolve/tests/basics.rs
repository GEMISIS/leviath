use std::path::PathBuf;

use super::*;
use crate::spec::env::LoadedBlueprint;
use crate::spec::graph::{DependencyDef, Needs};
use crate::spec::launch::Unattended;
use crate::spec::request::Attachment;
use crate::spec::run_spec::SpecOrigin;

#[tokio::test]
async fn a_raw_request_resolves_to_a_whole_spec() {
    let env = Fake {
        printed: ["mock".to_string()].into(),
        ..Fake::default()
    };
    let mut g = graph();
    g.title = None;
    let resolved = spawn(&raw(g), &env).await.unwrap();
    let spec = &resolved.spec;
    assert_eq!(spec.run_id.as_str(), "run-1");
    assert_eq!(spec.origin, SpecOrigin::Raw);
    assert_eq!(spec.stages.len(), 2);
    assert_eq!(spec.stages[0].stage.as_str(), "plan");
    assert_eq!(spec.stages[0].provider.as_str(), "mock");
    assert_eq!(spec.stages[0].model.as_str(), "gpt-mock");
    assert_eq!(spec.stages[0].context_window, 100_000);
    assert_eq!(spec.stages[0].max_output_tokens, None);
    assert_eq!(spec.stages[0].output, None);
    assert_eq!(spec.seeded["task"].text, "do the thing");
    assert_eq!(spec.placement.workdir, PathBuf::from("/work"));
    assert_eq!(spec.placement.depth, 0);
    assert_eq!(spec.placement.parent, None);
    assert_eq!(spec.launch.max_depth, 3, "the operator's default depth");
    assert!(spec.created_at > 0);
    assert_eq!(spec.env.leviath_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        spec.env
            .providers
            .keys()
            .map(|p| p.as_str())
            .collect::<Vec<_>>(),
        ["mock"]
    );
    assert!(resolved.code.is_empty());
    assert!(resolved.blobs.is_empty());
    assert_eq!(
        spec.inputs.get("task").map(|v| v.render_text()),
        Some("do the thing".into())
    );
}

#[tokio::test]
async fn a_titled_graph_names_its_run_after_the_title() {
    let env = Fake::default();
    let resolved = spawn(&raw(graph()), &env).await.unwrap();
    assert_eq!(resolved.spec.run_id.as_str(), "t-1");
}

fn installed(graph: RunGraph) -> LoadedBlueprint {
    LoadedBlueprint {
        graph,
        reference: BlueprintRef::parse(&format!("coder@{}", Digest::of(b"v1"))).unwrap(),
        version: "1.2.0".into(),
        base_dir: PathBuf::from("/agents/coder"),
    }
}

/// A fan-out's installed worker blueprint is pinned to the revision installed
/// when the run is resolved; one not installed, or already pinned, is left as
/// written, and so is any other stage.
#[tokio::test]
async fn a_fan_out_worker_blueprint_is_pinned_to_the_installed_revision() {
    use crate::spec::graph::{FanOutDef, StageMode, WorkerFailure, WorkerSource};
    let fan = |worker: &str| {
        StageMode::FanOut(FanOutDef {
            worker: WorkerSource::Blueprint(BlueprintRef::parse(worker).unwrap()),
            merge_stage: None,
            max_workers: Some(2),
            on_worker_failure: WorkerFailure::Continue,
            split_prompt: String::new(),
            results_region: None,
            max_items: None,
            max_attempts: None,
        })
    };
    let pinned = format!("coder@{}", Digest::of(b"v0"));
    let mut g = graph();
    g.stages[0].mode = fan("coder");
    let mut ghost = g.clone();
    ghost.stages[0].mode = fan("ghost");
    let mut kept = g.clone();
    kept.stages[0].mode = fan(&pinned);
    let env = Fake {
        blueprints: [("coder".to_string(), installed(graph()))].into(),
        ..Fake::default()
    };
    let worker = |g: RunGraph| async {
        let spec = spawn(&raw(g), &env).await.unwrap().spec;
        match &spec.graph.stages[0].mode {
            StageMode::FanOut(fan) => fan.worker.clone(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(
        worker(g).await,
        WorkerSource::Blueprint(
            BlueprintRef::parse(&format!("coder@{}", Digest::of(b"v1"))).unwrap()
        )
    );
    assert_eq!(
        worker(ghost).await,
        WorkerSource::Blueprint(BlueprintRef::parse("ghost").unwrap())
    );
    assert_eq!(
        worker(kept).await,
        WorkerSource::Blueprint(BlueprintRef::parse(&pinned).unwrap())
    );
}

#[tokio::test]
async fn a_blueprint_request_loads_the_installed_graph() {
    let mut g = graph();
    g.stages[0].hooks.on_stage_enter = Some(CodeRef::File("hooks/enter.rhai".into()));
    let env = Fake {
        blueprints: [("coder".to_string(), installed(g))].into(),
        files: [(
            "hooks/enter.rhai".to_string(),
            b"fn on_stage_enter() {}".to_vec(),
        )]
        .into(),
        ..Fake::default()
    };
    let request = SpawnRequest::new(SpawnSource::Blueprint(
        BlueprintRef::parse("coder").unwrap(),
    ))
    .input("task", RawInput::Text("go".into()));
    let resolved = spawn(&request, &env).await.unwrap();
    let spec = &resolved.spec;
    assert_eq!(spec.run_id.as_str(), "coder-1");
    assert!(matches!(
        &spec.origin,
        SpecOrigin::Blueprint { version, .. } if version == "1.2.0"
    ));
    // The run lists the file its blueprint was read from.
    let manifest = std::path::Path::new("/agents/coder").join("agent.toml");
    assert_eq!(spec.origin.manifest(), manifest.to_string_lossy());
    let digest = Digest::of(b"fn on_stage_enter() {}");
    assert_eq!(
        spec.code_digest(&CodeRef::File("hooks/enter.rhai".into())),
        Some(&digest)
    );
    assert_eq!(resolved.code[&digest], b"fn on_stage_enter() {}");
}

#[tokio::test]
async fn a_missing_blueprint_and_a_bad_workdir_are_both_reported() {
    let env = Fake {
        workdir: Err("the default workdir is gone".into()),
        ..Fake::default()
    };
    let request = SpawnRequest::new(SpawnSource::Blueprint(
        BlueprintRef::parse("ghost").unwrap(),
    ));
    let issues = spawn(&request, &env).await.unwrap_err();
    assert_eq!(
        found(&issues),
        ["source.blueprint.name Unknown", "workdir Unresolvable"]
    );
    assert_eq!(issues.0[1].got.as_deref(), Some("no workdir"));
}

#[tokio::test]
async fn a_requested_workdir_the_machine_refuses_says_which() {
    let mut request = raw(graph());
    request.workdir = Some("/nope".into());
    let issues = spawn(&request, &Fake::default()).await.unwrap_err();
    assert_eq!(found(&issues), ["workdir Unresolvable"]);
    assert_eq!(issues.0[0].got.as_deref(), Some("/nope"));
    assert_eq!(issues.0[0].message, "no such directory");
}

#[tokio::test]
async fn a_requested_workdir_is_used() {
    let mut request = raw(graph());
    request.workdir = Some("/elsewhere".into());
    let resolved = spawn(&request, &Fake::default()).await.unwrap();
    assert_eq!(resolved.spec.placement.workdir, PathBuf::from("/elsewhere"));
}

/// The case the resolver exists for: one request, many unrelated mistakes,
/// every one of them reported at once with its own path.
#[tokio::test]
async fn many_independent_problems_come_back_together() {
    let mut g = graph();
    g.dependencies.push(DependencyDef {
        name: "gh".into(),
        needs: Needs::Binary("gh".into()),
        required: true,
        remedy: None,
        description: None,
        install: None,
    });
    g.stages[1].hooks.on_stage_exit = Some(CodeRef::Inline("broken(".into()));
    let env = Fake {
        models: [(
            "plan".to_string(),
            Err(SpawnIssue::new(
                SpecPath::root(),
                IssueCode::Unresolvable,
                "no provider serves m",
            )
            .known(["gpt-mock"])),
        )]
        .into(),
        failing_deps: [("gh".to_string(), "`gh` is not on PATH".to_string())].into(),
        ..Fake::default()
    };
    let big = Attachment {
        name: "big.txt".into(),
        mime_type: None,
        region: None,
        deliver: None,
        caption: None,
        data: crate::spec::request::Bytes(vec![b'x'; 2048]),
    };
    let mut request = raw(g).input("colour", RawInput::Text("blue".into()));
    request.attachments = vec![big.clone(), big];
    request.launch.unattended = Unattended::All;
    let issues = spawn(&request, &env).await.unwrap_err();
    assert_eq!(
        found(&issues),
        [
            "attachments[0].data OutOfRange",
            "attachments[1].name Duplicate",
            "inputs.colour Unknown",
            "source.raw.stages.build.hooks.on_stage_exit Invalid",
            "source.raw.dependencies[0] Unresolvable",
            "source.raw.stages.plan.model Unresolvable",
        ]
    );
    let text = issues.to_string();
    assert!(text.starts_with("6 problems with this spawn:"), "{text}");
    assert!(text.contains("run `lev deps`"), "{text}");
    assert!(text.contains("Known: gpt-mock"), "{text}");
    assert!(env.seeds_run().is_empty());
}

#[tokio::test]
async fn an_invalid_graph_has_only_its_stages_models_checked() {
    let mut g = graph();
    g.entry = Some(n("nowhere"));
    let stages = g.stages.len();
    let request = raw(g).input("colour", RawInput::Bool(true));
    let env = Fake {
        models: [(
            "build".to_string(),
            Err(SpawnIssue::new(
                SpecPath::root(),
                IssueCode::Unresolvable,
                "no provider",
            )),
        )]
        .into(),
        ..Fake::default()
    };
    let issues = spawn(&request, &env).await.unwrap_err();
    assert_eq!(
        found(&issues),
        [
            "source.raw.entry Dangling",
            "inputs.colour Unknown",
            "source.raw.stages.build.model Unresolvable",
        ]
    );
    assert_eq!(env.asked().len(), stages, "each stage's model is checked");
    assert!(
        env.tool_bases.lock().unwrap().is_empty(),
        "no tools are chosen for a broken graph"
    );
}

#[tokio::test]
async fn dependencies_that_are_not_met_refuse_or_warn() {
    let dep = |name: &str, required: bool| DependencyDef {
        name: name.into(),
        needs: Needs::Env(format!("{name}_TOKEN")),
        required,
        remedy: Some(format!("export {name}_TOKEN")),
        description: None,
        install: None,
    };
    let mut g = graph();
    g.dependencies = vec![dep("slack", false), dep("jira", true), dep("ok", true)];
    let env = Fake {
        failing_deps: [
            ("slack".to_string(), "unset".to_string()),
            ("jira".to_string(), "unset".to_string()),
        ]
        .into(),
        ..Fake::default()
    };
    let issues = spawn(&raw(g.clone()), &env).await.unwrap_err();
    assert_eq!(found(&issues), ["source.raw.dependencies[1] Unresolvable"]);
    assert_eq!(issues.0[0].hint.as_deref(), Some("export jira_TOKEN"));

    g.dependencies.remove(1);
    let resolved = spawn(&raw(g), &env).await.unwrap();
    assert_eq!(
        resolved.spec.stages[0].notes,
        ["optional dependency 'slack' is not met: unset"]
    );
}

#[tokio::test]
async fn run_level_notes_land_on_the_entry_stage() {
    let mut g = graph();
    g.entry = Some(n("build"));
    g.dependencies.push(DependencyDef {
        name: "nice".into(),
        needs: Needs::Binary("jq".into()),
        required: false,
        remedy: None,
        description: None,
        install: None,
    });
    let env = Fake {
        failing_deps: [("nice".to_string(), "missing".to_string())].into(),
        ..Fake::default()
    };
    let resolved = spawn(&raw(g), &env).await.unwrap();
    assert!(resolved.spec.stages[0].notes.is_empty());
    assert_eq!(resolved.spec.stages[1].notes.len(), 1);
}

/// A blueprint read from a directory runs as itself: its origin names the
/// directory, the manifest's name and the revision read, and the run is
/// named after the manifest. A directory with no blueprint is refused at the
/// source.
#[tokio::test]
async fn a_blueprint_file_request_loads_the_graph_in_that_directory() {
    let dir = std::env::temp_dir().join("agents").join("local-coder");
    let path = crate::spec::names::BlueprintPath::new(dir.to_string_lossy()).unwrap();
    let env = Fake {
        blueprints: [(path.to_string(), installed(graph()))].into(),
        ..Fake::default()
    };
    let request = SpawnRequest::new(SpawnSource::BlueprintFile(path.clone()))
        .input("task", RawInput::Text("go".into()));
    let resolved = spawn(&request, &env).await.unwrap();
    assert_eq!(resolved.spec.run_id.as_str(), "coder-1");
    assert_eq!(resolved.spec.origin.blueprint_name(), Some("coder"));
    assert_eq!(
        resolved.spec.origin.digest(),
        Some(&Digest::of(b"v1")),
        "pinned to the revision read"
    );
    assert!(matches!(
        &resolved.spec.origin,
        SpecOrigin::BlueprintFile { path: p, version, .. } if *p == path && version == "1.2.0"
    ));
    assert_eq!(
        resolved.spec.origin.manifest(),
        path.path().join("agent.toml").to_string_lossy()
    );

    let gone = crate::spec::names::BlueprintPath::new(
        std::env::temp_dir().join("nothing-here").to_string_lossy(),
    )
    .unwrap();
    let issues = spawn(&SpawnRequest::new(SpawnSource::BlueprintFile(gone)), &env)
        .await
        .unwrap_err();
    assert_eq!(found(&issues), ["source.blueprint_file Unresolvable"]);
}

/// Only a caller on this machine may name a blueprint by its directory: a
/// request from the network that does is refused with where and why.
#[test]
fn a_remote_request_may_not_name_a_directory() {
    let path =
        crate::spec::names::BlueprintPath::new(std::env::temp_dir().join("x").to_string_lossy())
            .unwrap();
    let issues = SpawnRequest::new(SpawnSource::BlueprintFile(path))
        .check_remote()
        .unwrap_err();
    assert_eq!(found(&issues), ["source.blueprint_file NotAllowed"]);
    assert!(raw(graph()).check_remote().is_ok());
    let named = SpawnRequest::new(SpawnSource::Blueprint(
        BlueprintRef::parse("coder").unwrap(),
    ));
    assert!(named.check_remote().is_ok());
    // A run whose caller wrote its graph comes from no blueprint.
    assert_eq!(SpecOrigin::Raw.blueprint_name(), None);
    assert_eq!(SpecOrigin::Raw.digest(), None);
}

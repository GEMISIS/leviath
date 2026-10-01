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
async fn an_invalid_graph_stops_before_the_machine_is_asked_about_it() {
    let mut g = graph();
    g.entry = Some(n("nowhere"));
    let request = raw(g).input("colour", RawInput::Bool(true));
    let env = Fake::default();
    let issues = spawn(&request, &env).await.unwrap_err();
    assert_eq!(
        found(&issues),
        ["source.raw.entry Dangling", "inputs.colour Unknown"]
    );
    assert!(
        env.asked().is_empty(),
        "no model was chosen for a broken graph"
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

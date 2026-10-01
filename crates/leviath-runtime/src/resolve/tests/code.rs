use std::path::PathBuf;

use super::*;
use crate::spec::env::LoadedBlueprint;
use crate::spec::graph::{InstallDef, MimeRowDef, Needs, OutputDef, RegionKind};

fn blueprint_request(graph: RunGraph, files: &[(&str, &str)]) -> (SpawnRequest, Fake) {
    let env = Fake {
        blueprints: [(
            "coder".to_string(),
            LoadedBlueprint {
                graph,
                reference: BlueprintRef::parse("coder").unwrap(),
                version: "1".into(),
                base_dir: PathBuf::from("/agents/coder"),
            },
        )]
        .into(),
        files: files
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
            .collect(),
        ..Fake::default()
    };
    let request = SpawnRequest::new(SpawnSource::Blueprint(
        BlueprintRef::parse("coder").unwrap(),
    ))
    .input("task", RawInput::Text("go".into()));
    (request, env)
}

fn file(path: &str) -> CodeRef {
    CodeRef::File(path.into())
}

/// Every kind of code a graph can name, all by file, beside a blueprint.
fn every_use() -> RunGraph {
    let mut g = graph();
    g.output = Some(OutputDef {
        validator: Some(file("check.rhai")),
        ..OutputDef::default()
    });
    g.stages[0].hooks.on_stage_enter = Some(file("hooks.rhai"));
    g.stages[0].hooks.on_stage_exit = Some(file("hooks.rhai"));
    g.stages[1].hooks.after_inference = Some(file("check.rhai"));
    g.stages[1].output = Some(OutputDef {
        validator: Some(file("stage_check.rhai")),
        ..OutputDef::default()
    });
    g.layout.regions[0].kind = RegionKind::Custom {
        code: file("region.rhai"),
        pinned: false,
    };
    let mut own = g.layout.clone();
    own.regions[1].seed = Some(Seed::Code(file("seed.rhai")));
    g.stages[1].layout = Some(own);
    g.mime_types.insert(
        n("image/png"),
        MimeRowDef {
            check: Some(file("png.rhai")),
            ..MimeRowDef::default()
        },
    );
    g.dependencies.push(DependencyDef {
        name: "tool".into(),
        needs: Needs::Check(file("probe.rhai")),
        required: true,
        remedy: None,
        description: None,
        install: Some(InstallDef {
            script: Some(file("install.rhai")),
            ..InstallDef::default()
        }),
    });
    g
}

const EVERY_FILE: &[(&str, &str)] = &[
    ("check.rhai", "fn validate(x) {}"),
    ("hooks.rhai", "fn on_stage_enter() {}"),
    ("stage_check.rhai", "fn validate(y) {}"),
    ("region.rhai", "fn render() {}"),
    ("seed.rhai", "fn seed() {}"),
    ("png.rhai", "fn check(b) {}"),
    ("probe.rhai", "fn probe() {}"),
    ("install.rhai", "fn install() {}"),
];

#[tokio::test]
async fn every_piece_of_code_is_read_once_and_stored_by_digest() {
    let (mut request, env) = blueprint_request(every_use(), EVERY_FILE);
    request.output = Some(OutputDef {
        validator: Some(CodeRef::Inline("fn validate(z) {}".into())),
        ..OutputDef::default()
    });
    let resolved = spawn(&request, &env).await.unwrap();
    let spec = &resolved.spec;
    assert_eq!(spec.code.len(), 9, "{:?}", spec.code);
    assert_eq!(resolved.code.len(), 9);
    for (path, text) in EVERY_FILE {
        let digest = Digest::of(text.as_bytes());
        assert_eq!(spec.code_digest(&file(path)), Some(&digest), "{path}");
        assert_eq!(resolved.code[&digest], text.as_bytes());
    }
    assert_eq!(resolved.spec.seeded["task"].text, "code saw 1 inputs\n\ngo");
}

#[tokio::test]
async fn code_that_will_not_read_or_compile_is_named_where_it_is_used() {
    let files = [
        ("check.rhai", "broken"),
        ("hooks.rhai", "nohook"),
        ("stage_check.rhai", "fn validate(y) {}"),
        ("seed.rhai", "fn seed() {}"),
        ("png.rhai", "fn check(b) {}"),
        ("probe.rhai", "fn probe() {}"),
        ("install.rhai", "fn install() {}"),
    ];
    let (request, env) = blueprint_request(every_use(), &files);
    let issues = spawn(&request, &env).await.unwrap_err();
    assert_eq!(
        found(&issues),
        [
            "source.blueprint.output.validator Invalid",
            "source.blueprint.stages.build.hooks.after_inference Invalid",
            "source.blueprint.stages.plan.hooks.on_stage_enter Invalid",
            "source.blueprint.layout.regions[0].kind Unresolvable",
        ]
    );
    assert_eq!(issues.0[3].message, "cannot read 'region.rhai'");
}

#[tokio::test]
async fn a_raw_graph_cannot_name_code_by_file() {
    let mut g = graph();
    g.stages[0].hooks.on_stage_enter = Some(file("../../etc/passwd"));
    g.stages[1].hooks.on_error = Some(CodeRef::Inline("fn on_error() {}".into()));
    let env = Fake::default();
    let issues = spawn(&raw(g), &env).await.unwrap_err();
    assert_eq!(
        found(&issues),
        ["source.raw.stages.plan.hooks.on_stage_enter NotAllowed"]
    );
    let issue = &issues.0[0];
    assert!(issue.hint.as_deref().unwrap().contains("inline"), "{issue}");
    assert_eq!(issue.got.as_deref(), Some("the file \"../../etc/passwd\""));
}

#[tokio::test]
async fn a_request_output_validator_is_code_like_any_other() {
    let mut request = raw(graph());
    request.output = Some(OutputDef {
        validator: Some(file("mine.rhai")),
        ..OutputDef::default()
    });
    let issues = spawn(&request, &Fake::default()).await.unwrap_err();
    assert_eq!(found(&issues), ["output.validator NotAllowed"]);
}

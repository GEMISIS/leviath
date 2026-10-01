//! The sub-agent tools against a scripted host: what each call sends, and
//! every way a call is refused before it reaches the host.

use super::*;
use leviath_runtime::spec::request::SpawnSource;
use serde_json::json;

/// A handle on `workdir` whose host is gone, so a call that gets as far as
/// sending fails fast with "shutting down".
fn gone_host_in(workdir: &std::path::Path) -> SubAgentHandle {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    drop(rx);
    SubAgentHandle {
        workdir: workdir.to_string_lossy().to_string(),
        ..handle_with(tx)
    }
}

/// `spawn_agent`'s arguments for `blueprint` (a name or a directory) with
/// `task` as its task input.
fn spawn_args(blueprint: &str, task: &str) -> serde_json::Value {
    json!({"source": {"blueprint": blueprint}, "inputs": {"task": task}})
}

/// [`spawn_args`] for the blueprint in `dir`.
fn bp_args(dir: &tempfile::TempDir, task: &str) -> serde_json::Value {
    spawn_args(dir.path().to_str().unwrap(), task)
}

/// The escalation this closes: `write_file` is confined to the workdir, but
/// the spawner was not, so a model could author `x/agent.toml` in its own
/// workspace and spawn it, and that manifest's command seeds ran on the host
/// before the child's first inference. A bare name is an installed
/// blueprint, never a directory in the workspace, so only paths can reach
/// the workspace, and each one that does is refused.
#[tokio::test]
async fn spawn_refuses_a_blueprint_the_agent_could_have_written() {
    let work = tempfile::tempdir().unwrap();
    let planted = work.path().join("x");
    std::fs::create_dir(&planted).unwrap();
    std::fs::write(
        planted.join("agent.toml"),
        r#"[blueprint]
name = "x"
version = "0.1.0"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-4-6" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();
    let h = gone_host_in(work.path());
    for bad in [
        planted.to_string_lossy().to_string(),
        "./x".to_string(),
        "x/agent.toml".to_string(),
    ] {
        let out = spawn(&h, &spawn_args(&bad, "go"), None).await;
        assert!(
            out.contains("own working directory"),
            "{bad} must be refused: {out}"
        );
        assert!(out.contains("1. source.blueprint: not allowed"), "{out}");
    }
    // A bare name never looks in the workspace: it reaches the host.
    let out = spawn(&h, &spawn_args("x", "go"), None).await;
    assert!(out.contains("shutting down"), "{out}");
}

/// A blueprint from outside the workspace is taken as named: an installed
/// one by name or `{name, digest}`, or a directory a person chose, read as
/// its directory even when the manifest file itself is named.
#[test]
fn a_spawn_names_its_blueprint_by_name_reference_or_directory() {
    let work = tempfile::tempdir().unwrap();
    let elsewhere = temp_blueprint();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let h = SubAgentHandle {
        workdir: work.path().to_string_lossy().to_string(),
        ..handle_with(tx)
    };
    let source = |blueprint: serde_json::Value| {
        SpawnCall::parse(&json!({"source": {"blueprint": blueprint}}))
            .unwrap()
            .into_request(&h, Vec::new())
            .map(|r| r.source)
    };
    let dir = elsewhere.path().to_string_lossy().into_owned();
    let manifest = elsewhere.path().join("agent.toml");
    for named in [json!(dir), json!(manifest.to_string_lossy())] {
        match source(named).unwrap() {
            SpawnSource::BlueprintFile(path) => assert_eq!(path.path(), elsewhere.path()),
            other => panic!("a directory names a blueprint file: {other:?}"),
        }
    }
    let digest = "ab".repeat(32);
    match source(json!({"name": "coder", "digest": digest})).unwrap() {
        SpawnSource::Blueprint(r) => {
            assert_eq!(r.name.as_str(), "coder");
            assert_eq!(r.digest.unwrap().as_str(), digest);
        }
        other => panic!("{other:?}"),
    }
    match source(json!("coder")).unwrap() {
        SpawnSource::Blueprint(r) => assert_eq!(r.to_string(), "coder"),
        other => panic!("{other:?}"),
    }
    // A whole graph, as `spawn_schema` describes one, runs as written.
    let manifest = std::fs::read_to_string(elsewhere.path().join("agent.toml")).unwrap();
    let graph = leviath_blueprint::BlueprintFile::parse(&manifest)
        .unwrap()
        .run_graph();
    let request = SpawnCall::parse(&json!({"source": {"graph": graph}}))
        .unwrap()
        .into_request(&h, Vec::new())
        .unwrap();
    assert_eq!(request.source, SpawnSource::Raw(Box::new(graph)));
}

/// Every shape of `source` that does not name one thing to run is refused at
/// `source`, saying what it takes; a bad name, digest or graph is refused at
/// its own path. A relative path that is outside the workspace is still a
/// path, and is refused only if it does not read as a directory name.
#[test]
fn a_source_that_does_not_name_one_thing_is_refused_at_its_path() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let h = handle_with(tx);
    let refused = |args: serde_json::Value| {
        let issues = SpawnCall::parse(&args)
            .unwrap()
            .into_request(&h, Vec::new())
            .unwrap_err();
        refusal("spawn_agent", &issues)
    };
    for (source, says) in [
        (json!("coder"), "1. source: wrong type"),
        (json!({}), "1. source: conflict"),
        (
            json!({"blueprint": "a", "graph": {}}),
            "1. source: conflict",
        ),
        (json!({"blueprnt": "a"}), "Known: blueprint, graph"),
        (json!({"blueprint": 7}), "1. source.blueprint: wrong type"),
        (json!({"blueprint": ""}), "1. source.blueprint: invalid"),
        (
            json!({"blueprint": "/a\u{7}b"}),
            "1. source.blueprint: invalid",
        ),
        (
            json!({"blueprint": {"name": "a", "pin": "x"}}),
            "1. source.blueprint: invalid",
        ),
        (json!({"graph": {"stagez": []}}), "1. source.graph: invalid"),
    ] {
        let out = refused(json!({ "source": source }));
        assert!(out.contains(says), "{source}: {out}");
        assert!(out.contains(". Fix: "), "{out}");
    }
    let out = refused(json!({"source": {"graph": {"stagez": []}}}));
    assert!(out.contains("spawn_schema"), "{out}");
}

/// The arguments the old tool took are refused whole, naming the shape the
/// tool takes now, rather than half-read.
#[tokio::test]
async fn the_old_blueprint_and_task_arguments_are_refused_with_the_new_shape() {
    let (h, seen, _t) = fake_host(Ok("child-1".to_string()), vec![], false);
    for old in [
        json!({"blueprint": "coder", "task": "t"}),
        json!({"source": {"blueprint": "coder"}, "seed_context": "x"}),
        json!({"source": {"blueprint": "coder"}, "output_format": "json"}),
        json!({"source": {"blueprint": "coder"}, "output": {"format": "json", "shape": 1}}),
        json!({"task": "t"}),
        json!("spawn coder"),
    ] {
        let out = handle(&h, &tc("spawn_agent", old.clone())).await;
        assert!(
            out.starts_with("[error] spawn_agent refused: 1 problem."),
            "{old}: {out}"
        );
        assert!(
            out.contains("1. (request): invalid: the arguments do not read"),
            "{out}"
        );
        assert!(
            out.contains("\"inputs\": {\"task\""),
            "names the shape: {out}"
        );
    }
    assert!(seen.lock().unwrap().is_empty(), "nothing reached the host");
}

/// A refusal is one numbered line per problem, at the argument the model
/// wrote, with what was expected, what arrived, a fix, and the names it may
/// pick from. An issue with no hint of its own gets the fix its kind calls
/// for.
#[test]
fn a_refusal_numbers_every_problem_with_its_path_and_fix() {
    use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
    let at = |s: &str| SpecPath::root().field(s);
    let raw = SpecPath::root()
        .field("source")
        .field("raw")
        .field("stages")
        .key("plan");
    let mut issues = SpawnIssues::new();
    issues.push(
        SpawnIssue::new(at("inputs").key("depth"), IssueCode::OutOfRange, "too deep")
            .expected("1 to 3")
            .got("the integer 9")
            .hint("pick a depth from 1 to 3"),
    );
    issues.push(SpawnIssue::new(raw, IssueCode::Dangling, "no such region").known(["task"]));
    let codes = [
        (IssueCode::Missing, "supply it"),
        (IssueCode::Unknown, "remove it, or check its spelling"),
        (IssueCode::Dangling, "refer to something the graph declares"),
        (IssueCode::Unresolvable, "name something this machine has"),
        (IssueCode::WrongType, "send a value of the expected type"),
        (IssueCode::OutOfRange, "inside the allowed range"),
        (IssueCode::Invalid, "correct the value"),
        (IssueCode::Duplicate, "a different name"),
        (IssueCode::Conflict, "keep only one"),
        (IssueCode::NotAllowed, "this run may not ask for it"),
        (IssueCode::Unavailable, "try again shortly"),
        (IssueCode::Changed, "start a fresh run"),
    ];
    for (code, _) in codes {
        issues.push(SpawnIssue::new(at("x"), code, "m").expected("e"));
    }
    issues.push(
        SpawnIssue::new(at("y"), IssueCode::Unknown, "m")
            .got("g")
            .known(["a"]),
    );
    let out = refusal("validate_spawn", &issues);
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("[error] validate_spawn refused: 15 problems."));
    assert_eq!(
        lines[1],
        "1. inputs.depth: out of range: too deep (expected 1 to 3; got the integer 9). \
         Fix: pick a depth from 1 to 3"
    );
    assert_eq!(
        lines[2],
        "2. source.graph.stages.plan: dangling reference: no such region. Fix: use one of the \
         known names below. Known: task"
    );
    for (i, (_, fix)) in codes.iter().enumerate() {
        assert!(lines[i + 3].contains(fix), "{}", lines[i + 3]);
        assert!(lines[i + 3].contains("(expected e)"), "{}", lines[i + 3]);
    }
    assert!(lines[15].contains("(got g). Fix: use one of the known names below. Known: a"));
    let one = refusal(
        "spawn_agent",
        &SpawnIssue::new(at("z"), IssueCode::Missing, "m").into(),
    );
    assert!(one.contains("refused: 1 problem."), "{one}");
}

/// An output shape travels as the parent wrote it, schema included; one with
/// nothing in it asks for nothing.
#[tokio::test]
async fn spawn_passes_every_part_of_a_requested_output_shape() {
    let (h, seen, _t) = fake_host(Ok("child-1".to_string()), vec![], false);
    let mut args = spawn_args("coder", "go");
    args["output"] = json!({
        "format": "json",
        "instructions": "one object",
        "example": "{}",
        "schema": {"type": "object"}
    });
    handle(&h, &tc("spawn_agent", args.clone())).await;
    args["output"] = json!({"format": "  "});
    handle(&h, &tc("spawn_agent", args)).await;
    let seen = seen.lock().unwrap();
    let output = seen[0].output.as_ref().unwrap();
    assert_eq!(output.example.as_deref(), Some("{}"));
    assert_eq!(
        output.schema.as_ref().unwrap().value(),
        &json!({"type": "object"})
    );
    assert!(seen[1].output.is_none());
}

/// A parent whose own `--model` does not read as a model reference cannot
/// pass it on, and the child is refused saying so.
#[tokio::test]
async fn a_parent_model_that_does_not_read_refuses_the_child() {
    let (mut h, _seen, _t) = fake_host(Ok("child-1".to_string()), vec![], false);
    h.model_override = Some("/".to_string());
    let out = handle(&h, &tc("spawn_agent", spawn_args("coder", "go"))).await;
    assert!(out.contains("do not carry over to a child"), "{out}");
}

/// `validate_spawn` takes `spawn_agent`'s arguments, refuses the same way,
/// and answers with the summary of the run it would start, or the host's
/// issues.
#[tokio::test]
async fn validate_spawn_answers_with_a_summary_or_every_issue() {
    let (h, seen, _t) = fake_host(Ok("child-1".to_string()), vec![], false);
    let out = handle(&h, &tc("validate_spawn", spawn_args("coder", "go"))).await;
    assert!(
        out.starts_with("Valid: spawn_agent with these arguments"),
        "{out}"
    );
    assert!(out.contains("\"title\": \"checked\""), "{out}");
    assert_eq!(
        seen.lock().unwrap()[0].task,
        "go",
        "the host saw the request"
    );
    let out = handle(&h, &tc("validate_spawn", json!({"blueprint": "coder"}))).await;
    assert!(out.starts_with("[error] validate_spawn refused"), "{out}");

    let (h, _seen, _t) = fake_host(Err("no such input".to_string()), vec![], false);
    let out = handle(&h, &tc("validate_spawn", spawn_args("coder", "go"))).await;
    assert!(
        out.contains("1. (request): invalid: no such input"),
        "{out}"
    );
    let out = handle(
        &dead_handle(),
        &tc("validate_spawn", spawn_args("coder", "go")),
    )
    .await;
    assert!(out.contains("shutting down"), "{out}");
    let (h, _t) = drop_host();
    let out = handle(&h, &tc("validate_spawn", spawn_args("coder", "go"))).await;
    assert!(out.contains("dropped the validate request"), "{out}");
}

/// The summary the fake host gives for a spawn that would start.
fn summary() -> leviath_runtime::spec::summary::SpawnSummary {
    use leviath_runtime::spec::names::{ModelId, ProviderName, StageName, ToolName};
    leviath_runtime::spec::summary::SpawnSummary {
        title: "checked".to_string(),
        origin: leviath_runtime::spec::run_spec::SpecOrigin::Raw,
        entry_stage: StageName::new("plan").unwrap(),
        stages: vec![leviath_runtime::spec::summary::StageSummary {
            stage: StageName::new("plan").unwrap(),
            provider: ProviderName::new("p").unwrap(),
            model: ModelId::new("m").unwrap(),
            tools: vec![ToolName::new("read_file").unwrap()],
        }],
        inputs: Default::default(),
        launch: leviath_runtime::spec::launch::LaunchPolicy::top_level(
            &Default::default(),
            1,
            false,
        ),
        workdir: std::path::PathBuf::from("/w"),
    }
}

/// The history the fake host gives for any run it will read: two edges
/// taken, and the run finished with an answer.
fn history() -> leviath_runtime::host::RunHistory {
    use leviath_runtime::spec::names::StageName;
    use leviath_runtime::state::{TransitionReason, TransitionRecord};
    let edge = |from: &str, to: &str| TransitionRecord {
        from: StageName::new(from).unwrap(),
        to: StageName::new(to).unwrap(),
        edge: None,
        reason: TransitionReason::Condition,
        visit: "v".to_string(),
    };
    let mut state = leviath_runtime::state::RunState::initial(
        StageName::new("build").unwrap(),
        Default::default(),
        true,
    );
    state.seq = 7;
    state.status = leviath_runtime::state::RunStatus::Complete;
    leviath_runtime::host::RunHistory {
        summary: summary(),
        state,
        last_seq: 9,
        transitions: vec![(3, edge("plan", "build")), (5, edge("build", "plan"))],
    }
}

/// What a child's request asked for, as the tests read it.
#[derive(Debug, Clone)]
struct SeenSpawn {
    task: String,
    inputs: std::collections::BTreeMap<String, leviath_runtime::spec::inputs::RawInput>,
    max_depth: Option<usize>,
    yolo: bool,
    yolo_profile: Option<String>,
    allow: Vec<String>,
    parts: Vec<leviath_runtime::spec::request::Attachment>,
    model: Option<String>,
    output: Option<leviath_runtime::spec::graph::OutputDef>,
}

impl SeenSpawn {
    fn of(request: &leviath_runtime::spec::request::SpawnRequest) -> Self {
        use leviath_runtime::spec::launch::Unattended;
        Self {
            task: serde_json::to_value(&request.inputs).unwrap()["task"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            max_depth: request.launch.max_depth.map(usize::from),
            yolo: request.launch.unattended != Unattended::Off,
            yolo_profile: match &request.launch.unattended {
                Unattended::Profile(p) => Some(p.to_string()),
                _ => None,
            },
            allow: request
                .launch
                .allow
                .iter()
                .map(ToString::to_string)
                .collect(),
            parts: request.attachments.clone(),
            inputs: request.inputs.clone(),
            model: request.model.as_ref().map(ToString::to_string),
            output: request.output.clone(),
        }
    }
}

fn handle_with(sender: UnboundedSender<SubAgentOp>) -> SubAgentHandle {
    SubAgentHandle {
        offered_parts: Arc::new(std::sync::Mutex::new(Vec::new())),
        mime: None,
        sender,
        parent_run_id: "parent".to_string(),
        // This crate's own directory, deliberately *not* the system temp
        // dir: `temp_blueprint()` writes under temp, and on Linux that is
        // `/tmp` - so a workdir of `/tmp` made every fixture blueprint look
        // like one the agent had planted in its own workspace, and the
        // containment guard refused them all. macOS puts tempdirs under
        // `$TMPDIR` in `/var/folders`, so nothing local caught it.
        workdir: env!("CARGO_MANIFEST_DIR").to_string(),
        no_seed_commands: false,
        unattended: false,
        yolo_profile: None,
        allow: Vec::new(),
        model_override: None,
        agents_dir: None,
    }
}

/// A `SubAgentHandle` whose host answers each op from plain canned values -
/// no per-call-site closures, so this single service loop is the only region
/// (covered collectively across the suite). `spawn_result` answers `Spawn`
/// and the received args are recorded into the returned `Vec` for assertions;
/// `statuses` answers successive `Check`s for *children* in order (`None`
/// once exhausted); `ok` answers `Send`/`Kill`. The caller ("parent") is
/// reported `Active` - see [`fake_host_with_parent`] to script it.
fn fake_host(
    spawn_result: Result<String, String>,
    statuses: Vec<Option<AgentStatus>>,
    ok: bool,
) -> (
    SubAgentHandle,
    std::sync::Arc<std::sync::Mutex<Vec<SeenSpawn>>>,
    tokio::task::JoinHandle<()>,
) {
    fake_host_with_parent(spawn_result, statuses, ok, Some(AgentStatus::Active))
}

/// [`fake_host`] with the child's submitted answer scripted too, so the
/// "return its final result" half of `check`/`wait` can be exercised.
fn fake_host_with_output(
    statuses: Vec<Option<AgentStatus>>,
    output: Option<leviath_core::output::FinalOutput>,
) -> (
    SubAgentHandle,
    std::sync::Arc<std::sync::Mutex<Vec<SeenSpawn>>>,
    tokio::task::JoinHandle<()>,
) {
    fake_host_full(
        Ok("child-1".to_string()),
        statuses,
        false,
        Some(AgentStatus::Active),
        output,
    )
}

/// [`fake_host`] with the calling agent's own status scripted too.
fn fake_host_with_parent(
    spawn_result: Result<String, String>,
    statuses: Vec<Option<AgentStatus>>,
    ok: bool,
    parent_status: Option<AgentStatus>,
) -> (
    SubAgentHandle,
    std::sync::Arc<std::sync::Mutex<Vec<SeenSpawn>>>,
    tokio::task::JoinHandle<()>,
) {
    fake_host_full(spawn_result, statuses, ok, parent_status, None)
}

/// The one fake behind the three wrappers above.
fn fake_host_full(
    spawn_result: Result<String, String>,
    statuses: Vec<Option<AgentStatus>>,
    ok: bool,
    parent_status: Option<AgentStatus>,
    child_output: Option<leviath_core::output::FinalOutput>,
) -> (
    SubAgentHandle,
    std::sync::Arc<std::sync::Mutex<Vec<SeenSpawn>>>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_task = seen.clone();
    let task = tokio::spawn(async move {
        let mut checks = statuses.into_iter();
        while let Some(op) = rx.recv().await {
            match op {
                SubAgentOp::Spawn { reply, request, .. } => {
                    seen_task.lock().unwrap().push(SeenSpawn::of(&request));
                    let answer = spawn_result
                        .clone()
                        .map(|id| leviath_runtime::spec::names::RunId::new(id).unwrap())
                        .map_err(|e| {
                            leviath_runtime::spec::issues::SpawnIssue::new(
                                leviath_runtime::spec::issues::SpecPath::root(),
                                leviath_runtime::spec::issues::IssueCode::Invalid,
                                e,
                            )
                            .into()
                        });
                    let _ = reply.send(answer);
                }
                SubAgentOp::Validate { reply, request, .. } => {
                    seen_task.lock().unwrap().push(SeenSpawn::of(&request));
                    let answer = spawn_result.clone().map(|_| summary()).map_err(|e| {
                        leviath_runtime::spec::issues::SpawnIssue::new(
                            leviath_runtime::spec::issues::SpecPath::root(),
                            leviath_runtime::spec::issues::IssueCode::Invalid,
                            e,
                        )
                        .into()
                    });
                    let _ = reply.send(answer);
                }
                SubAgentOp::History { reply, run_id, .. } => {
                    let _ = reply.send(match run_id.as_str() {
                        "stranger" => Err("'stranger' is not this run".to_string()),
                        _ => Ok(history()),
                    });
                }
                // `wait` polls the *caller* as well as the child (to bail out
                // if the caller was itself cancelled), so the scripted queue
                // answers only for children - the caller is reported Active
                // unless a test scripts it otherwise.
                SubAgentOp::Check { reply, run_id } if run_id == "parent" => {
                    let _ = reply.send(parent_status.clone().map(|status| SubAgentReport {
                        status,
                        final_output: None,
                    }));
                }
                SubAgentOp::Check { reply, .. } => {
                    let _ = reply.send(checks.next().flatten().map(|status| SubAgentReport {
                        status,
                        final_output: child_output.clone(),
                    }));
                }
                SubAgentOp::Send { reply, .. } => {
                    let _ = reply.send(ok);
                }
                SubAgentOp::Kill { reply, .. } => {
                    let _ = reply.send(ok);
                }
            }
        }
    });
    (handle_with(tx), seen, task)
}

/// A host that drops every op without replying - the handler then sees a
/// dropped oneshot.
fn drop_host() -> (SubAgentHandle, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        while let Some(op) = rx.recv().await {
            drop(op);
        }
    });
    (handle_with(tx), task)
}

/// A handle whose host is already gone (sends fail immediately).
fn dead_handle() -> SubAgentHandle {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    handle_with(tx)
}

/// Write a minimal valid blueprint into a temp dir and return that dir (whose
/// path `find_blueprint` resolves to `<dir>/agent.toml`).
fn temp_blueprint() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("agent.toml"),
        r#"[blueprint]
name = "child"
version = "0.1.0"
description = "child"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-4-6" }] }

[graph.layout]
regions = [{ name = "task", kind = "pinned", budget = 1000 }]
total_budget_tokens = 1000

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
binds = [{ region = "task" }]
"#,
    )
    .unwrap();
    dir
}

fn tc(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "1".to_string(),
        name: name.to_string(),
        arguments: args,
        thought_signature: None,
    }
}

#[test]
fn is_subagent_tool_recognizes_every_subagent_name() {
    for name in SUBAGENT_TOOLS {
        assert!(is_subagent_tool(name));
    }
    assert!(!is_subagent_tool("read_file"));
}

#[test]
fn label_and_terminal_cover_all_statuses() {
    assert_eq!(label(&AgentStatus::Idle), "idle");
    assert_eq!(label(&AgentStatus::Active), "active");
    assert_eq!(label(&AgentStatus::Paused), "paused");
    assert_eq!(label(&AgentStatus::Waiting), "waiting");
    assert_eq!(label(&AgentStatus::Complete), "complete");
    assert_eq!(label(&AgentStatus::Cancelled), "cancelled");
    assert_eq!(
        label(&AgentStatus::Error {
            message: "boom".to_string()
        }),
        "error: boom"
    );
    for s in [AgentStatus::Active, AgentStatus::Waiting, AgentStatus::Idle] {
        assert!(!is_terminal(&s));
    }
    for s in [
        AgentStatus::Complete,
        AgentStatus::Cancelled,
        AgentStatus::Error {
            message: "x".to_string(),
        },
    ] {
        assert!(is_terminal(&s));
    }
}

#[tokio::test]
async fn spawn_sends_the_typed_inputs_and_reports_the_child_id() {
    let bp = temp_blueprint();
    let (h, seen, t) = fake_host(Ok("child-123".to_string()), vec![], false);
    let out = handle(
        &h,
        &tc(
            "spawn_agent",
            json!({
                "source": {"blueprint": bp.path().to_str().unwrap()},
                "inputs": {"task": "do it", "depth": 2},
                "max_child_depth": 2
            }),
        ),
    )
    .await;
    assert!(out.contains("Spawned sub-agent 'child-123'"));
    // Drop the handle and drain the host task - covers the loop's exit.
    drop(h);
    t.await.unwrap();
    // The inputs travel as typed values, nothing folded into the task.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].task, "do it");
    assert_eq!(
        seen[0].inputs["depth"],
        leviath_runtime::spec::inputs::RawInput::Int(2)
    );
    assert_eq!(seen[0].max_depth, Some(2));
}

/// A child of an unattended parent is unattended. Spawned attended it stops
/// at its first approval prompt with nobody there to answer, and parks the
/// parent behind it for good.
#[tokio::test]
async fn spawn_hands_the_parents_unattended_setting_to_the_child() {
    for unattended in [false, true] {
        let bp = temp_blueprint();
        let (mut h, seen, _t) = fake_host(Ok("child-1".to_string()), vec![], false);
        h.unattended = unattended;
        let out = handle(&h, &tc("spawn_agent", bp_args(&bp, "go"))).await;
        assert!(out.contains("Spawned sub-agent"), "{out}");
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].yolo, unattended,
            "a child inherits the parent's unattended setting"
        );
    }
}

/// `parts` hands the child files the parent holds: read from the parent's
/// store by name or hash prefix, typed and delivered as the parent's part
/// was, and refused by name when the parent has no such part or no store.
#[tokio::test]
async fn spawn_hands_named_parts_to_the_child() {
    use leviath_core::mime::{Blob, BlobStore, Delivery, MimeRegistry, MimeType, Part};
    let bp = temp_blueprint();
    let (mut h, seen, _t) = fake_host(Ok("child-1".to_string()), vec![], false);
    let store = std::sync::Arc::new(leviath_core::mime::MemoryBlobStore::new());
    let registry = MimeRegistry::builtin();
    let png = Blob::new(
        MimeType::parse("image/png").unwrap(),
        b"\x89PNG\r\n\x1a\nhero".to_vec(),
    );
    let stored = store.put("parent", &png, &registry).unwrap();
    let other = Blob::new(MimeType::parse("image/png").unwrap(), b"other".to_vec());
    let unnamed = store.put("parent", &other, &registry).unwrap();
    let sha = unnamed.sha256.clone();
    let lost = leviath_core::mime::BlobRef {
        sha256: "e".repeat(64),
        ..stored.clone()
    };
    *h.offered_parts.lock().unwrap() = vec![
        Part::text("words"),
        Part::stored(stored)
            .named("hero.png")
            .delivered(Delivery::Text),
        Part::stored(unnamed),
        Part::stored(lost).named("lost.png"),
    ];
    h.mime = Some(std::sync::Arc::new(leviath_tools::ToolMime {
        store,
        registry: std::sync::Arc::new(leviath_core::mime::RegistryCell::new(std::sync::Arc::new(
            registry,
        ))),
        run_id: "parent".to_string(),
        max_part_bytes: 1024,
    }));
    let prefix: String = sha.chars().take(8).collect();
    let out = handle(
        &h,
        &tc(
            "spawn_agent",
            json!({
                "source": {"blueprint": bp.path().to_str().unwrap()},
                "inputs": {"task": "edit @hero.png"},
                "parts": ["hero.png", prefix]
            }),
        ),
    )
    .await;
    assert!(out.contains("Spawned sub-agent"), "{out}");
    {
        let seen = seen.lock().unwrap();
        let parts = &seen[0].parts;
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name, "hero.png");
        assert_eq!(parts[0].mime_type.as_ref().unwrap().as_str(), "image/png");
        assert_eq!(parts[0].deliver, Some(Delivery::Text));
        assert_eq!(parts[0].data.0, b"\x89PNG\r\n\x1a\nhero");
        // The unnamed part is named by its hash.
        assert_eq!(parts[1].name, sha.chars().take(12).collect::<String>());
        assert_eq!(parts[1].data.0, b"other");
        assert!(parts[1].deliver.is_none());
    }

    // The stage's limit for spawn_agent: a part outside it refuses the
    // spawn by name, one inside it goes through.
    let audio_only = ["audio/*".to_string()];
    let out = handle_within(
            &h,
            &tc(
                "spawn_agent",
                json!({"source": {"blueprint": bp.path().to_str().unwrap()}, "inputs": {"task": "go"}, "parts": ["hero.png"]}),
            ),
            Some(&audio_only),
        )
        .await;
    assert!(out.contains("1. parts: invalid: "), "{out}");
    assert!(
        out.contains(
            "'hero.png' is image/png; at this stage spawn_agent may be handed only audio/*"
        ),
        "{out}"
    );
    let images = ["image/*".to_string()];
    let out = handle_within(
            &h,
            &tc(
                "spawn_agent",
                json!({"source": {"blueprint": bp.path().to_str().unwrap()}, "inputs": {"task": "go"}, "parts": ["hero.png"]}),
            ),
            Some(&images),
        )
        .await;
    assert!(out.contains("Spawned sub-agent"), "{out}");
    // A name the parent holds no part under, and a part whose bytes the
    // store has lost, each refuse the spawn by name.
    for (wanted, says) in [
        ("nope.png", "names no stored part"),
        ("lost.png", "could not be read from the store"),
    ] {
        let out = handle(
                &h,
                &tc(
                    "spawn_agent",
                    json!({"source": {"blueprint": bp.path().to_str().unwrap()}, "inputs": {"task": "go"}, "parts": [wanted]}),
                ),
            )
            .await;
        assert!(out.contains("1. parts: invalid: "), "{out}");
        assert!(out.contains(says), "{out}");
    }
    // No store at all: the argument is refused outright. Nothing named:
    // nothing handed on.
    h.mime = None;
    let out = handle(
            &h,
            &tc(
                "spawn_agent",
                json!({"source": {"blueprint": bp.path().to_str().unwrap()}, "inputs": {"task": "go"}, "parts": ["hero.png"]}),
            ),
        )
        .await;
    assert!(out.contains("no blob store"), "{out}");
    assert!(parts_for_child(&h, &[], None).unwrap().is_empty());
}

/// A run's `--model` covers the children it spawns as well. The child is
/// named by the model at run time, so it is part of this run rather than a
/// separate one, and dropping the override there is silent.
#[tokio::test]
async fn spawn_hands_the_parents_model_override_to_the_child() {
    let bp = temp_blueprint();
    let (mut h, seen, _t) = fake_host(Ok("child-1".to_string()), vec![], false);
    h.model_override = Some("cerebras/gpt-oss-120b".to_string());
    let out = handle(&h, &tc("spawn_agent", bp_args(&bp, "go"))).await;
    assert!(out.contains("Spawned sub-agent"), "{out}");
    assert_eq!(
        seen.lock().unwrap()[0].model.as_deref(),
        Some("cerebras/gpt-oss-120b")
    );
}

/// A run with no override leaves the child on its own blueprint's models.
#[tokio::test]
async fn spawn_without_an_override_leaves_the_child_to_its_blueprint() {
    let bp = temp_blueprint();
    let (h, seen, _t) = fake_host(Ok("child-1".to_string()), vec![], false);
    let out = handle(&h, &tc("spawn_agent", bp_args(&bp, "go"))).await;
    assert!(out.contains("Spawned sub-agent"), "{out}");
    assert!(seen.lock().unwrap()[0].model.is_none());
}

#[tokio::test]
async fn spawn_with_wait_blocks_until_the_child_finishes() {
    let bp = temp_blueprint();
    // Active on the first poll, Complete after.
    let (h, _seen, _t) = fake_host(
        Ok("child-1".to_string()),
        vec![Some(AgentStatus::Active), Some(AgentStatus::Complete)],
        false,
    );
    let out = handle(
            &h,
            &tc(
                "spawn_agent",
                json!({"source": {"blueprint": bp.path().to_str().unwrap()}, "inputs": {"task": "t"}, "wait": true }),
            ),
        )
        .await;
    assert!(out.contains("finished with status: complete"));
}

/// `wait_for_agent` gives up when the *calling* agent is cancelled. The loop
/// has no other exit, so a cancelled caller would otherwise poll for a child
/// that is being torn down with it until the daemon exits.
#[tokio::test]
async fn wait_gives_up_when_the_calling_agent_is_cancelled() {
    let (h, _seen, _t) = fake_host_with_parent(
        Ok("child-1".to_string()),
        // The child never finishes on its own.
        vec![Some(AgentStatus::Active); 8],
        false,
        Some(AgentStatus::Cancelled),
    );
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        handle(&h, &tc("wait_for_agent", json!({ "agent_id": "child-1" }))),
    )
    .await
    .expect("the wait returns instead of polling forever");
    assert!(
        out.contains("cancelled while waiting"),
        "reports why it stopped, got: {out}"
    );
}

/// `wait_for_agent` waits off the tool lane.
///
/// The child's own tool batches queue on that lane. A parent that kept lane
/// capacity for the length of the wait would hold exactly what the child
/// needs in order to finish, so a factory of parents waiting on children
/// wedges itself and stays wedged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_does_not_hold_the_tool_lane() {
    use leviath_runtime::tool_bridge::{ToolJob, ToolLane, ToolLaneStats};

    // The child stays busy for several polls - long enough that the parent is
    // demonstrably parked - and then finishes, so the wait is exercised to its
    // end rather than abandoned mid-await.
    let mut statuses = vec![Some(AgentStatus::Active); 6];
    statuses.push(Some(AgentStatus::Complete));
    let (h, _seen, _t) = fake_host(Ok("child-1".to_string()), statuses, false);

    let (job_tx, job_rx) = tokio::sync::mpsc::unbounded_channel();
    let (result_tx, mut results) = tokio::sync::mpsc::unbounded_channel();
    let stats = std::sync::Arc::new(ToolLaneStats::new(1));
    let lane = ToolLane::new(
        tokio::runtime::Handle::current(),
        result_tx,
        std::sync::Arc::new(tokio::sync::Notify::new()),
        1,
        stats.clone(),
    );
    let _serving = lane.serve(job_rx);
    let submit = |entity: u32, exec: leviath_runtime::tool_bridge::BoxedToolExec| {
        stats.enqueued();
        job_tx
            .send(ToolJob {
                entity: bevy_ecs::entity::Entity::from_raw_u32(entity)
                    .expect("a small index is a valid id"),
                exec,
                cancel: leviath_runtime::cancel::CancelToken::new(),
            })
            .expect("the lane is serving");
    };

    submit(
        1,
        Box::new(move || {
            Box::pin(async move {
                let out = handle(&h, &tc("wait_for_agent", json!({"agent_id": "child-1"}))).await;
                vec![("wait".to_string(), out.into())]
            })
        }),
    );
    // The waiter gives the lane back rather than sitting on it.
    leviath_testkit::wait_until("the wait stepped off the lane", || stats.parked() != 0).await;

    // Which is what lets anything else run - a child's tool batch, here.
    submit(
        2,
        Box::new(|| Box::pin(async { vec![("child".to_string(), "ran".into())] })),
    );
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), results.recv())
        .await
        .expect("the batch behind the waiter ran")
        .expect("an outcome arrived");
    assert_eq!(outcome.results, vec![("child".to_string(), "ran".into())]);

    // And the waiter takes a permit again and reports, once its child is done.
    let waited = tokio::time::timeout(std::time::Duration::from_secs(30), results.recv())
        .await
        .expect("the wait finished")
        .expect("an outcome arrived");
    assert_eq!(waited.results.len(), 1);
    // Bound first: an expression that only a *failing* assertion evaluates
    // is a region no passing run ever reaches.
    let reported = waited.results[0].1.clone();
    assert!(
        reported.contains("finished with status: complete"),
        "got: {reported}"
    );
}

/// A caller the host no longer knows about (daemon shutting down, or the
/// run already reaped) also ends the wait - there is nothing left to wait
/// for either way.
#[tokio::test]
async fn wait_gives_up_when_the_caller_is_unknown_to_the_host() {
    let (h, _seen, _t) = fake_host_with_parent(
        Ok("child-1".to_string()),
        vec![Some(AgentStatus::Active); 8],
        false,
        None, // the host has no such caller
    );
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        handle(&h, &tc("wait_for_agent", json!({ "agent_id": "child-1" }))),
    )
    .await
    .expect("the wait returns instead of polling forever");
    assert!(out.contains("cancelled while waiting"), "got: {out}");
}

#[tokio::test]
async fn spawn_reports_spawner_error_and_dead_host() {
    let bp = temp_blueprint();
    let (h, _seen, _t) = fake_host(Err("bad blueprint".to_string()), vec![], false);
    assert!(
        handle(&h, &tc("spawn_agent", bp_args(&bp, "t")))
            .await
            .contains("bad blueprint")
    );
    assert!(
        handle(&dead_handle(), &tc("spawn_agent", bp_args(&bp, "t")))
            .await
            .contains("shutting down")
    );
}

#[tokio::test]
async fn check_reports_status_or_missing() {
    let (h, _seen, _t) = fake_host(Ok(String::new()), vec![Some(AgentStatus::Active)], false);
    assert!(
        handle(&h, &tc("check_agent", json!({ "agent_id": "c" })))
            .await
            .contains("status: active")
    );
    let (h2, _seen2, _t2) = fake_host(Ok(String::new()), vec![], false);
    assert!(
        handle(&h2, &tc("check_agent", json!({ "agent_id": "c" })))
            .await
            .contains("no such sub-agent")
    );
    // A dead host: `status_of`'s send fails, so it returns `None` early.
    assert!(
        handle(
            &dead_handle(),
            &tc("check_agent", json!({ "agent_id": "c" }))
        )
        .await
        .contains("no such sub-agent")
    );
}

#[tokio::test]
async fn wait_requires_id_and_returns_when_terminal_or_missing() {
    assert!(
        handle(&dead_handle(), &tc("wait_for_agent", json!({})))
            .await
            .contains("requires 'agent_id'")
    );
    let (h, _seen, _t) = fake_host(
        Ok(String::new()),
        vec![Some(AgentStatus::Error {
            message: "boom".to_string(),
        })],
        false,
    );
    assert!(
        handle(&h, &tc("wait_for_agent", json!({ "agent_id": "c" })))
            .await
            .contains("error: boom")
    );
    let (h2, _seen2, _t2) = fake_host(Ok(String::new()), vec![], false);
    assert!(
        handle(&h2, &tc("wait_for_agent", json!({ "agent_id": "c" })))
            .await
            .contains("no such sub-agent")
    );
}

#[tokio::test]
async fn send_delivers_or_reports_failure() {
    let (h, _seen, _t) = fake_host(Ok(String::new()), vec![], true);
    assert!(
        handle(
            &h,
            &tc("send_to_agent", json!({ "agent_id": "c", "message": "hi" }))
        )
        .await
        .contains("Delivered message")
    );
    assert!(
        handle(&h, &tc("send_to_agent", json!({ "agent_id": "c" })))
            .await
            .contains("requires 'agent_id' and 'message'")
    );
    let (h2, _seen2, _t2) = fake_host(Ok(String::new()), vec![], false);
    assert!(
        handle(
            &h2,
            &tc("send_to_agent", json!({ "agent_id": "c", "message": "hi" }))
        )
        .await
        .contains("did not accept")
    );
    assert!(
        handle(
            &dead_handle(),
            &tc("send_to_agent", json!({ "agent_id": "c", "message": "hi" }))
        )
        .await
        .contains("shutting down")
    );
}

/// A host that answers only `Send`, plus what a test needs to assert on it.
struct SendRecordingHost {
    /// The handle under test.
    handle: SubAgentHandle,
    /// Each `Send` op's `target_region`, in arrival order.
    regions: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
    /// The service loop, joined at the end of the test.
    task: tokio::task::JoinHandle<()>,
}

/// A host that answers only `Send`, recording each op's `target_region`.
fn send_recording_host() -> SendRecordingHost {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let regions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let regions_task = regions.clone();
    let task = tokio::spawn(async move {
        while let Some(op) = rx.recv().await {
            match op {
                SubAgentOp::Send {
                    reply,
                    target_region,
                    ..
                } => {
                    regions_task.lock().unwrap().push(target_region);
                    let _ = reply.send(true);
                }
                // Any other op: drop it unanswered; callers see a dropped
                // oneshot, which every handler already tolerates.
                other => drop(other),
            }
        }
    });
    SendRecordingHost {
        handle: handle_with(tx),
        regions,
        task,
    }
}

/// `target_region` was schema-advertised and documented but never read on
/// this path; the host op now carries it. Absent and empty both mean the
/// documented default (conversation), so they forward as `None`.
#[tokio::test]
async fn send_forwards_target_region() {
    let SendRecordingHost {
        handle: h,
        regions,
        task,
    } = send_recording_host();
    for args in [
        json!({ "agent_id": "c", "message": "hi", "target_region": "notes" }),
        json!({ "agent_id": "c", "message": "hi" }),
        json!({ "agent_id": "c", "message": "hi", "target_region": "" }),
    ] {
        assert!(
            handle(&h, &tc("send_to_agent", args))
                .await
                .contains("Delivered message")
        );
    }
    assert_eq!(
        *regions.lock().unwrap(),
        vec![Some("notes".to_string()), None, None]
    );
    // A non-Send op goes through the recording host's drop arm.
    handle(&h, &tc("check_agent", json!({ "agent_id": "c" }))).await;
    // Closing the handle ends the host loop; the task exits cleanly.
    drop(h);
    task.await.unwrap();
}

#[tokio::test]
async fn kill_cancels_or_reports_missing() {
    let (h, _seen, _t) = fake_host(Ok(String::new()), vec![], true);
    assert!(
        handle(&h, &tc("kill_agent", json!({ "agent_id": "c" })))
            .await
            .contains("Killed sub-agent")
    );
    assert!(
        handle(&h, &tc("kill_agent", json!({})))
            .await
            .contains("requires 'agent_id'")
    );
    let (h2, _seen2, _t2) = fake_host(Ok(String::new()), vec![], false);
    assert!(
        handle(&h2, &tc("kill_agent", json!({ "agent_id": "c" })))
            .await
            .contains("no such sub-agent")
    );
    assert!(
        handle(
            &dead_handle(),
            &tc("kill_agent", json!({ "agent_id": "c" }))
        )
        .await
        .contains("shutting down")
    );
}

#[tokio::test]
async fn handle_rejects_a_non_subagent_tool() {
    assert!(
        handle(&dead_handle(), &tc("read_file", json!({})))
            .await
            .contains("is not a sub-agent tool")
    );
}

#[tokio::test]
async fn dropped_reply_paths_are_handled() {
    let (h, t) = drop_host();
    // status_of returns None on a dropped reply → "no such sub-agent".
    assert!(
        handle(&h, &tc("check_agent", json!({ "agent_id": "c" })))
            .await
            .contains("no such sub-agent")
    );
    assert!(
        handle(
            &h,
            &tc("send_to_agent", json!({ "agent_id": "c", "message": "m" }))
        )
        .await
        .contains("dropped the message")
    );
    assert!(
        handle(&h, &tc("kill_agent", json!({ "agent_id": "c" })))
            .await
            .contains("dropped the kill request")
    );
    let bp = temp_blueprint();
    assert!(
        handle(&h, &tc("spawn_agent", bp_args(&bp, "t")))
            .await
            .contains("dropped the spawn request")
    );
    drop(h);
    t.await.unwrap();
}

fn answer(text: &str) -> leviath_core::output::FinalOutput {
    leviath_core::output::FinalOutput::new(
        text,
        Some("markdown".to_string()),
        "fix_worker".to_string(),
        0,
    )
}

/// `wait_for_agent`'s schema has always said "block until a sub-agent
/// completes, then return its final result". It returned a status label and
/// nothing else, so a parent had to agree on a file path out of band to
/// receive any work at all.
#[tokio::test]
async fn wait_returns_the_childs_final_output() {
    let (h, _seen, _t) = fake_host_with_output(
        vec![Some(AgentStatus::Complete)],
        Some(answer("changed src/lib.rs and its test")),
    );
    let out = handle(&h, &tc("wait_for_agent", json!({"agent_id": "child-1"}))).await;
    assert!(out.contains("complete"), "{out}");
    assert!(out.contains("changed src/lib.rs and its test"), "{out}");
    assert!(out.contains("markdown"), "names the shape: {out}");
}

/// A child's files reach its parent: listed under the answer, and handed
/// up as parts stored under the parent's run and named after the child.
/// A file the store lost, one never stored (no hash) and one over the
/// ceiling are left out of the parts but still named in the text.
#[tokio::test]
async fn a_finished_childs_files_are_handed_up_as_parts() {
    use leviath_core::mime::{Blob, BlobStore, MimeRegistry, MimeType};
    let store = std::sync::Arc::new(leviath_core::mime::MemoryBlobStore::new());
    let registry = MimeRegistry::builtin();
    let png = Blob::new(
        MimeType::parse("image/png").unwrap(),
        b"\x89PNG\r\n\x1a\nhero".to_vec(),
    );
    let stored = store.put("child-1", &png, &registry).unwrap();
    let big = Blob::new(MimeType::parse("image/png").unwrap(), vec![0; 2048]);
    let too_big = store.put("child-1", &big, &registry).unwrap();
    let artifact = |name: &str, sha: String| leviath_core::output::Artifact {
        name: name.to_string(),
        path: format!("out/{name}.png"),
        mime_type: MimeType::parse("image/png").unwrap(),
        size: 12,
        sha256: sha,
    };
    let output = answer("drew the hero").with_artifacts(vec![
        artifact("hero", stored.sha256.clone()),
        artifact("lost", "e".repeat(64)),
        artifact("huge", too_big.sha256.clone()),
        leviath_core::output::Artifact::from_path("untracked.txt"),
    ]);
    let (mut h, _seen, _t) =
        fake_host_with_output(vec![Some(AgentStatus::Complete)], Some(output.clone()));
    h.mime = Some(std::sync::Arc::new(leviath_tools::ToolMime {
        store: store.clone(),
        registry: std::sync::Arc::new(leviath_core::mime::RegistryCell::new(std::sync::Arc::new(
            registry,
        ))),
        run_id: "parent".to_string(),
        max_part_bytes: 1024,
    }));
    let content = handle_content(
        &h,
        &tc("wait_for_agent", json!({"agent_id": "child-1"})),
        None,
    )
    .await;
    let text = content.as_str();
    assert!(text.contains("drew the hero"), "{text}");
    assert!(text.contains("--- files handed back ---"), "{text}");
    assert!(text.contains("- hero (image/png, 12 B)"), "{text}");
    assert!(text.contains("- untracked.txt"), "{text}");
    let stored_parts: Vec<_> = content.stored().collect();
    assert_eq!(stored_parts.len(), 1, "{text}");
    assert_eq!(stored_parts[0].name.as_deref(), Some("child-1/hero"));
    assert_eq!(stored_parts[0].blob().unwrap().sha256, stored.sha256);
    assert!(
        store.read("parent", &stored.sha256).is_ok(),
        "the bytes now sit under the parent's run"
    );

    // `check_agent` hands the same files up; a world with no store hands
    // up the text alone.
    let (mut h, _seen, _t) = fake_host_with_output(vec![Some(AgentStatus::Complete)], Some(output));
    h.mime = None;
    let content =
        handle_content(&h, &tc("check_agent", json!({"agent_id": "child-1"})), None).await;
    assert!(content.as_str().contains("- hero (image/png, 12 B)"));
    assert!(content.is_text_only());
}

#[tokio::test]
async fn check_returns_the_childs_final_output_once_it_is_done() {
    let (h, _seen, _t) = fake_host_with_output(
        vec![Some(AgentStatus::Complete)],
        Some(answer("all three tests pass")),
    );
    let out = handle(&h, &tc("check_agent", json!({"agent_id": "child-1"}))).await;
    assert!(out.contains("all three tests pass"), "{out}");
}

/// A child still working has nothing to report yet, so the status line
/// stands alone rather than claiming an empty answer.
#[tokio::test]
async fn check_on_a_running_child_reports_status_only() {
    let (h, _seen, _t) = fake_host_with_output(vec![Some(AgentStatus::Active)], None);
    let out = handle(&h, &tc("check_agent", json!({"agent_id": "child-1"}))).await;
    assert!(out.contains("active"), "{out}");
    assert!(!out.contains("final output"), "{out}");
}

/// A child whose answer hit the size limit says so, so the parent reads a
/// partial answer as partial rather than as everything the child had.
#[tokio::test]
async fn a_truncated_child_answer_is_marked_as_cut() {
    let mut cut = answer("the first part of a very long report");
    cut.truncated = true;
    let (h, _seen, _t) = fake_host_with_output(vec![Some(AgentStatus::Complete)], Some(cut));

    let out = handle(&h, &tc("wait_for_agent", json!({"agent_id": "child-1"}))).await;

    assert!(
        out.contains("the first part of a very long report"),
        "{out}"
    );
    assert!(out.contains("truncated at the size limit"), "{out}");
}

/// "produced no final output" is actionable - the parent can ask, or route
/// around it. A bare status line reads as success.
#[tokio::test]
async fn a_finished_child_that_submitted_nothing_says_so() {
    let (h, _seen, _t) = fake_host_with_output(vec![Some(AgentStatus::Complete)], None);
    let out = handle(&h, &tc("wait_for_agent", json!({"agent_id": "child-1"}))).await;
    assert!(out.contains("no final output"), "{out}");
}

/// A parent may ask its child for a shape. It travels as a label, so a
/// format nothing in this crate has heard of reaches the child intact.
#[tokio::test]
async fn spawn_passes_a_requested_output_shape_to_the_child() {
    let (h, seen, _t) = fake_host(Ok("child-1".to_string()), vec![], false);
    let dir = temp_blueprint();
    let _ = handle(
        &h,
        &tc(
            "spawn_agent",
            json!({
                "source": {"blueprint": dir.path().to_str().unwrap()},
                "inputs": {"task": "do it"},
                "output": {"format": "a2ui", "instructions": "One card per finding."},
            }),
        ),
    )
    .await;
    let args = seen.lock().unwrap();
    let spec = args[0]
        .output
        .as_ref()
        .expect("the request reached the child");
    assert_eq!(spec.format.as_deref(), Some("a2ui"));
    assert_eq!(spec.instructions.as_deref(), Some("One card per finding."));
}

/// A spawn that asks for nothing leaves the child's blueprint in charge.
#[tokio::test]
async fn spawn_without_output_args_requests_no_shape() {
    let (h, seen, _t) = fake_host(Ok("child-1".to_string()), vec![], false);
    let dir = temp_blueprint();
    let _ = handle(&h, &tc("spawn_agent", bp_args(&dir, "do it"))).await;
    assert!(seen.lock().unwrap()[0].output.is_none());
}

/// The profile travels with the bit: a child of a `careful` run is a
/// `careful` run, not a bare `--yolo` one.
#[tokio::test]
async fn spawn_hands_the_parents_yolo_profile_to_the_child() {
    let bp = temp_blueprint();
    let (mut h, seen, _t) = fake_host(Ok("child-1".to_string()), vec![], false);
    h.unattended = true;
    h.yolo_profile = Some("careful".to_string());
    let out = handle(&h, &tc("spawn_agent", bp_args(&bp, "go"))).await;
    assert!(out.contains("Spawned sub-agent"), "{out}");
    let seen = seen.lock().unwrap();
    assert!(seen[0].yolo);
    assert_eq!(seen[0].yolo_profile.as_deref(), Some("careful"));
}

/// A child asks for the tools its parent may call without asking, unless the
/// call names its own list; the host narrows either against the parent.
#[tokio::test]
async fn spawn_asks_for_the_parents_allowed_tools_unless_it_names_its_own() {
    let bp = temp_blueprint();
    let (mut h, seen, _t) = fake_host(Ok("child-1".to_string()), vec![], false);
    h.allow = vec!["write_file".to_string(), "shell".to_string()];
    let out = handle(&h, &tc("spawn_agent", bp_args(&bp, "go"))).await;
    assert!(out.contains("Spawned sub-agent"), "{out}");
    let mut own = bp_args(&bp, "go");
    own["allow"] = json!(["write_file"]);
    let out = handle(&h, &tc("spawn_agent", own)).await;
    assert!(out.contains("Spawned sub-agent"), "{out}");
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].allow, ["write_file", "shell"]);
    assert_eq!(seen[1].allow, ["write_file"]);
}

/// `spawn_schema` hands out the request one part at a time: the top level
/// with every part's name, then any part by that name. An unknown part
/// lists the real ones, and arguments it does not take are refused.
#[tokio::test]
async fn spawn_schema_answers_one_part_at_a_time() {
    let h = dead_handle();
    let top = handle(&h, &tc("spawn_schema", json!({}))).await;
    assert!(top.starts_with("The spawn request's top level."), "{top}");
    assert!(!top.contains("\"$defs\""), "the parts are not inlined");
    assert!(top.contains("Parts: ") && top.contains("RunGraph"), "{top}");
    let blank = handle(&h, &tc("spawn_schema", json!({"part": " "}))).await;
    assert_eq!(blank, top);
    let graph = handle(&h, &tc("spawn_schema", json!({"part": "RunGraph"}))).await;
    assert!(graph.starts_with("The part RunGraph."), "{graph}");
    assert!(graph.contains("\"stages\""), "{graph}");
    let missing = handle(&h, &tc("spawn_schema", json!({"part": "Nope"}))).await;
    assert!(missing.starts_with("[error] the spawn request has no part 'Nope'"));
    assert!(missing.contains("StageDef"), "{missing}");
    let bad = handle(&h, &tc("spawn_schema", json!({"parts": "x"}))).await;
    assert!(bad.starts_with("[error] spawn_schema takes"), "{bad}");
}

/// An installed blueprint whose task region takes the caller's task.
fn installed_blueprint(agents: &std::path::Path, name: &str) {
    let dir = agents.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("agent.toml"),
        format!(
            r#"[blueprint]
name = "{name}"
version = "1.2.3"
description = "writes code"

[graph]
entry = "plan"
edges = [{{ name = "next", from = "plan", to = "build" }}]

[[graph.stages]]
name = "plan"
description = "plan it"
model = {{ models = [{{ provider = "anthropic", model = "m" }}] }}

[[graph.stages]]
name = "build"
model = {{ models = [{{ provider = "anthropic", model = "m" }}] }}
mode = "interactive"

[graph.layout]
regions = [{{ name = "task", kind = "pinned", budget = 1000 }}]
total_budget_tokens = 1000

[[graph.inputs]]
name = "task"
type = {{ kind = "text", multiline = true }}
binds = [{{ region = "task" }}]
"#
        ),
    )
    .unwrap();
}

/// `describe_blueprint` says what an installed blueprint is for, its
/// stages, the inputs it declares and a call that would spawn it; a
/// blueprint that is not installed names the ones that are.
#[tokio::test]
async fn describe_blueprint_lists_stages_inputs_and_a_call() {
    let agents = tempfile::tempdir().unwrap();
    installed_blueprint(agents.path(), "coder");
    let mut h = dead_handle();
    h.agents_dir = Some(agents.path().to_path_buf());
    for named in [json!("coder"), json!({"name": "coder"})] {
        let out = handle(&h, &tc("describe_blueprint", json!({ "blueprint": named }))).await;
        let v: serde_json::Value = serde_json::from_str(&out).expect(&out);
        assert!(
            v["blueprint"].as_str().unwrap().starts_with("coder@"),
            "{out}"
        );
        assert_eq!(v["version"], "1.2.3");
        assert_eq!(v["description"], "writes code");
        assert_eq!(v["stages"][0]["name"], "plan");
        assert_eq!(v["stages"][0]["mode"], "autonomous");
        assert_eq!(v["stages"][1]["mode"], "interactive");
        assert_eq!(v["inputs"][0]["name"], "task");
        assert_eq!(v["spawn_with"]["source"]["blueprint"], "coder");
        assert_eq!(v["spawn_with"]["inputs"]["task"], "<task>");
    }
    let out = handle(&h, &tc("describe_blueprint", json!({"blueprint": "nope"}))).await;
    assert!(out.starts_with("[error] "), "{out}");
    assert!(
        out.contains("no blueprint named \"nope\"") && out.contains("coder"),
        "{out}"
    );
    for bad in [
        json!({}),
        json!({"blueprint": ""}),
        json!({"blueprint": {"nme": "x"}}),
    ] {
        let out = handle(&h, &tc("describe_blueprint", bad)).await;
        assert!(out.starts_with("[error] describe_blueprint takes"), "{out}");
    }
}

/// Every mode a stage can have reads as one word.
#[test]
fn every_stage_mode_has_a_word() {
    use leviath_runtime::spec::graph::{FanOutDef, StageMode};
    use leviath_runtime::spec::names::StageName;
    let words: Vec<&str> = [
        StageMode::Autonomous,
        StageMode::Interactive,
        StageMode::InteractivePoints(vec![]),
        StageMode::FanOut(FanOutDef::same_graph(StageName::new("w").unwrap())),
        StageMode::Output,
    ]
    .iter()
    .map(reads::mode_label)
    .collect();
    assert_eq!(
        words,
        [
            "autonomous",
            "interactive",
            "interactive_points",
            "fan_out",
            "output"
        ]
    );
}

/// `run_history` answers each view from the host's history, refuses a run
/// outside the caller's tree with the host's reason, and refuses
/// arguments it does not take.
#[tokio::test]
async fn run_history_answers_each_view() {
    let (h, _seen, _t) = fake_host(Ok("child-1".to_string()), vec![], false);
    let summary = handle(&h, &tc("run_history", json!({"run_id": "child-1"}))).await;
    let v: serde_json::Value = serde_json::from_str(&summary).expect(&summary);
    assert_eq!(v["run_id"], "child-1");
    assert_eq!(v["started_as"]["title"], "checked");
    assert_eq!(
        (v["step"].as_u64(), v["last_step"].as_u64()),
        (Some(7), Some(9))
    );
    assert_eq!(v["status"], "Complete");
    assert_eq!(v["stage"], "build");
    assert_eq!(v["transitions"], 2);
    assert_eq!(v["last_transition"]["step"], 5);
    let state = handle(
        &h,
        &tc(
            "run_history",
            json!({"run_id": "child-1", "view": "state", "at": 7}),
        ),
    )
    .await;
    assert!(
        state.starts_with("The state of 'child-1' at step 7 (its file records steps up to 9):"),
        "{state}"
    );
    assert!(state.contains("[state"), "{state}");
    let edges = handle(
        &h,
        &tc(
            "run_history",
            json!({"run_id": "child-1", "view": "transitions"}),
        ),
    )
    .await;
    assert!(edges.starts_with("'child-1' took 2 edges:"), "{edges}");
    assert!(edges.contains("\"from\": \"plan\""), "{edges}");
    let refused = handle(&h, &tc("run_history", json!({"run_id": "stranger"}))).await;
    assert_eq!(refused, "[error] 'stranger' is not this run");
    let bad = handle(&h, &tc("run_history", json!({"run": "x"}))).await;
    assert!(bad.starts_with("[error] run_history takes"), "{bad}");
    let out = handle(&dead_handle(), &tc("run_history", json!({"run_id": "c"}))).await;
    assert!(out.contains("shutting down"), "{out}");
    let (h, _t) = drop_host();
    let out = handle(&h, &tc("run_history", json!({"run_id": "c"}))).await;
    assert!(out.contains("dropped the history request"), "{out}");
}

/// Each sub-agent tool's advertised schema and its handler agree on the same
/// examples: a call the handler reads is one the schema takes, and a call the
/// schema refuses is one the handler refuses too. A raw graph's inside is
/// only an object to the schema; the handler checks it field by field, so it
/// is held to the one direction.
#[tokio::test]
async fn every_subagent_schema_agrees_with_its_handler() {
    use leviath_tools::validate::{ArgValidation, validate_tool_args};
    let schema_of = |name: &str| {
        leviath_tools::BuiltinTools::subagent_tool_defs()
            .into_iter()
            .find(|t| t.name == name)
            .unwrap()
            .parameters
    };
    let (h, _seen, _t) = fake_host(Ok("child-1".to_string()), vec![], false);
    let refused = |out: &str| out.starts_with("[error]");
    let elsewhere = temp_blueprint();
    let mut cases: Vec<(&str, serde_json::Value)> = Vec::new();
    for tool in ["spawn_agent", "validate_spawn"] {
        for args in [
            spawn_args("coder", "go"),
            bp_args(&elsewhere, "go"),
            json!({"source": {"blueprint": {"name": "coder"}}, "wait": false, "max_child_depth": 2}),
            json!({"source": {"blueprint": "coder"}, "output": {"format": "json"}, "parts": []}),
            json!({"source": {"graph": {}}}),
            json!({"source": {"blueprint": "coder", "graph": {}}}),
            json!({"source": {"blueprint": 7}}),
            json!({"source": {"blueprint": {"name": "c", "pin": 1}}}),
            json!({"source": "coder"}),
            json!({"blueprint": "coder", "task": "go"}),
            json!({"source": {"blueprint": "coder"}, "max_child_depth": 300}),
            json!({"source": {"blueprint": "coder"}, "output": {"shape": "x"}}),
        ] {
            cases.push((tool, args));
        }
    }
    for args in [
        json!({"run_id": "child-1"}),
        json!({"run_id": "child-1", "view": "transitions", "at": 2}),
        json!({"run_id": "child-1", "view": "everything"}),
        json!({"run_id": "child-1", "at": -1}),
        json!({"run": "child-1"}),
    ] {
        cases.push(("run_history", args));
    }
    for args in [
        json!({}),
        json!({"part": "RunGraph"}),
        json!({"parts": "x"}),
    ] {
        cases.push(("spawn_schema", args));
    }
    for args in [
        json!({"blueprint": {"name": "x", "extra": 1}}),
        json!({"blueprint": 3}),
        json!({}),
    ] {
        cases.push(("describe_blueprint", args));
    }
    for (tool, args) in cases {
        let valid = validate_tool_args(tool, &schema_of(tool), &args) == ArgValidation::Valid;
        let out = handle(&h, &tc(tool, args.clone())).await;
        let took = !refused(&out);
        assert!(
            valid || !took,
            "{tool} took what its schema refuses: {args} -> {out}"
        );
    }
}

/// One edge reads as one.
#[test]
fn a_single_transition_is_not_plural() {
    let mut one = history();
    one.transitions.truncate(1);
    let out = reads::render_history("r", reads::View::Transitions, &one);
    assert!(out.starts_with("'r' took 1 edge:"), "{out}");
}

//! Tests for the run file's typed views: every runtime value a run file can
//! hold converts, and every field of every converted type resolves.
//!
//! The values below fill every field and take every variant, so the
//! conversions are checked whole. A query that selects every field of every
//! type is then written from the schema's own SDL rather than by hand, so a
//! field added later is selected without anyone remembering to add it here.

use std::collections::{BTreeMap, HashMap};

use async_graphql::parser::types::{BaseType, Type, TypeKind, TypeSystemDefinition};
use async_graphql::{EmptyMutation, EmptySubscription, Object, Schema};
use leviath_core::JsonDoc;
use leviath_core::taint::TaintLevel as CoreTaint;
use leviath_runtime::spec::graph::{
    ArtifactDef, CodeRef as CoreCodeRef, EdgeCondition as CoreCondition, FanOutDef, OutputDef,
    RunGraph as CoreGraph, WorkerFailure, WorkerSource,
};
use leviath_runtime::spec::inputs::{
    InputDecl as CoreDecl, InputSlot as CoreSlot, InputType as CoreType, InputValue as CoreValue,
    InputValues, PathKind, RegionBinding, Template,
};
use leviath_runtime::spec::launch::{
    Callback, Delivery, LaunchPolicy, Placement, Secret, Unattended,
};
use leviath_runtime::spec::names::{
    BlueprintName, BlueprintPath, BlueprintRef, ChoiceName, Digest, EdgeName, HttpUrl, InputName,
    McpServerName, MimePattern, ModelId, ModelRef, ProfileName, ProviderName, RegionName, RunId,
    StageName, ToolName, WorkdirPath,
};
use leviath_runtime::spec::run_spec::{
    AutoAnswers, EnvFingerprint, RunSpec as CoreSpec, SeededContent, SpecOrigin, StagePlan,
    ToolDef as CoreTool, ToolSource,
};
use leviath_runtime::state::context::{
    BlobState, ContextDiff as CoreDiff, ContextState as CoreContext, EntryKind, EntryMeta,
    EntryState, PartBody, PartState, RegionChange, RegionHead, RegionState, TaintState,
    ToolCallState,
};
use leviath_runtime::state::{
    Change, Clock, Cursor, FanOutState, FinalOutputState, Flags, MessageState, OpenInteraction,
    PendingBatch, PipelinePhase, RunEvent, RunState as CoreState, RunStatus, Spend, StageProgress,
    StageRecord, StageStatus, StateDelta as CoreDelta, ToolResultState, Totals, TransitionReason,
    TransitionRecord, VisitRecord, WorkItemState,
};

use super::delta::StateDelta;
use super::graph::RunGraph;
use super::spec::{RunOrigin, RunSpec, SpawnSummary};
use super::state::RunState;
use super::values::{InputDecl, InputValue};

/// A name of one of the runtime's checked kinds.
macro_rules! named {
    ($ty:ident, $text:expr) => {
        $ty::new($text).expect(concat!("a valid ", stringify!($ty)))
    };
}

fn stage(name: &str) -> StageName {
    named!(StageName, name)
}

fn region(name: &str) -> RegionName {
    named!(RegionName, name)
}

fn spend() -> Spend {
    Spend {
        prompt_tokens: 100,
        completion_tokens: 20,
        cached_tokens: 10,
        cache_write_tokens: 5,
        priced_usd: 0.25,
        reported_calls: 1,
        computed_calls: 2,
        unpriced_calls: 3,
    }
}

fn clock() -> Clock {
    Clock {
        banked_secs: 90,
        since: Some(1_000),
    }
}

fn call() -> ToolCallState {
    ToolCallState {
        id: "call-1".into(),
        name: "read_file".into(),
        args: JsonDoc::new(serde_json::json!({ "path": "a.rs" })),
        thought_signature: Some("sig".into()),
    }
}

/// A part of each kind: kept inline, and stored by digest.
fn parts() -> Vec<PartState> {
    vec![
        PartState {
            mime_type: "text/plain".into(),
            body: PartBody::Inline("hi".into()),
            name: Some("note.txt".into()),
            deliver: Some(leviath_core::mime::Delivery::Text),
        },
        PartState {
            mime_type: "image/png".into(),
            body: PartBody::Stored(BlobState {
                digest: Digest::of(b"png"),
                size: 3,
                width: Some(4),
                height: Some(5),
                duration_ms: Some(6),
                tokens: 7,
                stand_in: "[an image]".into(),
            }),
            name: None,
            deliver: Some(leviath_core::mime::Delivery::Native),
        },
        PartState {
            mime_type: "application/pdf".into(),
            body: PartBody::Inline("%PDF".into()),
            name: None,
            deliver: Some(leviath_core::mime::Delivery::StandIn),
        },
    ]
}

fn entry(kind: EntryKind, meta: EntryMeta) -> EntryState {
    EntryState {
        text: "words".into(),
        parts: parts(),
        tokens: 12,
        timestamp: 50,
        kind,
        meta,
        key: Some("k".into()),
        reasoning: Some("because".into()),
    }
}

/// One entry of every kind.
fn entries() -> Vec<EntryState> {
    vec![
        entry(
            EntryKind::Text,
            EntryMeta::ChecklistItem {
                id: 1,
                done: true,
                note: Some("checked".into()),
            },
        ),
        entry(EntryKind::UserMessage, EntryMeta::None),
        entry(EntryKind::AssistantTurn(vec![call()]), EntryMeta::None),
        entry(
            EntryKind::ToolResult {
                call_id: "call-1".into(),
                tool: "read_file".into(),
                is_error: false,
            },
            EntryMeta::None,
        ),
    ]
}

fn taint() -> TaintState {
    TaintState {
        level: CoreTaint::Private,
        entries: vec![CoreTaint::Public, CoreTaint::Internal, CoreTaint::Private],
    }
}

fn context() -> CoreContext {
    CoreContext {
        regions: vec![RegionState {
            name: region("conversation"),
            max_tokens: 2_000,
            current_tokens: 300,
            needs_message_compaction: true,
            taint: Some(taint()),
            entries: entries(),
        }],
        hidden: vec![region("system")],
        max_tokens: 10_000,
    }
}

/// One value of every input type, a list and a record holding others.
fn values() -> Vec<CoreValue> {
    vec![
        CoreValue::Text("fix it".into()),
        CoreValue::Bool(true),
        CoreValue::Int(-3),
        CoreValue::Float(0.5),
        CoreValue::Choice(named!(ChoiceName, "fast")),
        CoreValue::List(vec![
            CoreValue::Int(1),
            CoreValue::List(vec![CoreValue::Text("deep".into())]),
        ]),
        CoreValue::Record(
            [(
                named!(InputName, "inner"),
                CoreValue::Record([(named!(InputName, "x"), CoreValue::Bool(false))].into()),
            )]
            .into(),
        ),
        CoreValue::File("spec.pdf".into()),
        CoreValue::Path(named!(WorkdirPath, "src/main.rs")),
        CoreValue::Model(ModelRef::parse("mock/gpt-mock").expect("a model")),
        CoreValue::Blueprint(
            BlueprintRef::parse(&format!("coder@{}", Digest::of(b"x"))).expect("a pin"),
        ),
        CoreValue::Duration(5_400),
        CoreValue::Url(named!(HttpUrl, "https://example.com")),
    ]
}

fn input_values() -> InputValues {
    InputValues(
        values()
            .into_iter()
            .enumerate()
            .map(|(i, value)| (named!(InputName, format!("v{i}")), value))
            .collect(),
    )
}

fn decl(name: &str, ty: CoreType) -> CoreDecl {
    CoreDecl {
        name: named!(InputName, name),
        ty,
        required: true,
        default: Some(CoreValue::Text("d".into())),
        description: Some("what it is for".into()),
        binds: Vec::new(),
    }
}

/// One declaration of every input type, a record and a list holding others,
/// and every kind of slot.
fn decls() -> Vec<CoreDecl> {
    let text = CoreType::Text {
        multiline: true,
        min_len: Some(1),
        max_len: Some(10),
    };
    let mut task = decl("task", text.clone());
    task.binds = vec![
        CoreSlot::Region(RegionBinding {
            region: region("task"),
            template: Some(Template::parse("do {task}").expect("a template")),
        }),
        CoreSlot::StageModel(stage("plan")),
        CoreSlot::StageMaxIterations(stage("plan")),
        CoreSlot::FanOutMaxWorkers(stage("plan")),
        CoreSlot::OutputFormat,
        CoreSlot::OutputInstructions,
    ];
    vec![
        task,
        decl("flag", CoreType::Bool),
        decl(
            "count",
            CoreType::Int {
                min: Some(1),
                max: Some(9),
            },
        ),
        decl(
            "ratio",
            CoreType::Float {
                min: Some(0.0),
                max: Some(1.0),
            },
        ),
        decl(
            "speed",
            CoreType::Choice {
                options: vec![named!(ChoiceName, "fast")],
            },
        ),
        decl(
            "items",
            CoreType::List {
                item: Box::new(CoreType::List {
                    item: Box::new(text.clone()),
                    min: None,
                    max: None,
                }),
                min: Some(1),
                max: Some(3),
            },
        ),
        decl(
            "pair",
            CoreType::Record {
                fields: vec![decl(
                    "inner",
                    CoreType::Record {
                        fields: vec![decl("x", CoreType::Bool)],
                    },
                )],
            },
        ),
        decl(
            "doc",
            CoreType::File {
                accepts: vec![named!(MimePattern, "application/pdf")],
            },
        ),
        decl(
            "file",
            CoreType::Path {
                kind: PathKind::File,
                must_exist: true,
            },
        ),
        decl(
            "dir",
            CoreType::Path {
                kind: PathKind::Dir,
                must_exist: false,
            },
        ),
        decl(
            "any",
            CoreType::Path {
                kind: PathKind::Any,
                must_exist: false,
            },
        ),
        decl("model", CoreType::Model),
        decl("worker", CoreType::Blueprint),
        decl("wait", CoreType::Duration),
        decl("site", CoreType::Url),
    ]
}

/// A graph with stages and edges, read from the coder-shaped test blueprint.
fn graph() -> CoreGraph {
    leviath_blueprint::BlueprintFile::parse(&crate::test_support::inline_coder_manifest())
        .expect("the test blueprint parses")
        .run_graph()
}

fn tool(name: &str, source: ToolSource) -> CoreTool {
    CoreTool {
        name: named!(ToolName, name),
        description: "a tool".into(),
        schema: JsonDoc::new(serde_json::json!({ "type": "object" })),
        source,
    }
}

fn output() -> OutputDef {
    OutputDef {
        format: Some("json".into()),
        instructions: Some("one object".into()),
        example: Some("{}".into()),
        schema: Some(JsonDoc::new(serde_json::json!({ "type": "object" }))),
        validator: Some(CoreCodeRef::Inline("true".into())),
        on_validator_error: Some(leviath_core::output::OnValidatorError::Accept),
        overwrite_artifacts: Some(true),
        artifacts: vec![ArtifactDef {
            name: "report".into(),
            mime_type: named!(MimePattern, "text/markdown"),
            required: true,
            description: Some("the report".into()),
        }],
    }
}

/// A spec with every field set and every tool source.
fn spec() -> CoreSpec {
    let digest = Digest::of(b"code");
    CoreSpec {
        run_id: named!(RunId, "coder-1"),
        origin: SpecOrigin::Blueprint {
            blueprint: BlueprintRef {
                name: named!(BlueprintName, "coder"),
                digest: Some(digest.clone()),
            },
            version: "1.0.0".into(),
        },
        graph: graph(),
        inputs: input_values(),
        stages: vec![StagePlan {
            stage: stage("analyze"),
            provider: named!(ProviderName, "mock"),
            model: named!(ModelId, "gpt-mock"),
            context_window: 128_000,
            max_output_tokens: Some(8_000),
            fallbacks: vec![ModelRef::parse("other/m").expect("a model")],
            tools: vec![
                tool("read_file", ToolSource::Builtin),
                tool("spawn_agent", ToolSource::Subagent),
                tool("complete_stage", ToolSource::StageControl),
                tool("mine", ToolSource::Script(digest.clone())),
                tool(
                    "gh__search",
                    ToolSource::Mcp {
                        server: named!(McpServerName, "gh"),
                        tool: "search".into(),
                    },
                ),
            ],
            output: Some(output()),
            region_budgets: [(region("task"), 500)].into(),
            notes: vec!["the operator's model".into()],
        }],
        seeded: [(
            region("task"),
            SeededContent {
                text: "fix it".into(),
                parts: parts(),
            },
        )]
        .into(),
        code: vec![
            (CoreCodeRef::File("hooks/enter.rhai".into()), digest.clone()),
            (CoreCodeRef::Inline("true".into()), digest.clone()),
        ],
        requested_output: Some(output()),
        requested_model: Some(ModelRef::parse("gpt-mock").expect("a model")),
        launch: LaunchPolicy {
            unattended: Unattended::All,
            allow: vec![named!(ToolName, "shell")],
            max_depth: 2,
            seed_commands: false,
            capture_model_input: true,
        },
        auto_answers: AutoAnswers::all(),
        placement: Placement {
            workdir: "/work".into(),
            parent: Some(named!(RunId, "parent-1")),
            depth: 1,
            worker_stage: Some(stage("analyze")),
        },
        delivery: Delivery {
            callback: Some(Callback {
                url: named!(HttpUrl, "https://example.com/hook"),
                secret: Some(Secret::new("shh")),
            }),
            metadata: [("ticket".to_string(), "42".to_string())].into(),
        },
        env: EnvFingerprint {
            providers: [(named!(ProviderName, "mock"), digest.clone())].into(),
            mcp_servers: [(named!(McpServerName, "gh"), digest)].into(),
            leviath_version: "0.6.4".into(),
        },
        created_at: 1_000,
    }
}

fn transition(reason: TransitionReason) -> TransitionRecord {
    TransitionRecord {
        from: stage("analyze"),
        to: stage("implement"),
        edge: Some(named!(EdgeName, "implement")),
        reason,
        visit: "v1".into(),
    }
}

fn record() -> StageRecord {
    StageRecord {
        stage: stage("analyze"),
        status: StageStatus::Complete,
        entered: true,
        spend: spend(),
        models: vec![ModelRef::parse("mock/gpt-mock").expect("a model")],
        visits: vec![VisitRecord {
            id: "v1".into(),
            entered_at: 10,
            left_at: Some(20),
            spend: spend(),
            clock: clock(),
        }],
        region_tokens: [("task".to_string(), 40)].into(),
        first_call_prompt_tokens: Some(400),
        runaway_warned: true,
        output_cap_raised: true,
        started_at: Some(10),
        ended_at: Some(20),
        clock: clock(),
    }
}

fn progress() -> StageProgress {
    StageProgress {
        total_tool_calls: 1,
        text_only_nudges: 2,
        cut_off_nudges: 3,
        raise_output_cap: true,
        iterations: 4,
        modifying_tool_calls: 5,
        blocked_modification_calls: 6,
        entry_region_digests: [("task".to_string(), 77)].into(),
        gate_reentries: 7,
        stage_started_at: Some(10),
        waiting_since: Some(11),
        edits_by_path: [("a.rs".to_string(), 2)].into(),
        stuck_fired: true,
        images_produced: 8,
        no_image_nudges: 9,
    }
}

fn pending() -> PendingBatch {
    PendingBatch {
        calls: vec![call()],
        done: [(
            "call-1".to_string(),
            ToolResultState {
                text: "contents".into(),
                is_error: false,
            },
        )]
        .into(),
    }
}

fn fan_out(worker: WorkerSource, failure: WorkerFailure) -> FanOutState {
    let mut config = FanOutDef::same_graph(stage("analyze"));
    config.worker = worker;
    config.on_worker_failure = failure;
    config.merge_stage = Some(stage("review"));
    FanOutState {
        stage: stage("analyze"),
        config,
        max_workers: 4,
        queued: vec![WorkItemState {
            id: "item-1".into(),
            inputs: input_values(),
        }],
        active: vec![("item-2".into(), named!(RunId, "worker-2"))],
        done: vec![("item-3".into(), "summary".into())],
        failed: vec![("item-4".into(), "it broke".into())],
        paused: true,
    }
}

fn message() -> MessageState {
    MessageState {
        from: "parent".into(),
        text: "carry on".into(),
        region: Some("conversation".into()),
    }
}

fn question() -> OpenInteraction {
    OpenInteraction {
        id: "q1".into(),
        prompt: "which?".into(),
        options: vec!["a".into(), "b".into()],
    }
}

fn flags() -> Flags {
    Flags {
        modified_files: vec!["a.rs".into()],
        modified_file_count: 1,
        empty_output: true,
        no_output_tools: true,
        searches_run: 2,
        searches_empty: 1,
        max_iterations_hit: 1,
        gates_forced: 1,
        required_regions_abandoned: vec!["plan".into()],
        workspace_lost: true,
        produced_output: true,
        output_forced: 1,
        splits_degraded: 1,
        broken_scripts: vec!["bad.rhai".into()],
    }
}

fn answer() -> FinalOutputState {
    FinalOutputState {
        content: "done".into(),
        format: Some("markdown".into()),
        stage: stage("review"),
        submitted_at: 99,
        truncated: true,
    }
}

/// A state with every field set.
fn state() -> CoreState {
    CoreState {
        seq: 4,
        status: RunStatus::Error("boom".into()),
        cursor: Cursor {
            stage: stage("implement"),
            visit: "v2".into(),
            iteration: 3,
        },
        phase: PipelinePhase::AwaitingChoice(vec![named!(EdgeName, "review")]),
        accepts_messages: true,
        visits: [(stage("analyze"), 1), (stage("implement"), 2)].into(),
        progress: progress(),
        ledger: vec![record()],
        context: context(),
        pending: Some(pending()),
        fan_out: Some(fan_out(
            WorkerSource::Blueprint(BlueprintRef::parse("worker").expect("a name")),
            WorkerFailure::FailAll,
        )),
        inbox: vec![message()],
        interactions: vec![question()],
        totals: Totals {
            spend: spend(),
            tool_calls: 7,
        },
        clock: clock(),
        flags: flags(),
        children: vec![named!(RunId, "child-1")],
        title: Some("A title".into()),
        final_output: Some(answer()),
        wait_reason: Some("a person".into()),
        last_transition: Some(transition(TransitionReason::Condition)),
    }
}

/// A context change of every kind.
fn diff() -> CoreDiff {
    let head = RegionHead {
        max_tokens: 2_000,
        current_tokens: 300,
        needs_message_compaction: false,
        taint: Some(taint()),
    };
    CoreDiff {
        regions: vec![
            (region("a"), head.clone(), None),
            (
                region("b"),
                head.clone(),
                Some(RegionChange::Append(entries())),
            ),
            (region("c"), head, Some(RegionChange::Replace(entries()))),
        ],
        removed: vec![region("gone")],
        order: Some(vec![region("a"), region("b")]),
        hidden: Some(vec![region("c")]),
        max_tokens: Some(9_000),
    }
}

/// One step changing every part of a state, with every kind of event.
fn delta() -> CoreDelta {
    let s = state();
    CoreDelta {
        seq: 5,
        at: 1_234,
        changes: vec![
            Change::Status(s.status.clone()),
            Change::Cursor(s.cursor.clone()),
            Change::Phase(s.phase.clone()),
            Change::AcceptsMessages(false),
            Change::Visits(s.visits.clone()),
            Change::Progress(s.progress.clone()),
            Change::LedgerRecord(0, record()),
            Change::LedgerTruncate(1),
            Change::Context(diff()),
            Change::Pending(Some(pending())),
            Change::FanOut(s.fan_out.clone()),
            Change::Inbox(vec![message()]),
            Change::Interactions(vec![question()]),
            Change::Totals(s.totals.clone()),
            Change::Clock(clock()),
            Change::Flags(flags()),
            Change::Children(s.children.clone()),
            Change::Title(Some("renamed".into())),
            Change::FinalOutput(Some(answer())),
            Change::WaitReason(None),
            Change::LastTransition(Some(transition(TransitionReason::Gate))),
        ],
        events: vec![
            RunEvent::Inference {
                attempt: "a1".into(),
                model: ModelRef::parse("mock/gpt-mock").expect("a model"),
                spend: spend(),
                finish_reason: Some("stop".into()),
            },
            RunEvent::Failover {
                from: ModelRef::parse("mock/a").expect("a model"),
                to: ModelRef::parse("mock/b").expect("a model"),
                reason: "overloaded".into(),
            },
            RunEvent::ToolStarted(call()),
            RunEvent::ToolFinished {
                call_id: "call-1".into(),
                result: ToolResultState {
                    text: "contents".into(),
                    is_error: true,
                },
                millis: 40,
            },
            RunEvent::Answered {
                id: "q1".into(),
                answer: "a".into(),
            },
            RunEvent::Message(message()),
            RunEvent::Log("a line".into()),
        ]
        .into_iter()
        .chain(kept_facts())
        .collect(),
    }
}

/// One of every fact a step keeps whole, in every word its vocabularies hold.
fn kept_facts() -> Vec<RunEvent> {
    use leviath_runtime::state::journal::{
        ArtifactState, AttemptOutcomeState, AttemptState, CaptureState, CauseState,
        ContextCommitState, ContextNoteState, ModelInputState, QuestionKind, RegionCommitState,
        RequestDigestState, RetryState, SettledState, ToolOutcomeState,
    };
    let attempt = |outcome: AttemptOutcomeState, capture: Option<CaptureState>| {
        RunEvent::Attempt(Box::new(AttemptState {
            id: "a1".into(),
            number: 2,
            provider: "mock".into(),
            model: "m".into(),
            outcome,
            finish_reason: Some("stop".into()),
            stopped_for: Some("filter".into()),
            duration_ms: 9,
            backoff_ms: 3,
            digest: RequestDigestState {
                system_hash: 7,
                messages: 4,
                tools: 2,
                max_tokens: 100,
                temperature: 0.5,
            },
            model_input: capture.map(|capture| ModelInputState {
                capture,
                request: Some(JsonDoc::new(serde_json::json!({ "model": "m" }))),
                bytes: 15,
                source_context_digest: "ctx".into(),
                parameters: [("t".to_string(), JsonDoc::new(serde_json::json!(0.5)))].into(),
                tool_catalog_version: "t1".into(),
                assembly_version: "1".into(),
            }),
        }))
    };
    let failed = |next| AttemptOutcomeState::Failed {
        kind: "timeout".into(),
        transient: true,
        capacity: false,
        next,
    };
    let mut events = vec![
        attempt(AttemptOutcomeState::Succeeded, Some(CaptureState::Retained)),
        attempt(
            AttemptOutcomeState::Succeeded,
            Some(CaptureState::NotCaptured),
        ),
        attempt(failed(RetryState::Reported), Some(CaptureState::Redacted)),
        attempt(failed(RetryState::SameModel), Some(CaptureState::Expired)),
        attempt(failed(RetryState::RenewedFiles), None),
        RunEvent::Dispatched {
            call_id: "c1".into(),
            execution_id: "x1".into(),
            requested_by: "a1".into(),
        },
        RunEvent::Artifacts {
            execution_id: "x1".into(),
            artifacts: vec![ArtifactState {
                name: "report".into(),
                path: "out/r.md".into(),
                mime_type: "text/markdown".into(),
                size: 9,
                sha256: "beef".into(),
            }],
        },
        RunEvent::ContextNoted(ContextNoteState {
            region: "plan".into(),
            cause: CauseState::Seed,
            entries_added: 1,
            entries_removed: 2,
            token_delta: -3,
        }),
    ];
    for outcome in [
        Some(ToolOutcomeState::Succeeded),
        Some(ToolOutcomeState::Failed),
        Some(ToolOutcomeState::Blocked),
        Some(ToolOutcomeState::Denied),
        Some(ToolOutcomeState::Indeterminate),
        None,
    ] {
        events.push(RunEvent::Completed {
            call_id: "c1".into(),
            execution_id: "x1".into(),
            outcome,
            parts: vec!["chart.png".into()],
        });
    }
    for (kind, settlement) in [
        (QuestionKind::FreeText, "\"timed_out\""),
        (QuestionKind::MultipleChoice, "\"cancelled\""),
        (QuestionKind::Confirm, "{}"),
        (QuestionKind::ToolApproval, "\"timed_out\""),
        (QuestionKind::EditText, "not json"),
    ] {
        events.push(RunEvent::Settled(Box::new(SettledState {
            id: "q1".into(),
            kind,
            tool: Some("shell".into()),
            prompt: "?".into(),
            stage: "plan".into(),
            settlement: settlement.into(),
            asked_at: 4,
        })));
    }
    for (at, cause) in [
        CauseState::Seed,
        CauseState::Message,
        CauseState::ModelReply,
        CauseState::ToolResult,
        CauseState::ProducedPart,
        CauseState::Compaction,
        CauseState::Transform,
        CauseState::ContextTool,
        CauseState::Hook,
        CauseState::FanOut,
        CauseState::Interaction,
        CauseState::Resume,
        CauseState::Framework,
    ]
    .into_iter()
    .enumerate()
    {
        events.push(RunEvent::ContextCommitted(Box::new(ContextCommitState {
            cause,
            execution_id: (at == 0).then(|| "x1".to_string()),
            revision_before: "cw1-a".into(),
            revision_after: "cw1-b".into(),
            regions: vec![RegionCommitState {
                region: "plan".into(),
                digest_before: "d1".into(),
                digest_after: "d2".into(),
                tokens_before: 1,
                tokens_after: 5,
                entries_before: 0,
                entries_after: 1,
                entries_added: 1,
            }],
        })));
    }
    events
}

/// Every phase a run can be in.
fn phases() -> Vec<PipelinePhase> {
    vec![
        PipelinePhase::ReadyToInfer,
        PipelinePhase::AwaitingInference,
        PipelinePhase::AwaitingTools,
        PipelinePhase::AwaitingCompaction,
        PipelinePhase::AwaitingChoice(vec![named!(EdgeName, "review")]),
        PipelinePhase::WaitingForChildren,
        PipelinePhase::FanOut,
        PipelinePhase::AwaitingPerson,
        PipelinePhase::Wedged("no way out".into()),
        PipelinePhase::Paused,
        PipelinePhase::Done,
    ]
}

/// A root that hands out one of every converted value.
struct Probe;

#[Object]
impl Probe {
    /// States in every status, phase, worker source and failure policy.
    async fn states(&self) -> Vec<RunState> {
        let statuses = [
            RunStatus::Idle,
            RunStatus::Active,
            RunStatus::Waiting,
            RunStatus::Paused,
            RunStatus::Complete,
            RunStatus::Error("boom".into()),
            RunStatus::Cancelled,
        ];
        let mut out = vec![RunState::from(&state())];
        for (i, status) in statuses.into_iter().enumerate() {
            let mut s = state();
            s.status = status;
            s.phase = phases()[i].clone();
            out.push(RunState::from(&s));
        }
        for (worker, failure) in [
            (
                WorkerSource::Stage(stage("analyze")),
                WorkerFailure::Continue,
            ),
            (WorkerSource::Query("fast".into()), WorkerFailure::FailAll),
            (
                WorkerSource::BlueprintFile(
                    BlueprintPath::new(std::env::temp_dir().to_string_lossy()).expect("a path"),
                ),
                WorkerFailure::Continue,
            ),
        ] {
            let mut s = state();
            s.fan_out = Some(fan_out(worker, failure));
            s.phase = phases()[10].clone();
            out.push(RunState::from(&s));
        }
        for phase in phases().into_iter().skip(7) {
            let mut s = state();
            s.phase = phase;
            out.push(RunState::from(&s));
        }
        let mut statuses = state();
        statuses.ledger = [
            StageStatus::Pending,
            StageStatus::Active,
            StageStatus::WaitingInput,
            StageStatus::Error,
            StageStatus::Skipped,
        ]
        .into_iter()
        .map(|status| StageRecord { status, ..record() })
        .collect();
        statuses.last_transition = None;
        out.push(RunState::from(&statuses));
        out
    }

    /// One step changing everything, and the transitions of every reason.
    async fn deltas(&self) -> Vec<StateDelta> {
        let reasons = [
            TransitionReason::Condition,
            TransitionReason::Gate,
            TransitionReason::ModelChoice,
            TransitionReason::Forced,
            TransitionReason::Worker,
            TransitionReason::Router,
        ];
        let mut out = vec![StateDelta::from(&delta())];
        for reason in reasons {
            out.push(StateDelta::from(&CoreDelta {
                seq: 6,
                at: 2,
                changes: vec![Change::LastTransition(Some(transition(reason)))],
                events: Vec::new(),
            }));
        }
        out
    }

    /// A spec, filled.
    async fn spec(&self) -> RunSpec {
        RunSpec::from(&spec())
    }

    /// Every input type.
    async fn decls(&self) -> Vec<InputDecl> {
        decls().iter().map(InputDecl::from).collect()
    }

    /// Every input value.
    async fn values(&self) -> Vec<InputValue> {
        values().iter().map(InputValue::from).collect()
    }

    /// A dry run's answer.
    async fn summary(&self) -> SpawnSummary {
        SpawnSummary::from(&leviath_runtime::spec::summary::SpawnSummary {
            title: "coder".into(),
            origin: SpecOrigin::Raw,
            entry_stage: stage("analyze"),
            stages: Vec::new(),
            inputs: input_values(),
            launch: spec().launch,
            workdir: "/work".into(),
        })
    }

    /// The graph, walked twice through one edge.
    async fn graph(&self) -> RunGraph {
        let spec = spec();
        let step = CoreDelta {
            seq: 1,
            at: 1,
            changes: vec![Change::LastTransition(Some(transition(
                TransitionReason::Condition,
            )))],
            events: Vec::new(),
        };
        RunGraph::of(&spec, &state(), &[step.clone(), step])
    }
}

/// The type a reference ultimately names.
fn named_type(ty: &Type) -> String {
    let mut base = &ty.base;
    loop {
        match base {
            BaseType::Named(name) => return name.to_string(),
            BaseType::List(inner) => base = &inner.base,
        }
    }
}

/// A selection set asking for every field of `name`, down to `depth` levels.
fn select_all(types: &HashMap<String, TypeKind>, name: &str, depth: usize) -> String {
    let leaf = |ty: &str| {
        !matches!(
            types.get(ty),
            Some(TypeKind::Object(_) | TypeKind::Union(_) | TypeKind::Interface(_))
        )
    };
    let mut out = vec!["__typename".to_string()];
    match types.get(name) {
        Some(TypeKind::Object(object)) => {
            for field in &object.fields {
                let field_name = field.node.name.node.to_string();
                let ty = named_type(&field.node.ty.node);
                if leaf(&ty) {
                    out.push(field_name);
                } else if depth > 0 {
                    out.push(format!(
                        "{field_name} {{ {} }}",
                        select_all(types, &ty, depth - 1)
                    ));
                }
            }
        }
        Some(TypeKind::Union(union)) if depth > 0 => {
            for member in &union.members {
                out.push(format!(
                    "... on {} {{ {} }}",
                    member.node,
                    select_all(types, member.node.as_str(), depth - 1)
                ));
            }
        }
        _ => {}
    }
    out.join(" ")
}

/// Every field of every value the probe hands out resolves.
#[tokio::test]
async fn every_field_of_every_converted_type_resolves() {
    let schema = Schema::build(Probe, EmptyMutation, EmptySubscription).finish();
    let parsed = async_graphql::parser::parse_schema(schema.sdl()).expect("the SDL parses");
    let types: HashMap<String, TypeKind> = parsed
        .definitions
        .into_iter()
        .filter_map(|definition| match definition {
            TypeSystemDefinition::Type(ty) => Some((ty.node.name.node.to_string(), ty.node.kind)),
            _ => None,
        })
        .collect();
    let query = format!("{{ {} }}", select_all(&types, "Probe", 12));
    let answer = schema.execute(query.as_str()).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");

    let full = &json["states"][0];
    assert_eq!(full["status"], "ERROR");
    assert_eq!(full["error"], "boom");
    assert_eq!(full["phase"]["kind"], "AWAITING_CHOICE");
    assert_eq!(full["phase"]["edges"][0], "review");
    assert_eq!(full["cursor"]["stage"], "implement");
    assert_eq!(full["visits"][1]["visits"], 2);
    assert_eq!(full["context"]["regions"][0]["taint"]["level"], "PRIVATE");
    let entry = &full["context"]["regions"][0]["entries"][2];
    assert_eq!(entry["kind"], "ASSISTANT_TURN");
    assert_eq!(entry["toolCalls"][0]["arguments"]["path"], "a.rs");
    assert_eq!(full["fanOut"]["workerKind"], "BLUEPRINT");
    assert_eq!(full["fanOut"]["active"][0]["detail"], "worker-2");
    assert_eq!(full["answer"]["content"], "done");
    assert_eq!(full["lastTransition"]["fromStage"], "analyze");
    assert_eq!(full["totals"]["spend"]["pricedUsd"], "0.25");
    let wedged = &json["states"][12];
    assert_eq!(wedged["phase"]["reason"], "no way out", "{wedged}");

    let step = &json["deltas"][0];
    assert_eq!(step["changes"].as_array().map(Vec::len), Some(21));
    assert_eq!(
        step["events"].as_array().map(Vec::len),
        Some(7 + 8 + 6 + 5 + 13)
    );
    let attempt = &step["events"][9];
    assert_eq!(attempt["__typename"], "AttemptStepOutput");
    assert_eq!(attempt["failure"]["next"], "REPORTED");
    assert_eq!(attempt["modelInput"]["capture"], "REDACTED");
    assert_eq!(attempt["modelInput"]["parameters"]["t"], 0.5);
    assert_eq!(step["events"][15]["outcome"], "SUCCEEDED");
    assert_eq!(step["events"][25]["settlement"], "not json");
    assert_eq!(step["events"][26]["committedBy"], "x1");
    assert_eq!(step["events"][38]["cause"], "FRAMEWORK");
    let context = &step["changes"][8]["diff"];
    assert_eq!(context["regions"][0]["mode"], serde_json::Value::Null);
    assert_eq!(context["regions"][1]["mode"], "APPEND");
    assert_eq!(context["regions"][2]["mode"], "REPLACE");
    assert_eq!(
        json["deltas"][3]["changes"][0]["transition"]["reason"],
        "MODEL_CHOICE"
    );

    let spec = &json["spec"];
    assert_eq!(spec["runId"], "coder-1");
    assert_eq!(spec["origin"]["kind"], "BLUEPRINT");
    assert_eq!(spec["stages"][0]["tools"][4]["mcpTool"], "search");
    assert_eq!(spec["delivery"]["callbackSigned"], true);
    assert_eq!(spec["placement"]["parentId"], "parent-1");
    assert!(
        spec["graph"]["stages"]
            .as_array()
            .is_some_and(|s| s.len() == 3),
        "{spec}"
    );

    assert_eq!(json["values"][5]["items"][1]["items"][0]["text"], "deep");
    assert_eq!(json["values"][11]["seconds"], 5_400);
    assert_eq!(json["decls"][0]["binds"][0]["template"], "do {task}");
    assert_eq!(json["summary"]["origin"]["kind"], "RAW");
}

/// Every origin reads with what it carries.
#[test]
fn every_origin_says_where_the_graph_came_from() {
    let file = RunOrigin::from(&SpecOrigin::BlueprintFile {
        path: BlueprintPath::new(std::env::temp_dir().to_string_lossy()).expect("a path"),
        name: named!(BlueprintName, "coder"),
        digest: Some(Digest::of(b"x")),
        version: "2.0.0".into(),
    });
    assert_eq!(file.blueprint_name.as_deref(), Some("coder"));
    assert!(file.path.is_some() && file.digest.is_some());
    let raw = RunOrigin::from(&SpecOrigin::Raw);
    assert!(raw.blueprint_name.is_none() && raw.version.is_none());
}

/// Every launch setting reads as the mode it is.
#[test]
fn every_unattended_setting_reads_as_its_mode() {
    use super::spec::{LaunchPolicy as Policy, UnattendedMode};
    let mut launch = spec().launch;
    for (setting, mode) in [
        (Unattended::Off, UnattendedMode::Off),
        (Unattended::All, UnattendedMode::All),
        (
            Unattended::Profile(named!(ProfileName, "safe")),
            UnattendedMode::Profile,
        ),
    ] {
        launch.unattended = setting;
        assert_eq!(Policy::from(&launch).unattended, mode);
    }
}

/// Every edge condition crosses over.
#[test]
fn every_edge_condition_crosses_over() {
    use super::graph::EdgeCondition;
    for (core, want) in [
        (CoreCondition::Always, EdgeCondition::Always),
        (CoreCondition::Error, EdgeCondition::Error),
        (CoreCondition::MaxIterations, EdgeCondition::MaxIterations),
        (CoreCondition::LlmChoice, EdgeCondition::LlmChoice),
        (CoreCondition::DeadEnd, EdgeCondition::DeadEnd),
        (CoreCondition::Stuck, EdgeCondition::Stuck),
    ] {
        assert_eq!(EdgeCondition::from(core), want);
    }
}

/// A graph's edges count each move once, by name, by the stages it joins
/// when it names no edge, and not at all when no edge joins them.
#[test]
fn each_move_counts_against_the_edge_it_took() {
    let spec = spec();
    let edges = &spec.graph.edges;
    let first = &edges[0];
    let named = TransitionRecord {
        from: first.from.clone(),
        to: first.to.clone(),
        edge: Some(first.name.clone()),
        reason: TransitionReason::Condition,
        visit: "v1".into(),
    };
    let unnamed = TransitionRecord {
        edge: None,
        visit: "v2".into(),
        ..named.clone()
    };
    let nowhere = TransitionRecord {
        from: first.to.clone(),
        to: first.from.clone(),
        edge: None,
        reason: TransitionReason::Forced,
        visit: "v3".into(),
    };
    let steps: Vec<CoreDelta> = [Some(named), Some(unnamed), Some(nowhere), None]
        .into_iter()
        .enumerate()
        .map(|(i, record)| CoreDelta {
            seq: i as u64 + 1,
            at: 1,
            changes: vec![
                Change::LastTransition(record),
                Change::AcceptsMessages(true),
            ],
            events: Vec::new(),
        })
        .collect();
    let graph = RunGraph::of(&spec, &state(), &steps);
    assert_eq!(graph.edges[0].taken, 2);
    assert!(graph.edges.iter().skip(1).all(|edge| edge.taken == 0));
    let current: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|node| node.current)
        .map(|node| node.stage.as_str())
        .collect();
    assert_eq!(current, ["implement"]);
    let visits: BTreeMap<&str, i32> = graph
        .nodes
        .iter()
        .map(|node| (node.stage.as_str(), node.visits))
        .collect();
    assert_eq!(visits.get("implement"), Some(&2));
    assert_eq!(visits.get("review"), Some(&0));
}

/// The helpers that shape numbers saturate rather than wrap.
#[test]
fn numbers_saturate_rather_than_wrap() {
    assert_eq!(super::saturating(u32::MAX), i32::MAX);
    assert_eq!(super::big(u64::MAX).0, i64::MAX);
    assert_eq!(super::saturating(7), 7);
}

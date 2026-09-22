//! Tests for a run's detail: why it is parked, what it produced, what each
//! stage cost, and the window it is working in.

use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};

use super::super::run::{CostBreakdown, TokenUsage, WorkingClock};
use super::{
    Artifact, BlobEntry, ContextRegion, ContextWindow, FinalOutput, RegionPeak, RunFlags,
    SetupBlocker, StageModelUse, StageRecord, StageStatus, StageVisit, WaitReason, WaitReasonKind,
};
use crate::commands::serve::graphql::filter::testkit::{exercise, exercise_enum, exercise_list};
use crate::commands::serve::graphql::scalars::{BigInt, Decimal, Timestamp};

/// A root handing out one window, so a field test is one query.
struct WindowProbe {
    window: ContextWindow,
}

#[async_graphql::Object]
impl WindowProbe {
    /// The window under test.
    async fn context(&self) -> &ContextWindow {
        &self.window
    }
}

/// Ask the schema for a window's fields.
async fn ask_window(
    snapshot: leviath_core::run_meta::ContextSnapshot,
    query: &str,
) -> serde_json::Value {
    let schema = Schema::build(
        WindowProbe {
            window: ContextWindow {
                snapshot: Arc::new(snapshot),
            },
        },
        EmptyMutation,
        EmptySubscription,
    )
    .finish();
    let answer = schema.execute(Request::new(query)).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).expect("data serializes")
}

/// A parked run says what it is waiting for, and whether anybody has to act.
///
/// That last part is the difference between a row somebody must answer and a
/// run that is simply waiting on its own workers.
#[test]
fn a_wait_reason_says_whether_it_needs_a_person() {
    use leviath_core::run_meta::WaitReason as Core;

    let prompt = WaitReason::from(&Core::UserPrompt);
    assert_eq!(prompt.reason, WaitReasonKind::UserPrompt);
    assert!(prompt.needs_a_person);
    assert!(prompt.outstanding.is_none());
    assert!(prompt.blocker.is_none());

    for reason in [Core::ToolApproval, Core::TaintGate, Core::InteractionPoint] {
        assert!(WaitReason::from(&reason).needs_a_person, "{reason:?}");
    }

    let workers = WaitReason::from(&Core::FanOutWorkers { outstanding: 7 });
    assert_eq!(workers.reason, WaitReasonKind::FanOutWorkers);
    assert_eq!(workers.outstanding, Some(7));
    assert!(!workers.needs_a_person, "workers resolve on their own");

    let children = WaitReason::from(&Core::Children { outstanding: 2 });
    assert_eq!(children.outstanding, Some(2));
    assert!(!children.needs_a_person);
}

/// A run parked on the machine's own configuration carries both the cause and
/// the remedy, so a client can offer the right thing to do.
#[test]
fn a_setup_blocker_carries_its_remedy() {
    use leviath_core::run_meta::{SetupBlocker as CoreBlocker, WaitReason as Core};
    let parked = WaitReason::from(&Core::NeedsSetup {
        blocker: CoreBlocker::CreditsExhausted,
        remedy: "top up the account".to_string(),
    });
    assert_eq!(parked.reason, WaitReasonKind::NeedsSetup);
    assert_eq!(parked.blocker, Some(SetupBlocker::CreditsExhausted));
    assert_eq!(parked.remedy.as_deref(), Some("top up the account"));
    assert!(parked.needs_a_person);
}

/// Every blocker the daemon can report has exactly one schema value.
#[test]
fn every_setup_blocker_maps_to_one_value() {
    use leviath_core::run_meta::SetupBlocker as Core;
    let cases = [
        (Core::ProviderMissing, SetupBlocker::ProviderMissing),
        (Core::CreditsExhausted, SetupBlocker::CreditsExhausted),
        (Core::AuthFailed, SetupBlocker::AuthFailed),
        (Core::Forbidden, SetupBlocker::Forbidden),
        (
            Core::ProvidersUnavailable,
            SetupBlocker::ProvidersUnavailable,
        ),
        (Core::ProviderUnreachable, SetupBlocker::ProviderUnreachable),
        (Core::ProviderTimedOut, SetupBlocker::ProviderTimedOut),
        (Core::ProviderFailed, SetupBlocker::ProviderFailed),
    ];
    for (core, expected) in cases {
        assert_eq!(SetupBlocker::from(&core), expected, "{core:?}");
    }
}

/// The diagnostics a finished run leaves behind, mapped field by field.
#[test]
fn the_run_flags_carry_every_counter() {
    let core = leviath_core::run_meta::RunFlags {
        empty_output: true,
        produced_output: false,
        output_forced: 2,
        no_output_tools: true,
        gates_forced: 3,
        max_iterations_hit: 1,
        splits_degraded: 4,
        modified_file_count: 5,
        modified_files: vec!["src/main.rs".to_string()],
        searches_run: 9,
        searches_empty: 8,
        required_regions_abandoned: vec!["plan".to_string()],
        workspace_lost: true,
        ..Default::default()
    };

    let flags = RunFlags::from(&core);
    assert!(flags.empty_output);
    assert!(!flags.produced_output);
    assert_eq!(flags.output_forced, 2);
    assert!(flags.no_output_tools);
    assert_eq!(flags.gates_forced, 3);
    assert_eq!(flags.max_iterations_hit, 1);
    assert_eq!(flags.splits_degraded, 4);
    assert_eq!(flags.modified_file_count, 5);
    assert_eq!(flags.modified_files, vec!["src/main.rs".to_string()]);
    assert_eq!(flags.searches_run, 9);
    assert_eq!(flags.searches_empty, 8);
    assert_eq!(flags.required_regions_abandoned, vec!["plan".to_string()]);
    assert!(flags.workspace_lost);
}

/// A stage's record: its roll-ups, its visits, and the most each region held
/// while it was active.
#[test]
fn a_stage_record_carries_its_ledger() {
    let mut core = leviath_core::run_meta::StageRecord::new("build".to_string(), 1);
    core.status = leviath_core::run_meta::StageRunStatus::Complete;
    core.entered = true;
    core.prompt_tokens = 1_000;
    core.completion_tokens = 200;
    core.cached_tokens = 700;
    core.cache_write_tokens = 50;
    core.cost_usd = Some(0.02);
    core.cost_priced_usd = 0.02;
    core.cost_is_exact = true;
    core.unpriced_calls = 0;
    core.visit_count = 2;
    core.runaway_warned = true;
    core.started_at = Some(100);
    core.ended_at = Some(200);
    core.region_tokens.insert("plan".to_string(), 120);
    core.region_tokens.insert("conversation".to_string(), 900);
    let mut visit = leviath_core::run_meta::StageVisitRecord::opened_at(100, "v-one".to_string());
    visit.left_at = Some(150);
    visit.prompt_tokens = 400;
    visit.cost_usd = Some(0.01);
    core.visits = vec![visit];
    // How long the stage actually worked, as against how long the run was parked
    // in it. A stage that spent an hour waiting on a person has an hour of wall
    // time and seconds of work, and the two must not be read as one.
    core.active = Some(leviath_core::run_meta::ActiveClock {
        banked_secs: 42,
        since: Some(180),
    });

    let record = StageRecord::from(&core);
    assert_eq!(record.name, "build");
    assert_eq!(record.index, 1);
    assert_eq!(record.status, StageStatus::Complete);
    assert!(record.entered);
    assert_eq!(record.usage.prompt_tokens.0, 1_000);
    assert_eq!(record.usage.cached_tokens.0, 700);
    assert_eq!(record.cost.cost_usd.map(|c| c.0), Some(0.02));
    assert!(record.cost.cost_is_exact);
    // Two stays recorded, one of them kept: the roll-ups above are still whole.
    assert_eq!(record.visit_count, 2);
    assert_eq!(record.visits.len(), 1);
    assert_eq!(record.visits[0].entered_at.0, 100);
    assert_eq!(record.visits[0].left_at.map(|t| t.0), Some(150));
    assert_eq!(record.visits[0].usage.prompt_tokens.0, 400);
    // Sorted by region, because the ledger holds these in a map.
    assert_eq!(record.region_peaks[0].region, "conversation");
    assert_eq!(record.region_peaks[0].tokens, 900);
    assert_eq!(record.region_peaks[1].region, "plan");
    assert!(record.runaway_warned);
    assert_eq!(record.started_at.map(|t| t.0), Some(100));
    assert_eq!(record.ended_at.map(|t| t.0), Some(200));
    let working = record.active.expect("the stage kept a working clock");
    assert_eq!(working.banked_secs, 42);
    assert_eq!(working.since.map(|t| t.0), Some(180), "a span in progress");
}

/// What a stage ran on is served in the order it reached each entry, and a
/// stage that has run nothing answers null rather than an empty list.
///
/// The difference is the whole point: an empty list is a claim that the stage
/// ran on nothing, and null is the absence of the answer, which is what a
/// stage the run never entered and a record from an older build both have.
#[test]
fn a_stage_says_what_it_ran_on_or_says_nothing() {
    let mut core = leviath_core::run_meta::StageRecord::new("build".to_string(), 1);
    core.record_model("anthropic", "claude-opus-5");
    core.record_model("openrouter", "anthropic/claude-opus-5");

    let record = StageRecord::from(&core);
    let models = record.models.expect("the stage ran on something");
    assert_eq!(models.len(), 2, "the move is visible, not hidden");
    assert_eq!(models[0].provider, "anthropic");
    assert_eq!(models[0].model, "claude-opus-5");
    assert_eq!(models[1].provider, "openrouter");
    assert_eq!(models[1].model, "anthropic/claude-opus-5");

    let never = leviath_core::run_meta::StageRecord::new("review".to_string(), 2);
    assert!(
        StageRecord::from(&never).models.is_none(),
        "nothing ran here, so there is no answer to give"
    );
}

/// Every stage state the daemon records has one schema value.
#[test]
fn every_stage_status_maps_to_one_value() {
    use leviath_core::run_meta::StageRunStatus as Core;
    let cases = [
        (Core::Pending, StageStatus::Pending),
        (Core::Active, StageStatus::Active),
        (Core::WaitingInput, StageStatus::WaitingInput),
        (Core::Complete, StageStatus::Complete),
        (Core::Error, StageStatus::Error),
        (Core::Skipped, StageStatus::Skipped),
    ];
    for (core, expected) in cases {
        assert_eq!(StageStatus::from(&core), expected, "{core:?}");
    }
}

/// The window reports what it holds, and each region's text is its own field.
///
/// That split is the point: a client drawing a token bar asks for the counts
/// and never pays for the contents.
#[tokio::test]
async fn a_context_window_reports_its_regions() {
    let snapshot = leviath_core::run_meta::ContextSnapshot {
        stage_name: "build".to_string(),
        total_tokens: 1_500,
        max_tokens: 8_000,
        regions: vec![
            leviath_core::run_meta::RegionSnapshot {
                name: "plan".to_string(),
                kind: "pinned".to_string(),
                current_tokens: 500,
                max_tokens: 2_000,
                description: Some("the plan".to_string()),
                entries: vec![
                    leviath_core::run_meta::RegionEntrySnapshot {
                        content: leviath_core::region::EntryContent::text("first"),
                        tokens: 1,
                        kind: Default::default(),
                        metadata: None,
                        key: None,
                        reasoning: None,
                        taint: Default::default(),
                    },
                    leviath_core::run_meta::RegionEntrySnapshot {
                        content: leviath_core::region::EntryContent::text("second"),
                        tokens: 1,
                        kind: Default::default(),
                        metadata: None,
                        key: None,
                        reasoning: None,
                        taint: Default::default(),
                    },
                ],
            },
            leviath_core::run_meta::RegionSnapshot {
                name: "conversation".to_string(),
                kind: "temporary".to_string(),
                current_tokens: 1_000,
                max_tokens: 6_000,
                description: None,
                entries: Vec::new(),
            },
        ],
    };

    let json = ask_window(
        snapshot,
        "{ context { totalTokens maxTokens stageName
                     regions { name kind tokens maxTokens entryCount description content } } }",
    )
    .await;
    let window = &json["context"];
    assert_eq!(window["totalTokens"], 1_500);
    assert_eq!(window["maxTokens"], 8_000);
    assert_eq!(window["stageName"], "build");
    let plan = &window["regions"][0];
    assert_eq!(plan["name"], "plan");
    assert_eq!(plan["kind"], "PINNED");
    assert_eq!(plan["tokens"], 500);
    assert_eq!(plan["entryCount"], 2);
    assert_eq!(plan["description"], "the plan");
    // Entries joined in order, which is what the model reads too.
    assert_eq!(plan["content"], "first\nsecond");
    let conversation = &window["regions"][1];
    assert_eq!(conversation["entryCount"], 0);
    assert_eq!(conversation["content"], "");
    assert!(conversation["description"].is_null());
}

/// Every function `#[mirror]` wrote for this file's types runs at least once.
///
/// The mirrors are straight lines of delegation, so running each of them once
/// is enough to measure all of them. One test per file rather than per query:
/// what a query happens to select is not what the mirror is made of.
#[tokio::test]
async fn every_mirrored_function_runs() {
    exercise_enum(&[WaitReasonKind::ToolApproval, WaitReasonKind::NeedsSetup]).await;
    exercise_enum(&[
        SetupBlocker::ProviderMissing,
        SetupBlocker::ProviderUnreachable,
    ])
    .await;
    exercise(&[WaitReason {
        reason: WaitReasonKind::NeedsSetup,
        blocker: Some(SetupBlocker::CreditsExhausted),
        remedy: Some("top up the account".to_string()),
        outstanding: None,
        needs_a_person: true,
    }])
    .await;
    exercise(&[RunFlags {
        empty_output: false,
        produced_output: true,
        output_forced: 0,
        no_output_tools: false,
        gates_forced: 0,
        max_iterations_hit: 0,
        splits_degraded: 0,
        modified_file_count: 1,
        modified_files: vec!["src/main.rs".to_string()],
        searches_run: 2,
        searches_empty: 0,
        required_regions_abandoned: Vec::new(),
        workspace_lost: false,
    }])
    .await;
    exercise(&[FinalOutput {
        content: "done".to_string(),
        format: Some("markdown".to_string()),
        stage: "build".to_string(),
        submitted_at: Timestamp(100),
        truncated: false,
    }])
    .await;
    exercise_enum(&[StageStatus::Pending, StageStatus::Complete]).await;

    let usage = || TokenUsage {
        prompt_tokens: BigInt(10),
        completion_tokens: BigInt(5),
        cached_tokens: BigInt(2),
        cache_write_tokens: BigInt(1),
    };
    let cost = || CostBreakdown {
        cost_usd: Some(Decimal(0.01)),
        cost_priced_usd: Decimal(0.01),
        cost_is_exact: true,
        unpriced_calls: 0,
    };
    let visit = StageVisit {
        id: Some(async_graphql::ID::from("v-one")),
        ordinal: 1,
        entered_at: Timestamp(10),
        left_at: Some(Timestamp(20)),
        in_progress: false,
        usage: usage(),
        cost: cost(),
    };
    exercise(std::slice::from_ref(&visit)).await;
    exercise_list(std::slice::from_ref(&visit)).await;

    let peak = RegionPeak {
        region: "plan".to_string(),
        tokens: 100,
    };
    exercise(std::slice::from_ref(&peak)).await;
    exercise_list(std::slice::from_ref(&peak)).await;

    let model_use = StageModelUse {
        provider: "anthropic".to_string(),
        model: "claude-opus-5".to_string(),
    };
    exercise(std::slice::from_ref(&model_use)).await;
    exercise_list(std::slice::from_ref(&model_use)).await;

    let record = StageRecord {
        name: "build".to_string(),
        index: 0,
        status: StageStatus::Active,
        entered: true,
        usage: usage(),
        cost: cost(),
        models: Some(vec![model_use]),
        visit_count: 1,
        visits: vec![visit],
        region_peaks: vec![peak],
        runaway_warned: false,
        started_at: Some(Timestamp(10)),
        ended_at: None,
        active: Some(WorkingClock {
            banked_secs: 5,
            since: Some(Timestamp(20)),
        }),
    };
    exercise(std::slice::from_ref(&record)).await;
    exercise_list(std::slice::from_ref(&record)).await;

    let snapshot = Arc::new(leviath_core::run_meta::ContextSnapshot {
        stage_name: "build".to_string(),
        total_tokens: 100,
        max_tokens: 1_000,
        regions: vec![leviath_core::run_meta::RegionSnapshot {
            name: "plan".to_string(),
            kind: "pinned".to_string(),
            current_tokens: 10,
            max_tokens: 100,
            description: None,
            entries: Vec::new(),
        }],
    });
    let region = ContextRegion {
        snapshot: Arc::clone(&snapshot),
        at: 0,
    };
    exercise(std::slice::from_ref(&region)).await;
    exercise_list(std::slice::from_ref(&region)).await;
    exercise(&[ContextWindow { snapshot }]).await;

    let blob = BlobEntry {
        sha256: "abc".to_string(),
        mime_type: "image/png".to_string(),
        name: Some("logo.png".to_string()),
        size: BigInt(1_024),
        width: Some(64),
        height: Some(64),
        duration_ms: None,
        tokens: 20,
        regions: vec!["conversation".to_string()],
        stored: true,
        url: Some("https://example.test/blob".to_string()),
    };
    exercise(std::slice::from_ref(&blob)).await;
    exercise_list(std::slice::from_ref(&blob)).await;

    let artifact = Artifact {
        name: "report.md".to_string(),
        mime_type: "text/markdown".to_string(),
        size: Some(BigInt(512)),
        sha256: Some("def".to_string()),
        path: "report.md".to_string(),
        url: "https://example.test/artifact".to_string(),
    };
    exercise(std::slice::from_ref(&artifact)).await;
    exercise_list(std::slice::from_ref(&artifact)).await;
}

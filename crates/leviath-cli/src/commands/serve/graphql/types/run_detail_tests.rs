//! Tests for a run's detail: why it is parked, what it produced, what each
//! stage cost, and the window it is working in.

use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};

use super::{
    ContextWindow, RunFlags, SetupBlocker, StageRecord, StageStatus, WaitReason, WaitReasonKind,
};

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

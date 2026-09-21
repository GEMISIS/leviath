//! Tests for the run filter.
//!
//! Three things are checked. That a filter reaches the listing as the
//! selection it describes, with every rejection happening where it is read.
//! That the compiled matcher keeps exactly the runs the filter names, one
//! field at a time and through nested combinators. And that a value of the
//! wrong type is refused wherever it sits in the object, last field included.

use std::collections::{HashMap, HashSet};

use async_graphql::{ID, InputType, Name, Value, indexmap::IndexMap};

use super::{RunFilter, RunMatcher, RunScope, RunSort, SearchScope, compile, page_size};
use crate::commands::serve::core::runs::predicate::{MatchContext, RunPredicate};
use crate::commands::serve::core::runs::{MAX_IDS, MAX_LIMIT, ParentFilter, SortKey, Source};
use crate::commands::serve::graphql::filters::{
    BooleanFilter, DecimalFilter, IntFilter, StringFilter, TimestampFilter,
};
use crate::commands::serve::graphql::inputs::BlueprintInput;
use crate::commands::serve::graphql::scalars::{Decimal, Timestamp};
use crate::commands::serve::graphql::types::run::RunStatus;
use crate::commands::serve::graphql::types::run_detail::WaitReasonKind;
use crate::commands::serve::testutil::state_with_agent_paths;
use crate::runstate::RunMeta;

/// A run recorded at a known second, so ordering and ages are assertable.
fn meta(id: &str) -> RunMeta {
    let mut record = RunMeta::new(
        id.to_string(),
        "coder".to_string(),
        "/agents/coder".to_string(),
        "ship the release".to_string(),
        None,
        "/work".to_string(),
        1,
    );
    record.started_at = 1_000;
    record.updated_at = 2_000;
    record
}

/// A context measuring every age from one instant, with no subtree resolved.
fn at(now: i64) -> MatchContext {
    MatchContext {
        now,
        subtrees: HashMap::new(),
    }
}

/// Compile one filter with no daemon behind it.
async fn compiled(filter: RunFilter) -> RunMatcher {
    let state = state_with_agent_paths(Vec::new());
    compile(filter, &state).await.expect("the filter compiles")
}

/// One object value, built from its fields in the order they are given.
fn object(fields: &[(&str, Value)]) -> Value {
    let mut map = IndexMap::new();
    for (name, value) in fields {
        map.insert(Name::new(*name), value.clone());
    }
    Value::Object(map)
}

// ─── page size ──────────────────────────────────────────────────────────────

/// A page size over the cap is refused rather than quietly cut down: a client
/// that asked for 500 and silently got 200 finds out by missing rows.
#[test]
fn a_page_size_is_bounded_at_both_ends() {
    assert_eq!(page_size(50).expect("in range"), 50);
    assert_eq!(page_size(MAX_LIMIT as i32).expect("at the cap"), MAX_LIMIT);
    let over = page_size(MAX_LIMIT as i32 + 1).expect_err("over the cap");
    assert!(over.to_string().contains("page-size cap"), "{over}");
    assert!(page_size(0).is_err(), "zero is not a page");
    assert!(page_size(-1).is_err(), "a negative page size is not a page");
}

// ─── the selection ──────────────────────────────────────────────────────────

/// The defaults: newest first, fifty at a time, searching only what is already
/// in memory, and no predicate at all.
#[tokio::test]
async fn the_default_filter_reads_nothing_from_disk() {
    let state = state_with_agent_paths(Vec::new());
    let selection = RunFilter::default()
        .selection(&state, 50)
        .await
        .expect("defaults resolve");
    assert_eq!(selection.limit, 50);
    assert_eq!(selection.sort, SortKey::Started);
    assert!(selection.descending);
    assert_eq!(selection.sources, vec![Source::Meta, Source::Files]);
    assert_eq!(selection.parent, ParentFilter::Any);
    assert!(selection.fields.is_none(), "projection is REST's, not ours");
    assert!(
        selection.predicate.is_none(),
        "an empty filter must digest as no filter, or every held cursor breaks"
    );
    assert!(selection.ids.is_none());
}

/// Sort, direction and search reach the listing, and the scopes digest as the
/// request spelled them.
#[tokio::test]
async fn the_listing_level_fields_travel_to_the_core() {
    let state = state_with_agent_paths(Vec::new());
    let selection = RunFilter {
        sort: Some(RunSort::Updated),
        ascending: Some(true),
        query: Some("boom".to_string()),
        query_in: Some(vec![SearchScope::Logs, SearchScope::Meta]),
        ..Default::default()
    }
    .selection(&state, 10)
    .await
    .expect("the listing fields resolve");
    assert_eq!(selection.sort, SortKey::Updated);
    assert!(!selection.descending);
    assert_eq!(selection.q.as_deref(), Some("boom"));
    assert_eq!(selection.sources, vec![Source::Logs, Source::Meta]);
    assert_eq!(selection.sources_raw, "logs,meta");

    let last = RunFilter {
        sort: Some(RunSort::LastProgress),
        ..Default::default()
    }
    .selection(&state, 10)
    .await
    .expect("the sort resolves");
    assert_eq!(last.sort, SortKey::LastProgress);
}

/// Every search scope maps to the source it reads, and digests as the request
/// spelled it.
#[tokio::test]
async fn every_search_scope_maps_to_one_source() {
    let state = state_with_agent_paths(Vec::new());
    let cases = [
        (SearchScope::Meta, Source::Meta, "meta"),
        (SearchScope::Files, Source::Files, "files"),
        (SearchScope::Context, Source::Context, "context"),
        (SearchScope::Logs, Source::Logs, "logs"),
        (SearchScope::Journal, Source::Journal, "journal"),
    ];
    for (scope, source, word) in cases {
        let selection = RunFilter {
            query: Some("x".to_string()),
            query_in: Some(vec![scope]),
            ..Default::default()
        }
        .selection(&state, 50)
        .await
        .expect("the scope resolves");
        assert_eq!(selection.sources, vec![source], "{word}");
        assert_eq!(selection.sources_raw, word);
        assert_eq!(
            source.reads_filesystem(),
            scope != SearchScope::Meta && scope != SearchScope::Files
        );
    }
}

/// A field that describes the listing has no meaning inside a combinator, and
/// says so rather than being ignored.
#[tokio::test]
async fn a_listing_level_field_is_refused_inside_a_combinator() {
    let state = state_with_agent_paths(Vec::new());
    let cases = [
        (
            RunFilter {
                query: Some("boom".to_string()),
                ..Default::default()
            },
            "query",
        ),
        (
            RunFilter {
                query_in: Some(vec![SearchScope::Logs]),
                ..Default::default()
            },
            "queryIn",
        ),
        (
            RunFilter {
                sort: Some(RunSort::Updated),
                ..Default::default()
            },
            "sort",
        ),
        (
            RunFilter {
                ascending: Some(true),
                ..Default::default()
            },
            "ascending",
        ),
    ];
    for (nested, named) in cases {
        let refused = RunFilter {
            and: Some(vec![nested]),
            ..Default::default()
        }
        .selection(&state, 50)
        .await
        .expect_err("a listing field inside a combinator is refused");
        assert!(refused.to_string().contains(named), "{refused}");
    }

    // Inside an alternation too, which compiles its members on its own.
    let alternative = RunFilter {
        or: Some(vec![RunFilter {
            sort: Some(RunSort::Updated),
            ..Default::default()
        }]),
        ..Default::default()
    }
    .selection(&state, 50)
    .await
    .expect_err("a listing field inside `or` is refused");
    assert!(alternative.to_string().contains("sort"), "{alternative}");

    // And every one of them at once, named together.
    let all_four = RunFilter {
        not: Some(Box::new(RunFilter {
            query: Some("boom".to_string()),
            query_in: Some(vec![SearchScope::Meta]),
            sort: Some(RunSort::Updated),
            ascending: Some(false),
            ..Default::default()
        })),
        ..Default::default()
    }
    .selection(&state, 50)
    .await
    .expect_err("all four are refused");
    for named in ["query", "queryIn", "sort", "ascending"] {
        assert!(all_four.to_string().contains(named), "{all_four}");
    }
}

/// `ids` on the filter a request passes says which runs to read as well as
/// which to keep, and more than one request may name is refused.
#[tokio::test]
async fn ids_say_which_runs_to_read_and_are_capped() {
    let state = state_with_agent_paths(Vec::new());
    let selection = RunFilter {
        ids: Some(vec![ID("run-a".to_string())]),
        status: Some(RunStatus::Running),
        ..Default::default()
    }
    .selection(&state, 50)
    .await
    .expect("ids compose with a filter");
    assert_eq!(selection.ids, Some(vec!["run-a".to_string()]));
    assert!(
        selection.predicate.is_some(),
        "the rest of the filter still applies to what comes back"
    );

    let too_many: Vec<ID> = (0..=MAX_IDS).map(|i| ID(format!("run-{i}"))).collect();
    let over = RunFilter {
        ids: Some(too_many.clone()),
        ..Default::default()
    }
    .selection(&state, 50)
    .await
    .expect_err("too many ids is refused");
    assert!(over.to_string().contains("at most"), "{over}");

    // And nested, where the same cap applies to the membership test.
    let nested = RunFilter {
        not: Some(Box::new(RunFilter {
            ids: Some(too_many),
            ..Default::default()
        })),
        ..Default::default()
    }
    .selection(&state, 50)
    .await
    .expect_err("too many nested ids is refused");
    assert!(nested.to_string().contains("at most"), "{nested}");
}

/// An export takes the same filter unpaged, so every other bound the listing
/// enforces still holds.
#[tokio::test]
async fn an_export_takes_the_filter_unpaged() {
    let state = state_with_agent_paths(Vec::new());
    let selection = RunFilter {
        status: Some(RunStatus::Complete),
        ..Default::default()
    }
    .everything(&state)
    .await
    .expect("an export resolves");
    assert_eq!(selection.limit, usize::MAX);
    assert!(selection.predicate.is_some());

    let refused = RunFilter {
        ids: Some((0..=MAX_IDS).map(|i| ID(format!("run-{i}"))).collect()),
        ..Default::default()
    }
    .everything(&state)
    .await
    .expect_err("an export is still bounded");
    assert!(refused.to_string().contains("at most"), "{refused}");
}

/// A blueprint reference pinned to a revision that is not the installed one is
/// refused where it is read, before a single run is looked at.
///
/// Runs are matched by the name they recorded, whatever is installed now, so
/// the pin is the only thing that tells a client asking for "this agent's runs"
/// that the agent has been edited under it.
#[tokio::test]
async fn a_stale_blueprint_pin_is_refused_before_anything_is_read() {
    let agents = tempfile::tempdir().expect("a temp agents dir");
    let dir = agents.path().join("drifted");
    std::fs::create_dir_all(&dir).expect("the agent directory");
    std::fs::write(
        dir.join(leviath_core::files::MANIFEST_FILENAME),
        "[agent]\nname = \"drifted\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\n\
         [stages.only]\nmode = \"autonomous\"\n",
    )
    .expect("the manifest is written");
    let stale = "0".repeat(64);

    crate::commands::serve::blueprints::TEST_AGENTS_DIR
        .scope(agents.path().to_path_buf(), async move {
            let state = state_with_agent_paths(Vec::new());
            let drifted = RunFilter {
                blueprint: Some(BlueprintInput {
                    name: "drifted".to_string(),
                    digest: Some(stale),
                }),
                ..Default::default()
            }
            .selection(&state, 50)
            .await
            .expect_err("the pin is stale");
            assert!(drifted.to_string().contains("drifted"), "{drifted}");
        })
        .await;
}

// ─── the matcher ────────────────────────────────────────────────────────────

/// Every field of the filter keeps the runs it names and drops the rest.
#[tokio::test]
async fn every_field_keeps_what_it_names() {
    let mut run = meta("coder-1");
    run.title = Some("Ship the release".to_string());
    run.yolo = true;
    run.yolo_profile = Some("careful".to_string());
    run.cost_usd = Some(0.25);
    run.status = leviath_core::run_meta::RunStatus::Running;
    run.waiting_on = None;

    run.stage_models = vec![
        leviath_core::run_meta::StageModelUse {
            provider: "anthropic".to_string(),
            model: "claude-opus-5".to_string(),
        },
        leviath_core::run_meta::StageModelUse {
            provider: "openai".to_string(),
            model: "gpt-5.5".to_string(),
        },
    ];

    let mut other = meta("writer-1");
    other.agent_name = "writer".to_string();
    other.task = "write the notes".to_string();
    other.title = None;
    other.yolo = false;
    other.yolo_profile = None;
    other.cost_usd = None;
    other.started_at = 5_000;
    other.updated_at = 6_000;
    other.status = leviath_core::run_meta::RunStatus::Complete;

    let text = |value: &str| StringFilter {
        eq: Some(value.to_string()),
        ..StringFilter::default()
    };
    let cases: Vec<(&str, RunFilter)> = vec![
        (
            "ids",
            RunFilter {
                ids: Some(vec![ID("coder-1".to_string())]),
                ..Default::default()
            },
        ),
        (
            "status",
            RunFilter {
                status: Some(RunStatus::Running),
                ..Default::default()
            },
        ),
        (
            "statusIn",
            RunFilter {
                status_in: Some(vec![RunStatus::Running, RunStatus::Paused]),
                ..Default::default()
            },
        ),
        (
            "title",
            RunFilter {
                title: Some(StringFilter {
                    contains: Some("release".to_string()),
                    ..StringFilter::default()
                }),
                ..Default::default()
            },
        ),
        (
            "task",
            RunFilter {
                task: Some(text("ship the release")),
                ..Default::default()
            },
        ),
        (
            "blueprintName",
            RunFilter {
                blueprint_name: Some(text("coder")),
                ..Default::default()
            },
        ),
        (
            "unattended",
            RunFilter {
                unattended: Some(BooleanFilter {
                    eq: Some(true),
                    ..BooleanFilter::default()
                }),
                ..Default::default()
            },
        ),
        (
            "yoloProfileName",
            RunFilter {
                yolo_profile_name: Some(text("careful")),
                ..Default::default()
            },
        ),
        (
            "startedAt",
            RunFilter {
                started_at: Some(TimestampFilter {
                    lt: Some(Timestamp(2_000)),
                    ..TimestampFilter::default()
                }),
                ..Default::default()
            },
        ),
        (
            "updatedAt",
            RunFilter {
                updated_at: Some(TimestampFilter {
                    lte: Some(Timestamp(2_000)),
                    ..TimestampFilter::default()
                }),
                ..Default::default()
            },
        ),
        (
            "ageSecs",
            RunFilter {
                age_secs: Some(IntFilter {
                    gte: Some(1_000),
                    ..IntFilter::default()
                }),
                ..Default::default()
            },
        ),
        (
            "costUsd",
            RunFilter {
                cost_usd: Some(DecimalFilter {
                    lte: Some(Decimal(0.25)),
                    ..DecimalFilter::default()
                }),
                ..Default::default()
            },
        ),
        // The second stage's provider, not the first: the question is about
        // any stage of the run, so matching only the entry stage would pass a
        // filter that reads the run-level `model` by another name.
        (
            "stageProvider",
            RunFilter {
                stage_provider: Some(text("openai")),
                ..Default::default()
            },
        ),
        (
            "stageModel",
            RunFilter {
                stage_model: Some(text("gpt-5.5")),
                ..Default::default()
            },
        ),
    ];
    for (named, filter) in cases {
        let matcher = compiled(filter).await;
        assert!(matcher.matches(&run, &at(2_000)), "{named} keeps the run");
        assert!(
            !matcher.matches(&other, &at(2_000)),
            "{named} drops the other"
        );
    }
}

/// Working time is measured from the moment the page was built, like every
/// other duration in one answer.
#[tokio::test]
async fn working_time_is_measured_from_the_page_instant() {
    let mut run = meta("coder-1");
    run.active = Some(leviath_core::run_meta::ActiveClock {
        banked_secs: 30,
        since: None,
    });
    let matcher = compiled(RunFilter {
        working_secs: Some(IntFilter {
            gte: Some(30),
            lt: Some(31),
            ..IntFilter::default()
        }),
        ..Default::default()
    })
    .await;
    assert!(matcher.matches(&run, &at(9_999)));

    let idle = meta("coder-2");
    assert!(!matcher.matches(&idle, &at(9_999)));
}

/// A parked run is matched on why it is parked, and a run that is not parked
/// matches no reason at all.
#[tokio::test]
async fn a_wait_reason_selects_parked_runs() {
    let mut parked = meta("coder-1");
    parked.waiting_on = Some(leviath_core::run_meta::WaitReason::UserPrompt);
    let mut workers = meta("coder-2");
    workers.waiting_on = Some(leviath_core::run_meta::WaitReason::FanOutWorkers { outstanding: 3 });
    let moving = meta("coder-3");

    let asked = compiled(RunFilter {
        wait_reason: Some(WaitReasonKind::UserPrompt),
        ..Default::default()
    })
    .await;
    assert!(asked.matches(&parked, &at(2_000)));
    assert!(!asked.matches(&workers, &at(2_000)));
    assert!(!asked.matches(&moving, &at(2_000)));

    let either = compiled(RunFilter {
        wait_reason_in: Some(vec![
            WaitReasonKind::UserPrompt,
            WaitReasonKind::FanOutWorkers,
        ]),
        ..Default::default()
    })
    .await;
    assert!(either.matches(&parked, &at(2_000)));
    assert!(either.matches(&workers, &at(2_000)));
    assert!(!either.matches(&moving, &at(2_000)));
}

/// The three-way scope replaces a pair of mirror booleans, and each value
/// selects exactly its own part of the tree.
#[tokio::test]
async fn the_scope_selects_one_part_of_the_tree() {
    let root = meta("root");
    let mut worker = meta("worker");
    worker.parent_run_id = Some("root".to_string());

    for (scope, keeps_root, keeps_worker) in [
        (RunScope::All, true, true),
        (RunScope::TopLevel, true, false),
        (RunScope::SubAgents, false, true),
    ] {
        let matcher = compiled(RunFilter {
            scope: Some(scope),
            ..Default::default()
        })
        .await;
        assert_eq!(matcher.matches(&root, &at(2_000)), keeps_root, "{scope:?}");
        assert_eq!(
            matcher.matches(&worker, &at(2_000)),
            keeps_worker,
            "{scope:?}"
        );
    }
}

/// `parent` is one level and `descendantOf` is the whole subtree, which needs
/// the tree rather than the record.
#[tokio::test]
async fn parentage_reads_one_level_or_the_whole_subtree() {
    let mut worker = meta("worker");
    worker.parent_run_id = Some("root".to_string());
    let mut grandchild = meta("grandchild");
    grandchild.parent_run_id = Some("worker".to_string());

    let children = compiled(RunFilter {
        parent: Some(ID("root".to_string())),
        ..Default::default()
    })
    .await;
    assert!(children.matches(&worker, &at(2_000)));
    assert!(!children.matches(&grandchild, &at(2_000)));

    let subtree = compiled(RunFilter {
        descendant_of: Some(ID("root".to_string())),
        ..Default::default()
    })
    .await;
    let mut roots = Vec::new();
    subtree.subtree_roots(&mut roots);
    assert_eq!(roots, vec!["root".to_string()]);

    let ctx = MatchContext {
        now: 2_000,
        subtrees: HashMap::from([(
            "root".to_string(),
            HashSet::from(["worker".to_string(), "grandchild".to_string()]),
        )]),
    };
    assert!(subtree.matches(&worker, &ctx));
    assert!(subtree.matches(&grandchild, &ctx));
    assert!(!subtree.matches(&meta("stranger"), &ctx));

    // A subtree nobody resolved keeps nothing, rather than keeping everything.
    assert!(!subtree.matches(&worker, &at(2_000)));
}

/// Every run the tree asks about is reported, through whichever combinator it
/// sits under.
#[tokio::test]
async fn every_subtree_the_filter_names_is_reported() {
    let matcher = compiled(RunFilter {
        and: Some(vec![RunFilter {
            descendant_of: Some(ID("a".to_string())),
            ..Default::default()
        }]),
        or: Some(vec![RunFilter {
            descendant_of: Some(ID("b".to_string())),
            ..Default::default()
        }]),
        not: Some(Box::new(RunFilter {
            descendant_of: Some(ID("c".to_string())),
            ..Default::default()
        })),
        status: Some(RunStatus::Running),
        ..Default::default()
    })
    .await;
    let mut roots = Vec::new();
    matcher.subtree_roots(&mut roots);
    roots.sort();
    assert_eq!(
        roots,
        vec!["a".to_string(), "b".to_string(), "c".to_string()]
    );

    // A filter that asks about no subtree reports none, so the listing walks
    // no tree at all.
    let plain = compiled(RunFilter {
        status: Some(RunStatus::Running),
        ..Default::default()
    })
    .await;
    let mut none = Vec::new();
    plain.subtree_roots(&mut none);
    assert!(none.is_empty());
}

/// The combinators compose, and nest inside one another.
#[tokio::test]
async fn the_combinators_compose_and_nest() {
    let mut running = meta("coder-1");
    running.status = leviath_core::run_meta::RunStatus::Running;
    let mut failed = meta("coder-2");
    failed.status = leviath_core::run_meta::RunStatus::Error;
    let mut cancelled = meta("coder-3");
    cancelled.status = leviath_core::run_meta::RunStatus::Cancelled;

    let of = |status: RunStatus| RunFilter {
        status: Some(status),
        ..Default::default()
    };

    let either = compiled(RunFilter {
        or: Some(vec![of(RunStatus::Running), of(RunStatus::Error)]),
        ..Default::default()
    })
    .await;
    assert!(either.matches(&running, &at(2_000)));
    assert!(either.matches(&failed, &at(2_000)));
    assert!(!either.matches(&cancelled, &at(2_000)));

    let both = compiled(RunFilter {
        and: Some(vec![
            of(RunStatus::Running),
            RunFilter {
                blueprint_name: Some(StringFilter {
                    eq: Some("coder".to_string()),
                    ..StringFilter::default()
                }),
                ..Default::default()
            },
        ]),
        ..Default::default()
    })
    .await;
    assert!(both.matches(&running, &at(2_000)));
    assert!(!both.matches(&failed, &at(2_000)));

    // Nested three deep: not(or(running, error)) inside an and.
    let nested = compiled(RunFilter {
        and: Some(vec![RunFilter {
            not: Some(Box::new(RunFilter {
                or: Some(vec![of(RunStatus::Running), of(RunStatus::Error)]),
                ..Default::default()
            })),
            ..Default::default()
        }]),
        ..Default::default()
    })
    .await;
    assert!(nested.matches(&cancelled, &at(2_000)));
    assert!(!nested.matches(&running, &at(2_000)));
    assert!(!nested.matches(&failed, &at(2_000)));

    // An alternation with no alternatives selects nothing.
    let nothing = compiled(RunFilter {
        or: Some(Vec::new()),
        ..Default::default()
    })
    .await;
    assert!(!nothing.matches(&running, &at(2_000)));
}

/// A run matches when *any* of its stages ran on what was asked for, the
/// filter nests like every other field, and a run with nothing recorded
/// matches nothing rather than falling back to its entry stage.
#[tokio::test]
async fn a_stage_model_filter_asks_about_every_stage() {
    use leviath_core::run_meta::StageModelUse;

    let used = |provider: &str, model: &str| StageModelUse {
        provider: provider.to_string(),
        model: model.to_string(),
    };

    let mut moved = meta("coder-1");
    moved.model = Some("anthropic/claude-opus-5".to_string());
    moved.stage_models = vec![
        used("anthropic", "claude-opus-5"),
        used("openai", "gpt-5.5"),
    ];

    let mut stayed = meta("coder-2");
    stayed.model = Some("anthropic/claude-opus-5".to_string());
    stayed.stage_models = vec![used("anthropic", "claude-opus-5")];

    // A run from a build that never recorded this: it names a model at the run
    // level and has nothing to say about its stages.
    let mut older = meta("coder-3");
    older.model = Some("openai/gpt-5.5".to_string());
    assert!(older.stage_models.is_empty());

    let on = |model: &str| RunFilter {
        stage_model: Some(StringFilter {
            eq: Some(model.to_string()),
            ..StringFilter::default()
        }),
        ..Default::default()
    };

    // The second stage counts as much as the first.
    let late = compiled(on("gpt-5.5")).await;
    assert!(late.matches(&moved, &at(2_000)));
    assert!(!late.matches(&stayed, &at(2_000)));
    assert!(
        !late.matches(&older, &at(2_000)),
        "the run-level model is the entry stage's and is not evidence here"
    );

    // `ne` is existential too: it keeps a run with a stage on something else,
    // which is not the same as a run no stage of which ran on this.
    let not_opus = compiled(RunFilter {
        stage_model: Some(StringFilter {
            ne: Some("claude-opus-5".to_string()),
            ..StringFilter::default()
        }),
        ..Default::default()
    })
    .await;
    assert!(not_opus.matches(&moved, &at(2_000)), "its second stage is");
    assert!(!not_opus.matches(&stayed, &at(2_000)));

    // Which makes `not` the way to ask the other question.
    let never_opus = compiled(RunFilter {
        not: Some(Box::new(on("claude-opus-5"))),
        ..Default::default()
    })
    .await;
    assert!(!never_opus.matches(&moved, &at(2_000)));
    assert!(!never_opus.matches(&stayed, &at(2_000)));
    assert!(never_opus.matches(&older, &at(2_000)));

    // And it composes inside the combinators like every other field.
    let either = compiled(RunFilter {
        or: Some(vec![
            on("gpt-5.4"),
            RunFilter {
                stage_provider: Some(StringFilter {
                    eq: Some("openai".to_string()),
                    ..StringFilter::default()
                }),
                ..Default::default()
            },
        ]),
        ..Default::default()
    })
    .await;
    assert!(either.matches(&moved, &at(2_000)));
    assert!(!either.matches(&stayed, &at(2_000)));

    // The two fields are separate conditions, not one pair: a run whose
    // provider came from one stage and model from another satisfies both.
    let split = compiled(RunFilter {
        stage_provider: Some(StringFilter {
            eq: Some("anthropic".to_string()),
            ..StringFilter::default()
        }),
        stage_model: Some(StringFilter {
            eq: Some("gpt-5.5".to_string()),
            ..StringFilter::default()
        }),
        ..Default::default()
    })
    .await;
    assert!(split.matches(&moved, &at(2_000)));
    assert!(!split.matches(&stayed, &at(2_000)));
}

/// Two fields set on one object both have to hold, which is what makes `and`
/// the default and leaves `or` to say otherwise.
#[tokio::test]
async fn two_fields_on_one_object_both_have_to_hold() {
    let mut running = meta("coder-1");
    running.status = leviath_core::run_meta::RunStatus::Running;
    let matcher = compiled(RunFilter {
        status: Some(RunStatus::Running),
        blueprint_name: Some(StringFilter {
            eq: Some("writer".to_string()),
            ..StringFilter::default()
        }),
        ..Default::default()
    })
    .await;
    assert!(!matcher.matches(&running, &at(2_000)));
}

/// Two different filters digest differently, and the same filter digests the
/// same way twice, which is what a cursor's promise rests on.
#[tokio::test]
async fn the_digest_follows_the_filter() {
    let of = |status: RunStatus| RunFilter {
        status: Some(status),
        ..Default::default()
    };
    let running = compiled(of(RunStatus::Running)).await.digest_part();
    let again = compiled(of(RunStatus::Running)).await.digest_part();
    let failed = compiled(of(RunStatus::Error)).await.digest_part();
    assert_eq!(running, again);
    assert_ne!(running, failed);
}

// ─── reading one off the wire ───────────────────────────────────────────────

/// A value of the wrong type is refused wherever it sits, first field or last.
#[test]
fn the_run_filter_refuses_what_it_cannot_read() {
    let wrong = Value::Number(7.into());
    assert!(RunFilter::parse(Some(Value::String("nope".into()))).is_err());
    assert!(RunFilter::parse(Some(object(&[("and", wrong.clone())]))).is_err());
    assert!(
        RunFilter::parse(Some(object(&[
            ("query", Value::String("parser".into())),
            ("statusIn", wrong.clone()),
        ])))
        .is_err()
    );
    assert!(
        RunFilter::parse(Some(object(&[
            ("query", Value::String("parser".into())),
            ("costUsd", wrong.clone()),
        ])))
        .is_err()
    );
    assert!(
        RunFilter::parse(Some(object(&[
            ("query", Value::String("parser".into())),
            ("ascending", wrong),
        ])))
        .is_err(),
        "the last field is read after every other one"
    );
}

/// A combinator reads the same type it sits on, however deep it goes.
#[test]
fn a_combinator_reads_the_filter_it_holds() {
    let inner = object(&[("status", Value::Enum(Name::new("RUNNING")))]);
    let nested = RunFilter::parse(Some(object(&[
        ("and", Value::List(vec![inner.clone()])),
        ("not", inner),
    ])))
    .expect("a nested filter reads");
    assert!(nested.and.is_some());
    assert!(nested.not.is_some());

    let broken = object(&[("status", Value::Number(7.into()))]);
    assert!(RunFilter::parse(Some(object(&[("not", broken)]))).is_err());
}

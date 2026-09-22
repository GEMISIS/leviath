//! Tests for what fills a region, and for the mirrors of those types.

use async_graphql::Value;

use super::{
    RegionAdmission, RegionSeed, RegionStrategy, RegionVolatility, SeedFromCaller, SeedFromCommand,
    SeedFromFiles, SeedFromGlob, SeedFromLiteral, SeedFromScript, SeedFromTools, SeedRefresh,
    SeedToolCall,
};
use crate::commands::serve::graphql::filter::testkit::{exercise, exercise_enum, exercise_list};
use crate::commands::serve::graphql::scalars::Json;

/// One call, as a manifest would write it.
fn call() -> SeedToolCall {
    SeedToolCall {
        tool: "read_file".to_string(),
        args: Json(serde_json::json!({ "path": "README.md" })),
    }
}

/// Every function `#[mirror]` wrote for this file's types runs at least once.
///
/// A union is matched on, so it takes one value per variant: the generated
/// code has one arm each, and an arm nothing reaches is an arm nothing
/// measures.
#[tokio::test]
async fn every_mirrored_function_runs() {
    exercise_enum(&[RegionVolatility::Stable, RegionVolatility::Rewritten]).await;
    exercise_enum(&[RegionAdmission::Evict, RegionAdmission::Reject]).await;
    exercise_enum(&[RegionStrategy::PerItem, RegionStrategy::Compact]).await;
    exercise_enum(&[SeedRefresh::Once, SeedRefresh::EachStage]).await;

    let calls = vec![call()];
    exercise(&calls).await;
    exercise_list(&calls).await;

    exercise(&[SeedFromCaller {
        key: "task".to_string(),
    }])
    .await;
    exercise(&[SeedFromGlob {
        pattern: "*.rs".to_string(),
    }])
    .await;
    exercise(&[SeedFromFiles {
        paths: vec!["README.md".to_string()],
    }])
    .await;
    exercise(&[SeedFromLiteral {
        text: "hello".to_string(),
    }])
    .await;
    exercise(&[SeedFromScript {
        script: "seed.rhai".to_string(),
    }])
    .await;
    exercise(&[SeedFromCommand {
        command: "git status".to_string(),
    }])
    .await;
    exercise(&[SeedFromTools {
        calls: vec![call()],
        refresh: SeedRefresh::Once,
    }])
    .await;

    exercise(&[
        RegionSeed::Caller(SeedFromCaller {
            key: "task".to_string(),
        }),
        RegionSeed::Glob(SeedFromGlob {
            pattern: "*.rs".to_string(),
        }),
        RegionSeed::Files(SeedFromFiles { paths: Vec::new() }),
        RegionSeed::Literal(SeedFromLiteral {
            text: String::new(),
        }),
        RegionSeed::Script(SeedFromScript {
            script: String::new(),
        }),
        RegionSeed::Command(SeedFromCommand {
            command: String::new(),
        }),
        RegionSeed::Tools(SeedFromTools {
            calls: Vec::new(),
            refresh: SeedRefresh::EachStage,
        }),
    ])
    .await;
}

/// A seed's variant filter names the variant, and refuses the others.
#[test]
fn a_seed_filter_names_one_variant() {
    use crate::commands::serve::graphql::filter::{Filterable, MatchCx, Tri};

    let cx = MatchCx::at(0);
    let seed = RegionSeed::Literal(SeedFromLiteral {
        text: "hello".to_string(),
    });
    let wants_a_literal = super::RegionSeedFilter {
        seed_from_literal: Some(Box::new(super::SeedFromLiteralFilter {
            text: Some(Box::new(
                crate::commands::serve::graphql::filter::scalars::StringFilter {
                    eq: Some("hello".to_string()),
                    ..Default::default()
                },
            )),
            ..Default::default()
        })),
        ..Default::default()
    };
    assert_eq!(seed.test(&wants_a_literal, &cx), Tri::Yes);

    let wants_a_glob = super::RegionSeedFilter {
        seed_from_glob: Some(Box::new(super::SeedFromGlobFilter::default())),
        ..Default::default()
    };
    assert_eq!(
        seed.test(&wants_a_glob, &cx),
        Tri::No,
        "a value of another variant never matches"
    );
}

/// The mirrors read off the wire the way a client writes them.
#[test]
fn a_seed_filter_is_read_from_the_wire() {
    use async_graphql::InputType;

    let value = Value::from_json(serde_json::json!({
        "seedFromLiteral": { "text": { "eq": "hello" } }
    }))
    .expect("a filter value");
    let filter = super::RegionSeedFilter::parse(Some(value)).expect("a seed filter");
    assert!(filter.seed_from_literal.is_some());
    assert!(super::RegionSeedFilter::parse(Some(Value::Boolean(true))).is_err());
}

//! Tests for the blueprint-level and region-level detail.
//!
//! The same manifest the stage tests use, read from the top: what the agent
//! block declares about the whole run, and what each region declares about
//! itself.

use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};

use super::super::blueprint::Blueprint;
use crate::commands::serve::core::blueprints::{BlueprintSource as CoreSource, digest_of};

/// Ask the schema about the manifest the stage tests declare.
async fn ask(query: &str) -> serde_json::Value {
    let text = super::stage_tests::manifest();
    let parsed = leviath_core::manifest::parse_manifest(&text).expect("the manifest parses");
    let schema = Schema::build(
        Probe {
            blueprint: Blueprint {
                parsed: Arc::new(parsed),
                digest: digest_of(&text),
                source: CoreSource::Snapshot.into(),
            },
        },
        EmptyMutation,
        EmptySubscription,
    )
    .data(crate::commands::serve::testutil::state_with_agent_paths(
        Vec::new(),
    ))
    .finish();
    let answer = schema.execute(Request::new(query)).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).expect("data serializes")
}

/// A root handing out the blueprint.
struct Probe {
    blueprint: Blueprint,
}

#[async_graphql::Object]
impl Probe {
    /// The blueprint under test.
    async fn blueprint(&self) -> &Blueprint {
        &self.blueprint
    }
}

/// The blocks that decide how a whole run behaves come back.
#[tokio::test]
async fn a_blueprint_carries_its_run_wide_settings() {
    let json = ask(r#"{ blueprint {
             security { taintTracking }
             sandbox { kind image allowNetwork mounts onUnavailable }
             nudge { policy max text }
             compaction { provider model maxSummaryTokens temperature }
             fileTracking { region { name } regionName trackReads trackWrites }
             repetitionDetection { enabled maxRepeatCalls maxReadonlyStreak }
             safeCommands { tools shell }
             output { format instructions validator onValidatorError
                      artifacts { name mimeType required description } }
           } }"#)
    .await;
    let bp = &json["blueprint"];
    assert_eq!(bp["security"]["taintTracking"], "TRACK");
    assert_eq!(bp["sandbox"]["kind"], "CONTAINER");
    assert_eq!(bp["sandbox"]["image"], "python:3.12");
    assert_eq!(bp["sandbox"]["mounts"][0], "./data:/data");
    // A field the block leaves out keeps the type's own default rather than
    // reading as unset: this one has one, and it is the safe answer.
    assert_eq!(bp["sandbox"]["onUnavailable"], "ERROR");
    // The nudge block sets only `max`, so the other two inherit, and saying so
    // is the point of a policy word rather than a nullable boolean.
    assert_eq!(bp["nudge"]["policy"], "INHERIT");
    assert_eq!(bp["nudge"]["max"], 5);
    assert!(bp["nudge"]["text"].is_null());
    assert_eq!(bp["compaction"]["model"], "claude-haiku-4-5");
    assert_eq!(bp["compaction"]["maxSummaryTokens"], 800);
    assert_eq!(bp["fileTracking"]["region"]["name"], "files");
    assert_eq!(bp["fileTracking"]["regionName"], "files");
    assert_eq!(bp["fileTracking"]["trackWrites"], false);
    assert_eq!(bp["repetitionDetection"]["maxRepeatCalls"], 4);
    assert!(bp["repetitionDetection"]["enabled"].is_null(), "inherited");
    assert_eq!(bp["safeCommands"]["shell"][0], "cargo test");
    assert_eq!(bp["output"]["format"], "json");
    assert_eq!(bp["output"]["onValidatorError"], "ACCEPT");
    assert_eq!(bp["output"]["artifacts"][0]["name"], "report");
    assert_eq!(bp["output"]["artifacts"][0]["mimeType"], "text/markdown");
    assert_eq!(bp["output"]["artifacts"][0]["required"], true);
}

/// Dependencies carry the fields their own kind uses, and only those.
///
/// A flat type per kind would mean a binary dependency answering `server: null`
/// and an MCP one answering `command: null`, which is what the `kind` field is
/// there to tell a client not to read.
#[tokio::test]
async fn dependencies_carry_what_their_kind_needs() {
    let json = ask(r#"{ blueprint { dependencies {
             name kind required remedy description
             server env var command check
             install { command commands { os command } script
                       server { transport command args env { name value } } }
           } } }"#)
    .await;
    let deps = json["blueprint"]["dependencies"]
        .as_array()
        .expect("dependencies");
    assert_eq!(deps.len(), 2);

    let binary = &deps[0];
    assert_eq!(binary["kind"], "BINARY");
    assert_eq!(binary["command"], "blender");
    assert_eq!(binary["required"], true, "the default");
    assert!(binary["remedy"].as_str().is_some());
    assert!(binary["server"].is_null(), "not an MCP dependency");
    assert!(binary["install"].is_null());

    let server = &deps[1];
    assert_eq!(server["kind"], "MCP_SERVER");
    assert_eq!(server["server"], "docs");
    assert_eq!(server["env"][0], "DOCS_TOKEN");
    assert!(server["command"].is_null(), "not a binary dependency");
    assert_eq!(server["install"]["command"], "npm i -g docs-mcp");
    assert_eq!(server["install"]["server"]["transport"], "STDIO");
    assert_eq!(server["install"]["server"]["args"][0], "--stdio");
    // The value ships as the reference, so the secret stays out of the file
    // and out of this answer.
    assert_eq!(server["install"]["server"]["env"][0]["name"], "DOCS_TOKEN");
    assert_eq!(
        server["install"]["server"]["env"][0]["value"],
        "${DOCS_TOKEN}"
    );
}

/// A blueprint's own mime rows come back with their token rule typed.
#[tokio::test]
async fn a_blueprint_carries_the_mime_rows_it_ships() {
    let json = ask(r#"{ blueprint { mimeTypes {
             mimeType family isText extensions magic standIn check
             tokens { perByte perPixel max perSecond perPage fixed }
           } } }"#)
    .await;
    let row = &json["blueprint"]["mimeTypes"][0];
    assert_eq!(row["mimeType"], "model/gltf+json");
    assert_eq!(row["family"], "model");
    assert_eq!(row["isText"], true);
    assert_eq!(row["extensions"][0], "gltf");
    assert_eq!(row["tokens"]["fixed"], 500);
    // Exactly one rate is set, so a client reads the one that is there rather
    // than working out which applies.
    assert!(row["tokens"]["perByte"].is_null());
    assert!(row["tokens"]["perPixel"].is_null());
    assert!(row["magic"].is_null());
}

/// A region carries its budget, its policies and what fills it.
#[tokio::test]
async fn a_region_carries_its_budget_and_policies() {
    let json = ask(r#"{ blueprint { regions {
             name kind maxTokens budgetPercent minTokens budgetMaxTokens
             required requiredMessage volatility admission summarizable accepts
             maxItems strategy overflow compactCount maxEntries thresholdTokens
             seed { __typename ... on SeedFromLiteral { text } }
           } } }"#)
    .await;
    let regions = json["blueprint"]["regions"].as_array().expect("regions");
    let plan = regions
        .iter()
        .find(|region| region["name"] == "plan")
        .expect("the plan region");
    assert_eq!(plan["kind"], "PINNED");
    assert_eq!(plan["budgetPercent"], 20.0);
    assert_eq!(plan["minTokens"], 500);
    assert_eq!(plan["budgetMaxTokens"], 4000);
    assert_eq!(plan["required"], true);
    assert_eq!(
        plan["requiredMessage"],
        "{region} has to say something first"
    );
    assert_eq!(plan["volatility"], "REWRITTEN");
    assert_eq!(plan["admission"], "REJECT");
    assert_eq!(plan["accepts"][0], "text/*");
    assert_eq!(plan["seed"]["__typename"], "SeedFromLiteral");
    assert_eq!(plan["seed"]["text"], "start here");
    // A pinned region does not slide, so the sliding numbers are null rather
    // than zero: zero would read as a ceiling of none.
    assert!(plan["maxItems"].is_null());
    assert!(plan["strategy"].is_null());

    let notes = regions
        .iter()
        .find(|region| region["name"] == "notes")
        .expect("the notes region");
    assert_eq!(notes["maxItems"], 20);
    assert_eq!(notes["strategy"], "BULK");
    assert_eq!(notes["overflow"], 3);
    assert!(notes["compactCount"].is_null(), "not a compacting strategy");
    // A fixed ceiling rather than a share, so the budget fields stay null.
    assert!(notes["budgetPercent"].is_null());
    assert_eq!(notes["maxTokens"], 1000);

    let facts = regions
        .iter()
        .find(|region| region["name"] == "facts")
        .expect("the facts region");
    assert_eq!(facts["kind"], "HASHMAP");
    assert_eq!(facts["maxEntries"], 50);
}

/// A seed that runs tools says what it calls and when it runs again.
///
/// It is the one seed that executes something through the run's own tool layer,
/// so which calls it makes is what an operator reads before trusting it.
#[tokio::test]
async fn a_tool_seed_carries_its_calls_and_its_refresh() {
    let json = ask(r#"{ blueprint { regions { name seed {
             __typename
             ... on SeedFromTools { refresh calls { tool args } }
           } } } }"#)
    .await;
    let env = json["blueprint"]["regions"]
        .as_array()
        .expect("regions")
        .iter()
        .find(|region| region["name"] == "env")
        .expect("the env region");
    assert_eq!(env["seed"]["__typename"], "SeedFromTools");
    assert_eq!(env["seed"]["refresh"], "EACH_STAGE");
    assert_eq!(env["seed"]["calls"][0]["tool"], "which_command");
    assert_eq!(env["seed"]["calls"][0]["args"]["command"], "git");
}

/// A second manifest, for the branches the first one does not take: the other
/// seed kinds, the other token rules, the other region formats, and a transform.
fn variants() -> String {
    r#"
[agent]
name = "variants"
version = "1.0.0"
description = "the other branches"

[sandbox]
kind = "namespace"
network = true
keep_warm = true
on_unavailable = "warn"

[agent.nudge]
enabled = false
text = "call a tool"

[[transforms]]
from_blueprint = "variants"
to_blueprint = "other"

[[transforms.mappings]]
from_region = "task"
to_region = "task"
transform = "extract"
fields = ["title"]

[[transforms.mappings]]
from_region = "notes"
to_region = "notes"

[mime_types."image/webp"]
family = "image"
tokens = { per_pixel = 750, max = 1600 }
magic = "52494646"
stand_in = "[a picture: {name}]"
check = "checks/webp.rhai"

[mime_types."audio/mpeg"]
family = "audio"
tokens = { per_second = 32 }

[mime_types."application/pdf"]
family = "document"
tokens = { per_page = 600 }

[mime_types."text/x-thing"]
family = "text"
tokens = { per_byte = 0.25 }

[[dependencies]]
name = "token"
kind = "env"
var = "THING_TOKEN"

[[dependencies]]
name = "ready"
kind = "script"
check = "checks/ready.rhai"
required = false

[dependencies.install]
script = "install/ready.rhai"

[dependencies.install.commands]
macos = "brew install thing"
linux = "apt install thing"

[context.regions.task]
kind = "pinned"
max_tokens = 1000
seed = { caller = "task" }

[context.regions.notes]
kind = "sliding_window"
max_tokens = 1000
max_items = 10
strategy = "compact"
compact_count = 4

[context.regions.sources]
kind = "temporary"
max_tokens = 1000
seed = { glob = "*.md" }

[context.regions.files]
kind = "temporary"
max_tokens = 1000
seed = { files = ["a.txt", "b.txt"] }

[context.regions.script_region]
kind = "custom"
script = "context_hooks/own.rhai"
pinned = true
max_tokens = 1000
seed = { rhai = "seeds/now.rhai" }

[context.regions.env]
kind = "temporary"
max_tokens = 1000
seed = { command = "git status --short" }

[context.regions.history]
kind = "compact_history"
source_region = "work"
max_tokens = 1000

[context.regions.work]
kind = "compacting"
max_tokens = 2000
threshold_tokens = 1500
seed = { tool = "which_command" }

[context.regions.plan]
kind = "checklist"
max_tokens = 1000

[stages.only]
mode = "autonomous"

[stages.only.transitions.done]
condition = "dead_end"
transform = "compact"

[stages.only.transitions.other]
condition = "max_iterations"
transform = "clear"

[stages.done]
mode = "autonomous"

[stages.other]
mode = "interactive"
"#
    .to_string()
}

/// Ask the schema about the second manifest.
async fn ask_variants(query: &str) -> serde_json::Value {
    let text = variants();
    let parsed = leviath_core::manifest::parse_manifest(&text).expect("the manifest parses");
    let schema = Schema::build(
        Probe {
            blueprint: Blueprint {
                parsed: Arc::new(parsed),
                digest: digest_of(&text),
                source: CoreSource::Installed.into(),
            },
        },
        EmptyMutation,
        EmptySubscription,
    )
    .data(crate::commands::serve::testutil::state_with_agent_paths(
        Vec::new(),
    ))
    .finish();
    let answer = schema.execute(Request::new(query)).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).expect("data serializes")
}

/// Every kind of seed comes back as its own type, so a client matches on the
/// type rather than on which of ten fields is not null.
#[tokio::test]
async fn each_kind_of_seed_is_its_own_type() {
    let json = ask_variants(
        r#"{ blueprint { regions { name seed {
             __typename
             ... on SeedFromCaller { key }
             ... on SeedFromGlob { pattern }
             ... on SeedFromFiles { paths }
             ... on SeedFromScript { script }
             ... on SeedFromCommand { command }
             ... on SeedFromTools { calls { tool } refresh }
           } } } }"#,
    )
    .await;
    let by_name = |name: &str| -> serde_json::Value {
        json["blueprint"]["regions"]
            .as_array()
            .expect("regions")
            .iter()
            .find(|region| region["name"] == name)
            .expect("the region")
            .clone()
    };
    assert_eq!(by_name("task")["seed"]["__typename"], "SeedFromCaller");
    assert_eq!(by_name("task")["seed"]["key"], "task");
    assert_eq!(by_name("sources")["seed"]["__typename"], "SeedFromGlob");
    assert_eq!(by_name("sources")["seed"]["pattern"], "*.md");
    assert_eq!(by_name("files")["seed"]["__typename"], "SeedFromFiles");
    assert_eq!(by_name("files")["seed"]["paths"][1], "b.txt");
    assert_eq!(
        by_name("script_region")["seed"]["__typename"],
        "SeedFromScript"
    );
    assert_eq!(by_name("env")["seed"]["__typename"], "SeedFromCommand");
    assert_eq!(by_name("env")["seed"]["command"], "git status --short");
    // The single-tool shorthand is a one-entry list here: one shape for one
    // idea, so a client reads `calls` whichever way the manifest wrote it.
    assert_eq!(by_name("work")["seed"]["__typename"], "SeedFromTools");
    assert_eq!(by_name("work")["seed"]["calls"][0]["tool"], "which_command");
    assert_eq!(by_name("work")["seed"]["refresh"], "ONCE");
    assert!(by_name("notes")["seed"].is_null(), "no seed at all");
}

/// The region kinds that carry a number each report their own.
#[tokio::test]
async fn each_region_kind_reports_its_own_numbers() {
    let json = ask_variants(
        r#"{ blueprint { regions {
             name kind strategy compactCount overflow thresholdTokens
             sourceRegion { name } sourceRegionName script pinned
           } } }"#,
    )
    .await;
    let by_name = |name: &str| -> serde_json::Value {
        json["blueprint"]["regions"]
            .as_array()
            .expect("regions")
            .iter()
            .find(|region| region["name"] == name)
            .expect("the region")
            .clone()
    };
    let notes = by_name("notes");
    assert_eq!(notes["strategy"], "COMPACT");
    assert_eq!(notes["compactCount"], 4);
    assert!(notes["overflow"].is_null(), "not a bulk strategy");
    let work = by_name("work");
    assert_eq!(work["kind"], "COMPACTING");
    assert_eq!(work["thresholdTokens"], 1500);
    let history = by_name("history");
    assert_eq!(history["kind"], "COMPACT_HISTORY");
    assert_eq!(history["sourceRegion"]["name"], "work");
    assert_eq!(history["sourceRegionName"], "work");
    let own = by_name("script_region");
    assert_eq!(own["kind"], "CUSTOM");
    assert_eq!(own["script"], "context_hooks/own.rhai");
    assert_eq!(own["pinned"], true);
}

/// Every token rule has its own field, and only one is ever set.
#[tokio::test]
async fn each_token_rule_sets_one_field() {
    let json = ask_variants(
        r#"{ blueprint { mimeTypes { mimeType magic standIn check
             tokens { perByte perPixel max perSecond perPage fixed } } } }"#,
    )
    .await;
    let rows = json["blueprint"]["mimeTypes"].as_array().expect("rows");
    let by_type = |mime: &str| -> serde_json::Value {
        rows.iter()
            .find(|row| row["mimeType"] == mime)
            .expect("the row")
            .clone()
    };
    let webp = by_type("image/webp");
    assert_eq!(webp["tokens"]["perPixel"], 750);
    assert_eq!(webp["tokens"]["max"], 1600);
    assert_eq!(webp["magic"], "52494646");
    assert_eq!(webp["standIn"], "[a picture: {name}]");
    assert_eq!(webp["check"], "checks/webp.rhai");
    assert_eq!(by_type("audio/mpeg")["tokens"]["perSecond"], 32);
    assert_eq!(by_type("application/pdf")["tokens"]["perPage"], 600);
    assert_eq!(by_type("text/x-thing")["tokens"]["perByte"], 0.25);
    // Sorted by type, so two reads of one blueprint agree.
    assert_eq!(rows[0]["mimeType"], "application/pdf");
}

/// A handoff says which of its regions becomes which of the other's.
#[tokio::test]
async fn a_transform_maps_one_layout_onto_another() {
    let json = ask_variants(
        r#"{ blueprint { transforms { fromBlueprint toBlueprint
             mappings { fromRegion toRegion transform fields } } } }"#,
    )
    .await;
    let transform = &json["blueprint"]["transforms"][0];
    assert_eq!(transform["fromBlueprint"], "variants");
    assert_eq!(transform["toBlueprint"], "other");
    assert_eq!(transform["mappings"][0]["transform"], "EXTRACT");
    assert_eq!(transform["mappings"][0]["fields"][0], "title");
    // A mapping with no transform carries the content across as it is, and says
    // so by leaving the field null rather than by naming a default.
    assert!(transform["mappings"][1]["transform"].is_null());
    assert_eq!(
        transform["mappings"][1]["fields"].as_array().map(Vec::len),
        Some(0)
    );
}

/// The remaining enum arms: a namespace sandbox that falls back with a warning,
/// a nudge turned off outright, and the two simpler edge transforms.
#[tokio::test]
async fn the_remaining_settings_arms_come_back() {
    let json = ask_variants(
        r#"{ blueprint {
             sandbox { kind allowNetwork keepWarm onUnavailable }
             nudge { policy text }
             dependencies { name kind var check required
                            install { script commands { os command } } }
           } }"#,
    )
    .await;
    let bp = &json["blueprint"];
    assert_eq!(bp["sandbox"]["kind"], "NAMESPACE");
    assert_eq!(bp["sandbox"]["allowNetwork"], true);
    assert_eq!(bp["sandbox"]["keepWarm"], true);
    assert_eq!(bp["sandbox"]["onUnavailable"], "WARN");
    // Off outright, which is a different answer from inheriting: a nullable
    // boolean could not tell the two apart.
    assert_eq!(bp["nudge"]["policy"], "NEVER_NUDGE");
    assert_eq!(bp["nudge"]["text"], "call a tool");
    let deps = bp["dependencies"].as_array().expect("dependencies");
    let env = deps
        .iter()
        .find(|d| d["name"] == "token")
        .expect("the env one");
    assert_eq!(env["kind"], "ENV");
    assert_eq!(env["var"], "THING_TOKEN");
    let script = deps
        .iter()
        .find(|d| d["name"] == "ready")
        .expect("the script one");
    assert_eq!(script["kind"], "SCRIPT");
    assert_eq!(script["check"], "checks/ready.rhai");
    assert_eq!(script["required"], false);
    assert_eq!(script["install"]["script"], "install/ready.rhai");
    let commands = script["install"]["commands"].as_array().expect("commands");
    assert_eq!(commands.len(), 2);
    assert!(commands.iter().any(|entry| entry["os"] == "macos"));
}

/// The edge transforms the first manifest does not use come back too.
#[tokio::test]
async fn the_remaining_edge_transforms_come_back() {
    let json = ask_variants(
        r#"{ blueprint { stages { name
             transitions { targetName condition transform
                           transformConfig { compactPrompt carry { name } carryNames } } } } }"#,
    )
    .await;
    let stages = json["blueprint"]["stages"].as_array().expect("stages");
    let only = stages
        .iter()
        .find(|stage| stage["name"] == "only")
        .expect("the stage");
    let edges = only["transitions"].as_array().expect("edges");
    let done = edges
        .iter()
        .find(|edge| edge["targetName"] == "done")
        .expect("an edge");
    assert_eq!(done["condition"], "DEAD_END");
    assert_eq!(done["transform"], "COMPACT");
    // A compact edge takes the whole context, so it has no per-region lists to
    // report: only what it asks the summarizer for, which this one leaves to the
    // default.
    assert!(done["transformConfig"]["compactPrompt"].is_null());
    assert_eq!(
        done["transformConfig"]["carry"].as_array().map(Vec::len),
        Some(0)
    );
    let other = edges
        .iter()
        .find(|edge| edge["targetName"] == "other")
        .expect("an edge");
    assert_eq!(other["condition"], "MAX_ITERATIONS");
    assert_eq!(other["transform"], "CLEAR");
    assert!(
        other["transformConfig"].is_null(),
        "a clear edge has nothing to configure"
    );
}

/// A permission word the daemon does not recognise reads as `ASK`.
///
/// The manifest parser refuses such a word, so this builds the stage directly:
/// the path exists for a record that reached the server another way. `ASK` is
/// what the daemon's own resolution makes of it, and reporting `ALLOW` or `DENY`
/// would describe a dispatch that will not happen.
#[tokio::test]
async fn a_permission_word_that_is_not_one_reads_as_ask() {
    use super::super::manifest::stage::Stage as StageObject;

    let mut stage = leviath_core::blueprint::Stage::new(
        "only".to_string(),
        leviath_core::blueprint::ModelConfig {
            models: Vec::new(),
            allow_user_default: true,
            parameters: std::collections::HashMap::new(),
            request_timeout_secs: None,
        },
    );
    stage
        .tool_permissions
        .insert("shell".to_string(), "sideways".to_string());
    let blueprint = Arc::new(leviath_core::Blueprint::new(
        "hand-built".to_string(),
        "one stage with a word that is not a policy".to_string(),
        vec![stage],
        leviath_core::layout::ContextLayout::new(Vec::new(), 0),
    ));

    let schema = Schema::build(
        StageProbe {
            stage: StageObject { blueprint, at: 0 },
        },
        EmptyMutation,
        EmptySubscription,
    )
    .finish();
    let answer = schema
        .execute(Request::new(
            "{ stage { toolPermissions { tool policy } } }",
        ))
        .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    assert_eq!(json["stage"]["toolPermissions"][0]["tool"], "shell");
    assert_eq!(
        json["stage"]["toolPermissions"][0]["policy"], "ASK",
        "a word that is not a policy is what the daemon reads it as"
    );
}

/// A root handing out one hand-built stage.
struct StageProbe {
    stage: super::super::manifest::stage::Stage,
}

#[async_graphql::Object]
impl StageProbe {
    /// The stage under test.
    async fn stage(&self) -> &super::super::manifest::stage::Stage {
        &self.stage
    }
}

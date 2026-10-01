use super::*;

use leviath_blueprint::BlueprintFile;
use leviath_runtime::spec::graph::{
    Budget, EdgeCarry, RegionKind, RegionLayoutDef, RunGraph, StageMode, ToolSelector,
};

/// The text of a bundled agent's `agent.toml`.
fn text_of(agent: &BundledAgent) -> &'static str {
    agent
        .files
        .iter()
        .find(|(rel, _)| *rel == leviath_blueprint::FILE_NAME)
        .map(|(_, c)| *c)
        .expect("every bundled agent ships an agent.toml")
}

/// Every bundled agent with its file read. A file that does not read fails
/// here, naming nothing: `every_bundled_blueprint_reads_and_agrees_with_its_recorded_version`
/// names it.
fn bundled_files() -> Vec<(&'static BundledAgent, BlueprintFile)> {
    BUNDLED_AGENTS
        .iter()
        .map(|agent| {
            let file = BlueprintFile::parse(text_of(agent)).expect("every bundled blueprint reads");
            (agent, file)
        })
        .collect()
}

/// A stage's tool names, groups left out (no bundled stage grants one).
fn tool_names(tools: &[ToolSelector]) -> Vec<&str> {
    tools
        .iter()
        .filter_map(|t| match t {
            ToolSelector::Tool(name) => Some(name.as_str()),
            ToolSelector::Group(_) => None,
        })
        .collect()
}

/// The words a prompt wraps in backticks, which is how these blueprints
/// refer to a region.
fn backticked_words(prompt: &str) -> Vec<String> {
    prompt
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|w| {
            !w.is_empty()
                && w.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
        })
        .map(str::to_string)
        .collect()
}

/// A stage prompt must not send the model to a region the blueprint does
/// not have.
///
/// `wide-researcher`'s adversarial pass told the model to "go through
/// `claims`, `contradictions` and `analysis`" - three regions belonging to
/// `deep-researcher`, which is where the stage had been copied from. Nothing
/// failed. The stage ran, found nothing to attack because it was looking for
/// regions that were not there, wrote a token gesture and passed the report
/// through unchallenged. A copied stage keeps working right up until it
/// mentions a region by name, which is exactly the edit nobody re-reads.
///
/// Matched on the backticked names a prompt uses, against the regions the
/// blueprint declares plus the ones the runtime supplies. Anything else in
/// backticks - a tool, a filename, a TOML key - is not a region and does not
/// belong in the comparison, so the check runs against the set of names that
/// ARE regions somewhere in the bundled set: a name that is a region in one
/// blueprint and undeclared in another is the mistake being caught.
#[test]
fn no_stage_prompt_names_a_region_its_blueprint_does_not_have() {
    // Supplied by the runtime rather than declared, so a prompt may name
    // them anywhere.
    const RUNTIME_REGIONS: [&str; 4] = [
        "conversation",
        "tool_results",
        "final_output",
        "stage_instructions",
    ];

    let parsed = bundled_files();

    // Every name that is a region in at least one bundled blueprint. A word
    // in backticks is only held to this if it names a region somewhere.
    let mut region_vocabulary: std::collections::HashSet<&str> =
        RUNTIME_REGIONS.into_iter().collect();
    for (_, file) in &parsed {
        for region in &file.graph.layout.regions {
            region_vocabulary.insert(region.name.as_str());
        }
    }

    let mut checked = 0;
    for (agent, file) in &parsed {
        let graph = &file.graph;
        let declared: std::collections::HashSet<&str> = graph
            .layout
            .regions
            .iter()
            .map(|r| r.name.as_str())
            .chain(RUNTIME_REGIONS)
            .collect();

        for stage in &graph.stages {
            let split = match &stage.mode {
                StageMode::FanOut(fan) => Some(fan.split_prompt.as_str()),
                _ => None,
            };
            let prompts = stage
                .system_prompt
                .as_deref()
                .into_iter()
                .chain(split)
                .chain(stage.transition_prompt.as_deref());
            for named in prompts.flat_map(backticked_words) {
                if !region_vocabulary.contains(named.as_str()) {
                    continue;
                }
                checked += 1;
                assert!(
                    declared.contains(named.as_str()),
                    "{}: stage `{}` tells the model to use region `{named}`, which this \
                     blueprint does not declare. It is a region in another bundled blueprint, \
                     so this is a stage copied across without renaming its regions.",
                    agent.name,
                    stage.name,
                );
            }
        }
    }
    assert!(
        checked > 0,
        "no prompt named a region -- this test now proves nothing"
    );
}

/// A `temporary` region must declare its `volatility`, because the default
/// fails quietly and only at scale.
///
/// `rewritten` - the default - says the whole region changes every turn, so
/// none of it is cached. That is right for a scratchpad and wrong for the
/// append-only region tool results land in, where it was worth 4% cache hits
/// instead of most of the region on a measured run. A region re-sent in full
/// on every inference for the rest of a stage is the single largest thing a
/// blueprint can get wrong about its own cost.
///
/// Keyed on the kind, not on how large the percentage is: accumulating tool
/// output is what makes a region worth checking, and `temporary` is the kind
/// that does it. A `compacting` workspace of the same size is genuinely
/// rewritten and manages its own size by compacting, so it is not held to
/// this.
///
/// An invariant over discovered blueprints rather than a list of known ones:
/// the next bulk region somebody adds is the one this is here to catch.
#[test]
fn a_temporary_region_says_how_its_contents_change() {
    use leviath_core::region::Volatility;

    let mut checked = 0;
    for (agent, file) in bundled_files() {
        let bulk = file
            .graph
            .layout
            .regions
            .iter()
            .filter_map(|region| match region.budget {
                Budget::Percent { percent, .. } if region.kind == RegionKind::Temporary => {
                    Some((region, percent))
                }
                _ => None,
            });

        for (region, percent) in bulk {
            checked += 1;
            // Computed here rather than in the failure message: an argument
            // expression evaluated only on failure is its own uncovered
            // region on an assertion that has to pass.
            let share = percent * 100.0;
            assert_ne!(
                region.volatility,
                Volatility::Rewritten,
                "{}: `{}` holds {share:.0}% of the window as accumulated tool output but \
                 is `rewritten`, so none of it is ever cached",
                agent.name,
                region.name,
            );
        }
    }
    assert!(
        checked > 0,
        "no temporary region on a percentage budget was examined -- this test now proves nothing"
    );
}

/// Every assertion here is an invariant over *all* discovered blueprints.
/// Naming individual agents would turn adding or renaming one into a test
/// edit, and would stop testing the property the moment the list drifted.
/// No bundled blueprint puts a deadline on a person. A prompt waits until
/// somebody answers unless the operator's own `[limits]
/// interaction_timeout_secs` says otherwise, and a shipped agent must not
/// decide that for them through any key that names the timeout.
#[test]
fn no_bundled_agent_sets_an_interaction_timeout() {
    assert!(!BUNDLED_AGENTS.is_empty());
    for agent in BUNDLED_AGENTS {
        for (rel, contents) in agent.files {
            assert!(
                !contents.contains("interaction_timeout"),
                "bundled agent {} sets an interaction timeout in {rel}",
                agent.name
            );
        }
    }
}

#[test]
fn every_bundled_agent_has_a_name_version_and_blueprint_file() {
    assert!(
        !BUNDLED_AGENTS.is_empty(),
        "the binary shipped with no blueprints -- build.rs found no agents/ directory"
    );
    for agent in BUNDLED_AGENTS {
        assert!(!agent.name.is_empty(), "a bundled agent has an empty name");
        assert!(
            !agent.version.is_empty(),
            "bundled agent {} has an empty version",
            agent.name
        );
        assert!(
            agent
                .files
                .iter()
                .any(|(rel, _)| *rel == leviath_blueprint::FILE_NAME),
            "bundled agent {} has no agent.toml",
            agent.name
        );
        for (rel, contents) in agent.files {
            assert!(
                !rel.is_empty(),
                "bundled agent {} has an empty path",
                agent.name
            );
            assert!(
                !contents.is_empty(),
                "bundled agent {} has an empty file {rel}",
                agent.name
            );
        }
    }
}

/// A tool script shipped under the same filename by more than one agent
/// must be byte-identical everywhere.
///
/// Each agent directory is self-contained - that is what lets `lev add
/// <dir>` and `lev pack` work - so `web_fetch.rhai` and `web_search.rhai`
/// exist as several copies each rather than one shared file. That is fine
/// until one copy is fixed and the others are not: these scripts are the
/// agents' network surface, so a hardening change applied to one copy is
/// the other agents still carrying the unfixed behaviour, with nothing to
/// say so.
///
/// Deliberately keyed on filename over *all* discovered agents rather than
/// naming them, so it keeps holding as agents are added or renamed.
#[test]
fn a_tool_script_shared_by_several_agents_is_identical_in_all_of_them() {
    use std::collections::HashMap;

    // filename -> (first agent that shipped it, its contents)
    let mut first_seen: HashMap<&str, (&str, &str)> = HashMap::new();
    for agent in BUNDLED_AGENTS {
        for (rel, contents) in agent.files {
            let Some(filename) = rel.strip_prefix("tools/") else {
                continue;
            };
            match first_seen.get(filename) {
                Some((other, expected)) => assert!(
                    expected == contents,
                    "tools/{filename} differs between bundled agents {other} and {} - \
                     a change to one copy was not applied to the others",
                    agent.name
                ),
                None => {
                    first_seen.insert(filename, (agent.name, contents));
                }
            }
        }
    }
    // Guard against a vacuous pass: if the scan found no tool scripts at
    // all, the loop above asserts nothing.
    assert!(
        !first_seen.is_empty(),
        "no bundled agent ships a tools/ script - this invariant is not being tested"
    );
}

/// Every bundled blueprint reads, holds together, writes back as itself, and
/// agrees with the version and name `build.rs` recorded for it.
///
/// The recorded version drives install/update planning, so a build.rs scan
/// that disagreed with the file would make the wizard lie.
#[test]
fn every_bundled_blueprint_reads_and_agrees_with_its_recorded_version() {
    for agent in BUNDLED_AGENTS {
        // `.expect`, not `.unwrap_or_else(|e| panic!(...))`: the closure in
        // the latter is a function that never runs on a passing test, which
        // reads to llvm-cov as an uncovered region.
        let parsed = BlueprintFile::parse(text_of(agent));
        assert!(parsed.is_ok(), "bundled agent {} does not read", agent.name);
        let file = parsed.expect("asserted Ok just above");
        assert_eq!(file.blueprint.version, agent.version);
        assert_eq!(file.blueprint.name.as_str(), agent.name);
        let graph = file.run_graph();
        assert!(
            graph
                .validate(&leviath_runtime::spec::issues::SpecPath::root())
                .is_ok(),
            "bundled agent {}'s graph does not hold together",
            agent.name
        );
        // Written back by the same writer `lev blueprint migrate` uses, it
        // reads as the same blueprint: the hand-written file says nothing the
        // types cannot hold.
        let rewritten = file.to_toml().expect("a bundled blueprint writes as TOML");
        assert_eq!(
            BlueprintFile::parse(&rewritten).expect("the written file reads"),
            file
        );
    }
}

/// A bundled stage that routes tool output into a region must be able to
/// read one.
///
/// Routing leaves a pointer in the conversation saying where the output
/// went. If the stage also grants a file-reading tool and no
/// `context_read`, the only read verb the model has points at the
/// filesystem, and it aims it at the region name: 90 of 168 failed
/// `read_file` calls across 152 local runs were exactly that, one of them
/// spending five turns on five spellings of `raw_findings`.
///
/// Asserted over whatever is bundled rather than a fixed list of stages, so
/// a new routed stage is held to it the day it lands.
#[test]
fn every_bundled_stage_that_routes_can_also_read_a_region() {
    let mut routed = 0;
    for (agent, file) in bundled_files() {
        for stage in &file.graph.stages {
            let routes = stage.tool_routing.as_ref().is_some_and(|r| {
                r.default_region.as_str() != "conversation"
                    || r.tool_regions
                        .values()
                        .any(|v| v.as_str() != "conversation")
            });
            let tools = tool_names(&stage.tools);
            let reads_files = tools
                .iter()
                .any(|t| *t == "read_file" || *t == "read_files");
            if !routes || !reads_files {
                continue;
            }
            routed += 1;
            assert!(
                tools.contains(&"context_read"),
                "{}'s stage '{}' routes tool output into a region and grants a \
                 file-reading tool, but not 'context_read' - the only way the \
                 model can act on the pointer is to aim read_file at the region \
                 name",
                agent.name,
                stage.name
            );
        }
    }
    // A vacuous pass would be a bundled set that routes nowhere.
    assert!(
        routed > 0,
        "no bundled stage routes tool output to a region"
    );
}

/// A budget in tokens against a window, the way the resolver sizes one: a
/// percentage is rounded, capped at `max`, then floored at `min`.
fn tokens(budget: &Budget, window: u32) -> u32 {
    match budget {
        Budget::Tokens(n) => *n,
        Budget::Percent { percent, min, max } => {
            let share = (f64::from(window) * percent).round() as u32;
            share.min(max.unwrap_or(u32::MAX)).max(min.unwrap_or(0))
        }
    }
}

/// What a layout's fixed regions (pinned, keyed, histories, pinned custom
/// regions) take of a window, which the rest of the run has to work around.
fn fixed_tokens(layout: &RegionLayoutDef, window: u32) -> u32 {
    layout
        .regions
        .iter()
        .filter(|r| {
            matches!(
                r.kind,
                RegionKind::Pinned
                    | RegionKind::Keyed { .. }
                    | RegionKind::CompactHistory { .. }
                    | RegionKind::Custom { pinned: true, .. }
            )
        })
        .map(|r| tokens(&r.budget, window))
        .fold(0, u32::saturating_add)
}

/// A bundled layout must actually grow with the model's context window, and
/// leave a stage room to work at every window a real model has.
///
/// A region capped by an absolute size is the smaller of the two on any
/// window worth having: `researcher` once ran on a 1,048,576-token model with
/// `raw_findings` asking for 30% and getting the 40,000 its cap allowed. The
/// bulk capture regions keep a cap where uncapped would be the worse failure
/// (30% of a 1M-token window is 300,000 tokens of raw scrape re-sent on every
/// inference), but the regions the agent reasons in must still scale.
///
/// Stated as a ratio rather than a per-region ceiling so it holds whatever
/// the percentages are: resolve each layout against two windows a little
/// over 5x apart, and the room must scale with them. A layout clamped by
/// absolute caps scores 1.0 here, because both windows resolve to the same
/// fixed numbers; a layout with one deliberate ceiling on its bulk region
/// scores just under 4.
#[test]
fn every_bundled_layout_scales_with_the_model_window() {
    const NARROW: u32 = 200_000;
    const WIDE: u32 = 1_048_576;
    const MIN_GROWTH: f64 = 3.5;
    // The resolver refuses a stage left with less than this to work in.
    const MIN_WORKING_TOKENS: u32 = 8000;

    let room = |graph: &RunGraph, window: u32| -> u32 {
        graph
            .layout
            .regions
            .iter()
            .map(|r| tokens(&r.budget, window))
            .fold(0, u32::saturating_add)
    };

    let mut checked = 0;
    for (agent, file) in bundled_files() {
        let graph = &file.graph;
        // Percentage ceilings may sum past 100% on purpose (regions rarely
        // fill together), so the sum is not the thing to assert. What must
        // hold at every window is room to work: the fixed regions have to
        // leave the agent some. Checked across the range a real model spans,
        // because the floors bind at the bottom of it and the percentages at
        // the top. The shared layout and any stage's own, as one sequence.
        for window in [32_768, 128_000, NARROW, WIDE] {
            let layouts = std::iter::once(&graph.layout)
                .chain(graph.stages.iter().filter_map(|s| s.layout.as_ref()));
            for layout in layouts {
                let working = window.saturating_sub(fixed_tokens(layout, window));
                assert!(
                    working >= MIN_WORKING_TOKENS,
                    "a layout of {} leaves {working} tokens to work in at a {window}-token window",
                    agent.name
                );
            }
        }

        let narrow = room(graph, NARROW);
        let wide = room(graph, WIDE);
        let growth = f64::from(wide) / f64::from(narrow);
        assert!(
            growth >= MIN_GROWTH,
            "{}'s context layout barely grows between a narrow window and a \
             wide one (growth {growth:.2}x, wanted at least {MIN_GROWTH}x, \
             {narrow} -> {wide} tokens of region room). An absolute cap is \
             overriding the percentage budgets.",
            agent.name
        );
        checked += 1;
    }
    // A vacuous pass would be a loop over nothing.
    assert!(checked > 0, "no bundled agent was checked");
}

/// Every bundled agent ends in a stage that hands something back, and
/// nothing upstream can end the run before reaching it.
///
/// The second half is the part that fails quietly. `allow_complete` on any
/// earlier stage offers the model a "DONE" it can pick instead of routing
/// onward - and it is appended even to a stage's custom `transition_prompt`,
/// so a blueprint can offer an exit its own prompt never mentions. A run
/// that takes it finishes with no answer, looking exactly like success.
///
/// Asserted over whatever is bundled rather than a hard-coded list, so a
/// new agent is held to it the day it lands.
#[test]
fn every_bundled_agent_ends_by_handing_something_back() {
    for (agent, file) in bundled_files() {
        let stages = &file.graph.stages;
        let outputs: Vec<_> = stages
            .iter()
            .filter(|s| s.mode == StageMode::Output)
            .collect();
        assert!(
            !outputs.is_empty(),
            "bundled agent {} has no output stage, so a run of it hands back nothing",
            agent.name
        );

        for stage in &outputs {
            let tools = tool_names(&stage.tools);
            // The mode is meant to imply all three; a stage where it did
            // not would advertise a tool it is not required to call.
            assert!(stage.require_output, "{} output stage", agent.name);
            assert!(
                tools.contains(&leviath_core::stage_tools::SUBMIT_OUTPUT_TOOL),
                "{} output stage cannot submit",
                agent.name
            );
            // A stage whose job is to report has no business writing files.
            assert!(
                !tools.iter().any(|t| {
                    ["write_file", "edit_file"].contains(&leviath_tools::canonical_tool_name(t))
                }),
                "{} output stage can modify files",
                agent.name
            );
        }

        for stage in stages {
            assert!(
                !stage.allow_complete || stage.mode == StageMode::Output,
                "bundled agent {}: stage '{}' may end the run, skipping the output stage",
                agent.name,
                stage.name
            );
        }
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

fn key() -> String {
    "not-used-offline".to_string()
}

/// Every provider `lev setup` can configure, built so a test can ask them
/// what they serve. Bedrock is not in this list, because its ids are
/// region-prefixed vendor ids (`us.anthropic.claude-sonnet-5`) that a
/// bundled stage naming `claude-sonnet-5` does not resolve on by
/// spelling. The provider crate proves its own routing.
///
/// The keys are never used: `serves_model` reads a table compiled into the
/// build, which is exactly the offline answer wanted here.
fn setup_providers() -> Vec<(&'static str, Box<dyn leviath_providers::Provider>)> {
    let client = reqwest::Client::new();
    let key = || "not-used-offline".to_string();
    vec![
        (
            "anthropic",
            Box::new(leviath_providers::AnthropicProvider::new(
                client.clone(),
                key(),
            )) as Box<dyn leviath_providers::Provider>,
        ),
        (
            "openai",
            Box::new(leviath_providers::OpenAIProvider::new(
                client.clone(),
                key(),
            )),
        ),
        (
            "google",
            Box::new(leviath_providers::GeminiProvider::new(
                client.clone(),
                key(),
            )),
        ),
        (
            "openrouter",
            Box::new(leviath_providers::OpenRouterProvider::new(
                client.clone(),
                key(),
            )),
        ),
        (
            "meshy",
            Box::new(leviath_providers::MeshyProvider::new(client.clone(), key())),
        ),
        (
            "ollama",
            Box::new(leviath_providers::OllamaProvider::new(client)),
        ),
    ]
}

/// The published JSON Schema for `agent.toml`.
///
/// Compiled into the test so it cannot drift from the file that ships: this
/// is the same text served at
/// `https://leviath.dev/docs/<channel>/blueprint.schema.json`, and
/// `leviath-blueprint` generates it from the types the file is read into.
const BLUEPRINT_SCHEMA: &str = include_str!("../../../docs/schema/blueprint.schema.json");

/// Every way `value` fails `validator`, as readable lines.
///
/// Shared by the positive and negative tests so the formatting closure runs
/// against real errors. Called only from the passing path of each, because
/// a call inside an `assert!` message is a region only failure reaches.
fn schema_problems(validator: &jsonschema::Validator, value: &serde_json::Value) -> Vec<String> {
    validator
        .iter_errors(value)
        .map(|e| format!("{}: {e}", e.instance_path()))
        .collect()
}

/// Convert parsed TOML to JSON so a JSON Schema can be applied to it.
fn toml_to_json(value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::Value::from(*i),
        toml::Value::Float(f) => serde_json::Value::from(*f),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        // A TOML datetime has no JSON counterpart; the blueprint format has
        // no datetime-valued key, so rendering it as its own text is enough
        // for the schema to reject it wherever it appears.
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        toml::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(toml_to_json).collect())
        }
        toml::Value::Table(table) => serde_json::Value::Object(
            table
                .iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect(),
        ),
    }
}

#[test]
fn toml_converts_to_json_for_every_value_kind() {
    // Every arm, because a kind converted wrongly would be validated
    // against the wrong JSON type and the schema would pass or fail for the
    // wrong reason. `temperature` is a real float-valued blueprint key, so
    // that arm is not hypothetical.
    let source = concat!(
        "s = \"text\"\n",
        "i = 7\n",
        "f = 0.5\n",
        "b = true\n",
        "d = 1979-05-27T07:32:00Z\n",
        "a = [1, \"two\"]\n",
        "[t]\n",
        "nested = 1\n"
    );
    let parsed: toml::Value = toml::from_str(source).expect("valid TOML");
    let json = toml_to_json(&parsed);
    assert_eq!(json["s"], serde_json::json!("text"));
    assert_eq!(json["i"], serde_json::json!(7));
    assert_eq!(json["f"], serde_json::json!(0.5));
    assert_eq!(json["b"], serde_json::json!(true));
    // No JSON counterpart for a datetime, so it becomes its own text.
    assert!(json["d"].is_string());
    assert_eq!(json["a"], serde_json::json!([1, "two"]));
    assert_eq!(json["t"]["nested"], serde_json::json!(1));
}

/// The published schema, compiled.
fn schema_validator() -> jsonschema::Validator {
    let schema: serde_json::Value =
        serde_json::from_str(BLUEPRINT_SCHEMA).expect("the schema is valid JSON");
    jsonschema::validator_for(&schema).expect("the schema compiles")
}

#[test]
fn every_bundled_blueprint_validates_against_the_published_schema() {
    // The schema is the machine-readable description of this format, and an
    // agent authoring a blueprint writes against it. Generated from the types,
    // it cannot forget a key the reader takes; this proves the short forms the
    // bundled files use are in it too.
    let validator = schema_validator();
    for agent in BUNDLED_AGENTS {
        let parsed: toml::Value =
            toml::from_str(text_of(agent)).expect("the blueprint is valid TOML");
        assert_eq!(
            schema_problems(&validator, &toml_to_json(&parsed)),
            Vec::<String>::new(),
            "{} does not match blueprint.schema.json",
            agent.name
        );
    }
}

#[test]
fn the_blueprint_schema_rejects_what_the_reader_rejects() {
    // A schema that accepts everything would pass the test above over any
    // input at all. These are the mistakes it exists to catch before a run
    // is ever spawned.
    let validator = schema_validator();
    let rejects = |text: &str| {
        let parsed: toml::Value = toml::from_str(text).expect("valid TOML");
        !schema_problems(&validator, &toml_to_json(&parsed)).is_empty()
    };
    let minimal = crate::test_support::tiny_blueprint("a");
    // The minimum the reader accepts passes, so the rules below are not
    // rejecting everything.
    assert!(!rejects(&minimal), "a minimal blueprint");
    assert!(BlueprintFile::parse(&minimal).is_ok());

    for (bad, why) in [
        (
            minimal.replace("[blueprint]\n", "[agent]\n"),
            "no [blueprint]",
        ),
        (
            minimal.replace("kind = \"pinned\"", "kind = \"pinned\", bugdet = 3"),
            "a typo'd region key",
        ),
        (
            minimal.replace("system_prompt = \"work\"", "max_iteratoins = 5"),
            "a typo'd stage key",
        ),
        (
            minimal.replace(
                "[graph]\n",
                "[graph]\ntool_permissions = { bash = \"maybe\" }\n",
            ),
            "an invalid tool policy",
        ),
    ] {
        assert!(rejects(&bad), "{why}");
        // And the reader refuses the same file.
        assert!(BlueprintFile::parse(&bad).is_err(), "{why}");
    }
}

/// A declared `gate` must actually check something.
///
/// A gate whose every field is at its default passes every time. The edge
/// still reads and still validates, which is exactly how a gate silently
/// stops gating.
#[test]
fn a_declared_gate_always_checks_something() {
    let mut gates = 0;
    for (agent, file) in bundled_files() {
        for edge in &file.graph.edges {
            let Some(gate) = &edge.gate else { continue };
            let checks = gate.require_region_entries.is_some()
                || gate.require_modifications
                || gate.region.is_some()
                || gate.require_region_updated.is_some()
                || !gate.require_regions.is_empty()
                || gate.require_no_open_items.is_some()
                || !gate.tools.is_empty();
            assert!(
                checks,
                "{}: {} -> {} declares a gate that checks nothing, so it passes every time",
                agent.name, edge.from, edge.to
            );
            gates += 1;
        }
    }
    assert!(gates > 0, "no bundled edge declares a gate");
}

/// The registry every provider `lev setup` can configure makes, as the
/// resolver sees them.
fn setup_registry() -> leviath_runtime::ProviderRegistry {
    let mut r = leviath_runtime::ProviderRegistry::new();
    for (name, _) in direct_providers() {
        let p: std::sync::Arc<dyn leviath_providers::Provider> = match name {
            "anthropic" => {
                std::sync::Arc::new(leviath_providers::AnthropicProvider::new(client(), key()))
            }
            "openai" => {
                std::sync::Arc::new(leviath_providers::OpenAIProvider::new(client(), key()))
            }
            "google" => {
                std::sync::Arc::new(leviath_providers::GeminiProvider::new(client(), key()))
            }
            "openrouter" => {
                std::sync::Arc::new(leviath_providers::OpenRouterProvider::new(client(), key()))
            }
            "meshy" => std::sync::Arc::new(leviath_providers::MeshyProvider::new(client(), key())),
            _ => std::sync::Arc::new(leviath_providers::OllamaProvider::new(client())),
        };
        r.register(name.to_string(), p);
    }
    r
}

/// The providers `lev setup` can configure that answer from a table compiled
/// into the build. A gateway (OpenRouter) is left out: it answers from a model
/// list read at startup, and a stage is refused while that list is unread,
/// which an offline test cannot avoid.
fn direct_providers() -> Vec<(&'static str, Box<dyn leviath_providers::Provider>)> {
    setup_providers()
        .into_iter()
        .filter(|(name, _)| *name != "openrouter")
        .collect()
}

/// Every stage resolves to the first model it lists that anything serves.
///
/// A blueprint's `models` is a preference order. The host picks a route
/// within that order; it does not get to pick a different preference. The
/// failure this rules out: matching entries as whole `provider/model`
/// pairs, so `default_provider` chooses among routes and whatever model
/// its route names comes back - `polish` asking for
/// `gemini-3.1-pro-preview` and running `claude-sonnet-5`.
///
/// Run over every bundled agent, because the blueprints are what ship.
#[test]
fn every_bundled_stage_runs_the_first_model_anything_serves() {
    let providers = direct_providers();
    let registry = setup_registry();
    // A real default provider, because the reordering only runs when one is
    // set and registered. Every provider is in the preference, since a bare
    // name only resolves to a provider listed there and this test is about
    // which model wins, not which providers are allowed.
    let defaults = leviath_runtime::pipeline::ModelDefaults {
        provider: "google".to_string(),
        provider_order: providers.iter().map(|(n, _)| (*n).to_string()).collect(),
        ..Default::default()
    };
    let bare = |m: &str| m.rsplit('/').next().unwrap_or(m).to_string();

    let mut checked = 0;
    for (agent, file) in bundled_files() {
        // A stage that only a gateway route can run is out of reach here
        // (see [`direct_providers`]).
        let direct = file.graph.stages.iter().filter(|s| {
            !s.model.models.iter().all(|m| {
                m.provider
                    .as_ref()
                    .is_some_and(|p| p.as_str() == "openrouter")
            })
        });
        for stage in direct {
            checked += 1;
            // Kept rather than found: filtering evaluates the predicate for
            // every entry, so the arm handling a pinned route is exercised by
            // the entries that pin one.
            let reachable: Vec<_> = stage
                .model
                .models
                .iter()
                .filter(|e| match &e.provider {
                    None => providers
                        .iter()
                        .any(|(_, p)| p.serves_model(&bare(e.model.as_str())).is_some()),
                    Some(p) => providers.iter().any(|(n, _)| *n == p.as_str()),
                })
                .collect();
            let listed: Vec<String> = stage.model.models.iter().map(ToString::to_string).collect();
            assert!(
                !reachable.is_empty(),
                "{}/{} lists {listed:?} and this registry reaches none of them",
                agent.name,
                stage.name,
            );
            let want = bare(reachable[0].model.as_str());

            let got = leviath_runtime::bind::host::choose_model(stage, None, &defaults, &registry)
                .expect("a reachable stage resolves");
            let got_key = bare(got.model.as_str());
            assert_eq!(
                got_key, want,
                "{}/{} lists {listed:?} and the first one reachable is {want}, but it \
                 resolved to {got_key} on {}: the host chose a different model, not a \
                 different route",
                agent.name, stage.name, got.provider,
            );
        }
    }
    assert!(checked > 0, "no bundled stage was checked");
}

/// A provider claims the models it serves, and not the rest.
///
/// `serves_model` decides which provider a bare model name resolves to, so a
/// provider that over-claims wins models it cannot run. It is the stage
/// test above from the other direction: there the host picks a route and
/// gets the wrong model, here a provider claims a model it has never heard
/// of.
#[test]
fn a_provider_does_not_claim_models_from_other_vendors() {
    let providers = setup_providers();
    let get = |want: &str| {
        providers
            .iter()
            .find(|(n, _)| *n == want)
            .map(|(_, p)| p)
            .expect("configured above")
    };

    // Each of these is unmistakably one vendor's.
    for (owner, model) in [
        ("anthropic", "claude-opus-5"),
        ("openai", "gpt-5.5"),
        ("google", "gemini-3.1-pro-preview"),
    ] {
        assert!(
            get(owner).serves_model(model).is_some(),
            "{owner} should serve its own {model}"
        );
        for other in ["anthropic", "openai", "google"] {
            if other == owner {
                continue;
            }
            assert!(
                get(other).serves_model(model).is_none(),
                "{other} claims {model}, which belongs to {owner}: a bare \
                 model name would resolve to a provider that cannot run it"
            );
        }
    }

    // And nobody claims a model that does not exist.
    for name in ["anthropic", "openai", "google"] {
        assert!(
            get(name).serves_model("not-a-real-model-xyz").is_none(),
            "{name} claims a model nobody has"
        );
    }
}

/// A `custom` carry must actually name regions.
///
/// One that names none is an expensive no-op that still reads and still
/// validates. Discovered from BUNDLED_AGENTS so it covers whatever ships, not
/// a list kept by hand.
#[test]
fn a_custom_carry_always_names_regions() {
    let mut custom = 0;
    for (agent, file) in bundled_files() {
        for edge in &file.graph.edges {
            let EdgeCarry::Custom {
                carry,
                compact,
                clear,
                ..
            } = &edge.carry
            else {
                continue;
            };
            custom += 1;
            assert!(
                !(carry.is_empty() && compact.is_empty() && clear.is_empty()),
                "{}: {} -> {} declares a custom carry that names no regions, so it does nothing",
                agent.name,
                edge.from,
                edge.to
            );
        }
    }
    assert!(custom > 0, "no bundled edge carries regions by name");
}

#[test]
fn every_bundled_stage_offers_every_provider_setup_can_configure() {
    // Getting Started promises that one provider is all you need: on a
    // machine configured with exactly one of them, every stage still runs.
    //
    // Asked of the providers themselves rather than of the spelling. A
    // blueprint names models and leaves routing to the machine, so the
    // question is not "does this stage name a provider" but "does this
    // provider serve anything this stage named".
    //
    // Discovered from BUNDLED_AGENTS rather than enumerated, so a new
    // blueprint is covered the day it lands.
    let providers = setup_providers();
    // A gateway fronts the other vendors, so it reaches whatever they reach.
    // Its own answer comes from a catalogue fetched at startup, which a test
    // with no network cannot consult.
    let native = |key: &str| {
        providers
            .iter()
            .any(|(n, p)| *n != "openrouter" && p.serves_model(key).is_some())
    };
    for (agent, file) in bundled_files() {
        for stage in &file.graph.stages {
            // Portability is about stages that fall back to the user's
            // configured default. A stage that pins its models on purpose
            // (`allow_user_default = false`) is opting out of it - an image
            // or 3D stage cannot be provider-portable, because a vendor like
            // Anthropic has no image model at all - so it is not held to the
            // every-provider rule.
            if !stage.model.allow_user_default {
                continue;
            }
            let models = &stage.model.models;
            for (name, provider) in &providers {
                // A media-only provider (Meshy makes 3D models, not text) is
                // a supplement a machine adds alongside a text provider for
                // the stages that need it, never its sole provider.
                if *name == "meshy" {
                    continue;
                }
                let reachable = models.iter().any(|entry| {
                    if let Some(pinned) = &entry.provider {
                        return pinned.as_str() == *name;
                    }
                    let model = entry.model.as_str();
                    let key = model.rsplit('/').next().unwrap_or(model);
                    if *name == "openrouter" {
                        return native(key);
                    }
                    provider.serves_model(key).is_some()
                });
                assert!(
                    reachable,
                    "{}/{} names nothing {name} can run, so a machine holding only that \
                     provider cannot reach this stage: {models:?}",
                    agent.name, stage.name
                );
            }
            // Ollama needs no API key, so it registers on every machine. Any
            // position but last makes it beat a provider the user actually
            // configured, and the run then dies on its first inference.
            assert_eq!(
                models
                    .last()
                    .and_then(|m| m.provider.as_ref())
                    .map(|p| p.as_str()),
                Some("ollama"),
                "{}/{} must list ollama last",
                agent.name,
                stage.name
            );
        }
    }
}

/// The lint env for a bundled agent: the built-ins, the sub-agent tools,
/// and the agent's own `tools/<name>.rhai`, each of which defines `<name>`.
///
/// Built by hand rather than through `LintEnv::offline`, which discovers
/// script tools by reading a directory: a bundled agent's files are
/// compiled into the binary and there is no directory to read.
fn lint_env_for(agent: &BundledAgent) -> crate::lint::LintEnv {
    let mut known_tools: std::collections::HashSet<String> = leviath_tools::BuiltinTools::new(
        leviath_tools::ToolContext::new(std::path::PathBuf::from(".")),
    )
    .names()
    .into_iter()
    .collect();
    known_tools.extend(leviath_tools::BuiltinTools::subagent_tool_names());
    known_tools.extend(
        agent
            .files
            .iter()
            .filter_map(|(rel, _)| rel.strip_prefix("tools/"))
            .filter_map(|f| f.strip_suffix(".rhai"))
            .map(str::to_string),
    );
    crate::lint::LintEnv {
        known_tools,
        // Empty on purpose: no bundled blueprint grants a tool group, so
        // the group-aware checks have nothing to classify here.
        tool_sources: std::collections::HashMap::new(),
        known_models: crate::commands::models::closed_catalog_models(),
        available_providers: None,
        read_paths: None,
        safe_commands_granted: None,
        // No machine to ask, which is the point: this test asserts what a
        // bundled blueprint says about itself, not what one install happens
        // to have configured.
        provider_catalogs: std::collections::HashMap::new(),
        provider_refusals: std::collections::HashMap::new(),
        unrouted_models: std::collections::HashSet::new(),
        model_windows: crate::commands::models::builtin_model_windows(),
        retention_refusals: std::collections::HashMap::new(),
    }
}

/// No bundled agent ships a blueprint the linter calls broken.
///
/// The errors this catches are the ones that are invisible on inspection: a
/// tool name matching nothing is silently dropped from what the stage
/// advertises, so the model is told the tool does not exist and the stage
/// cannot do its job. A permission for a tool the stage never granted is the
/// same drift from the other side, reading as a grant and not being one.
///
/// Asserted by running the shipped linter rather than by a parallel copy of
/// its rules, and over all discovered agents rather than a list of names.
#[test]
fn no_bundled_agent_has_a_lint_error() {
    for (agent, file) in bundled_files() {
        // Every finding is rendered up front, and the errors are then
        // *counted* rather than collected: a per-error closure would only
        // run when the test is about to fail, which llvm-cov reads as an
        // uncovered region for as long as the invariant holds.
        let rendered: Vec<(bool, String)> =
            crate::lint::lint_blueprint(&file, &lint_env_for(agent))
                .iter()
                .map(|f| (f.is_error(), format!("{} [{}]", f.one_line(), f.code)))
                .collect();
        let error_count = rendered.iter().filter(|(is_error, _)| *is_error).count();
        assert_eq!(
            error_count, 0,
            "bundled agent {} has lint errors, among {rendered:?}",
            agent.name
        );
    }
}

/// The invariant above can actually fail - a check over shipped data that
/// happens to pass says nothing about whether it would catch drift.
#[test]
fn the_lint_invariant_catches_a_typo_and_an_orphan_permission() {
    let text = r#"[blueprint]
name = "x"
version = "0.1.0"

[graph]
layout = { total_budget_tokens = 1000, regions = [{ name = "task", kind = "pinned", budget = 1000 }] }

[[graph.stages]]
name = "only"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
tools = ["read_file", "raed_file"]
tool_permissions = { write_file = "allow" }
"#;
    let file =
        BlueprintFile::parse(text).expect("the fixture reads; it is the lint that should object");
    // Reuse the same env shape a real bundled agent gets, minus any scripts.
    let env = lint_env_for(&BundledAgent {
        name: "x",
        version: "0.1.0",
        files: &[],
    });
    let codes: Vec<&str> = crate::lint::lint_blueprint(&file, &env)
        .iter()
        .filter(|f| f.is_error())
        .map(|f| f.code)
        .collect();
    assert_eq!(codes, ["unknown-tool", "orphan-stage-permission"]);
}

/// The names a split prompt's example item fills: the keys of every
/// `"inputs": {...}` object it shows.
fn example_inputs(prompt: &str) -> Vec<String> {
    prompt
        .split("\"inputs\": {")
        .skip(1)
        .flat_map(|rest| {
            let object = rest.split('}').next().unwrap_or_default();
            object
                .split("\", \"")
                .filter_map(|pair| pair.trim_start_matches('"').split('"').next())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn example_inputs_reads_the_keys_of_every_inputs_object() {
    let prompt = r#"items: [{"id": "a", "inputs": {"task": "<x, y>", "scope": "<why>"}}]
or [{"id": "b", "inputs": {"slice": "s"}}]"#;
    assert_eq!(example_inputs(prompt), ["task", "scope", "slice"]);
    assert!(example_inputs("no example here").is_empty());
}

/// A fan-out's split prompt shows the model the shape of a work item, and
/// every input it names must be one the worker declares (or the `task` every
/// worker takes): a `fan_out` item is checked against the worker's inputs,
/// so an example naming anything else teaches the model a call that is
/// refused.
#[test]
fn every_split_prompt_names_only_inputs_its_worker_takes() {
    use leviath_runtime::spec::graph::WorkerSource;
    let files = bundled_files();
    let declared = |graph: &RunGraph| -> Vec<String> {
        graph
            .inputs
            .iter()
            .map(|d| d.name.to_string())
            .chain(["task".to_string()])
            .collect()
    };
    let mut checked = 0;
    for (agent, file) in &files {
        for stage in &file.graph.stages {
            let StageMode::FanOut(fan) = &stage.mode else {
                continue;
            };
            let worker_inputs = match &fan.worker {
                WorkerSource::Blueprint(reference) => {
                    let (_, worker) = files
                        .iter()
                        .find(|(a, _)| a.name == reference.name.as_str())
                        .expect("a bundled fan-out runs a bundled worker");
                    declared(&worker.graph)
                }
                _ => declared(&file.graph),
            };
            let named = example_inputs(&fan.split_prompt);
            assert!(
                !named.is_empty(),
                "{}/{}: the split prompt shows no `inputs` example",
                agent.name,
                stage.name
            );
            for name in named {
                checked += 1;
                assert!(
                    worker_inputs.contains(&name),
                    "{}/{}: the split prompt fills input `{name}`, which its worker does not \
                     declare ({worker_inputs:?})",
                    agent.name,
                    stage.name
                );
            }
        }
    }
    assert!(checked > 0, "no bundled fan-out was checked");
}

#[test]
fn bundled_agent_names_are_unique() {
    let mut names: Vec<&str> = BUNDLED_AGENTS.iter().map(|a| a.name).collect();
    names.sort_unstable();
    let count = names.len();
    names.dedup();
    assert_eq!(count, names.len(), "duplicate bundled agent names");
}

// ─── installed_version ──────────────────────────────────────────────────

#[test]
fn installed_version_reads_the_blueprint_table() {
    let dir = tempfile::tempdir().unwrap();
    let agent = &BUNDLED_AGENTS[0];
    install_bundled(agent, dir.path()).unwrap();

    assert_eq!(
        installed_version(dir.path(), agent.name).as_deref(),
        Some(agent.version)
    );
}

#[test]
fn installed_version_is_none_when_nothing_is_installed() {
    let dir = tempfile::tempdir().unwrap();
    assert!(installed_version(dir.path(), "not-installed").is_none());
}

#[test]
fn installed_version_is_none_for_an_unreadable_file() {
    // A half-written install must read as "not installed" so the wizard
    // offers a clean reinstall rather than refusing to plan.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("broken")).unwrap();
    std::fs::write(
        dir.path().join("broken").join(leviath_blueprint::FILE_NAME),
        "not valid toml {{{",
    )
    .unwrap();

    assert!(installed_version(dir.path(), "broken").is_none());
}

// ─── plan_agent_actions ─────────────────────────────────────────────────

#[test]
fn plan_offers_to_install_everything_into_an_empty_dir() {
    let dir = tempfile::tempdir().unwrap();

    let plan = plan_agent_actions(dir.path());

    assert_eq!(plan.len(), BUNDLED_AGENTS.len());
    for (agent, action) in &plan {
        assert_eq!(*action, AgentAction::Install);
        assert!(action.is_change());
        assert_eq!(
            action.label(agent.version),
            format!("install {}", agent.version)
        );
    }
}

#[test]
fn plan_reports_up_to_date_after_installing() {
    let dir = tempfile::tempdir().unwrap();
    for agent in BUNDLED_AGENTS {
        install_bundled(agent, dir.path()).unwrap();
    }

    let plan = plan_agent_actions(dir.path());

    for (agent, action) in &plan {
        assert_eq!(*action, AgentAction::UpToDate, "{}", agent.name);
        assert!(!action.is_change());
        assert_eq!(action.label(agent.version), "up to date");
    }
}

#[test]
fn plan_reports_an_update_when_the_installed_version_differs() {
    let dir = tempfile::tempdir().unwrap();
    let agent = &BUNDLED_AGENTS[0];
    install_bundled(agent, dir.path()).unwrap();
    // Rewrite the installed blueprint at a different version.
    let file_path = dir
        .path()
        .join(agent.name)
        .join(leviath_blueprint::FILE_NAME);
    let text = std::fs::read_to_string(&file_path).unwrap();
    let bumped = text.replacen(
        &format!("version = \"{}\"", agent.version),
        "version = \"9.9.9\"",
        1,
    );
    std::fs::write(&file_path, bumped).unwrap();

    let plan = plan_agent_actions(dir.path());
    let (_, action) = plan
        .iter()
        .find(|(a, _)| a.name == agent.name)
        .expect("the bundled agent is in the plan");

    assert_eq!(
        *action,
        AgentAction::Update {
            from: "9.9.9".to_string()
        }
    );
    assert!(action.is_change());
    assert_eq!(
        action.label(agent.version),
        format!("update 9.9.9 → {}", agent.version)
    );
}

/// The limitation this closes: comparing versions alone meant a blueprint
/// edited without a version bump read as current forever, so the user was
/// never told their copy had drifted from the one that shipped.
#[test]
fn plan_reports_an_edited_install_as_modified() {
    let dir = tempfile::tempdir().unwrap();
    let agent = &BUNDLED_AGENTS[0];
    install_bundled(agent, dir.path()).unwrap();
    let file_path = dir
        .path()
        .join(agent.name)
        .join(leviath_blueprint::FILE_NAME);
    let text = std::fs::read_to_string(&file_path).unwrap();
    std::fs::write(&file_path, text + "\n# a local edit\n").unwrap();

    let action = action_for(&plan_agent_actions(dir.path()), agent.name);
    assert_eq!(action, AgentAction::Modified);
    // It would change disk, so the wizard offers it - but never unasked,
    // because reinstalling destroys the edit.
    assert!(action.is_change());
    assert!(!action.preselect());
    let label = action.label(agent.version);
    assert!(label.contains("edited locally"), "{label}");
}

/// `install_bundled` removes the destination first, so a file the user
/// added is destroyed by a reinstall too - which makes it exactly as
/// important to notice as an edited one.
#[test]
fn a_file_the_user_added_or_removed_counts_as_modified() {
    let agent = &BUNDLED_AGENTS[0];

    let added = tempfile::tempdir().unwrap();
    install_bundled(agent, added.path()).unwrap();
    std::fs::write(added.path().join(agent.name).join("notes.md"), "mine").unwrap();
    assert_eq!(
        action_for(&plan_agent_actions(added.path()), agent.name),
        AgentAction::Modified
    );

    // A file deleted from a blueprint that ships more than its agent.toml.
    // The agent.toml still reads at the bundled version, so only the file
    // comparison can catch this.
    let multi = BUNDLED_AGENTS
        .iter()
        .find(|a| a.files.len() > 1)
        .expect("some bundled blueprint ships more than its agent.toml");
    let removed = tempfile::tempdir().unwrap();
    install_bundled(multi, removed.path()).unwrap();
    let extra = multi
        .files
        .iter()
        .map(|(rel, _)| *rel)
        .find(|rel| *rel != leviath_blueprint::FILE_NAME)
        .expect("a file other than the agent.toml");
    std::fs::remove_file(removed.path().join(multi.name).join(extra)).unwrap();
    assert_eq!(
        action_for(&plan_agent_actions(removed.path()), multi.name),
        AgentAction::Modified
    );
}

/// A directory that cannot be walked reads as differing, which is the safe
/// direction: this decides whether overwriting is safe.
#[test]
fn an_unreadable_tree_is_not_up_to_date() {
    assert_eq!(installed_file_count(Path::new("/no/such/dir")), 0);
    let dir = tempfile::tempdir().unwrap();
    assert!(!matches_bundled(&BUNDLED_AGENTS[0], dir.path()));
}

#[test]
fn installed_file_count_walks_nested_directories() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
    std::fs::write(dir.path().join("top.txt"), "x").unwrap();
    std::fs::write(dir.path().join("a/mid.txt"), "x").unwrap();
    std::fs::write(dir.path().join("a/b/leaf.txt"), "x").unwrap();
    assert_eq!(installed_file_count(dir.path()), 3);
}

fn action_for(plan: &[(&'static BundledAgent, AgentAction)], name: &str) -> AgentAction {
    plan.iter()
        .find(|(a, _)| a.name == name)
        .expect("the bundled agent is in the plan")
        .1
        .clone()
}

// ─── stale_install_hint ─────────────────────────────────────────────────

/// The report that prompted this: a user on alpha whose installed `coder`
/// stopped loading, with an error about graph shape and nothing saying the
/// file was simply old. A blueprint that predates a graph rule fails in a
/// way that reads as a broken agent rather than an out-of-date one.
#[test]
fn an_installed_agent_that_will_not_load_is_named_as_out_of_date() {
    let dir = tempfile::tempdir().unwrap();
    let agent = &BUNDLED_AGENTS[0];
    install_bundled(agent, dir.path()).unwrap();
    let manifest = dir
        .path()
        .join(agent.name)
        .join(leviath_blueprint::FILE_NAME);

    // Byte-identical to what this build ships: age is not the story, and
    // sending the user to reinstall the same file would waste their time.
    assert_eq!(stale_install_hint(&manifest, Some(dir.path())), None);

    // Any difference is enough. The version field is deliberately not
    // consulted, because it routinely does not move when the file does:
    // both coder blueprints in the report were `0.0.2`.
    std::fs::write(
        &manifest,
        "[blueprint]\nname = \"x\"\nversion = \"0.0.2\"\n",
    )
    .unwrap();
    let hint = stale_install_hint(&manifest, Some(dir.path())).expect("a changed copy is named");
    assert!(hint.contains(agent.name), "{hint}");
    assert!(hint.contains("lev setup"), "{hint}");
}

/// Narrow in the same way as the note: it speaks only for the installed
/// copy, so a blueprint of the user's own is never blamed on a bundled one
/// that shares its name, and neither is a path with no agents dir to check.
/// The suffix form is what both call sites actually use, and the thing that
/// must not decorate an error with a blank paragraph when there is no hint.
#[test]
fn the_suffix_carries_the_hint_or_nothing_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let agent = &BUNDLED_AGENTS[0];
    install_bundled(agent, dir.path()).unwrap();
    let manifest = dir
        .path()
        .join(agent.name)
        .join(leviath_blueprint::FILE_NAME);

    // Nothing to say: an empty string, not a separator with nothing after it.
    assert_eq!(
        stale_install_suffix(&manifest, Some(dir.path()), "\n\n"),
        ""
    );

    std::fs::write(&manifest, "[blueprint]\nname = \"x\"\n").unwrap();
    let suffix = stale_install_suffix(&manifest, Some(dir.path()), "\n\n");
    assert!(suffix.starts_with("\n\n"), "{suffix:?}");
    assert!(suffix.contains(agent.name), "{suffix:?}");
    // The daemon writes one line rather than a paragraph, same hint.
    assert!(
        stale_install_suffix(&manifest, Some(dir.path()), ". ").starts_with(". "),
        "the separator is the caller's choice"
    );
}

#[test]
fn the_hint_stays_quiet_outside_the_installed_copy() {
    let dir = tempfile::tempdir().unwrap();
    let agent = &BUNDLED_AGENTS[0];
    install_bundled(agent, dir.path()).unwrap();

    let elsewhere = dir.path().join("elsewhere").join(agent.name);
    std::fs::create_dir_all(&elsewhere).unwrap();
    let mine = elsewhere.join(leviath_blueprint::FILE_NAME);
    std::fs::write(&mine, "[blueprint]\nname = \"mine\"\n").unwrap();
    assert_eq!(stale_install_hint(&mine, Some(dir.path())), None);

    // A name no bundled agent has, inside the agents dir.
    let other = dir.path().join("not-a-bundled-agent");
    std::fs::create_dir_all(&other).unwrap();
    let manifest = other.join(leviath_blueprint::FILE_NAME);
    std::fs::write(&manifest, "[blueprint]\nname = \"other\"\n").unwrap();
    assert_eq!(stale_install_hint(&manifest, Some(dir.path())), None);

    // And with nowhere to look, it says nothing rather than guessing.
    assert_eq!(
        stale_install_hint(
            &dir.path()
                .join(agent.name)
                .join(leviath_blueprint::FILE_NAME),
            None
        ),
        None
    );
}

// ─── stale_install_note ─────────────────────────────────────────────────

/// The case that prompted this: an install sitting versions behind, with
/// nothing saying so at the moment it mattered.
#[test]
fn a_stale_install_is_named_when_the_run_starts() {
    let dir = tempfile::tempdir().unwrap();
    let agent = &BUNDLED_AGENTS[0];
    install_bundled(agent, dir.path()).unwrap();
    let file = dir
        .path()
        .join(agent.name)
        .join(leviath_blueprint::FILE_NAME);

    // At the bundled version there is nothing to say.
    assert_eq!(
        stale_install_note(&file, agent.name, agent.version, Some(dir.path())),
        None
    );

    let note = stale_install_note(&file, agent.name, "0.0.1", Some(dir.path()))
        .expect("a behind install is named");
    assert!(note.contains("0.0.1"), "{note}");
    assert!(note.contains(agent.version), "{note}");
    assert!(note.contains("lev setup"), "{note}");
}

/// Deliberately narrow: a blueprint of the user's own that happens to share
/// a name with a bundled one is never nagged about, and neither is one this
/// build does not ship.
#[test]
fn a_blueprint_that_is_not_the_installed_copy_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let agent = &BUNDLED_AGENTS[0];
    install_bundled(agent, dir.path()).unwrap();
    let file = dir
        .path()
        .join(agent.name)
        .join(leviath_blueprint::FILE_NAME);

    // Somewhere else on disk, under the same name.
    let elsewhere = tempfile::tempdir().unwrap();
    let copy = elsewhere
        .path()
        .join(agent.name)
        .join(leviath_blueprint::FILE_NAME);
    assert_eq!(
        stale_install_note(&copy, agent.name, "0.0.1", Some(dir.path())),
        None,
        "not the installed copy"
    );

    // No agents dir resolves at all.
    assert_eq!(stale_install_note(&file, agent.name, "0.0.1", None), None);

    // A name this build ships nothing for.
    let other = dir
        .path()
        .join("not-a-bundled-agent")
        .join(leviath_blueprint::FILE_NAME);
    assert_eq!(
        stale_install_note(&other, "not-a-bundled-agent", "0.0.1", Some(dir.path())),
        None
    );
}

// ─── install_bundled ────────────────────────────────────────────────────

#[test]
fn install_writes_every_file_including_nested_ones() {
    let dir = tempfile::tempdir().unwrap();
    // Pick a blueprint that actually has a nested `tools/` file, so the
    // create_dir_all arm is exercised by a real shipped layout rather than
    // a fixture. If none ships nested files any more, the flat arm below
    // still covers the rest.
    for agent in BUNDLED_AGENTS {
        install_bundled(agent, dir.path()).unwrap();
        for (rel, contents) in agent.files {
            let written = std::fs::read_to_string(dir.path().join(agent.name).join(rel));
            assert!(written.is_ok(), "{}/{rel} was not written", agent.name);
            assert_eq!(written.expect("asserted Ok just above"), *contents);
        }
    }
    assert!(
        BUNDLED_AGENTS
            .iter()
            .any(|a| a.files.iter().any(|(rel, _)| rel.contains('/'))),
        "no bundled blueprint has a nested file, so install's mkdir path is untested"
    );
}

#[test]
fn install_replaces_an_existing_tree_and_drops_stale_files() {
    let dir = tempfile::tempdir().unwrap();
    let agent = &BUNDLED_AGENTS[0];
    install_bundled(agent, dir.path()).unwrap();
    let stale = dir
        .path()
        .join(agent.name)
        .join("stale-from-an-older-version");
    std::fs::write(&stale, "leftover").unwrap();

    install_bundled(agent, dir.path()).unwrap();

    assert!(
        !stale.exists(),
        "a reinstall must not leave files from the previous version behind"
    );
    assert!(
        dir.path()
            .join(agent.name)
            .join(leviath_blueprint::FILE_NAME)
            .exists()
    );
}

#[test]
fn install_surfaces_a_directory_creation_failure() {
    // `agents_dir` is itself a file, so creating the blueprint directory
    // under it fails.
    let dir = tempfile::tempdir().unwrap();
    let blocked = dir.path().join("not-a-dir");
    std::fs::write(&blocked, "").unwrap();

    let result = install_bundled(&BUNDLED_AGENTS[0], &blocked);

    assert!(result.is_err());
}

#[test]
fn install_surfaces_a_file_write_failure() {
    // Isolating the `write` error from the `create_dir_all` error needs a
    // layout where the directory step succeeds and only the write fails.
    // A synthetic blueprint whose second entry names a path the first entry
    // already created as a *directory* does exactly that: `create_dir_all`
    // sees an existing dir and returns Ok, then the write hits EISDIR.
    // No shipped blueprint has that shape, hence the hand-built one.
    let agent = BundledAgent {
        name: "collides-with-its-own-directory",
        version: "0.0.1",
        files: &[("tools/a.rhai", "nested first"), ("tools", "then the dir")],
    };
    let dir = tempfile::tempdir().unwrap();

    let result = install_bundled(&agent, dir.path());

    assert!(result.is_err());
}

#[test]
fn install_surfaces_a_remove_failure() {
    // The destination exists but is a *file*, so `remove_dir_all` fails
    // rather than the write.
    let dir = tempfile::tempdir().unwrap();
    let agent = &BUNDLED_AGENTS[0];
    std::fs::write(dir.path().join(agent.name), "").unwrap();

    let result = install_bundled(agent, dir.path());

    assert!(result.is_err());
}

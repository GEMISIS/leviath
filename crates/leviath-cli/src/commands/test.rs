//! `lev test` - Run agent tests

use clap::Args;
use leviath_providers::InferenceRequest;
use leviath_runtime::{ContextWindow, ProviderRegistry, context_setup};
use serde::Deserialize;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use crate::config::Config;
use leviath_core::manifest::parse_manifest;
use leviath_core::truncate_at_boundary;

/// Arguments for `lev test`.
#[derive(Args)]
pub struct TestArgs {
    /// Path to agent project
    #[arg(value_name = "PATH")]
    pub path: Option<String>,

    /// Test filter pattern
    #[arg(short, long)]
    pub filter: Option<String>,

    /// Validate test structure without running agents (no API calls)
    #[arg(long)]
    pub dry_run: bool,
}

/// A test case loaded from a TOML test file.
#[derive(Debug, Deserialize)]
struct TestCase {
    name: String,
    input: String,
    #[serde(default)]
    expect_contains: Option<String>,
    #[serde(default)]
    expect_tool_call: Option<String>,
    #[serde(default)]
    max_tokens: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct TestFile {
    test: Vec<TestCase>,
}

/// Run `lev test`: drive a blueprint's declared test cases.
pub(crate) async fn execute(args: TestArgs) -> anyhow::Result<()> {
    execute_with_registry(args, Box::new(build_registry_from_config)).await
}

/// Builds the real provider registry from a loaded [`Config`] - the
/// production `build_registry` passed to [`execute_with_registry`] by
/// [`execute`].
fn build_registry_from_config(
    config: &Config,
) -> Result<ProviderRegistry, leviath_providers::ProviderError> {
    build_registry_from_config_with(config, &leviath_providers::provider::build_http_client)
}

/// [`build_registry_from_config`], with client construction injected so the
/// failure path is reachable from a test.
fn build_registry_from_config_with(
    config: &Config,
    build_client: leviath_providers::provider::HttpClientFactory<'_>,
) -> Result<ProviderRegistry, leviath_providers::ProviderError> {
    let mut reg = ProviderRegistry::new();
    // `lev test` uses one client for every provider it registers: the command
    // has no per-provider timeout to honour, so there is nothing to key on.
    let client = build_client(None)
        .map_err(|e| leviath_providers::ProviderError::ClientBuild(e.to_string()))?;

    if let Some(ref key) = config.providers.anthropic_api_key {
        reg.register(
            "anthropic".to_string(),
            Arc::new(leviath_providers::AnthropicProvider::new(
                client.clone(),
                key.clone(),
            )),
        );
    }
    if let Some(ref key) = config.providers.openai_api_key {
        reg.register(
            "openai".to_string(),
            Arc::new(leviath_providers::OpenAIProvider::new(
                client.clone(),
                key.clone(),
            )),
        );
    }
    if let Some(ref key) = config.providers.google_api_key {
        reg.register(
            "google".to_string(),
            Arc::new(leviath_providers::GeminiProvider::new(
                client.clone(),
                key.clone(),
            )),
        );
    }
    if let Some(ref key) = config.openrouter_api_key {
        reg.register(
            "openrouter".to_string(),
            Arc::new(leviath_providers::OpenRouterProvider::new(
                client.clone(),
                key.clone(),
            )),
        );
    }
    let ollama_url = config
        .ollama_base_url
        .as_deref()
        .unwrap_or("http://localhost:11434");
    reg.register(
        "ollama".to_string(),
        Arc::new(leviath_providers::OllamaProvider::with_base_url(
            client.clone(),
            ollama_url.to_string(),
        )),
    );

    Ok(reg)
}

/// How `lev test` gets its provider registry.
///
/// Fallible because constructing a provider's outbound HTTPS client reads the
/// machine's root certificate store and can fail; boxed for the
/// monomorphization reason spelled out on [`execute_with_registry`].
type RegistryBuilder =
    Box<dyn FnOnce(&Config) -> Result<ProviderRegistry, leviath_providers::ProviderError>>;

/// Core of [`execute`], with provider-registry construction injected so
/// tests can drive the non-dry-run path with a mock [`Provider`] instead of
/// either skipping it (dry-run only) or making a real, billed network call
/// through whatever the developer's real `~/.leviath/config.toml` happens to
/// contain.
///
/// `build_registry` is a boxed trait object ([`RegistryBuilder`]) rather than an
/// `impl FnOnce` bound so every caller - production's `build_registry_from_config` and every
/// test's distinct `mock_registry_builder(...)` closure - shares exactly
/// ONE monomorphization of this (large, many-branch) function instead of
/// one per closure type. This was a confirmed generic-monomorphization
/// coverage-attribution artifact: every source position had a covered
/// instantiation (confirmed via HTML/JSON segment inspection showing no
/// red/uncovered regions anywhere in this function), but the summary table
/// still reported 32 regions / 21 lines missed - the largest such residual
/// in this crate.
async fn execute_with_registry(
    args: TestArgs,
    build_registry: RegistryBuilder,
) -> anyhow::Result<()> {
    let path = args.path.unwrap_or_else(|| ".".to_string());
    tracing::info!(path = %path, "Running agent tests");

    let project_path = Path::new(&path);

    // Verify agent.leviath exists
    let manifest_path = project_path.join(leviath_core::files::MANIFEST_FILENAME);
    if !manifest_path.exists() {
        anyhow::bail!(
            "No agent.leviath found in '{}'. Not an agent project.",
            project_path.display()
        );
    }

    let tests_dir = project_path.join("tests");
    if !tests_dir.exists() {
        println!("No tests directory found. Create tests/ with .toml or .rhai files.");
        println!("\nExample test file (tests/basic.toml):");
        println!("  [[test]]");
        println!("  name = \"basic_response\"");
        println!("  input = \"Hello\"");
        println!("  expect_contains = \"hello\"");
        // The other two keys are deliberately not spelled out here: a second
        // partial example is a second thing to drift. `expect_tool_call` and
        // `max_tokens` were each parsed and ignored for months, which is what
        // an undocumented format buys.
        println!("\nAlso available: expect_tool_call, max_tokens.");
        println!("See https://leviath.dev/docs/cli#lev-test-path for what each does.");
        return Ok(());
    }

    if args.dry_run {
        println!("Dry run mode: validating test structure only (no API calls)\n");
    }

    // Parse blueprint and set up providers (only if not dry_run)
    let manifest_content = fs::read_to_string(&manifest_path)?;
    let blueprint = parse_manifest(&manifest_content)?;

    // Custom regions' Rhai scripts, resolved exactly as a real spawn would
    // (blueprint-dir-relative, compile-checked, hard error) - `lev test` is
    // precisely the preview loop where a hook author wants the hook to run.
    let region_scripts =
        crate::daemon::spawn::resolve_region_scripts(&blueprint, &manifest_path.to_string_lossy())
            .map_err(|e| anyhow::anyhow!(e))?;

    let registry = if !args.dry_run {
        let config = Config::load()?;
        Some(build_registry(&config)?)
    } else {
        None
    };

    let mut total = 0;
    let mut passed = 0;
    let mut failed = 0;
    let mut failures: Vec<String> = Vec::new();

    // Run .toml test files and .rhai test scripts (single directory scan)
    for entry in fs::read_dir(&tests_dir)?.flatten() {
        let test_path = entry.path();

        if test_path.extension().and_then(|e| e.to_str()) == Some("toml") {
            let file_name = test_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");

            println!("Running test file: {}", file_name);

            let content = fs::read_to_string(&test_path)?;
            let test_file: TestFile = toml::from_str(&content)
                .map_err(|e| anyhow::anyhow!("Failed to parse test file '{}': {}", file_name, e))?;

            for test_case in &test_file.test {
                // Apply filter if provided
                if let Some(ref filter) = args.filter
                    && !test_case.name.contains(filter.as_str())
                {
                    continue;
                }

                total += 1;

                if args.dry_run {
                    // Dry-run: validate structure only
                    let test_valid = validate_test_case(test_case);
                    if test_valid {
                        passed += 1;
                        println!("  PASS (dry-run): {}", test_case.name);
                    } else {
                        failed += 1;
                        let msg = format!("{}: test case validation failed", test_case.name);
                        println!("  FAIL (dry-run): {}", msg);
                        failures.push(msg);
                    }
                } else {
                    // Real run: execute inference and check assertions
                    let registry = registry
                        .as_ref()
                        .expect("registry should exist in non-dry-run");
                    match run_test_case(&blueprint, registry, test_case, &region_scripts).await {
                        Ok(true) => {
                            passed += 1;
                            println!("  PASS: {}", test_case.name);
                        }
                        Ok(false) => {
                            failed += 1;
                            let msg = format!("{}: assertions failed", test_case.name);
                            println!("  FAIL: {}", msg);
                            failures.push(msg);
                        }
                        Err(e) => {
                            failed += 1;
                            let msg = format!("{}: {}", test_case.name, e);
                            println!("  FAIL: {}", msg);
                            failures.push(msg);
                        }
                    }
                }
            }
        } else if test_path.extension().and_then(|e| e.to_str()) == Some("rhai") {
            let file_name = test_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");

            // Apply filter if provided
            if let Some(ref filter) = args.filter
                && !file_name.contains(filter.as_str())
            {
                continue;
            }

            total += 1;
            println!("Running script: {}", file_name);

            let script = fs::read_to_string(&test_path)?;
            let engine = leviath_scripting::ScriptEngine::new();
            let mut scope = rhai::Scope::new();

            match engine.execute(&script, &mut scope) {
                Ok(result) => {
                    if let Ok(success) = result.as_bool() {
                        if success {
                            passed += 1;
                            println!("  PASS: {}", file_name);
                        } else {
                            failed += 1;
                            let msg = format!("{}: script returned false", file_name);
                            println!("  FAIL: {}", msg);
                            failures.push(msg);
                        }
                    } else {
                        passed += 1;
                        println!("  PASS: {} (returned: {})", file_name, result);
                    }
                }
                Err(e) => {
                    failed += 1;
                    let msg = format!("{}: {}", file_name, e);
                    println!("  FAIL: {}", msg);
                    failures.push(msg);
                }
            }
        }
    }

    // Report results
    println!("\n--- Results ---");
    println!("{} passed, {} failed, {} total", passed, failed, total);

    if !failures.is_empty() {
        println!("\nFailures:");
        for f in &failures {
            println!("  - {}", f);
        }
        anyhow::bail!("{} test(s) failed", failed);
    }

    if total == 0 {
        println!("No test files found in tests/ directory.");
    }

    Ok(())
}

/// How many output tokens one case may ask for.
///
/// A case's `max_tokens` narrows `ceiling` and never widens it: it is there to
/// keep one test cheap, not to let a test ask for more than the context window
/// or the model allows. A free function rather than an inline `map_or` so the
/// rule is exercised without reaching a provider.
fn resolved_max_tokens(case_cap: Option<usize>, ceiling: usize) -> usize {
    match case_cap {
        Some(cap) => cap.min(ceiling),
        None => ceiling,
    }
}

/// The tools a stage advertises, as the provider wants them.
///
/// `lev test` drives one inference, so this is the same set the first turn of a
/// real run would see - which is what makes `expect_tool_call` mean the same
/// thing here as it does in production.
fn stage_tools(stage: &leviath_core::Stage) -> Vec<leviath_providers::Tool> {
    // Built over a throwaway workdir: `lev test` never executes a tool, it only
    // needs the definitions so the model can choose to call one.
    let builtins =
        leviath_tools::BuiltinTools::new(leviath_tools::ToolContext::new(std::env::temp_dir()));
    let mut defs = builtins.tool_defs();
    defs.extend(leviath_tools::BuiltinTools::subagent_tool_defs());
    stage
        .available_tools
        .iter()
        .filter_map(|name| defs.iter().find(|d| d.name == *name).cloned())
        .collect()
}

/// Run a single test case: build a one-off context window from the blueprint,
/// run one inference against the resolved provider, and check the assertions.
async fn run_test_case(
    blueprint: &leviath_core::Blueprint,
    registry: &ProviderRegistry,
    test: &TestCase,
    region_scripts: &std::collections::HashMap<
        String,
        std::sync::Arc<leviath_scripting::region_hook::RegionScript>,
    >,
) -> anyhow::Result<bool> {
    // Model config comes from the first stage.
    let stage = blueprint
        .stages
        .first()
        .ok_or(anyhow::anyhow!("Blueprint has no stages"))?;
    let provider_name = stage.model.provider();
    let model_name = stage.model.model();

    let provider = registry.get(provider_name).ok_or_else(|| {
        anyhow::anyhow!(
            "Provider '{}' is not configured. Set API key in ~/.leviath/config.toml",
            provider_name
        )
    })?;

    // Build a standalone context window from the blueprint's layout, seeding the
    // test input as the task, then assemble a single inference request. This
    // mirrors what the ECS pipeline's spawner does, without the shared world:
    // `lev test` only needs one inference to validate a stage's first response.
    let mut window = ContextWindow::new(blueprint.context_layout.total_budget_tokens);
    window.region_scripts = region_scripts.clone();
    context_setup::init_window(&mut window, blueprint, &test.input);

    // Assemble with real stage metadata so custom-region render hooks see
    // what a live run's first inference would (iteration 0).
    let assembled = window.assemble_with_meta(&leviath_runtime::custom_region::AssembleMeta {
        stage_name: stage.name.clone(),
        stage_iterations: 0,
        model: model_name.to_string(),
        // One assembly, so there is no previous request to compare against.
        previous_system_hash: None,
        previous_block_hashes: Vec::new(),
    });
    let caps = provider.capabilities(model_name);
    let remaining = window.max_tokens.saturating_sub(window.current_tokens);
    // A case's `max_tokens` narrows the ceiling and never widens it: it is there
    // to keep one test cheap, not to let a test ask for more than the window or
    // the model allows.
    let max_tokens = resolved_max_tokens(test.max_tokens, remaining.min(caps.max_output_tokens));
    let temperature = if caps.supports_temperature { 0.7 } else { 0.0 };
    let request = InferenceRequest {
        system: assembled.system_blocks,
        messages: assembled.messages,
        model: model_name.to_string(),
        max_tokens,
        temperature,
        // The stage's own tools, so a case can assert on a tool call at all.
        // Advertising none was the prior behaviour and made `expect_tool_call`
        // unsatisfiable: the model cannot call a tool it was never offered, so
        // every such assertion failed whatever the agent did.
        tools: stage_tools(stage),
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    };

    let response = provider
        .infer(&request)
        .await
        .map_err(|e| anyhow::anyhow!("Inference failed: {}", e))?;

    // Check assertions
    let mut all_passed = true;

    if let Some(ref expected) = test.expect_contains {
        let content_lower = response.content.to_lowercase();
        let expected_lower = expected.to_lowercase();
        if !content_lower.contains(&expected_lower) {
            println!(
                "    expect_contains failed: response does not contain '{}'",
                expected
            );
            println!("    response: {}", truncate_str(&response.content, 200));
            all_passed = false;
        }
    }

    if let Some(ref expected_tool) = test.expect_tool_call {
        let has_tool = response
            .tool_calls
            .iter()
            .any(|tc| tc.name == *expected_tool);
        if !has_tool {
            println!(
                "    expect_tool_call failed: no tool call to '{}'",
                expected_tool
            );
            let tool_names: Vec<&str> = response
                .tool_calls
                .iter()
                .map(|tc| tc.name.as_str())
                .collect();
            println!("    actual tool calls: {:?}", tool_names);
            all_passed = false;
        }
    }

    Ok(all_passed)
}

/// Validate a test case structure (checks that it's well-formed).
fn validate_test_case(test: &TestCase) -> bool {
    if test.name.is_empty() {
        return false;
    }
    if test.input.is_empty() {
        return false;
    }
    // Must have at least one assertion
    if test.expect_contains.is_none() && test.expect_tool_call.is_none() {
        return false;
    }
    true
}

/// Shorten a model response for the assertion-failure preview.
///
/// Cuts on a char boundary: this runs on raw model output, and a byte cut-off
/// through an emoji once panicked `lev test` outright.
fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", truncate_at_boundary(s, max))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fixtures, with_tracing, write_test_agent};

    // ─── validate_test_case ────────────────────────────────────────────────

    #[test]
    fn validate_test_case_valid_with_expect_contains() {
        let tc = TestCase {
            name: "basic".to_string(),
            input: "hello".to_string(),
            expect_contains: Some("world".to_string()),
            expect_tool_call: None,
            max_tokens: None,
        };
        assert!(validate_test_case(&tc));
    }

    #[test]
    fn validate_test_case_valid_with_expect_tool_call() {
        let tc = TestCase {
            name: "tool_test".to_string(),
            input: "do something".to_string(),
            expect_contains: None,
            expect_tool_call: Some("bash".to_string()),
            max_tokens: None,
        };
        assert!(validate_test_case(&tc));
    }

    #[test]
    fn validate_test_case_valid_with_both_assertions() {
        let tc = TestCase {
            name: "both".to_string(),
            input: "test".to_string(),
            expect_contains: Some("output".to_string()),
            expect_tool_call: Some("read_file".to_string()),
            max_tokens: Some(100),
        };
        assert!(validate_test_case(&tc));
    }

    #[test]
    fn validate_test_case_empty_name_fails() {
        let tc = TestCase {
            name: String::new(),
            input: "hello".to_string(),
            expect_contains: Some("world".to_string()),
            expect_tool_call: None,
            max_tokens: None,
        };
        assert!(!validate_test_case(&tc));
    }

    #[test]
    fn validate_test_case_empty_input_fails() {
        let tc = TestCase {
            name: "test".to_string(),
            input: String::new(),
            expect_contains: Some("world".to_string()),
            expect_tool_call: None,
            max_tokens: None,
        };
        assert!(!validate_test_case(&tc));
    }

    #[test]
    fn validate_test_case_no_assertions_fails() {
        let tc = TestCase {
            name: "test".to_string(),
            input: "hello".to_string(),
            expect_contains: None,
            expect_tool_call: None,
            max_tokens: None,
        };
        assert!(!validate_test_case(&tc));
    }

    // ─── truncate_str ──────────────────────────────────────────────────────

    #[test]
    fn truncate_str_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_exact() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn truncate_str_long() {
        assert_eq!(truncate_str("hello world", 5), "hello...");
    }

    #[test]
    fn truncate_str_empty() {
        assert_eq!(truncate_str("", 5), "");
    }

    // ─── TestFile TOML parsing ─────────────────────────────────────────────

    #[test]
    fn parse_test_file_toml() {
        let toml_content = r#"
[[test]]
name = "greeting"
input = "Say hello"
expect_contains = "hello"

[[test]]
name = "tool_use"
input = "Read file.txt"
expect_tool_call = "read_file"
max_tokens = 500
"#;
        let test_file: TestFile = toml::from_str(toml_content).unwrap();
        assert_eq!(test_file.test.len(), 2);
        assert_eq!(test_file.test[0].name, "greeting");
        assert_eq!(test_file.test[0].input, "Say hello");
        assert_eq!(test_file.test[0].expect_contains.as_deref(), Some("hello"));
        assert!(test_file.test[0].expect_tool_call.is_none());
        assert!(test_file.test[0].max_tokens.is_none());

        assert_eq!(test_file.test[1].name, "tool_use");
        assert_eq!(
            test_file.test[1].expect_tool_call.as_deref(),
            Some("read_file")
        );
        assert_eq!(test_file.test[1].max_tokens, Some(500));
    }

    /// A minimal model config, since `Stage::new` needs one and these tests
    /// never reach a provider.
    fn test_model() -> leviath_core::blueprint::ModelConfig {
        leviath_core::blueprint::ModelConfig::new("anthropic".to_string(), "m".to_string())
    }

    /// The bug these two fixes closed, pinned so it cannot reopen: both keys
    /// were parsed, asserted on *as parsed values*, and then ignored. A test
    /// that only checks deserialisation certifies nothing about behaviour.
    #[test]
    fn a_case_max_tokens_narrows_the_ceiling_and_never_widens_it() {
        let ceiling = 4_000;
        assert_eq!(
            resolved_max_tokens(Some(500), ceiling),
            500,
            "a smaller case cap wins"
        );
        assert_eq!(
            resolved_max_tokens(Some(99_000), ceiling),
            ceiling,
            "a case may not ask for more than the model allows"
        );
        assert_eq!(
            resolved_max_tokens(None, ceiling),
            ceiling,
            "no cap means the full ceiling"
        );
    }

    /// `expect_tool_call` was unsatisfiable: the request advertised no tools, so
    /// the model could never call one and every such assertion failed whatever
    /// the agent did.
    #[test]
    fn a_stage_advertises_its_tools_so_a_tool_call_is_possible() {
        let mut stage = leviath_core::Stage::new("s".to_string(), test_model());
        stage.available_tools = vec!["read_file".to_string(), "write_file".to_string()];
        let tools = stage_tools(&stage);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"read_file"), "got {names:?}");
        assert!(names.contains(&"write_file"), "got {names:?}");
    }

    /// A stage that advertises nothing still sends nothing, so a plain
    /// text-assertion case is unchanged.
    #[test]
    fn a_stage_with_no_tools_advertises_none() {
        let stage = leviath_core::Stage::new("s".to_string(), test_model());
        assert!(stage_tools(&stage).is_empty());
    }

    /// A name the builtins do not know is dropped rather than sent as a tool the
    /// provider would reject.
    #[test]
    fn an_unknown_tool_name_is_not_advertised() {
        let mut stage = leviath_core::Stage::new("s".to_string(), test_model());
        stage.available_tools = vec!["definitely_not_a_tool".to_string()];
        assert!(stage_tools(&stage).is_empty());
    }

    #[test]
    fn parse_test_file_minimal() {
        let toml_content = r#"
[[test]]
name = "min"
input = "test"
expect_contains = "ok"
"#;
        let test_file: TestFile = toml::from_str(toml_content).unwrap();
        assert_eq!(test_file.test.len(), 1);
    }

    #[test]
    fn parse_test_file_invalid_toml_errors() {
        let result: Result<TestFile, _> = toml::from_str("not valid toml {{{{");
        assert!(result.is_err());
    }

    // ─── dry_run flag ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn dry_run_with_temp_project() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();

        // Create minimal agent.leviath
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);

        // Create tests directory with a test file
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        let test_toml = r#"
[[test]]
name = "valid_test"
input = "hello"
expect_contains = "world"
"#;
        std::fs::write(tests_dir.join("basic.toml"), test_toml).unwrap();

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: true,
        };

        let result = execute(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dry_run_no_tests_dir() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();

        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: true,
        };

        // Should succeed but report no tests found
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn execute_no_manifest_errors() {
        let dir = tempfile::tempdir().unwrap();
        let args = TestArgs {
            path: Some(dir.path().to_str().unwrap().to_string()),
            filter: None,
            dry_run: true,
        };
        let result = execute(args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("agent.leviath"));
    }

    // ─── TestCase struct construction ──────────────────────────────────────

    #[test]
    fn test_case_all_fields_from_toml() {
        let toml_content = r#"
[[test]]
name = "full_test"
input = "full input"
expect_contains = "expected"
expect_tool_call = "bash"
max_tokens = 1000
"#;
        let test_file: TestFile = toml::from_str(toml_content).unwrap();
        let tc = &test_file.test[0];
        assert_eq!(tc.name, "full_test");
        assert_eq!(tc.input, "full input");
        assert_eq!(tc.expect_contains.as_deref(), Some("expected"));
        assert_eq!(tc.expect_tool_call.as_deref(), Some("bash"));
        assert_eq!(tc.max_tokens, Some(1000));
    }

    #[test]
    fn test_case_minimal_from_toml() {
        let toml_content = r#"
[[test]]
name = "min"
input = "hello"
expect_contains = "world"
"#;
        let test_file: TestFile = toml::from_str(toml_content).unwrap();
        let tc = &test_file.test[0];
        assert!(tc.expect_tool_call.is_none());
        assert!(tc.max_tokens.is_none());
    }

    #[test]
    fn test_file_multiple_cases() {
        let toml_content = r#"
[[test]]
name = "case1"
input = "a"
expect_contains = "b"

[[test]]
name = "case2"
input = "c"
expect_tool_call = "read_file"

[[test]]
name = "case3"
input = "d"
expect_contains = "e"
expect_tool_call = "bash"
max_tokens = 500
"#;
        let test_file: TestFile = toml::from_str(toml_content).unwrap();
        assert_eq!(test_file.test.len(), 3);
    }

    // ─── validate_test_case edge cases ────────────────────────────────────

    #[test]
    fn validate_test_case_whitespace_name_passes() {
        // A whitespace-only name is technically non-empty
        let tc = TestCase {
            name: " ".to_string(),
            input: "hello".to_string(),
            expect_contains: Some("world".to_string()),
            expect_tool_call: None,
            max_tokens: None,
        };
        assert!(validate_test_case(&tc));
    }

    // ─── truncate_str edge cases ──────────────────────────────────────────

    #[test]
    fn truncate_str_one_char_max() {
        assert_eq!(truncate_str("hello", 1), "h...");
    }

    #[test]
    fn truncate_str_unicode() {
        assert_eq!(truncate_str("abcde", 3), "abc...");
        // The cut lands inside a multi-byte character, where a byte-indexed
        // slice panics ("byte index N is not a char boundary") on the
        // assertion-failure path, which prints raw model output. '🎉' occupies
        // bytes 3..7.
        assert_eq!(truncate_str("abc🎉def", 4), "abc...");
        assert_eq!(truncate_str("abc🎉def", 6), "abc...");
        // A boundary-aligned cut is unaffected.
        assert_eq!(truncate_str("abc🎉def", 7), "abc🎉...");
        // Every character straddles the cut - the preview degrades to the marker
        // rather than panicking.
        assert_eq!(truncate_str("🎉🎉", 2), "...");
    }

    // ─── dry_run with filter ──────────────────────────────────────────────

    #[tokio::test]
    async fn dry_run_with_filter_matches() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        let test_toml = r#"
[[test]]
name = "alpha_test"
input = "hello"
expect_contains = "world"

[[test]]
name = "beta_test"
input = "hello"
expect_contains = "world"
"#;
        std::fs::write(tests_dir.join("basic.toml"), test_toml).unwrap();

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: Some("alpha".to_string()),
            dry_run: true,
        };
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dry_run_failing_test_case() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        // No assertions = fails validation
        let test_toml = r#"
[[test]]
name = "bad_test"
input = "hello"
"#;
        std::fs::write(tests_dir.join("fail.toml"), test_toml).unwrap();

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: true,
        };
        let result = execute(args).await;
        assert!(result.is_err()); // Should report failures
    }

    // ─── validate_test_case more cases ───────────────────────────────────

    #[test]
    fn validate_test_case_with_max_tokens_only_and_no_assertion_fails() {
        let tc = TestCase {
            name: "has-max-tokens".to_string(),
            input: "test".to_string(),
            expect_contains: None,
            expect_tool_call: None,
            max_tokens: Some(500),
        };
        assert!(!validate_test_case(&tc));
    }

    #[test]
    fn validate_test_case_with_only_tool_call_assertion() {
        let tc = TestCase {
            name: "tool-only".to_string(),
            input: "do it".to_string(),
            expect_contains: None,
            expect_tool_call: Some("write_file".to_string()),
            max_tokens: None,
        };
        assert!(validate_test_case(&tc));
    }

    // ─── truncate_str additional ─────────────────────────────────────────

    #[test]
    fn truncate_str_zero_max() {
        assert_eq!(truncate_str("hello", 0), "...");
    }

    #[test]
    fn truncate_str_large_max() {
        let s = "short";
        assert_eq!(truncate_str(s, 1000), "short");
    }

    // ─── TestFile TOML parsing edge cases ────────────────────────────────

    #[test]
    fn parse_test_file_empty_tests_array() {
        let toml_content = r#"
test = []
"#;
        let test_file: TestFile = toml::from_str(toml_content).unwrap();
        assert!(test_file.test.is_empty());
    }

    #[test]
    fn parse_test_file_missing_test_key_errors() {
        let result: Result<TestFile, _> = toml::from_str("something_else = 42");
        assert!(result.is_err());
    }

    // ─── dry_run with no matching filter ─────────────────────────────────

    #[tokio::test]
    async fn dry_run_with_filter_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        let test_toml = r#"
[[test]]
name = "alpha_test"
input = "hello"
expect_contains = "world"
"#;
        std::fs::write(tests_dir.join("basic.toml"), test_toml).unwrap();

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: Some("nonexistent_filter".to_string()),
            dry_run: true,
        };
        // All tests filtered out = 0 total, no failures
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    // ─── Rhai script tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn dry_run_with_rhai_script_passing() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();

        // Write a Rhai script that returns true (passes)
        std::fs::write(tests_dir.join("pass_test.rhai"), "true").unwrap();

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: true,
        };
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dry_run_with_rhai_script_returning_false() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();

        // Write a Rhai script that returns false (fails)
        std::fs::write(tests_dir.join("fail_test.rhai"), "false").unwrap();

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: true,
        };
        let result = execute(args).await;
        assert!(result.is_err()); // Should report test failure
    }

    #[tokio::test]
    async fn dry_run_with_rhai_script_error() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();

        // Write a Rhai script that throws an error
        std::fs::write(
            tests_dir.join("error_test.rhai"),
            "throw \"intentional error\"",
        )
        .unwrap();

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: true,
        };
        let result = execute(args).await;
        assert!(result.is_err()); // Should report script error as failure
    }

    #[tokio::test]
    async fn dry_run_with_rhai_non_bool_result_passes() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();

        // Write a Rhai script that returns a non-bool (treated as pass)
        std::fs::write(tests_dir.join("nonbool_test.rhai"), "42").unwrap();

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: true,
        };
        let result = execute(args).await;
        assert!(result.is_ok()); // Non-bool return treated as pass
    }

    #[tokio::test]
    async fn dry_run_with_rhai_filter_matches() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();

        // A rhai script whose name won't match the filter
        std::fs::write(tests_dir.join("fail_test.rhai"), "false").unwrap();
        // A rhai script that passes and matches the filter
        std::fs::write(tests_dir.join("good_test.rhai"), "true").unwrap();

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: Some("good".to_string()),
            dry_run: true,
        };
        let result = execute(args).await;
        assert!(result.is_ok()); // Only "good_test.rhai" runs, which passes
    }

    // ─── dry_run with multiple test files ────────────────────────────────

    #[tokio::test]
    async fn dry_run_with_multiple_test_files() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();

        let test1 = r#"
[[test]]
name = "test_a"
input = "hello"
expect_contains = "world"
"#;
        let test2 = r#"
[[test]]
name = "test_b"
input = "foo"
expect_tool_call = "bar"
"#;
        std::fs::write(tests_dir.join("file1.toml"), test1).unwrap();
        std::fs::write(tests_dir.join("file2.toml"), test2).unwrap();

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: true,
        };
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    // ─── dry_run with invalid TOML file ──────────────────────────────────

    #[tokio::test]
    async fn dry_run_with_invalid_toml_file() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        std::fs::write(tests_dir.join("bad.toml"), "not valid {{{ toml").unwrap();

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: true,
        };
        let result = execute(args).await;
        assert!(result.is_err());
    }

    // ─── run_test_case: mock provider (no real network calls) ────────────
    //
    // `execute()`'s non-dry-run path calls `Config::load()`, which reads the
    // developer's real `~/.leviath/config.toml` (and env var fallbacks) --
    // there's no path-injection seam for it from this file, and adding one
    // would require touching `config.rs`, which is out of scope. Driving
    // `execute(dry_run: false)` in a test would risk registering a real
    // provider with a real API key and making a live network call, which is
    // exactly the kind of flakiness/cost we must not introduce. Instead, we
    // exercise `run_test_case` directly with an in-memory mock `Provider`,
    // which covers the same assertion/response-handling logic without any
    // I/O.

    use leviath_providers::{InferenceRequest, InferenceResponse, Provider, ToolCall};

    /// A mock provider that returns a fixed canned response, entirely in
    /// memory - no network calls, no subprocess spawning.
    struct MockProvider {
        content: String,
        tool_calls: Vec<ToolCall>,
    }

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        async fn infer(
            &self,
            _request: &InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            Ok(InferenceResponse {
                tool_calls: self.tool_calls.clone(),
                ..fixtures::inference_response(&self.content)
            })
        }

        async fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len()
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            8192
        }

        fn name(&self) -> &str {
            "mock"
        }

        fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    /// A mock provider that does NOT support temperature (the default caps have
    /// it `true`), so the `else { 0.0 }` branch of the temperature choice runs.
    struct NoTemperatureProvider;

    #[async_trait::async_trait]
    impl Provider for NoTemperatureProvider {
        async fn infer(
            &self,
            _request: &InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            Ok(fixtures::inference_response("cold hello"))
        }

        async fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len()
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            8192
        }

        fn name(&self) -> &str {
            "no-temperature"
        }

        fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities {
                supports_temperature: false,
                ..Default::default()
            }
        }
    }

    /// A mock provider that always returns an error.
    struct ErrorProvider;

    #[async_trait::async_trait]
    impl Provider for ErrorProvider {
        async fn infer(
            &self,
            _request: &InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            Err(leviath_providers::ProviderError::ApiError(
                "simulated inference error".to_string(),
            ))
        }

        async fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len()
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            8192
        }

        fn name(&self) -> &str {
            "error-provider"
        }

        fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    fn basic_blueprint() -> leviath_core::Blueprint {
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        parse_manifest(manifest).unwrap()
    }

    /// Blueprint with an explicit `tool_results` region, so the
    /// `if window.get_region("tool_results").is_none()` branch is NOT taken.
    fn blueprint_with_tool_results_region() -> leviath_core::Blueprint {
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }

[context.regions.tool_results]
kind = "temporary"
max_tokens = 5000
"#;
        parse_manifest(manifest).unwrap()
    }

    /// A provider that records the request it receives, so a test can assert
    /// what `lev test` actually assembled (e.g. a custom region's rendered
    /// output).
    struct RecordingProvider {
        seen: std::sync::Arc<std::sync::Mutex<Option<InferenceRequest>>>,
    }

    #[async_trait::async_trait]
    impl Provider for RecordingProvider {
        async fn infer(
            &self,
            request: &InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            *self.seen.lock().unwrap() = Some(request.clone());
            Ok(fixtures::inference_response("recorded"))
        }

        async fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len()
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            8192
        }

        fn name(&self) -> &str {
            "recording"
        }

        fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    /// `lev test` runs custom-region render hooks with the entry stage's real
    /// metadata - the preview a hook author iterates against.
    #[tokio::test]
    async fn run_test_case_renders_custom_region_through_its_script() {
        let manifest = r#"
[agent]
name = "custom-test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }

[context.regions.task]
kind = "pinned"
max_tokens = 4000

[context.regions.brain]
kind = "custom"
script = "hooks/brain.rhai"
max_tokens = 4000
"#;
        let blueprint = parse_manifest(manifest).unwrap();
        let scripts = std::collections::HashMap::from([(
            "hooks/brain.rhai".to_string(),
            std::sync::Arc::new(
                leviath_scripting::region_hook::compile(
                    "hooks/brain.rhai",
                    "fn render(ctx) { `<brain stage=${ctx.stage_name} model=${ctx.model}>` }",
                )
                .unwrap(),
            ),
        )]);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mut registry = ProviderRegistry::new();
        registry.register(
            "anthropic".to_string(),
            Arc::new(RecordingProvider { seen: seen.clone() }),
        );
        let tc = TestCase {
            name: "custom_render".to_string(),
            input: "hi".to_string(),
            expect_contains: Some("recorded".to_string()),
            expect_tool_call: None,
            max_tokens: None,
        };
        let passed = run_test_case(&blueprint, &registry, &tc, &scripts)
            .await
            .unwrap();
        assert!(passed);
        let request = seen.lock().unwrap().take().expect("provider saw a request");
        // Precompute the texts so the assert message costs no extra branch.
        let system_texts: Vec<&String> = request.system.iter().map(|b| &b.text).collect();
        let rendered = system_texts
            .iter()
            .any(|t| t.as_str() == "<brain stage=main model=claude-sonnet-4-6>");
        assert!(
            rendered,
            "custom region rendered with stage metadata; system blocks: {system_texts:?}"
        );

        // Exercise the recording provider's remaining trait surface directly.
        let provider = registry.get("anthropic").unwrap();
        assert_eq!(provider.count_tokens("abcd", "m").await, 4);
        assert_eq!(provider.max_context_tokens("m"), 8192);
        assert_eq!(provider.name(), "recording");
    }

    /// The custom-region resolve error path in `execute` (a declared script
    /// that doesn't exist fails before any provider setup, dry-run or not).
    #[tokio::test]
    async fn execute_fails_fast_on_a_broken_custom_region_script() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        std::fs::write(
            project.join("agent.leviath"),
            r#"
[agent]
name = "broken-custom"
version = "0.1.0"
description = "d"

[stages.main]
model = { provider = "anthropic", model = "m" }

[context.regions.brain]
kind = "custom"
script = "hooks/missing.rhai"
max_tokens = 4000
"#,
        )
        .unwrap();
        std::fs::create_dir(project.join("tests")).unwrap();
        std::fs::write(
            project.join("tests/basic.toml"),
            "[[test]]\nname = \"t\"\ninput = \"hi\"\n",
        )
        .unwrap();
        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: true,
        };
        let err = execute(args).await.unwrap_err().to_string();
        assert!(err.contains("region 'brain'"), "{err}");
        assert!(err.contains("hooks/missing.rhai"), "{err}");
    }

    // ── new coverage tests ────────────────────────────────────────────────────

    /// Covers the `map_err(|e| anyhow!("Inference failed: {}", e))` closure
    /// path at the `provider.infer(...)` call-site.
    #[tokio::test]
    async fn run_test_case_inference_error_propagates() {
        let blueprint = basic_blueprint();
        let mut registry = ProviderRegistry::new();
        registry.register("anthropic".to_string(), Arc::new(ErrorProvider));
        let tc = TestCase {
            name: "inference_error".to_string(),
            input: "hi".to_string(),
            expect_contains: Some("x".to_string()),
            expect_tool_call: None,
            max_tokens: None,
        };
        let result = run_test_case(&blueprint, &registry, &tc, &Default::default()).await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Inference failed"));
    }

    /// Covers the `ok_or(anyhow!("Blueprint has no stages"))` path.
    #[tokio::test]
    async fn run_test_case_blueprint_with_no_stages_errors() {
        use leviath_core::{Blueprint, layout::ContextLayout};
        let blueprint = Blueprint::new(
            "no-stages".to_string(),
            "test".to_string(),
            vec![],
            ContextLayout::new(vec![], 4096),
        );
        let registry = ProviderRegistry::new();
        let tc = TestCase {
            name: "no_stages".to_string(),
            input: "hi".to_string(),
            expect_contains: Some("x".to_string()),
            expect_tool_call: None,
            max_tokens: None,
        };
        let result = run_test_case(&blueprint, &registry, &tc, &Default::default()).await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Blueprint has no stages"));
    }

    /// A blueprint that already declares a `tool_results` region runs fine (the
    /// window builder leaves the existing region in place).
    #[tokio::test]
    async fn run_test_case_with_preexisting_tool_results_region() {
        let blueprint = blueprint_with_tool_results_region();
        let mut registry = ProviderRegistry::new();
        registry.register(
            "anthropic".to_string(),
            Arc::new(MockProvider {
                content: "hello world".to_string(),
                tool_calls: vec![],
            }),
        );
        let tc = TestCase {
            name: "has_tool_results_region".to_string(),
            input: "hi".to_string(),
            expect_contains: Some("world".to_string()),
            expect_tool_call: None,
            max_tokens: None,
        };
        let result = run_test_case(&blueprint, &registry, &tc, &Default::default()).await;
        assert!(result.unwrap());
    }

    /// Covers `fs::read_to_string(&manifest_path)?` failing by making
    /// `agent.leviath` a *directory*: `exists()` passes the guard but the read
    /// fails on every platform.
    #[tokio::test]
    async fn execute_with_registry_manifest_unreadable_errors() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        std::fs::create_dir_all(project.join("agent.leviath")).unwrap();
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: true,
        };
        let result = execute_with_registry(args, Box::new(build_registry_from_config)).await;
        assert!(result.is_err());
    }

    /// Covers `Config::load()?` (line 139) failing when the config file exists
    /// but contains invalid TOML.  Uses `isolate_config_path_for_test` so that
    /// we redirect `LEVIATH_CONFIG_PATH` to a temp file we control, avoiding
    /// any mutation of the user's real `~/.leviath/config.toml`.
    #[tokio::test]
    async fn execute_with_registry_config_load_fails_errors() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();

        // Redirect Config::load() to a file with invalid TOML.
        crate::config::with_isolated_config_path_async(
            "test-cmd-config-fail",
            |fake_dir| async move {
                let bad_config = fake_dir.join("config.toml");
                std::fs::write(&bad_config, "not valid toml {{{").unwrap();

                let args = TestArgs {
                    path: Some(project.to_str().unwrap().to_string()),
                    filter: None,
                    dry_run: false, // triggers Config::load()
                };
                let result =
                    execute_with_registry(args, Box::new(build_registry_from_config)).await;
                assert!(result.is_err());
            },
        )
        .await;
    }

    /// Covers the `parse_manifest(&manifest_content)?` error path
    /// in `execute_with_registry` (invalid TOML in agent.leviath).
    #[tokio::test]
    async fn execute_with_registry_manifest_invalid_toml_errors() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        std::fs::write(project.join("agent.leviath"), "not valid toml {{{").unwrap();
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: false,
        };
        let result =
            execute_with_registry(args, Box::new(mock_registry_builder("irrelevant", vec![])))
                .await;
        assert!(result.is_err());
    }

    /// Covers `fs::read_dir(&tests_dir)?` failing by making `tests` a *file*:
    /// `exists()` passes the guard but `read_dir` fails on every platform.
    #[tokio::test]
    async fn execute_with_registry_tests_dir_unreadable_errors() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);
        // `tests` is a file, not a directory.
        std::fs::write(project.join("tests"), "not a dir").unwrap();
        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: true,
        };
        let result = execute_with_registry(args, Box::new(build_registry_from_config)).await;
        assert!(result.is_err());
    }

    /// Covers `fs::read_to_string(&test_path)?` for a `.toml` entry by making
    /// it a *directory* (extension is still `toml`): `read_dir` yields it but
    /// the read fails on every platform.
    #[tokio::test]
    async fn execute_with_registry_toml_unreadable_errors() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        std::fs::create_dir_all(tests_dir.join("unreadable.toml")).unwrap();
        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: true,
        };
        let result = execute_with_registry(args, Box::new(build_registry_from_config)).await;
        assert!(result.is_err());
    }

    /// Covers `fs::read_to_string(&test_path)?` for a `.rhai` entry by making
    /// it a *directory* (extension is still `rhai`): `read_dir` yields it but
    /// the read fails on every platform.
    #[tokio::test]
    async fn execute_with_registry_rhai_unreadable_errors() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        std::fs::create_dir_all(tests_dir.join("unreadable.rhai")).unwrap();
        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: true,
        };
        let result = execute_with_registry(args, Box::new(build_registry_from_config)).await;
        assert!(result.is_err());
    }

    /// A blueprint with no Pinned region still runs: `init_window` simply skips
    /// seeding the task, and the inference proceeds.
    #[tokio::test]
    async fn run_test_case_with_no_pinned_region_still_runs() {
        use leviath_core::Blueprint;
        use leviath_core::layout::ContextLayout;
        let blueprint = Blueprint::new(
            "no-regions".to_string(),
            "test".to_string(),
            vec![leviath_core::Stage::new(
                "main".to_string(),
                leviath_core::blueprint::ModelConfig::new(
                    "anthropic".to_string(),
                    "claude-sonnet-4-6".to_string(),
                ),
            )],
            ContextLayout::new(vec![], 4096),
        );
        let mut registry = ProviderRegistry::new();
        registry.register(
            "anthropic".to_string(),
            Arc::new(MockProvider {
                content: "hello world".to_string(),
                tool_calls: vec![],
            }),
        );
        let tc = TestCase {
            name: "no_pinned".to_string(),
            input: "hi".to_string(),
            expect_contains: Some("world".to_string()),
            expect_tool_call: None,
            max_tokens: None,
        };
        assert!(
            run_test_case(&blueprint, &registry, &tc, &Default::default())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn run_test_case_passes_with_expect_contains() {
        let blueprint = basic_blueprint();
        let mut registry = ProviderRegistry::new();
        registry.register(
            "anthropic".to_string(),
            Arc::new(MockProvider {
                content: "Hello, world!".to_string(),
                tool_calls: vec![],
            }),
        );

        let tc = TestCase {
            name: "greeting".to_string(),
            input: "say hello".to_string(),
            expect_contains: Some("world".to_string()),
            expect_tool_call: None,
            max_tokens: None,
        };

        let result = run_test_case(&blueprint, &registry, &tc, &Default::default()).await;
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn run_test_case_fails_expect_contains_mismatch() {
        let blueprint = basic_blueprint();
        let mut registry = ProviderRegistry::new();
        registry.register(
            "anthropic".to_string(),
            Arc::new(MockProvider {
                content: "Goodbye".to_string(),
                tool_calls: vec![],
            }),
        );

        let tc = TestCase {
            name: "greeting".to_string(),
            input: "say hello".to_string(),
            expect_contains: Some("world".to_string()),
            expect_tool_call: None,
            max_tokens: None,
        };

        let result = run_test_case(&blueprint, &registry, &tc, &Default::default()).await;
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn run_test_case_passes_with_expect_tool_call() {
        let blueprint = basic_blueprint();
        let mut registry = ProviderRegistry::new();
        registry.register(
            "anthropic".to_string(),
            Arc::new(MockProvider {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                }],
            }),
        );

        let tc = TestCase {
            name: "tool_test".to_string(),
            input: "run a command".to_string(),
            expect_contains: None,
            expect_tool_call: Some("bash".to_string()),
            max_tokens: None,
        };

        let result = run_test_case(&blueprint, &registry, &tc, &Default::default()).await;
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn run_test_case_fails_expect_tool_call_missing() {
        let blueprint = basic_blueprint();
        let mut registry = ProviderRegistry::new();
        registry.register(
            "anthropic".to_string(),
            Arc::new(MockProvider {
                content: "no tools here".to_string(),
                // A non-matching (rather than empty) tool call list still
                // fails the "has_tool" check but also exercises the
                // subsequent `tool_names` diagnostic's `.map()` closure,
                // which an empty Vec's `.iter().map(...)` never invokes at
                // all.
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "write_file".to_string(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                }],
            }),
        );

        let tc = TestCase {
            name: "tool_test".to_string(),
            input: "run a command".to_string(),
            expect_contains: None,
            expect_tool_call: Some("bash".to_string()),
            max_tokens: None,
        };

        let result = run_test_case(&blueprint, &registry, &tc, &Default::default()).await;
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn run_test_case_fails_both_assertions() {
        let blueprint = basic_blueprint();
        let mut registry = ProviderRegistry::new();
        registry.register(
            "anthropic".to_string(),
            Arc::new(MockProvider {
                content: "unrelated content".to_string(),
                tool_calls: vec![],
            }),
        );

        let tc = TestCase {
            name: "both".to_string(),
            input: "do stuff".to_string(),
            expect_contains: Some("expected".to_string()),
            expect_tool_call: Some("write_file".to_string()),
            max_tokens: None,
        };

        let result = run_test_case(&blueprint, &registry, &tc, &Default::default()).await;
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn run_test_case_no_assertions_always_passes() {
        let blueprint = basic_blueprint();
        let mut registry = ProviderRegistry::new();
        registry.register(
            "anthropic".to_string(),
            Arc::new(MockProvider {
                content: "anything".to_string(),
                tool_calls: vec![],
            }),
        );

        let tc = TestCase {
            name: "no_assertions".to_string(),
            input: "hi".to_string(),
            expect_contains: None,
            expect_tool_call: None,
            max_tokens: None,
        };

        let result = run_test_case(&blueprint, &registry, &tc, &Default::default()).await;
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn run_test_case_provider_not_registered_errors() {
        let blueprint = basic_blueprint();
        let registry = ProviderRegistry::new(); // empty -- "anthropic" not registered

        let tc = TestCase {
            name: "no_provider".to_string(),
            input: "hi".to_string(),
            expect_contains: Some("x".to_string()),
            expect_tool_call: None,
            max_tokens: None,
        };

        let result = run_test_case(&blueprint, &registry, &tc, &Default::default()).await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not configured"));
    }

    #[tokio::test]
    async fn no_temperature_provider_metadata_is_exercised() {
        let p = NoTemperatureProvider;
        assert_eq!(p.name(), "no-temperature");
        assert_eq!(p.count_tokens("abcd", "m").await, 4);
        assert_eq!(p.max_context_tokens("m"), 8192);
    }

    #[tokio::test]
    async fn run_test_case_omits_temperature_when_provider_lacks_it() {
        let blueprint = basic_blueprint();
        let mut registry = ProviderRegistry::new();
        registry.register("anthropic".to_string(), Arc::new(NoTemperatureProvider));
        let tc = TestCase {
            name: "no_temp".to_string(),
            input: "hi".to_string(),
            expect_contains: Some("cold".to_string()),
            expect_tool_call: None,
            max_tokens: None,
        };
        assert!(
            run_test_case(&blueprint, &registry, &tc, &Default::default())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn run_test_case_long_input_runs() {
        // A long input still seeds cleanly into the pinned task region.
        let blueprint = basic_blueprint();
        let mut registry = ProviderRegistry::new();
        registry.register(
            "anthropic".to_string(),
            Arc::new(MockProvider {
                content: "response text mentioning keyword".to_string(),
                tool_calls: vec![],
            }),
        );

        let tc = TestCase {
            name: "long_input".to_string(),
            input: "x".repeat(500),
            expect_contains: Some("keyword".to_string()),
            expect_tool_call: None,
            max_tokens: None,
        };

        let result = run_test_case(&blueprint, &registry, &tc, &Default::default()).await;
        assert!(result.unwrap());
    }

    // ─── execute_with_registry: non-dry-run path (mock provider) ────────────
    //
    // `execute()`'s non-dry-run path still calls the real `Config::load()`
    // (no path-injection seam for that without touching config.rs, out of
    // scope here). `execute_with_registry` takes the registry-building step
    // as a parameter, so we can hand it a registry built entirely from an
    // in-memory `MockProvider` and don't care what the config *contains* --
    // no network calls, no real API keys read. But `Config::load()?` still
    // propagates a hard error via `?` if it fails, which is *not* irrelevant:
    // every test below that reaches this line uses
    // `isolate_config_path_for_test` to point `LEVIATH_CONFIG_PATH` at a
    // guaranteed-absent path, so `Config::load()` deterministically falls
    // back to defaults instead of racing some *other*, concurrently-running
    // test's temporarily-malformed config file at the same process-global
    // env var (see `models.rs`'s own `isolate_config_path_for_test` users
    // for the other side of that race - without this, this whole group was
    // observed to fail intermittently, with a config-parse error instead of
    // the expected test-run outcome, when run alongside `commands::models`'s
    // test suite).

    fn mock_registry_builder(
        content: &'static str,
        tool_calls: Vec<ToolCall>,
    ) -> impl FnOnce(&Config) -> Result<ProviderRegistry, leviath_providers::ProviderError> {
        move |_config: &Config| {
            let mut reg = ProviderRegistry::new();
            reg.register(
                "anthropic".to_string(),
                Arc::new(MockProvider {
                    content: content.to_string(),
                    tool_calls,
                }),
            );
            Ok(reg)
        }
    }

    fn write_project_with_test_file(project: &std::path::Path, test_toml: &str) {
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        std::fs::write(tests_dir.join("basic.toml"), test_toml).unwrap();
    }

    #[tokio::test]
    async fn execute_with_registry_non_dry_run_all_pass() {
        crate::config::with_isolated_config_path_async(
            "test-rs-non-dry-run-all-pass",
            |_fake_dir| async move {
                let dir = tempfile::tempdir().unwrap();
                let project = dir.path();
                write_project_with_test_file(
                    project,
                    r#"
[[test]]
name = "greeting"
input = "say hello"
expect_contains = "world"
"#,
                );

                let args = TestArgs {
                    path: Some(project.to_str().unwrap().to_string()),
                    filter: None,
                    dry_run: false,
                };

                let result = with_tracing(|| {
                    execute_with_registry(
                        args,
                        Box::new(mock_registry_builder("Hello, world!", vec![])),
                    )
                })
                .await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_with_registry_none_path_defaults_to_current_dir() {
        // Covers the `unwrap_or_else(|| ".".to_string())` closure, never
        // invoked by any other test (all of which pass an explicit `path`).
        // `cargo test`'s cwd is this crate's own source directory, which
        // has no `agent.leviath`, so this deterministically hits the
        // "No agent.leviath found" bail - proving the closure ran without
        // depending on (or mutating) any real project directory.
        let args = TestArgs {
            path: None,
            filter: None,
            dry_run: true,
        };
        let result = execute_with_registry(args, Box::new(build_registry_from_config)).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No agent.leviath found")
        );
    }

    #[tokio::test]
    async fn execute_with_registry_non_dry_run_failure_bails_with_count() {
        crate::config::with_isolated_config_path_async(
            "test-rs-non-dry-run-failure-bails-with-count",
            |_fake_dir| async move {
                let dir = tempfile::tempdir().unwrap();
                let project = dir.path();
                write_project_with_test_file(
                    project,
                    r#"
[[test]]
name = "greeting"
input = "say hello"
expect_contains = "world"
"#,
                );

                let args = TestArgs {
                    path: Some(project.to_str().unwrap().to_string()),
                    filter: None,
                    dry_run: false,
                };

                let result =
                    execute_with_registry(args, Box::new(mock_registry_builder("goodbye", vec![])))
                        .await;
                let err = result.unwrap_err().to_string();
                assert!(err.contains("1 test(s) failed"));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_with_registry_non_dry_run_applies_filter() {
        crate::config::with_isolated_config_path_async(
            "test-rs-non-dry-run-applies-filter",
            |_fake_dir| async move {
                let dir = tempfile::tempdir().unwrap();
                let project = dir.path();
                write_project_with_test_file(
                    project,
                    r#"
[[test]]
name = "keep_me"
input = "say hello"
expect_contains = "world"

[[test]]
name = "skip_me"
input = "say hello"
expect_contains = "unmatchable content"
"#,
                );

                let args = TestArgs {
                    path: Some(project.to_str().unwrap().to_string()),
                    filter: Some("keep".to_string()),
                    dry_run: false,
                };

                // "skip_me" would fail (its expectation never matches the mock
                // response), but the filter excludes it - only "keep_me" runs, and
                // it passes, so the whole run succeeds.
                let result = execute_with_registry(
                    args,
                    Box::new(mock_registry_builder("Hello, world!", vec![])),
                )
                .await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_with_registry_non_dry_run_tool_call_assertion() {
        crate::config::with_isolated_config_path_async(
            "test-rs-non-dry-run-tool-call-assertion",
            |_fake_dir| async move {
                let dir = tempfile::tempdir().unwrap();
                let project = dir.path();
                write_project_with_test_file(
                    project,
                    r#"
[[test]]
name = "tool_test"
input = "run a command"
expect_tool_call = "bash"
"#,
                );

                let args = TestArgs {
                    path: Some(project.to_str().unwrap().to_string()),
                    filter: None,
                    dry_run: false,
                };

                let tool_calls = vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                }];
                let result =
                    execute_with_registry(args, Box::new(mock_registry_builder("", tool_calls)))
                        .await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_with_registry_non_dry_run_provider_error_counts_as_failure() {
        crate::config::with_isolated_config_path_async(
            "test-rs-non-dry-run-provider-error-counts-as-failure",
            |_fake_dir| async move {
                let dir = tempfile::tempdir().unwrap();
                let project = dir.path();
                write_project_with_test_file(
                    project,
                    r#"
[[test]]
name = "no_such_provider"
input = "hi"
expect_contains = "x"
"#,
                );
                // Overwrite the manifest with a provider name the mock registry never
                // registers, so `run_test_case`'s "not configured" error path fires
                // (the `Err(e)` arm of `execute`'s match, not `Ok(false)`).
                std::fs::write(
                    project.join("agent.leviath"),
                    r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "nonexistent-provider", model = "x" }
"#,
                )
                .unwrap();

                let args = TestArgs {
                    path: Some(project.to_str().unwrap().to_string()),
                    filter: None,
                    dry_run: false,
                };

                let result = execute_with_registry(
                    args,
                    Box::new(mock_registry_builder("irrelevant", vec![])),
                )
                .await;
                let err = result.unwrap_err().to_string();
                assert!(err.contains("1 test(s) failed"));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_with_registry_non_dry_run_toml_malformed_errors() {
        crate::config::with_isolated_config_path_async(
            "test-rs-non-dry-run-toml-malformed-errors",
            |_fake_dir| async move {
                let dir = tempfile::tempdir().unwrap();
                let project = dir.path();
                write_project_with_test_file(project, "not valid {{{ toml");

                let args = TestArgs {
                    path: Some(project.to_str().unwrap().to_string()),
                    filter: None,
                    dry_run: false,
                };

                let result = execute_with_registry(
                    args,
                    Box::new(mock_registry_builder("irrelevant", vec![])),
                )
                .await;
                assert!(result.is_err());
                assert!(result.unwrap_err().to_string().contains("Failed to parse"));
            },
        )
        .await;
    }

    // ─── rhai script execution path ──────────────────────────────────────────

    #[tokio::test]
    async fn execute_with_registry_rhai_script_passes() {
        crate::config::with_isolated_config_path_async(
            "test-rs-rhai-script-passes",
            |_fake_dir| async move {
                let dir = tempfile::tempdir().unwrap();
                let project = dir.path();
                let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
                write_test_agent(project, manifest);
                let tests_dir = project.join("tests");
                std::fs::create_dir_all(&tests_dir).unwrap();
                std::fs::write(tests_dir.join("script.rhai"), "true").unwrap();

                let args = TestArgs {
                    path: Some(project.to_str().unwrap().to_string()),
                    filter: None,
                    dry_run: false,
                };

                let result =
                    execute_with_registry(args, Box::new(mock_registry_builder("unused", vec![])))
                        .await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_with_registry_rhai_script_returns_false_fails() {
        crate::config::with_isolated_config_path_async(
            "test-rs-rhai-script-returns-false-fails",
            |_fake_dir| async move {
                let dir = tempfile::tempdir().unwrap();
                let project = dir.path();
                let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
                write_test_agent(project, manifest);
                let tests_dir = project.join("tests");
                std::fs::create_dir_all(&tests_dir).unwrap();
                std::fs::write(tests_dir.join("script.rhai"), "false").unwrap();

                let args = TestArgs {
                    path: Some(project.to_str().unwrap().to_string()),
                    filter: None,
                    dry_run: false,
                };

                let result =
                    execute_with_registry(args, Box::new(mock_registry_builder("unused", vec![])))
                        .await;
                let err = result.unwrap_err().to_string();
                assert!(err.contains("1 test(s) failed"));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_with_registry_rhai_script_error_fails() {
        crate::config::with_isolated_config_path_async(
            "test-rs-rhai-script-error-fails",
            |_fake_dir| async move {
                let dir = tempfile::tempdir().unwrap();
                let project = dir.path();
                let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
                write_test_agent(project, manifest);
                let tests_dir = project.join("tests");
                std::fs::create_dir_all(&tests_dir).unwrap();
                std::fs::write(tests_dir.join("script.rhai"), "this is not valid rhai (((")
                    .unwrap();

                let args = TestArgs {
                    path: Some(project.to_str().unwrap().to_string()),
                    filter: None,
                    dry_run: false,
                };

                let result =
                    execute_with_registry(args, Box::new(mock_registry_builder("unused", vec![])))
                        .await;
                assert!(result.is_err());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_with_registry_rhai_script_non_bool_return_passes() {
        crate::config::with_isolated_config_path_async(
            "test-rs-rhai-script-non-bool-return-passes",
            |_fake_dir| async move {
                let dir = tempfile::tempdir().unwrap();
                let project = dir.path();
                let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
                write_test_agent(project, manifest);
                let tests_dir = project.join("tests");
                std::fs::create_dir_all(&tests_dir).unwrap();
                // Returns an integer, not a bool - exercises the `else` arm of the
                // `result.as_bool()` match (treated as an automatic pass).
                std::fs::write(tests_dir.join("script.rhai"), "42").unwrap();

                let args = TestArgs {
                    path: Some(project.to_str().unwrap().to_string()),
                    filter: None,
                    dry_run: false,
                };

                let result =
                    execute_with_registry(args, Box::new(mock_registry_builder("unused", vec![])))
                        .await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_with_registry_rhai_script_filter_excludes_all() {
        crate::config::with_isolated_config_path_async(
            "test-rs-rhai-script-filter-excludes-all",
            |_fake_dir| async move {
                let dir = tempfile::tempdir().unwrap();
                let project = dir.path();
                let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
                write_test_agent(project, manifest);
                let tests_dir = project.join("tests");
                std::fs::create_dir_all(&tests_dir).unwrap();
                std::fs::write(tests_dir.join("script.rhai"), "false").unwrap();

                let args = TestArgs {
                    path: Some(project.to_str().unwrap().to_string()),
                    filter: Some("no-such-script".to_string()),
                    dry_run: false,
                };

                // Filter excludes the only script - 0 total, reports "no test files
                // found" and succeeds (rather than failing on the script's `false`).
                let result =
                    execute_with_registry(args, Box::new(mock_registry_builder("unused", vec![])))
                        .await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    // ─── build_registry_from_config ──────────────────────────────────────────
    //
    // The production registry builder passed to `execute_with_registry` by
    // `execute()`. `Provider::new`/`with_base_url` constructors just store
    // config - they don't make network calls - so this is safe to exercise
    // directly with fake keys, registering every provider branch.

    #[test]
    fn build_registry_from_config_registers_all_providers() {
        let config = Config {
            default_provider: "anthropic".to_string(),
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("fake-anthropic-key".to_string()),
                openai_api_key: Some("fake-openai-key".to_string()),
                google_api_key: Some("fake-google-key".to_string()),
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                anthropic_cache_ttl: None,
                fallback_order: Vec::new(),
                ..Default::default()
            },
            openrouter_api_key: Some("fake-openrouter-key".to_string()),
            ollama_base_url: Some("http://localhost:12345".to_string()),
            ..Config::default()
        };

        let registry =
            build_registry_from_config(&config).expect("an HTTPS client builds in tests");
        assert!(registry.has("anthropic"));
        assert!(registry.has("openai"));
        assert!(registry.has("google"));
        assert!(registry.has("openrouter"));
        assert!(registry.has("ollama"));
    }

    #[test]
    fn build_registry_from_config_no_keys_still_registers_ollama_with_default_url() {
        let config = Config::default();
        let registry =
            build_registry_from_config(&config).expect("an HTTPS client builds in tests");
        assert!(!registry.has("anthropic"));
        assert!(!registry.has("openai"));
        assert!(!registry.has("google"));
        assert!(!registry.has("openrouter"));
        // ollama has no key gate - always registered, with the default URL
        // when `ollama_base_url` is unset.
        assert!(registry.has("ollama"));
    }

    /// Covers the implicit `else` branch in the `if .toml / else if .rhai`
    /// check: a file in tests/ whose extension is neither is silently skipped.
    #[tokio::test]
    async fn execute_with_registry_ignores_non_test_files_in_tests_dir() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        write_test_agent(project, manifest);
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        // A .txt file - neither .toml nor .rhai - exercises the implicit else
        // path that simply skips unrecognized files.
        std::fs::write(tests_dir.join("readme.txt"), "this file should be ignored").unwrap();
        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: true,
        };
        let result = execute_with_registry(args, Box::new(build_registry_from_config)).await;
        assert!(result.is_ok());
    }

    // ─── ErrorProvider trivial trait methods ─────────────────────────────────

    #[tokio::test]
    async fn error_provider_trivial_trait_methods() {
        let provider = ErrorProvider;
        assert_eq!(provider.count_tokens("hello", "any-model").await, 5);
        assert_eq!(provider.max_context_tokens("any-model"), 8192);
        assert_eq!(provider.name(), "error-provider");
        let caps = provider.capabilities("any-model");
        let _ = caps; // just verify it doesn't panic
    }

    // ─── MockProvider trivial trait methods ──────────────────────────────────

    #[tokio::test]
    async fn mock_provider_trivial_trait_methods() {
        let provider = MockProvider {
            content: "x".to_string(),
            tool_calls: vec![],
        };
        assert_eq!(provider.count_tokens("hello", "any-model").await, 5);
        assert_eq!(provider.max_context_tokens("any-model"), 8192);
        assert_eq!(provider.name(), "mock");
    }

    #[test]
    fn a_registry_needs_an_https_client_it_can_build() {
        // `lev test` registers every configured provider against one client;
        // if that client cannot be built there is nothing to test against.
        let mut config = Config::default();
        config.providers.anthropic_api_key = Some("k".to_string());
        let err = build_registry_from_config_with(&config, &|_t| {
            Err(leviath_providers::provider::malformed_url_error())
        })
        .err()
        .expect("a failing client factory should fail the registry");
        assert!(err.to_string().contains("root certificate store"));
    }

    #[tokio::test]
    async fn a_real_run_stops_when_the_registry_will_not_build() {
        // Not a dry run, so the registry is built - and a machine that cannot
        // build an HTTPS client has nothing to run the cases against.
        crate::config::with_isolated_config_path_async(
            "test-a_real_run_stops_when_the_registry_will_not_build",
            |_fake_dir| async move {
                let dir = tempfile::tempdir().expect("tempdir");
                let project = dir.path();
                write_project_with_test_file(project, "[[test]]\nname = \"t\"\ninput = \"hi\"\n");
                let args = TestArgs {
                    path: Some(project.to_str().expect("utf-8 path").to_string()),
                    filter: None,
                    dry_run: false,
                };
                let failing: RegistryBuilder = Box::new(|_config: &Config| {
                    Err(leviath_providers::ProviderError::ClientBuild(
                        "no roots".to_string(),
                    ))
                });
                let err = execute_with_registry(args, failing)
                    .await
                    .expect_err("a failing registry builder should stop the run");
                assert!(err.to_string().contains("no roots"), "{err}");
            },
        )
        .await;
    }
}

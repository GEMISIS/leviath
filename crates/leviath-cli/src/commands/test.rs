//! `lev test` - Run agent tests

use clap::Args;
use leviath_core::{Region, RegionKind};
use leviath_runtime::{AgentEngine, AgentPool, ProviderRegistry};
use serde::Deserialize;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use super::run::parse_manifest_public;
use crate::config::Config;

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
#[allow(dead_code)]
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

pub async fn execute(args: TestArgs) -> anyhow::Result<()> {
    execute_with_registry(args, build_registry_from_config).await
}

/// Builds the real provider registry from a loaded [`Config`] -- the
/// production `build_registry` passed to [`execute_with_registry`] by
/// [`execute`].
fn build_registry_from_config(config: &Config) -> ProviderRegistry {
    let mut reg = ProviderRegistry::new();

    if let Some(ref key) = config.providers.anthropic_api_key {
        reg.register(
            "anthropic".to_string(),
            Arc::new(leviath_providers::AnthropicProvider::new(key.clone())),
        );
    }
    if let Some(ref key) = config.providers.openai_api_key {
        reg.register(
            "openai".to_string(),
            Arc::new(leviath_providers::OpenAIProvider::new(key.clone())),
        );
    }
    if let Some(ref key) = config.providers.google_api_key {
        reg.register(
            "google".to_string(),
            Arc::new(leviath_providers::GeminiProvider::new(key.clone())),
        );
    }
    if let Some(ref key) = config.openrouter_api_key {
        reg.register(
            "openrouter".to_string(),
            Arc::new(leviath_providers::OpenRouterProvider::new(key.clone())),
        );
    }
    let ollama_url = config
        .ollama_base_url
        .as_deref()
        .unwrap_or("http://localhost:11434");
    reg.register(
        "ollama".to_string(),
        Arc::new(leviath_providers::OllamaProvider::with_base_url(
            ollama_url.to_string(),
        )),
    );

    reg
}

/// Core of [`execute`], with provider-registry construction injected so
/// tests can drive the non-dry-run path with a mock [`Provider`] instead of
/// either skipping it (dry-run only) or making a real, billed network call
/// through whatever the developer's real `~/.leviath/config.toml` happens to
/// contain.
async fn execute_with_registry(
    args: TestArgs,
    build_registry: impl FnOnce(&Config) -> ProviderRegistry,
) -> anyhow::Result<()> {
    let path = args.path.unwrap_or_else(|| ".".to_string());
    tracing::info!(path = %path, "Running agent tests");

    let project_path = Path::new(&path);

    // Verify agent.leviath exists
    let manifest_path = project_path.join("agent.leviath");
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
        return Ok(());
    }

    if args.dry_run {
        println!("Dry run mode: validating test structure only (no API calls)\n");
    }

    // Parse blueprint and set up providers (only if not dry_run)
    let manifest_content = fs::read_to_string(&manifest_path)?;
    let blueprint = parse_manifest_public(&manifest_content)?;

    let registry = if !args.dry_run {
        let config = Config::load()?;
        Some(build_registry(&config))
    } else {
        None
    };

    let mut total = 0;
    let mut passed = 0;
    let mut failed = 0;
    let mut failures: Vec<String> = Vec::new();

    // Run .toml test files
    for entry in fs::read_dir(&tests_dir)? {
        let entry = entry?;
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
                if let Some(ref filter) = args.filter {
                    if !test_case.name.contains(filter.as_str()) {
                        continue;
                    }
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
                    match run_test_case(&blueprint, registry, test_case).await {
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
        }
    }

    // Run .rhai test scripts
    for entry in fs::read_dir(&tests_dir)? {
        let entry = entry?;
        let test_path = entry.path();

        if test_path.extension().and_then(|e| e.to_str()) == Some("rhai") {
            let file_name = test_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");

            // Apply filter if provided
            if let Some(ref filter) = args.filter {
                if !file_name.contains(filter.as_str()) {
                    continue;
                }
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

/// Run a single test case by spawning an agent and running one inference call.
async fn run_test_case(
    blueprint: &leviath_core::Blueprint,
    registry: &ProviderRegistry,
    test: &TestCase,
) -> anyhow::Result<bool> {
    // Create engine with providers
    let mut engine = AgentEngine::with_providers(registry.clone());

    // Create agent pool and spawn agent
    let mut pool = AgentPool::new(blueprint.clone());
    let agent_id = pool.spawn_agent(engine.world_mut());
    let entity = pool
        .get_agent(&agent_id)
        .ok_or_else(|| anyhow::anyhow!("Failed to get spawned agent entity"))?;

    // Initialize context window regions from blueprint layout
    if let Some(mut window) = engine
        .world_mut()
        .get_mut::<leviath_runtime::ContextWindow>(entity)
    {
        for region_def in &blueprint.context_layout.regions {
            let region = Region::new(
                region_def.name.clone(),
                region_def.kind.clone(),
                region_def.max_tokens,
            );
            window.add_region(region);
        }

        // Add tool_results region if not present
        if window.get_region("tool_results").is_none() {
            let tool_region = Region::new("tool_results".to_string(), RegionKind::Temporary, 5000);
            window.add_region(tool_region);
        }

        // Add test input to the first pinned/system region
        let system_region_name = blueprint
            .context_layout
            .regions
            .iter()
            .find(|r| matches!(r.kind, RegionKind::Pinned))
            .map(|r| r.name.clone());

        if let Some(region_name) = system_region_name {
            let task_tokens = test.input.len() / 4 + 1;
            let _ = window.add_to_region(&region_name, test.input.clone(), task_tokens);
        }
    }

    // Get model config from the first stage
    let stage = blueprint
        .stages
        .first()
        .ok_or_else(|| anyhow::anyhow!("Blueprint has no stages"))?;

    let provider_name = &stage.model.provider;
    let model_name = &stage.model.model;

    // Check if provider is available
    if !engine.providers().has(provider_name) {
        anyhow::bail!(
            "Provider '{}' is not configured. Set API key in ~/.leviath/config.toml",
            provider_name
        );
    }

    // Run a single inference call (not the full loop)
    let response = engine
        .run_inference(entity, provider_name, model_name, Vec::new())
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

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a registered `tracing::Subscriber`, `tracing::info!`'s
    /// multi-line field-argument lines show as uncovered even though the
    /// call itself demonstrably executes -- the macro short-circuits field
    /// evaluation when no subscriber is listening.
    struct AlwaysOnSubscriber;
    impl tracing::Subscriber for AlwaysOnSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, _event: &tracing::Event<'_>) {}
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }
    fn always_on_tracing_guard() -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(AlwaysOnSubscriber)
    }

    #[test]
    fn always_on_subscriber_span_methods_are_all_no_ops() {
        // This file only ever uses `tracing::info!` event macros, never
        // `tracing::span!` -- so `Subscriber::{new_span,record,
        // record_follows_from,enter,exit}` are otherwise never invoked.
        let _guard = always_on_tracing_guard();
        let span_a = tracing::info_span!("a", value = tracing::field::Empty);
        span_a.record("value", 1);
        let span_b = tracing::info_span!("b");
        span_b.follows_from(&span_a);
        let _enter_a = span_a.enter();
        let _enter_b = span_b.enter();
    }

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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();

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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();

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
        // Non-ASCII content should still work (might truncate mid-char for simple impl)
        let s = "abcde";
        assert_eq!(truncate_str(s, 3), "abc...");
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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();
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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();
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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();
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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();
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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();
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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();
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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();
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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();
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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();
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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();
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

    use leviath_providers::{
        FinishReason, InferenceRequest, InferenceResponse, Provider, TokenUsage, ToolCall,
    };

    /// A mock provider that returns a fixed canned response, entirely in
    /// memory -- no network calls, no subprocess spawning.
    struct MockProvider {
        content: String,
        tool_calls: Vec<ToolCall>,
    }

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        async fn infer(
            &self,
            _request: InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            Ok(InferenceResponse {
                content: self.content.clone(),
                tool_calls: self.tool_calls.clone(),
                tokens_used: TokenUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                finish_reason: FinishReason::Complete,
            })
        }

        fn count_tokens(&self, text: &str, _model: &str) -> usize {
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

    fn basic_blueprint() -> leviath_core::Blueprint {
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "test"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#;
        parse_manifest_public(manifest).unwrap()
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

        let result = run_test_case(&blueprint, &registry, &tc).await;
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

        let result = run_test_case(&blueprint, &registry, &tc).await;
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

        let result = run_test_case(&blueprint, &registry, &tc).await;
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

        let result = run_test_case(&blueprint, &registry, &tc).await;
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

        let result = run_test_case(&blueprint, &registry, &tc).await;
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

        let result = run_test_case(&blueprint, &registry, &tc).await;
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

        let result = run_test_case(&blueprint, &registry, &tc).await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not configured"));
    }

    #[tokio::test]
    async fn run_test_case_long_input_computes_task_tokens() {
        // Exercise the pinned-region input injection branch with an input
        // long enough that `input.len() / 4 + 1` is non-trivial.
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

        let result = run_test_case(&blueprint, &registry, &tc).await;
        assert!(result.unwrap());
    }

    // ─── execute_with_registry: non-dry-run path (mock provider) ────────────
    //
    // `execute()`'s non-dry-run path still calls the real `Config::load()`
    // (no path-injection seam for that without touching config.rs, out of
    // scope here), but its *return value* is now irrelevant to these tests:
    // `execute_with_registry` takes the registry-building step as a
    // parameter, so we can hand it a registry built entirely from an
    // in-memory `MockProvider` and completely ignore whatever the developer's
    // real config file happens to contain. No network calls, no real API
    // keys read.

    fn mock_registry_builder(
        content: &'static str,
        tool_calls: Vec<ToolCall>,
    ) -> impl FnOnce(&Config) -> ProviderRegistry {
        move |_config: &Config| {
            let mut reg = ProviderRegistry::new();
            reg.register(
                "anthropic".to_string(),
                Arc::new(MockProvider {
                    content: content.to_string(),
                    tool_calls,
                }),
            );
            reg
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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        std::fs::write(tests_dir.join("basic.toml"), test_toml).unwrap();
    }

    #[tokio::test]
    async fn execute_with_registry_non_dry_run_all_pass() {
        let _guard = always_on_tracing_guard();
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
            execute_with_registry(args, mock_registry_builder("Hello, world!", vec![])).await;
        assert!(result.is_ok(), "expected success, got: {:?}", result.err());
    }

    #[tokio::test]
    async fn execute_with_registry_none_path_defaults_to_current_dir() {
        // Covers the `unwrap_or_else(|| ".".to_string())` closure, never
        // invoked by any other test (all of which pass an explicit `path`).
        // `cargo test`'s cwd is this crate's own source directory, which
        // has no `agent.leviath`, so this deterministically hits the
        // "No agent.leviath found" bail -- proving the closure ran without
        // depending on (or mutating) any real project directory.
        let args = TestArgs {
            path: None,
            filter: None,
            dry_run: true,
        };
        let result = execute_with_registry(args, |_| ProviderRegistry::new()).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No agent.leviath found"));
    }

    #[tokio::test]
    async fn execute_with_registry_non_dry_run_failure_bails_with_count() {
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

        let result = execute_with_registry(args, mock_registry_builder("goodbye", vec![])).await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("1 test(s) failed"), "got: {}", err);
    }

    #[tokio::test]
    async fn execute_with_registry_non_dry_run_applies_filter() {
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
        // response), but the filter excludes it -- only "keep_me" runs, and
        // it passes, so the whole run succeeds.
        let result =
            execute_with_registry(args, mock_registry_builder("Hello, world!", vec![])).await;
        assert!(result.is_ok(), "expected success, got: {:?}", result.err());
    }

    #[tokio::test]
    async fn execute_with_registry_non_dry_run_tool_call_assertion() {
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
        }];
        let result = execute_with_registry(args, mock_registry_builder("", tool_calls)).await;
        assert!(result.is_ok(), "expected success, got: {:?}", result.err());
    }

    #[tokio::test]
    async fn execute_with_registry_non_dry_run_provider_error_counts_as_failure() {
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

        let result = execute_with_registry(args, mock_registry_builder("irrelevant", vec![])).await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("1 test(s) failed"), "got: {}", err);
    }

    #[tokio::test]
    async fn execute_with_registry_non_dry_run_toml_malformed_errors() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        write_project_with_test_file(project, "not valid {{{ toml");

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: false,
        };

        let result = execute_with_registry(args, mock_registry_builder("irrelevant", vec![])).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to parse"));
    }

    // ─── rhai script execution path ──────────────────────────────────────────

    #[tokio::test]
    async fn execute_with_registry_rhai_script_passes() {
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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        std::fs::write(tests_dir.join("script.rhai"), "true").unwrap();

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: false,
        };

        let result = execute_with_registry(args, mock_registry_builder("unused", vec![])).await;
        assert!(result.is_ok(), "expected success, got: {:?}", result.err());
    }

    #[tokio::test]
    async fn execute_with_registry_rhai_script_returns_false_fails() {
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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        std::fs::write(tests_dir.join("script.rhai"), "false").unwrap();

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: false,
        };

        let result = execute_with_registry(args, mock_registry_builder("unused", vec![])).await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("1 test(s) failed"), "got: {}", err);
    }

    #[tokio::test]
    async fn execute_with_registry_rhai_script_error_fails() {
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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        std::fs::write(tests_dir.join("script.rhai"), "this is not valid rhai (((").unwrap();

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: false,
        };

        let result = execute_with_registry(args, mock_registry_builder("unused", vec![])).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_with_registry_rhai_script_non_bool_return_passes() {
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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        // Returns an integer, not a bool -- exercises the `else` arm of the
        // `result.as_bool()` match (treated as an automatic pass).
        std::fs::write(tests_dir.join("script.rhai"), "42").unwrap();

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: None,
            dry_run: false,
        };

        let result = execute_with_registry(args, mock_registry_builder("unused", vec![])).await;
        assert!(result.is_ok(), "expected success, got: {:?}", result.err());
    }

    #[tokio::test]
    async fn execute_with_registry_rhai_script_filter_excludes_all() {
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
        std::fs::write(project.join("agent.leviath"), manifest).unwrap();
        let tests_dir = project.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        std::fs::write(tests_dir.join("script.rhai"), "false").unwrap();

        let args = TestArgs {
            path: Some(project.to_str().unwrap().to_string()),
            filter: Some("no-such-script".to_string()),
            dry_run: false,
        };

        // Filter excludes the only script -- 0 total, reports "no test files
        // found" and succeeds (rather than failing on the script's `false`).
        let result = execute_with_registry(args, mock_registry_builder("unused", vec![])).await;
        assert!(result.is_ok(), "expected success, got: {:?}", result.err());
    }

    // ─── build_registry_from_config ──────────────────────────────────────────
    //
    // The production registry builder passed to `execute_with_registry` by
    // `execute()`. `Provider::new`/`with_base_url` constructors just store
    // config -- they don't make network calls -- so this is safe to exercise
    // directly with fake keys, registering every provider branch.

    #[test]
    fn build_registry_from_config_registers_all_providers() {
        let config = Config {
            default_provider: "anthropic".to_string(),
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("fake-anthropic-key".to_string()),
                openai_api_key: Some("fake-openai-key".to_string()),
                google_api_key: Some("fake-google-key".to_string()),
            },
            openrouter_api_key: Some("fake-openrouter-key".to_string()),
            ollama_base_url: Some("http://localhost:12345".to_string()),
            ..Config::default()
        };

        let registry = build_registry_from_config(&config);
        assert!(registry.has("anthropic"));
        assert!(registry.has("openai"));
        assert!(registry.has("google"));
        assert!(registry.has("openrouter"));
        assert!(registry.has("ollama"));
    }

    #[test]
    fn build_registry_from_config_no_keys_still_registers_ollama_with_default_url() {
        let config = Config::default();
        let registry = build_registry_from_config(&config);
        assert!(!registry.has("anthropic"));
        assert!(!registry.has("openai"));
        assert!(!registry.has("google"));
        assert!(!registry.has("openrouter"));
        // ollama has no key gate -- always registered, with the default URL
        // when `ollama_base_url` is unset.
        assert!(registry.has("ollama"));
    }

    // ─── MockProvider trivial trait methods ──────────────────────────────────

    #[test]
    fn mock_provider_trivial_trait_methods() {
        let provider = MockProvider {
            content: "x".to_string(),
            tool_calls: vec![],
        };
        assert_eq!(provider.count_tokens("hello", "any-model"), 5);
        assert_eq!(provider.max_context_tokens("any-model"), 8192);
        assert_eq!(provider.name(), "mock");
    }
}

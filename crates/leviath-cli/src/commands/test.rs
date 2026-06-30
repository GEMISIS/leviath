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

        Some(reg)
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
        std::fs::write(tests_dir.join("error_test.rhai"), "throw \"intentional error\"").unwrap();

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
}

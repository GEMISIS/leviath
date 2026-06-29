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

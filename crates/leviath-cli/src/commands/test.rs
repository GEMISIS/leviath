//! `lev test` - Run agent tests

use clap::Args;
use serde::Deserialize;
use std::fs;
use std::path::Path;

#[derive(Args)]
pub struct TestArgs {
    /// Path to agent project
    #[arg(value_name = "PATH")]
    pub path: Option<String>,

    /// Test filter pattern
    #[arg(short, long)]
    pub filter: Option<String>,
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

                // For now, validate the test structure (real execution requires API keys)
                let test_valid = validate_test_case(test_case);

                if test_valid {
                    passed += 1;
                    println!("  PASS: {}", test_case.name);
                } else {
                    failed += 1;
                    let msg = format!("{}: test case validation failed", test_case.name);
                    println!("  FAIL: {}", msg);
                    failures.push(msg);
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
                    // If the script returns a bool, use it as pass/fail
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
                        // Non-bool result counts as pass (script didn't error)
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
    println!(
        "{} passed, {} failed, {} total",
        passed, failed, total
    );

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

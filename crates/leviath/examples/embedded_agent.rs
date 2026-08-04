//! A complete embedded agent: build a world, spawn one agent, stream its
//! events, and answer its questions from the terminal.
//!
//! Run with an Anthropic API key:
//!
//! ```sh
//! ANTHROPIC_API_KEY=sk-... cargo run --example embedded_agent -p leviath
//! ```
//!
//! The agent works in the current directory with the read-only built-in
//! tools, so it is safe to point at any checkout.

use leviath::prelude::*;

/// A self-contained blueprint: explore the workdir, ask one clarifying
/// question if needed, and report. No manifest file required.
const BLUEPRINT: &str = r#"[agent]
name = "explorer"
version = "0.1.0"
description = "Looks around the working directory and reports what it finds."
entry_stage = "explore"

[stages.explore]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Explore and summarize"
available_tools = ["read_file", "list_dir", "ask_user_text"]
allow_complete = true
system_prompt = """
Look at the files in the working directory and produce a short summary of
what this project is. Use list_dir and read_file. If something important is
ambiguous, ask the user one question with ask_user_text. Finish with a
plain-text summary.
"""

[context.regions]
task = { kind = "pinned", max_tokens = 2000, seed = "task_input" }
conversation = { kind = "sliding_window", max_items = 60, max_tokens = 60000 }
"#;

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") else {
        eprintln!("Set ANTHROPIC_API_KEY to run this example:");
        eprintln!("  ANTHROPIC_API_KEY=sk-... cargo run --example embedded_agent -p leviath");
        return Ok(());
    };

    // A world with one provider, fully in memory (add .state_dir(dir) to
    // persist runs in the daemon's on-disk layout).
    let world = AgentWorld::builder()
        .provider(ProviderCreds {
            api_key: Some(api_key),
            ..ProviderCreds::simple("anthropic")
        })
        .build()?;

    let mut events = world.events();
    let run = world
        .spawn(SpawnSpec::new(
            BlueprintSource::Toml(BLUEPRINT.to_string()),
            "Tell me what this project is.",
            std::env::current_dir()?,
        ))
        .await?;
    println!("spawned {run}");

    while let Some(event) = events.next().await {
        match event {
            AgentEvent::StageTransition { from, to, .. } => {
                println!("stage: {from} -> {to}");
            }
            AgentEvent::ToolCallStarted { tool, .. } => {
                println!("tool: {tool}...");
            }
            AgentEvent::ToolCallFinished { tool, ok, .. } => {
                println!("tool: {tool} {}", if ok { "ok" } else { "failed" });
            }
            AgentEvent::Interaction { request, .. } => {
                // The agent asked something; answer from the terminal.
                println!("agent asks: {}", request.prompt);
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                world.answer(InteractionResponse::text(
                    request.id.clone(),
                    answer.trim().to_string(),
                ));
            }
            AgentEvent::Log { line, .. } => {
                println!("  {line}");
            }
            AgentEvent::Completed {
                run_id,
                status,
                final_output,
                ..
            } if run_id == run.as_ref() => {
                println!("finished: {status}");
                // The answer rides the event, so there is no second call to
                // make and no race with the persistence tick. An agent whose
                // blueprint never asks for one leaves this `None`.
                match final_output {
                    Some(output) => println!("\n--- final output ---\n{}", output.content),
                    None => println!("(this agent produced no final output)"),
                }
                break;
            }
            _ => {}
        }
    }

    world.shutdown().await;
    Ok(())
}

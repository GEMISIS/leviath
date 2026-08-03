---
title: Embedding
group: Reference
group_order: 3
order: 13
---

# Embedding Leviath in a Rust application

Leviath is also a library. The same runtime the `lev` daemon serves can run
inside your own process: add the `leviath` crate, build a world, spawn agents,
and consume their events as an async stream. No CLI, no daemon, no config
file, no socket.

```toml
[dependencies]
leviath = "0.1"
tokio = { version = "1", features = ["full"] }
```

## The shape of it

```rust
use leviath::prelude::*;

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let world = AgentWorld::builder()
        .provider(ProviderCreds {
            api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
            ..ProviderCreds::simple("anthropic")
        })
        .build()?;

    let mut events = world.events();
    let run = world
        .spawn(SpawnSpec::new(
            BlueprintSource::Path("coder.leviath".into()),
            "Build a CSV parser",
            std::env::current_dir()?,
        ))
        .await?;

    while let Some(event) = events.next().await {
        match event {
            AgentEvent::StageTransition { from, to, .. } => println!("{from} -> {to}"),
            AgentEvent::ToolCallFinished { tool, ok, .. } => println!("{tool}: ok={ok}"),
            AgentEvent::Completed { run_id, status, .. } if run_id == run.as_ref() => {
                println!("finished: {status}");
                break;
            }
            _ => {}
        }
    }
    world.shutdown().await;
    Ok(())
}
```

`AgentWorld::builder()` must run inside a Tokio runtime (or be handed one via
`.runtime(handle)`). The serve loop runs as a background task on that runtime;
`shutdown()` stops it and drains any pending persistence writes before
returning.

## Builder options

| Method | What it does |
| --- | --- |
| `provider(creds)` | Register a provider from credentials. Repeatable. `ProviderCreds::simple(name)` covers key-free providers like `ollama`. |
| `register_provider(name, arc)` | Register your own `Provider` implementation, including mocks for tests. Wins over a credentials entry with the same name. |
| `default_model(provider, model)` | The fallback when none of a stage's listed models has a registered provider. |
| `tool_service(arc)` | Replace the built-in tool service with your own (see below). |
| `state_dir(dir)` | Persist runs on disk in the daemon's layout (`dir/runs/<run_id>/`). Without it the world is fully in memory and never touches disk. |
| `inference_pool(config)` | Per-model inference concurrency limits. |
| `tool_concurrency(n)` | How many tool batches may execute at once (default 4). |
| `runtime(handle)` | Run on a specific Tokio runtime instead of the ambient one. |

Blueprints come from three places: `BlueprintSource::Path` (a `.leviath`
blueprint file), `BlueprintSource::Toml` (blueprint text in memory), or
`BlueprintSource::Inline` (a constructed `Blueprint` value). Region seeds of
kind `caller_input` fill from `SpawnSpec::regions` (and the task prompt fills
the `task` key), `literal` seeds resolve as written; the file, glob, rhai, and
command seed kinds are daemon behavior and only error when the region is
`required`.

## Events

Everything the world does streams through `world.events()`, an async stream of
`AgentEvent` (the same enum the daemon broadcasts to WebSocket clients).

| Event | When |
| --- | --- |
| `Spawned` | A run appeared in the world. |
| `Status` | Status, stage, iteration, or tool-call count changed. |
| `Tokens` / `Context` | Token totals or context-window usage changed. |
| `StageTransition` | The run moved from one stage to another. |
| `ToolCallStarted` / `ToolCallFinished` | A tool call entered the async lane / returned, paired by `call_id`. |
| `Interaction` | The agent asked something and is waiting on an answer. |
| `Log` | A readable output or operational log line. |
| `Completed` | The run reached a terminal status. |

The enum is non-exhaustive; keep a catch-all arm. Every variant carries the
run id (`event.run_id()`), so one stream serves any number of concurrent
agents. A consumer that falls behind the channel skips ahead rather than
erroring, and the stream ends after `shutdown()`.

## Answering an agent's questions

When a blueprint uses `ask_user_text`, `ask_user_choice`, `ask_user_confirm`,
`present_for_review`, or `edit_document`, the call parks on the interaction
hub and surfaces as an `Interaction` event carrying the request. Answer it and
the agent resumes:

```rust
AgentEvent::Interaction { request, .. } => {
    let reply = my_ui.ask(&request.prompt).await;
    world.answer(InteractionResponse::text(request.id.clone(), reply));
}
```

`world.pending_inputs()` lists everything currently waiting, if you would
rather poll than watch the stream.

## Controlling runs

`status`, `pause`, `resume`, `cancel`, and `send_message` all address a run by
its `RunId`. A completed run is unloaded from memory shortly after its
`Completed` event; with `state_dir` set, its snapshots stay on disk in the
same format `lev ps` and the dashboard read.

## What the built-in tool service covers

Embedded agents get the built-in tools (file reads and writes, directory
listing, shell) confined to the spawn's workdir, plus the interaction tools
routed through the hub. The daemon-only layers are deliberately absent: MCP
servers, Rhai script tools, sandboxes, taint gates, and tool-approval
prompts. The embedder is code, and code that wants richer behavior implements
the `ToolService` trait and passes it to `tool_service()`; the trait is one
method plus optional per-stage hooks.

## Stability layers

- `AgentWorld` and the other embed types are the stable surface.
- `WorldHost` and `PipelineWorld` are the semi-stable machinery underneath,
  for hosts that need their own assembly (custom spawners, hooks, manual tick
  control).
- The raw ECS behind `PipelineWorld::world_mut()` is the unstable escape
  hatch. It tracks the runtime's `bevy_ecs` version, re-exported as
  `leviath::runtime::ecs` so downstream code stays version-aligned.

The daemon's control-socket transport is compiled out of library builds by
default. If you are writing a client for a running `lev` daemon rather than
hosting agents yourself, enable the `control-socket` feature on the `leviath`
crate.

A complete runnable program lives at `crates/leviath/examples/embedded_agent.rs`
in the repository: `cargo run --example embedded_agent -p leviath`.

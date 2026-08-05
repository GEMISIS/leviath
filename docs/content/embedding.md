---
title: Embedding
description: Run the Leviath runtime inside your own Rust process with the leviath crate, with no CLI, daemon, or config file.
group: Reference
group_order: 3
order: 14
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
            AgentEvent::Completed {
                run_id,
                status,
                final_output,
                ..
            } if run_id == run.as_ref() => {
                println!("finished: {status}");
                if let Some(output) = final_output {
                    println!("{}", output.content);
                }
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

Blueprints come from three places: `BlueprintSource::Path` for a `.leviath` file,
`BlueprintSource::Toml` for blueprint text you already have in memory, and
`BlueprintSource::Inline` for a `Blueprint` value you built yourself.

Not every [seed kind](/docs/context) works when embedded, because some of them are daemon
behaviour:

| Seed kind | Embedded |
|---|---|
| `caller_input` | Filled from `SpawnSpec::regions`. The task prompt fills the `task` key |
| `literal` | Resolves as written |
| `file`, `glob`, `rhai`, `command` | Not run. They only produce an error when the region is `required` |

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
| `Completed` | The run reached a terminal status, with whatever it handed back. |

The enum is non-exhaustive; keep a catch-all arm. Every variant carries the
run id (`event.run_id()`), so one stream serves any number of concurrent
agents. A consumer that falls behind the channel skips ahead rather than
erroring, and the stream ends after `shutdown()`.

## Getting the answer back

`Completed` carries `final_output`: the answer the agent submitted, its format label, and the stage
that produced it. Reading it from the event avoids a second call and avoids racing the write to
disk.

`AgentWorld::result(&run_id)` asks for the same thing at any point while the run is loaded.

Ask for a shape when you spawn. The label reaches the model untouched, so your own house format
works with no support from this crate.

```rust
let spec = SpawnSpec::new(source, "audit the auth module", cwd)
    .output("a2ui", Some("One card per finding.".to_string()));
```

[Final outputs](/docs/outputs) covers the whole cascade, including schema validation.

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

## How much can break under you

The API comes in three layers, and how careful you need to be depends on which one you reach for:

| Layer | What it is | Stability |
|---|---|---|
| `AgentWorld` and the other embed types | The normal way in. Everything above uses it | Stable. Breaking changes get a major version |
| `WorldHost` and `PipelineWorld` | The machinery underneath, for hosts that assemble their own spawners, hooks, or tick loop | Semi-stable. May change between minor versions |
| `PipelineWorld::world_mut()` | The raw [ECS world](/docs/engine), for anything the layers above cannot express | Unstable. No guarantees at all |

Stay on the first row unless you have a reason not to.

If you do use `world_mut()`, note that it hands you `bevy_ecs` types directly, so your code is
coupled to whichever version Leviath uses. It is re-exported as `leviath::runtime::ecs` for exactly
that reason: import it from there and your types stay aligned with the runtime's.

The daemon's control-socket transport is compiled out of library builds by
default. If you are writing a client for a running `lev` daemon rather than
hosting agents yourself, enable the `control-socket` feature on the `leviath`
crate.

A complete runnable program lives at `crates/leviath/examples/embedded_agent.rs`
in the repository: `cargo run --example embedded_agent -p leviath`.

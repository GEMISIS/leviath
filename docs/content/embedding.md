---
title: Embedding
description: Run the Leviath runtime inside your own Rust process with the leviath crate, with no CLI, daemon, or config file.
group: Reference
group_order: 3
order: 16
---

# Embedding Leviath in a Rust application

Leviath is also a library. The same runtime the `lev` daemon serves can run
inside your own process: add the `leviath` crate, build a world, start runs,
and consume their events as an async stream. No CLI, no daemon, no config
file, no socket.

```toml
[dependencies]
leviath = "0.3"
tokio = { version = "1", features = ["full"] }
```

## The shape of it

```rust
use leviath::prelude::*;

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let coder = leviath::blueprint::load("coder/agent.toml".as_ref())?;
    let world = AgentWorld::builder()
        .provider(ProviderCreds {
            api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
            ..ProviderCreds::simple("anthropic")
        })
        .blueprint(coder)
        .workdir(std::env::current_dir()?)
        .build()?;

    // A run is asked for with the same typed request every front door takes:
    // a blueprint and its inputs here, or a whole graph of your own.
    let request = SpawnRequest::new(SpawnSource::Blueprint(BlueprintRef::parse("coder")?))
        .input("task", RawInput::Text("Build a CSV parser".into()));

    let mut events = world.events();
    let run = world.spawn(request).await?;

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
| `blueprint(loaded)` | Offer a blueprint to the world's requests, under its own name. Repeatable. `world.add_blueprint` does the same later. |
| `workdir(dir)` | The directory a run works in when its request names none. Without it, every request names one. |
| `register_provider(name, arc)` | Register your own `Provider` implementation, including mocks for tests. Wins over a credentials entry with the same name. |
| `default_provider(provider)` | The provider bare model names route to. Each stage keeps the model its blueprint names. |
| `override_model(provider, model)` | The embedded `override_model`. One model every stage that allows a user default starts on. |
| `fallback_model(model)` | The embedded `fallback_model`. A last model to try when a stage's own list is spent. |
| `fallback_route(provider, model)` | Where a run moves when its provider fails mid-run. |
| `prompt_hints(hints)` | Turn on the batch-tool and shell hints, which are off by default on the embed path. |
| `tool_service(arc)` | Replace the built-in tool service with your own (see below). |
| `state_dir(dir)` | Keep each run's [run file](/docs/run-file) on disk in the daemon's layout (`dir/runs/<run_id>/run.lvr`). Without it the world stays in memory. |
| `inference_pool(config)` | Per-model inference concurrency limits. |
| `tool_concurrency(n)` | How many tool batches may execute at once (default 4). |
| `runtime(handle)` | Run on a specific Tokio runtime instead of the ambient one. |

`override_model` sets the default provider as well. Its model goes ahead of what a stage's
blueprint names, on every stage that allows a user default. `fallback_model` names a model on the
default provider, tried after every model a stage names and never ahead of them. `fallback_route`
is how a single-model blueprint survives an outage.

## Asking for a run

Every run starts from a `SpawnRequest`, the same typed request the CLI, the HTTP API and the agent
tools take. [Starting a run](/docs/starting-a-run) describes every field. Its `source` says what
to run:

| Source | What it runs |
|---|---|
| `SpawnSource::Blueprint(BlueprintRef::parse("coder")?)` | A blueprint the world was given with `.blueprint(...)` or `add_blueprint` |
| `SpawnSource::Raw(Box::new(graph))` | A `RunGraph` you hold, such as one read from text |
| `SpawnSource::BlueprintFile(path)` | Refused. An embedded world reads no blueprint from a path |

`leviath::blueprint::load(path)` reads an `agent.toml`, or the directory holding one, and pins it
to the digest of the file's bytes. For blueprint text you already have in memory, read the graph
out of it and send that. This is how the repository's example does it:

```rust
let graph = leviath::blueprint::BlueprintFile::parse(BLUEPRINT)?.run_graph();
let mut request = SpawnRequest::new(SpawnSource::Raw(Box::new(graph))).input(
    "task",
    RawInput::Text("Tell me what this project is.".into()),
);
request.workdir = Some(std::env::current_dir()?);
```

`input(name, value)` gives one of the graph's declared [inputs](/docs/starting-a-run#inputs).
`RawInput` has `Text`, `Int`, `Float`, `Bool`, `List` and `Record`, and the declared type decides
how a value is read. Every field of the request is public, so set `workdir`, `model`, `output`,
`launch` and `attachments` directly.

A request that cannot run comes back from `spawn` as `SpawnIssues`: every problem at once, each
naming the place in the request it is about. `world.validate(request)` makes the same checks
without starting anything, and answers with a summary of the run it would start.

```rust
match world.validate(request.clone()).await {
    Ok(summary) => println!("would run {} on {:?}", summary.title, summary.stages),
    Err(issues) => {
        for issue in issues.iter() {
            println!("{}: {}", issue.path, issue.message);
        }
    }
}
```

Not every [seed](/docs/context) works when embedded, because some of them are daemon behaviour.
A `literal` seed resolves as written. A `files`, `glob`, `command`, `code` or `tools` seed is not
run: it is an issue when the region is `required`, and leaves the region empty otherwise. Pass that
content as an input instead.

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

`AgentWorld::result(&run_id)` asks for the same thing at any point while the run is loaded. Its
`artifacts` are the files the run produced, each with a path relative to the workdir, a mime
type, a size and the hash the run's blob store holds it under. `artifact_bytes(&run_id, &artifact)`
reads one of them back from that store, so an embedder never needs to know where the world keeps
its files. It answers `None` for a file the store does not hold, such as one too large to store or
an artifact recorded by path alone.

Files go in through the request's `attachments`. A `file` input names an attachment by its
`name`, and an attachment with a `region` goes straight into that region. The run's registry types
it unless it declares a `mime_type`. `send_message_with` sends a message with files, and an
`InteractionResponse::text` answer takes files through `with_parts`. A `@path` inside the text is
not resolved here. Attach the file and keep the name in the text, and the model reads the same name.

```rust
use leviath::runtime::spec::names::RegionName;
use leviath::runtime::spec::request::{Attachment, Bytes};

let mut request = SpawnRequest::new(source)
    .input("task", RawInput::Text("edit @hero.png so the arm is longer".into()));
request.attachments.push(Attachment {
    name: "hero.png".into(),
    mime_type: None,
    region: Some(RegionName::new("task")?),
    deliver: None,
    caption: None,
    data: Bytes(std::fs::read("hero.png")?),
});
```

Ask for a shape when you spawn. The label reaches the model untouched, so your own house format
works with no support from this crate.

```rust
use leviath::runtime::spec::graph::OutputDef;

request.output = Some(OutputDef {
    format: Some("a2ui".into()),
    instructions: Some("One card per finding.".into()),
    ..Default::default()
});
```

[Final outputs](/docs/outputs) covers the whole cascade, including schema validation.

## Answering an agent's questions

When a blueprint's stage uses `ask_user_text`, `ask_user_choice`, `ask_user_confirm`,
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
`Completed` event. With `state_dir` set, its run file stays on disk in the
same format `lev ps` and the dashboard read.

`world.inspect(&run)` reads a run's whole state: where it is, its context, what it is waiting on
and what it has spent. It reads live while the run is in the world, and from its run file once it
has left, when a `state_dir` is set. It answers `None` for a run the world does not know.

```rust
if let Some(state) = world.inspect(&run).await {
    println!("{:?} at step {} in {}", state.status, state.seq, state.cursor.stage);
}
```

The state is the same one `lev run show` prints. See [Inspecting a run](/docs/inspecting-a-run).

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

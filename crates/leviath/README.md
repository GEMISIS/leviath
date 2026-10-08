# leviath

Leviath is a structured agent runtime for LLMs: context memory laid out in
regions with token budgets, multi-stage workflows described by blueprints, and
an ECS-based execution engine.

This crate is the library entry point. It re-exports the whole runtime under
one namespace so an application only needs a single dependency:

```toml
[dependencies]
leviath = "0.1"
```

Every model provider is built in by default. To compile only the ones your
application uses, turn the defaults off and name them:

```toml
[dependencies]
leviath = { version = "0.6", default-features = false, features = ["anthropic"] }
```

The provider features are `anthropic`, `openai`, `openai-subscription`, `xai`,
`xai-subscription`, `google`, `openrouter`, `bedrock`, `meta`, `ollama`,
`meshy` and `rhai`; [Choosing providers](https://leviath.dev/docs/choosing-providers)
says what each one is.

Running an agent in-process takes a provider, a blueprint, and an event
loop. No daemon, no config file:

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

    let request = SpawnRequest::new(SpawnSource::Blueprint(BlueprintRef::parse("coder")?))
        .input("task", RawInput::Text("Build a CSV parser".into()));

    let mut events = world.events();
    let run = world.spawn(request).await?;

    while let Some(event) = events.next().await {
        match event {
            AgentEvent::StageTransition { from, to, .. } => println!("{from} -> {to}"),
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

A run is asked for with a `SpawnRequest`, the same typed request the CLI,
the HTTP API and the agent tools take. It names a blueprint the world was
given and fills the inputs that blueprint declares, or it carries a whole
graph of its own. A request that cannot run comes back as `SpawnIssues`,
every problem at once, each naming the place in the request it is about.
`world.validate(request)` makes the same checks without starting anything,
and `world.inspect(&run)` reads a run's whole state.

A full program that also answers the agent's questions lives in
`examples/embedded_agent.rs` (`cargo run --example embedded_agent -p
leviath`), and the embedding guide at
[leviath.dev/docs/embedding](https://leviath.dev/docs/embedding) walks
through the builder options, the event stream, and the tool-service seam.

The most-used types are one import away with `use leviath::prelude::*;`.

The modules map one-to-one onto the underlying crates: `leviath::core`,
`leviath::runtime`, `leviath::providers`, `leviath::tools`, `leviath::mcp`,
`leviath::scripting`, `leviath::telemetry`, `leviath::blueprint`,
`leviath::package`, and `leviath::agent_client`. If you only need one layer, you can depend on that
crate directly instead.

If you want the `lev` command-line tool rather than a library, install
[`leviath-cli`](https://crates.io/crates/leviath-cli).

Source, documentation, and issue tracker live at
[github.com/GEMISIS/leviath](https://github.com/GEMISIS/leviath).
See also [leviath.dev](https://leviath.dev).

Licensed under the MIT license.

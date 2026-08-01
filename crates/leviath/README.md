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

Running an agent in-process takes a provider, a blueprint, and an event
loop. No daemon, no config file:

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

A full program that also answers the agent's questions lives in
`examples/embedded_agent.rs` (`cargo run --example embedded_agent -p
leviath`), and the embedding guide at
[leviath.dev/docs/embedding](https://leviath.dev/docs/embedding) walks
through the builder options, the event stream, and the tool-service seam.

The most-used types are one import away with `use leviath::prelude::*;`.

The modules map one-to-one onto the underlying crates: `leviath::core`,
`leviath::runtime`, `leviath::providers`, `leviath::tools`, `leviath::mcp`,
`leviath::scripting`, `leviath::telemetry`, `leviath::package`, and
`leviath::agent_client`. If you only need one layer, you can depend on that
crate directly instead.

If you want the `lev` command-line tool rather than a library, install
[`leviath-cli`](https://crates.io/crates/leviath-cli).

Source, documentation, and issue tracker live at
[github.com/GEMISIS/leviath](https://github.com/GEMISIS/leviath).
See also [leviath.dev](https://leviath.dev).

Licensed under the MIT license.

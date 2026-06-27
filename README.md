# Leviath

**Hardware-inspired context window management for LLM agents**

Leviath is a Rust framework for building LLM agents with structured, tiered memory management. Inspired by hardware memory architectures (think SNES VRAM), Leviath gives agents explicit control over what stays in context and what gets evicted when memory fills up.

## Key Features

- **Structured Context Windows**: Define explicit memory regions (Pinned, SlidingWindow, Temporary, Compacting)
- **Hardware-Inspired Design**: Memory management based on proven hardware patterns
- **ECS Runtime**: Agent orchestration using `bevy_ecs` for parallel execution
- **Multi-Provider**: Anthropic Claude and OpenAI support
- **MCP Integration**: Model Context Protocol for tool discovery and execution
- **Agent Packaging**: Share and install agents via `leviath.toml` manifests

## Quick Start

```bash
# Install the CLI
cargo install --path crates/leviath-cli

# Create a new agent project
lev init my-agent

# Run your agent
lev run my-agent --task "Analyze this codebase"
```

## Architecture

Leviath is structured as a workspace with six crates:

- **leviath-core**: Core types (regions, layouts, blueprints)
- **leviath-runtime**: ECS-based agent execution engine
- **leviath-providers**: LLM provider integrations (Anthropic, OpenAI)
- **leviath-mcp**: Model Context Protocol tool integration
- **leviath-package**: Agent packaging and distribution
- **leviath-cli**: Command-line interface (`lev`)

## Documentation

- [V0 Overview](docs/v0-overview.md) - Comprehensive introduction and technical spec
- [Context Regions](docs/regions.md) - Deep dive into region types
- [Agent Blueprints](docs/blueprints.md) - Defining agent architectures
- [Examples](examples/) - Sample agents and use cases

## License

MIT OR Apache-2.0

## Project Info

- Website: [leviath.dev](https://leviath.dev)
- Domain: [leviath.ai](https://leviath.ai)
- Repository: [github.com/GEMISIS/leviath](https://github.com/GEMISIS/leviath)

//! `lev init` - Create a new agent project

use clap::Args;
use std::fs;
use std::path::Path;

#[derive(Args)]
pub struct InitArgs {
    /// Project name
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Project template (default, coding, research)
    #[arg(short, long, default_value = "default")]
    pub template: String,
}

pub async fn execute(args: InitArgs) -> anyhow::Result<()> {
    tracing::info!(name = %args.name, template = %args.template, "Initializing agent project");
    
    let project_dir = Path::new(&args.name);
    
    // Check if directory already exists
    if project_dir.exists() {
        anyhow::bail!("Directory '{}' already exists", args.name);
    }
    
    // Create project directory
    fs::create_dir_all(project_dir)?;
    fs::create_dir_all(project_dir.join("scripts"))?;
    fs::create_dir_all(project_dir.join("scripts/validators"))?;
    fs::create_dir_all(project_dir.join("scripts/transforms"))?;
    
    // Create agent.leviath manifest
    let manifest = create_manifest(&args.name, &args.template);
    fs::write(project_dir.join("agent.leviath"), manifest)?;
    
    // Create README
    let readme = create_readme(&args.name);
    fs::write(project_dir.join("README.md"), readme)?;

    // Create .gitignore
    fs::write(
        project_dir.join(".gitignore"),
        ".env\n*.leviath-bundle\n.leviath/\n",
    )?;

    // Create .env.example
    fs::write(
        project_dir.join(".env.example"),
        "# Copy this to .env and fill in your API key\n# ANTHROPIC_API_KEY=sk-ant-...\n# OPENAI_API_KEY=sk-...\n# OPENROUTER_API_KEY=sk-or-...\n",
    )?;

    println!("✓ Created agent project: {}", args.name);
    println!("\nNext steps:");
    println!("  cd {}", args.name);
    println!("  lev run --task \"Your task here\"");
    
    Ok(())
}

fn create_manifest(name: &str, template: &str) -> String {
    match template {
        "coding" => format!(r#"[agent]
name = "{name}"
version = "0.1.0"
description = "A coding assistant agent"

[stages.analyze]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-5" }}

[stages.implement]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-5" }}

[context.regions]
architecture = {{ kind = "pinned", max_tokens = 4000 }}
task = {{ kind = "pinned", max_tokens = 2000 }}
conversation = {{ kind = "sliding_window", max_items = 20, max_tokens = 15000 }}
files = {{ kind = "temporary", max_tokens = 30000 }}
scratch = {{ kind = "clearable", max_tokens = 10000 }}
"#, name = name),
        "research" => format!(r#"[agent]
name = "{name}"
version = "0.1.0"
description = "A research assistant agent"

[stages.gather]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-5" }}

[stages.analyze]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-5" }}

[stages.synthesize]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-opus-4" }}

[context.regions]
objective = {{ kind = "pinned", max_tokens = 2000 }}
sources = {{ kind = "temporary", max_tokens = 40000 }}
findings = {{ kind = "compacting", threshold_tokens = 8000, max_tokens = 15000 }}
findings_history = {{ kind = "compact_history", source_region = "findings", max_tokens = 10000 }}
conversation = {{ kind = "sliding_window", max_items = 15, max_tokens = 12000 }}
scratch = {{ kind = "clearable", max_tokens = 8000 }}
"#, name = name),
        _ => format!(r#"[agent]
name = "{name}"
version = "0.1.0"
description = "A simple agent"

[stages.main]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-5" }}

[context.regions]
system = {{ kind = "pinned", max_tokens = 2000 }}
conversation = {{ kind = "sliding_window", max_items = 10, max_tokens = 10000 }}
scratch = {{ kind = "clearable", max_tokens = 5000 }}
"#, name = name),
    }
}

fn create_readme(name: &str) -> String {
    format!(r#"# {name}

An agent built with Leviath.

## Usage

Run the agent:

```bash
lev run --task "Your task description"
```

## Configuration

Edit `agent.leviath` to customize:
- Stages and models
- Context regions and budgets
- Tools and permissions

## Learn More

- [Leviath Documentation](https://leviath.dev)
- [Agent Packaging Guide](https://leviath.dev/packaging)
"#, name = name)
}

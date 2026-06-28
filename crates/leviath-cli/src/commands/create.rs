//! `lev create` - Create a new agent blueprint

use clap::Args;
use std::fs;
use std::path::Path;

#[derive(Args)]
pub struct CreateArgs {
    /// Blueprint name
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Starting template (software-engineer, coder, researcher)
    #[arg(short, long, default_value = "software-engineer")]
    pub template: String,
}

pub async fn execute(args: CreateArgs) -> anyhow::Result<()> {
    tracing::info!(name = %args.name, template = %args.template, "Creating agent blueprint");

    let blueprint_dir = Path::new(&args.name);

    if blueprint_dir.exists() {
        anyhow::bail!("Directory '{}' already exists", args.name);
    }

    fs::create_dir_all(blueprint_dir)?;

    let manifest = create_manifest(&args.name, &args.template);
    fs::write(blueprint_dir.join("agent.leviath"), manifest)?;

    fs::write(
        blueprint_dir.join(".gitignore"),
        ".env\n*.leviath-bundle\n.leviath/\n",
    )?;

    fs::write(
        blueprint_dir.join(".env.example"),
        "# Copy this to .env and fill in your API key\n# ANTHROPIC_API_KEY=sk-ant-...\n# OPENAI_API_KEY=sk-...\n# OPENROUTER_API_KEY=sk-or-...\n",
    )?;

    println!("Created blueprint: {}", args.name);
    println!("\nNext steps:");
    println!("  cd {}", args.name);
    println!("  lev run . --task \"Your task here\"");
    println!("  lev install . && lev run {} --task \"Your task here\"", args.name);

    Ok(())
}

fn create_manifest(name: &str, template: &str) -> String {
    match template {
        "coder" => format!(
r#"[agent]
name = "{name}"
version = "0.1.0"
description = "A coding assistant blueprint"

[stages.analyze]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Understand the task and plan the implementation"
available_tools = ["read_file", "list_dir"]
max_iterations = 15
system_prompt = """
Analyze the coding task and produce a concise implementation plan.
"""

[stages.implement]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Write code according to the plan"
available_tools = ["write_file", "read_file", "edit_file", "list_dir", "bash"]
max_iterations = 50
system_prompt = """
Implement the plan. Create all necessary files and verify with bash.
"""

[context.regions]
task         = {{ kind = "pinned",          max_tokens = 2000 }}
codebase     = {{ kind = "temporary",       max_tokens = 30000 }}
conversation = {{ kind = "sliding_window",  max_items = 20, max_tokens = 15000 }}
scratch      = {{ kind = "clearable",       max_tokens = 10000 }}
"#,
            name = name
        ),

        "researcher" => format!(
r#"[agent]
name = "{name}"
version = "0.1.0"
description = "A research assistant blueprint"

[stages.gather]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Gather relevant information"
available_tools = ["read_file", "list_dir", "bash"]
max_iterations = 20

[stages.synthesize]
mode = "interactive"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Synthesize findings and discuss with user"
available_tools = ["read_file", "list_dir"]
max_iterations = 15

[context.regions]
objective    = {{ kind = "pinned",          max_tokens = 2000 }}
sources      = {{ kind = "temporary",       max_tokens = 40000 }}
findings     = {{ kind = "compacting",      threshold_tokens = 8000, max_tokens = 15000 }}
conversation = {{ kind = "sliding_window",  max_items = 15, max_tokens = 12000 }}
scratch      = {{ kind = "clearable",       max_tokens = 8000 }}
"#,
            name = name
        ),

        _ => format!(
r#"[agent]
name = "{name}"
version = "0.1.0"
description = "A simple agent blueprint"

[stages.main]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Main execution stage"
available_tools = ["read_file", "list_dir", "write_file", "bash"]
max_iterations = 30
system_prompt = """
You are a helpful agent. Complete the task thoroughly.
"""

[context.regions]
system       = {{ kind = "pinned",         max_tokens = 2000 }}
conversation = {{ kind = "sliding_window", max_items = 10, max_tokens = 10000 }}
scratch      = {{ kind = "clearable",      max_tokens = 5000 }}
"#,
            name = name
        ),
    }
}

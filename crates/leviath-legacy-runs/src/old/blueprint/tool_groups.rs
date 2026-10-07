//! Tool groups as an old manifest writes them: one `available_tools` entry,
//! such as `@builtin`, that stands for a whole source of tools.

/// A source of tools that one `available_tools` entry can grant whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolGroup {
    /// Every tool the install has, across every other group.
    All,
    /// The tools compiled into this build of Leviath.
    Builtin,
    /// The sub-agent tools (`spawn_agent` and its siblings).
    Subagent,
    /// Every Rhai script tool: the agent's own `tools/` and the global
    /// directory, plus, for a `dynamic_tools` agent, whatever it installs
    /// mid-run.
    Scripts,
    /// Every tool every connected MCP server advertises.
    Mcp,
}

impl ToolGroup {
    /// Every group.
    pub const ALL: &'static [ToolGroup] = &[
        ToolGroup::All,
        ToolGroup::Builtin,
        ToolGroup::Subagent,
        ToolGroup::Scripts,
        ToolGroup::Mcp,
    ];

    /// The token a manifest writes for this group.
    pub fn token(self) -> &'static str {
        match self {
            ToolGroup::All => "@all",
            ToolGroup::Builtin => "@builtin",
            ToolGroup::Subagent => "@subagent",
            ToolGroup::Scripts => "@scripts",
            ToolGroup::Mcp => "@mcp",
        }
    }

    /// The group a manifest entry names, or `None` for an ordinary tool name
    /// or a token that names no group.
    pub fn parse(entry: &str) -> Option<ToolGroup> {
        ToolGroup::ALL.iter().copied().find(|g| g.token() == entry)
    }
}

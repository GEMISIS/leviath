//! Tool groups: one `available_tools` entry that stands for a whole source.
//!
//! `available_tools` is an exact-match list, and for a stage that wants a few
//! named tools that is the right shape. For a stage that wants "everything the
//! binary ships, plus the two MCP tools I care about" it means copying the
//! built-in list into the manifest and keeping it current by hand, and a tool
//! added to Leviath later is simply never offered.
//!
//! A group token names a source instead of its members. It is resolved at
//! spawn against what the install actually has then, the same way a connector
//! grant is resolved against what its server advertises, and merged with the
//! names beside it. So the four shapes a stage tends to want each have a
//! spelling:
//!
//! | wants | writes |
//! |---|---|
//! | a hand-picked few | `["read_file", "shell"]` |
//! | every built-in, plus chosen scripts and MCP tools | `["@builtin", "summarize", "github__create_issue"]` |
//! | every built-in and every script, plus chosen MCP tools | `["@builtin", "@scripts", "github__create_issue"]` |
//! | everything the install has | `["@all"]` |
//!
//! A group grants *visibility* only. `tool_permissions`, the taint gate and the
//! approval prompts apply to a tool reached through a group exactly as they do
//! to one named by hand, so `@all` on a stage with `shell = "ask"` still asks.
//!
//! The stage-control tools `submit_output` and `fan_out` belong to no group:
//! the stage mode grants them, or the manifest names them. A group that quietly
//! handed every stage a way to end the run would be a surprise, not a
//! convenience.

use std::fmt;

/// The character every group token starts with. A tool name never contains
/// it (built-ins, script tools and sanitized MCP names are all
/// `[A-Za-z0-9_-]`), so a token cannot be mistaken for a tool and a tool
/// cannot be mistaken for a token.
pub const TOOL_GROUP_PREFIX: char = '@';

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
    /// Every group, in the order a picker should list them.
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

    /// What the group covers, in one line a picker or an error can show.
    pub fn describe(self) -> &'static str {
        match self {
            ToolGroup::All => "every tool this install has: built in, sub-agent, scripts, and MCP",
            ToolGroup::Builtin => "every tool compiled into Leviath",
            ToolGroup::Subagent => "the sub-agent tools: spawn, check, wait, send, kill",
            ToolGroup::Scripts => "every Rhai script tool, the agent's own and the global ones",
            ToolGroup::Mcp => "every tool every connected MCP server advertises",
        }
    }

    /// The group a manifest entry names, or `None` for an ordinary tool name.
    ///
    /// An entry that starts with the prefix but names no group is *also*
    /// `None` here; [`unknown_group`] is the question to ask about those.
    pub fn parse(entry: &str) -> Option<ToolGroup> {
        ToolGroup::ALL.iter().copied().find(|g| g.token() == entry)
    }

    /// Whether a tool from `source` is covered by this group.
    pub fn covers(self, source: ToolGroup) -> bool {
        self == ToolGroup::All || self == source
    }
}

impl fmt::Display for ToolGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

/// Whether `entry` is spelled like a group token, whether or not it names one.
pub fn is_tool_group_token(entry: &str) -> bool {
    entry.starts_with(TOOL_GROUP_PREFIX)
}

/// An entry spelled like a group that names no group: `@builtins`, `@ALL`,
/// `@`. Such an entry can never match a tool either, so it is a typo by
/// construction, and the one place the exact-match list gets to say so.
pub fn unknown_group(entry: &str) -> bool {
    is_tool_group_token(entry) && ToolGroup::parse(entry).is_none()
}

/// The groups a grant list names, in list order, de-duplicated.
pub fn groups_in(entries: &[String]) -> Vec<ToolGroup> {
    let mut groups = Vec::new();
    for entry in entries {
        if let Some(group) = ToolGroup::parse(entry)
            && !groups.contains(&group)
        {
            groups.push(group);
        }
    }
    groups
}

/// The comma-separated list of every group token, for an error message.
pub fn group_tokens_list() -> String {
    ToolGroup::ALL
        .iter()
        .map(|g| g.token())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_token_round_trips_through_parse() {
        for group in ToolGroup::ALL {
            assert_eq!(ToolGroup::parse(group.token()), Some(*group));
            assert!(group.token().starts_with(TOOL_GROUP_PREFIX));
            assert!(!group.describe().is_empty());
            assert_eq!(group.to_string(), group.token());
        }
    }

    #[test]
    fn a_tool_name_is_not_a_group() {
        assert_eq!(ToolGroup::parse("read_file"), None);
        assert_eq!(ToolGroup::parse("github__create_issue"), None);
        assert!(!is_tool_group_token("read_file"));
        assert!(!unknown_group("read_file"));
    }

    #[test]
    fn a_misspelled_group_is_unknown_not_a_tool() {
        for entry in ["@builtins", "@ALL", "@", "@mcp "] {
            assert_eq!(ToolGroup::parse(entry), None, "{entry}");
            assert!(is_tool_group_token(entry), "{entry}");
            assert!(unknown_group(entry), "{entry}");
        }
        assert!(!unknown_group("@all"));
    }

    #[test]
    fn all_covers_every_source_and_a_source_covers_itself() {
        for source in ToolGroup::ALL {
            assert!(ToolGroup::All.covers(*source));
            assert!(source.covers(*source));
        }
        assert!(!ToolGroup::Builtin.covers(ToolGroup::Scripts));
        assert!(!ToolGroup::Mcp.covers(ToolGroup::Builtin));
    }

    #[test]
    fn groups_in_keeps_list_order_and_drops_repeats() {
        let entries: Vec<String> = ["@scripts", "read_file", "@builtin", "@scripts", "@nope"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            groups_in(&entries),
            vec![ToolGroup::Scripts, ToolGroup::Builtin]
        );
        assert!(groups_in(&[]).is_empty());
    }

    #[test]
    fn the_token_list_names_every_group() {
        let list = group_tokens_list();
        for group in ToolGroup::ALL {
            assert!(list.contains(group.token()), "{list}");
        }
    }
}

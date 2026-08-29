//! What tools an agent on this machine can actually use, and where each one
//! came from.
//!
//! One answer, two callers. The lint asks "is this name a tool" when it checks a
//! blueprint's `available_tools`, and `GET /api/tools` asks "what may I pick"
//! on behalf of an editor. Those were the same four discovery rules - built-ins,
//! sub-agent tools, the agent's own `tools/`, and the global drop-in directory -
//! and a second copy of them would have drifted from the first the moment either
//! side gained a source.
//!
//! The compile failures come out with the tools rather than being dropped. A
//! script missing because its file has a syntax error looks exactly like a
//! script that was never written, and only one of those is worth telling
//! somebody about.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Where a tool comes from, which is the part that answers "will this work if I
/// pick it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolSource {
    /// Compiled into this build of Leviath. Available to every agent, always.
    Builtin,
    /// A sub-agent tool, offered to an agent that may spawn children.
    Subagent,
    /// A `.rhai` script in the agent's own `tools/` directory, so it travels
    /// with that agent and no other.
    Agent,
    /// A `.rhai` script in the global tools directory, so every agent on this
    /// machine gets it.
    Global,
}

impl ToolSource {
    /// The wire name for this source, as the REST API spells it.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Subagent => "subagent",
            Self::Agent => "agent",
            Self::Global => "global",
        }
    }
}

/// One tool an agent may name in `available_tools`.
#[derive(Debug, Clone)]
pub(crate) struct ToolEntry {
    /// The name the model calls and a blueprint lists.
    pub name: String,
    /// Where the tool comes from.
    pub source: ToolSource,
    /// The `.rhai` file behind it, for the script-backed sources only.
    pub path: Option<PathBuf>,
    /// Which agent owns it, for [`ToolSource::Agent`] only.
    pub agent: Option<String>,
}

/// A `.rhai` file that was found but did not become a usable tool.
#[derive(Debug, Clone)]
pub(crate) struct SkippedScript {
    /// The file that was passed over.
    pub path: PathBuf,
    /// Why, in the words the compiler or the shadowing rule used.
    pub reason: String,
    /// Which directory it was found in.
    pub source: ToolSource,
}

/// The full tool inventory for one scope: an agent plus the global directory,
/// or the global directory alone.
#[derive(Debug, Clone, Default)]
pub(crate) struct ToolInventory {
    /// Every usable tool, built-ins first, then the agent's scripts, then the
    /// global ones.
    pub tools: Vec<ToolEntry>,
    /// Every `.rhai` file that was found and could not be offered.
    pub skipped: Vec<SkippedScript>,
}

impl ToolInventory {
    /// Discover everything an agent rooted at `agent_dir` could call.
    ///
    /// `agent_dir` is `None` for a question about the machine rather than about
    /// one agent: built-ins and the global directory, with no agent scripts.
    /// `agent_name` only labels the agent-scoped entries, so a caller that has
    /// a directory but no name (the daemon's own offline lint) may leave it out.
    ///
    /// A script whose name is already taken - by a built-in, by a sub-agent
    /// tool, or by the agent's own copy shadowing a global one - is reported in
    /// [`skipped`](Self::skipped) rather than listed twice. That mirrors what
    /// the daemon does at spawn: the earlier directory wins and a core tool is
    /// never shadowed, so listing the loser as available would be a lie.
    pub(crate) fn discover(agent_dir: Option<&Path>, agent_name: Option<&str>) -> Self {
        let ctx_dir = agent_dir.map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let builtins = leviath_tools::BuiltinTools::new(leviath_tools::ToolContext::new(ctx_dir));

        let mut tools: Vec<ToolEntry> = Vec::new();
        for name in builtins.names() {
            tools.push(ToolEntry {
                name,
                source: ToolSource::Builtin,
                path: None,
                agent: None,
            });
        }
        for name in leviath_tools::BuiltinTools::subagent_tool_names() {
            tools.push(ToolEntry {
                name,
                source: ToolSource::Subagent,
                path: None,
                agent: None,
            });
        }

        let mut taken: HashSet<String> = tools.iter().map(|t| t.name.clone()).collect();
        let mut skipped: Vec<SkippedScript> = Vec::new();

        // The agent's own `tools/` first, then the global one every agent gets:
        // the same order, and so the same precedence, the daemon scans in.
        let scopes = [
            (ToolSource::Agent, agent_dir.map(|d| d.join("tools"))),
            (ToolSource::Global, leviath_core::tools_dir()),
        ];
        for (source, dir) in scopes {
            let Some(dir) = dir else {
                continue;
            };
            let (set, failed) = leviath_scripting::ScriptToolSet::discover(&[dir]);
            for f in failed {
                skipped.push(SkippedScript {
                    path: f.path,
                    reason: f.reason,
                    source,
                });
            }
            for (meta, path) in set.sources() {
                if taken.contains(&meta.name) {
                    skipped.push(SkippedScript {
                        path,
                        reason: format!(
                            "the name '{}' is already taken by a tool that wins over this one",
                            meta.name
                        ),
                        source,
                    });
                    continue;
                }
                taken.insert(meta.name.clone());
                let agent = match source {
                    ToolSource::Agent => agent_name.map(str::to_string),
                    _ => None,
                };
                tools.push(ToolEntry {
                    name: meta.name,
                    source,
                    path: Some(path),
                    agent,
                });
            }
        }

        Self { tools, skipped }
    }

    /// Just the names, which is all the lint needs to answer "is this a tool".
    pub(crate) fn names(&self) -> HashSet<String> {
        self.tools.iter().map(|t| t.name.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point the global tools directory at a scratch root, so a developer's real
    /// `~/.leviath/tools` never decides what these assert.
    fn with_home<R>(f: impl FnOnce(&Path) -> R) -> R {
        let dir = tempfile::tempdir().expect("a temp dir");
        temp_env::with_var("LEVIATH_HOME", Some(dir.path()), || f(dir.path()))
    }

    /// The global tools directory under a `LEVIATH_HOME` scratch root.
    fn global_tools(home: &Path) -> PathBuf {
        let dir = home.join(".leviath").join("tools");
        std::fs::create_dir_all(&dir).expect("the global tools dir");
        dir
    }

    fn write_tool(dir: &Path, file: &str, body: &str) {
        std::fs::create_dir_all(dir).expect("the tools dir");
        std::fs::write(dir.join(file), body).expect("the script");
    }

    #[test]
    fn source_names_are_the_wire_spelling() {
        assert_eq!(ToolSource::Builtin.as_str(), "builtin");
        assert_eq!(ToolSource::Subagent.as_str(), "subagent");
        assert_eq!(ToolSource::Agent.as_str(), "agent");
        assert_eq!(ToolSource::Global.as_str(), "global");
    }

    /// All four sources in one inventory, which is the whole point of the
    /// `source` field: three different answers to whether a name will work.
    #[test]
    fn every_source_appears_with_the_path_behind_it() {
        with_home(|home| {
            let agent = home.join("agents").join("researcher");
            write_tool(
                &agent.join("tools"),
                "web_search.rhai",
                "// @tool web_search\n// @description searches\n1",
            );
            write_tool(
                &global_tools(home),
                "summarize.rhai",
                "// @tool summarize\n// @description sums\n2",
            );

            let inv = ToolInventory::discover(Some(&agent), Some("researcher"));

            let own = inv
                .tools
                .iter()
                .find(|t| t.name == "web_search")
                .expect("the agent's own tool");
            assert_eq!(own.source, ToolSource::Agent);
            assert_eq!(own.agent.as_deref(), Some("researcher"));
            assert_eq!(
                own.path.as_deref(),
                Some(agent.join("tools").join("web_search.rhai").as_path())
            );

            let global = inv
                .tools
                .iter()
                .find(|t| t.name == "summarize")
                .expect("the global tool");
            assert_eq!(global.source, ToolSource::Global);
            assert!(global.agent.is_none());
            assert!(global.path.is_some());

            assert!(inv.tools.iter().any(|t| t.source == ToolSource::Builtin));
            assert!(inv.tools.iter().any(|t| t.source == ToolSource::Subagent));
            assert!(
                inv.tools
                    .iter()
                    .all(|t| t.source != ToolSource::Builtin || t.path.is_none())
            );
            assert!(inv.skipped.is_empty());
        });
    }

    /// Without a name, an agent-scoped entry still resolves; it just cannot say
    /// whose it is. That is the daemon's own offline lint, which has the
    /// directory and never had a name.
    #[test]
    fn an_unnamed_scope_still_finds_the_agents_own_tools() {
        with_home(|home| {
            let agent = home.join("agents").join("nameless");
            write_tool(&agent.join("tools"), "local.rhai", "// @tool local\n1");

            let inv = ToolInventory::discover(Some(&agent), None);
            let own = inv
                .tools
                .iter()
                .find(|t| t.name == "local")
                .expect("the agent's own tool");
            assert_eq!(own.source, ToolSource::Agent);
            assert!(own.agent.is_none());
        });
    }

    /// No agent means no agent scripts, and the global directory still answers.
    #[test]
    fn without_an_agent_only_the_machine_wide_scripts_are_listed() {
        with_home(|home| {
            write_tool(
                &global_tools(home),
                "summarize.rhai",
                "// @tool summarize\n1",
            );

            let inv = ToolInventory::discover(None, None);
            assert!(inv.tools.iter().any(|t| t.name == "summarize"));
            assert!(inv.tools.iter().all(|t| t.source != ToolSource::Agent));
        });
    }

    /// A file that will not compile is reported instead of quietly vanishing.
    #[test]
    fn a_script_that_does_not_compile_is_reported_with_its_reason() {
        with_home(|home| {
            let agent = home.join("agents").join("broken");
            write_tool(&agent.join("tools"), "bad.rhai", "// no directive\nlet");

            let inv = ToolInventory::discover(Some(&agent), Some("broken"));
            assert_eq!(inv.skipped.len(), 1);
            assert!(inv.skipped[0].path.ends_with("bad.rhai"));
            assert_eq!(inv.skipped[0].source, ToolSource::Agent);
            assert!(!inv.skipped[0].reason.is_empty());
        });
    }

    /// A script named after a built-in never routes, so it is reported as
    /// skipped rather than listed twice under two sources.
    #[test]
    fn a_script_shadowed_by_a_builtin_is_skipped_not_listed_twice() {
        with_home(|home| {
            let agent = home.join("agents").join("shadow");
            write_tool(
                &agent.join("tools"),
                "read_file.rhai",
                "// @tool read_file\n1",
            );

            let inv = ToolInventory::discover(Some(&agent), Some("shadow"));
            let listed: Vec<_> = inv.tools.iter().filter(|t| t.name == "read_file").collect();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].source, ToolSource::Builtin);
            assert_eq!(inv.skipped.len(), 1);
            assert!(inv.skipped[0].reason.contains("already taken"));
        });
    }

    /// The agent's own copy wins over the global one of the same name, and the
    /// global copy is reported so a picker can explain why it is not the one
    /// that will run.
    #[test]
    fn an_agents_own_script_wins_over_the_global_one() {
        with_home(|home| {
            let agent = home.join("agents").join("winner");
            write_tool(&agent.join("tools"), "dup.rhai", "// @tool dup\n1");
            write_tool(&global_tools(home), "dup.rhai", "// @tool dup\n2");

            let inv = ToolInventory::discover(Some(&agent), Some("winner"));
            let listed: Vec<_> = inv.tools.iter().filter(|t| t.name == "dup").collect();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].source, ToolSource::Agent);
            assert_eq!(inv.skipped.len(), 1);
            assert_eq!(inv.skipped[0].source, ToolSource::Global);
        });
    }

    /// The set the lint checks `available_tools` against carries every listed
    /// name and nothing that was skipped.
    #[test]
    fn names_carry_the_builtins_and_the_scripts_that_compiled() {
        with_home(|home| {
            let agent = home.join("agents").join("named");
            write_tool(&agent.join("tools"), "ok.rhai", "// @tool ok\n1");
            write_tool(&agent.join("tools"), "bad.rhai", "// nothing\nlet");

            let names = ToolInventory::discover(Some(&agent), Some("named")).names();
            assert!(names.contains("ok"));
            assert!(names.contains("read_file"));
            assert!(!names.contains("bad"));
        });
    }
}

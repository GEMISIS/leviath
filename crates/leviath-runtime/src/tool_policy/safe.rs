//! The safe list: what a run may call without asking anybody, where its
//! policy says `ask`.
//!
//! A safe entry is a key, in the space a "for this run" approval is
//! remembered in: a tool's name, or a shell command prefix under
//! [`SHELL_KEY_PREFIX`]. A call whose every key is on the list runs
//! unprompted. Safe entries only ever turn `ask` into `allow`; they never
//! reach a `deny`.
//!
//! [`resolve_safe_keys`] is the one place that knows which layers count for
//! a run: the shipped list, the user's `[safe_commands]`, their
//! `[agent_safe_commands.<name>]` for an installed blueprint, and the
//! graph's own `safe_commands` once the user opted in. Reading a shell line
//! into its keys is a host's own work, so the shell half of the list is
//! held only for a host that says which prefixes it can match.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{ToolPermissions, spawns_a_raw_graph};
use crate::spec::graph::SafeCommandsDef;

/// The namespace every shell key carries, so a key can never collide with the
/// bare tool name a non-shell approval uses.
pub const SHELL_KEY_PREFIX: &str = "shell:";

/// Shell commands that are safe to run without asking.
///
/// The rule an entry has to pass, which matters more than the list because the
/// list will grow: **it must not be able to write a file, execute another
/// program, or open a network connection under any flag.**
///
/// That rule is why several obvious candidates are absent. `find` takes
/// `-exec` and `-delete`; `sed` takes `-i`; `awk` has `system()`; `sort` takes
/// `-o`; `tee`, `xargs`, `env`, `nohup`, `timeout` and `watch` all run a
/// program named in their arguments; `cargo` runs build scripts and test
/// binaries. Any of them can be added by name in `[safe_commands] shell`, which
/// is the point of the setting.
///
/// Three more are absent though they read as harmless:
/// - `uniq` takes an **output operand** (`uniq IN OUT`), so `uniq payload
///   ~/.bashrc` writes an arbitrary file. Positional, so no flag check could
///   catch it.
/// - `tree` takes `-o FILE`.
/// - `rg` takes `--pre COMMAND`, which runs that command over every input file,
///   and `-z`, which shells out to decompressors.
///
/// The `git` entries stay, because read-only git is most of what a coding agent
/// does - but `--output` is a diff-machinery option accepted by `diff`, `log`
/// and `show`, so a host that reads shell lines refuses a git segment
/// carrying it. That is a patch on one known escape, not a claim that
/// git has no others: `[sandbox]` is the durable answer, and this list is
/// pre-decided convenience inside it.
///
/// Two consequences worth stating rather than burying. `cat`, `head` and `grep`
/// being safe would let an agent read any file the user can, so an entry only
/// covers a segment whose path operands all stay inside the workdir or a
/// granted `[read_paths]` root: the fence `read_file` has, asked of every word
/// that could name a file. `cat src/main.rs` runs unprompted; `cat /etc/hosts`,
/// `grep -r key ~` and `cd / && cat etc/passwd` fall back to the shell's
/// policy, which asks by default, and `[safe_commands] outside_workdir =
/// "deny"` refuses them outright. The check fails closed - what it cannot
/// place counts as outside - but it reads words, so a program the line runs
/// can still read what it likes. And the read-only `git` subcommands honour a
/// repository's `core.pager` and `diff.external`. A pager does not run without
/// a tty, but `diff.external` does - reachable only via `git -c`, which keys as
/// a bare `git` and so is not covered by any entry here.
pub const DEFAULT_SAFE_SHELL: &[&str] = &[
    // Reading and listing.
    "ls",
    "cat",
    "head",
    "tail",
    "wc",
    "cut",
    "tr",
    "grep",
    "egrep",
    "fgrep",
    "diff",
    "cmp",
    "file",
    "stat",
    "du",
    "df",
    "jq",
    "column",
    "od",
    "strings",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "pwd",
    "cd",
    // Reporting.
    "echo",
    "printf",
    "date",
    "seq",
    "sleep",
    "true",
    "false",
    "test",
    "which",
    "type",
    "uname",
    "hostname",
    "whoami",
    "id",
    "ps",
    "pgrep",
    // Read-only git.
    "git status",
    "git diff",
    "git log",
    "git show",
    "git branch",
    "git remote",
    "git rev-parse",
    "git blame",
    "git describe",
    "git ls-files",
];

/// Where a safe key came from, for `lev approvals safe`.
///
/// The question this answers is "why did it not ask me", which is the one a
/// person asks the first time a run does something unprompted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeSource {
    /// [`DEFAULT_SAFE_SHELL`], or a built-in tool that never prompts.
    Default,
    /// The user's `[safe_commands]`.
    Config,
    /// The user's `[agent_safe_commands.<name>]`.
    Agent,
    /// The blueprint's own `[safe_commands]`, which the user opted into.
    Blueprint,
}

/// The user's `[safe_commands]`, as far as which keys are safe goes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SafeCommandSettings {
    /// `defaults`: whether the shipped list ([`DEFAULT_SAFE_SHELL`]) counts.
    /// On unless the user turns it off.
    #[serde(default = "leviath_core::default_true")]
    pub defaults: bool,
    /// `tools`: tools that need no prompt whatever their arguments. Built-in
    /// names, or MCP names in their advertised form (`server__tool`).
    #[serde(default)]
    pub tools: Vec<String>,
    /// `shell`: shell command prefixes that need no prompt.
    #[serde(default)]
    pub shell: Vec<String>,
}

/// Written out, not derived: the shipped list is on for a user who has no
/// `[safe_commands]` table and for one who has no config file at all, and a
/// derived default would turn it off for the second.
impl Default for SafeCommandSettings {
    fn default() -> Self {
        Self {
            defaults: leviath_core::default_true(),
            tools: Vec::new(),
            shell: Vec::new(),
        }
    }
}

/// One `[agent_safe_commands.<name>]` table: what the user adds to the safe
/// list for runs of one installed blueprint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct AgentSafeCommandSettings {
    /// `tools`: tool names pre-approved for this blueprint.
    pub tools: Vec<String>,
    /// `shell`: shell command prefixes pre-approved for it.
    pub shell: Vec<String>,
    /// `allow_blueprint`: honour this blueprint's own `safe_commands`. Off
    /// by default: declaring is not granting.
    pub allow_blueprint: bool,
}

/// The safe keys in effect for one run, and where each came from.
///
/// **Declaring is not granting.** A blueprint's own block contributes nothing
/// unless `allow_blueprint` names this agent or
/// `[security] allow_blueprint_safe_commands` is set, which is the same shape
/// `[read_paths]` uses: a manifest the user downloaded may describe what
/// it would like to run unprompted, and the user decides whether that counts.
///
/// `shell_prefix_ok` is a host's reading of shell lines: whether an entry is
/// a command prefix a call's key can match. With it the list holds the
/// shipped shell commands and every `shell` entry that passes, each under
/// [`SHELL_KEY_PREFIX`]; an entry that fails is skipped with a warning and
/// not fatal, since a typo in a config file should cost one prompt, not a
/// run. Without it (`None`) the list holds the tool names alone, which is
/// all a caller that reads no shell lines can match a call against.
pub fn resolve_safe_keys(
    config: &SafeCommandSettings,
    agent: Option<&AgentSafeCommandSettings>,
    blueprint: Option<&SafeCommandsDef>,
    allow_blueprint_globally: bool,
    shell_prefix_ok: Option<&dyn Fn(&str) -> bool>,
) -> BTreeMap<String, SafeSource> {
    let mut keys = BTreeMap::new();
    if config.defaults && shell_prefix_ok.is_some() {
        for entry in DEFAULT_SAFE_SHELL {
            keys.insert(format!("{SHELL_KEY_PREFIX}{entry}"), SafeSource::Default);
        }
    }
    let mut add = |tools: &[String], shell: &[String], source: SafeSource| {
        add_layer(&mut keys, tools, shell, source, shell_prefix_ok);
    };
    add(&config.tools, &config.shell, SafeSource::Config);
    if let Some(agent) = agent {
        add(&agent.tools, &agent.shell, SafeSource::Agent);
    }
    let opted_in = allow_blueprint_globally || agent.is_some_and(|a| a.allow_blueprint);
    if let (true, Some(bp)) = (opted_in, blueprint) {
        let tools: Vec<String> = bp.tools.iter().map(ToString::to_string).collect();
        add(&tools, &bp.shell, SafeSource::Blueprint);
    }
    keys
}

/// Fold one layer's entries in, later layers winning the `source` label so
/// `lev approvals safe` names the narrowest thing that put a key there.
fn add_layer(
    keys: &mut BTreeMap<String, SafeSource>,
    tools: &[String],
    shell: &[String],
    source: SafeSource,
    shell_prefix_ok: Option<&dyn Fn(&str) -> bool>,
) {
    for tool in tools {
        // `tools` and `shell` land in one map, so a `tools` entry spelled with
        // the shell prefix would enter the shell key space without being
        // checked as a prefix - which is the only thing standing between
        // a config file and a pre-approved `shell:>/root/.bashrc`,
        // `shell:sh`, or `shell:env:PATH`. A tool name never needs that
        // prefix, so refusing it costs nothing and closes the back door.
        if tool.starts_with(SHELL_KEY_PREFIX) {
            tracing::warn!(
                "ignoring safe_commands tools entry {tool:?}: shell commands belong in the \
                 `shell` list, where they are checked"
            );
            continue;
        }
        keys.insert(tool.clone(), source);
    }
    let Some(prefix_ok) = shell_prefix_ok else {
        return;
    };
    for entry in shell {
        if !prefix_ok(entry) {
            tracing::warn!(
                "ignoring safe_commands shell entry {entry:?}: it is not a bare command prefix \
                 (a program, optionally with the subcommand that narrows it)"
            );
            continue;
        }
        keys.insert(format!("{SHELL_KEY_PREFIX}{entry}"), source);
    }
}

/// The key a call that is not a shell line is approved under, and so what
/// the safe list has to hold for it to run unprompted: the tool's name as
/// called, or the `spawn_raw_graph` permission for a `spawn_agent` call that
/// carries a graph of its own (approving one raw graph approves raw graphs,
/// never every later `spawn_agent`).
///
/// `None` for a shell call: its keys are the commands on its line, which a
/// host that reads shell lines derives.
pub fn approval_key(tool_name: &str, arguments: &serde_json::Value) -> Option<String> {
    if spawns_a_raw_graph(tool_name, arguments) {
        return Some(leviath_tools::SPAWN_RAW_GRAPH_PERMISSION.to_string());
    }
    (leviath_tools::canonical_tool_name(tool_name) != "shell").then(|| tool_name.to_string())
}

impl ToolPermissions {
    /// The safe keys in effect for a run of the blueprint `agent` (`None`
    /// for a graph its caller wrote, which gets the user's own list and no
    /// per-agent table), and where each came from. `graph` is the graph's
    /// own `safe_commands`, which counts only where the user opted in. See
    /// [`resolve_safe_keys`] for `shell_prefix_ok`.
    pub fn safe_keys(
        &self,
        agent: Option<&str>,
        graph: Option<&SafeCommandsDef>,
        shell_prefix_ok: Option<&dyn Fn(&str) -> bool>,
    ) -> BTreeMap<String, SafeSource> {
        resolve_safe_keys(
            &self.safe_commands,
            agent.and_then(|name| self.agent_safe_commands.get(name)),
            graph,
            self.allow_blueprint_safe_commands,
            shell_prefix_ok,
        )
    }
}

#[cfg(test)]
#[path = "safe_tests.rs"]
mod tests;

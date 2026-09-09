//! The shape of one profile in `yolo.toml`, and the compiled rules it becomes.
//!
//! The raw `*Spec` types are what serde reads and what the API hands back; the
//! compiled types beside them are what a decision runs against. Compiling at
//! load is what turns a typo in a glob into an error naming the entry, rather
//! than a rule that silently never matches.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::config::ToolPolicy;
use leviath_core::blueprint::{ToolGroup, is_tool_group_token};

/// What a profile decides for a call: run it, put it to a person, or refuse it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Verdict {
    /// Run without a prompt.
    Allow,
    /// The ordinary approval prompt, exactly what the call gets without
    /// `--yolo`.
    Ask,
    /// Refuse outright; nothing waits on anybody.
    Deny,
}

impl Verdict {
    /// The policy the dispatcher acts on.
    pub(crate) fn policy(self) -> ToolPolicy {
        match self {
            Verdict::Allow => ToolPolicy::Allow,
            Verdict::Ask => ToolPolicy::Ask,
            Verdict::Deny => ToolPolicy::Deny,
        }
    }

    /// The stricter of two verdicts, for a line whose commands disagree.
    pub(crate) fn stricter(self, other: Verdict) -> Verdict {
        match self.rank() >= other.rank() {
            true => self,
            false => other,
        }
    }

    fn rank(self) -> u8 {
        match self {
            Verdict::Allow => 0,
            Verdict::Ask => 1,
            Verdict::Deny => 2,
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Verdict::Allow => "allow",
            Verdict::Ask => "ask",
            Verdict::Deny => "deny",
        })
    }
}

/// The `default` key: what the profile does with a call no rule names and the
/// config would ask about. `allow` is bare `--yolo`; `ask` leaves the prompt
/// in place.
///
/// Deliberately two values, not three. A profile that refused everything it
/// did not list would be a denylist by omission, and a person who wants a
/// call refused writes it in `deny` where it can be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Waiver {
    /// Run it: the yolo waiver.
    Allow,
    /// Ask: the standard prompt.
    Ask,
}

impl Waiver {
    pub(crate) fn verdict(self) -> Verdict {
        match self {
            Waiver::Allow => Verdict::Allow,
            Waiver::Ask => Verdict::Ask,
        }
    }
}

/// Whether a human-in-the-loop mechanism reaches a person or answers itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Human {
    /// A real prompt, however the run was launched.
    Ask,
    /// Today's `--yolo`: auto-approved, or not offered at all.
    Auto,
}

impl Human {
    /// Whether nobody is expected to be there for this mechanism.
    pub(crate) fn is_auto(self) -> bool {
        self == Human::Auto
    }
}

/// `[<name>.tools]`: tool names, globs and `@groups` under each verdict.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolListsSpec {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub ask: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

/// One `[[<name>.shell.<verdict>]]` entry as written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShellRuleSpec {
    /// Leading words, one glob per word: `rm -r*`, `git push*`, `cargo *`.
    pub command: String,
    /// Globs every remaining word must satisfy. Absent means any remaining
    /// words; empty means none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
}

/// `[<name>.shell]`: shell rules under each verdict.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShellListsSpec {
    #[serde(default)]
    pub allow: Vec<ShellRuleSpec>,
    #[serde(default)]
    pub ask: Vec<ShellRuleSpec>,
    #[serde(default)]
    pub deny: Vec<ShellRuleSpec>,
}

/// One profile as written in `yolo.toml`.
///
/// `default` has no serde default on purpose: it is the one key that decides
/// what the profile is for, and a file that leaves it out should say so at
/// load rather than quietly become bare `--yolo`. The three human knobs
/// default to `auto`, because a profile is a kind of `--yolo` until it says
/// which parts of it a person still wants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProfileSpec {
    pub default: Waiver,
    #[serde(default = "auto")]
    pub questions: Human,
    #[serde(default = "auto")]
    pub checkpoints: Human,
    #[serde(default = "auto")]
    pub gate: Human,
    #[serde(default)]
    pub tools: ToolListsSpec,
    #[serde(default)]
    pub shell: ShellListsSpec,
}

fn auto() -> Human {
    Human::Auto
}

impl ProfileSpec {
    /// Bare `--yolo` as a profile: every `Ask` becomes `Allow`, nothing waits
    /// on a person, no lists.
    pub(crate) fn builtin_default() -> Self {
        Self {
            default: Waiver::Allow,
            questions: Human::Auto,
            checkpoints: Human::Auto,
            gate: Human::Auto,
            tools: ToolListsSpec::default(),
            shell: ShellListsSpec::default(),
        }
    }
}

/// Why an entry could not be compiled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuleError {
    /// The entry as written.
    pub entry: String,
    /// What is wrong with it.
    pub reason: String,
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.entry, self.reason)
    }
}

/// How one `tools` entry matches a called tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolMatcher {
    /// A group token: every tool of that source.
    Group(ToolGroup),
    /// A glob over the advertised name, for `github__*` and the like.
    Glob(glob::Pattern),
    /// An exact name, matched under every spelling the tool answers to.
    Name(String),
}

/// One compiled `tools` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolRule {
    /// As written, for the reason a decision reports.
    pub entry: String,
    pub matcher: ToolMatcher,
}

impl ToolRule {
    fn compile(entry: &str) -> Result<Self, RuleError> {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return Err(RuleError {
                entry: entry.to_string(),
                reason: "an empty entry names no tool".to_string(),
            });
        }
        let matcher = if is_tool_group_token(trimmed) {
            match ToolGroup::parse(trimmed) {
                Some(group) => ToolMatcher::Group(group),
                None => {
                    return Err(RuleError {
                        entry: entry.to_string(),
                        reason: format!(
                            "not a tool group; the groups are {}",
                            leviath_core::blueprint::group_tokens_list()
                        ),
                    });
                }
            }
        } else if trimmed.contains(['*', '?', '[']) {
            ToolMatcher::Glob(glob::Pattern::new(trimmed).map_err(|e| RuleError {
                entry: entry.to_string(),
                reason: format!("not a valid glob: {e}"),
            })?)
        } else {
            ToolMatcher::Name(trimmed.to_string())
        };
        Ok(Self {
            entry: trimmed.to_string(),
            matcher,
        })
    }

    /// Whether this entry covers `tool`, a tool of `source`.
    pub(crate) fn matches(&self, tool: &str, source: ToolGroup) -> bool {
        match &self.matcher {
            ToolMatcher::Group(group) => group.covers(source),
            ToolMatcher::Glob(pattern) => pattern.matches(tool),
            ToolMatcher::Name(name) => leviath_tools::tool_name_spellings(tool).any(|s| s == name),
        }
    }
}

/// One compiled shell rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellRule {
    /// As written, for the reason a decision reports.
    pub entry: String,
    /// One pattern per leading word.
    pub command: Vec<glob::Pattern>,
    /// Patterns every remaining word must satisfy, or `None` for any.
    /// Kept as text because a path pattern is expanded against the run's
    /// workdir at decision time.
    pub args: Option<Vec<String>>,
}

impl ShellRule {
    fn compile(spec: &ShellRuleSpec) -> Result<Self, RuleError> {
        let entry = spec.command.trim().to_string();
        let bad = |reason: String| RuleError {
            entry: entry.clone(),
            reason,
        };
        if entry.is_empty() {
            return Err(bad("an empty command matches nothing".to_string()));
        }
        let command = entry
            .split_whitespace()
            .map(|word| glob::Pattern::new(word).map_err(|e| bad(format!("not a valid glob: {e}"))))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(args) = &spec.args {
            for arg in args {
                if arg.trim().is_empty() {
                    return Err(bad("an empty args pattern matches nothing".to_string()));
                }
                glob::Pattern::new(arg)
                    .map_err(|e| bad(format!("args {arg:?} is not a valid glob: {e}")))?;
            }
        }
        Ok(Self {
            entry,
            command,
            args: spec.args.clone(),
        })
    }
}

/// The compiled lists of one profile, by verdict.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Compiled {
    pub tools: Vec<(Verdict, ToolRule)>,
    pub shell: Vec<(Verdict, ShellRule)>,
}

impl Compiled {
    /// Compile every list of `spec`, in the order a decision consults them:
    /// `deny`, then `ask`, then `allow`, so the first hit is the strictest.
    pub(crate) fn compile(spec: &ProfileSpec) -> Result<Self, RuleError> {
        let mut tools = Vec::new();
        for (verdict, entries) in [
            (Verdict::Deny, &spec.tools.deny),
            (Verdict::Ask, &spec.tools.ask),
            (Verdict::Allow, &spec.tools.allow),
        ] {
            for entry in entries {
                tools.push((verdict, ToolRule::compile(entry)?));
            }
        }
        let mut shell = Vec::new();
        for (verdict, rules) in [
            (Verdict::Deny, &spec.shell.deny),
            (Verdict::Ask, &spec.shell.ask),
            (Verdict::Allow, &spec.shell.allow),
        ] {
            for rule in rules {
                shell.push((verdict, ShellRule::compile(rule)?));
            }
        }
        Ok(Self { tools, shell })
    }
}

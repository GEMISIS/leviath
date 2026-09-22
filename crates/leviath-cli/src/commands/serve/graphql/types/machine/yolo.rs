//! The yolo profiles this machine has configured: what an unattended run
//! waives, rule by rule, and what one of them would decide about a call.

use async_graphql::{Context, Enum, ID, Object, SimpleObject};
use leviath_graphql_derive::mirror;

use super::super::super::scalars::Json;
use super::super::manifest::tools::ToolPermissionPolicy;

/// The resolver state behind the `YoloProfileOutput` type.
///
/// A plain record: every field is read from the file at load, and the one
/// resolver that is not a field of it, [`decide`](YoloProfile::decide), asks
/// the shared decider rather than this.
pub(crate) struct YoloProfile {
    /// `yoloProfile:<name>`.
    pub(crate) id: ID,
    /// The profile's name, as `--yolo=<name>` spells it.
    pub(crate) name: String,
    /// What tools with no explicit rule do.
    pub(crate) default: YoloWaiver,
    /// What happens to the run's own questions.
    pub(crate) questions: YoloHuman,
    /// What happens at blueprint checkpoints.
    pub(crate) checkpoints: YoloHuman,
    /// What happens at the taint gate.
    pub(crate) gate: YoloHuman,
    /// The tools it names, under each verdict.
    pub(crate) tool_rules: YoloToolRules,
    /// The shell lines it names, under each verdict.
    pub(crate) shell_rules: YoloShellRules,
}

/// One named yolo profile, whole.
///
/// The rules themselves rather than a count of them: a profile is a grant of
/// permissions, and a settings screen that showed only how many there were
/// would be describing a document nobody can read.
#[mirror(list)]
#[Object]
impl YoloProfile {
    /// `yoloProfile:<name>`. One file holds the profiles, one profile per
    /// name, so the name is the whole key.
    #[filter(orderable)]
    pub(crate) async fn id(&self) -> ID {
        self.id.clone()
    }

    /// The profile's name, as `--yolo=<name>` spells it.
    #[filter(orderable)]
    async fn name(&self) -> &str {
        &self.name
    }

    /// What tools with no explicit rule do.
    async fn default(&self) -> YoloWaiver {
        self.default
    }

    /// What happens to the run's own questions.
    async fn questions(&self) -> YoloHuman {
        self.questions
    }

    /// What happens at blueprint checkpoints.
    async fn checkpoints(&self) -> YoloHuman {
        self.checkpoints
    }

    /// What happens at the taint gate.
    async fn gate(&self) -> YoloHuman {
        self.gate
    }

    /// The tools it names, under each verdict.
    async fn tool_rules(&self) -> &YoloToolRules {
        &self.tool_rules
    }

    /// The shell lines it names, under each verdict.
    async fn shell_rules(&self) -> &YoloShellRules {
        &self.shell_rules
    }

    /// What this profile would decide about one tool call.
    ///
    /// The same code path `lev yolo test` runs, so the command and the API
    /// cannot disagree about a call. Decides and reports: nothing is run, and
    /// nothing is written.
    ///
    /// `configured` is what the config layers resolve the tool to before this
    /// profile sees it, which is the half a profile may loosen. A configured
    /// `DENY` is terminal and no profile lifts it.
    #[filter(skip)] // a decision is an act of asking, not a property to select on
    async fn decide(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The tool the model would call.")] tool: String,
        #[graphql(desc = "Where that tool comes from, which is what a `@group` token matches on.")]
        kind: ToolKind,
        #[graphql(desc = "The call's arguments. The shell reads its line from `command` here.")]
        args: Option<Json>,
    ) -> async_graphql::Result<YoloDecision> {
        use super::super::super::error::IntoGraphql;

        let state = ctx.data_unchecked::<super::super::super::super::types::AppState>();
        let called = tool.clone();
        let decided = super::super::super::super::yolo::decided(
            state,
            super::super::super::super::yolo::TestReq {
                profile: self.name.clone(),
                tool,
                command: None,
                arguments: args.map(|json| json.0),
                workdir: None,
                configured: None,
                kind: Some(kind.wire().to_string()),
                allowed: false,
            },
        )
        .gql()?;
        Ok(YoloDecision {
            tool: called,
            configured: policy_of(&decided, "configured"),
            policy: policy_of(&decided, "policy"),
            reason: decided
                .get("reason")
                .and_then(|value| value.as_str())
                .map(str::to_string),
        })
    }
}

impl super::super::super::connection::Paged for YoloProfile {
    const NAME: &'static str = "YoloProfile";
}

/// One policy word out of the decision the command and the API share.
///
/// The shared decider answers in the shape `lev yolo test --json` prints, so
/// this reads that shape rather than a second one built for here. A word that
/// is not one of the three reads as `ASK`, exactly as the dispatcher reads it.
fn policy_of(decided: &serde_json::Value, key: &str) -> ToolPermissionPolicy {
    ToolPermissionPolicy::of(
        decided
            .get(key)
            .and_then(|value| value.as_str())
            .unwrap_or_default(),
    )
}

/// What a profile does with a tool no rule names.
///
/// Two values, not three: a profile waives prompts, it never adds a refusal.
/// A tool a profile does not reach is decided by the config's own permissions,
/// which can still deny it.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum YoloWaiver {
    /// Runs without asking.
    Allow,
    /// Stops and asks, as it would with no profile.
    Ask,
}

/// Whether one human-in-the-loop mechanism still reaches a person.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum YoloHuman {
    /// Reaches a person and waits.
    Ask,
    /// Answers itself and carries on.
    Auto,
}

/// The tools a profile names, under each verdict.
///
/// Each entry is a tool name, a glob over one, or a `@group` token. A name is
/// matched under every spelling the tool answers to, so a rule written for one
/// spelling covers the others.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct YoloToolRules {
    /// Runs without asking.
    pub(crate) allow: Vec<String>,
    /// Goes to a person, as it would with no profile.
    pub(crate) ask: Vec<String>,
    /// Refused at dispatch; nobody is asked.
    pub(crate) deny: Vec<String>,
}

/// The shell lines a profile names, under each verdict.
///
/// Consulted strictest first, so a line that two lists cover is denied rather
/// than allowed.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct YoloShellRules {
    /// Runs without asking.
    pub(crate) allow: Vec<ShellRule>,
    /// Goes to a person.
    pub(crate) ask: Vec<ShellRule>,
    /// Refused at dispatch.
    pub(crate) deny: Vec<ShellRule>,
}

/// One shell rule as the profile writes it.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ShellRule {
    /// The leading words, one glob per word: `rm -r*`, `git push*`, `cargo *`.
    pub(crate) command: String,
    /// Globs every remaining word has to satisfy. Null means any remaining
    /// words; an empty list means none.
    pub(crate) args: Option<Vec<String>>,
}

impl ShellRule {
    /// Read one rule as the file spells it.
    pub(crate) fn from_spec(spec: &crate::yolo::rules::ShellRuleSpec) -> Self {
        Self {
            command: spec.command.clone(),
            args: spec.args.clone(),
        }
    }
}

impl YoloProfile {
    /// Describe one profile the file holds.
    pub(crate) fn from_profile(profile: &crate::yolo::YoloProfile) -> Self {
        let spec = &profile.spec;
        let rules = |rules: &[crate::yolo::rules::ShellRuleSpec]| {
            rules.iter().map(ShellRule::from_spec).collect()
        };
        Self {
            id: super::super::super::node::yolo_profile_id(&profile.name),
            name: profile.name.clone(),
            default: waiver_of(spec.default),
            questions: human_of(spec.questions),
            checkpoints: human_of(spec.checkpoints),
            gate: human_of(spec.gate),
            tool_rules: YoloToolRules {
                allow: spec.tools.allow.clone(),
                ask: spec.tools.ask.clone(),
                deny: spec.tools.deny.clone(),
            },
            shell_rules: YoloShellRules {
                allow: rules(&spec.shell.allow),
                ask: rules(&spec.shell.ask),
                deny: rules(&spec.shell.deny),
            },
        }
    }
}

/// What a profile's default does, in the word the config file uses.
pub(crate) fn waiver_of(waiver: crate::yolo::rules::Waiver) -> YoloWaiver {
    match waiver {
        crate::yolo::rules::Waiver::Allow => YoloWaiver::Allow,
        crate::yolo::rules::Waiver::Ask => YoloWaiver::Ask,
    }
}

/// Whether a human-in-the-loop mechanism reaches a person, in the same words.
pub(crate) fn human_of(human: crate::yolo::rules::Human) -> YoloHuman {
    match human {
        crate::yolo::rules::Human::Ask => YoloHuman::Ask,
        crate::yolo::rules::Human::Auto => YoloHuman::Auto,
    }
}

/// Where a called tool comes from, which is what a `@group` token matches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum ToolKind {
    /// One of the tools this build ships.
    Builtin,
    /// A subagent a blueprint declares.
    Subagent,
    /// A registered Rhai script.
    Script,
    /// A tool an MCP server advertises.
    Mcp,
}

impl ToolKind {
    /// The word the shared decider reads this kind as.
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Subagent => "subagent",
            Self::Script => "script",
            Self::Mcp => "mcp",
        }
    }
}

/// What a yolo profile would do with one call.
///
/// Read and never filtered: a decision is what one call asked about one
/// profile, so there is no listing of them to select from.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct YoloDecision {
    /// The tool the call was for.
    pub(crate) tool: String,
    /// What the config layers resolve the tool to, before the profile.
    pub(crate) configured: ToolPermissionPolicy,
    /// What the profile makes of it.
    pub(crate) policy: ToolPermissionPolicy,
    /// Why, in words, when the decision has a reason to give.
    pub(crate) reason: Option<String>,
}

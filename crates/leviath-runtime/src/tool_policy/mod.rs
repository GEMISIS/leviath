//! Which tool calls run without asking: the permission layers, and how they
//! settle on one policy for a call.
//!
//! Four layers name a policy for a tool: the launch's `allow` list, the
//! stage's and the graph's own `tool_permissions`, and the user's
//! `[tool_permissions]` (with `[agent_tool_permissions.<name>]` on top for
//! an installed blueprint). [`resolve_policy`] is how they combine, and it is
//! the one place that knows who may loosen what: the daemon's tool lane, its
//! spawn-time seeds and an embedder's own checks all ask it.
//!
//! Beside them sits the safe list: the keys a run may call without asking
//! where its policy says `ask`. [`resolve_safe_keys`] is the one place that
//! knows which of its layers count for a run.
//!
//! [`ToolPermissions`] is the user's half of all of it, as their
//! `config.toml` holds it, and [`RunPermissions`] is every layer of one run.
//! The rules decided from them before a run starts are here too, so a program
//! that embeds the runtime gets the answer the daemon gives:
//! [`check_validator_tools`] is the rule that a stage's validator tool must
//! simply run.
//!
//! # What the layers do not hold
//!
//! A call is also held to what only a running host knows: what a shell line
//! runs, writes and reads, the write paths and write budget in force, the
//! files a run may not change, and a named yolo profile. Those are the
//! host's to judge. [`RunPermissions::verdict`] decides a call from the
//! layers alone, and [`validator_tool_refusals`] takes the verdict from
//! whoever can give the fuller one.

use std::collections::{HashMap, HashSet};

use leviath_core::interaction::GrantKeys;
use leviath_core::policy::ToolPolicy;
use leviath_providers::ToolCall;

use crate::pipeline::ToolVerdict;
use crate::spec::graph::RunGraph;
use crate::spec::launch::{LaunchPolicy, Unattended};
use crate::spec::names::ToolName;
use crate::spec::run_spec::{RunSpec, ToolSource};

mod safe;
mod validator;

pub use safe::{
    AgentSafeCommandSettings, DEFAULT_SAFE_SHELL, SHELL_KEY_PREFIX, SafeCommandSettings,
    SafeSource, approval_key, resolve_safe_keys,
};
pub use validator::{check_validator_tools, validator_tool_refusals};

/// Default policy for a tool: read-only builtins are allowed, mutating ones ask,
/// human-in-the-loop tools are always allowed, and anything else requires
/// approval.
pub fn default_tool_policy(tool_name: &str, is_builtin: bool) -> ToolPolicy {
    // Matched on the canonical name, so the `shell` arm covers a call named
    // `bash` and vice versa.
    match leviath_tools::canonical_tool_name(tool_name) {
        "read_file" | "read_files" | "list_dir" | "search_files" | "glob" => ToolPolicy::Allow,
        // The context tools write the agent's own context regions, not the
        // filesystem. Left to fall through to `Ask` below, a run that keeps
        // notes with them would pay a prompt per note, none of which a person
        // can act on.
        "context_write" | "context_append" | "context_read" | "context_delete" | "context_list"
        | "context_attach" | "context_export"
        // The same reasoning for the checklist tools: they write to the agent's
        // own context and touch nothing outside it, and prompting per item
        // would make tracking work cost more than not tracking it.
        | "todo_add" | "todo_done" | "todo_note" => ToolPolicy::Allow,
        // The environment tools report the clock, the machine, the locale, the
        // paths and the run's own state. They read nothing the agent could not
        // already infer from a `shell` call it was granted, mutate nothing, and
        // reach no network. Prompting for the date would make grounding a run
        // in the present cost an approval, which is how it would end up unused.
        //
        // `environment_info` is on this list despite naming environment
        // variables because it answers to `[security] allow_env_vars` for their
        // *values*: a credential-shaped name comes back listed, not read.
        "current_time" | "system_info" | "locale_info" | "environment_info" | "which_command"
        | "runtime_info" => ToolPolicy::Allow,
        // The installers are listed rather than left to the fall-through because
        // it is the persistence primitive: an unprompted install puts
        // model-authored code in front of every future run on this machine.
        "write_file" | "edit_file" | "shell" | "install_self_tool" | "install_global_tool" => {
            ToolPolicy::Ask
        }
        // The sub-agent tools default to `Allow`, and the point of routing them
        // through this function at all is the *config*, not the prompt.
        //
        // If they skipped policy resolution entirely, a user's
        // `[tool_permissions] spawn_agent = "deny"` would be silently ignored -
        // the "a configured deny is terminal" guarantee would not cover these
        // five names. That is the hole worth closing, and it is closed by being
        // here.
        //
        // Defaulting them to `Ask` instead would change what working agents do:
        // every fan-out would stop on a prompt, and an unattended run would
        // block on an approval nothing is there to give. A child named by
        // blueprint is one the user installed, a child the model writes as a
        // graph is asked about separately (below) and gets no per-agent
        // grants, the tree is depth-capped, and children inherit
        // `--no-seed-commands`, so the `Allow` default keeps working agents
        // working while making the user's own setting count. Set
        // `spawn_agent = "ask"` to be prompted.
        "spawn_agent" | "check_agent" | "wait_for_agent" | "send_to_agent" | "kill_agent" => {
            ToolPolicy::Allow
        }
        // These read and start nothing: the request schema, an installed
        // blueprint's declared inputs, a dry run of a spawn, and the history
        // of a run in the caller's own tree.
        "spawn_schema" | "describe_blueprint" | "validate_spawn" | "run_history" => {
            ToolPolicy::Allow
        }
        // A graph the model wrote itself can declare its own seed commands
        // and MCP servers, which run on the host when the child starts. An
        // installed blueprint is one a person chose; a raw graph is not, so a
        // person is asked unless they said otherwise.
        leviath_tools::SPAWN_RAW_GRAPH_PERMISSION => ToolPolicy::Ask,
        // A shell line naming a path outside the workdir is not pre-approved
        // by the safe list; it asks, unless configured to be refused.
        leviath_tools::OUTSIDE_WORKDIR_PERMISSION => ToolPolicy::Ask,
        // These tools ARE the human-in-the-loop mechanism - gating them behind
        // a separate tool-approval prompt would mean asking the user "may I
        // ask you something?" before actually asking them.
        "ask_user_text" | "ask_user_choice" | "ask_user_confirm" | "edit_document" => {
            ToolPolicy::Allow
        }
        _ => {
            // All other tools (built-in or MCP) default to Ask
            let _ = is_builtin;
            ToolPolicy::Ask
        }
    }
}

/// How restrictive a policy is, for clamping. `Allow` < `Ask` < `Deny`.
pub fn restrictiveness(p: ToolPolicy) -> u8 {
    match p {
        ToolPolicy::Allow => 0,
        ToolPolicy::Ask => 1,
        ToolPolicy::Deny => 2,
    }
}

/// The more restrictive of two policies.
pub fn stricter(a: ToolPolicy, b: ToolPolicy) -> ToolPolicy {
    if restrictiveness(b) > restrictiveness(a) {
        b
    } else {
        a
    }
}

/// Whether a call is a `spawn_agent` that carries a whole graph of its own.
pub fn spawns_a_raw_graph(tool_name: &str, arguments: &serde_json::Value) -> bool {
    tool_name == "spawn_agent"
        && arguments
            .get("source")
            .and_then(|s| s.get("graph"))
            .is_some()
}

/// Clamp a `spawn_agent` call that carries a whole graph by the
/// `spawn_raw_graph` permission as well as its own: it can only come out
/// stricter.
///
/// `raw_policy` resolves `spawn_raw_graph` through the same layers as any
/// tool, and is asked only for a call that needs it.
pub fn clamp_raw_graph(
    tool_name: &str,
    arguments: &serde_json::Value,
    policy: ToolPolicy,
    raw_policy: &dyn Fn() -> ToolPolicy,
) -> ToolPolicy {
    match spawns_a_raw_graph(tool_name, arguments) {
        true => stricter(policy, raw_policy()),
        false => policy,
    }
}

/// Tools a blueprint may declare *more* permissively than the built-in default
/// without the user opting in.
///
/// Without this list any tool the user had not configured would be loosenable,
/// and saying nothing is the normal state: nobody writes `shell = "ask"` into
/// their config, because that is already the default. An `agent.toml` from
/// `lev add` could then give itself `shell = "allow"` on a stock machine,
/// which is the opposite of what SECURITY.md promises.
///
/// The justification for allowing *any* loosening is real but narrow: a
/// shipped agent should be able to pre-approve the tools that are its whole
/// point, so the researcher does not prompt for every page it reads. Checking
/// the bundled agents, the only policies any of them loosens relative to
/// the default are these two - the rest of their `[tool_permissions]` lines
/// are `ask`, or `allow` on tools that already default to `allow`.
///
/// An allowlist rather than a denylist of dangerous tools: a denylist has to
/// be complete to be correct, and loses the moment a new tool ships.
///
/// Anything else needs the user to say so: `[security]
/// allow_blueprint_permissions` for every agent, or naming the tool under
/// `[agent_tool_permissions.<name>]`, which makes it a ceiling that agent's
/// blueprint may go up to. Same shape `[read_paths]` and `[safe_commands]`
/// already use, where declaring is not granting.
pub const BLUEPRINT_LOOSENABLE: &[&str] = &["web_search", "web_fetch"];

/// Whether [`BLUEPRINT_LOOSENABLE`] names this tool, under any of its spellings.
pub fn blueprint_loosenable(tool_name: &str) -> bool {
    leviath_tools::tool_name_spellings(tool_name).any(|n| BLUEPRINT_LOOSENABLE.contains(&n))
}

/// Resolve the effective policy for a tool call.
///
/// Scope order is narrowest-first - stage, then agent, then the user's global
/// config, then the built-in default - but *narrower does not mean stronger*.
/// The stage and graph layers come out of the blueprint, which for any agent
/// installed with `lev add` is a file the user downloaded. So a blueprint may
/// only ever **tighten** what the user configured, never loosen it: whatever the
/// user explicitly wrote in `[tool_permissions]` is a ceiling on how permissive
/// a blueprint can be for that tool.
///
/// Only an *explicitly configured* global entry acts as a ceiling. For a tool
/// the user has said nothing about there is no ceiling to clamp against, and
/// what a blueprint may do then is bounded by `BLUEPRINT_LOOSENABLE` rather
/// than unbounded - see there for why.
///
/// A user who wants to grant one specific agent more than their global setting
/// says so in their own config, keyed by agent name - see
/// [`ToolPermissions::for_agent`], which is what `global_permissions` holds
/// for a run.
///
/// `launch_overrides` (the launch's `allow` list) come from the person at the
/// terminal, so they may relax `Ask` to `Allow`. They may **not** override
/// a `Deny`: a denied tool stays denied under `--yolo`, matching the guarantee
/// other agent runtimes make about their deny rules. To lift a `Deny`, edit the
/// config that set it.
pub fn resolve_policy(
    tool_name: &str,
    is_builtin: bool,
    launch_overrides: &HashMap<String, ToolPolicy>,
    stage_permissions: &HashMap<String, String>,
    agent_permissions: &HashMap<String, String>,
    global_permissions: &HashMap<String, ToolPolicy>,
    blueprint_may_loosen: bool,
) -> ToolPolicy {
    let ceiling = by_any_spelling(global_permissions, tool_name).copied();

    // Blueprint layers: stage over agent, each clamped by the user's ceiling.
    let blueprint = by_any_spelling(stage_permissions, tool_name)
        .or_else(|| by_any_spelling(agent_permissions, tool_name))
        .map(|s| parse_policy_str(s));

    let configured = match (blueprint, ceiling) {
        (Some(b), Some(c)) => stricter(b, c),
        // No ceiling to clamp against, so the built-in default is the floor a
        // blueprint may not sink below unless the tool is one it is allowed to
        // pre-approve, or the user opted this blueprint in.
        (Some(b), None) => {
            let default = default_tool_policy(tool_name, is_builtin);
            match blueprint_may_loosen || blueprint_loosenable(tool_name) {
                true => b,
                false => stricter(b, default),
            }
        }
        (None, Some(c)) => c,
        (None, None) => default_tool_policy(tool_name, is_builtin),
    };

    // A `Deny` is terminal - no launch flag lifts it.
    if configured == ToolPolicy::Deny {
        return ToolPolicy::Deny;
    }

    by_any_spelling(launch_overrides, tool_name)
        .or_else(|| launch_overrides.get("*"))
        .copied()
        .unwrap_or(configured)
}

/// Whether the launch's `allow` list named this tool for the run, under any
/// of its spellings.
///
/// The one launch override a yolo profile defers to: a person who typed
/// `--allow web_fetch` answered that question at the terminal, so a profile's
/// `ask` list does not put it back to them. A `deny` still wins.
pub fn launch_allows(launch_overrides: &HashMap<String, ToolPolicy>, tool_name: &str) -> bool {
    by_any_spelling(launch_overrides, tool_name) == Some(&ToolPolicy::Allow)
}

/// Look a tool up in a permission map under any name that refers to it.
///
/// Policy is matched against the name the *model* calls, which is always the
/// canonical one (`shell`), while a blueprint, a config file, or a `--allow` flag
/// may write an alias (`bash`). Matching only the name as called would leave
/// every `bash` entry dead: `[tool_permissions] bash = "allow"` would grant
/// nothing and `lev run --allow bash` would do nothing, because neither key
/// would ever be asked for.
fn by_any_spelling<'a, V>(map: &'a HashMap<String, V>, tool_name: &str) -> Option<&'a V> {
    leviath_tools::tool_name_spellings(tool_name).find_map(|name| map.get(name))
}

/// Whether the graph itself says `allow` for this tool, the way
/// [`resolve_policy`] reads it: the stage's entry over the graph's own.
pub fn graph_says_allow(
    stage_permissions: &HashMap<String, String>,
    graph_permissions: &HashMap<String, String>,
    tool_name: &str,
) -> bool {
    by_any_spelling(stage_permissions, tool_name)
        .or_else(|| by_any_spelling(graph_permissions, tool_name))
        .is_some_and(|s| parse_policy_str(s) == ToolPolicy::Allow)
}

/// Whether a graph's `allow` can take effect for this tool: the user has set
/// no ceiling of their own for it, and either `[security]
/// allow_blueprint_permissions` is on or [`BLUEPRINT_LOOSENABLE`] names it.
pub fn graph_may_allow(
    global_permissions: &HashMap<String, ToolPolicy>,
    tool_name: &str,
    blueprint_may_loosen: bool,
) -> bool {
    !user_sets(global_permissions, tool_name)
        && (blueprint_may_loosen || blueprint_loosenable(tool_name))
}

/// Whether the user's own permissions (global, with the agent's on top) set
/// this tool, which makes their value a ceiling a graph cannot go above.
pub fn user_sets(global_permissions: &HashMap<String, ToolPolicy>, tool_name: &str) -> bool {
    by_any_spelling(global_permissions, tool_name).is_some()
}

/// A blueprint's policy string as a [`ToolPolicy`].
///
/// The fallback is defensive rather than load-bearing: the blueprint reader
/// refuses a spelling that is not `allow`/`ask`/`deny`, so the only string that
/// reaches the last arm is `ask` itself. A reader that let any other word
/// through would have a misspelled `deny` resolve to `ask`, the more
/// permissive of the two, which a grant or `--yolo` could then approve.
pub fn parse_policy_str(s: &str) -> ToolPolicy {
    match s.to_lowercase().as_str() {
        "allow" => ToolPolicy::Allow,
        "deny" => ToolPolicy::Deny,
        _ => ToolPolicy::Ask,
    }
}

/// What a call to a tool the layers deny is answered with. It says what
/// lifts the deny: a run re-reads its permissions when it resumes, so the
/// message names an edit and a resume, not a cancel and a fresh run.
pub fn denied_refusal(tool_name: &str) -> String {
    format!(
        "[denied] Tool '{tool_name}' is not permitted. To allow it, set it to \"allow\" or \
         \"ask\" under [tool_permissions] in config.toml and resume this run \
         (`lev resume`); the run re-reads them and does not need restarting."
    )
}

/// The user's settings that decide which tool calls run without asking, as
/// their `config.toml` holds them.
///
/// `lev` copies these out of its own config; a program that embeds the
/// runtime reads them with
/// [`load_tool_permissions`](crate::user_creds::load_tool_permissions), or
/// builds them itself. The default is a machine whose user has set nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolPermissions {
    /// `[tool_permissions]`: a policy by tool name. Each entry overrides the
    /// built-in default and is a ceiling a graph's own `tool_permissions` may
    /// tighten and never loosen.
    pub tool_permissions: HashMap<String, ToolPolicy>,
    /// `[agent_tool_permissions.<name>]`: the same, for runs of one installed
    /// blueprint. An entry replaces the global value for that blueprint and
    /// is then the ceiling its graph is held to.
    pub agent_tool_permissions: HashMap<String, HashMap<String, ToolPolicy>>,
    /// `[security] allow_blueprint_permissions`: whether a graph's own
    /// `tool_permissions` may go above the built-in default for a tool the
    /// user has not set.
    pub allow_blueprint_permissions: bool,
    /// `[safe_commands]`: what runs without a prompt where its policy says
    /// `ask`.
    pub safe_commands: SafeCommandSettings,
    /// `[agent_safe_commands.<name>]`: the same, added for runs of one
    /// installed blueprint.
    pub agent_safe_commands: HashMap<String, AgentSafeCommandSettings>,
    /// `[security] allow_blueprint_safe_commands`: whether every graph's own
    /// `safe_commands` count.
    pub allow_blueprint_safe_commands: bool,
}

impl ToolPermissions {
    /// The ceiling a run of the blueprint `agent` is held to: the global
    /// `[tool_permissions]` with that blueprint's
    /// `[agent_tool_permissions.<name>]` entries laid over it. `None`, a
    /// graph its caller wrote, gets the global table only: per-agent grants
    /// name blueprints the user installed, never a name a caller chose.
    pub fn for_agent(&self, agent: Option<&str>) -> HashMap<String, ToolPolicy> {
        let mut merged = self.tool_permissions.clone();
        if let Some(per_agent) = agent.and_then(|name| self.agent_tool_permissions.get(name)) {
            merged.extend(per_agent.iter().map(|(k, v)| (k.clone(), *v)));
        }
        merged
    }
}

/// A tool policy as the permission layers write it.
fn policy_word(policy: ToolPolicy) -> String {
    match policy {
        ToolPolicy::Allow => "allow",
        ToolPolicy::Ask => "ask",
        ToolPolicy::Deny => "deny",
    }
    .to_string()
}

/// A graph's or a stage's `tool_permissions`, as the layers hold them.
fn permissions(
    table: &std::collections::BTreeMap<ToolName, ToolPolicy>,
) -> HashMap<String, String> {
    table
        .iter()
        .map(|(tool, policy)| (tool.to_string(), policy_word(*policy)))
        .collect()
}

/// Every permission layer of one run: what [`resolve_policy`] is asked with
/// for a call from any of its stages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunPermissions {
    /// The launch's `allow` list, each tool at `allow`.
    pub launch: HashMap<String, ToolPolicy>,
    /// Each stage's `tool_permissions`, by the stage's position in the graph.
    pub stages: Vec<HashMap<String, String>>,
    /// The graph's own `tool_permissions`.
    pub graph: HashMap<String, String>,
    /// The user's ceiling for this run: [`ToolPermissions::for_agent`] for
    /// the blueprint it came from.
    pub user: HashMap<String, ToolPolicy>,
    /// `[security] allow_blueprint_permissions`.
    pub blueprint_may_loosen: bool,
    /// The run's safe keys ([`resolve_safe_keys`]): a call whose approval
    /// key is here runs where the layers say `ask`.
    pub safe: HashSet<String>,
}

impl RunPermissions {
    /// The layers of a run of `graph` launched under `launch`, on a machine
    /// whose user holds this run to `user` and has
    /// `allow_blueprint_permissions` set as `blueprint_may_loosen` says.
    /// Its safe list starts empty: [`of`](Self::of) fills it from the user's
    /// settings.
    pub fn new(
        graph: &RunGraph,
        launch: &LaunchPolicy,
        user: HashMap<String, ToolPolicy>,
        blueprint_may_loosen: bool,
    ) -> Self {
        Self {
            launch: launch
                .allow
                .iter()
                .map(|t| (t.to_string(), ToolPolicy::Allow))
                .collect(),
            stages: graph
                .stages
                .iter()
                .map(|s| permissions(&s.tool_permissions))
                .collect(),
            graph: permissions(&graph.tool_permissions),
            user,
            blueprint_may_loosen,
            safe: HashSet::new(),
        }
    }

    /// The layers of a resolved run on a machine with the user's `settings`.
    /// The per-agent tables are looked up under the blueprint the run came
    /// from; a graph its caller wrote has none, whatever it is titled.
    ///
    /// The safe list holds the tool names that are safe for the run: the
    /// user's, the blueprint's per-agent ones, and the graph's own where the
    /// user opted in. Its shell half is left to a host that reads shell
    /// lines.
    pub fn of(spec: &RunSpec, settings: &ToolPermissions) -> Self {
        let agent = spec.origin.blueprint_name();
        let safe = settings.safe_keys(agent, Some(&spec.graph.safe_commands), None);
        Self {
            safe: safe.into_keys().collect(),
            ..Self::new(
                &spec.graph,
                &spec.launch,
                settings.for_agent(agent),
                settings.allow_blueprint_permissions,
            )
        }
    }

    /// The policy the layers settle on for a call to `tool_name` from the
    /// stage at `stage_index`. A stage the graph does not have has no
    /// permissions of its own.
    pub fn resolve(&self, stage_index: usize, tool_name: &str, is_builtin: bool) -> ToolPolicy {
        let no_stage = HashMap::new();
        resolve_policy(
            tool_name,
            is_builtin,
            &self.launch,
            self.stages.get(stage_index).unwrap_or(&no_stage),
            &self.graph,
            &self.user,
            self.blueprint_may_loosen,
        )
    }

    /// What the layers alone make of `call`, made from the stage of `spec`
    /// at `stage_index`: run it, refuse it, ask a person, or (for a tool
    /// that is itself a question) put it to one.
    ///
    /// This is the part of the daemon's decision that needs no running
    /// daemon. A run launched fully unattended ([`Unattended::All`]) has
    /// every `ask` waived, a call whose [`approval_key`] is on the run's
    /// safe list runs where the layers say `ask`, and a deny stands whatever
    /// the launch and the safe list say. What it leaves out is what only a
    /// host knows (see the [module docs](self)), so a named yolo profile
    /// changes nothing here, a shell line is judged as the `shell` tool and
    /// not by what it runs, and the verdict charges no bytes and names no
    /// grant keys.
    pub fn verdict(&self, spec: &RunSpec, stage_index: usize, call: &ToolCall) -> ToolVerdict {
        let stage = spec.graph.stages.get(stage_index);
        if crate::dynamic_interaction::BLOCKING_INTERACTION_TOOLS.contains(&call.name.as_str()) {
            let asked = leviath_tools::canonical_tool_name(&call.name);
            let kept = stage.is_some_and(|s| {
                s.required_tools
                    .iter()
                    .any(|t| leviath_tools::canonical_tool_name(t.as_str()) == asked)
            });
            return ToolVerdict::Interact {
                attended: !spec.auto_answers.questions || kept,
            };
        }
        let is_builtin = stage
            .and_then(|s| spec.stage(s.name.as_str()))
            .and_then(|plan| plan.tools.iter().find(|t| t.name.as_str() == call.name))
            .is_some_and(|t| t.source == ToolSource::Builtin);
        let policy = self.resolve(stage_index, &call.name, is_builtin);
        let policy = clamp_raw_graph(&call.name, &call.arguments, policy, &|| {
            self.resolve(
                stage_index,
                leviath_tools::SPAWN_RAW_GRAPH_PERMISSION,
                false,
            )
        });
        let policy = match (&spec.launch.unattended, policy) {
            (Unattended::All, ToolPolicy::Ask) => ToolPolicy::Allow,
            (_, policy) => policy,
        };
        // Covered when the call's key is on the safe list. Only ever an
        // `ask` into an `allow`: a deny is not the safe list's to lift.
        let safe =
            approval_key(&call.name, &call.arguments).is_some_and(|key| self.safe.contains(&key));
        let policy = match policy {
            ToolPolicy::Ask if safe => ToolPolicy::Allow,
            other => other,
        };
        match policy {
            ToolPolicy::Deny => ToolVerdict::Refuse(denied_refusal(&call.name)),
            ToolPolicy::Ask => ToolVerdict::Ask {
                keys: GrantKeys::default(),
                charge: 0,
            },
            ToolPolicy::Allow => ToolVerdict::Run { charge: 0 },
        }
    }
}

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use leviath_core::policy::ToolPolicy;

use super::*;
use crate::spec::graph::{OutputDef, ValidatorToolDef};
use crate::spec::launch::Unattended;
use crate::spec::names::{McpServerName, StageName, ToolName};
use crate::spec::run_spec::{AutoAnswers, SpecOrigin, ToolDef, ToolSource};

/// An MCP server's tool, as a stage that judges its answers by one names it.
const MCP: &str = "polywright__check_design";

fn tool(name: &str) -> ToolName {
    ToolName::new(name).unwrap()
}

fn given(name: &str, source: ToolSource) -> ToolDef {
    ToolDef {
        name: tool(name),
        description: String::new(),
        schema: Default::default(),
        source,
    }
}

/// An attended run of the `coder` blueprint whose `plan` stage submits and
/// is judged by `judge`, with the graph saying `graph_says` for it.
fn judged(judge: &str, graph_says: Option<ToolPolicy>) -> RunSpec {
    let mut spec = crate::spec::run_spec::tests::spec();
    spec.launch.unattended = Unattended::Off;
    spec.auto_answers = AutoAnswers::default();
    spec.graph.tool_permissions = graph_says
        .map(|policy| (tool(judge), policy))
        .into_iter()
        .collect();
    let server = ToolSource::Mcp {
        server: McpServerName::new("polywright").unwrap(),
        tool: "check_design".into(),
    };
    spec.stages[0].tools.push(given(judge, server));
    spec.stages[0].tools.push(given(
        leviath_tools::SUBMIT_OUTPUT_TOOL,
        ToolSource::StageControl,
    ));
    spec.stages[0].output = Some(OutputDef {
        validator_tool: Some(ValidatorToolDef {
            tool: tool(judge),
            content_arg: "content".into(),
            args: leviath_core::JsonDoc::new(serde_json::json!({})),
        }),
        ..OutputDef::default()
    });
    spec
}

/// A machine whose user has set nothing.
fn stock() -> ToolPermissions {
    ToolPermissions::default()
}

/// The one refusal `spec` gets on a machine with `settings`, or `None` when
/// its validator tool would run.
fn refusal(spec: &RunSpec, settings: &ToolPermissions) -> Option<SpawnIssue> {
    let mut issues = check_validator_tools(spec, settings).0;
    assert!(issues.len() <= 1, "{issues:?}");
    issues.pop()
}

/// The hint of a refusal for a tool set to `ask`.
fn ask_hint_for(spec: &RunSpec, settings: &ToolPermissions) -> String {
    let issue = refusal(spec, settings).expect("the validator tool would ask");
    assert!(issue.message.contains("set to ask"), "{issue:?}");
    issue.hint.expect("a refusal says what to do")
}

/// The case the rule is here for. A graph's `allow` for an MCP tool is
/// clamped back to ask, so a stage judged by one is refused, at the stage's
/// validator tool, with a hint that does not send the caller to write the
/// line the graph already has. The launch's own `allow` is what lets it run.
#[test]
fn a_clamped_allow_on_an_mcp_validator_is_refused_and_told_what_can_allow_it() {
    let mut spec = judged(MCP, Some(ToolPolicy::Allow));
    let issue = refusal(&spec, &stock()).expect("the allow is clamped");
    assert_eq!(
        issue.path.to_string(),
        "stages.plan.output.validator_tool.tool"
    );
    assert_eq!(issue.code, IssueCode::NotAllowed);
    assert_eq!(
        issue.message,
        format!(
            "the validator tool \"{MCP}\" would not run: it is set to ask, and nobody is there \
             to approve it mid-submission"
        )
    );
    assert_eq!(
        issue.hint,
        Some(format!(
            "`{MCP} = \"allow\"` in tool_permissions has no effect, because a graph cannot \
             allow this tool by itself; name it in this spawn's `launch.allow` (`--allow` on \
             the command line), or set `{MCP} = \"allow\"` under \
             `[agent_tool_permissions.coder]` in the user's config, or turn on `[security] \
             allow_blueprint_permissions` in the user's config, or name a tool the stage may \
             call unprompted"
        ))
    );
    spec.launch.allow.push(tool(MCP));
    assert_eq!(refusal(&spec, &stock()), None);
}

/// The user's opt-ins the hint names each make the validator run, and so
/// does a launch nobody watches at all.
#[test]
fn what_the_hint_names_lets_the_validator_run() {
    let mut spec = judged(MCP, Some(ToolPolicy::Allow));
    let loose = ToolPermissions {
        allow_blueprint_permissions: true,
        ..stock()
    };
    assert_eq!(refusal(&spec, &loose), None);
    let per_agent = ToolPermissions {
        agent_tool_permissions: HashMap::from([(
            "coder".to_string(),
            HashMap::from([(MCP.to_string(), ToolPolicy::Allow)]),
        )]),
        ..stock()
    };
    assert_eq!(refusal(&spec, &per_agent), None);
    spec.launch.unattended = Unattended::All;
    assert_eq!(refusal(&spec, &stock()), None);
}

/// A graph its caller wrote has no blueprint name, so the per-agent table
/// cannot apply to it and is not named, and neither is the switch that only
/// makes sense beside it.
#[test]
fn a_raw_graph_is_not_told_about_the_per_agent_table() {
    let mut spec = judged(MCP, Some(ToolPolicy::Allow));
    spec.origin = SpecOrigin::Raw;
    assert_eq!(
        ask_hint_for(&spec, &stock()),
        format!(
            "`{MCP} = \"allow\"` in tool_permissions has no effect, because a graph cannot \
             allow this tool by itself; name it in this spawn's `launch.allow` (`--allow` on \
             the command line), or name a tool the stage may call unprompted"
        )
    );
}

/// Where nothing says `allow`, a graph's `allow` is not the way out for an
/// MCP tool either; `allow_blueprint_permissions` only lets a graph's own
/// `allow` count, so it is not named without one.
#[test]
fn an_mcp_validator_nothing_allows_is_not_told_to_write_a_dead_line() {
    let spec = judged(MCP, None);
    assert_eq!(
        ask_hint_for(&spec, &stock()),
        format!(
            "a graph cannot allow this tool by itself; name it in this spawn's `launch.allow` \
             (`--allow` on the command line), or set `{MCP} = \"allow\"` under \
             `[agent_tool_permissions.coder]` in the user's config, or name a tool the stage \
             may call unprompted"
        )
    );
}

/// A stage's own `allow` is the graph saying so as much as the graph's is.
#[test]
fn a_stages_own_allow_is_clamped_like_the_graphs() {
    let mut spec = judged(MCP, None);
    spec.graph.stages[0].tool_permissions = [(tool(MCP), ToolPolicy::Allow)].into();
    let hint = ask_hint_for(&spec, &stock());
    assert!(
        hint.starts_with(&format!(
            "`{MCP} = \"allow\"` in tool_permissions has no effect"
        )),
        "{hint}"
    );
    assert!(hint.contains("allow_blueprint_permissions"), "{hint}");
}

/// A tool a graph may loosen, with nothing set, keeps the hint to write the
/// line, and writing it works. With the user's own `ask` as a ceiling it
/// does not work, and the hint stops saying so and stops naming the switch
/// that cannot lift a ceiling.
#[test]
fn a_tool_a_graph_may_loosen_is_told_to_allow_it_unless_the_user_set_ask() {
    let asking = judged("web_fetch", Some(ToolPolicy::Ask));
    assert_eq!(
        ask_hint_for(&asking, &stock()),
        "set `web_fetch = \"allow\"` in tool_permissions, or name a tool the stage may call \
         unprompted"
    );
    let allowed = judged("web_fetch", Some(ToolPolicy::Allow));
    assert_eq!(refusal(&allowed, &stock()), None);

    let ceiling = ToolPermissions {
        tool_permissions: HashMap::from([("web_fetch".to_string(), ToolPolicy::Ask)]),
        ..stock()
    };
    assert_eq!(
        ask_hint_for(&allowed, &ceiling),
        "`web_fetch = \"allow\"` in tool_permissions has no effect, because a graph cannot \
         allow this tool by itself; name it in this spawn's `launch.allow` (`--allow` on the \
         command line), or set `web_fetch = \"allow\"` under `[agent_tool_permissions.coder]` \
         in the user's config, or name a tool the stage may call unprompted"
    );
}

/// With the user's opt-in on, even an MCP tool is a graph's to allow, and
/// the hint to write the line is right again.
#[test]
fn with_the_opt_in_on_an_mcp_validator_is_told_to_allow_it() {
    let spec = judged(MCP, None);
    let loose = ToolPermissions {
        allow_blueprint_permissions: true,
        ..stock()
    };
    assert_eq!(
        ask_hint_for(&spec, &loose),
        format!(
            "set `{MCP} = \"allow\"` in tool_permissions, or name a tool the stage may call \
             unprompted"
        )
    );
}

/// A denied tool and a tool that is itself a question are refused too, each
/// saying why, and neither is told about the launch's `allow`, which lifts
/// neither.
#[test]
fn a_denied_validator_and_a_question_are_refused() {
    let denied = judged(MCP, Some(ToolPolicy::Deny));
    let issue = refusal(&denied, &stock()).expect("denied");
    assert_eq!(
        issue.message,
        format!(
            "the validator tool \"{MCP}\" would not run: [denied] Tool '{MCP}' is not permitted"
        )
    );
    assert_eq!(
        issue.hint,
        Some(format!(
            "set `{MCP} = \"allow\"` in tool_permissions, or name a tool the stage may call \
             unprompted"
        ))
    );

    let question = judged("ask_user_text", None);
    let issue = refusal(&question, &stock()).expect("a question");
    assert_eq!(
        issue.message,
        "the validator tool \"ask_user_text\" would not run: it puts a question to a person, \
         which no check can wait on"
    );
    assert_eq!(
        issue.hint.as_deref(),
        Some(
            "set `ask_user_text = \"allow\"` in tool_permissions, or name a tool the stage may \
             call unprompted"
        )
    );
}

/// Only a stage that hands back an answer has it judged: one with no
/// `submit_output`, one that names no validator tool, and a plan for a stage
/// the graph does not have are not asked about.
#[test]
fn a_stage_that_never_submits_is_not_judged() {
    let mut spec = judged(MCP, None);
    spec.stages[0]
        .tools
        .retain(|t| t.name.as_str() != leviath_tools::SUBMIT_OUTPUT_TOOL);
    assert_eq!(refusal(&spec, &stock()), None);

    let mut spec = judged(MCP, None);
    spec.stages[0].output = Some(OutputDef::default());
    assert_eq!(refusal(&spec, &stock()), None);
    spec.stages[0].output = None;
    assert_eq!(refusal(&spec, &stock()), None);

    let mut spec = judged(MCP, None);
    spec.stages[0].stage = StageName::new("elsewhere").unwrap();
    assert_eq!(refusal(&spec, &stock()), None);
}

/// A validator tool on the run's safe list simply runs, so it is not
/// refused: on the user's own list, on the per-agent one of the blueprint
/// the run came from, or on the graph's own once the user opted in. A graph
/// that only declares it, and a graph its caller wrote under a per-agent
/// table, are refused as any other, with the same hint.
#[test]
fn a_safe_listed_validator_tool_is_not_refused() {
    use crate::tool_policy::{AgentSafeCommandSettings, SafeCommandSettings};

    let spec = judged(MCP, None);
    let users = ToolPermissions {
        safe_commands: SafeCommandSettings {
            tools: vec![MCP.to_string()],
            ..SafeCommandSettings::default()
        },
        ..stock()
    };
    assert_eq!(refusal(&spec, &users), None);

    let per_agent = ToolPermissions {
        agent_safe_commands: HashMap::from([(
            "coder".to_string(),
            AgentSafeCommandSettings {
                tools: vec![MCP.to_string()],
                ..AgentSafeCommandSettings::default()
            },
        )]),
        ..stock()
    };
    assert_eq!(refusal(&spec, &per_agent), None);
    let mut raw = judged(MCP, None);
    raw.origin = SpecOrigin::Raw;
    assert!(refusal(&raw, &per_agent).is_some());

    let mut declares = judged(MCP, None);
    declares.graph.safe_commands.tools = vec![tool(MCP)];
    assert_eq!(
        ask_hint_for(&declares, &stock()),
        ask_hint_for(&spec, &stock()),
        "declaring is not granting"
    );
    let opted_in = ToolPermissions {
        allow_blueprint_safe_commands: true,
        ..stock()
    };
    assert_eq!(refusal(&declares, &opted_in), None);
    let trusted = ToolPermissions {
        agent_safe_commands: HashMap::from([(
            "coder".to_string(),
            AgentSafeCommandSettings {
                allow_blueprint: true,
                ..AgentSafeCommandSettings::default()
            },
        )]),
        ..stock()
    };
    assert_eq!(refusal(&declares, &trusted), None);

    // A deny is not the safe list's to lift.
    let denied = judged(MCP, Some(ToolPolicy::Deny));
    assert!(refusal(&denied, &users).is_some());
}

/// A host that knows more about the call gives the verdict itself: it is
/// asked about the tool's call from the stage's place in the graph, a
/// refusal is reported by its first sentence, and a call it lets run passes
/// whatever the layers say.
#[test]
fn a_host_gives_its_own_verdict() {
    let mut spec = judged(MCP, None);
    spec.stages[0].stage = StageName::new("build").unwrap();
    let layers = RunPermissions::of(&spec, &stock());
    let seen = std::cell::RefCell::new(Vec::new());
    let refusing = |text: &'static str| {
        let seen = &seen;
        move |stage_index: usize, call: &ToolCall| {
            seen.borrow_mut()
                .push((stage_index, call.name.clone(), call.arguments.clone()));
            ToolVerdict::Refuse(text.to_string())
        }
    };

    let issues = validator_tool_refusals(
        &spec,
        &layers,
        &refusing("the disk is full. Free some space"),
    );
    assert_eq!(issues.0.len(), 1, "{issues:?}");
    assert_eq!(
        issues.0[0].path.to_string(),
        "stages.build.output.validator_tool.tool"
    );
    assert_eq!(
        issues.0[0].message,
        format!("the validator tool \"{MCP}\" would not run: the disk is full")
    );
    let issues = validator_tool_refusals(&spec, &layers, &refusing("no room"));
    assert_eq!(
        issues.0[0].message,
        format!("the validator tool \"{MCP}\" would not run: no room")
    );
    assert_eq!(
        *seen.borrow(),
        vec![
            (1, MCP.to_string(), serde_json::json!({"content": ""})),
            (1, MCP.to_string(), serde_json::json!({"content": ""})),
        ]
    );

    let running = |_: usize, _: &ToolCall| ToolVerdict::Run { charge: 0 };
    assert!(validator_tool_refusals(&spec, &layers, &running).is_empty());
}

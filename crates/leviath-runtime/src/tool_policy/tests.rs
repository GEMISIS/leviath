use super::*;
use crate::spec::launch::Unattended;
use crate::spec::run_spec::{AutoAnswers, RunSpec, SpecOrigin, ToolDef};

// Named rather than a closure per call site: one function has one
// coverage-mapping instance, so the tests where the resolver is
// deliberately never reached do not each leave an unexecuted body behind.
fn deny() -> ToolPolicy {
    ToolPolicy::Deny
}

// ─── spawn_raw_graph ──────────────────────────────────────────────────────

/// A spawn that carries a graph answers to `spawn_raw_graph` too, which
/// asks by default and can only make the call stricter; a spawn of an
/// installed blueprint, and every other tool, is untouched.
#[test]
fn a_raw_graph_spawn_answers_to_its_own_permission() {
    let raw = serde_json::json!({"source": {"graph": {}}});
    let named = serde_json::json!({"source": {"blueprint": "coder"}});
    assert_eq!(
        default_tool_policy(leviath_tools::SPAWN_RAW_GRAPH_PERMISSION, false),
        ToolPolicy::Ask
    );
    assert!(spawns_a_raw_graph("spawn_agent", &raw));
    assert!(!spawns_a_raw_graph("spawn_agent", &named));
    assert!(!spawns_a_raw_graph("validate_spawn", &raw));
    assert!(!spawns_a_raw_graph("spawn_agent", &serde_json::json!({})));
    let ask = || ToolPolicy::Ask;
    assert_eq!(
        clamp_raw_graph("spawn_agent", &raw, ToolPolicy::Allow, &ask),
        ToolPolicy::Ask
    );
    assert_eq!(
        clamp_raw_graph("spawn_agent", &raw, ToolPolicy::Allow, &deny),
        ToolPolicy::Deny
    );
    assert_eq!(
        clamp_raw_graph("spawn_agent", &raw, ToolPolicy::Deny, &ask),
        ToolPolicy::Deny
    );
    assert_eq!(
        clamp_raw_graph("spawn_agent", &named, ToolPolicy::Allow, &deny),
        ToolPolicy::Allow
    );
}

/// A shell line naming a path outside the workdir answers to a permission
/// of its own, which asks unless somebody set it.
#[test]
fn reading_outside_the_workdir_asks_by_default() {
    assert_eq!(
        default_tool_policy(leviath_tools::OUTSIDE_WORKDIR_PERMISSION, false),
        ToolPolicy::Ask
    );
}

// ─── the user's settings and a run's layers ───────────────────────────────

fn tool(name: &str) -> ToolName {
    ToolName::new(name).unwrap()
}

/// The user's settings with `shell` asked and `write_file` denied globally,
/// and `shell` allowed for the `coder` blueprint.
fn settings() -> ToolPermissions {
    ToolPermissions {
        tool_permissions: HashMap::from([
            ("shell".to_string(), ToolPolicy::Ask),
            ("write_file".to_string(), ToolPolicy::Deny),
        ]),
        agent_tool_permissions: HashMap::from([(
            "coder".to_string(),
            HashMap::from([("shell".to_string(), ToolPolicy::Allow)]),
        )]),
        ..ToolPermissions::default()
    }
}

/// A per-agent entry replaces the global value for that blueprint and no
/// other, and leaves every other tool where the global table put it. A
/// graph its caller wrote gets the global table alone.
#[test]
fn the_named_blueprints_grant_is_laid_over_the_global_table() {
    let settings = settings();
    let coder = settings.for_agent(Some("coder"));
    assert_eq!(coder.get("shell"), Some(&ToolPolicy::Allow), "granted");
    assert_eq!(coder.get("write_file"), Some(&ToolPolicy::Deny));
    let other = settings.for_agent(Some("researcher"));
    assert_eq!(other.get("shell"), Some(&ToolPolicy::Ask));
    assert_eq!(settings.for_agent(None), settings.tool_permissions);
}

/// An attended run of the test spec, whose `plan` stage holds a built-in, an
/// MCP and a script tool, and whose blueprint is `coder`.
fn attended() -> RunSpec {
    let mut spec = crate::spec::run_spec::tests::spec();
    spec.launch.unattended = Unattended::Off;
    spec.auto_answers = AutoAnswers::default();
    spec
}

/// A run's layers are read off its graph and its launch: each stage's own
/// `tool_permissions` by position, the graph's, the `allow` list at
/// `allow`, and the user's table for the blueprint the run came from.
#[test]
fn a_runs_layers_are_its_graphs_its_launchs_and_the_users() {
    let mut spec = attended();
    spec.graph.tool_permissions = [(tool("web_fetch"), ToolPolicy::Deny)].into();
    spec.graph.stages[1].tool_permissions = [
        (tool("web_fetch"), ToolPolicy::Ask),
        (tool("read_file"), ToolPolicy::Allow),
    ]
    .into();
    spec.launch.allow = vec![tool("edit_file")];
    let mut settings = settings();
    settings.allow_blueprint_permissions = true;

    let layers = RunPermissions::of(&spec, &settings);
    assert_eq!(
        layers.launch,
        HashMap::from([("edit_file".to_string(), ToolPolicy::Allow)])
    );
    assert_eq!(layers.stages.len(), 2);
    assert!(layers.stages[0].is_empty());
    assert_eq!(
        layers.stages[1],
        HashMap::from([
            ("web_fetch".to_string(), "ask".to_string()),
            ("read_file".to_string(), "allow".to_string()),
        ])
    );
    assert_eq!(
        layers.graph,
        HashMap::from([("web_fetch".to_string(), "deny".to_string())])
    );
    assert_eq!(layers.user, settings.for_agent(Some("coder")));
    assert!(layers.blueprint_may_loosen);
    assert!(layers.safe.is_empty(), "the user named no safe tool");

    // The stage's entry is read before the graph's, and a stage the graph
    // does not have has only the graph's.
    assert_eq!(layers.resolve(0, "web_fetch", true), ToolPolicy::Deny);
    assert_eq!(layers.resolve(1, "web_fetch", true), ToolPolicy::Ask);
    assert_eq!(layers.resolve(9, "web_fetch", true), ToolPolicy::Deny);
    // The launch's `allow`, the user's per-agent grant, and the user's deny.
    assert_eq!(layers.resolve(0, "edit_file", true), ToolPolicy::Allow);
    assert_eq!(layers.resolve(0, "shell", true), ToolPolicy::Allow);
    assert_eq!(layers.resolve(0, "write_file", true), ToolPolicy::Deny);

    // A graph its caller wrote is held to the global table, whatever it is
    // titled.
    spec.origin = SpecOrigin::Raw;
    let raw = RunPermissions::of(&spec, &settings);
    assert_eq!(raw.user, settings.tool_permissions);
    assert_eq!(raw.resolve(0, "shell", true), ToolPolicy::Ask);
}

// ─── the verdict the layers alone give ────────────────────────────────────

fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "c".to_string(),
        name: name.to_string(),
        arguments,
        thought_signature: None,
    }
}

fn verdict_of(spec: &RunSpec, settings: &ToolPermissions, name: &str) -> ToolVerdict {
    RunPermissions::of(spec, settings).verdict(spec, 0, &call(name, serde_json::json!({})))
}

const RUNS: ToolVerdict = ToolVerdict::Run { charge: 0 };

fn asks() -> ToolVerdict {
    ToolVerdict::Ask {
        keys: GrantKeys::default(),
        charge: 0,
    }
}

/// A call runs, asks or is refused as its policy says, whatever kind of
/// tool it is: a built-in, an MCP server's, a script, or one the stage was
/// never given. A refusal says what lifts it.
#[test]
fn the_layers_decide_a_call_by_its_policy() {
    let spec = attended();
    let none = ToolPermissions::default();
    assert_eq!(verdict_of(&spec, &none, "read_file"), RUNS);
    assert_eq!(verdict_of(&spec, &none, "gh__search"), asks());
    assert_eq!(verdict_of(&spec, &none, "mine"), asks());
    assert_eq!(verdict_of(&spec, &none, "never_given"), asks());

    let denying = ToolPermissions {
        tool_permissions: HashMap::from([("gh__search".to_string(), ToolPolicy::Deny)]),
        ..ToolPermissions::default()
    };
    assert_eq!(
        verdict_of(&spec, &denying, "gh__search"),
        ToolVerdict::Refuse(
            "[denied] Tool 'gh__search' is not permitted. To allow it, set it to \"allow\" or \
             \"ask\" under [tool_permissions] in config.toml and resume this run (`lev \
             resume`); the run re-reads them and does not need restarting."
                .to_string()
        )
    );

    // A stage the graph does not have is decided by the graph's and the
    // user's layers.
    let layers = RunPermissions::of(&spec, &none);
    let stray = call("gh__search", serde_json::json!({}));
    assert_eq!(layers.verdict(&spec, 9, &stray), asks());
}

/// A run launched fully unattended has every `ask` waived and no deny
/// lifted. A named profile's lists are the host's to read, so under one the
/// layers' answer stands.
#[test]
fn a_fully_unattended_launch_waives_an_ask_and_never_a_deny() {
    let mut spec = attended();
    let denying = ToolPermissions {
        tool_permissions: HashMap::from([("mine".to_string(), ToolPolicy::Deny)]),
        ..ToolPermissions::default()
    };
    spec.launch.unattended = Unattended::All;
    assert_eq!(verdict_of(&spec, &denying, "gh__search"), RUNS);
    assert!(matches!(
        verdict_of(&spec, &denying, "mine"),
        ToolVerdict::Refuse(_)
    ));

    spec.launch.unattended =
        Unattended::Profile(crate::spec::names::ProfileName::new("careful").unwrap());
    assert_eq!(verdict_of(&spec, &denying, "gh__search"), asks());
    assert_eq!(verdict_of(&spec, &denying, "read_file"), RUNS);
}

/// A tool that is itself a question is put to a person, whatever its
/// policy: by one when the run's questions reach a person or the stage kept
/// the tool in `required_tools`, and answered by the run itself otherwise.
#[test]
fn a_question_is_a_question_whatever_its_policy() {
    let mut spec = attended();
    let none = ToolPermissions::default();
    assert_eq!(
        verdict_of(&spec, &none, "ask_user_text"),
        ToolVerdict::Interact { attended: true }
    );
    spec.auto_answers = AutoAnswers::all();
    assert_eq!(
        verdict_of(&spec, &none, "ask_user_text"),
        ToolVerdict::Interact { attended: false }
    );
    spec.graph.stages[0].required_tools = vec![tool("read_file"), tool("ask_user_text")];
    assert_eq!(
        verdict_of(&spec, &none, "ask_user_text"),
        ToolVerdict::Interact { attended: true }
    );
    assert_eq!(
        verdict_of(&spec, &none, "ask_user_choice"),
        ToolVerdict::Interact { attended: false }
    );
    // From a stage the graph does not have, nothing was kept.
    let layers = RunPermissions::of(&spec, &none);
    let question = call("ask_user_text", serde_json::json!({}));
    assert_eq!(
        layers.verdict(&spec, 9, &question),
        ToolVerdict::Interact { attended: false }
    );
}

/// A `spawn_agent` call carrying a graph of its own answers to
/// `spawn_raw_graph` as well, through the same layers.
#[test]
fn a_raw_graph_spawn_is_decided_by_both_permissions() {
    let mut spec = attended();
    spec.stages[0].tools.push(ToolDef {
        name: tool("spawn_agent"),
        description: String::new(),
        schema: Default::default(),
        source: ToolSource::Subagent,
    });
    let none = ToolPermissions::default();
    let layers = RunPermissions::of(&spec, &none);
    let named = call(
        "spawn_agent",
        serde_json::json!({"source": {"blueprint": "coder"}}),
    );
    let raw = call("spawn_agent", serde_json::json!({"source": {"graph": {}}}));
    assert_eq!(layers.verdict(&spec, 0, &named), RUNS);
    assert_eq!(layers.verdict(&spec, 0, &raw), asks());

    let allowing = ToolPermissions {
        tool_permissions: HashMap::from([(
            leviath_tools::SPAWN_RAW_GRAPH_PERMISSION.to_string(),
            ToolPolicy::Allow,
        )]),
        ..ToolPermissions::default()
    };
    let layers = RunPermissions::of(&spec, &allowing);
    assert_eq!(layers.verdict(&spec, 0, &raw), RUNS);
}

/// A run's safe list is the tool names that are safe for it: the user's,
/// the per-agent ones of the blueprint it came from, and the graph's own
/// where the user opted in. Layers built without the user's settings have
/// none.
#[test]
fn a_runs_safe_list_is_resolved_from_the_users_settings() {
    let mut spec = attended();
    spec.graph.safe_commands.tools = vec![tool("mine")];
    let settings = ToolPermissions {
        safe_commands: SafeCommandSettings {
            tools: vec!["gh__search".to_string()],
            shell: vec!["rg".to_string()],
            ..SafeCommandSettings::default()
        },
        agent_safe_commands: HashMap::from([(
            "coder".to_string(),
            AgentSafeCommandSettings {
                tools: vec!["edit_file".to_string()],
                ..AgentSafeCommandSettings::default()
            },
        )]),
        ..ToolPermissions::default()
    };
    let named = |layers: &RunPermissions| {
        let mut names: Vec<&str> = layers.safe.iter().map(String::as_str).collect();
        names.sort_unstable();
        names.join(" ")
    };
    assert_eq!(
        named(&RunPermissions::of(&spec, &settings)),
        "edit_file gh__search",
        "no shell key, and nothing the graph only declared"
    );
    let opted_in = ToolPermissions {
        allow_blueprint_safe_commands: true,
        ..settings.clone()
    };
    assert_eq!(
        named(&RunPermissions::of(&spec, &opted_in)),
        "edit_file gh__search mine"
    );
    spec.origin = SpecOrigin::Raw;
    assert_eq!(named(&RunPermissions::of(&spec, &settings)), "gh__search");
    let bare = RunPermissions::new(&spec.graph, &spec.launch, HashMap::new(), false);
    assert!(bare.safe.is_empty());
}

/// A call whose key is on the run's safe list runs where the layers say
/// `ask`, and nowhere else: a deny stands, a shell call is never covered by
/// a tool name, and a spawn that carries a graph is covered only under the
/// raw-graph permission.
#[test]
fn a_safe_tool_runs_where_the_layers_would_ask() {
    let mut spec = attended();
    spec.stages[0].tools.push(ToolDef {
        name: tool("shell"),
        description: String::new(),
        schema: Default::default(),
        source: ToolSource::Builtin,
    });
    let safe = |tools: &[&str]| ToolPermissions {
        safe_commands: SafeCommandSettings {
            tools: tools.iter().map(ToString::to_string).collect(),
            ..SafeCommandSettings::default()
        },
        ..ToolPermissions::default()
    };
    assert_eq!(
        verdict_of(&spec, &safe(&["gh__search"]), "gh__search"),
        RUNS
    );
    assert_eq!(verdict_of(&spec, &safe(&["gh__search"]), "mine"), asks());

    let denied = ToolPermissions {
        tool_permissions: HashMap::from([("gh__search".to_string(), ToolPolicy::Deny)]),
        ..safe(&["gh__search"])
    };
    assert!(matches!(
        verdict_of(&spec, &denied, "gh__search"),
        ToolVerdict::Refuse(_)
    ));

    let layers = RunPermissions::of(&spec, &safe(&["shell", "bash", "spawn_agent"]));
    let line = call("shell", serde_json::json!({"command": "uptime"}));
    assert_eq!(layers.verdict(&spec, 0, &line), asks());
    let raw = call("spawn_agent", serde_json::json!({"source": {"graph": {}}}));
    assert_eq!(layers.verdict(&spec, 0, &raw), asks());
    let layers = RunPermissions::of(&spec, &safe(&[leviath_tools::SPAWN_RAW_GRAPH_PERMISSION]));
    assert_eq!(layers.verdict(&spec, 0, &raw), RUNS);
}

// ─── what a blueprint may loosen ──────────────────────────────────────────

/// One blueprint `tool_permissions` entry, for a tool the user has said
/// nothing about.
fn blueprint_says(tool: &str, policy: &str, may_loosen: bool) -> ToolPolicy {
    let mut agent = HashMap::new();
    agent.insert(tool.to_string(), policy.to_string());
    resolve_policy(
        tool,
        true,
        &HashMap::new(),
        &HashMap::new(),
        &agent,
        &HashMap::new(),
        may_loosen,
    )
}

/// Saying nothing about `shell` is the normal state: nobody writes out a
/// default. So a tool the user never configured is held to its built-in
/// default, or a downloaded blueprint could pre-approve its own shell on a
/// stock machine.
#[test]
fn a_blueprint_cannot_loosen_a_tool_the_user_never_configured() {
    for tool in ["shell", "write_file", "edit_file"] {
        assert_eq!(
            blueprint_says(tool, "allow", false),
            ToolPolicy::Ask,
            "{tool} must fall back to its built-in default"
        );
    }
    // A tool whose default is already `Allow` is not being loosened by a
    // blueprint that says `allow`, so nothing changes for it. The clamp is
    // a floor, not a rule about what may be written.
    assert_eq!(
        blueprint_says("spawn_agent", "allow", false),
        ToolPolicy::Allow
    );
}

/// Tightening is always a blueprint's to do, so one that wants to be more
/// careful than the default can say so.
#[test]
fn a_blueprint_may_still_tighten_anything() {
    assert_eq!(blueprint_says("shell", "deny", false), ToolPolicy::Deny);
    assert_eq!(blueprint_says("read_file", "ask", false), ToolPolicy::Ask);
}

/// The case the loosenable list exists to serve: an agent whose whole point
/// is reading the web should not prompt for every page.
#[test]
fn a_blueprint_may_preapprove_the_read_only_web_tools() {
    assert_eq!(
        blueprint_says("web_fetch", "allow", false),
        ToolPolicy::Allow
    );
    assert_eq!(
        blueprint_says("web_search", "allow", false),
        ToolPolicy::Allow
    );
}

/// The escape hatch, for a blueprint the user does trust.
#[test]
fn an_opted_in_blueprint_may_loosen_anything() {
    assert_eq!(blueprint_says("shell", "allow", true), ToolPolicy::Allow);
}

/// A user-configured ceiling still governs, in both directions: a blueprint
/// may go up to it and no further.
#[test]
fn a_configured_ceiling_still_bounds_a_blueprint() {
    let mut agent = HashMap::new();
    agent.insert("shell".to_string(), "allow".to_string());
    let mut global = HashMap::new();
    global.insert("shell".to_string(), ToolPolicy::Allow);
    assert_eq!(
        resolve_policy(
            "shell",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &agent,
            &global,
            false,
        ),
        ToolPolicy::Allow,
        "naming the tool in the user's own config is the per-agent grant"
    );

    global.insert("shell".to_string(), ToolPolicy::Deny);
    assert_eq!(
        resolve_policy(
            "shell",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &agent,
            &global,
            true,
        ),
        ToolPolicy::Deny,
        "a configured deny is terminal even for an opted-in blueprint"
    );
}

#[test]
fn test_default_policy_read_file() {
    assert_eq!(default_tool_policy("read_file", true), ToolPolicy::Allow);
    assert_eq!(default_tool_policy("list_dir", true), ToolPolicy::Allow);
}

#[test]
fn test_default_policy_write_tools() {
    assert_eq!(default_tool_policy("write_file", true), ToolPolicy::Ask);
    assert_eq!(default_tool_policy("edit_file", true), ToolPolicy::Ask);
    assert_eq!(default_tool_policy("bash", true), ToolPolicy::Ask);
    assert_eq!(default_tool_policy("install_tool", true), ToolPolicy::Ask);
}

#[test]
fn test_default_policy_ask_user_tools_allow_by_default() {
    // These tools ARE the human-in-the-loop mechanism - they must not
    // require a separate approval prompt before asking the user.
    assert_eq!(
        default_tool_policy("ask_user_text", true),
        ToolPolicy::Allow
    );
    assert_eq!(
        default_tool_policy("ask_user_choice", true),
        ToolPolicy::Allow
    );
    assert_eq!(
        default_tool_policy("ask_user_confirm", true),
        ToolPolicy::Allow
    );
    assert_eq!(
        default_tool_policy("edit_document", true),
        ToolPolicy::Allow
    );
}

#[test]
fn test_resolve_policy_launch_override_wins() {
    let mut launch = HashMap::new();
    launch.insert("bash".to_string(), ToolPolicy::Allow);
    let policy = resolve_policy(
        "bash",
        true,
        &launch,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Allow);
}

#[test]
fn test_resolve_policy_yolo_wins() {
    let mut launch = HashMap::new();
    launch.insert("*".to_string(), ToolPolicy::Allow);
    let policy = resolve_policy(
        "bash",
        true,
        &launch,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Allow);
}

/// A stage may tighten the user's setting.
#[test]
fn test_resolve_policy_stage_may_tighten_global() {
    let mut stage = HashMap::new();
    stage.insert("bash".to_string(), "deny".to_string());
    let mut global = HashMap::new();
    global.insert("bash".to_string(), ToolPolicy::Allow);
    let policy = resolve_policy(
        "bash",
        true,
        &HashMap::new(),
        &stage,
        &HashMap::new(),
        &global,
        false,
    );
    assert_eq!(policy, ToolPolicy::Deny);
}

/// ...but it may NOT loosen it. A blueprint is a file the user
/// downloaded; letting a stage's `tool_permissions` overrule the user's
/// own `[tool_permissions]` would let an installed agent self-grant the
/// shell the user had explicitly denied.
#[test]
fn test_resolve_policy_stage_cannot_loosen_global() {
    let mut stage = HashMap::new();
    stage.insert("bash".to_string(), "allow".to_string());
    let mut global = HashMap::new();
    global.insert("bash".to_string(), ToolPolicy::Deny);
    let policy = resolve_policy(
        "bash",
        true,
        &HashMap::new(),
        &stage,
        &HashMap::new(),
        &global,
        false,
    );
    assert_eq!(policy, ToolPolicy::Deny);
}

/// The ceiling is only what the user *explicitly* configured. A tool they
/// have said nothing about is still the blueprint's to set - otherwise the
/// shipped researcher agent could not pre-approve its own `web_fetch`.
#[test]
fn test_resolve_policy_blueprint_free_when_user_silent() {
    let mut agent = HashMap::new();
    agent.insert("web_fetch".to_string(), "allow".to_string());
    let policy = resolve_policy(
        "web_fetch",
        false,
        &HashMap::new(),
        &HashMap::new(),
        &agent,
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Allow);
}

/// `--yolo` must not lift a `Deny` the user configured. Skipping *prompts*
/// is what `--yolo` is for; skipping a deny rule is not, or a denied tool
/// would be reachable from any unattended run.
#[test]
fn test_yolo_does_not_override_configured_deny() {
    let mut launch = HashMap::new();
    launch.insert("*".to_string(), ToolPolicy::Allow);
    let mut global = HashMap::new();
    global.insert("bash".to_string(), ToolPolicy::Deny);
    let policy = resolve_policy(
        "bash",
        true,
        &launch,
        &HashMap::new(),
        &HashMap::new(),
        &global,
        false,
    );
    assert_eq!(policy, ToolPolicy::Deny);
}

/// The same holds for a named `--allow`, not just the `--yolo` wildcard.
#[test]
fn test_named_allow_does_not_override_configured_deny() {
    let mut launch = HashMap::new();
    launch.insert("bash".to_string(), ToolPolicy::Allow);
    let mut global = HashMap::new();
    global.insert("bash".to_string(), ToolPolicy::Deny);
    let policy = resolve_policy(
        "bash",
        true,
        &launch,
        &HashMap::new(),
        &HashMap::new(),
        &global,
        false,
    );
    assert_eq!(policy, ToolPolicy::Deny);
}

/// A blueprint's own `deny` is terminal too - an agent that declares it
/// never needs a tool doesn't get handed it by an unattended `--yolo`.
#[test]
fn test_yolo_does_not_override_blueprint_deny() {
    let mut launch = HashMap::new();
    launch.insert("*".to_string(), ToolPolicy::Allow);
    let mut agent = HashMap::new();
    agent.insert("bash".to_string(), "deny".to_string());
    let policy = resolve_policy(
        "bash",
        true,
        &launch,
        &HashMap::new(),
        &agent,
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Deny);
}

/// What `--yolo` *does* still do: collapse `Ask` to `Allow`.
#[test]
fn test_yolo_still_collapses_ask_to_allow() {
    let mut launch = HashMap::new();
    launch.insert("*".to_string(), ToolPolicy::Allow);
    let mut global = HashMap::new();
    global.insert("bash".to_string(), ToolPolicy::Ask);
    let policy = resolve_policy(
        "bash",
        true,
        &launch,
        &HashMap::new(),
        &HashMap::new(),
        &global,
        false,
    );
    assert_eq!(policy, ToolPolicy::Allow);
}

#[test]
fn test_resolve_policy_falls_through_to_default() {
    let policy = resolve_policy(
        "bash",
        true,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Ask);
}

#[test]
fn test_default_policy_unknown_tools() {
    assert_eq!(default_tool_policy("unknown_tool", false), ToolPolicy::Ask);
    assert_eq!(default_tool_policy("mcp_tool", false), ToolPolicy::Ask);
    assert_eq!(default_tool_policy("custom_thing", true), ToolPolicy::Ask);
}

/// The agent layer is clamped the same way the stage layer is.
#[test]
fn test_resolve_policy_agent_cannot_loosen_global() {
    let mut agent = HashMap::new();
    agent.insert("bash".to_string(), "allow".to_string());
    let mut global = HashMap::new();
    global.insert("bash".to_string(), ToolPolicy::Deny);
    let policy = resolve_policy(
        "bash",
        true,
        &HashMap::new(),
        &HashMap::new(),
        &agent,
        &global,
        false,
    );
    assert_eq!(policy, ToolPolicy::Deny);
}

/// A global `ask` still bounds a blueprint's `allow` - the user gets their
/// prompt rather than silent execution.
#[test]
fn test_resolve_policy_global_ask_bounds_blueprint_allow() {
    let mut agent = HashMap::new();
    agent.insert("write_file".to_string(), "allow".to_string());
    let mut global = HashMap::new();
    global.insert("write_file".to_string(), ToolPolicy::Ask);
    let policy = resolve_policy(
        "write_file",
        true,
        &HashMap::new(),
        &HashMap::new(),
        &agent,
        &global,
        false,
    );
    assert_eq!(policy, ToolPolicy::Ask);
}

#[test]
fn test_resolve_policy_launch_override_specific_beats_wildcard() {
    let mut launch = HashMap::new();
    launch.insert("bash".to_string(), ToolPolicy::Deny);
    launch.insert("*".to_string(), ToolPolicy::Allow);
    let policy = resolve_policy(
        "bash",
        true,
        &launch,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        false,
    );
    // Specific tool match checked before wildcard
    assert_eq!(policy, ToolPolicy::Deny);
}

#[test]
fn test_resolve_policy_global_overrides_default() {
    let mut global = HashMap::new();
    global.insert("read_file".to_string(), ToolPolicy::Deny);
    let policy = resolve_policy(
        "read_file",
        true,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &global,
        false,
    );
    assert_eq!(policy, ToolPolicy::Deny);
}

#[test]
fn test_resolve_policy_stage_deny() {
    let mut stage = HashMap::new();
    stage.insert("bash".to_string(), "deny".to_string());
    let policy = resolve_policy(
        "bash",
        true,
        &HashMap::new(),
        &stage,
        &HashMap::new(),
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Deny);
}

#[test]
fn test_resolve_policy_stage_ask() {
    let mut stage = HashMap::new();
    stage.insert("read_file".to_string(), "ask".to_string());
    let policy = resolve_policy(
        "read_file",
        true,
        &HashMap::new(),
        &stage,
        &HashMap::new(),
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Ask);
}

#[test]
fn test_resolve_policy_unknown_stage_string_defaults_to_ask() {
    let mut stage = HashMap::new();
    stage.insert("bash".to_string(), "unknown_policy".to_string());
    let policy = resolve_policy(
        "bash",
        true,
        &HashMap::new(),
        &stage,
        &HashMap::new(),
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Ask);
}

#[test]
fn test_parse_policy_str_values() {
    assert_eq!(parse_policy_str("allow"), ToolPolicy::Allow);
    assert_eq!(parse_policy_str("Allow"), ToolPolicy::Allow);
    assert_eq!(parse_policy_str("ALLOW"), ToolPolicy::Allow);
    assert_eq!(parse_policy_str("deny"), ToolPolicy::Deny);
    assert_eq!(parse_policy_str("Deny"), ToolPolicy::Deny);
    assert_eq!(parse_policy_str("ask"), ToolPolicy::Ask);
    assert_eq!(parse_policy_str("Ask"), ToolPolicy::Ask);
    assert_eq!(parse_policy_str("anything_else"), ToolPolicy::Ask);
    assert_eq!(parse_policy_str(""), ToolPolicy::Ask);
}

/// A launch flag outranks a stage's `ask`, which is the point of `--allow`.
#[test]
fn test_resolve_policy_launch_overrides_stage_ask() {
    let mut launch = HashMap::new();
    launch.insert("bash".to_string(), ToolPolicy::Allow);
    let mut stage = HashMap::new();
    stage.insert("bash".to_string(), "ask".to_string());
    let policy = resolve_policy(
        "bash",
        true,
        &launch,
        &stage,
        &HashMap::new(),
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Allow);
}

/// It does not outrank a stage's `deny` - see
/// `test_yolo_does_not_override_blueprint_deny` for the rationale.
#[test]
fn test_resolve_policy_launch_cannot_override_stage_deny() {
    let mut launch = HashMap::new();
    launch.insert("bash".to_string(), ToolPolicy::Allow);
    let mut stage = HashMap::new();
    stage.insert("bash".to_string(), "deny".to_string());
    let policy = resolve_policy(
        "bash",
        true,
        &launch,
        &stage,
        &HashMap::new(),
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Deny);
}

#[test]
fn test_resolve_policy_stage_overrides_agent() {
    let mut stage = HashMap::new();
    stage.insert("bash".to_string(), "deny".to_string());
    let mut agent = HashMap::new();
    agent.insert("bash".to_string(), "allow".to_string());
    let policy = resolve_policy(
        "bash",
        true,
        &HashMap::new(),
        &stage,
        &agent,
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Deny);
}

#[test]
fn test_resolve_policy_agent_overrides_global() {
    let mut agent = HashMap::new();
    agent.insert("write_file".to_string(), "deny".to_string());
    let mut global = HashMap::new();
    global.insert("write_file".to_string(), ToolPolicy::Allow);
    let policy = resolve_policy(
        "write_file",
        true,
        &HashMap::new(),
        &HashMap::new(),
        &agent,
        &global,
        false,
    );
    assert_eq!(policy, ToolPolicy::Deny);
}

#[test]
fn test_resolve_policy_wildcard_launch_with_missing_specific() {
    let mut launch = HashMap::new();
    launch.insert("*".to_string(), ToolPolicy::Allow);
    // unknown_tool has no specific override, should match wildcard
    let policy = resolve_policy(
        "unknown_tool",
        false,
        &launch,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Allow);
}

#[test]
fn test_resolve_policy_mcp_tool_defaults_to_ask() {
    let policy = resolve_policy(
        "mcp_custom_tool",
        false,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Ask);
}

#[test]
fn test_resolve_policy_read_file_default_is_allow() {
    let policy = resolve_policy(
        "read_file",
        true,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Allow);
}

#[test]
fn test_resolve_policy_list_dir_default_is_allow() {
    let policy = resolve_policy(
        "list_dir",
        true,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Allow);
}

#[test]
fn test_resolve_policy_write_file_default_is_ask() {
    let policy = resolve_policy(
        "write_file",
        true,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Ask);
}

#[test]
fn test_resolve_policy_edit_file_default_is_ask() {
    let policy = resolve_policy(
        "edit_file",
        true,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Ask);
}

#[test]
fn test_default_policy_search_is_ask() {
    assert_eq!(default_tool_policy("search", true), ToolPolicy::Ask);
}

/// `glob` is the built-in path matcher, a read like `list_dir`.
#[test]
fn test_default_policy_glob_is_allow() {
    assert_eq!(default_tool_policy("glob", true), ToolPolicy::Allow);
    assert_eq!(default_tool_policy("search_files", true), ToolPolicy::Allow);
}

#[test]
fn test_default_policy_http_request_is_ask() {
    assert_eq!(default_tool_policy("http_request", true), ToolPolicy::Ask);
}

#[test]
fn test_default_policy_read_file_not_builtin_still_allow() {
    // Even if is_builtin is false, the name-based lookup should still match
    assert_eq!(default_tool_policy("read_file", false), ToolPolicy::Allow);
}

#[test]
fn test_default_policy_list_dir_not_builtin_still_allow() {
    assert_eq!(default_tool_policy("list_dir", false), ToolPolicy::Allow);
}

/// Policy is matched against the name the model calls, which is always the
/// canonical `shell`, while a blueprint, a config, or a `--allow` flag may
/// have written `bash`. Without the alias fold every one of those entries
/// is dead: `lev run --allow bash` does nothing, and `bash = "ask"` in the
/// shipped `coder` looks right only because the default for an unlisted
/// tool is also `ask`.
#[test]
fn a_permission_written_as_an_alias_reaches_the_tool() {
    let policy = |layer: &str, spelling: &str, called: &str| {
        let mut launch = HashMap::new();
        let mut stage = HashMap::new();
        let mut agent = HashMap::new();
        let mut global = HashMap::new();
        match layer {
            "launch" => {
                launch.insert(spelling.to_string(), ToolPolicy::Allow);
            }
            "stage" => {
                stage.insert(spelling.to_string(), "deny".to_string());
            }
            "agent" => {
                agent.insert(spelling.to_string(), "deny".to_string());
            }
            _ => {
                global.insert(spelling.to_string(), ToolPolicy::Deny);
            }
        }
        resolve_policy(called, true, &launch, &stage, &agent, &global, false)
    };

    // Written as the alias, called canonically: the shape every real run has.
    assert_eq!(policy("launch", "bash", "shell"), ToolPolicy::Allow);
    assert_eq!(policy("stage", "bash", "shell"), ToolPolicy::Deny);
    assert_eq!(policy("agent", "bash", "shell"), ToolPolicy::Deny);
    assert_eq!(policy("global", "bash", "shell"), ToolPolicy::Deny);

    // And the reverse, so neither spelling is the privileged one.
    assert_eq!(policy("launch", "shell", "bash"), ToolPolicy::Allow);
    assert_eq!(policy("global", "shell", "bash"), ToolPolicy::Deny);

    // A tool with no alias is unaffected: nothing else starts matching.
    assert_eq!(policy("global", "read_file", "write_file"), ToolPolicy::Ask);
}

/// The built-in default is matched canonically too, so the shell's `ask`
/// applies however the call was spelled.
#[test]
fn the_default_shell_policy_covers_both_spellings() {
    assert_eq!(default_tool_policy("shell", true), ToolPolicy::Ask);
    assert_eq!(default_tool_policy("bash", true), ToolPolicy::Ask);
}

/// The context tools write the agent's own context regions, not the
/// filesystem, so they default to `Allow`. Falling through to `Ask` would
/// charge a run that keeps notes one prompt per note.
#[test]
fn the_context_tools_do_not_prompt() {
    for tool in [
        "context_write",
        "context_append",
        "context_read",
        "context_delete",
        "context_list",
        "context_attach",
        "context_export",
        "read_files",
    ] {
        assert_eq!(
            default_tool_policy(tool, true),
            ToolPolicy::Allow,
            "{tool} must not raise a prompt by default"
        );
    }
}

#[test]
fn test_resolve_policy_agent_deny() {
    let mut agent = HashMap::new();
    agent.insert("read_file".to_string(), "deny".to_string());
    let policy = resolve_policy(
        "read_file",
        true,
        &HashMap::new(),
        &HashMap::new(),
        &agent,
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Deny);
}

#[test]
fn test_resolve_policy_agent_unknown_string_defaults_to_ask() {
    let mut agent = HashMap::new();
    agent.insert("bash".to_string(), "foobar".to_string());
    let policy = resolve_policy(
        "bash",
        true,
        &HashMap::new(),
        &HashMap::new(),
        &agent,
        &HashMap::new(),
        false,
    );
    assert_eq!(policy, ToolPolicy::Ask);
}

#[test]
fn test_resolve_policy_global_allow_overrides_default_ask() {
    let mut global = HashMap::new();
    global.insert("bash".to_string(), ToolPolicy::Allow);
    let policy = resolve_policy(
        "bash",
        true,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &global,
        false,
    );
    assert_eq!(policy, ToolPolicy::Allow);
}

/// With every level saying `deny`, nothing lifts it - not the stage, not the
/// agent, not `--allow`. Asserting `Allow` here would mean a launch flag
/// beats a unanimous deny.
#[test]
fn test_resolve_policy_full_chain_deny_is_terminal() {
    let mut launch = HashMap::new();
    launch.insert("bash".to_string(), ToolPolicy::Allow);
    let mut stage = HashMap::new();
    stage.insert("bash".to_string(), "deny".to_string());
    let mut agent = HashMap::new();
    agent.insert("bash".to_string(), "deny".to_string());
    let mut global = HashMap::new();
    global.insert("bash".to_string(), ToolPolicy::Deny);

    let policy = resolve_policy("bash", true, &launch, &stage, &agent, &global, false);
    assert_eq!(policy, ToolPolicy::Deny);
}

/// The full chain with nothing denying: stage `ask` is the tightest
/// configured level, and the launch flag relaxes it.
#[test]
fn test_resolve_policy_full_chain_launch_relaxes_ask() {
    let mut launch = HashMap::new();
    launch.insert("bash".to_string(), ToolPolicy::Allow);
    let mut stage = HashMap::new();
    stage.insert("bash".to_string(), "ask".to_string());
    let mut agent = HashMap::new();
    agent.insert("bash".to_string(), "ask".to_string());
    let mut global = HashMap::new();
    global.insert("bash".to_string(), ToolPolicy::Ask);

    let policy = resolve_policy("bash", true, &launch, &stage, &agent, &global, false);
    assert_eq!(policy, ToolPolicy::Allow);
}

// ─── What may run without asking ──────────────────────────────────────────
//
// `default_tool_policy` falls through to `Ask`, so a new tool is safe by
// construction *unless* somebody adds a match arm for it. These pin the
// other direction: which tools that arm is allowed to name.

/// Every tool that runs unprompted under the shipped defaults.
///
/// Adding a name here is the decision to let that tool act with no human in
/// the loop, on every run, for ever. It belongs in a diff a reviewer sees
/// rather than falling out of a match arm somebody extended - which is the
/// whole reason this list is written out instead of derived.
///
/// The four groups, and why each is defensible:
///
/// - **Reads.** `read_file`/`read_files`/`list_dir`/`search_files`/`glob`
///   are already bounded by `[read_paths]`, so the prompt would be asking
///   about a capability the confinement has already decided.
/// - **Context.** The `context_*` tools write the agent's own context
///   regions, not the filesystem. A prompt per note is a prompt nobody can
///   act on.
/// - **Sub-agents.** These default to `Allow` so a fan-out does not stop on
///   a prompt nothing is there to answer. They are routed through policy
///   resolution anyway so a configured `deny` still counts.
/// - **Asking.** These *are* the human-in-the-loop mechanism; gating them
///   would mean asking permission to ask.
const ALLOWED_WITHOUT_ASKING: &[&str] = &[
    "read_file",
    "read_files",
    "list_dir",
    "search_files",
    "glob",
    "context_write",
    "context_append",
    "context_read",
    "context_delete",
    "context_list",
    "context_attach",
    "context_export",
    // Reviewed: these write item state into the agent's own checklist
    // region and reach nothing outside the context window - the same
    // standard the `context_*` tools above are held to. Prompting per item
    // would make tracking work cost more than not tracking it, which is how
    // the feature would end up unused.
    "todo_add",
    "todo_done",
    "todo_note",
    "spawn_agent",
    "check_agent",
    "wait_for_agent",
    "send_to_agent",
    "kill_agent",
    // These read and start nothing: the request schema, an installed
    // blueprint's inputs, a dry run that resolves without starting, and
    // the history of a run in the caller's own tree.
    "spawn_schema",
    "describe_blueprint",
    "validate_spawn",
    "run_history",
    "ask_user_text",
    "ask_user_choice",
    "ask_user_confirm",
    "edit_document",
    // Reviewed: read-only reports about the host and the run - the clock,
    // the OS, the locale, the well-known directories, whether a program is
    // installed, and this run's own stage and limits. None of them mutates
    // anything, and `environment_info` withholds credential-shaped values
    // under the same `allow_env_vars` rule a Rhai `env_var` read answers to.
    "current_time",
    "system_info",
    "locale_info",
    "environment_info",
    "which_command",
    "runtime_info",
];

/// Every tool the runtime actually advertises, discovered rather than
/// listed.
///
/// Discovered on purpose: a test that enumerated tool names by hand would
/// keep passing on the day somebody adds one, which is exactly the day it
/// needs to fail.
fn advertised_tool_names() -> Vec<String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let builtins =
        leviath_tools::BuiltinTools::new(leviath_tools::ToolContext::new(dir.path().to_path_buf()));
    let mut names: Vec<String> = builtins
        .tool_defs()
        .into_iter()
        .map(|t| t.name)
        .chain(
            leviath_tools::BuiltinTools::subagent_tool_defs()
                .into_iter()
                .map(|t| t.name),
        )
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The discovery has to find something, or every assertion below passes by
/// iterating an empty list.
#[test]
fn the_tool_discovery_finds_the_real_toolset() {
    let names = advertised_tool_names();
    // Precomputed rather than called inside the message: an expression in an
    // `assert!` message is its own coverage region and only runs on failure.
    let count = names.len();
    assert!(count >= 15, "only {count} tools discovered: {names:?}");
    for expected in ["read_file", "write_file", "shell"] {
        assert!(names.contains(&expected.to_string()), "{expected} missing");
    }
}

/// No advertised tool runs unprompted unless it is on the reviewed list.
///
/// This is the guard that makes adding a tool safe. Ship a new one with an
/// `Allow` arm and this fails until the name is added above, which is the
/// moment somebody has to justify it.
#[test]
fn only_reviewed_tools_run_without_asking() {
    for name in advertised_tool_names() {
        let policy = default_tool_policy(&name, true);
        let reviewed = ALLOWED_WITHOUT_ASKING.contains(&name.as_str());
        match reviewed {
            true => assert_eq!(
                policy,
                ToolPolicy::Allow,
                "{name} is on the reviewed unprompted list but does not resolve to Allow"
            ),
            false => assert_ne!(
                policy,
                ToolPolicy::Allow,
                "{name} runs with no prompt and is not on the reviewed list"
            ),
        }
    }
}

/// A tool nobody has heard of asks.
///
/// The fall-through is what makes an MCP server's tools safe without this
/// file knowing their names, so it is worth pinning separately from the
/// discovered set.
#[test]
fn an_unknown_tool_asks_rather_than_running() {
    for name in [
        "definitely_not_a_real_tool",
        "mcp__someserver__delete_everything",
        "",
    ] {
        assert_eq!(default_tool_policy(name, false), ToolPolicy::Ask, "{name}");
        assert_eq!(default_tool_policy(name, true), ToolPolicy::Ask, "{name}");
    }
}

/// The tools that touch the filesystem, the one that runs code, and the
/// one that installs code for every future run are never `Allow` by
/// default under any spelling.
///
/// Spelled out separately from the list above because these are the ones
/// an aliasing mistake would quietly promote: `default_tool_policy`
/// matches on the *canonical* name, so a call arriving as `bash` has to land
/// on the same arm as `shell`.
#[test]
fn the_effectful_tools_ask_under_every_spelling() {
    for name in ["write_file", "edit_file", "shell", "install_tool"] {
        for spelling in leviath_tools::tool_name_spellings(name) {
            assert_eq!(
                default_tool_policy(spelling, true),
                ToolPolicy::Ask,
                "{spelling:?} (a spelling of {name}) does not ask"
            );
        }
    }
}

/// `--allow` is recognised under any spelling of the tool, and only as an
/// allow: an `ask` in the launch map is not a person having said yes.
#[test]
fn launch_allows_matches_any_spelling_and_only_an_allow() {
    let launch = HashMap::from([
        ("bash".to_string(), ToolPolicy::Allow),
        ("web_fetch".to_string(), ToolPolicy::Ask),
    ]);
    assert!(launch_allows(&launch, "shell"));
    assert!(launch_allows(&launch, "bash"));
    assert!(!launch_allows(&launch, "web_fetch"));
    assert!(!launch_allows(&launch, "read_file"));
}

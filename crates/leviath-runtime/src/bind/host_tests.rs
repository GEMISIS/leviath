use std::sync::Arc;

use super::*;
use crate::spec::graph::ToolGroup;

/// A provider that answers every question with a fixed value and serves any
/// model it is asked about.
pub(crate) struct Stub;

#[async_trait::async_trait]
impl leviath_providers::Provider for Stub {
    async fn infer(
        &self,
        _r: &leviath_providers::InferenceRequest,
    ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
        Err(leviath_providers::ProviderError::Other("stub".into()))
    }
    async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _m: &str) -> usize {
        5000
    }
    fn name(&self) -> &str {
        "stub"
    }
    fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
        leviath_providers::ModelCapabilities::default()
    }
}

/// A registry holding a [`Stub`] under each name.
pub(crate) fn registry(names: &[&str]) -> ProviderRegistry {
    let mut r = ProviderRegistry::new();
    for name in names {
        r.register(name.to_string(), Arc::new(Stub));
    }
    r
}

/// A stage with only a name and a model list.
pub(crate) fn stage(models: &[&str]) -> StageDef {
    serde_json::from_value(serde_json::json!({
        "name": "plan",
        "model": {
            "models": models.iter().map(|m| serde_json::to_value(ModelRef::parse(m).unwrap()).unwrap()).collect::<Vec<_>>(),
        },
    }))
    .unwrap()
}

#[tokio::test]
async fn the_stub_answers_every_question() {
    use leviath_providers::Provider;
    let p = Stub;
    let req = leviath_providers::InferenceRequest {
        system: vec![],
        messages: vec![],
        model: "m".to_string(),
        max_tokens: 1,
        temperature: 0.0,
        tools: vec![],
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    };
    assert!(p.infer(&req).await.is_err());
    assert_eq!(p.count_tokens("x", "m").await, 1);
    assert_eq!(p.name(), "stub");
    let _ = p.capabilities("m");
}

#[test]
fn code_is_read_inline_or_from_beside_the_blueprint_and_never_from_outside() {
    assert_eq!(read_code(&CodeRef::Inline("x".into()), None).unwrap(), b"x");
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("bp");
    std::fs::create_dir_all(base.join("hooks")).unwrap();
    std::fs::write(base.join("hooks/a.rhai"), "fn a() {}").unwrap();
    std::fs::write(dir.path().join("secret"), "s").unwrap();
    assert_eq!(
        read_code(&CodeRef::File("hooks/a.rhai".into()), Some(&base)).unwrap(),
        b"fn a() {}"
    );
    let no_base = read_code(&CodeRef::File("hooks/a.rhai".into()), None).unwrap_err();
    assert!(no_base.contains("put the code inline"), "{no_base}");
    let out = read_code(&CodeRef::File("../secret".into()), Some(&base)).unwrap_err();
    assert!(out.contains("outside the blueprint's directory"), "{out}");
    let missing = read_code(&CodeRef::File("hooks/none.rhai".into()), Some(&base)).unwrap_err();
    assert!(missing.contains("cannot read"), "{missing}");
}

#[test]
fn code_is_checked_for_what_it_is_used_as() {
    let cases: [(&str, CodeUse); 7] = [
        ("fn on_stage_enter(ctx) { () }", CodeUse::Hook),
        ("fn validate(content) { () }", CodeUse::Validator),
        ("fn render(ctx) { \"\" }", CodeUse::Region),
        ("fn check(bytes, mime_type) { () }", CodeUse::MimeCheck),
        ("fn check() { () }", CodeUse::DependencyCheck),
        ("// @tool mine\nlet x = 1;", CodeUse::Tool),
        ("anything at all", CodeUse::Seed),
    ];
    for (source, used_as) in cases {
        assert_eq!(
            check_code(source.as_bytes(), used_as),
            Ok(()),
            "{used_as:?}"
        );
    }
    let wrong = check_code(b"fn nothing() {}", CodeUse::Validator).unwrap_err();
    assert!(wrong.contains("fn validate"), "{wrong}");
    let binary = check_code(&[0xff, 0xfe], CodeUse::Seed).unwrap_err();
    assert!(binary.contains("not UTF-8"), "{binary}");
}

#[test]
fn a_stage_gets_its_own_model_on_a_registered_provider_with_its_window() {
    let defaults = ModelDefaults {
        fallback_order: vec![
            ModelRef::parse("mock/backup").unwrap(),
            ModelRef::parse("ghost/unregistered").unwrap(),
        ],
        ..Default::default()
    };
    let plan = choose_model(
        &stage(&["mock/gpt-mock"]),
        None,
        &defaults,
        &registry(&["mock"]),
    )
    .unwrap();
    assert_eq!(plan.model.provider.as_str(), "mock");
    assert_eq!(plan.model.id.as_str(), "gpt-mock");
    assert_eq!(plan.model.context_window, 5000);
    assert_eq!(
        plan.model.fallbacks,
        vec![ModelRef::parse("mock/backup").unwrap()],
        "a fallback on a provider that is not registered is left out"
    );
}

#[test]
fn a_requested_model_wins_and_an_unservable_stage_is_an_issue() {
    let r = registry(&["mock"]);
    let requested = ModelRef::parse("mock/forced").unwrap();
    let plan = choose_model(
        &stage(&["mock/gpt-mock"]),
        Some(&requested),
        &ModelDefaults::default(),
        &r,
    )
    .unwrap();
    assert_eq!(plan.model.id.as_str(), "forced");

    let mut nowhere = stage(&["gone/x"]);
    nowhere.model.allow_user_default = false;
    let issue = choose_model(&nowhere, None, &ModelDefaults::default(), &r).unwrap_err();
    assert_eq!(issue.path.to_string(), "(request)");
    assert_eq!(issue.code, IssueCode::Unresolvable);
    assert!(issue.message.contains("no usable provider"), "{issue}");
    assert_eq!(issue.known, ["mock"]);
}

#[test]
fn a_chosen_name_that_is_not_a_valid_name_is_an_issue() {
    let bad_provider = ModelDefaults {
        provider: "has space".into(),
        override_model: Some("m".into()),
        ..Default::default()
    };
    let issue =
        choose_model(&stage(&[]), None, &bad_provider, &registry(&["has space"])).unwrap_err();
    assert!(issue.message.starts_with("the chosen provider"), "{issue}");

    let bad_model = ModelDefaults {
        provider: "mock".into(),
        override_model: Some("has space".into()),
        ..Default::default()
    };
    let issue = choose_model(&stage(&[]), None, &bad_model, &registry(&["mock"])).unwrap_err();
    assert!(issue.message.starts_with("the chosen model"), "{issue}");
}

fn tool(name: &str) -> Tool {
    Tool {
        name: name.into(),
        description: format!("{name} does things"),
        parameters: serde_json::json!({"type": "object"}),
    }
}

fn catalog() -> Vec<ToolDef> {
    let mut defs = builtin_defs(&[tool("read_file"), tool("submit_output"), tool("bad name")]);
    defs.push(tool_def(&tool("spawn_agent"), ToolSource::Subagent).unwrap());
    defs.extend(mcp_defs(
        &McpServerName::new("gh").unwrap(),
        &[tool("gh__search"), tool("plain")],
    ));
    defs.push(tool_def(&tool("mine"), ToolSource::Script(Digest::of(b"mine"))).unwrap());
    defs
}

/// A graph's declaration of the MCP server `name`, however it is reached.
fn brought(name: &str) -> McpServerDef {
    McpServerDef {
        name: McpServerName::new(name).unwrap(),
        transport: None,
        command: Some(name.to_string()),
        url: None,
        args: vec![],
        env: Default::default(),
        headers: Default::default(),
    }
}

fn picked(stage: &StageDef) -> Vec<String> {
    select_tools(&catalog(), stage, &[])
        .unwrap()
        .iter()
        .map(|d| d.name.to_string())
        .collect()
}

#[test]
fn catalog_entries_carry_where_each_tool_comes_from() {
    let defs = catalog();
    let sources: Vec<(&str, &ToolSource)> =
        defs.iter().map(|d| (d.name.as_str(), &d.source)).collect();
    assert_eq!(sources[0], ("read_file", &ToolSource::Builtin));
    assert_eq!(sources[1], ("submit_output", &ToolSource::StageControl));
    assert_eq!(
        sources.len(),
        6,
        "a tool with a name no provider accepts is left out"
    );
    let gh = McpServerName::new("gh").unwrap();
    assert_eq!(
        defs[3].source,
        ToolSource::Mcp {
            server: gh.clone(),
            tool: "search".into()
        }
    );
    assert_eq!(
        defs[4].source,
        ToolSource::Mcp {
            server: gh,
            tool: "plain".into()
        },
        "a name without the server's prefix is kept whole"
    );
    assert_eq!(
        defs[0].schema.value(),
        &serde_json::json!({"type": "object"})
    );
}

#[test]
fn a_stage_gets_the_tools_it_names_groups_and_connectors() {
    let mut s = stage(&[]);
    assert!(picked(&s).is_empty());
    s.tools = vec![ToolSelector::Tool(ToolName::new("read_file").unwrap())];
    assert_eq!(picked(&s), ["read_file"]);
    s.tools = vec![ToolSelector::Tool(ToolName::new("search").unwrap())];
    assert_eq!(
        picked(&s),
        ["gh__search"],
        "an MCP tool named without its server"
    );
    let group = |g| vec![ToolSelector::Group(g)];
    s.tools = group(ToolGroup::Builtin);
    assert_eq!(
        picked(&s),
        ["read_file"],
        "a group never grants a stage-control tool"
    );
    s.tools = group(ToolGroup::Subagent);
    assert_eq!(picked(&s), ["spawn_agent"]);
    s.tools = group(ToolGroup::Scripts);
    assert_eq!(picked(&s), ["mine"]);
    s.tools = group(ToolGroup::Mcp);
    assert_eq!(picked(&s), ["gh__search", "plain"]);
    s.tools = group(ToolGroup::All);
    assert_eq!(picked(&s).len(), 5);
    s.tools = vec![];
    s.connectors = vec![McpServerName::new("gh").unwrap()];
    assert_eq!(picked(&s), ["gh__search", "plain"]);
}

#[test]
fn a_required_tool_the_stage_cannot_have_is_an_issue() {
    let mut s = stage(&[]);
    s.tools = vec![ToolSelector::Tool(ToolName::new("read_file").unwrap())];
    s.required_tools = vec![
        ToolName::new("read_file").unwrap(),
        ToolName::new("nope").unwrap(),
    ];
    let issues = select_tools(&catalog(), &s, &[]).unwrap_err();
    assert_eq!(issues.len(), 1);
    let issue = issues.iter().next().unwrap();
    assert_eq!(issue.path.to_string(), "required_tools[1]");
    assert!(issue.known.contains(&"mine".to_string()));
}

#[test]
fn a_named_tool_nothing_offers_is_an_issue_unless_it_is_an_mcp_tool() {
    let mut s = stage(&[]);
    s.tools = ["read_file", "mine", "search", "raed_file", "gone__tool"]
        .into_iter()
        .map(|n| ToolSelector::Tool(ToolName::new(n).unwrap()))
        .collect();
    let issues = select_tools(&catalog(), &s, &[]).unwrap_err();
    let paths: Vec<String> = issues.iter().map(|i| i.path.to_string()).collect();
    assert_eq!(
        paths,
        ["tools[3]"],
        "a tool of a server the graph does not bring is left out: {issues}"
    );
    let issue = issues.iter().next().unwrap();
    assert_eq!(issue.code, IssueCode::Unknown);
    assert!(issue.known.contains(&"read_file".to_string()));
}

/// A stage naming a tool of a server the graph brings gets an issue when the
/// catalog does not offer it: a misspelling when the server offers other
/// tools (which are the known names), and the server not having connected
/// when it offers none. The server's own tools, named either way, pass.
#[test]
fn a_named_tool_of_a_server_the_graph_brings_must_be_offered() {
    let mut s = stage(&[]);
    s.tools = [
        "gh__search",
        "gh__serach",
        "items__check_model",
        "gone__tool",
    ]
    .into_iter()
    .map(|n| ToolSelector::Tool(ToolName::new(n).unwrap()))
    .collect();
    let declared = [brought("gh"), brought("items")];
    let issues = select_tools(&catalog(), &s, &declared).unwrap_err();
    let paths: Vec<String> = issues.iter().map(|i| i.path.to_string()).collect();
    assert_eq!(paths, ["tools[1]", "tools[2]"], "{issues}");

    let misspelled = &issues.0[0];
    assert_eq!(misspelled.code, IssueCode::Unknown);
    assert_eq!(
        misspelled.message,
        "stage 'plan' names tool 'gh__serach', and the 'gh' server offers no tool 'serach'"
    );
    assert_eq!(misspelled.known, ["search", "plain"]);
    assert!(
        misspelled.hint.as_deref().unwrap().contains("spelling"),
        "{misspelled}"
    );

    let unconnected = &issues.0[1];
    assert_eq!(unconnected.code, IssueCode::Unresolvable);
    assert_eq!(
        unconnected.message,
        "stage 'plan' names tool 'items__check_model', and the 'items' server offers no tool \
         'check_model'"
    );
    assert!(unconnected.known.is_empty());
    assert!(
        unconnected
            .hint
            .as_deref()
            .unwrap()
            .contains("not have connected"),
        "{unconnected}"
    );

    s.tools = ["gh__search", "plain"]
        .into_iter()
        .map(|n| ToolSelector::Tool(ToolName::new(n).unwrap()))
        .collect();
    let names: Vec<String> = select_tools(&catalog(), &s, &declared)
        .unwrap()
        .iter()
        .map(|d| d.name.to_string())
        .collect();
    assert_eq!(names, ["gh__search", "plain"]);
}

#[test]
fn bytes_are_typed_by_the_registry_and_the_declaration() {
    let r = leviath_core::mime::MimeRegistry::builtin();
    let png = b"\x89PNG\r\n\x1a\n0000";
    let pattern = |p: &str| MimePattern::new(p).unwrap();
    assert_eq!(sniff(&r, "a.txt", b"hello", None).unwrap(), "text/plain");
    assert_eq!(
        sniff(&r, "x", png, Some(&pattern("image/*"))).unwrap(),
        "image/png"
    );
    let not_image = sniff(&r, "a.txt", b"hello", Some(&pattern("image/*"))).unwrap_err();
    assert!(
        not_image.contains("is text/plain, which is not image/*"),
        "{not_image}"
    );
    assert_eq!(
        sniff(&r, "a", b"{}", Some(&pattern("application/json"))).unwrap(),
        "application/json"
    );
    let contradicted = sniff(&r, "a", png, Some(&pattern("text/plain"))).unwrap_err();
    assert!(
        contradicted.contains("its bytes are image/png"),
        "{contradicted}"
    );
    // A type whose opening bytes are known must open with them.
    let mislabelled = sniff(&r, "a", b"hello", Some(&pattern("image/png"))).unwrap_err();
    assert!(
        mislabelled.contains("do not begin the way image/png files do"),
        "{mislabelled}"
    );
    assert_eq!(
        sniff(&r, "a", png, Some(&pattern("image/png"))).unwrap(),
        "image/png"
    );

    let mut checked = leviath_core::mime::MimeRegistry::builtin();
    let table: toml::Table =
        toml::from_str("[\"application/json\"]\ncheck = \"j.rhai\"\n").unwrap();
    checked.layer(&table, "test").unwrap();
    checked
        .attach_check(
            "application/json",
            Arc::new(leviath_core::mime::FnCheck::new(
                "json",
                |_: &leviath_core::mime::MimeType, bytes: &[u8]| match bytes.starts_with(b"{") {
                    true => Ok(()),
                    false => Err("not an object".to_string()),
                },
            )),
        )
        .unwrap();
    let invalid = sniff(&checked, "a", b"[]", Some(&pattern("application/json"))).unwrap_err();
    assert!(
        invalid.contains("not a valid application/json: not an object"),
        "{invalid}"
    );
}

#[test]
fn a_provider_fingerprint_follows_its_configuration_and_never_its_secrets() {
    let mut creds = ProviderCreds::simple("openai");
    creds.api_key = Some("sk-1".into());
    let base = provider_fingerprint(&creds);
    creds.api_key = Some("sk-2".into());
    assert_eq!(
        provider_fingerprint(&creds),
        base,
        "the key is not part of it"
    );
    creds
        .options
        .insert("header:0:Authorization".into(), "Bearer a".into());
    let with_header = provider_fingerprint(&creds);
    assert_ne!(with_header, base, "which headers it sends is");
    creds
        .options
        .insert("header:0:Authorization".into(), "Bearer b".into());
    assert_eq!(
        provider_fingerprint(&creds),
        with_header,
        "their values are not"
    );
    creds.base_url = Some("https://gateway.example".into());
    assert_ne!(provider_fingerprint(&creds), with_header);
    creds
        .options
        .insert("kind".into(), "openai-compatible".into());
    let kind = provider_fingerprint(&creds);
    creds.options.insert("kind".into(), "openai".into());
    assert_ne!(provider_fingerprint(&creds), kind);
    assert_ne!(registered_fingerprint("a"), registered_fingerprint("b"));
}

#[test]
fn a_tool_list_fingerprint_ignores_order_and_follows_every_field() {
    let a = tool_def(&tool("a"), ToolSource::Builtin).unwrap();
    let b = tool_def(&tool("b"), ToolSource::Builtin).unwrap();
    let ab = tools_fingerprint(&[a.clone(), b.clone()]);
    assert_eq!(tools_fingerprint(&[b.clone(), a.clone()]), ab);
    let mut a2 = a.clone();
    a2.description = "other".into();
    assert_ne!(tools_fingerprint(&[a2, b.clone()]), ab);
    let mut a3 = a;
    a3.schema = JsonDoc::default();
    assert_ne!(tools_fingerprint(&[a3, b]), ab);
}

#[test]
fn a_plan_carries_the_models_longest_reply() {
    let plan = choose_model(
        &stage(&["mock/gpt-mock"]),
        None,
        &ModelDefaults::default(),
        &registry(&["mock"]),
    )
    .unwrap();
    let most = leviath_providers::ModelCapabilities::default().max_output_tokens;
    assert_eq!(plan.max_output_tokens as usize, most);
}

#[test]
fn an_install_script_must_define_install() {
    assert_eq!(check_code(b"fn install() { }", CodeUse::Install), Ok(()));
    let none = check_code(b"fn check() { }", CodeUse::Install).unwrap_err();
    assert!(none.contains("fn install()"), "{none}");
}

/// A provider that serves exactly the models it is given.
struct Serves(&'static [&'static str]);

#[async_trait::async_trait]
impl leviath_providers::Provider for Serves {
    async fn infer(
        &self,
        r: &leviath_providers::InferenceRequest,
    ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
        Stub.infer(r).await
    }
    async fn count_tokens(&self, t: &str, m: &str) -> usize {
        Stub.count_tokens(t, m).await
    }
    fn max_context_tokens(&self, m: &str) -> usize {
        Stub.max_context_tokens(m)
    }
    fn name(&self) -> &str {
        "serves"
    }
    fn capabilities(&self, m: &str) -> leviath_providers::ModelCapabilities {
        Stub.capabilities(m)
    }
    fn serves_model(&self, model_key: &str) -> Option<String> {
        self.0.contains(&model_key).then(|| model_key.to_string())
    }
    fn served_catalog(&self) -> Option<Vec<String>> {
        Some(self.0.iter().map(ToString::to_string).collect())
    }
}

#[tokio::test]
async fn the_serving_stub_answers_as_the_plain_one_does() {
    use leviath_providers::Provider as _;
    let p = Serves(&["m"]);
    let request: leviath_providers::InferenceRequest = serde_json::from_value(serde_json::json!({
        "messages": [], "model": "m", "max_tokens": 1, "temperature": 0.0, "tools": [], "extra": null,
    }))
    .unwrap();
    assert!(p.infer(&request).await.is_err());
    assert_eq!(p.count_tokens("x", "m").await, 1);
    assert_eq!(p.max_context_tokens("m"), 5000);
    assert_eq!(p.name(), "serves");
    let _ = p.capabilities("m");
}

/// The compaction models, resolved as a stage's are: an open route lands on
/// the provider the operator prefers, a pinned one stays, and the rest of
/// the chain follows as failover.
#[test]
fn compaction_models_resolve_as_a_stages_do() {
    let model = |m: &str| ModelRef::parse(m).unwrap();
    let mut r = ProviderRegistry::new();
    r.register("cfg".to_string(), Arc::new(Serves(&["small"])));
    r.register("other".to_string(), Arc::new(Serves(&["small", "big"])));
    let prefer = |order: &[&str]| ModelDefaults {
        provider: order[0].to_string(),
        provider_order: order.iter().map(ToString::to_string).collect(),
        ..ModelDefaults::default()
    };
    let routes = |named: &[&str], d: &ModelDefaults| {
        let named: Vec<ModelRef> = named.iter().map(|m| model(m)).collect();
        compaction_routes(&named, d, &r)
            .unwrap()
            .routes
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };
    assert!(
        routes(&[], &prefer(&["cfg"])).is_empty(),
        "the stage's model"
    );
    assert_eq!(routes(&["small"], &prefer(&["cfg"])), ["cfg/small"]);
    assert_eq!(
        routes(&["small"], &prefer(&["other", "cfg"])),
        ["other/small", "cfg/small"],
        "the operator's order, the rest as failover"
    );
    assert_eq!(
        routes(&["big", "cfg/small"], &prefer(&["cfg", "other"])),
        ["other/big", "cfg/small"]
    );
    let overridden = ModelDefaults {
        override_model: Some("big".into()),
        ..prefer(&["other"])
    };
    assert_eq!(
        routes(&["cfg/small"], &overridden),
        ["other/big", "cfg/small"],
        "override_model leads, as it does for a stage"
    );
    assert_eq!(
        routes(&["nobody-serves-this", "gone/m"], &prefer(&["cfg"])),
        ["nobody-serves-this", "gone/m"],
        "kept as written, and never called"
    );
}

/// A compaction model the stage resolver would refuse is refused, with the
/// resolver's reason.
#[test]
fn a_compaction_model_is_refused_only_where_it_would_keep_the_context() {
    let zero = ModelDefaults {
        retention: leviath_providers::retention::RetentionSettings {
            zero_requested: true,
            ..Default::default()
        },
        ..ModelDefaults::default()
    };
    let r = registry(&["openai"]);
    let model = |m: &str| vec![ModelRef::parse(m).unwrap()];
    let refused = compaction_routes(&model("openai/gpt-5.5"), &zero, &r).unwrap_err();
    assert!(
        refused.starts_with("the compaction model names openai/gpt-5.5"),
        "{refused}"
    );
    assert_eq!(
        compaction_routes(&model("openai/gpt-5.5"), &ModelDefaults::default(), &r)
            .map(|plan| plan.routes),
        Ok(model("openai/gpt-5.5"))
    );
}

/// A compaction model its provider says it does not carry is refused, and
/// the refusal points at the graph's compaction, not at a stage.
#[test]
fn a_compaction_model_its_provider_does_not_carry_is_refused() {
    let mut r = ProviderRegistry::new();
    r.register("cfg".to_string(), Arc::new(Serves(&["small"])));
    let named = [ModelRef::parse("cfg/big").unwrap()];
    let refused = compaction_routes(&named, &ModelDefaults::default(), &r).unwrap_err();
    assert!(
        refused.starts_with("the compaction model names cfg/big, which provider 'cfg'"),
        "{refused}"
    );
    assert!(
        refused.contains("the graph's `compaction.models`"),
        "{refused}"
    );
}

/// The resolver's notes come back with the routes, so the run's log says
/// when the user's `override_model` moved the summary off the graph's model.
#[test]
fn a_compaction_moved_by_override_model_says_so() {
    let mut r = ProviderRegistry::new();
    r.register("cfg".to_string(), Arc::new(Serves(&["small", "big"])));
    let defaults = ModelDefaults {
        provider: "cfg".to_string(),
        override_model: Some("big".into()),
        ..ModelDefaults::default()
    };
    let plan = compaction_routes(&[ModelRef::parse("small").unwrap()], &defaults, &r).unwrap();
    assert_eq!(
        plan.notes,
        [
            "[model] the compaction model starts on cfg/big (override_model); blueprint asked for small"
        ]
    );
}

#[test]
fn a_graphs_mime_rows_layer_over_the_machines() {
    use crate::spec::graph::{MimeRowDef, MimeRows, TokenRule};
    let row = |tokens: TokenRule, check: Option<CodeRef>| MimeRowDef {
        family: Some("doc".into()),
        text: Some(false),
        tokens: Some(tokens),
        extensions: Some(vec!["acme".into()]),
        magic: Some("41434d45".into()),
        stand_in: Some("[{name}]".into()),
        check,
    };
    let rows: MimeRows = [
        (
            MimePattern::new("application/x-a").unwrap(),
            row(
                TokenRule::PerByte(0.5),
                Some(CodeRef::File("c.rhai".into())),
            ),
        ),
        (
            MimePattern::new("application/x-b").unwrap(),
            row(
                TokenRule::PerPixel {
                    divisor: 10,
                    max: 99,
                },
                Some(CodeRef::Inline("fn check(b, m) {}".into())),
            ),
        ),
        (
            MimePattern::new("application/x-c").unwrap(),
            row(TokenRule::PerSecond(3), None),
        ),
        (
            MimePattern::new("application/x-d").unwrap(),
            row(TokenRule::PerPage(4), None),
        ),
        (
            MimePattern::new("application/x-e").unwrap(),
            row(TokenRule::Fixed(5), None),
        ),
    ]
    .into();
    let table = mime_table(&rows);
    assert_eq!(table.len(), 5);
    assert_eq!(table["application/x-a"]["check"].as_str(), Some("c.rhai"));
    let inline = format!("inline:{}", Digest::of(b"fn check(b, m) {}"));
    assert_eq!(
        table["application/x-b"]["check"].as_str(),
        Some(inline.as_str())
    );
    let base = leviath_core::mime::MimeRegistry::builtin();
    let built = run_registry(&base, &rows).unwrap();
    let e = leviath_core::mime::MimeType::parse("application/x-e").unwrap();
    assert_eq!(built.info(&e).family, "doc");
    assert_eq!(
        built.info(&e).tokens,
        leviath_core::mime::TokenRule::Fixed(5)
    );

    let odd: MimeRows = [(
        MimePattern::new("application/x-f").unwrap(),
        MimeRowDef {
            magic: Some("not hex".into()),
            ..MimeRowDef::default()
        },
    )]
    .into();
    assert!(run_registry(&base, &odd).is_err());
}

#[test]
fn a_dependency_check_runs_from_the_runs_copy() {
    assert_eq!(run_check(Some(b"fn check() { () }")), Ok(()));
    assert_eq!(
        run_check(Some(b"fn check() { \"get it\" }")),
        Err("get it".to_string())
    );
    assert!(run_check(None).unwrap_err().contains("holds no code"));
    assert!(run_check(Some(&[0xff])).unwrap_err().contains("not UTF-8"));
}

#[test]
fn a_stage_grants_its_tools_groups_and_connectors() {
    let mut s = stage(&[]);
    s.tools = vec![
        ToolSelector::Tool(ToolName::new("read_file").unwrap()),
        ToolSelector::Group(ToolGroup::Mcp),
    ];
    s.connectors = vec![McpServerName::new("gh").unwrap()];
    let owners: ToolOwners = [("gh__search".to_string(), "gh".to_string())].into();
    let grants = stage_grants(&s, &owners);
    assert!(grants.contains(&"read_file".to_string()), "{grants:?}");
    assert!(grants.contains(&"gh__search".to_string()), "{grants:?}");
}

/// A provider that serves every model and describes each one with the same
/// mime, for the choices that turn on what a model can hand back.
struct Makes(Option<leviath_providers::ModelMime>);

#[async_trait::async_trait]
impl leviath_providers::Provider for Makes {
    async fn infer(
        &self,
        _r: &leviath_providers::InferenceRequest,
    ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
        Err(leviath_providers::ProviderError::Other("makes".into()))
    }
    async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _m: &str) -> usize {
        5000
    }
    fn name(&self) -> &str {
        "makes"
    }
    fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
        leviath_providers::ModelCapabilities::default()
    }
    fn mime(&self, _m: &str) -> leviath_providers::ModelMime {
        self.0
            .clone()
            .unwrap_or_else(leviath_providers::ModelMime::text_only)
    }
    fn serves_model(&self, model_key: &str) -> Option<String> {
        Some(model_key.to_string())
    }
}

#[tokio::test]
async fn the_mime_stub_answers_every_question() {
    use leviath_providers::Provider;
    let p = Makes(None);
    let req = leviath_providers::InferenceRequest {
        system: vec![],
        messages: vec![],
        model: "m".to_string(),
        max_tokens: 1,
        temperature: 0.0,
        tools: vec![],
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    };
    assert!(p.infer(&req).await.is_err());
    assert_eq!(p.count_tokens("x", "m").await, 1);
    assert_eq!(p.name(), "makes");
    let _ = p.capabilities("m");
}

/// Three routes to one model: one whose description says it writes text only
/// (`wordy`), one nothing has described (`unsure`), and one that can speak
/// (`speaks`), in that preference.
fn routes_by_output() -> (ProviderRegistry, ModelDefaults) {
    use leviath_providers::ModelMime;
    let mut r = ProviderRegistry::new();
    r.register(
        "wordy".into(),
        Arc::new(Makes(Some(ModelMime::new(
            &["text/*", "image/*"],
            &["text/*"],
        )))),
    );
    r.register("unsure".into(), Arc::new(Makes(None)));
    r.register(
        "speaks".into(),
        Arc::new(Makes(Some(ModelMime::new(
            &["text/*"],
            &["text/*", "audio/*"],
        )))),
    );
    let defaults = ModelDefaults {
        provider: "wordy".into(),
        provider_order: vec!["wordy".into(), "unsure".into(), "speaks".into()],
        ..Default::default()
    };
    (r, defaults)
}

/// A stage on the bare model `voice` that routes `routed` to a region.
fn stage_routing(routed: &str) -> StageDef {
    let mut s = stage(&["voice"]);
    s.output_routing.insert(
        routed.to_string(),
        crate::spec::names::RegionName::new("speech").unwrap(),
    );
    s
}

#[test]
fn a_stage_that_makes_audio_starts_on_a_route_that_can_make_it() {
    let (r, defaults) = routes_by_output();
    let plan = choose_model(&stage_routing("audio/mpeg"), None, &defaults, &r).unwrap();
    assert_eq!(
        (plan.model.provider.as_str(), plan.model.id.as_str()),
        ("speaks", "voice"),
        "the route whose model can hand back audio leads"
    );
    assert_eq!(
        plan.model.fallbacks,
        vec![ModelRef::parse("unsure/voice").unwrap()],
        "a route nothing described stays behind it; one described as text only is dropped"
    );
    assert!(
        plan.notes
            .iter()
            .any(|n| n.contains("wordy/voice") && n.contains("audio/mpeg")),
        "the dropped route is named in the stage's notes: {:?}",
        plan.notes
    );

    // Text-shaped output asks nothing of the model beyond its reply, so the
    // preference stands.
    let mut json = stage(&["voice"]);
    json.output = Some(crate::spec::graph::OutputDef {
        format: Some("application/json".into()),
        ..Default::default()
    });
    let plan = choose_model(&json, None, &defaults, &r).unwrap();
    assert_eq!(plan.model.provider.as_str(), "wordy");
    let plan = choose_model(&stage(&["voice"]), None, &defaults, &r).unwrap();
    assert_eq!(plan.model.provider.as_str(), "wordy");
}

#[test]
fn a_stage_no_route_can_make_the_output_of_is_refused_by_name() {
    let (r, mut defaults) = routes_by_output();
    defaults.provider_order = vec!["wordy".into()];
    let issue = choose_model(&stage_routing("audio/*"), None, &defaults, &r).unwrap_err();
    assert_eq!(issue.code, IssueCode::Unresolvable);
    for named in ["plan", "voice", "audio/*", "wordy"] {
        assert!(issue.message.contains(named), "names {named}: {issue}");
    }
    // A route nothing described is not a refusal: it may well speak.
    defaults.provider_order = vec!["wordy".into(), "unsure".into()];
    let plan = choose_model(&stage_routing("audio/*"), None, &defaults, &r).unwrap();
    assert_eq!(plan.model.provider.as_str(), "unsure");
    // A run that pins `provider/model` asked for that route.
    let pinned = ModelRef::parse("wordy/voice").unwrap();
    let plan = choose_model(&stage_routing("audio/*"), Some(&pinned), &defaults, &r).unwrap();
    assert_eq!(plan.model.provider.as_str(), "wordy");
}

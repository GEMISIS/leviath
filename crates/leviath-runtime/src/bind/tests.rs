use std::collections::BTreeMap;

use async_trait::async_trait;
use leviath_core::JsonDoc;

use super::*;
use crate::spec::graph::CodeRef;
use crate::spec::names::{ModelRef, StageName, ToolName};
use crate::spec::run_spec::tests::spec;

/// A machine with a fixed set of providers and MCP servers, whose stages'
/// tools all run in one kind of sandbox (`None`: it runs none). Its
/// `mcp_tools` is the trait's own, which lists nothing.
#[derive(Default)]
struct Plain {
    providers: BTreeMap<String, Digest>,
    mcp: BTreeMap<String, Digest>,
    /// Whether the secret store has lost the run's webhook secret.
    secret_gone: bool,
    sandbox: Option<SandboxKind>,
}

#[async_trait]
impl BindEnv for Plain {
    fn provider_fingerprint(&self, provider: &ProviderName) -> Option<Digest> {
        self.providers.get(provider.as_str()).cloned()
    }
    fn mcp_fingerprint(&self, server: &McpServerName) -> Option<Digest> {
        self.mcp.get(server.as_str()).cloned()
    }
    fn sandbox_kind(
        &self,
        _graph: &crate::spec::graph::RunGraph,
        _stage: &crate::spec::graph::StageDef,
    ) -> Option<SandboxKind> {
        self.sandbox
    }
    fn providers_now(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }
    fn mcp_servers_now(&self) -> Vec<String> {
        self.mcp.keys().cloned().collect()
    }
    fn holds_secret(&self, secret: &crate::spec::names::SecretRef) -> bool {
        assert_eq!(secret.as_str(), "t-1.secret");
        !self.secret_gone
    }
    async fn bind(&self, spec: &RunSpec, code: &CodeFiles) -> Result<Bindings, SpawnIssues> {
        assert!(code.contains_key(&Digest::of(b"code")));
        Ok(Bindings::new().with(Bound(spec.run_id.to_string())))
    }
}

/// A machine that can also list an MCP server's tools now.
struct Listing {
    plain: Plain,
    tools: Vec<ToolDef>,
}

#[async_trait]
impl BindEnv for Listing {
    fn provider_fingerprint(&self, provider: &ProviderName) -> Option<Digest> {
        self.plain.provider_fingerprint(provider)
    }
    fn mcp_fingerprint(&self, server: &McpServerName) -> Option<Digest> {
        self.plain.mcp_fingerprint(server)
    }
    fn sandbox_kind(
        &self,
        graph: &crate::spec::graph::RunGraph,
        stage: &crate::spec::graph::StageDef,
    ) -> Option<SandboxKind> {
        self.plain.sandbox_kind(graph, stage)
    }
    fn mcp_tools(&self, _server: &McpServerName) -> Option<Vec<ToolDef>> {
        Some(self.tools.clone())
    }
    async fn bind(&self, spec: &RunSpec, code: &CodeFiles) -> Result<Bindings, SpawnIssues> {
        self.plain.bind(spec, code).await
    }
}

#[derive(bevy_ecs::component::Component, Debug, PartialEq)]
struct Bound(String);

fn d() -> Digest {
    Digest::of(b"code")
}

/// The machine exactly as `spec()` recorded it.
fn same() -> Plain {
    Plain {
        providers: [("mock".to_string(), d())].into(),
        mcp: [("gh".to_string(), d())].into(),
        secret_gone: false,
        sandbox: Some(SandboxKind::Container),
    }
}

fn code() -> CodeFiles {
    [(d(), b"code".to_vec())].into()
}

fn mcp_tool(tool: &str, description: &str) -> ToolDef {
    ToolDef {
        name: ToolName::new(format!("gh__{tool}")).unwrap(),
        description: description.into(),
        schema: JsonDoc::default(),
        source: ToolSource::Mcp {
            server: McpServerName::new("gh").unwrap(),
            tool: tool.into(),
        },
    }
}

fn paths(issues: &SpawnIssues) -> Vec<String> {
    issues.iter().map(|i| i.path.to_string()).collect()
}

async fn refused(spec: &RunSpec, code: &CodeFiles, env: &dyn BindEnv) -> SpawnIssues {
    bind(spec, code, env).await.unwrap_err()
}

#[tokio::test]
async fn an_unchanged_machine_binds_through_the_host() {
    let bindings = bind(&spec(), &code(), &same()).await.unwrap();
    let mut world = bevy_ecs::world::World::new();
    let e = crate::insert::insert(
        &mut world,
        std::sync::Arc::new(spec()),
        bindings,
        &crate::state::RunState::initial(
            crate::spec::names::StageName::new("plan").unwrap(),
            Default::default(),
            true,
        ),
    );
    assert_eq!(world.get::<Bound>(e), Some(&Bound("t-1".into())));
}

/// A stage whose tools ran in a container and would run on the machine
/// itself now is held, naming the stage and both kinds; so is one moved to
/// another kind of sandbox.
#[tokio::test]
async fn a_changed_sandbox_holds_the_run_and_names_the_stage() {
    for now in [SandboxKind::None, SandboxKind::Namespace] {
        let env = Plain {
            sandbox: Some(now),
            ..same()
        };
        let issues = refused(&spec(), &code(), &env).await;
        assert_eq!(paths(&issues), ["env.sandbox.plan"]);
        let issue = issues.iter().next().unwrap();
        assert_eq!(issue.code, IssueCode::Changed);
        assert!(
            issue.message.contains("stage 'plan'")
                && issue.message.contains("'container' sandbox")
                && issue.message.contains(kind_name(now)),
            "{issue}"
        );
        assert!(issue.hint.as_deref().unwrap().contains("'container'"));
    }
}

/// A host that runs no sandboxes holds a run that ran in one, and carries
/// on one that ran on the machine itself. A run that recorded no sandbox (a
/// converted one), or a stage its graph no longer has, is not compared.
/// A machine that runs no sandboxes, as the trait's own answer says.
struct NoSandboxes(Plain);

#[async_trait]
impl BindEnv for NoSandboxes {
    fn provider_fingerprint(&self, provider: &ProviderName) -> Option<Digest> {
        self.0.provider_fingerprint(provider)
    }
    fn mcp_fingerprint(&self, server: &McpServerName) -> Option<Digest> {
        self.0.mcp_fingerprint(server)
    }
    async fn bind(&self, spec: &RunSpec, code: &CodeFiles) -> Result<Bindings, SpawnIssues> {
        self.0.bind(spec, code).await
    }
}

#[tokio::test]
async fn a_host_without_sandboxes_holds_only_a_sandboxed_run() {
    let none = NoSandboxes(same());
    let issues = refused(&spec(), &code(), &none).await;
    let issue = issues.iter().next().unwrap();
    assert_eq!(issue.code, IssueCode::Unavailable);
    assert!(issue.message.contains("runs no sandboxes"), "{issue}");

    let mut on_host = spec();
    on_host.env.sandbox = [(StageName::new("plan").unwrap(), SandboxKind::None)].into();
    assert!(bind(&on_host, &code(), &none).await.is_ok());

    let mut converted = spec();
    converted.env.sandbox.clear();
    assert!(bind(&converted, &code(), &none).await.is_ok());

    let mut gone = spec();
    gone.env.sandbox = [(StageName::new("gone").unwrap(), SandboxKind::Container)].into();
    assert!(bind(&gone, &code(), &none).await.is_ok());
}

#[tokio::test]
async fn a_removed_provider_is_unavailable_where_the_stage_uses_it() {
    // Renamed: the provider the run used is gone, and another is configured.
    let env = Plain {
        providers: [("renamed".to_string(), d())].into(),
        ..same()
    };
    let issues = refused(&spec(), &code(), &env).await;
    assert_eq!(paths(&issues), ["stages.plan.model.provider"]);
    let issue = issues.iter().next().unwrap();
    assert_eq!(issue.code, IssueCode::Unavailable);
    assert!(issue.message.contains("'mock'"), "{issue}");
    assert_eq!(issue.known, ["renamed"], "what is configured now");
}

#[tokio::test]
async fn a_changed_provider_names_itself_and_the_different_configuration() {
    let env = Plain {
        providers: [("mock".to_string(), Digest::of(b"other"))].into(),
        ..same()
    };
    let issues = refused(&spec(), &code(), &env).await;
    let issue = issues.iter().next().unwrap();
    assert_eq!(issue.code, IssueCode::Changed);
    assert_eq!(issue.path.to_string(), "stages.plan.model.provider");
    assert!(
        issue.message.contains("provider 'mock'")
            && issue
                .message
                .contains("started against a different configuration"),
        "{issue}"
    );
}

#[tokio::test]
async fn a_fallback_provider_and_an_unused_one_are_reported_where_they_are_recorded() {
    let mut s = spec();
    s.env
        .providers
        .insert(ProviderName::new("other").unwrap(), d());
    s.env
        .providers
        .insert(ProviderName::new("compactor").unwrap(), d());
    let issues = refused(&s, &code(), &same()).await;
    assert_eq!(
        paths(&issues),
        ["env.providers.compactor", "stages.plan.model.fallbacks"]
    );
    assert_eq!(issues.iter().next().unwrap().known, ["mock"]);
    // A fallback without a provider names none.
    s.stages[0].model.fallbacks = vec![ModelRef::parse("bare").unwrap()];
    let issues = refused(&s, &code(), &same()).await;
    assert_eq!(
        paths(&issues),
        ["env.providers.compactor", "env.providers.other"]
    );
}

#[tokio::test]
async fn a_removed_mcp_server_is_unavailable_at_the_stages_that_use_its_tools() {
    let env = Plain {
        mcp: BTreeMap::new(),
        ..same()
    };
    let issues = refused(&spec(), &code(), &env).await;
    assert_eq!(paths(&issues), ["stages.plan.tools"]);
    let issue = issues.iter().next().unwrap();
    assert_eq!(issue.code, IssueCode::Unavailable);
    assert!(issue.message.contains("MCP server 'gh'"), "{issue}");
    assert!(issue.known.is_empty(), "no server is configured now");
    let renamed = Plain {
        mcp: [("github".to_string(), d())].into(),
        ..same()
    };
    let issues = refused(&spec(), &code(), &renamed).await;
    assert_eq!(issues.iter().next().unwrap().known, ["github"]);
}

#[tokio::test]
async fn a_changed_mcp_tool_list_names_the_tools_that_differ() {
    let changed = Plain {
        mcp: [("gh".to_string(), Digest::of(b"new"))].into(),
        ..same()
    };
    // Without a listing the server is named as a whole.
    let issues = refused(&spec(), &code(), &changed).await;
    let issue = issues.iter().next().unwrap();
    assert_eq!(issue.code, IssueCode::Changed);
    assert!(
        issue
            .message
            .ends_with("offers a different tool list from when the run started"),
        "{issue}"
    );

    // With one, each difference is named. `search` is now described
    // differently, `create` is new, and a built-in the host listed alongside
    // is not an MCP tool at all.
    let mut builtin = mcp_tool("x", "x");
    builtin.source = ToolSource::Builtin;
    let listing = Listing {
        plain: Plain {
            mcp: changed.mcp.clone(),
            ..same()
        },
        tools: vec![
            mcp_tool("search", "new words"),
            mcp_tool("create", "c"),
            builtin,
        ],
    };
    let issues = refused(&spec(), &code(), &listing).await;
    let message = &issues.iter().next().unwrap().message;
    assert!(
        message.contains("(changed: search; added: create)"),
        "{message}"
    );

    // The schema counts as much as the description, and a tool that is gone
    // is named as removed.
    let mut reshaped = mcp_tool("search", "s");
    reshaped.schema = JsonDoc::new(serde_json::json!({"type": "object"}));
    let listing = Listing {
        plain: Plain {
            mcp: changed.mcp.clone(),
            ..same()
        },
        tools: vec![reshaped],
    };
    let message = refused(&spec(), &code(), &listing).await.0[0]
        .message
        .clone();
    assert!(message.contains("(changed: search)"), "{message}");
    let listing = Listing {
        plain: Plain {
            mcp: changed.mcp.clone(),
            ..same()
        },
        tools: vec![],
    };
    let message = refused(&spec(), &code(), &listing).await.0[0]
        .message
        .clone();
    assert!(message.contains("(removed: search)"), "{message}");

    // A listing identical to the recorded tools says nothing more.
    let listing = Listing {
        plain: Plain {
            mcp: changed.mcp,
            ..same()
        },
        tools: vec![mcp_tool("search", "s")],
    };
    let message = refused(&spec(), &code(), &listing).await.0[0]
        .message
        .clone();
    assert!(message.ends_with("when the run started"), "{message}");
}

#[tokio::test]
async fn an_mcp_server_no_stage_uses_is_reported_against_its_fingerprint() {
    let mut s = spec();
    s.env
        .mcp_servers
        .insert(McpServerName::new("idle").unwrap(), d());
    let issues = refused(&s, &code(), &same()).await;
    assert_eq!(paths(&issues), ["env.mcp_servers.idle"]);
    assert_eq!(issues.iter().next().unwrap().known, ["gh"]);
}

#[tokio::test]
async fn code_the_run_file_lacks_or_that_does_not_match_is_reported() {
    let issues = refused(&spec(), &CodeFiles::new(), &same()).await;
    let issue = issues.iter().next().unwrap();
    assert_eq!(issue.path.to_string(), "code[0]");
    assert_eq!(issue.code, IssueCode::Missing);
    assert!(issue.message.contains("'hooks/enter.rhai'"), "{issue}");

    let mut s = spec();
    s.code.push((CodeRef::Inline("x".into()), Digest::of(b"x")));
    let tampered: CodeFiles = [(d(), b"code".to_vec()), (Digest::of(b"x"), b"y".to_vec())].into();
    let issues = refused(&s, &tampered, &same()).await;
    let issue = issues.iter().next().unwrap();
    assert_eq!(issue.path.to_string(), "code[1]");
    assert_eq!(issue.code, IssueCode::Changed);
    assert!(issue.message.contains("inline code"), "{issue}");
    assert_eq!(
        issue.got.as_deref(),
        Some(&*format!("digest {}", Digest::of(b"y")))
    );
}

#[tokio::test]
async fn empty_fingerprints_were_not_recorded_and_are_not_compared() {
    let mut s = spec();
    s.env.providers.clear();
    s.env.mcp_servers.clear();
    s.env.sandbox.clear();
    let bindings = bind(&s, &code(), &Plain::default()).await.unwrap();
    assert_eq!(bindings.len(), 1);
}

#[tokio::test]
async fn every_problem_is_reported_at_once() {
    let issues = refused(&spec(), &CodeFiles::new(), &Plain::default()).await;
    assert_eq!(
        paths(&issues),
        [
            "stages.plan.model.provider",
            "stages.plan.tools",
            "env.sandbox.plan",
            "code[0]"
        ]
    );
}

/// A resume whose webhook secret has gone from the store is held with an
/// issue at the secret, saying where it was kept; a run with no secret never
/// asks.
#[tokio::test]
async fn a_lost_webhook_secret_holds_the_run_at_the_secret() {
    let gone = Plain {
        secret_gone: true,
        ..same()
    };
    let issues = refused(&spec(), &code(), &gone).await;
    assert_eq!(paths(&issues), ["delivery.callback.signed_with"]);
    let issue = &issues.0[0];
    assert_eq!(issue.code, IssueCode::Unavailable);
    assert!(issue.to_string().contains("t-1.secret"), "{issue}");
    let mut unsigned = spec();
    unsigned.delivery.callback = None;
    assert!(bind(&unsigned, &code(), &gone).await.is_ok());
}

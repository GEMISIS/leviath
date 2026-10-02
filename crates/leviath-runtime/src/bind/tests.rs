use std::collections::BTreeMap;

use async_trait::async_trait;
use leviath_core::JsonDoc;

use super::*;
use crate::spec::graph::CodeRef;
use crate::spec::names::{ModelRef, ToolName};
use crate::spec::run_spec::tests::spec;

/// A machine with a fixed set of providers and MCP servers. Its `mcp_tools`
/// is the trait's own, which lists nothing.
#[derive(Default)]
struct Plain {
    providers: BTreeMap<String, Digest>,
    mcp: BTreeMap<String, Digest>,
}

#[async_trait]
impl BindEnv for Plain {
    fn provider_fingerprint(&self, provider: &ProviderName) -> Option<Digest> {
        self.providers.get(provider.as_str()).cloned()
    }
    fn mcp_fingerprint(&self, server: &McpServerName) -> Option<Digest> {
        self.mcp.get(server.as_str()).cloned()
    }
    fn providers_now(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }
    fn mcp_servers_now(&self) -> Vec<String> {
        self.mcp.keys().cloned().collect()
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

#[tokio::test]
async fn a_removed_provider_is_unavailable_where_the_stage_uses_it() {
    // Renamed: the provider the run used is gone, and another is configured.
    let env = Plain {
        providers: [("renamed".to_string(), d())].into(),
        ..same()
    };
    let issues = refused(&spec(), &code(), &env).await;
    assert_eq!(paths(&issues), ["stages.plan.provider"]);
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
    assert_eq!(issue.path.to_string(), "stages.plan.provider");
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
        ["env.providers.compactor", "stages.plan.fallbacks"]
    );
    assert_eq!(issues.iter().next().unwrap().known, ["mock"]);
    // A fallback without a provider names none.
    s.stages[0].fallbacks = vec![ModelRef::parse("bare").unwrap()];
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
    let bindings = bind(&s, &code(), &Plain::default()).await.unwrap();
    assert_eq!(bindings.len(), 1);
}

#[tokio::test]
async fn every_problem_is_reported_at_once() {
    let issues = refused(&spec(), &CodeFiles::new(), &Plain::default()).await;
    assert_eq!(
        paths(&issues),
        ["stages.plan.provider", "stages.plan.tools", "code[0]"]
    );
}

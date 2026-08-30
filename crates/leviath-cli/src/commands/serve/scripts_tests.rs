//! Tests for the script read/write routes.

use super::*;
use crate::commands::serve::testutil::{state_with_agent_paths, with_home};
use crate::config::Config;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::routing::{get, post};
use tower::ServiceExt;

/// Write a file, creating its directory first.
fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
    std::fs::write(path, body).expect("the file");
}

/// The agents directory under a scratch `LEVIATH_HOME`.
fn agent_root(home: &Path, name: &str) -> PathBuf {
    home.join(".leviath").join("agents").join(name)
}

/// The global tools directory under a scratch `LEVIATH_HOME`.
fn global_root(home: &Path) -> PathBuf {
    home.join(".leviath").join("tools")
}

/// The global providers directory under a scratch `LEVIATH_HOME`.
fn providers_root(home: &Path) -> PathBuf {
    home.join(".leviath").join("providers")
}

/// Every script route, mounted the way production mounts them under
/// `--allow-admin`, so a test drives the real handlers.
fn admin_router(agent_paths: Vec<PathBuf>) -> Router {
    Router::new()
        .route("/api/scripts", get(list_scripts))
        .route("/api/scripts/validate", post(validate_script))
        .route(
            "/api/scripts/{kind}/{name}",
            get(get_script).put(put_script).delete(delete_script),
        )
        .with_state(state_with_agent_paths(agent_paths))
}

async fn call(req: Request<Body>) -> (StatusCode, serde_json::Value) {
    call_with_paths(Vec::new(), req).await
}

/// `call`, against a server whose config lists `agent_paths`.
async fn call_with_paths(
    agent_paths: Vec<PathBuf>,
    req: Request<Body>,
) -> (StatusCode, serde_json::Value) {
    let resp = admin_router(agent_paths)
        .oneshot(req)
        .await
        .expect("a response");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("a body");
    // A 204 has no body, and `Value::Null` stands in for it so a caller that
    // only asserts on the status needs no second shape.
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn get_json(uri: &str) -> (StatusCode, serde_json::Value) {
    call(
        Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("a request"),
    )
    .await
}

async fn send(method: &str, uri: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    call(
        Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).expect("json")))
            .expect("a request"),
    )
    .await
}

/// A tool script that compiles.
const GOOD_TOOL: &str = "// @tool summarize\n// @description sums up\n\"ok\"";

/// A provider script that compiles, with every annotation the listing reports.
const GOOD_PROVIDER: &str = "// @provider groq\n\
                             // @description an OpenAI-compatible gateway\n\
                             // @default_model llama-3.3-70b\n\
                             // @max_context_tokens 128000\n\
                             // @supports_streaming true\n\
                             fn initialize(config) { #{ url: config.base_url } }\n\
                             fn inference(state, request) { #{ content: \"ok\" } }";

// ─── ScriptKind ─────────────────────────────────────────────────────────────

#[test]
fn every_kind_round_trips_through_its_wire_spelling() {
    for kind in [
        ScriptKind::Tool,
        ScriptKind::RegionHook,
        ScriptKind::StageHook,
        ScriptKind::OutputValidator,
        ScriptKind::Provider,
    ] {
        assert_eq!(ScriptKind::parse(kind.as_str()), Some(kind));
    }
    assert_eq!(ScriptKind::parse("model_provider"), None);
}

// ─── compile_status ─────────────────────────────────────────────────────────

/// One arm per kind, so the compiler each one reaches is the one it names.
#[test]
fn each_kind_is_compiled_by_its_own_compiler() {
    assert!(compile_status(ScriptKind::Tool, "t", GOOD_TOOL, &[]).is_ok());
    assert!(
        compile_status(
            ScriptKind::RegionHook,
            "r",
            "fn render(ctx) { \"out\" }",
            &[]
        )
        .is_ok()
    );
    assert!(
        compile_status(
            ScriptKind::StageHook,
            "h",
            "fn on_stage_enter(ctx) { () }",
            &["on_stage_enter"]
        )
        .is_ok()
    );
    assert!(
        compile_status(
            ScriptKind::OutputValidator,
            "v",
            "fn validate(content) { #{ valid: true } }",
            &[]
        )
        .is_ok()
    );
    assert!(compile_status(ScriptKind::Provider, "p", GOOD_PROVIDER, &[]).is_ok());
}

/// The other half of the same claim: a refusal carries the words of the
/// compiler that refused it, so an editor shows the author what a run would
/// have told them rather than a generic "does not compile".
#[test]
fn each_kind_reports_its_own_compilers_refusal() {
    let refuse = |kind, content| {
        compile_status(kind, "s", content, &[]).expect_err("the compiler refuses it")
    };
    let tool = refuse(ScriptKind::Tool, "// nothing at all\nlet");
    assert!(tool.contains("@tool"), "{tool}");
    let region = refuse(ScriptKind::RegionHook, "fn other(ctx) { () }");
    assert!(region.contains("fn render(ctx)"), "{region}");
    let hook = refuse(ScriptKind::StageHook, "let");
    assert!(hook.contains("s:"), "{hook}");
    let validator = refuse(ScriptKind::OutputValidator, "fn other(content) { () }");
    assert!(validator.contains("fn validate(content)"), "{validator}");
}

/// The provider compiler reads the AST for both entry points. Before it
/// existed, a script with no `inference` compiled, initialized, cached, and
/// failed at the first inference - mid-run, looking like a provider outage.
#[test]
fn a_provider_that_is_missing_inference_fails() {
    let status = compile_status(
        ScriptKind::Provider,
        "p",
        "fn initialize(config) { #{} }",
        &[],
    );
    let reason = status.expect_err("inference is required");
    assert!(reason.contains("inference"), "{reason}");
}

/// A stage hook named for a hook it does not define is a spawn error, so it is
/// an error here too rather than something a run discovers.
#[test]
fn a_stage_hook_that_is_missing_the_hook_it_was_named_for_fails() {
    let status = compile_status(
        ScriptKind::StageHook,
        "h",
        "fn on_stage_exit(ctx) { () }",
        &["on_stage_enter"],
    );
    let reason = status.expect_err("the named hook is absent");
    assert!(reason.contains("on_stage_enter"), "{reason}");
}

#[test]
fn status_pairs_flatten_both_outcomes() {
    assert_eq!(status_pair(Ok(())), (true, None));
    assert_eq!(
        status_pair(Err("boom".to_string())),
        (false, Some("boom".to_string()))
    );
}

// ─── resolve ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_tool_resolves_into_the_agents_tools_directory() {
    with_home(|home| async move {
        let target =
            resolve(&Config::default(), "tool", "summarize", Some("researcher")).expect("resolves");
        assert_eq!(target.dir, agent_root(&home, "researcher").join("tools"));
        assert_eq!(target.scope, "agent");
        assert_eq!(target.agent.as_deref(), Some("researcher"));
        assert!(target.path.ends_with("summarize.rhai"));
    })
    .await;
}

/// A hook has no directory of its own: the manifest names it relative to the
/// agent's own directory, so that is where these routes put it.
#[tokio::test]
async fn a_hook_resolves_beside_the_manifest_not_under_tools() {
    with_home(|home| async move {
        let target = resolve(
            &Config::default(),
            "stage_hook",
            "hooks",
            Some("researcher"),
        )
        .expect("resolves");
        assert_eq!(target.dir, agent_root(&home, "researcher"));
        assert_eq!(
            target.path,
            agent_root(&home, "researcher").join("hooks.rhai")
        );
    })
    .await;
}

#[tokio::test]
async fn a_global_tool_resolves_into_the_shared_directory() {
    with_home(|home| async move {
        let target = resolve(&Config::default(), "tool", "summarize", None).expect("resolves");
        assert_eq!(target.dir, global_root(&home));
        assert_eq!(target.scope, "global");
        assert!(target.agent.is_none());
    })
    .await;
}

/// The name is spelled back with its extension by the listing, so it is
/// accepted with one and means the same file.
#[tokio::test]
async fn a_name_that_already_carries_the_extension_is_the_same_file() {
    with_home(|home| async move {
        let bare = resolve(&Config::default(), "tool", "summarize", None).expect("resolves");
        let dotted = resolve(&Config::default(), "tool", "summarize.rhai", None).expect("resolves");
        assert_eq!(bare.path, dotted.path);
        assert!(bare.path.starts_with(global_root(&home)));
    })
    .await;
}

#[tokio::test]
async fn an_unknown_kind_is_refused() {
    with_home(|_home| async move {
        let (status, _) =
            resolve(&Config::default(), "model_provider", "x", None).expect_err("no such kind");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

#[tokio::test]
async fn a_traversing_script_name_is_refused() {
    with_home(|_home| async move {
        let (status, _) =
            resolve(&Config::default(), "tool", "../../evil", None).expect_err("a traversal");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

#[tokio::test]
async fn a_traversing_agent_name_is_refused() {
    with_home(|_home| async move {
        let (status, _) =
            resolve(&Config::default(), "tool", "x", Some("../../etc")).expect_err("a traversal");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

/// An agent the catalog found through `config.agent_paths` resolves to its
/// own directory, the one `GET /api/blueprints` reports as `path`. Resolving
/// to `~/.leviath/agents/<name>` instead would write the tool into a directory
/// that does not exist for such an agent, where nothing would ever load it.
#[tokio::test]
async fn an_agent_from_a_configured_path_resolves_to_its_own_directory() {
    with_home(|home| async move {
        let workspace = home.join("workspace");
        let agent = workspace.join("researcher");
        write(&agent.join("agent.leviath"), manifest_with_hooks());
        let config = Config {
            agent_paths: vec![workspace],
            ..Default::default()
        };

        let tool = resolve(&config, "tool", "summarize", Some("researcher")).expect("resolves");
        assert_eq!(tool.dir, agent.join("tools"));
        let hook = resolve(&config, "stage_hook", "hooks", Some("researcher")).expect("resolves");
        assert_eq!(hook.dir, agent);
        // A name the catalog does not know still lands in the installed
        // directory, so a new agent can be created there.
        let fresh = resolve(&config, "tool", "summarize", Some("newcomer")).expect("resolves");
        assert_eq!(fresh.dir, agent_root(&home, "newcomer").join("tools"));
    })
    .await;
}

/// Only tools have a machine-wide directory. A hook without an agent has no
/// directory at all, and inventing one would be inventing a layout.
#[tokio::test]
async fn a_hook_without_an_agent_is_refused() {
    with_home(|_home| async move {
        let (status, body) =
            resolve(&Config::default(), "region_hook", "x", None).expect_err("no scope");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.0.error.contains("?agent="), "{}", body.0.error);
    })
    .await;
}

// ─── guard ──────────────────────────────────────────────────────────────────

/// With no home to resolve, the base directory is empty, and an empty base
/// cannot be shown to contain anything. The write is refused rather than
/// landing in the process's working directory.
#[test]
fn a_path_that_cannot_be_shown_to_be_contained_is_refused() {
    let target = Target {
        kind: ScriptKind::Tool,
        dir: PathBuf::new(),
        file_dir: PathBuf::new(),
        path: PathBuf::from("no-such-file-for-the-guard-test.rhai"),
        name: "no-such-file-for-the-guard-test".to_string(),
        scope: "global",
        agent: None,
    };
    let (status, _) = guard(&target, Presence::Optional).expect_err("nothing contains it");
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ─── the fallible filesystem helpers ────────────────────────────────────────

/// The write helper reports the failure rather than the route pretending the
/// file landed. Reached with a path two directories deep, which the routes
/// themselves never build but which stands in for any write the filesystem
/// refuses.
#[tokio::test]
async fn a_write_the_filesystem_refuses_is_reported() {
    with_home(|home| async move {
        let dir = global_root(&home);
        std::fs::create_dir_all(&dir).expect("the directory");
        let target = Target {
            kind: ScriptKind::Tool,
            dir: dir.clone(),
            file_dir: dir.join("missing"),
            path: dir.join("missing").join("x.rhai"),
            name: "missing/x".to_string(),
            scope: "global",
            agent: None,
        };
        let (status, _) = write_script(&target, GOOD_TOOL).expect_err("no such directory");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    })
    .await;
}

#[tokio::test]
async fn a_delete_the_filesystem_refuses_is_reported() {
    with_home(|home| async move {
        let dir = global_root(&home);
        let path = dir.join("adirectory.rhai");
        std::fs::create_dir_all(&path).expect("the directory");
        let target = Target {
            kind: ScriptKind::Tool,
            dir: dir.clone(),
            file_dir: dir,
            path,
            name: "adirectory".to_string(),
            scope: "global",
            agent: None,
        };
        let (status, _) = remove_script(&target).expect_err("a directory is not a file");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    })
    .await;
}

// ─── addressed_path ─────────────────────────────────────────────────────────

/// The `{name}` off a URL: one component or a `/`-separated path, with or
/// without the extension, and nothing that could leave the directory.
#[test]
fn a_name_is_read_as_a_relative_path_of_safe_components() {
    let one = addressed_path("hooks").expect("a bare name");
    assert_eq!(one.name, "hooks");
    assert_eq!(one.relative, "hooks.rhai");
    assert_eq!(one.dirs, Vec::<String>::new());
    assert_eq!(one.file, "hooks.rhai");
    // The extension is optional and means the same file either way.
    assert_eq!(addressed_path("hooks.rhai"), Some(one));

    let deep = addressed_path("validators/a2ui.rhai").expect("a relative path");
    assert_eq!(deep.name, "validators/a2ui");
    assert_eq!(deep.relative, "validators/a2ui.rhai");
    assert_eq!(deep.dirs, vec!["validators".to_string()]);
    assert_eq!(deep.file, "a2ui.rhai");

    // A traversal, an absolute path, an empty segment, a Windows separator and
    // a name with nothing left after the extension are all unaddressable.
    for bad in [
        "../hooks",
        "validators/../../hooks",
        "/etc/hooks",
        "validators//a2ui",
        "validators\\a2ui",
        ".rhai",
        "",
    ] {
        assert_eq!(addressed_path(bad), None, "{bad}");
    }
}

/// A declared path is read the same way, except that it must already *be* a
/// `.rhai` file: `addressed_path` appends the extension, so a declared
/// `notes.txt` would otherwise be reported as `notes.txt.rhai`.
#[test]
fn a_declared_path_must_already_name_a_rhai_file() {
    assert_eq!(
        declared_address("hooks.rhai").map(|a| a.name),
        Some("hooks".to_string())
    );
    assert_eq!(
        declared_address("hooks/deep.rhai").map(|a| a.relative),
        Some("hooks/deep.rhai".to_string())
    );
    assert_eq!(declared_address("hooks.txt"), None);
    assert_eq!(declared_address("..rhai"), None);
    assert_eq!(declared_address("../hooks.rhai"), None);
}

// ─── GET /api/scripts ───────────────────────────────────────────────────────

/// A manifest declaring one of each hook kind, so the listing has something to
/// classify beyond tools.
fn manifest_with_hooks() -> &'static str {
    r#"
[agent]
name = "researcher"
version = "0.1.0"
description = "d"

[agent.output]
validator = "check.rhai"

[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Main"
max_iterations = 5

[stages.main.hooks]
on_stage_enter = "hooks.rhai"

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
notes = { kind = "custom", script = "notes.rhai", max_tokens = 500 }
"#
}

/// Both scopes and all five kinds in one answer, which is the listing's whole
/// job: what is here, what it plugs into, and whether it works.
#[tokio::test]
async fn the_listing_carries_every_kind_and_both_scopes() {
    with_home(|home| async move {
        let agent = agent_root(&home, "researcher");
        write(&agent.join("agent.leviath"), manifest_with_hooks());
        write(&agent.join("tools").join("web_search.rhai"), GOOD_TOOL);
        write(&agent.join("hooks.rhai"), "fn on_stage_enter(ctx) { () }");
        write(&agent.join("notes.rhai"), "fn render(ctx) { \"n\" }");
        write(
            &agent.join("check.rhai"),
            "fn validate(content) { #{ valid: true } }",
        );
        write(&global_root(&home).join("shared.rhai"), GOOD_TOOL);
        write(&providers_root(&home).join("groq.rhai"), GOOD_PROVIDER);

        let (status, body) = get_json("/api/scripts?agent=researcher").await;
        assert_eq!(status, StatusCode::OK);
        let scripts = body["scripts"].as_array().expect("a scripts array");

        let find = |kind: &str, name: &str| {
            scripts
                .iter()
                .find(|s| s["kind"] == kind && s["name"] == name)
                .expect("the script is listed")
        };
        assert_eq!(find("tool", "web_search")["source"], "agent");
        assert_eq!(find("tool", "web_search")["agent"], "researcher");
        assert_eq!(find("stage_hook", "hooks")["compiles"], true);
        assert_eq!(find("region_hook", "notes")["compiles"], true);
        assert_eq!(find("output_validator", "check")["compiles"], true);
        assert_eq!(find("tool", "shared")["source"], "global");
        assert!(find("tool", "shared").get("agent").is_none());
        assert_eq!(find("provider", "groq")["source"], "global");
        assert_eq!(find("provider", "groq")["compiles"], true);
    })
    .await;
}

/// A file that will not compile is listed with the reason, because an editor
/// that cannot see why a script is missing is no better than the shell.
#[tokio::test]
async fn the_listing_says_why_a_script_does_not_compile() {
    with_home(|home| async move {
        let agent = agent_root(&home, "broken");
        write(&agent.join("tools").join("bad.rhai"), "// nothing\nlet");
        let (status, body) = get_json("/api/scripts?agent=broken").await;

        assert_eq!(status, StatusCode::OK);
        let scripts = body["scripts"].as_array().expect("a scripts array");
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0]["compiles"], false);
        assert!(!scripts[0]["error"].as_str().expect("a reason").is_empty());
    })
    .await;
}

/// A hook the manifest declares but nobody wrote is listed as not compiling,
/// which is the state a run would fail on.
#[tokio::test]
async fn a_declared_hook_with_no_file_is_listed_as_failing() {
    with_home(|home| async move {
        let agent = agent_root(&home, "researcher");
        write(&agent.join("agent.leviath"), manifest_with_hooks());
        let (status, body) = get_json("/api/scripts?agent=researcher").await;

        assert_eq!(status, StatusCode::OK);
        let scripts = body["scripts"].as_array().expect("a scripts array");
        let hook = scripts
            .iter()
            .find(|s| s["name"] == "hooks")
            .expect("the declared hook");
        assert_eq!(hook["compiles"], false);
        let reason = hook["error"].as_str().expect("a reason");
        assert!(reason.contains("cannot read"), "{reason}");
    })
    .await;
}

/// A hook declared in a subdirectory is listed, addressed by the relative path
/// the manifest wrote, and the read route opens that exact file. It used to be
/// dropped from the listing entirely, so a console could not show a validator
/// declared the way the docs themselves declare one.
#[tokio::test]
async fn a_hook_declared_in_a_subdirectory_is_listed_and_readable() {
    with_home(|home| async move {
        let agent = agent_root(&home, "nested");
        let manifest = r#"
[agent]
name = "nested"
version = "0.1.0"
description = "d"

# An output spec that names a format and no validator, which is the common case:
# an output block is not a declaration that a script exists.
[agent.output]
format = "markdown"

[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Main"
max_iterations = 5

[stages.main.hooks]
on_stage_enter = "hooks/deep.rhai"

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
        write(&agent.join("agent.leviath"), manifest);
        write(
            &agent.join("hooks").join("deep.rhai"),
            "fn on_stage_enter(ctx) { () }",
        );
        let (status, body) = get_json("/api/scripts?agent=nested").await;

        assert_eq!(status, StatusCode::OK);
        let scripts = body["scripts"].as_array().expect("an array");
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0]["kind"], "stage_hook");
        assert_eq!(scripts[0]["name"], "hooks/deep");
        assert_eq!(scripts[0]["relative_path"], "hooks/deep.rhai");
        assert_eq!(scripts[0]["compiles"], true);
        assert_eq!(scripts[0]["declared"], true);

        // The name the listing reports is one the read route opens, with the
        // separator percent-encoded so it stays one path segment.
        let (status, body) = get_json("/api/scripts/stage_hook/hooks%2Fdeep?agent=nested").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "hooks/deep");
        assert_eq!(body["content"], "fn on_stage_enter(ctx) { () }");
    })
    .await;
}

/// A manifest that declares something these routes still cannot address - a
/// path that is not a `.rhai` file, or one that climbs out of the agent's
/// directory - is left out rather than listed under a name that would fetch a
/// different file.
#[tokio::test]
async fn a_declaration_that_cannot_be_addressed_is_left_out() {
    with_home(|home| async move {
        let agent = agent_root(&home, "odd");
        let manifest = r#"
[agent]
name = "odd"
version = "0.1.0"
description = "d"

[agent.output]
validator = "../outside.rhai"

[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Main"
max_iterations = 5

[stages.main.hooks]
on_stage_enter = "notes.txt"

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
        write(&agent.join("agent.leviath"), manifest);
        let (status, body) = get_json("/api/scripts?agent=odd").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body["scripts"].as_array().expect("an array").is_empty());
    })
    .await;
}

/// A manifest that will not parse takes nothing else down with it: the tools
/// still list, and the blueprint routes are where a broken manifest is reported.
#[tokio::test]
async fn a_manifest_that_will_not_parse_leaves_the_tools_listed() {
    with_home(|home| async move {
        let agent = agent_root(&home, "mangled");
        write(&agent.join("agent.leviath"), "this is not toml =");
        write(&agent.join("tools").join("web_search.rhai"), GOOD_TOOL);
        let (status, body) = get_json("/api/scripts?agent=mangled").await;

        assert_eq!(status, StatusCode::OK);
        let scripts = body["scripts"].as_array().expect("a scripts array");
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0]["name"], "web_search");
    })
    .await;
}

/// Without an agent the answer is the machine-wide directory alone.
#[tokio::test]
async fn the_listing_without_an_agent_is_the_global_directory() {
    with_home(|home| async move {
        write(&global_root(&home).join("shared.rhai"), GOOD_TOOL);
        let (status, body) = get_json("/api/scripts").await;

        assert_eq!(status, StatusCode::OK);
        let scripts = body["scripts"].as_array().expect("a scripts array");
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0]["source"], "global");
    })
    .await;
}

#[tokio::test]
async fn the_listing_refuses_a_traversing_agent_name() {
    with_home(|_home| async move {
        let (status, _) = get_json("/api/scripts?agent=..%2F..%2Fetc").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

// ─── GET /api/scripts?include=candidates ────────────────────────────────────

/// An agent whose directory holds one declared tool, one declared validator in
/// a subdirectory, and one `.rhai` nothing names.
fn agent_with_a_draft(home: &Path) -> PathBuf {
    let agent = agent_root(home, "picker");
    write(
        &agent.join("agent.leviath"),
        r#"
[agent]
name = "picker"
version = "0.1.0"
description = "d"

[agent.output]
validator = "validators/a2ui.rhai"

[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Main"
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#,
    );
    write(&agent.join("tools").join("summarize.rhai"), GOOD_TOOL);
    write(
        &agent.join("validators").join("a2ui.rhai"),
        "fn validate(content) { #{ valid: true } }",
    );
    write(
        &agent.join("validators").join("draft.rhai"),
        "fn validate(content) { #{ valid: false, reason: \"wip\" } }",
    );
    agent
}

/// Without the parameter the answer is the one a client already gets: the
/// declared scripts and nothing else, whatever else is lying beside the agent.
///
/// Asserted as whole objects rather than field by field, because the promise
/// being kept here is about the shape a current client parses, not about one
/// key of it.
#[tokio::test]
async fn the_default_listing_holds_only_what_is_declared() {
    with_home(|home| async move {
        let agent = agent_with_a_draft(&home);
        let (status, body) = get_json("/api/scripts?agent=picker").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({
                "scripts": [
                    {
                        "kind": "tool",
                        "name": "summarize",
                        "source": "agent",
                        "agent": "picker",
                        "path": agent.join("tools").join("summarize.rhai").display().to_string(),
                        "relative_path": "tools/summarize.rhai",
                        "declared": true,
                        "compiles": true,
                    },
                    {
                        "kind": "output_validator",
                        "name": "validators/a2ui",
                        "source": "agent",
                        "agent": "picker",
                        "path": agent
                            .join("validators")
                            .join("a2ui.rhai")
                            .display()
                            .to_string(),
                        "relative_path": "validators/a2ui.rhai",
                        "declared": true,
                        "compiles": true,
                    },
                ]
            })
        );
    })
    .await;
}

/// With it, the file nothing declares is offered too - which is the whole
/// point: a picker cannot ask somebody to declare a validator it will only
/// show once it has been declared.
#[tokio::test]
async fn include_candidates_adds_the_files_nothing_declares() {
    with_home(|home| async move {
        let agent = agent_with_a_draft(&home);
        let (status, body) = get_json("/api/scripts?agent=picker&include=candidates").await;

        assert_eq!(status, StatusCode::OK);
        let scripts = body["scripts"].as_array().expect("an array");
        // The two declared entries are unchanged and the draft is the third.
        assert_eq!(scripts.len(), 3);
        assert_eq!(
            scripts[2],
            serde_json::json!({
                "kind": "unknown",
                "name": "validators/draft",
                "source": "agent",
                "agent": "picker",
                "path": agent
                    .join("validators")
                    .join("draft.rhai")
                    .display()
                    .to_string(),
                "relative_path": "validators/draft.rhai",
                "declared": false,
            })
        );
        // A declared file is not repeated as a candidate, and neither is a
        // tool the `tools/` scan already reported.
        let names: Vec<&str> = scripts
            .iter()
            .map(|s| s["name"].as_str().expect("a name"))
            .collect();
        assert_eq!(names, ["summarize", "validators/a2ui", "validators/draft"]);

        // `kind` is not a spelling the routes accept, so a client has to pick
        // one - and the name the candidate carries opens the file under it.
        let (status, _) = get_json("/api/scripts/unknown/validators%2Fdraft?agent=picker").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, body) =
            get_json("/api/scripts/output_validator/validators%2Fdraft?agent=picker").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "validators/draft");
        assert_eq!(body["compiles"], true);
    })
    .await;
}

/// Only `.rhai` files, and only ones these routes could name. A directory that
/// ends in `.rhai` is a directory, and a file with nothing before the extension
/// has no name left to be addressed by.
#[tokio::test]
async fn a_candidate_is_a_rhai_file_with_a_name_that_can_be_addressed() {
    with_home(|home| async move {
        let agent = agent_root(&home, "mixed");
        write(&agent.join("notes.txt"), "not a script");
        write(&agent.join("real.rhai"), "1");
        write(&agent.join(".rhai"), "1");
        write(&agent.join("has a space.rhai"), "1");
        std::fs::create_dir_all(agent.join("looks.rhai")).expect("the directory");

        let (status, body) = get_json("/api/scripts?agent=mixed&include=candidates").await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<&str> = body["scripts"]
            .as_array()
            .expect("an array")
            .iter()
            .map(|s| s["name"].as_str().expect("a name"))
            .collect();
        assert_eq!(names, ["real"]);
    })
    .await;
}

/// An agent with no directory at all answers with an empty listing rather than
/// an error: `?include=candidates` is a question about files, and "none" is a
/// perfectly good answer to it.
#[tokio::test]
async fn an_agent_with_no_directory_has_no_candidates() {
    with_home(|_home| async move {
        let (status, body) = get_json("/api/scripts?agent=ghost&include=candidates").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["scripts"].as_array().expect("an array").is_empty());
    })
    .await;
}

/// The scan stops at [`CANDIDATE_MAX_DEPTH`], so an agent directory that
/// happens to contain a source tree cannot be walked to the bottom by anybody
/// holding the token.
#[tokio::test]
async fn the_candidate_scan_stops_at_the_depth_limit() {
    with_home(|home| async move {
        let agent = agent_root(&home, "deep");
        let mut dir = agent.clone();
        // One file at every level, including one past the limit.
        for level in 0..=CANDIDATE_MAX_DEPTH + 1 {
            write(&dir.join(format!("at{level}.rhai")), "1");
            dir = dir.join(format!("d{level}"));
        }

        let (status, body) = get_json("/api/scripts?agent=deep&include=candidates").await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<&str> = body["scripts"]
            .as_array()
            .expect("an array")
            .iter()
            .map(|s| s["name"].as_str().expect("a name"))
            .collect();
        assert_eq!(names.len(), CANDIDATE_MAX_DEPTH + 1);
        assert!(names.contains(&"at0"), "{names:?}");
        assert!(
            !names
                .iter()
                .any(|n| n.ends_with(&format!("at{}", CANDIDATE_MAX_DEPTH + 1)))
        );
    })
    .await;
}

/// And at [`CANDIDATE_MAX_FILES`], so a directory holding thousands of scripts
/// answers with a list rather than with all of them.
#[tokio::test]
async fn the_candidate_scan_stops_at_the_file_limit() {
    with_home(|home| async move {
        let agent = agent_root(&home, "many");
        for n in 0..CANDIDATE_MAX_FILES + 5 {
            write(&agent.join(format!("s{n:04}.rhai")), "1");
        }

        let (status, body) = get_json("/api/scripts?agent=many&include=candidates").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["scripts"].as_array().expect("an array").len(),
            CANDIDATE_MAX_FILES
        );
    })
    .await;
}

/// And at [`CANDIDATE_MAX_DIRS`], because depth alone does not bound a tree
/// that is wide rather than deep.
#[tokio::test]
async fn the_candidate_scan_stops_at_the_directory_limit() {
    with_home(|home| async move {
        let agent = agent_root(&home, "wide");
        // Each subdirectory holds one script, so the count of files reported is
        // the count of directories the walk got to open.
        for n in 0..CANDIDATE_MAX_DIRS + 5 {
            write(&agent.join(format!("d{n:04}")).join("s.rhai"), "1");
        }

        let (status, body) = get_json("/api/scripts?agent=wide&include=candidates").await;
        assert_eq!(status, StatusCode::OK);
        // The agent's own directory is the first one opened, so one fewer of
        // the subdirectories is reached.
        assert_eq!(
            body["scripts"].as_array().expect("an array").len(),
            CANDIDATE_MAX_DIRS - 1
        );
    })
    .await;
}

/// A symlink out of the agent's directory is not followed and not reported.
/// The scan runs over a directory a config file chose, and the agents directory
/// itself may be a symlink, so containment is asked of every entry rather than
/// assumed from where the walk started.
#[cfg(unix)]
#[tokio::test]
async fn a_symlink_out_of_the_agent_directory_is_not_a_candidate() {
    with_home(|home| async move {
        let outside = tempfile::tempdir().expect("a temp dir");
        std::fs::write(outside.path().join("stolen.rhai"), "1").expect("the file");
        let agent = agent_root(&home, "linked");
        write(&agent.join("own.rhai"), "1");
        std::os::unix::fs::symlink(outside.path(), agent.join("escape")).expect("the link");
        std::os::unix::fs::symlink(
            outside.path().join("stolen.rhai"),
            agent.join("escape.rhai"),
        )
        .expect("the link");

        let (status, body) = get_json("/api/scripts?agent=linked&include=candidates").await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<&str> = body["scripts"]
            .as_array()
            .expect("an array")
            .iter()
            .map(|s| s["name"].as_str().expect("a name"))
            .collect();
        assert_eq!(names, ["own"]);
    })
    .await;
}

/// Windows twin of `a_symlink_out_of_the_agent_directory_is_not_a_candidate`.
/// Windows spells the two links `symlink_dir` and `symlink_file`; the fence is
/// the same one, because `resolves_within` canonicalizes before comparing.
#[cfg(windows)]
#[tokio::test]
async fn a_symlink_out_of_the_agent_directory_is_not_a_candidate_windows() {
    with_home(|home| async move {
        let outside = tempfile::tempdir().expect("a temp dir");
        std::fs::write(outside.path().join("stolen.rhai"), "1").expect("the file");
        let agent = agent_root(&home, "linked");
        write(&agent.join("own.rhai"), "1");
        std::os::windows::fs::symlink_dir(outside.path(), agent.join("escape")).expect("the link");
        std::os::windows::fs::symlink_file(
            outside.path().join("stolen.rhai"),
            agent.join("escape.rhai"),
        )
        .expect("the link");

        let (status, body) = get_json("/api/scripts?agent=linked&include=candidates").await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<&str> = body["scripts"]
            .as_array()
            .expect("an array")
            .iter()
            .map(|s| s["name"].as_str().expect("a name"))
            .collect();
        assert_eq!(names, ["own"]);
    })
    .await;
}

/// `include` takes a comma-separated list, an empty one asks for nothing, and
/// a token nobody serves is a 400 rather than a silently short answer.
#[tokio::test]
async fn include_refuses_a_token_it_does_not_serve() {
    with_home(|home| async move {
        agent_with_a_draft(&home);

        let (status, body) = get_json("/api/scripts?agent=picker&include=candidate").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let reason = body["error"].as_str().expect("a reason");
        assert!(reason.contains("candidate"), "{reason}");

        let (status, body) = get_json("/api/scripts?agent=picker&include=").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["scripts"].as_array().expect("an array").len(), 2);

        let (status, body) = get_json("/api/scripts?agent=picker&include=,candidates").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["scripts"].as_array().expect("an array").len(), 3);
    })
    .await;
}

/// Without an agent the parameter changes nothing, and says so rather than
/// failing: every file in the two global directories is already listed from
/// disk, so there is no undeclared half of that answer to add.
#[tokio::test]
async fn include_candidates_without_an_agent_changes_nothing() {
    with_home(|home| async move {
        write(&global_root(&home).join("shared.rhai"), GOOD_TOOL);
        write(&providers_root(&home).join("groq.rhai"), GOOD_PROVIDER);

        let (plain_status, plain) = get_json("/api/scripts").await;
        let (status, body) = get_json("/api/scripts?include=candidates").await;
        assert_eq!(plain_status, StatusCode::OK);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(plain, body);
        assert_eq!(body["scripts"].as_array().expect("an array").len(), 2);
    })
    .await;
}

// ─── GET /api/scripts/{kind}/{name} ─────────────────────────────────────────

#[tokio::test]
async fn reading_a_script_returns_its_text_and_its_verdict() {
    with_home(|home| async move {
        write(
            &agent_root(&home, "researcher")
                .join("tools")
                .join("web_search.rhai"),
            GOOD_TOOL,
        );
        let (status, body) = get_json("/api/scripts/tool/web_search?agent=researcher").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["content"], GOOD_TOOL);
        assert_eq!(body["kind"], "tool");
        assert_eq!(body["name"], "web_search");
        assert_eq!(body["source"], "agent");
        assert_eq!(body["agent"], "researcher");
        assert_eq!(body["compiles"], true);
        assert!(body.get("error").is_none());
    })
    .await;
}

#[tokio::test]
async fn reading_a_script_that_does_not_compile_still_returns_it() {
    with_home(|home| async move {
        write(&global_root(&home).join("bad.rhai"), "// nothing\nlet");
        let (status, body) = get_json("/api/scripts/tool/bad").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["compiles"], false);
        assert!(!body["error"].as_str().expect("a reason").is_empty());
    })
    .await;
}

#[tokio::test]
async fn reading_a_script_that_is_not_there_is_a_404() {
    with_home(|_home| async move {
        let (status, _) = get_json("/api/scripts/tool/absent").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    })
    .await;
}

/// A directory sitting where a script should be is refused rather than read
/// through. That is the same refusal a planted symlink meets, and it is the one
/// that can be arranged portably in a test.
#[tokio::test]
async fn a_target_that_is_not_a_plain_file_is_refused() {
    with_home(|home| async move {
        std::fs::create_dir_all(global_root(&home).join("odd.rhai")).expect("the directory");
        let (status, body) = get_json("/api/scripts/tool/odd").await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        let message = body["error"].as_str().expect("a message");
        assert!(message.contains("not a plain file"), "{message}");
    })
    .await;
}

/// Text that is not UTF-8 is not a script, and the read says so instead of
/// returning half of it.
#[tokio::test]
async fn a_file_that_is_not_text_is_reported_rather_than_returned() {
    with_home(|home| async move {
        let path = global_root(&home).join("binary.rhai");
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
        std::fs::write(&path, [0xff, 0xfe, 0x00]).expect("the file");
        let (status, _) = get_json("/api/scripts/tool/binary").await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    })
    .await;
}

#[tokio::test]
async fn reading_with_an_unknown_kind_is_a_400() {
    with_home(|_home| async move {
        let (status, _) = get_json("/api/scripts/model_provider/x").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

// ─── PUT /api/scripts/{kind}/{name} ─────────────────────────────────────────

#[tokio::test]
async fn writing_a_script_creates_it_and_reports_the_verdict() {
    with_home(|home| async move {
        let (status, body) = send(
            "PUT",
            "/api/scripts/tool/summarize?agent=researcher",
            serde_json::json!({ "content": GOOD_TOOL }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["compiles"], true);
        let path = agent_root(&home, "researcher")
            .join("tools")
            .join("summarize.rhai");
        assert_eq!(std::fs::read_to_string(&path).expect("the file"), GOOD_TOOL);
    })
    .await;
}

/// Work in progress is still saved: a tool that does not compile is skipped at
/// spawn rather than breaking the agent, so refusing the write would only cost
/// the author their draft.
#[tokio::test]
async fn writing_a_script_that_does_not_compile_saves_it_and_says_so() {
    with_home(|home| async move {
        let (status, body) = send(
            "PUT",
            "/api/scripts/tool/draft?agent=researcher",
            serde_json::json!({ "content": "// nothing\nlet" }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["compiles"], false);
        assert!(!body["error"].as_str().expect("a reason").is_empty());
        assert!(
            agent_root(&home, "researcher")
                .join("tools")
                .join("draft.rhai")
                .exists()
        );
    })
    .await;
}

/// A hook is written beside the manifest, not under `tools/`, because that is
/// where the manifest's own path would resolve it.
#[tokio::test]
async fn writing_a_hook_puts_it_beside_the_manifest() {
    with_home(|home| async move {
        let (status, _) = send(
            "PUT",
            "/api/scripts/stage_hook/hooks?agent=researcher",
            serde_json::json!({ "content": "fn on_stage_enter(ctx) { () }" }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(agent_root(&home, "researcher").join("hooks.rhai").exists());
    })
    .await;
}

#[tokio::test]
async fn writing_over_a_directory_is_refused() {
    with_home(|home| async move {
        std::fs::create_dir_all(global_root(&home).join("odd.rhai")).expect("the directory");
        let (status, _) = send(
            "PUT",
            "/api/scripts/tool/odd",
            serde_json::json!({ "content": GOOD_TOOL }),
        )
        .await;

        assert_eq!(status, StatusCode::FORBIDDEN);
    })
    .await;
}

/// A directory that cannot be created is reported rather than swallowed. An
/// ordinary file where the agent's directory should be is the portable way to
/// arrange that.
#[tokio::test]
async fn a_directory_that_cannot_be_created_is_reported() {
    with_home(|home| async move {
        let agent = agent_root(&home, "afile");
        write(&agent, "this is a file, not a directory");
        let (status, _) = send(
            "PUT",
            "/api/scripts/tool/x?agent=afile",
            serde_json::json!({ "content": GOOD_TOOL }),
        )
        .await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    })
    .await;
}

/// A name the routes cannot address is refused on the way in, so no read can
/// be talked into opening a file outside the directory it is fenced to.
#[tokio::test]
async fn reading_a_name_that_cannot_be_addressed_is_refused() {
    with_home(|home| async move {
        let agent = agent_root(&home, "researcher");
        write(&agent.join("hooks.rhai"), "fn on_stage_enter(ctx) { () }");
        for name in [
            "..%2F..%2Fevil",
            "%2Fetc%2Fpasswd",
            "hooks%2F..%2F..%2Fevil",
        ] {
            let (status, _) =
                get_json(&format!("/api/scripts/stage_hook/{name}?agent=researcher")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
        }
    })
    .await;
}

/// A write may address a subdirectory too, and creates it - so a console that
/// can show a validator declared as `validators/a2ui.rhai` can also save one.
#[tokio::test]
async fn writing_into_a_subdirectory_creates_it() {
    with_home(|home| async move {
        let (status, body) = send(
            "PUT",
            "/api/scripts/output_validator/validators%2Fa2ui?agent=researcher",
            serde_json::json!({ "content": "fn validate(content) { #{ valid: true } }" }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["compiles"], true);
        let written = agent_root(&home, "researcher")
            .join("validators")
            .join("a2ui.rhai");
        assert!(written.exists());
    })
    .await;
}

/// A subdirectory that cannot be created is reported rather than swallowed,
/// the same way the agent's own directory is. An ordinary file where the
/// subdirectory should be is the portable way to arrange that.
#[tokio::test]
async fn a_subdirectory_that_cannot_be_created_is_reported() {
    with_home(|home| async move {
        let agent = agent_root(&home, "researcher");
        write(&agent.join("validators"), "a file, not a directory");
        let (status, _) = send(
            "PUT",
            "/api/scripts/output_validator/validators%2Fa2ui?agent=researcher",
            serde_json::json!({ "content": "fn validate(content) { #{ valid: true } }" }),
        )
        .await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    })
    .await;
}

/// A symlink planted at a directory along the way is refused, and refused
/// *before* anything is created through it: the containment check runs first,
/// so the write neither lands outside the agent's directory nor leaves a new
/// directory there.
#[cfg(unix)]
#[tokio::test]
async fn writing_through_a_symlinked_subdirectory_is_refused() {
    with_home(|home| async move {
        let outside = tempfile::tempdir().expect("a temp dir");
        let agent = agent_root(&home, "researcher");
        std::fs::create_dir_all(&agent).expect("the agent directory");
        std::os::unix::fs::symlink(outside.path(), agent.join("validators")).expect("the link");

        let (status, _) = send(
            "PUT",
            "/api/scripts/output_validator/validators%2Fdeep%2Fa2ui?agent=researcher",
            serde_json::json!({ "content": "fn validate(content) { #{ valid: true } }" }),
        )
        .await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(!outside.path().join("deep").exists());
    })
    .await;
}

/// Windows twin of `writing_through_a_symlinked_subdirectory_is_refused`.
/// Windows spells a directory link `symlink_dir`; the fence is the same one.
#[cfg(windows)]
#[tokio::test]
async fn writing_through_a_symlinked_subdirectory_is_refused_windows() {
    with_home(|home| async move {
        let outside = tempfile::tempdir().expect("a temp dir");
        let agent = agent_root(&home, "researcher");
        std::fs::create_dir_all(&agent).expect("the agent directory");
        std::os::windows::fs::symlink_dir(outside.path(), agent.join("validators"))
            .expect("the link");

        let (status, _) = send(
            "PUT",
            "/api/scripts/output_validator/validators%2Fdeep%2Fa2ui?agent=researcher",
            serde_json::json!({ "content": "fn validate(content) { #{ valid: true } }" }),
        )
        .await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(!outside.path().join("deep").exists());
    })
    .await;
}

#[tokio::test]
async fn writing_with_a_traversing_name_is_refused() {
    with_home(|_home| async move {
        let (status, _) = send(
            "PUT",
            "/api/scripts/tool/..%2F..%2Fevil",
            serde_json::json!({ "content": GOOD_TOOL }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

// ─── DELETE /api/scripts/{kind}/{name} ──────────────────────────────────────

#[tokio::test]
async fn deleting_a_script_removes_it() {
    with_home(|home| async move {
        let path = global_root(&home).join("gone.rhai");
        write(&path, GOOD_TOOL);
        let (status, _) = send("DELETE", "/api/scripts/tool/gone", serde_json::Value::Null).await;

        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!path.exists());
    })
    .await;
}

#[tokio::test]
async fn deleting_a_script_that_is_not_there_is_a_404() {
    with_home(|_home| async move {
        let (status, _) = send(
            "DELETE",
            "/api/scripts/tool/absent",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    })
    .await;
}

#[tokio::test]
async fn deleting_with_a_traversing_name_is_refused() {
    with_home(|_home| async move {
        let (status, _) = send(
            "DELETE",
            "/api/scripts/tool/..%2F..%2Fevil",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

// ─── POST /api/scripts/validate ─────────────────────────────────────────────

#[tokio::test]
async fn validating_good_text_writes_nothing_and_says_it_is_valid() {
    with_home(|home| async move {
        let (status, body) = send(
            "POST",
            "/api/scripts/validate",
            serde_json::json!({ "kind": "tool", "content": GOOD_TOOL }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["valid"], true);
        assert!(body.get("error").is_none());
        assert!(!global_root(&home).exists(), "validation writes nothing");
    })
    .await;
}

#[tokio::test]
async fn validating_broken_text_reports_the_compilers_complaint() {
    with_home(|_home| async move {
        let (status, body) = send(
            "POST",
            "/api/scripts/validate",
            serde_json::json!({ "kind": "region_hook", "content": "fn render(ctx) {" }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["valid"], false);
        assert!(!body["error"].as_str().expect("a reason").is_empty());
    })
    .await;
}

/// The hook names a blueprint would name the file for are checked too, so an
/// editor can ask the question a spawn would ask.
#[tokio::test]
async fn validating_a_stage_hook_checks_the_hooks_it_was_named_for() {
    with_home(|_home| async move {
        let (status, body) = send(
            "POST",
            "/api/scripts/validate",
            serde_json::json!({
                "kind": "stage_hook",
                "content": "fn on_stage_exit(ctx) { () }",
                "hooks": ["on_stage_enter"],
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["valid"], false);
    })
    .await;
}

#[tokio::test]
async fn validating_an_unknown_kind_is_a_400() {
    with_home(|_home| async move {
        let (status, _) = send(
            "POST",
            "/api/scripts/validate",
            serde_json::json!({ "kind": "model_provider", "content": "1" }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

// ─── Providers ──────────────────────────────────────────────────────────────

/// A provider is the one kind with a machine-wide directory and no agent-owned
/// one, which is the inverse of the hooks.
#[tokio::test]
async fn a_provider_resolves_into_the_providers_directory() {
    with_home(|home| async move {
        let target =
            resolve(&Config::default(), "provider", "groq", None).expect("a provider is global");
        assert_eq!(target.path, providers_root(&home).join("groq.rhai"));
        assert_eq!(target.scope, "global");
        assert!(target.agent.is_none());
    })
    .await;
}

/// Nothing scopes a provider to an agent, so an `?agent=` would write a file
/// nothing would ever load. Refused rather than quietly ignored.
#[tokio::test]
async fn a_provider_with_an_agent_is_refused() {
    with_home(|_home| async move {
        let (status, body) = resolve(&Config::default(), "provider", "groq", Some("researcher"))
            .expect_err("global");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.0.error.contains("?agent="), "{}", body.0.error);
    })
    .await;
}

/// Listed whether or not the request named an agent, because the answer is the
/// same either way and a console draws one page from one call.
#[tokio::test]
async fn the_listing_carries_providers_in_both_scopes() {
    with_home(|home| async move {
        write(&providers_root(&home).join("groq.rhai"), GOOD_PROVIDER);
        write(&agent_root(&home, "researcher").join("agent.leviath"), "");

        for uri in ["/api/scripts", "/api/scripts?agent=researcher"] {
            let (status, body) = get_json(uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            let scripts = body["scripts"].as_array().expect("a scripts array");
            let listed = scripts
                .iter()
                .find(|s| s["kind"] == "provider")
                .unwrap_or_else(|| panic!("no provider listed for {uri}"));
            assert_eq!(listed["name"], "groq");
            assert_eq!(listed["source"], "global");
            assert!(listed.get("agent").is_none());
            assert_eq!(listed["compiles"], true);
        }
    })
    .await;
}

/// What the console shows without fetching and re-parsing every script.
#[tokio::test]
async fn a_listed_provider_carries_what_it_declares_about_itself() {
    with_home(|home| async move {
        write(&providers_root(&home).join("groq.rhai"), GOOD_PROVIDER);
        let (status, body) = get_json("/api/scripts").await;

        assert_eq!(status, StatusCode::OK);
        let meta = &body["scripts"][0]["provider"];
        assert_eq!(meta["provider"], "groq");
        assert_eq!(meta["description"], "an OpenAI-compatible gateway");
        assert_eq!(meta["default_model"], "llama-3.3-70b");
        assert_eq!(meta["max_context_tokens"], 128_000);
        assert_eq!(meta["supports_streaming"], true);
    })
    .await;
}

/// Only a provider has annotations to report, so nothing else carries the key
/// at all rather than carrying it empty.
#[tokio::test]
async fn a_listed_tool_carries_no_provider_metadata() {
    with_home(|home| async move {
        write(&global_root(&home).join("shared.rhai"), GOOD_TOOL);
        let (_, body) = get_json("/api/scripts").await;
        assert!(body["scripts"][0].get("provider").is_none());
    })
    .await;
}

/// A directory sitting where a script should be cannot be read, and the listing
/// says so instead of leaving the entry out. Portable: `read_to_string` refuses
/// a directory on every platform, so this needs no `cfg(unix)` twin.
#[tokio::test]
async fn a_provider_that_cannot_be_read_is_listed_as_failing() {
    with_home(|home| async move {
        std::fs::create_dir_all(providers_root(&home).join("groq.rhai")).expect("the directory");
        let (status, body) = get_json("/api/scripts").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["scripts"][0]["kind"], "provider");
        assert_eq!(body["scripts"][0]["compiles"], false);
        assert!(
            body["scripts"][0]["error"]
                .as_str()
                .expect("a reason")
                .contains("cannot read"),
            "{}",
            body["scripts"][0]["error"]
        );
    })
    .await;
}

/// Returned verbatim. The credential a provider uses comes from the config or
/// the environment, not from the source, so there is nothing here to redact and
/// an editor that saved what it was shown would overwrite the real script.
#[tokio::test]
async fn reading_a_provider_returns_its_source_and_its_annotations() {
    with_home(|home| async move {
        write(&providers_root(&home).join("groq.rhai"), GOOD_PROVIDER);
        let (status, body) = get_json("/api/scripts/provider/groq").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["content"], GOOD_PROVIDER);
        assert_eq!(body["kind"], "provider");
        assert_eq!(body["source"], "global");
        assert_eq!(body["compiles"], true);
        assert_eq!(body["provider"]["default_model"], "llama-3.3-70b");
    })
    .await;
}

#[tokio::test]
async fn reading_a_provider_with_an_agent_is_a_400() {
    with_home(|home| async move {
        write(&providers_root(&home).join("groq.rhai"), GOOD_PROVIDER);
        let (status, _) = get_json("/api/scripts/provider/groq?agent=researcher").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

#[tokio::test]
async fn writing_a_provider_creates_it_in_the_providers_directory() {
    with_home(|home| async move {
        let (status, body) = send(
            "PUT",
            "/api/scripts/provider/groq",
            serde_json::json!({ "content": GOOD_PROVIDER }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["compiles"], true);
        let path = providers_root(&home).join("groq.rhai");
        assert_eq!(
            std::fs::read_to_string(&path).expect("the file"),
            GOOD_PROVIDER
        );
    })
    .await;
}

/// The draft is still saved. A provider that will not load is skipped with a
/// warning and selection falls through, so refusing the write would cost the
/// author their work without protecting anything.
#[tokio::test]
async fn writing_a_provider_that_is_missing_inference_saves_it_and_says_so() {
    with_home(|home| async move {
        let (status, body) = send(
            "PUT",
            "/api/scripts/provider/half",
            serde_json::json!({ "content": "fn initialize(config) { #{} }" }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["compiles"], false);
        assert!(
            body["error"]
                .as_str()
                .expect("a reason")
                .contains("inference"),
            "{}",
            body["error"]
        );
        assert!(providers_root(&home).join("half.rhai").exists());
    })
    .await;
}

#[tokio::test]
async fn deleting_a_provider_removes_it() {
    with_home(|home| async move {
        let path = providers_root(&home).join("groq.rhai");
        write(&path, GOOD_PROVIDER);
        let (status, _) = send(
            "DELETE",
            "/api/scripts/provider/groq",
            serde_json::json!({}),
        )
        .await;

        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!path.exists());
    })
    .await;
}

#[tokio::test]
async fn validating_a_provider_answers_both_ways() {
    with_home(|_home| async move {
        let (status, body) = send(
            "POST",
            "/api/scripts/validate",
            serde_json::json!({ "kind": "provider", "content": GOOD_PROVIDER }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["valid"], true);

        let (status, body) = send(
            "POST",
            "/api/scripts/validate",
            serde_json::json!({ "kind": "provider", "content": "fn initialize(config) { #{} }" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["valid"], false);
        assert!(
            body["error"]
                .as_str()
                .expect("a reason")
                .contains("inference"),
            "{}",
            body["error"]
        );
    })
    .await;
}

/// Validating compiles and reads the AST. It must never run the script, or an
/// ungated route would execute whatever was posted to it.
#[tokio::test]
async fn validating_a_provider_does_not_run_initialize() {
    with_home(|_home| async move {
        let (status, body) = send(
            "POST",
            "/api/scripts/validate",
            serde_json::json!({
                "kind": "provider",
                "content": "fn initialize(config) { throw \"it ran\"; }\n\
                            fn inference(state, request) { #{} }",
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["valid"], true);
    })
    .await;
}

// ─── agents discovered through config.agent_paths ───────────────────────────

/// The whole round trip for an agent that lives in a configured path rather
/// than the installed directory: its tools are listed, its hook opens, and a
/// write lands beside its manifest, rather than in an empty
/// `~/.leviath/agents/<name>/`.
#[tokio::test]
async fn the_routes_see_an_agent_discovered_through_agent_paths() {
    with_home(|home| async move {
        let workspace = home.join("workspace");
        let agent = workspace.join("researcher");
        write(&agent.join("agent.leviath"), manifest_with_hooks());
        write(&agent.join("tools").join("web_search.rhai"), GOOD_TOOL);
        write(&agent.join("hooks.rhai"), "fn on_stage_enter(ctx) { () }");
        let paths = vec![workspace];

        let (status, body) = call_with_paths(
            paths.clone(),
            Request::builder()
                .uri("/api/scripts?agent=researcher")
                .body(Body::empty())
                .expect("a request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let scripts = body["scripts"].as_array().expect("a scripts array");
        let own = scripts
            .iter()
            .find(|s| s["name"] == "web_search")
            .expect("the agent's own tool");
        assert_eq!(own["source"], "agent");
        assert!(
            own["path"]
                .as_str()
                .expect("a path")
                .starts_with(&agent.display().to_string()),
            "{}",
            own["path"]
        );

        let (status, body) = call_with_paths(
            paths.clone(),
            Request::builder()
                .uri("/api/scripts/stage_hook/hooks?agent=researcher")
                .body(Body::empty())
                .expect("a request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["content"], "fn on_stage_enter(ctx) { () }");

        let (status, body) = call_with_paths(
            paths.clone(),
            Request::builder()
                .method("PUT")
                .uri("/api/scripts/tool/summarize?agent=researcher")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({ "content": GOOD_TOOL })).expect("json"),
                ))
                .expect("a request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(agent.join("tools").join("summarize.rhai").is_file());
        assert!(!agent_root(&home, "researcher").exists());

        let (status, _) = call_with_paths(
            paths,
            Request::builder()
                .method("DELETE")
                .uri("/api/scripts/tool/summarize?agent=researcher")
                .body(Body::empty())
                .expect("a request"),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!agent.join("tools").join("summarize.rhai").exists());
    })
    .await;
}

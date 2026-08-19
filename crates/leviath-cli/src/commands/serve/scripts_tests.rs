//! Tests for the script read/write routes.

use super::*;
use crate::commands::serve::testutil::with_home;
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
fn admin_router() -> Router {
    Router::new()
        .route("/api/scripts", get(list_scripts))
        .route("/api/scripts/validate", post(validate_script))
        .route(
            "/api/scripts/{kind}/{name}",
            get(get_script).put(put_script).delete(delete_script),
        )
}

async fn call(req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let resp = admin_router().oneshot(req).await.expect("a response");
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
        let target = resolve("tool", "summarize", Some("researcher")).expect("resolves");
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
        let target = resolve("stage_hook", "hooks", Some("researcher")).expect("resolves");
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
        let target = resolve("tool", "summarize", None).expect("resolves");
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
        let bare = resolve("tool", "summarize", None).expect("resolves");
        let dotted = resolve("tool", "summarize.rhai", None).expect("resolves");
        assert_eq!(bare.path, dotted.path);
        assert!(bare.path.starts_with(global_root(&home)));
    })
    .await;
}

#[tokio::test]
async fn an_unknown_kind_is_refused() {
    with_home(|_home| async move {
        let (status, _) = resolve("model_provider", "x", None).expect_err("no such kind");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

#[tokio::test]
async fn a_traversing_script_name_is_refused() {
    with_home(|_home| async move {
        let (status, _) = resolve("tool", "../../evil", None).expect_err("a traversal");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

#[tokio::test]
async fn a_traversing_agent_name_is_refused() {
    with_home(|_home| async move {
        let (status, _) = resolve("tool", "x", Some("../../etc")).expect_err("a traversal");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

/// Only tools have a machine-wide directory. A hook without an agent has no
/// directory at all, and inventing one would be inventing a layout.
#[tokio::test]
async fn a_hook_without_an_agent_is_refused() {
    with_home(|_home| async move {
        let (status, body) = resolve("region_hook", "x", None).expect_err("no scope");
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
        path: PathBuf::from("no-such-file-for-the-guard-test.rhai"),
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
            path: dir.join("missing").join("x.rhai"),
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
            dir,
            path,
            scope: "global",
            agent: None,
        };
        let (status, _) = remove_script(&target).expect_err("a directory is not a file");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    })
    .await;
}

// ─── addressable_name ───────────────────────────────────────────────────────

#[test]
fn only_a_bare_rhai_filename_is_addressable() {
    assert_eq!(addressable_name("hooks.rhai"), Some("hooks".to_string()));
    // A subdirectory, a traversal, a missing extension and a name that is not a
    // safe component are all things these routes cannot address.
    assert_eq!(addressable_name("nested/hooks.rhai"), None);
    assert_eq!(addressable_name("../hooks.rhai"), None);
    assert_eq!(addressable_name("hooks.txt"), None);
    assert_eq!(addressable_name("..rhai"), None);
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

/// A hook declared in a subdirectory is outside what these routes can address,
/// so it is left out rather than listed under a name that fetches nothing.
#[tokio::test]
async fn a_hook_declared_in_a_subdirectory_is_left_out() {
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
        let (status, body) = get_json("/api/scripts?agent=nested").await;

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
        let target = resolve("provider", "groq", None).expect("a provider is global");
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
        let (status, body) = resolve("provider", "groq", Some("researcher")).expect_err("global");
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

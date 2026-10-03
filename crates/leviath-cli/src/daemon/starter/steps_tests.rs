//! The starter's own steps: what it warms, what it writes, and what it does
//! when a step fails.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use leviath_runtime::ProviderRegistry;
use leviath_runtime::host::RunStarter;
use leviath_runtime::runfile::RunFileReader;
use leviath_runtime::spec::env::Caller;
use leviath_runtime::spec::graph::RunGraph;
use leviath_runtime::spec::request::{SpawnRequest, SpawnSource};
use leviath_runtime::state::RunStatus;

use super::testing::{
    blueprint_with_mcp, manifest_in, run_on_disk, starter, stub_server_py, task_request,
};
use super::*;
use crate::config::Config;
use crate::test_support::FakeProvider;

/// The fake providers the blueprints here name.
fn registry() -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    for name in ["anthropic", "fake"] {
        registry.register(
            name.to_string(),
            Arc::new(FakeProvider::new().context_window(100_000)),
        );
    }
    registry
}

/// A request for a blueprint directory that is not there.
fn nowhere() -> SpawnRequest {
    let gone = std::env::temp_dir().join("no-such-blueprint-anywhere");
    SpawnRequest::new(SpawnSource::BlueprintFile(
        leviath_runtime::spec::names::BlueprintPath::new(gone.to_string_lossy()).unwrap(),
    ))
}

/// The graph of the blueprint `text`.
fn graph_of_manifest(text: &str) -> RunGraph {
    leviath_blueprint::BlueprintFile::parse(text)
        .expect("the blueprint reads")
        .run_graph()
}

/// The coder blueprint, in `dir`.
fn coder(dir: &Path) -> PathBuf {
    manifest_in(dir, &crate::test_support::inline_coder_manifest())
}

/// A blueprint in `dir` whose `parallel` stage fans out to `worker`, a
/// `WorkerSource` as TOML: `{ blueprint_file = "/path" }`,
/// `{ blueprint = { name = "x" } }`, `{ query = "x" }` or `{ stage = "w" }`.
fn fanning_out(dir: &Path, worker: &str) -> PathBuf {
    let model = "model = { models = [{ provider = \"fake\", model = \"m\" }] }";
    manifest_in(
        dir,
        &format!(
            "[blueprint]\nname = \"parent\"\nversion = \"0.1.0\"\n\n[graph]\nentry = \"main\"\n\
             layout = {{ total_budget_tokens = 200, regions = [{{ name = \"task\", kind = \"pinned\", budget = 200 }}] }}\n\
             inputs = [{{ name = \"task\", type = \"text\", binds = [{{ region = \"task\" }}] }}]\n\
             edges = [{{ name = \"parallel\", from = \"main\", to = \"parallel\" }}]\n\n\
             [[graph.stages]]\nname = \"main\"\n{model}\n\n\
             [[graph.stages]]\nname = \"parallel\"\n{model}\nmode = {{ fan_out = {{ worker = {worker}, split_prompt = \"go\" }} }}\n\n\
             [[graph.stages]]\nname = \"w\"\nallow_as_worker = true\n{model}\n"
        ),
    )
}

/// Every entry across every stage, once each, sorted, with the provider half
/// dropped; a stage that names no model warms the default it would run on.
#[test]
fn a_graphs_models_are_each_named_once() {
    let graph = graph_of_manifest(
        r#"[blueprint]
name = "m"
version = "0.1.0"

[graph]
entry = "one"
edges = [
    { name = "two", from = "one", to = "two" },
    { name = "three", from = "two", to = "three" },
]

[[graph.stages]]
name = "one"
system_prompt = "x"

[graph.stages.model]
models = [{ model = "shared" }, { provider = "ollama", model = "local:latest" }]

[[graph.stages]]
name = "two"
system_prompt = "y"
model = { models = [{ model = "shared" }, { model = "other" }] }

[[graph.stages]]
name = "three"
system_prompt = "z"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-4-6" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    );
    assert_eq!(
        model_names(&graph),
        ["claude-sonnet-4-6", "local:latest", "other", "shared"]
    );
}

#[test]
fn a_graphs_mcp_servers_are_what_the_pool_connects() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = blueprint_with_mcp(dir.path(), Path::new("/stub.py"));
    let graph = graph_of_manifest(&std::fs::read_to_string(manifest).unwrap());
    let servers = mcp_configs(&graph);
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "search");
}

#[tokio::test]
async fn a_requests_graph_is_its_blueprints_or_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let starter = starter(Config::default(), registry(), runs.path());
    let manifest = coder(dir.path());

    let graph = starter
        .graph_of(&task_request(&manifest, "t"))
        .expect("the blueprint loads");
    assert_eq!(graph.stages.len(), 3);
    let gone = SpawnRequest::new(SpawnSource::Blueprint(
        leviath_runtime::spec::names::BlueprintRef::parse("not-installed").unwrap(),
    ));
    assert!(starter.graph_of(&gone).is_none(), "resolving says why");
    assert!(starter.graph_of(&nowhere()).is_none());
    let raw = SpawnRequest::new(SpawnSource::Raw(Box::new(graph.clone())));
    assert_eq!(starter.graph_of(&raw), Some(graph));
}

/// The MCP servers of the blueprints a graph's fan-outs start, whether named
/// by directory or by name, or found by a query. A same-graph worker, a query
/// nothing matches and a worker blueprint that will not load add nothing.
#[tokio::test]
async fn a_fan_outs_worker_servers_are_found_by_path_or_query() {
    let (_stub_dir, stub) = stub_server_py();
    let agents = tempfile::tempdir().unwrap();
    // Installed under the name it gives itself.
    let worker = agents.path().join("mcpagent");
    std::fs::create_dir_all(&worker).unwrap();
    blueprint_with_mcp(&worker, &stub);
    let runs = tempfile::tempdir().unwrap();
    let mut starter = starter(Config::default(), registry(), runs.path());
    starter.agents_dir = Some(agents.path().to_path_buf());

    let names = |worker_line: &str| {
        let dir = tempfile::tempdir().unwrap();
        let manifest = fanning_out(dir.path(), worker_line);
        let graph = graph_of_manifest(&std::fs::read_to_string(manifest).unwrap());
        starter
            .worker_servers(&graph)
            .into_iter()
            .map(|s| s.name)
            .collect::<Vec<_>>()
    };
    let by_path = format!("{{ blueprint_file = '{}' }}", worker.display());
    assert_eq!(names(&by_path), ["search"]);
    assert_eq!(names("{ blueprint = { name = \"mcpagent\" } }"), ["search"]);
    assert_eq!(names("{ query = \"mcpagent\" }"), ["search"]);
    assert!(names("{ query = \"nothing-matches\" }").is_empty());
    assert!(names("{ stage = \"w\" }").is_empty());
    let missing = format!(
        "{{ blueprint_file = '{}' }}",
        agents.path().join("no-such-worker").display()
    );
    assert!(names(&missing).is_empty());
}

/// Warming a run connects its own MCP servers and its fan-out workers', so
/// the first stage and the first worker both have their tools.
#[tokio::test]
async fn warming_connects_the_runs_servers_and_its_workers() {
    let (_stub_dir, stub) = stub_server_py();
    let worker = tempfile::tempdir().unwrap();
    let worker_manifest = blueprint_with_mcp(worker.path(), &stub);
    let parent = tempfile::tempdir().unwrap();
    let manifest = fanning_out(
        parent.path(),
        &format!("{{ blueprint_file = '{}' }}", worker.path().display()),
    );
    let runs = tempfile::tempdir().unwrap();
    let starter = starter(Config::default(), registry(), runs.path());

    starter
        .warm(&task_request(&manifest, "t"), &Config::default())
        .await;

    let worker_graph = graph_of_manifest(&std::fs::read_to_string(worker_manifest).unwrap());
    let defs = starter
        .mcp_pool
        .cached_defs_for(&mcp_configs(&worker_graph));
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].name, "search__stub_search");

    // A request whose blueprint does not load warms the global set and
    // nothing else, and resolving says why a moment later.
    starter.warm(&nowhere(), &Config::default()).await;
}

/// Checking a run says what it would be and starts nothing.
#[tokio::test]
async fn validate_says_what_a_run_would_be_without_starting_it() {
    let dir = tempfile::tempdir().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let starter = starter(Config::default(), registry(), runs.path());
    let manifest = coder(dir.path());

    let summary = starter
        .validate(task_request(&manifest, "t"), Caller::TopLevel)
        .await
        .expect("the run checks");
    assert_eq!(summary.entry_stage.as_str(), "analyze");
    assert_eq!(summary.stages.len(), 3);
    assert_eq!(std::fs::read_dir(runs.path()).unwrap().count(), 0);

    assert!(starter.validate(nowhere(), Caller::TopLevel).await.is_err());
}

/// The host reaches the starter through the trait: a start, a check, and the
/// world brought up to date before a run is placed in it.
#[tokio::test]
async fn the_host_starts_and_checks_through_the_trait() {
    let dir = tempfile::tempdir().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.limits.max_concurrent_tools = 7;
    let starter: Arc<dyn RunStarter> = Arc::new(starter(config, registry(), runs.path()));
    let manifest = coder(dir.path());

    let prepared = starter
        .start(task_request(&manifest, "t"), Caller::TopLevel)
        .await
        .expect("the run starts");
    assert!(
        runs.path()
            .join(prepared.spec.run_id.as_str())
            .join(leviath_core::files::RUN_FILE)
            .is_file()
    );
    assert!(
        starter
            .check(task_request(&manifest, "t"), Caller::TopLevel)
            .await
            .is_ok()
    );

    let mut world = leviath_runtime::PipelineWorld::new(
        ProviderRegistry::new(),
        Arc::new(CliToolService::new()),
        leviath_runtime::inference_pool::InferencePoolConfig::new(),
        1,
        None,
        tokio::runtime::Handle::current(),
    );
    starter.before_insert(&mut world, &prepared.spec);
    assert_eq!(
        world.tool_concurrency(),
        7,
        "the world runs on the limits the config names now"
    );
}

/// A run whose file cannot be written is refused, saying so, rather than
/// started with nothing on disk to resume it from.
#[tokio::test]
async fn a_run_whose_file_cannot_be_written_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("runs");
    std::fs::write(&blocker, "a file, not a directory").unwrap();
    let starter = starter(Config::default(), registry(), &blocker);
    let request = task_request(&coder(dir.path()), "t");
    let env = starter.env_for(&request, starter.config.current());

    let issues = starter
        .start_with(env, request, Caller::TopLevel)
        .await
        .expect_err("there is nowhere to write the run");
    assert!(
        issues.to_string().contains("could not be written"),
        "{issues}"
    );
}

/// A run that resolved and then could not be bound ends on its file, saying
/// why; one whose file is gone by then is logged, not fatal.
#[tokio::test]
async fn a_refusal_after_the_file_is_written_is_recorded_on_it() {
    let dir = tempfile::tempdir().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let run_id = run_on_disk(
        Config::default(),
        registry(),
        runs.path(),
        &coder(dir.path()),
    );
    let path = runs
        .path()
        .join(&run_id)
        .join(leviath_core::files::RUN_FILE);
    let state = RunFileReader::open(&path).unwrap().latest_state().unwrap();
    let issues: SpawnIssues = refusal(IssueCode::Unavailable, "the provider went away");

    record_failed(&path, &state, &issues).expect("the refusal is written");
    match RunFileReader::open(&path)
        .unwrap()
        .latest_state()
        .unwrap()
        .status
    {
        RunStatus::Error(why) => assert!(why.contains("the provider went away")),
        other => panic!("not recorded: {other:?}"),
    }
    // It ended before it entered a stage, so every stage reads never reached
    // rather than still to come.
    let ledger = RunFileReader::open(&path)
        .unwrap()
        .latest_state()
        .unwrap()
        .ledger;
    let unreached = ledger
        .iter()
        .filter(|r| r.status == leviath_runtime::state::StageStatus::Skipped)
        .count();
    assert_eq!((unreached, ledger.is_empty()), (ledger.len(), false));
    assert!(record_failed(&dir.path().join("gone.lvr"), &state, &issues).is_err());

    crate::test_support::with_tracing(|| {
        let starter = starter(Config::default(), registry(), runs.path());
        let request = task_request(&coder(dir.path()), "t");
        let env = starter.env_for(&request, starter.config.current());
        let resolved = crate::daemon::block_on::block_on(resolve(
            &request,
            &Caller::TopLevel,
            &env,
            ResolveMode::Spawn,
        ))
        .expect("the run resolves");
        // Never recorded, so there is no file to write the refusal to.
        starter.record_refusal(&resolved, &state, &issues);
    });
}

/// A store whose writes all fail.
struct Refusing;

impl leviath_core::mime::BlobStore for Refusing {
    fn put(
        &self,
        _run_id: &str,
        _blob: &leviath_core::mime::Blob,
        _reg: &leviath_core::mime::MimeRegistry,
    ) -> std::io::Result<leviath_core::mime::BlobRef> {
        Err(std::io::Error::other("full"))
    }
    fn read(&self, _run_id: &str, _sha256: &str) -> std::io::Result<Arc<[u8]>> {
        Err(std::io::Error::other("empty"))
    }
    fn copy(&self, _from: &str, _to: &str, _sha256: &str) -> std::io::Result<()> {
        Err(std::io::Error::other("empty"))
    }
    fn list(&self, _run_id: &str) -> std::io::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// A run's attached files go to its blob directory, where its tools and
/// every reader find them, and never into its run file; a store that
/// refuses one refuses the run.
#[tokio::test]
async fn a_runs_files_are_stored_beside_its_run_file_never_in_it() {
    let dir = tempfile::tempdir().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut starter = starter(Config::default(), registry(), runs.path());
    let image = b"\x89PNG\r\n\x1a\n0000IHDR a picture worth finding".to_vec();
    let request = || {
        let mut request = task_request(&coder(dir.path()), "look at this");
        request.attachments = vec![crate::daemon::requests::attachment(
            leviath_core::mime::InboundPart::from_bytes("hero.png", image.clone()),
        )];
        request
    };
    let env = starter.env_for(&request(), starter.config.current());
    let prepared = starter
        .start_with(env, request(), Caller::TopLevel)
        .await
        .expect("the run starts");
    let run_id = prepared.spec.run_id.to_string();
    let digest = leviath_runtime::spec::names::Digest::of(&image);
    assert!(
        starter.blob_store.has(&run_id, digest.as_str()),
        "the image is where the run's tools read it"
    );
    let file = std::fs::read(
        runs.path()
            .join(&run_id)
            .join(leviath_core::files::RUN_FILE),
    )
    .unwrap();
    assert!(
        !file.windows(image.len()).any(|w| w == image.as_slice()),
        "and not in the run file"
    );

    starter.blob_store = Arc::new(Refusing);
    let env = starter.env_for(&request(), starter.config.current());
    let Err(refused) = starter.start_with(env, request(), Caller::TopLevel).await else {
        panic!("a file that cannot be stored refuses the run");
    };
    assert!(refused.to_string().contains("full"), "{refused}");
}

/// A run that resolves and then cannot be bound (here, a grant in the
/// operator's own config that does not read) is refused, and the refusal
/// is the last step of its file, so it ends there rather than sitting as a
/// run that never moved.
#[tokio::test]
async fn a_run_refused_at_its_binding_ends_on_its_file() {
    let dir = tempfile::tempdir().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let manifest = manifest_in(
        dir.path(),
        r#"[blueprint]
name = "reader"
version = "0.1.0"
description = "d"

[graph]
read_paths = ["/tmp"]

[[graph.stages]]
name = "main"
system_prompt = "be brief"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
regions = [{ name = "task", kind = "pinned", budget = 4000 }]
total_budget_tokens = 4000

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
binds = [{ region = "task" }]
"#,
    );
    let mut config = Config::default();
    config.security.read_paths = vec!["glob:[".to_string()];
    let starter = starter(config, registry(), runs.path());
    let request = task_request(&manifest, "t");
    let env = starter.env_for(&request, starter.config.current());
    let issues = starter
        .start_with(env, request, Caller::TopLevel)
        .await
        .expect_err("the grant does not read");
    let run_dir = std::fs::read_dir(runs.path())
        .unwrap()
        .flatten()
        .next()
        .expect("the run was recorded before it was bound")
        .path();
    let state = RunFileReader::open(&run_dir.join(leviath_core::files::RUN_FILE))
        .unwrap()
        .latest_state()
        .unwrap();
    assert_eq!(state.status, RunStatus::Error(issues.to_string()));
}

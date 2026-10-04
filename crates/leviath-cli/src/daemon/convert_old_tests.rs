use super::*;

use std::path::PathBuf;

#[cfg(feature = "legacy-runs")]
use std::{collections::HashSet, sync::Arc};

#[cfg(feature = "legacy-runs")]
use crate::daemon::mcp_pool::McpPool;
#[cfg(feature = "legacy-runs")]
use crate::test_support::{FakeProvider, McpStub};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("leviath-legacy-runs")
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap().flatten() {
        let to = dst.join(entry.file_name());
        match entry.file_type().unwrap().is_dir() {
            true => copy_dir(&entry.path(), &to),
            false => {
                std::fs::copy(entry.path(), &to).unwrap();
            }
        }
    }
}

/// The probe blueprint, with a stdio MCP server running `stub` whose one
/// tool the stage names.
fn probe_with_server(stub: &Path) -> String {
    format!(
        r#"[agent]
name = "probe"
version = "0.1.0"
description = "One stage, one MCP server"
entry_stage = "main"

[[mcp_servers]]
name = "docs"
command = "python3"
args = [{stub:?}]

[context.regions]
task = {{ kind = "pinned", max_tokens = 2000, required = true, seed = "task" }}

[stages.main]
mode = "autonomous"
description = "Do whatever the mock provider asks"
model = {{ models = [{{ provider = "openai", model = "gpt-mock" }}] }}
available_tools = ["shell", "current_time", "docs__lookup"]
max_iterations = 4
system_prompt = "probe"
"#
    )
}

/// An old run stopped mid tool batch converts at start with the window this
/// machine gives its model and every tool its stage had, its MCP server's
/// among them; a finished one converts too, without its servers connected.
#[cfg(feature = "legacy-runs")]
#[tokio::test]
async fn old_runs_convert_at_start_with_this_machines_models_and_tools() {
    let home = tempfile::tempdir().unwrap();
    let stub_dir = tempfile::tempdir().unwrap();
    let stub = stub_dir.path().join("stub.py");
    let source = McpStub::new()
        .tool("lookup", Some("look a thing up"))
        .input_schema(r#"{"type": "object", "properties": {}}"#)
        .replying("ok")
        .source();
    std::fs::write(&stub, source).unwrap();
    let runs = home.path().join("runs");
    let old = runs.join("old");
    copy_dir(&fixture("mid-tool-batch"), &old);
    std::fs::write(old.join("blueprint.leviath"), probe_with_server(&stub)).unwrap();
    let done = runs.join("done");
    copy_dir(&fixture("finished"), &done);
    // Its blueprint holds a key the old release never read.
    let text = std::fs::read_to_string(done.join("blueprint.leviath")).unwrap();
    let text = text.replace("seed = \"task\" }", "seed = \"task\", max_stored = 4 }");
    std::fs::write(done.join("blueprint.leviath"), text).unwrap();
    std::fs::create_dir_all(runs.join("junk")).unwrap();

    let mut registry = leviath_runtime::ProviderRegistry::new();
    registry.register("openai".into(), Arc::new(FakeProvider::new()));
    let window = registry
        .get("openai")
        .unwrap()
        .max_context_tokens("gpt-mock");
    let shared = Arc::new(tokio::sync::Mutex::new(leviath_mcp::ToolExecutor::new()));
    let pool = McpPool::new(shared.clone(), HashSet::new());
    let config = crate::config::Config::default();
    let agents = fixture("agents");
    let home_path = home.path().to_str().unwrap().to_string();
    let board = StartupBoard::default();
    let upgrade = temp_env::async_with_vars([("LEVIATH_HOME", Some(home_path))], async {
        convert_at_start(
            &runs,
            AtStart {
                config: &config,
                registry,
                agents_dir: Some(&agents),
                mcp_defs: &[],
                mcp_owners: &Default::default(),
                shared_mcp: shared.clone(),
                pool: &pool,
                child: None,
            },
            &board,
        )
        .await
    })
    .await;
    let now = board.current();
    assert_eq!(
        (now.step.as_str(), now.done, now.total),
        ("converting runs", 2, 2)
    );
    assert_eq!(upgrade.converted, 2);
    let dropped: Vec<_> = upgrade.dropped_in_runs.iter().collect();
    assert_eq!(dropped.len(), 1, "{dropped:?}");
    let ((name, line), runs_of) = dropped[0];
    assert_eq!((name.as_str(), *runs_of), ("probe", 1));
    assert!(line.contains("`max_stored = 4` was dropped"), "{line}");

    let run = leviath_runtime::restore::read_for_resume(&old)
        .unwrap()
        .expect("the old run is a run file now");
    let main = run.spec.stage("main").unwrap();
    assert_eq!(main.context_window as usize, window);
    let names: Vec<&str> = main.tools.iter().map(|t| t.name.as_str()).collect();
    for name in ["shell", "current_time", "docs__lookup"] {
        assert!(names.contains(&name), "{name} in {names:?}");
    }
    assert!(done.join(leviath_core::files::RUN_FILE).is_file());
    assert!(done.join("legacy").is_dir());
}

/// An old run the daemon finds when it brings its runs back is converted
/// against the daemon's own providers and tools, and comes back with them.
#[cfg(feature = "legacy-runs")]
#[tokio::test]
async fn an_old_run_found_on_resume_converts_against_the_daemon() {
    let home = tempfile::tempdir().unwrap();
    let runs = home.path().join("runs");
    let old = runs.join("old");
    copy_dir(&fixture("mid-tool-batch"), &old);
    let mut registry = leviath_runtime::ProviderRegistry::new();
    registry.register(
        "openai".into(),
        Arc::new(FakeProvider::new().context_window(64_000)),
    );
    let starter =
        crate::daemon::starter::testing::starter(crate::config::Config::default(), registry, &runs);
    let mut world = leviath_runtime::world::PipelineWorld::new(
        starter.providers.registry(),
        starter.tool_service.clone(),
        leviath_runtime::inference_pool::InferencePoolConfig::new(),
        1,
        Some(starter.runs_dir.clone()),
        tokio::runtime::Handle::current(),
    );
    crate::daemon::recovery::resume_all(&mut world, &starter, &runs);
    let run =
        leviath_runtime::runfile::RunFileReader::open(&old.join(leviath_core::files::RUN_FILE))
            .unwrap();
    let main = run.spec().stage("main").unwrap();
    assert_eq!(main.context_window, 64_000);
    assert!(main.tools.iter().any(|t| t.name.as_str() == "shell"));
    // It recorded no child-run limit, so it has the operator's default.
    use leviath_runtime::spec::env::ResolveEnv;
    let default = starter
        .env_for_graph(&run.spec().graph, starter.config.current())
        .limits()
        .default_max_depth;
    assert!(default > 0);
    assert_eq!(run.spec().launch.max_depth, default);
}

/// A stage this machine cannot serve, or whose tools it cannot give, is
/// answered with why.
#[cfg(feature = "legacy-runs")]
#[test]
fn what_the_machine_cannot_answer_comes_back_as_why() {
    use leviath_legacy_runs::StageLookup;
    let runs = tempfile::tempdir().unwrap();
    let old = runs.path().join("old");
    copy_dir(&fixture("mid-tool-batch"), &old);
    let graph = leviath_legacy_runs::graph(&old, &Default::default()).unwrap();
    let pool = McpPool::new(
        Arc::new(tokio::sync::Mutex::new(leviath_mcp::ToolExecutor::new())),
        HashSet::new(),
    );
    let config = crate::config::Config::default();
    let start = AtStart {
        config: &config,
        registry: leviath_runtime::ProviderRegistry::new(),
        agents_dir: None,
        mcp_defs: &[],
        mcp_owners: &Default::default(),
        shared_mcp: Arc::new(tokio::sync::Mutex::new(leviath_mcp::ToolExecutor::new())),
        pool: &pool,
        child: None,
    };
    let envs = |g: &RunGraph| start.env(g);
    let lookup = Lookup(&envs);
    let mut stage = graph.stages[0].clone();
    assert!(lookup.model(&graph, &stage, None).is_err());
    stage.required_tools = vec![leviath_runtime::spec::names::ToolName::new("nope").unwrap()];
    assert!(
        lookup
            .tools(&graph, &stage, &Default::default(), None, None)
            .is_err()
    );
}

/// Converting with no runs directory does nothing, and a directory that is
/// already a run file is left as it is.
#[test]
fn nothing_to_convert_is_left_alone() {
    let runs = tempfile::tempdir().unwrap();
    let gone = convert_all(
        &runs.path().join("gone"),
        None,
        None,
        &StartupBoard::default(),
    );
    assert_eq!(gone.converted + gone.failed, 0);
    convert_one(runs.path(), None, None);
    assert!(std::fs::read_dir(runs.path()).unwrap().next().is_none());
    assert!(servers_of_unfinished(&runs.path().join("gone"), None).is_empty());
    assert!(Unconverted::path_for(Path::new("/")).ends_with("runs.unconverted"));
}

/// The list of runs that did not convert, as `convert_all` left it.
fn unconverted(runs: &Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(Unconverted::path_for(runs)).ok()?;
    Some(serde_json::from_slice(&bytes).unwrap())
}

/// A run that does not convert is left as it was and listed, and later
/// starts leave it alone rather than trying it again; a new release tries it
/// again, and the list goes once nothing is on it.
#[cfg(feature = "legacy-runs")]
#[test]
fn a_run_that_does_not_convert_is_tried_once_per_release() {
    let home = tempfile::tempdir().unwrap();
    let runs = home.path().join("runs");
    let run = runs.join("old");
    copy_dir(&fixture("finished"), &run);
    let meta = std::fs::read(run.join("meta.json")).unwrap();
    std::fs::write(run.join("meta.json"), "not json").unwrap();

    let upgrade = crate::test_support::with_tracing(|| {
        convert_all(&runs, None, None, &StartupBoard::default())
    });
    assert_eq!(upgrade.failed, 1);
    assert_eq!(upgrade.unconverted, Some(Unconverted::path_for(&runs)));
    let listed = unconverted(&runs).expect("the run is listed");
    assert_eq!(listed["version"], env!("CARGO_PKG_VERSION"));
    let why = listed["runs"]["old"].as_str().unwrap();
    assert!(why.contains("does not parse"), "{why}");
    assert!(leviath_legacy_runs::is_legacy(&run));

    // Mended, it is still left alone by this release.
    std::fs::write(run.join("meta.json"), &meta).unwrap();
    convert_all(&runs, None, None, &StartupBoard::default());
    assert!(leviath_legacy_runs::is_legacy(&run));
    assert!(unconverted(&runs).is_some());

    // A list another release wrote is tried again, and goes once empty.
    let mut older = listed;
    older["version"] = "0.0.1".into();
    std::fs::write(Unconverted::path_for(&runs), older.to_string()).unwrap();
    convert_one(&run, None, None);
    assert!(!leviath_legacy_runs::is_legacy(&run));
    assert!(unconverted(&runs).is_none());
    // A list from another release with nothing left to try is removed too.
    std::fs::write(Unconverted::path_for(&runs), older.to_string()).unwrap();
    convert_all(&runs, None, None, &StartupBoard::default());
    assert!(unconverted(&runs).is_none());
}

/// A run that cannot be backed up first is not converted, and is not listed
/// as one that does not convert: the next start tries again.
#[cfg(feature = "legacy-runs")]
#[test]
fn a_run_that_cannot_be_backed_up_is_not_converted() {
    let home = tempfile::tempdir().unwrap();
    let runs = home.path().join("runs");
    let run = runs.join("old");
    copy_dir(&fixture("finished"), &run);
    std::fs::write(home.path().join(crate::home_backup::BACKUPS_DIR), "a file").unwrap();
    let upgrade = crate::test_support::with_tracing(|| {
        convert_all(&runs, None, None, &StartupBoard::default())
    });
    assert_eq!((upgrade.converted, upgrade.failed), (0, 1));
    assert!(leviath_legacy_runs::is_legacy(&run));
    assert!(unconverted(&runs).is_none());
}

/// A list that cannot be written is said so in the log, and changes nothing
/// else.
#[cfg(feature = "legacy-runs")]
#[test]
fn a_list_that_cannot_be_written_is_said_so() {
    let home = tempfile::tempdir().unwrap();
    let runs = home.path().join("runs");
    let run = runs.join("old");
    copy_dir(&fixture("finished"), &run);
    std::fs::write(run.join("meta.json"), "not json").unwrap();
    std::fs::create_dir_all(Unconverted::path_for(&runs)).unwrap();
    crate::test_support::with_tracing(|| convert_all(&runs, None, None, &StartupBoard::default()));
    assert!(Unconverted::path_for(&runs).is_dir());
    assert!(leviath_legacy_runs::is_legacy(&run));
}

/// An old run stopped mid tool batch carries the batch on after it converts,
/// rather than dispatching it again: each call stays the one execution the
/// old journal named, and the call that was running ends once, unobserved.
#[cfg(feature = "legacy-runs")]
#[tokio::test]
async fn a_converted_batch_keeps_the_executions_the_old_journal_named() {
    let home = tempfile::tempdir().unwrap();
    let runs = home.path().join("runs");
    // Under its own run id, as an old daemon left it.
    let old = runs.join("probe-1790811952-ac34bdb9b3eb");
    copy_dir(&fixture("mid-tool-batch"), &old);
    let mut registry = leviath_runtime::ProviderRegistry::new();
    registry.register("openai".into(), Arc::new(FakeProvider::new()));
    let starter =
        crate::daemon::starter::testing::starter(crate::config::Config::default(), registry, &runs);
    let mut world = leviath_runtime::world::PipelineWorld::new(
        starter.providers.registry(),
        starter.tool_service.clone(),
        leviath_runtime::inference_pool::InferencePoolConfig::new(),
        1,
        Some(starter.runs_dir.clone()),
        tokio::runtime::Handle::current(),
    );
    crate::daemon::recovery::resume_all(&mut world, &starter, &runs);
    use leviath_runtime::state::RunEvent;
    use leviath_runtime::state::journal::ToolOutcomeState;
    let path = old.join(leviath_core::files::RUN_FILE);
    let events_of = |path: &Path| {
        let file = leviath_runtime::runfile::RunFileReader::open(path).unwrap();
        file.deltas(1, file.last_seq())
            .unwrap()
            .into_iter()
            .flat_map(|d| d.events)
            .collect::<Vec<_>>()
    };
    let endings_of = |events: &[RunEvent], call: &str| -> Vec<(String, Option<ToolOutcomeState>)> {
        events
            .iter()
            .filter_map(|e| match e {
                RunEvent::Completed {
                    call_id,
                    execution_id,
                    outcome,
                    ..
                } if call_id == call => Some((execution_id.clone(), *outcome)),
                _ => None,
            })
            .collect()
    };
    let sent_of = |events: &[RunEvent]| -> Vec<(String, String)> {
        events
            .iter()
            .filter_map(|e| match e {
                RunEvent::Dispatched {
                    call_id,
                    execution_id,
                    ..
                } => Some((call_id.clone(), execution_id.clone())),
                _ => None,
            })
            .collect()
    };
    let (call, execution) = sent_of(&events_of(&path))
        .pop()
        .expect("the old batch was dispatched");
    for _ in 0..1500 {
        let _ = tokio::time::timeout(std::time::Duration::from_millis(20), world.run()).await;
        if !endings_of(&events_of(&path), &call).is_empty() {
            break;
        }
    }
    let events = events_of(&path);
    let sent = sent_of(&events);
    let started = events
        .iter()
        .filter(|e| matches!(e, RunEvent::ToolStarted(_)))
        .count();
    assert_eq!(started, sent.len(), "{sent:?}");
    let unique: HashSet<&str> = sent.iter().map(|(c, _)| c.as_str()).collect();
    assert_eq!(
        unique.len(),
        sent.len(),
        "each call is dispatched once: {sent:?}"
    );
    assert_eq!(
        endings_of(&events, &call),
        [(execution, Some(ToolOutcomeState::Indeterminate))],
        "the running call ends once, as the execution it was"
    );
}

/// The daemon's child is this executable's `lev daemon convert-runs`, for
/// the daemon's runs, blueprints and build, logging as the daemon does.
#[test]
fn the_child_is_this_lev_converting_these_runs() {
    let exe = std::env::current_exe().unwrap();
    let cmd = ChildCmd::lev(
        exe.clone(),
        Path::new("/home/runs"),
        Some(Path::new("/home/agents")),
    );
    assert_eq!(cmd.program, exe);
    let args: Vec<String> = cmd
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let at = |flag: &str| args.iter().position(|a| a == flag).map(|i| &args[i + 1]);
    assert_eq!(&args[..2], ["daemon", "convert-runs"]);
    assert_eq!(at("--runs-dir").unwrap(), "/home/runs");
    assert_eq!(at("--agents-dir").unwrap(), "/home/agents");
    assert_eq!(at("--build").unwrap(), crate::daemon::setup::CURRENT_BUILD);
    assert_eq!(cmd.quiet_limit, ChildCmd::QUIET_LIMIT);
    assert!(cmd.env.is_empty());
    let bare = ChildCmd::lev(exe, Path::new("/home/runs"), None);
    assert!(!bare.args.iter().any(|a| a == "--agents-dir"));
}

/// A run whose conversion was stopped part way is put back and converted
/// again; one that cannot be put back is said so and left as it is.
#[cfg(feature = "legacy-runs")]
#[test]
fn a_run_stopped_part_way_converts_again() {
    let home = tempfile::tempdir().unwrap();
    let runs = home.path().join("runs");
    let run = runs.join("old");
    copy_dir(&fixture("finished"), &run);
    std::fs::create_dir_all(run.join("legacy")).unwrap();
    for name in ["meta.json", "run.lvr"] {
        std::fs::rename(run.join(name), run.join("legacy").join(name)).unwrap();
    }
    let stuck = runs.join("stuck");
    copy_dir(&fixture("finished"), &stuck);
    std::fs::create_dir_all(stuck.join("legacy")).unwrap();
    std::fs::copy(
        stuck.join("meta.json"),
        stuck.join("legacy").join("meta.json"),
    )
    .unwrap();
    std::fs::rename(stuck.join("run.lvr"), stuck.join("legacy").join("run.lvr")).unwrap();
    let upgrade = crate::test_support::with_tracing(|| {
        convert_all(&runs, None, None, &StartupBoard::default())
    });
    assert_eq!(upgrade.converted, 2);
    assert!(run.join("legacy").join("meta.json").is_file());
    assert!(leviath_runtime::restore::read_for_resume(&run).is_ok());
    assert!(stuck.join(leviath_core::files::RUN_FILE).is_file());
}

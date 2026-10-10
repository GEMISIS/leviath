use super::*;

use std::collections::HashSet;
use std::io::BufReader;
use std::time::Duration;

use leviath_runtime::spec::run_spec::RunSpec;

use crate::daemon::convert_old::convert_at_start;
use crate::daemon::mcp_pool::McpPool;
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

/// A home whose runs directory holds a finished old run whose blueprint has
/// a key the old release never read (`a-done`), an unfinished one stopped
/// mid tool batch whose stage uses an MCP server (`b-old`), and a run file an
/// alpha build wrote in layout 2 (`c-alpha`).
struct Home {
    dir: tempfile::TempDir,
    _stub: tempfile::TempDir,
    stub: PathBuf,
}

impl Home {
    fn new() -> Self {
        let stub_dir = tempfile::tempdir().unwrap();
        let stub = stub_dir.path().join("stub.py");
        let source = McpStub::new()
            .tool("lookup", Some("look a thing up"))
            .input_schema(r#"{"type": "object", "properties": {}}"#)
            .replying("ok")
            .source();
        std::fs::write(&stub, source).unwrap();
        let home = Self {
            dir: tempfile::tempdir().unwrap(),
            _stub: stub_dir,
            stub,
        };
        home.fill();
        home
    }

    fn runs(&self) -> PathBuf {
        self.dir.path().join("runs")
    }

    /// Put the three runs back, as a fresh copy, with nothing else.
    fn fill(&self) {
        for entry in std::fs::read_dir(self.dir.path()).unwrap().flatten() {
            match entry.file_type().unwrap().is_dir() {
                true => std::fs::remove_dir_all(entry.path()).unwrap(),
                false => std::fs::remove_file(entry.path()).unwrap(),
            }
        }
        let done = self.runs().join("a-done");
        copy_dir(&fixture("finished"), &done);
        let text = std::fs::read_to_string(done.join("blueprint.leviath")).unwrap();
        let text = text.replace("seed = \"task\" }", "seed = \"task\", max_stored = 4 }");
        std::fs::write(done.join("blueprint.leviath"), text).unwrap();
        let old = self.runs().join("b-old");
        copy_dir(&fixture("mid-tool-batch"), &old);
        std::fs::write(old.join("blueprint.leviath"), probe_with_server(&self.stub)).unwrap();
        copy_dir(&fixture("layout-2"), &self.runs().join("c-alpha"));
    }

    /// Each run's spec, by directory name.
    fn specs(&self) -> Vec<(String, RunSpec)> {
        ["a-done", "b-old"]
            .iter()
            .map(|name| {
                let file = self.runs().join(name).join(leviath_core::files::RUN_FILE);
                let reader = leviath_runtime::runfile::RunFileReader::open(&file).unwrap();
                (name.to_string(), reader.spec().clone())
            })
            .collect()
    }

    fn home_var(&self) -> [(&'static str, Option<String>); 1] {
        [(
            "LEVIATH_HOME",
            Some(self.dir.path().to_str().unwrap().to_string()),
        )]
    }
}

/// What the daemon has at start, in a test: one provider, no global MCP
/// servers, and `pool`.
fn at_start<'a>(
    config: &'a crate::config::Config,
    agents: &'a Path,
    pool: &'a McpPool,
    child: Option<ChildCmd>,
) -> AtStart<'a> {
    let mut registry = leviath_runtime::ProviderRegistry::new();
    registry.register(
        "openai".into(),
        Arc::new(FakeProvider::new().context_window(64_000)),
    );
    AtStart {
        config,
        registry,
        agents_dir: Some(agents),
        mcp_defs: &[],
        mcp_owners: Box::leak(Box::default()),
        shared_mcp: Arc::new(tokio::sync::Mutex::new(leviath_mcp::ToolExecutor::new())),
        pool,
        child,
    }
}

fn pool() -> Arc<McpPool> {
    Arc::new(McpPool::new(
        Arc::new(tokio::sync::Mutex::new(leviath_mcp::ToolExecutor::new())),
        HashSet::new(),
    ))
}

/// Run [`serve`] on a thread of its own, connected by pipes to [`drive`] on
/// this one, as a child process is connected to the daemon.
async fn over_pipes(
    runs: &Path,
    start: &AtStart<'_>,
    board: &StartupBoard,
) -> (Result<Tally, Stopped>, Upgrade) {
    let (answers, mut answer) = std::io::pipe().unwrap();
    let (said, says) = std::io::pipe().unwrap();
    let (runs_dir, agents) = (runs.to_path_buf(), start.agents_dir.map(Path::to_path_buf));
    let child = std::thread::spawn(move || {
        serve(
            &runs_dir,
            agents.as_deref(),
            Box::new(BufReader::new(answers)),
            Box::new(says),
        )
    });
    let driven = drive(
        Box::new(BufReader::new(said)),
        &mut answer,
        (start, board),
        Duration::from_secs(120),
    )
    .await;
    drop(answer);
    (driven, child.join().unwrap())
}

/// The child converts every old run exactly as the daemon converts it in
/// its own process: the same spec for each run, looked up against the
/// daemon's providers and the MCP server it connected for the child, and the
/// same summary.
#[tokio::test]
async fn a_child_converts_exactly_as_the_daemon_does() {
    let home = Home::new();
    let runs = home.runs();
    let config = crate::config::Config::default();
    let agents = fixture("agents");

    let pool_a = pool();
    let board = StartupBoard::default();
    let in_daemon = temp_env::async_with_vars(home.home_var(), async {
        convert_at_start(&runs, at_start(&config, &agents, &pool_a, None), &board).await
    })
    .await;
    let expected = home.specs();
    assert_eq!((in_daemon.converted, in_daemon.upgraded), (2, 1));

    home.fill();
    assert!(!crate::run_index::path_for(&runs).exists());
    let pool_b = pool();
    let board = StartupBoard::default();
    let start = at_start(&config, &agents, &pool_b, None);
    let (driven, child) = temp_env::async_with_vars(home.home_var(), async {
        over_pipes(&runs, &start, &board).await
    })
    .await;
    let tally = driven.expect("the child finished");
    assert_eq!(tally.clone().upgrade(&runs), in_daemon);
    assert_eq!(child, in_daemon);
    assert_eq!(home.specs(), expected);
    // The child built the run index the daemon starts on.
    assert!(crate::run_index::path_for(&runs).is_file());
    let main = expected[1].1.stage("main").unwrap();
    assert_eq!(main.model.context_window, 64_000);
    assert!(main.tools.iter().any(|t| t.name.as_str() == "docs__lookup"));
    let now = board.current();
    assert_eq!(
        (now.step.as_str(), now.done, now.total),
        ("converting runs", 3, 3)
    );
}

/// A daemon that answers a question with something else gets the stage
/// looked up as one this machine cannot answer, and the run still converts.
#[test]
fn a_wrong_answer_is_a_lookup_that_failed() {
    let home = Home::new();
    let runs = home.runs();
    std::fs::remove_dir_all(runs.join("a-done")).unwrap();
    let old = runs.join("b-old");
    std::fs::copy(
        fixture("mid-tool-batch").join("blueprint.leviath"),
        old.join("blueprint.leviath"),
    )
    .unwrap();
    let (answers, mut answer) = std::io::pipe().unwrap();
    let (said, says) = std::io::pipe().unwrap();
    let dir = runs.clone();
    let child = std::thread::spawn(move || {
        serve(
            &dir,
            None,
            Box::new(BufReader::new(answers)),
            Box::new(says),
        )
    });
    let mut asked = Vec::new();
    for line in BufReader::new(said).lines().map_while(Result::ok) {
        let message: FromChild = serde_json::from_str(&line).unwrap();
        match message {
            FromChild::Finished { .. } => break,
            FromChild::Model { .. } | FromChild::Tools { .. } | FromChild::MaxDepth { .. } => {
                asked.push(line.chars().take(12).collect::<String>());
                writeln!(
                    answer,
                    "{}",
                    serde_json::to_string(&ToChild::Connected).unwrap()
                )
                .unwrap();
            }
            _ => {}
        }
    }
    drop(answer);
    let upgrade = child.join().unwrap();
    assert_eq!(upgrade.converted, 1);
    assert!(asked.len() >= 3, "{asked:?}");
}

/// A child whose daemon is gone stops before it writes the run it was
/// asking about, and between runs; one that cannot write to its daemon
/// stops too.
#[test]
fn a_child_whose_daemon_is_gone_stops() {
    let home = Home::new();
    let runs = home.runs();
    std::fs::remove_dir_all(runs.join("a-done")).unwrap();
    let old = runs.join("b-old");
    std::fs::copy(
        fixture("mid-tool-batch").join("blueprint.leviath"),
        old.join("blueprint.leviath"),
    )
    .unwrap();
    let dir = runs.clone();
    let stopped = std::thread::spawn(move || {
        serve(
            &dir,
            None,
            Box::new(std::io::Cursor::new(Vec::new())),
            Box::new(std::io::sink()),
        )
    })
    .join();
    assert!(stopped.is_err());
    assert!(leviath_legacy_runs::is_legacy(&old), "nothing was written");

    // Between runs: the daemon's end closed, and the reader saw it.
    let wire = Wire::new(
        Box::new(std::io::Cursor::new(Vec::new())),
        Box::new(std::io::sink()),
    );
    assert!(leviath_core::sync::lock(&wire.replies).recv().is_err());
    let detail = || wire.detail("x".into());
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(detail)).is_err());

    // A daemon that cannot be written to.
    let (answers, _answer) = std::io::pipe().unwrap();
    let wire = Wire::new(Box::new(BufReader::new(answers)), Box::new(Refuses));
    let begin = || wire.begin("x", 0);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(begin)).is_err());
}

/// A writer that refuses everything.
struct Refuses;

impl Write for Refuses {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("refused"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The daemon stops listening to a child that ends before it finishes, that
/// says nothing for too long, or that cannot be answered, and keeps what it
/// had done. A line that is not a message is passed over.
#[tokio::test]
async fn the_daemon_stops_on_a_child_that_ends_goes_quiet_or_cannot_be_answered() {
    let config = crate::config::Config::default();
    let agents = fixture("agents");
    let pool = pool();
    let start = at_start(&config, &agents, &pool, None);
    let board = StartupBoard::default();
    let done = FromChild::Done {
        done: 1,
        so_far: Tally {
            converted: 1,
            upgraded: 0,
            failed: 0,
            dropped: vec![("probe".into(), "a line".into(), 1)],
        },
    };
    let lines = |messages: &[&FromChild]| -> Box<dyn BufRead + Send> {
        let text: String = std::iter::once("not a message\n".to_string())
            .chain(
                messages
                    .iter()
                    .map(|m| serde_json::to_string(m).unwrap() + "\n"),
            )
            .collect();
        Box::new(std::io::Cursor::new(text.into_bytes()))
    };

    let (tally, why) = drive(
        lines(&[&done]),
        &mut Vec::new(),
        (&start, &board),
        Duration::from_secs(60),
    )
    .await
    .unwrap_err();
    assert_eq!(tally.converted, 1);
    assert!(why.contains("ended"), "{why}");

    let (quiet, _keep) = std::io::pipe().unwrap();
    let (_, why) = drive(
        Box::new(BufReader::new(quiet)),
        &mut Vec::new(),
        (&start, &board),
        Duration::from_millis(50),
    )
    .await
    .unwrap_err();
    assert!(why.contains("said nothing"), "{why}");

    let graph =
        leviath_legacy_runs::graph(&fixture("mid-tool-batch"), &Default::default()).unwrap();
    let question = FromChild::MaxDepth {
        graph: Box::new(graph),
    };
    let (_, why) = drive(
        lines(&[&question]),
        &mut Refuses,
        (&start, &board),
        Duration::from_secs(60),
    )
    .await
    .unwrap_err();
    assert!(why.contains("could not be answered"), "{why}");
}

/// A child that stopped part way adds the keys its runs dropped to those of
/// the runs the daemon converted after it, and the runs converted are those
/// on disk.
#[test]
fn what_a_stopped_child_did_adds_to_what_the_daemon_did_after_it() {
    let runs = Path::new("/runs");
    let child = Tally {
        converted: 2,
        upgraded: 1,
        failed: 1,
        dropped: vec![("probe".into(), "a line".into(), 2)],
    }
    .upgrade(runs);
    assert_eq!(child.upgraded, 1);
    let mut rest = Upgrade {
        converted: 3,
        upgraded: 1,
        failed: 2,
        unconverted: Some(Unconverted::path_for(runs)),
        ..Upgrade::default()
    };
    rest.dropped_in_run("probe", "a line".into());
    rest.dropped_in_run("other", "b line".into());
    let all = then(child, rest, (6, 3));
    assert_eq!((all.converted, all.upgraded, all.failed), (6, 3, 2));
    assert_eq!(
        all.dropped_in_runs
            .get(&("probe".to_string(), "a line".to_string())),
        Some(&3)
    );
    assert_eq!(all.dropped_in_runs.len(), 2);
    assert_eq!(all.unconverted, Some(Unconverted::path_for(runs)));
}

/// `lev daemon convert-runs` serves only a daemon of its own build.
#[test]
fn a_child_of_another_build_refuses() {
    let runs = tempfile::tempdir().unwrap();
    let args = ConvertRunsArgs {
        runs_dir: runs.path().to_path_buf(),
        agents_dir: None,
        build: "not-this-build".into(),
    };
    let refused = run_child(
        &args,
        Box::new(std::io::Cursor::new(Vec::new())),
        Box::new(std::io::sink()),
    )
    .unwrap_err();
    assert!(refused.to_string().contains("not-this-build"), "{refused}");
    let args = ConvertRunsArgs {
        build: crate::daemon::setup::CURRENT_BUILD.into(),
        ..args
    };
    let (answers, _answer) = std::io::pipe().unwrap();
    run_child(
        &args,
        Box::new(BufReader::new(answers)),
        Box::new(Vec::new()),
    )
    .unwrap();
}

/// This test binary, started as a converting child: it runs
/// [`child_stub`] alone, which acts as the child when its variables are set.
fn stub(runs: &Path, agents: &Path, home: &Path, mode: &str) -> ChildCmd {
    ChildCmd {
        program: std::env::current_exe().unwrap(),
        args: [
            "--exact",
            "daemon::convert_child::tests::child_stub",
            "-q",
            "--nocapture",
            "--test-threads=1",
        ]
        .iter()
        .map(Into::into)
        .collect(),
        env: vec![
            ("LEVIATH_TEST_CONVERT_RUNS".into(), runs.into()),
            ("LEVIATH_TEST_CONVERT_AGENTS".into(), agents.into()),
            ("LEVIATH_TEST_CONVERT_MODE".into(), mode.into()),
            ("LEVIATH_HOME".into(), home.into()),
        ],
        quiet_limit: Duration::from_secs(60),
    }
}

/// Writes through to stdout until it is about to say a line that starts
/// with its prefix (a question about a model, or a run done), then dies as
/// a crashed child does.
struct DiesAt(&'static [u8], std::io::Stdout);

impl Write for DiesAt {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.starts_with(self.0) {
            std::process::abort();
        }
        self.1.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.1.flush()
    }
}

/// The child [`stub`] starts. Does nothing in an ordinary test run.
#[test]
fn child_stub() {
    let Some(runs) = std::env::var_os("LEVIATH_TEST_CONVERT_RUNS") else {
        return;
    };
    let agents = std::env::var_os("LEVIATH_TEST_CONVERT_AGENTS").map(PathBuf::from);
    let mode = std::env::var("LEVIATH_TEST_CONVERT_MODE").unwrap_or_default();
    eprintln!("the converting child's log line");
    let out: Box<dyn Write + Send> = match mode.as_str() {
        "hang" => {
            std::thread::sleep(Duration::from_secs(60));
            std::process::exit(0);
        }
        "die" => Box::new(DiesAt(b"{\"model\":", std::io::stdout())),
        "die-at-done" => Box::new(DiesAt(b"{\"done\":", std::io::stdout())),
        _ => Box::new(std::io::stdout()),
    };
    let input = Box::new(BufReader::new(std::io::stdin()));
    serve(Path::new(&runs), agents.as_deref(), input, out);
    std::process::exit(0);
}

/// A daemon converts its old runs in a child process, and shows its
/// progress; one that cannot start a child, or whose child dies part way or
/// goes quiet, converts what is left itself, and nothing is lost. The
/// summary counts every run converted, also one the child converted and
/// died before it reported (`die-at-done`).
#[tokio::test]
async fn the_daemon_converts_in_a_child_and_takes_over_from_one_that_fails() {
    let config = crate::config::Config::default();
    let agents = fixture("agents");
    let modes = [
        ("serve", 60),
        ("die", 60),
        ("die-at-done", 60),
        ("hang", 1),
        ("missing", 60),
    ];
    for (mode, quiet) in modes {
        let home = Home::new();
        let runs = home.runs();
        let pool = pool();
        let board = StartupBoard::default();
        let mut cmd = stub(&runs, &agents, home.dir.path(), mode);
        cmd.quiet_limit = Duration::from_secs(quiet);
        if mode == "missing" {
            cmd.program = home.dir.path().join("no-such-lev");
        }
        let upgrade = temp_env::async_with_vars(home.home_var(), async {
            convert_at_start(&runs, at_start(&config, &agents, &pool, Some(cmd)), &board).await
        })
        .await;
        assert_eq!((upgrade.converted, upgrade.upgraded), (2, 1), "{mode}");
        // A run the child did not report keeps what it dropped in its own log.
        let dropped = usize::from(mode != "die-at-done");
        assert_eq!(upgrade.dropped_in_runs.len(), dropped, "{mode}");
        let specs = home.specs();
        let main = specs[1].1.stage("main").unwrap();
        assert_eq!(main.model.context_window, 64_000, "{mode}");
        assert!(
            main.tools.iter().any(|t| t.name.as_str() == "docs__lookup"),
            "{mode}"
        );
    }
}

/// A daemon with nothing left to convert starts no child: the runs that
/// failed to convert before are left as they are.
#[tokio::test]
async fn nothing_to_convert_starts_no_child() {
    let config = crate::config::Config::default();
    let agents = fixture("agents");
    let home = Home::new();
    let runs = home.runs();
    std::fs::remove_dir_all(runs.join("b-old")).unwrap();
    std::fs::write(runs.join("a-done").join("meta.json"), "not json").unwrap();
    let pool = pool();
    let board = StartupBoard::default();
    let first = temp_env::async_with_vars(home.home_var(), async {
        convert_at_start(&runs, at_start(&config, &agents, &pool, None), &board).await
    })
    .await;
    assert_eq!(first.failed, 1);
    let mut cmd = stub(&runs, &agents, home.dir.path(), "serve");
    cmd.program = home.dir.path().join("no-such-lev");
    let second = temp_env::async_with_vars(home.home_var(), async {
        convert_at_start(&runs, at_start(&config, &agents, &pool, Some(cmd)), &board).await
    })
    .await;
    assert_eq!((second.converted, second.failed), (0, 0));
}

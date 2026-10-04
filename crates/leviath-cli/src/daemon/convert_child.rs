//! Converting old runs in a child process.
//!
//! Converting a home's old runs reads every old journal whole, and the
//! memory that takes stays with the process that does it: the allocator
//! keeps the freed pages, so a daemon converting in its own process holds
//! them until it restarts. The daemon starts its own executable with the
//! hidden `lev daemon convert-runs` instead, and the memory goes back to the
//! system when the child exits. It builds the run index from the converted
//! runs before it exits, for the same reason.
//!
//! The child loads nothing but the runs and the installed blueprints. What a
//! resumable old run's stages are looked up against (this machine's
//! providers as the daemon read them at start, and the tools of the MCP
//! servers it connected) lives only in the daemon, so the child asks the
//! daemon, and every stage resolves exactly as it would have in the daemon.
//!
//! The two speak one JSON object a line: the child writes `FromChild` on
//! its stdout, and the daemon answers each question with a `ToChild` on the
//! child's stdin. The child's stderr is its log, which the daemon writes into
//! its own. The daemon shows the child's progress on its start-up board.
//!
//! A child that cannot be started, that stops before it says it finished, or
//! that says nothing for too long, is stopped, and the daemon converts the
//! runs that are left itself, as it does when it has no child at all. Each
//! run converts on its own, so nothing a child finished is converted twice,
//! and a run it was part way through is put back and converted again (see
//! [`leviath_legacy_runs::put_back`]). A child whose daemon goes away stops
//! between two runs, or before it writes a run it was asking about.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use leviath_runtime::spec::env::{CodeFiles, ModelPlan, StageTools};
use leviath_runtime::spec::graph::{RunGraph, StageDef};
use leviath_runtime::spec::names::ModelRef;
use serde::{Deserialize, Serialize};

use leviath_runtime::control_socket::StartupBoard;

use crate::commands::daemon::ConvertRunsArgs;
use crate::daemon::convert_old::{AtStart, ChildCmd, Lookup, Progress, Unconverted};
use crate::daemon::upgrade::Upgrade;

/// What the child says to the daemon, one a line on its stdout.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FromChild {
    /// A step begins, of `total` items (`0` when they cannot be counted).
    Begin { step: String, total: u64 },
    /// A line under the step.
    Detail { detail: String },
    /// `done` items of the step are done, and the pass has done `so_far`.
    Done { done: u64, so_far: Tally },
    /// Connect these MCP servers, which unfinished old runs' stages use,
    /// before any stage is looked up. Answered with [`ToChild::Connected`].
    Connect {
        servers: Vec<leviath_mcp::MCPServerConfig>,
    },
    /// The model a stage runs on. Answered with [`ToChild::Model`].
    Model {
        graph: Box<RunGraph>,
        stage: Box<StageDef>,
        requested: Option<ModelRef>,
    },
    /// The tools a stage gets. Answered with [`ToChild::Tools`].
    Tools {
        graph: Box<RunGraph>,
        stage: Box<StageDef>,
        code: CodeFiles,
        base: Option<PathBuf>,
        workdir: Option<PathBuf>,
    },
    /// How deep a run of `graph` may nest child runs when it sets no limit.
    /// Answered with [`ToChild::MaxDepth`].
    MaxDepth { graph: Box<RunGraph> },
    /// Every run is done; the pass did `so_far`.
    Finished { so_far: Tally },
}

/// The daemon's answer to a question the child asked, one a line on the
/// child's stdin.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToChild {
    /// The servers are connected, as far as they connect.
    Connected,
    /// The stage's model, or why it has none here.
    Model { plan: Result<ModelPlan, String> },
    /// The stage's tools, or why it cannot have them here.
    Tools { tools: Result<StageTools, String> },
    /// The operator's default depth.
    MaxDepth { depth: u8 },
}

/// What a pass has done so far, as the summary of the upgrade counts it.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Tally {
    converted: usize,
    failed: usize,
    /// Each key dropped from a converted run's blueprint: the blueprint, the
    /// line, and how many runs.
    dropped: Vec<(String, String, usize)>,
}

impl Tally {
    fn of(upgrade: &Upgrade) -> Self {
        Self {
            converted: upgrade.converted,
            failed: upgrade.failed,
            dropped: upgrade
                .dropped_in_runs
                .iter()
                .map(|((name, line), runs)| (name.clone(), line.clone(), *runs))
                .collect(),
        }
    }

    /// The upgrade this counts, for the runs under `runs_dir`.
    fn upgrade(self, runs_dir: &Path) -> Upgrade {
        Upgrade {
            converted: self.converted,
            failed: self.failed,
            dropped_in_runs: self
                .dropped
                .into_iter()
                .map(|(name, line, runs)| ((name, line), runs))
                .collect(),
            unconverted: Some(Unconverted::path_for(runs_dir)),
            ..Upgrade::default()
        }
    }
}

/// What a child that stopped part way had done, and the runs the daemon
/// converted after it, as one upgrade. The daemon tries again every run the
/// child did not convert, so its count of the ones that failed is the one
/// that stands.
pub(crate) fn then(child: Upgrade, rest: Upgrade) -> Upgrade {
    let mut dropped = child.dropped_in_runs;
    for (key, runs) in rest.dropped_in_runs {
        *dropped.entry(key).or_default() += runs;
    }
    Upgrade {
        converted: child.converted + rest.converted,
        dropped_in_runs: dropped,
        ..rest
    }
}

/// The panic that stops a child whose daemon is gone. A run it was looking
/// up has not been written yet, and every run before it is whole.
const GONE: &str = "the daemon this child converts old runs for is gone";

/// The child's end of the conversation.
struct Wire {
    out: Mutex<Box<dyn Write + Send>>,
    replies: Mutex<std::sync::mpsc::Receiver<String>>,
    /// Set once the daemon's end of the child's stdin closes.
    gone: Arc<AtomicBool>,
}

impl Wire {
    /// Talk over `output`, reading the daemon's answers from `input` on a
    /// thread of their own, so a daemon that goes away is noticed between
    /// two runs as well as while a question waits.
    fn new(input: Box<dyn BufRead + Send>, output: Box<dyn Write + Send>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let gone = Arc::new(AtomicBool::new(false));
        let flag = gone.clone();
        std::thread::spawn(move || {
            let _ = input
                .lines()
                .map_while(Result::ok)
                .try_for_each(|line| tx.send(line));
            flag.store(true, Ordering::SeqCst);
        });
        Self {
            out: Mutex::new(output),
            replies: Mutex::new(rx),
            gone,
        }
    }

    /// Say `message` to the daemon. Stops the child when the daemon is gone.
    fn send(&self, message: &FromChild) {
        let line = serde_json::to_string(message).expect("a message is plain data");
        let mut out = leviath_core::sync::lock(&self.out);
        let said = writeln!(out, "{line}").and_then(|()| out.flush());
        assert!(said.is_ok() && !self.gone.load(Ordering::SeqCst), "{GONE}");
    }

    /// Ask the daemon `question` and wait for its answer.
    fn ask(&self, question: &FromChild) -> ToChild {
        self.send(question);
        let line = leviath_core::sync::lock(&self.replies).recv().expect(GONE);
        serde_json::from_str(&line).expect(GONE)
    }
}

impl Progress for Wire {
    fn begin(&self, step: &str, total: u64) {
        self.send(&FromChild::Begin {
            step: step.to_string(),
            total,
        });
    }

    fn detail(&self, detail: String) {
        self.send(&FromChild::Detail { detail });
    }

    fn done(&self, done: u64, so_far: &Upgrade) {
        self.send(&FromChild::Done {
            done,
            so_far: Tally::of(so_far),
        });
    }
}

/// An answer the daemon gave to the wrong question, as a lookup that failed.
fn wrong(answer: &ToChild) -> String {
    format!("the daemon answered a different question: {answer:?}")
}

impl leviath_legacy_runs::StageLookup for Wire {
    fn model(
        &self,
        graph: &RunGraph,
        stage: &StageDef,
        requested: Option<&ModelRef>,
    ) -> Result<ModelPlan, String> {
        match self.ask(&FromChild::Model {
            graph: Box::new(graph.clone()),
            stage: Box::new(stage.clone()),
            requested: requested.cloned(),
        }) {
            ToChild::Model { plan } => plan,
            other => Err(wrong(&other)),
        }
    }

    fn tools(
        &self,
        graph: &RunGraph,
        stage: &StageDef,
        code: &CodeFiles,
        base: Option<&Path>,
        workdir: Option<&Path>,
    ) -> Result<StageTools, String> {
        match self.ask(&FromChild::Tools {
            graph: Box::new(graph.clone()),
            stage: Box::new(stage.clone()),
            code: code.clone(),
            base: base.map(Path::to_path_buf),
            workdir: workdir.map(Path::to_path_buf),
        }) {
            ToChild::Tools { tools } => tools,
            other => Err(wrong(&other)),
        }
    }

    fn default_max_depth(&self, graph: &RunGraph) -> u8 {
        match self.ask(&FromChild::MaxDepth {
            graph: Box::new(graph.clone()),
        }) {
            ToChild::MaxDepth { depth } => depth,
            _ => 0,
        }
    }
}

/// The child's work: connect, through the daemon, the MCP servers the
/// unfinished old runs under `runs_dir` use, convert every old run there
/// asking the daemon about each stage, and say how far along it is, all over
/// `input` and `output`. Returns what the pass did.
pub(crate) fn serve(
    runs_dir: &Path,
    agents_dir: Option<&Path>,
    input: Box<dyn BufRead + Send>,
    output: Box<dyn Write + Send>,
) -> Upgrade {
    let wire = Wire::new(input, output);
    let servers = crate::daemon::convert_old::servers_of_unfinished(runs_dir, agents_dir);
    if !servers.is_empty() {
        wire.begin("connecting the MCP servers of unfinished old runs", 0);
        wire.ask(&FromChild::Connect { servers });
    }
    let upgrade =
        crate::daemon::convert_old::convert_each(runs_dir, agents_dir, Some(&wire), &wire);
    // The run index is built from every run file, which takes the same kind
    // of memory converting does, so it is built here too, and the daemon
    // starts on an index that is up to date.
    crate::run_index::list(runs_dir);
    wire.send(&FromChild::Finished {
        so_far: Tally::of(&upgrade),
    });
    upgrade
}

/// `lev daemon convert-runs`: `serve` over this process's stdin and
/// stdout, for a daemon of this same build only.
pub fn run_child(
    args: &ConvertRunsArgs,
    input: Box<dyn BufRead + Send>,
    output: Box<dyn Write + Send>,
) -> anyhow::Result<()> {
    let build = crate::daemon::setup::CURRENT_BUILD;
    anyhow::ensure!(
        args.build == build,
        "this lev is build {build}, and the daemon that started it is build {}",
        args.build
    );
    serve(&args.runs_dir, args.agents_dir.as_deref(), input, output);
    Ok(())
}

/// Why a child stopped before it finished, and what it had done.
type Stopped = (Tally, String);

/// The daemon's end of the conversation: read what the child says from
/// `from_child`, show its progress on `board`, and answer its questions on
/// `to_child` against `start`. Returns what the child did when it says it
/// finished, or what it had done and why it stopped when it does not: it
/// ended, said nothing for `quiet`, or could not be answered.
async fn drive(
    from_child: Box<dyn BufRead + Send>,
    to_child: &mut dyn Write,
    (start, board): (&AtStart<'_>, &StartupBoard),
    quiet: std::time::Duration,
) -> Result<Tally, Stopped> {
    use leviath_legacy_runs::StageLookup;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let _ = from_child
            .lines()
            .map_while(Result::ok)
            .try_for_each(|line| tx.send(line));
    });
    let envs = |graph: &RunGraph| start.env(graph);
    let lookup = Lookup(&envs);
    let mut so_far = Tally::default();
    loop {
        let line = match tokio::time::timeout(quiet, rx.recv()).await {
            Ok(Some(line)) => line,
            Ok(None) => return Err((so_far, "it ended before it finished".to_string())),
            Err(_) => {
                let why = format!("it said nothing for {} seconds", quiet.as_secs_f32());
                return Err((so_far, why));
            }
        };
        let Ok(message) = serde_json::from_str::<FromChild>(&line) else {
            tracing::debug!(line = %line, "a line from the child converting old runs that is not one of its messages");
            continue;
        };
        let answer = match message {
            FromChild::Begin { step, total } => {
                board.begin(&step, total);
                continue;
            }
            FromChild::Detail { detail } => {
                board.detail(detail);
                continue;
            }
            FromChild::Done { done, so_far: now } => {
                board.done(done);
                so_far = now;
                continue;
            }
            FromChild::Finished { so_far } => return Ok(so_far),
            FromChild::Connect { servers } => {
                for server in &servers {
                    start.pool.ensure(server).await;
                }
                ToChild::Connected
            }
            FromChild::Model {
                graph,
                stage,
                requested,
            } => ToChild::Model {
                plan: lookup.model(&graph, &stage, requested.as_ref()),
            },
            FromChild::Tools {
                graph,
                stage,
                code,
                base,
                workdir,
            } => ToChild::Tools {
                tools: lookup.tools(&graph, &stage, &code, base.as_deref(), workdir.as_deref()),
            },
            FromChild::MaxDepth { graph } => ToChild::MaxDepth {
                depth: lookup.default_max_depth(&graph),
            },
        };
        let line = serde_json::to_string(&answer).expect("an answer is plain data");
        if let Err(e) = writeln!(to_child, "{line}").and_then(|()| to_child.flush()) {
            return Err((so_far, format!("it could not be answered: {e}")));
        }
    }
}

/// Convert the old runs under `runs_dir` in a child started with `cmd`,
/// showing its progress on `board` and answering its questions against
/// `start`. Returns what it did, or, when it could not be started or stopped
/// before it finished, what it had done; the child is stopped and gone by
/// then, so the daemon can convert the rest itself.
pub(crate) async fn convert(
    cmd: &ChildCmd,
    runs_dir: &Path,
    start: &AtStart<'_>,
    board: &StartupBoard,
) -> Result<Upgrade, Upgrade> {
    use std::process::Stdio;
    let spawned = leviath_sys::child_command(&cmd.program)
        .args(&cmd.args)
        .envs(cmd.env.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            let (program, why) = (cmd.program.display().to_string(), e.to_string());
            tracing::warn!(program = %program, error = %why, "could not start a child to convert the old runs; the daemon converts them itself");
            return Err(Upgrade::default());
        }
    };
    let stderr = child.stderr.take().expect("the child's stderr is piped");
    let log = std::thread::spawn(move || {
        std::io::BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
            .for_each(|line| crate::logging::forward(&line));
    });
    let stdout = child.stdout.take().expect("the child's stdout is piped");
    let mut stdin = child.stdin.take().expect("the child's stdin is piped");
    let driven = drive(
        Box::new(std::io::BufReader::new(stdout)),
        &mut stdin,
        (start, board),
        cmd.quiet_limit,
    )
    .await;
    if driven.is_err() {
        let _ = child.kill();
    }
    drop(stdin);
    let _ = child.wait();
    let _ = log.join();
    driven
        .map(|tally| tally.upgrade(runs_dir))
        .map_err(|(tally, why)| {
            tracing::warn!(why = %why, "the child converting the old runs stopped before it finished; the daemon converts the rest itself");
            tally.upgrade(runs_dir)
        })
}

#[cfg(test)]
#[path = "convert_child_tests.rs"]
mod tests;

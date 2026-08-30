//! `POST /api/update` - carrying out the plan `GET /api/update` prints.
//!
//! The read route answers "am I current, and what do I type to fix it". This is
//! the half that does it: it runs `binary.commands`, installs the blueprints the
//! plan marks `preselected`, and applies the config migrations - the same three
//! steps `lev update` runs, from the same [`plan`], so the console and the
//! terminal cannot carry out different updates.
//!
//! # Why it is a job rather than a request
//!
//! A `brew update && brew upgrade` is a download and an install. It takes a
//! minute on a good day and can fail halfway, and an HTTP request held open for
//! it tells a console nothing until it is over - so the button would say
//! "updating" with no way to know whether anything was happening. The route
//! answers `202` with a job id immediately, the work runs behind it, and every
//! step change goes out on the websocket as it happens. A client that missed a
//! frame, or connected late, polls `GET /api/update/jobs/{id}` for the same
//! record.
//!
//! # What it will not do
//!
//! A `cargo install` copy is `binary.action == "advise"`, and stays advice: the
//! step is recorded as advised, with the plan's own sentence, and no compile is
//! started. Same for a binary in a directory no installer writes to. A blueprint
//! the user edited is never installed either - [`install_bundled`] removes the
//! destination directory first, so a bulk yes would take their edits and any
//! file they added with it, which is why `lev update` asks about each one on its
//! own and no flag covers it. There is nobody to ask over HTTP, so the answer
//! is no, said out loud in the step's detail.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use super::events::ServerEvent;
use crate::bundled::install_bundled;
use crate::commands::update::{
    BinaryStep, CommandRunner, ConfigState, UpdateArgs, UpdateEnv, UpdatePlan, plan,
    render_commands,
};

// ─── The record ──────────────────────────────────────────────────────────────

/// How many jobs to remember. An update is a thing somebody does once and
/// watches, so the history is for the console that reconnected mid-run and the
/// operator reading back what happened - not an audit log.
const KEEP_JOBS: usize = 8;

/// The three steps, in the order they run. `binary` first because the other two
/// are decided by what the *new* binary ships.
pub(super) const STEPS: [&str; 3] = ["binary", "agents", "migrations"];

/// A step that has not been reached yet.
const PENDING: &str = "pending";
/// A step that is happening now.
const RUNNING: &str = "running";
/// A step that did what it set out to.
const DONE: &str = "done";
/// A step the request did not ask for, or that had nothing to do.
const SKIPPED: &str = "skipped";
/// A step that is the reader's to carry out, with the reason in its detail.
/// The one status that is neither success nor failure: nothing was done and
/// nothing went wrong.
const ADVISED: &str = "advised";
/// A step that tried and did not manage it.
const FAILED: &str = "failed";
/// Every step is finished and none failed.
const COMPLETE: &str = "complete";

/// What became of one step of an update run.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct UpdateStep {
    /// One of [`STEPS`].
    pub(super) step: &'static str,
    /// `pending`, `running`, `done`, `skipped`, `advised` or `failed`.
    pub(super) status: &'static str,
    /// One line a console can print under the step's name.
    pub(super) detail: String,
}

/// One run of `POST /api/update`, as the poll route and the finish frame report
/// it.
#[derive(Debug, Clone, Serialize)]
pub(super) struct UpdateJob {
    /// What `POST /api/update` answered with, and what the frames carry.
    pub(super) id: String,
    /// `running`, `complete` or `failed`.
    pub(super) status: &'static str,
    /// Every step, always all three, so a client renders a fixed list rather
    /// than one that grows under it.
    pub(super) steps: Vec<UpdateStep>,
    /// Whether the binary on disk is newer than the processes serving this.
    ///
    /// Set only when the binary step actually ran and succeeded. `lev serve`
    /// and the daemon it talks to are both still the old build until they
    /// restart, so a console that reports the version it can see would be
    /// telling the truth in the least useful way possible.
    pub(super) restart_required: bool,
    /// What to say about that, present exactly when `restart_required` is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) restart_hint: Option<String>,
    /// When the job started, in unix seconds.
    pub(super) started_at: u64,
    /// When it reached a terminal status, or `null` while it is still going.
    pub(super) finished_at: Option<u64>,
}

impl UpdateJob {
    /// A job that has not started any of its steps.
    fn new(id: String, started_at: u64) -> Self {
        Self {
            id,
            status: RUNNING,
            steps: STEPS
                .iter()
                .map(|step| UpdateStep {
                    step,
                    status: PENDING,
                    detail: String::new(),
                })
                .collect(),
            restart_required: false,
            restart_hint: None,
            started_at,
            finished_at: None,
        }
    }
}

/// The sentence a console shows when the binary moved under it.
const RESTART_HINT: &str = "the new binary is on disk, but this server and the \
     daemon it talks to are still running the old one. Restart `lev serve` (and \
     `lev daemon restart`) to be on the version that was just installed.";

// ─── The request ─────────────────────────────────────────────────────────────

/// Which parts of the plan to carry out.
///
/// Every field defaults to on, so an empty body means "everything `lev update`
/// would do". Naming them is how a console offers the same three checkboxes the
/// command's own prompts do - somebody who installed the new binary their own
/// way still wants the blueprints.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct ApplyRequest {
    /// Run `binary.commands`, where the plan has any.
    #[serde(default = "asked_for")]
    pub(super) binary: bool,
    /// Install the blueprints the plan marks `preselected`.
    #[serde(default = "asked_for")]
    pub(super) agents: bool,
    /// Apply the config migrations the plan lists.
    #[serde(default = "asked_for")]
    pub(super) migrations: bool,
}

/// The default for every field of [`ApplyRequest`]: a part not mentioned is a
/// part the caller wants.
fn asked_for() -> bool {
    true
}

impl Default for ApplyRequest {
    fn default() -> Self {
        Self {
            binary: true,
            agents: true,
            migrations: true,
        }
    }
}

/// Read a `POST /api/update` body.
///
/// An empty body is the whole plan rather than an error: "update me" is the
/// ordinary request, and making a console send `{}` to express it would be a
/// ceremony with no meaning behind it. `deny_unknown_fields` is the other half
/// of that - a body that names `agent` instead of `agents` is a caller asking
/// for something this route did not do, and silently running the default would
/// be the worst possible answer.
pub(super) fn parse_request(body: &[u8]) -> Result<ApplyRequest, String> {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(ApplyRequest::default());
    }
    serde_json::from_slice(body).map_err(|e| format!("could not read the request body: {e}"))
}

// ─── The store ───────────────────────────────────────────────────────────────

/// Every update run this server has done, and the machine to do another on.
///
/// On the app state rather than in a `static` for the same reason the update
/// cache is: two servers in one process, or two tests, must not write into each
/// other's answer.
#[derive(Clone)]
pub(super) struct UpdateJobs {
    /// Oldest first, capped at [`KEEP_JOBS`].
    jobs: Arc<Mutex<Vec<UpdateJob>>>,
    /// The environment a job acts on: where the blueprints and the config are,
    /// and how to run a command. A seam, so a test drives the whole thing
    /// against a temp directory and a runner that spawns nothing.
    env: Arc<dyn Fn() -> UpdateEnv + Send + Sync>,
    /// Current unix time; a fn so a long-lived server stays current.
    clock: fn() -> u64,
    /// Distinguishes two jobs started in the same second.
    seq: Arc<AtomicU64>,
}

impl std::fmt::Debug for UpdateJobs {
    /// Hand-written because a closure has no `Debug`. Prints the jobs, which is
    /// the part anyone reading a state dump wants.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateJobs")
            .field("jobs", &self.all())
            .finish_non_exhaustive()
    }
}

impl Default for UpdateJobs {
    /// The real machine, with a runner that refuses everything.
    ///
    /// Refusing rather than spawning: the spawn belongs to the binary's
    /// composition root like every other one, and `lev serve` passes it in.
    /// A no-op default would let a server built without one report a
    /// successful update that ran nothing.
    fn default() -> Self {
        Self::with_runner(Arc::new(no_runner))
    }
}

/// The runner a store built without one carries.
fn no_runner(_argv: &[String]) -> anyhow::Result<()> {
    anyhow::bail!("this server was built without a way to run an upgrade command")
}

/// Real unix time in seconds.
fn system_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl UpdateJobs {
    /// The real machine, running upgrade commands the given way.
    pub(super) fn with_runner(runner: CommandRunner) -> Self {
        Self::with_env(Arc::new(move || {
            UpdateEnv::for_applying(Arc::clone(&runner))
        }))
    }

    /// A store over an environment a caller built itself. Tests point this at a
    /// temp home; production points it at the real one.
    pub(super) fn with_env(env: Arc<dyn Fn() -> UpdateEnv + Send + Sync>) -> Self {
        Self {
            jobs: Arc::new(Mutex::new(Vec::new())),
            env,
            clock: system_now,
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Every job, oldest first.
    pub(super) fn all(&self) -> Vec<UpdateJob> {
        self.jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// One job by id.
    pub(super) fn get(&self, id: &str) -> Option<UpdateJob> {
        self.all().into_iter().find(|job| job.id == id)
    }

    /// Record a new job and return its id, or the id of the one already going.
    ///
    /// `pub(super)` for [`Self::spawn`]'s own tests, which need a job parked in
    /// `running` without a task behind it to prove the route refuses a second.
    ///
    /// One update at a time: two package-manager upgrades of the same binary
    /// racing each other is not a state anybody wants to debug, and a console
    /// that double-clicked the button meant one update. The check and the
    /// insert are one locked step on purpose - two requests arriving together
    /// would both see "nothing running" if they were two.
    pub(super) fn start(&self) -> Result<String, String> {
        let mut jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(running) = jobs.iter().find(|job| job.status == RUNNING) {
            return Err(running.id.clone());
        }
        let now = (self.clock)();
        let id = format!(
            "update-{now}-{}",
            self.seq.fetch_add(1, Ordering::SeqCst) + 1
        );
        jobs.push(UpdateJob::new(id.clone(), now));
        // Oldest first, so trimming from the front drops the oldest.
        while jobs.len() > KEEP_JOBS {
            jobs.remove(0);
        }
        Ok(id)
    }

    /// Change one step of a job, and announce it.
    ///
    /// A job that has been trimmed out from under a running task is left alone
    /// rather than re-inserted: it aged out because seven newer ones arrived,
    /// and resurrecting it would put a stale record back at the front.
    fn step(
        &self,
        id: &str,
        step: &'static str,
        status: &'static str,
        detail: String,
        events: &broadcast::Sender<ServerEvent>,
    ) {
        {
            let mut jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(job) = jobs.iter_mut().find(|job| job.id == id) else {
                return;
            };
            let Some(slot) = job.steps.iter_mut().find(|slot| slot.step == step) else {
                return;
            };
            slot.status = status;
            slot.detail = detail.clone();
        }
        // Send outside the lock: a subscriber's slow socket must not hold up the
        // update it is watching.
        let _ = events.send(ServerEvent::UpdateProgress {
            job_id: id.to_string(),
            step: step.to_string(),
            status: status.to_string(),
            detail,
        });
    }

    /// Close a job out, and announce the whole record.
    ///
    /// The finish frame carries the record rather than a summary so a console
    /// that connected mid-run, or dropped a frame, needs no follow-up request
    /// to render the result.
    fn finish(&self, id: &str, restart_required: bool, events: &broadcast::Sender<ServerEvent>) {
        let finished = {
            let mut jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(job) = jobs.iter_mut().find(|job| job.id == id) else {
                return;
            };
            job.status = match job.steps.iter().any(|step| step.status == FAILED) {
                true => FAILED,
                false => COMPLETE,
            };
            job.restart_required = restart_required;
            job.restart_hint = restart_required.then(|| RESTART_HINT.to_string());
            job.finished_at = Some((self.clock)());
            job.clone()
        };
        let _ = events.send(ServerEvent::UpdateFinished {
            job_id: finished.id.clone(),
            status: finished.status.to_string(),
            restart_required: finished.restart_required,
            job: serde_json::to_value(&finished)
                .expect("an update job is plain data and always serializes"),
        });
    }

    /// Start a job and carry `req` out on a blocking thread.
    ///
    /// Returns the id the caller answers `202` with. The work is
    /// `spawn_blocking` because all of it is: a package manager subprocess, a
    /// directory rewrite, and a config save.
    pub(super) fn spawn(
        &self,
        req: ApplyRequest,
        events: &broadcast::Sender<ServerEvent>,
    ) -> Result<String, String> {
        let id = self.start()?;
        let (store, events, job_id) = (self.clone(), events.clone(), id.clone());
        tokio::task::spawn_blocking(move || store.apply(&job_id, req, &events));
        Ok(id)
    }

    /// The three steps, in order, against a freshly read plan.
    ///
    /// The plan is built here rather than passed in: a caller that planned,
    /// waited for a queue, and then acted on the older answer is exactly the
    /// surprise `lev update` prints its plan to avoid.
    fn apply(&self, id: &str, req: ApplyRequest, events: &broadcast::Sender<ServerEvent>) {
        let env = (self.env)();
        let plan = plan(&UpdateArgs::default(), &env);
        let installed = self.binary_step(id, req, &plan, &env, events);
        self.agents_step(id, req, &plan, &env, events, installed);
        self.migrations_step(id, req, &plan, &env, events, installed);
        self.finish(id, matches!(installed, Binary::Installed), events);
    }

    /// What the binary step did, which the two steps after it need to know.
    fn binary_step(
        &self,
        id: &str,
        req: ApplyRequest,
        plan: &UpdatePlan,
        env: &UpdateEnv,
        events: &broadcast::Sender<ServerEvent>,
    ) -> Binary {
        let step = STEPS[0];
        if !req.binary {
            self.step(id, step, SKIPPED, "not asked for".to_string(), events);
            return Binary::Untouched;
        }
        let commands = match &plan.binary {
            // Advice stays advice. A `cargo install` copy is a full rebuild of
            // the workspace, which is minutes of somebody's CPU, and a binary
            // nowhere an installer writes is not something to guess at - both
            // are the reader's call, and this route says so rather than
            // starting one.
            BinaryStep::Advise(message) => {
                self.step(id, step, ADVISED, message.clone(), events);
                return Binary::Untouched;
            }
            BinaryStep::Run(commands) => commands,
        };
        let shown = render_commands(commands);
        self.step(id, step, RUNNING, format!("running `{shown}`"), events);
        for argv in commands {
            if let Err(e) = (env.runner)(argv) {
                self.step(id, step, FAILED, e.to_string(), events);
                return Binary::Failed;
            }
        }
        self.step(id, step, DONE, format!("ran `{shown}`"), events);
        Binary::Installed
    }

    /// The bundled blueprints, as far as consent reaches.
    fn agents_step(
        &self,
        id: &str,
        req: ApplyRequest,
        plan: &UpdatePlan,
        env: &UpdateEnv,
        events: &broadcast::Sender<ServerEvent>,
        binary: Binary,
    ) {
        let step = STEPS[1];
        let Some(()) = self.reached(id, step, req.agents, binary, events) else {
            return;
        };
        // `preselect` rather than `is_change`, which is the whole difference:
        // a blueprint the user edited is a change this route must not make.
        let (offered, edited): (Vec<_>, Vec<_>) = plan
            .agents
            .iter()
            .filter(|(_, action)| action.is_change())
            .partition(|(_, action)| action.preselect());
        if offered.is_empty() && edited.is_empty() {
            let detail = "every bundled blueprint is up to date".to_string();
            self.step(id, step, SKIPPED, detail, events);
            return;
        }

        let mut said = Vec::new();
        let mut failed = Vec::new();
        if !offered.is_empty() {
            self.step(
                id,
                step,
                RUNNING,
                format!("installing {} blueprint(s)", offered.len()),
                events,
            );
            let mut installed = Vec::new();
            for (agent, _) in &offered {
                match install_bundled(agent, &env.agents_dir) {
                    Ok(()) => installed.push(agent.name),
                    Err(e) => failed.push(format!("{}: {e}", agent.name)),
                }
            }
            if !installed.is_empty() {
                said.push(format!("installed {}", installed.join(", ")));
            }
            // A failed install is reported, not fatal - the same reason
            // `lev setup` treats it that way. Most of the blueprints plus a
            // named failure is a better place to leave somebody than a step
            // that gave up in the middle.
            if !failed.is_empty() {
                said.push(format!("could not install {}", failed.join(", ")));
            }
        }
        if !edited.is_empty() {
            said.push(format!(
                "{} left alone because you edited them - installing removes the \
                 directory first, so that is yours to do",
                edited.len()
            ));
        }
        // Nothing offered means nothing was attempted, however much there was to
        // say about the ones that were passed over.
        let status = match (failed.is_empty(), offered.is_empty()) {
            (false, _) => FAILED,
            (true, true) => SKIPPED,
            (true, false) => DONE,
        };
        self.step(id, step, status, said.join("; "), events);
    }

    /// The config migrations the plan found.
    fn migrations_step(
        &self,
        id: &str,
        req: ApplyRequest,
        plan: &UpdatePlan,
        env: &UpdateEnv,
        events: &broadcast::Sender<ServerEvent>,
        binary: Binary,
    ) {
        let step = STEPS[2];
        let Some(()) = self.reached(id, step, req.migrations, binary, events) else {
            return;
        };
        let config = match &plan.config {
            ConfigState::Unreadable(e) => {
                let detail = format!("the config could not be read, so it was left alone: {e}");
                self.step(id, step, SKIPPED, detail, events);
                return;
            }
            ConfigState::Loaded(config) => config,
        };
        if plan.migrations.is_empty() {
            self.step(id, step, SKIPPED, "nothing to migrate".to_string(), events);
            return;
        }
        self.step(
            id,
            step,
            RUNNING,
            format!("applying {} migration(s)", plan.migrations.len()),
            events,
        );
        let mut config = config.as_ref().clone();
        let mut changed = Vec::new();
        for migration in &plan.migrations {
            for line in (migration.apply)(&mut config) {
                changed.push(format!("{}: {line}", migration.name));
            }
        }
        match config.save_to_path_public(&env.config_path) {
            Ok(()) => self.step(id, step, DONE, changed.join("; "), events),
            Err(e) => self.step(id, step, FAILED, e.to_string(), events),
        }
    }

    /// Whether a step runs at all, recording why when it does not.
    ///
    /// `Some(())` to carry on, `None` when the step has already been closed out.
    /// A binary step that failed stops the two after it for the same reason
    /// `lev update` stops there: the blueprints and the config that matter are
    /// the ones the *new* binary ships, and applying the old one's over a failed
    /// upgrade is half a job done twice.
    fn reached(
        &self,
        id: &str,
        step: &'static str,
        asked_for: bool,
        binary: Binary,
        events: &broadcast::Sender<ServerEvent>,
    ) -> Option<()> {
        let reason = match (asked_for, binary) {
            (false, _) => "not asked for",
            (true, Binary::Failed) => "the binary step failed, so this was left alone",
            (true, _) => return Some(()),
        };
        self.step(id, step, SKIPPED, reason.to_string(), events);
        None
    }
}

/// What the binary step left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Binary {
    /// New commands ran and every one of them succeeded.
    Installed,
    /// Nothing was run: not asked for, or advice this route will not act on.
    Untouched,
    /// A command was run and it failed.
    Failed,
}

#[cfg(test)]
#[path = "update_job_tests.rs"]
mod tests;

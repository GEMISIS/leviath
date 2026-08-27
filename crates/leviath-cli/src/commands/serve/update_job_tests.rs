//! Tests for the update job machinery.
//!
//! Every one of these drives the real [`UpdateJobs::apply`] against a temp
//! directory: a real agents dir, a real config file, and a runner that records
//! the argv it was handed instead of spawning it. Nothing here reaches a
//! package manager, and nothing writes outside the temp dir.

use super::*;

use std::sync::atomic::AtomicUsize;

use crate::commands::update::{Channel, InstallMethod, MIGRATIONS, binary_step};

/// A runner that records the argv it was handed instead of spawning it, and
/// can be told to fail on the nth call or to do something to the filesystem
/// while it "runs".
#[derive(Clone)]
struct Recorder {
    ran: Arc<Mutex<Vec<Vec<String>>>>,
    /// Which call to fail on, counting from one. `0` never fails.
    fail_at: usize,
    calls: Arc<AtomicUsize>,
    /// Run on the first call. An upgrade takes a minute of wall clock, and the
    /// machine can change under it - which is the only way to reach the arm
    /// where a config that read fine no longer saves.
    during: Arc<dyn Fn() + Send + Sync>,
}

impl Default for Recorder {
    fn default() -> Self {
        Self {
            ran: Arc::default(),
            fail_at: 0,
            calls: Arc::default(),
            during: Arc::new(|| {}),
        }
    }
}

impl Recorder {
    fn runner(&self) -> CommandRunner {
        let me = self.clone();
        Arc::new(move |argv: &[String]| {
            let nth = me.calls.fetch_add(1, Ordering::SeqCst) + 1;
            me.ran
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(argv.to_vec());
            if nth == 1 {
                (me.during)();
            }
            match nth == me.fail_at {
                true => anyhow::bail!("`{}` exited with 1", argv.join(" ")),
                false => Ok(()),
            }
        })
    }

    fn ran(&self) -> Vec<Vec<String>> {
        self.ran
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The machine an update runs against in these tests.
struct Fixture {
    _dir: tempfile::TempDir,
    agents_dir: std::path::PathBuf,
    config_path: std::path::PathBuf,
    recorder: Recorder,
    jobs: UpdateJobs,
    events: broadcast::Sender<ServerEvent>,
    rx: broadcast::Receiver<ServerEvent>,
}

impl Fixture {
    /// A fixture whose plan detects the given install method, with the config
    /// text written as given.
    fn new(exe: std::path::PathBuf, config: &str, fail_at: usize) -> Self {
        Self::with_side_effect(exe, config, fail_at, Arc::new(|| {}))
    }

    /// [`Self::new`], with something for the runner to do to the filesystem
    /// while the binary step is "running".
    fn with_side_effect(
        exe: std::path::PathBuf,
        config: &str,
        fail_at: usize,
        during: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let dir = tempfile::tempdir().expect("a temp dir");
        let agents_dir = dir.path().join("agents");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, config).expect("the temp config writes");
        let recorder = Recorder {
            fail_at,
            during,
            ..Recorder::default()
        };
        let (env_agents, env_config, runner) =
            (agents_dir.clone(), config_path.clone(), recorder.runner());
        let jobs = UpdateJobs::with_env(Arc::new(move || UpdateEnv {
            exe: exe.clone(),
            agents_dir: env_agents.clone(),
            config_path: env_config.clone(),
            ..UpdateEnv::for_applying(Arc::clone(&runner))
        }));
        let (events, rx) = broadcast::channel(256);
        Self {
            _dir: dir,
            agents_dir,
            config_path,
            recorder,
            jobs,
            events,
            rx,
        }
    }

    /// A fixture whose binary step is a `Run`, because the exe sits in a Scoop
    /// layout. Scoop rather than Homebrew so the same path detects the same way
    /// on every runner: `detect` asks the real `brew --prefix` for the Homebrew
    /// arms, and a temp path is not under it on a machine that has one either.
    fn runnable(config: &str, fail_at: usize) -> Self {
        Self::new(
            std::path::PathBuf::from("/opt/scoop/apps/leviath/current/lev.exe"),
            config,
            fail_at,
        )
    }

    /// A fixture whose binary step is `Advise`, because nothing claims the path.
    fn advising(config: &str) -> Self {
        Self::new(std::path::PathBuf::from("/nowhere/at/all/lev"), config, 0)
    }

    /// Run one apply to completion, on this thread.
    fn apply(&self, req: ApplyRequest) -> UpdateJob {
        let id = self.jobs.start().expect("nothing else is running");
        self.jobs.apply(&id, req, &self.events);
        self.jobs.get(&id).expect("the job it just ran")
    }

    /// Every frame sent so far.
    fn frames(&mut self) -> Vec<ServerEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            out.push(event);
        }
        out
    }
}

/// One step out of a finished job.
fn step<'a>(job: &'a UpdateJob, name: &str) -> &'a UpdateStep {
    job.steps
        .iter()
        .find(|step| step.step == name)
        .expect("every job carries all three steps")
}

// ─── The request body ─────────────────────────────────────────────────────────

/// An empty body is "do everything", because that is the ordinary request and
/// making a console send `{}` to say it would be ceremony.
#[test]
fn an_empty_body_asks_for_the_whole_plan() {
    assert_eq!(
        parse_request(b"").expect("empty is fine"),
        ApplyRequest {
            binary: true,
            agents: true,
            migrations: true,
        }
    );
    assert_eq!(
        parse_request(b"  \n\t ").expect("whitespace is empty"),
        ApplyRequest::default()
    );
}

/// A body that names some parts leaves the rest on, which is what makes the
/// three fields a console's checkboxes rather than an all-or-nothing switch.
#[test]
fn a_partial_body_leaves_the_parts_it_does_not_name_alone() {
    let req = parse_request(br#"{"binary": false}"#).expect("valid JSON");
    assert!(!req.binary);
    assert!(req.agents);
    assert!(req.migrations);
}

/// A misspelled field is refused rather than silently running the default.
/// `{"agent": false}` is somebody asking for something this route did not do.
#[test]
fn a_body_with_a_field_this_route_does_not_know_is_refused() {
    let e = parse_request(br#"{"agent": false}"#).expect_err("unknown fields are refused");
    assert!(e.contains("could not read the request body"), "{e}");
    assert!(parse_request(b"not json at all").is_err());
}

// ─── The binary step ──────────────────────────────────────────────────────────

/// The commands the plan named are the commands that ran, in order, and the
/// job says a restart is needed afterwards.
#[test]
fn a_runnable_binary_step_runs_the_planned_commands_in_order() {
    let fixture = Fixture::runnable("", 0);
    let job = fixture.apply(ApplyRequest {
        binary: true,
        agents: false,
        migrations: false,
    });

    let planned = match binary_step(&InstallMethod::Scoop {
        package: "leviath".to_string(),
    }) {
        BinaryStep::Run(commands) => commands,
        BinaryStep::Advise(text) => unreachable!("a scoop path plans commands, not {text}"),
    };
    assert_eq!(fixture.recorder.ran(), planned);
    assert_eq!(step(&job, "binary").status, DONE);
    assert_eq!(job.status, COMPLETE);
    assert!(job.restart_required);
    assert!(
        job.restart_hint
            .as_deref()
            .is_some_and(|hint| hint.contains("still running the old one")),
        "{:?}",
        job.restart_hint
    );
    assert!(job.finished_at.is_some());
}

/// Advice stays advice. A `cargo install` copy - or a binary nowhere an
/// installer writes - is the reader's to rebuild, and this route says so with
/// the plan's own sentence rather than starting a compile nobody asked for.
#[test]
fn an_advising_binary_step_says_so_and_runs_nothing() {
    let fixture = Fixture::advising("");
    let job = fixture.apply(ApplyRequest::default());

    assert!(fixture.recorder.ran().is_empty());
    let binary = step(&job, "binary");
    assert_eq!(binary.status, ADVISED);
    assert!(
        binary.detail.contains("not where any installer"),
        "{binary:?}"
    );
    // Advice is neither success nor failure, so the job as a whole completed -
    // and the steps after it still ran.
    assert_eq!(job.status, COMPLETE);
    assert!(!job.restart_required);
    assert_eq!(job.restart_hint, None);
    assert_ne!(step(&job, "agents").status, SKIPPED);
}

/// A binary step that fails stops the two after it, the same way `lev update`
/// stops there: the blueprints worth installing are the ones the *new* binary
/// ships.
#[test]
fn a_failed_binary_step_stops_the_steps_after_it() {
    let fixture = Fixture::runnable("", 1);
    let job = fixture.apply(ApplyRequest::default());

    assert_eq!(fixture.recorder.ran().len(), 1, "it stopped at the failure");
    let binary = step(&job, "binary");
    assert_eq!(binary.status, FAILED);
    assert!(binary.detail.contains("exited with 1"), "{binary:?}");
    for later in ["agents", "migrations"] {
        assert_eq!(step(&job, later).status, SKIPPED);
        assert!(
            step(&job, later).detail.contains("the binary step failed"),
            "{:?}",
            step(&job, later)
        );
    }
    assert_eq!(job.status, FAILED);
    assert!(!job.restart_required, "nothing was installed");
    // The blueprints the failed run did not install are still absent.
    assert!(!fixture.agents_dir.exists());
}

/// The second command failing is still a failure, which is the whole reason the
/// step is a sequence: a `scoop update` that worked and a `scoop update leviath`
/// that did not is not an update.
#[test]
fn a_failure_part_way_through_the_sequence_fails_the_step() {
    let fixture = Fixture::runnable("", 2);
    let job = fixture.apply(ApplyRequest {
        binary: true,
        agents: false,
        migrations: false,
    });
    assert_eq!(fixture.recorder.ran().len(), 2);
    assert_eq!(step(&job, "binary").status, FAILED);
    assert!(!job.restart_required);
}

// ─── What the request asked for ───────────────────────────────────────────────

/// A part the caller turned off is skipped and says why, rather than being
/// absent from the record: a console renders the same three rows every time.
#[test]
fn a_step_the_request_turned_off_is_skipped_and_says_so() {
    let fixture = Fixture::runnable("", 0);
    let job = fixture.apply(ApplyRequest {
        binary: false,
        agents: false,
        migrations: false,
    });

    assert!(fixture.recorder.ran().is_empty());
    assert_eq!(job.steps.len(), 3);
    for name in STEPS {
        assert_eq!(step(&job, name).status, SKIPPED);
        assert_eq!(step(&job, name).detail, "not asked for");
    }
    assert_eq!(job.status, COMPLETE);
    assert!(!job.restart_required);
}

// ─── The blueprints ───────────────────────────────────────────────────────────

/// An empty agents directory is every bundled blueprint waiting to be
/// installed, and they land where the environment says.
#[test]
fn the_agents_step_installs_the_blueprints_the_plan_preselects() {
    let fixture = Fixture::advising("");
    let job = fixture.apply(ApplyRequest {
        binary: false,
        agents: true,
        migrations: false,
    });

    let agents = step(&job, "agents");
    assert_eq!(agents.status, DONE, "{agents:?}");
    assert!(agents.detail.starts_with("installed "), "{agents:?}");
    // Assert over what was discovered rather than over a list of names: the
    // bundled set changes, and a test that enumerated it would only ever be a
    // second copy of the list.
    let installed: Vec<_> = std::fs::read_dir(&fixture.agents_dir)
        .expect("the agents dir was created")
        .map(|e| e.expect("a readable entry").file_name())
        .collect();
    assert!(!installed.is_empty());
    for entry in &installed {
        assert!(
            agents.detail.contains(&entry.to_string_lossy().to_string()),
            "{agents:?} does not name {entry:?}"
        );
    }
}

/// Run twice and the second time has nothing to do - which is a skip, not a
/// failure, and not a silent success either.
#[test]
fn an_agents_step_with_nothing_to_install_skips() {
    let fixture = Fixture::advising("");
    let only_agents = ApplyRequest {
        binary: false,
        agents: true,
        migrations: false,
    };
    fixture.apply(only_agents);
    let job = fixture.apply(only_agents);

    let agents = step(&job, "agents");
    assert_eq!(agents.status, SKIPPED);
    assert_eq!(agents.detail, "every bundled blueprint is up to date");
    assert_eq!(job.status, COMPLETE);
}

/// A blueprint the user edited is left alone and named as left alone.
///
/// `install_bundled` removes the destination directory first, so installing
/// over an edited copy takes the edits and any file they added with them. `lev
/// update` asks about each one on its own and no flag covers it; there is
/// nobody to ask over HTTP, so the answer is no.
#[test]
fn an_edited_blueprint_is_left_alone_and_the_step_says_why() {
    let fixture = Fixture::advising("");
    let only_agents = ApplyRequest {
        binary: false,
        agents: true,
        migrations: false,
    };
    fixture.apply(only_agents);

    // Edit one of whatever was installed, without naming it.
    let edited = std::fs::read_dir(&fixture.agents_dir)
        .expect("the agents dir exists")
        .next()
        .expect("at least one blueprint was installed")
        .expect("a readable entry")
        .path();
    let marker = edited.join("agent.leviath");
    let original = std::fs::read_to_string(&marker).expect("a bundled blueprint has a manifest");
    std::fs::write(&marker, format!("{original}\n# mine\n")).expect("the edit writes");

    let job = fixture.apply(only_agents);
    let agents = step(&job, "agents");
    assert_eq!(agents.status, SKIPPED, "{agents:?}");
    assert_eq!(
        agents.detail,
        "1 left alone because you edited them - installing removes the \
         directory first, so that is yours to do"
    );
    assert_eq!(
        std::fs::read_to_string(&marker).expect("still readable"),
        format!("{original}\n# mine\n"),
        "the edit survived"
    );
}

/// An install that cannot be written is reported and does not stop the run -
/// the same reason `lev setup` treats it as a warning. Most of the blueprints
/// plus a named failure beats a step that gave up in the middle.
#[test]
fn an_install_that_fails_is_named_and_the_run_carries_on() {
    let fixture = Fixture::advising("");
    // A file where the agents directory should be: every install fails, and
    // nothing about the failure is this test's to arrange per blueprint.
    std::fs::write(&fixture.agents_dir, "not a directory").expect("the blocker writes");

    let job = fixture.apply(ApplyRequest {
        binary: false,
        agents: true,
        migrations: false,
    });
    let agents = step(&job, "agents");
    assert_eq!(agents.status, FAILED, "{agents:?}");
    assert!(agents.detail.contains("could not install"), "{agents:?}");
    assert_eq!(job.status, FAILED);
    // The step after it still ran.
    assert_ne!(step(&job, "migrations").status, PENDING);
}

// ─── The config ───────────────────────────────────────────────────────────────

/// A config with a migration due is migrated, and the step names what changed.
#[test]
fn the_migrations_step_applies_what_the_plan_found_and_writes_it() {
    // `serves = []` is what the one shipped migration removes. Written through
    // the real config path so the plan reads it the way production does.
    let fixture = Fixture::advising(
        "[model_providers.demo]\nbase_url = \"https://example.invalid\"\nserves = []\n",
    );
    assert!(!MIGRATIONS.is_empty(), "this test needs one to apply");

    let job = fixture.apply(ApplyRequest {
        binary: false,
        agents: false,
        migrations: true,
    });
    let migrations = step(&job, "migrations");
    assert_eq!(migrations.status, DONE, "{migrations:?}");
    assert!(
        migrations.detail.contains("stale-empty-serves"),
        "{migrations:?}"
    );
    let written = std::fs::read_to_string(&fixture.config_path).expect("the config was rewritten");
    assert!(!written.contains("serves"), "{written}");
}

/// A config with nothing due is a skip, so a console does not report a write
/// that never happened.
#[test]
fn a_config_with_nothing_to_migrate_skips() {
    let fixture = Fixture::advising("");
    let job = fixture.apply(ApplyRequest {
        binary: false,
        agents: false,
        migrations: true,
    });
    let migrations = step(&job, "migrations");
    assert_eq!(migrations.status, SKIPPED);
    assert_eq!(migrations.detail, "nothing to migrate");
}

/// A config that will not parse is left alone and said so, rather than
/// overwritten with the defaults - which is what "migrate" would otherwise mean
/// for a file nothing could read.
#[test]
fn a_config_that_will_not_parse_is_left_exactly_as_it_is() {
    let fixture = Fixture::advising("this is not [ toml");
    let job = fixture.apply(ApplyRequest {
        binary: false,
        agents: false,
        migrations: true,
    });
    let migrations = step(&job, "migrations");
    assert_eq!(migrations.status, SKIPPED);
    assert!(
        migrations.detail.contains("could not be read"),
        "{migrations:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&fixture.config_path).expect("still there"),
        "this is not [ toml"
    );
}

/// A save that cannot land fails the step rather than reporting a write that
/// did not happen.
///
/// The config is read when the plan is built and written a minute later, after
/// a package manager has had its turn - so "it read fine and then would not
/// save" is a real interval, not a contrived one. Here the directory holding it
/// is replaced by a file while the binary step runs.
#[test]
fn a_config_that_cannot_be_written_fails_the_step() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let home = dir.path().join("home");
    std::fs::create_dir(&home).expect("the config dir is a directory to begin with");
    let config_path = home.join("config.toml");
    let during = {
        let home = home.clone();
        Arc::new(move || {
            std::fs::remove_dir_all(&home).expect("the directory goes");
            std::fs::write(&home, "a file where the directory was")
                .expect("a file takes its place");
        })
    };
    let fixture = Fixture::with_side_effect(
        std::path::PathBuf::from("/opt/scoop/apps/leviath/current/lev.exe"),
        "",
        0,
        during,
    );
    // The config the plan reads is the one about to become unwritable.
    std::fs::write(
        &config_path,
        "[model_providers.demo]\nbase_url = \"https://example.invalid\"\nserves = []\n",
    )
    .expect("the config writes");
    let jobs = UpdateJobs::with_env({
        let (agents, runner, config_path) = (
            fixture.agents_dir.clone(),
            fixture.recorder.runner(),
            config_path.clone(),
        );
        Arc::new(move || UpdateEnv {
            exe: std::path::PathBuf::from("/opt/scoop/apps/leviath/current/lev.exe"),
            agents_dir: agents.clone(),
            config_path: config_path.clone(),
            ..UpdateEnv::for_applying(Arc::clone(&runner))
        })
    });
    let id = jobs.start().expect("nothing else is running");
    jobs.apply(
        &id,
        ApplyRequest {
            binary: true,
            agents: false,
            migrations: true,
        },
        &fixture.events,
    );
    let job = jobs.get(&id).expect("the job it just ran");

    assert_eq!(
        step(&job, "binary").status,
        DONE,
        "the upgrade itself worked"
    );
    let migrations = step(&job, "migrations");
    assert_eq!(migrations.status, FAILED, "{migrations:?}");
    assert!(!migrations.detail.is_empty(), "the failure says something");
    assert_eq!(job.status, FAILED);
    // A failed migration is not a reason to say the new binary is not there.
    assert!(job.restart_required);
}

// ─── The frames ───────────────────────────────────────────────────────────────

/// Every step change goes out as it happens, and the last frame carries the
/// whole record - which is what a console that connected mid-run reads.
#[tokio::test]
async fn every_step_is_announced_and_the_last_frame_carries_the_record() {
    let mut fixture = Fixture::runnable("", 0);
    let job = fixture.apply(ApplyRequest {
        binary: true,
        agents: false,
        migrations: false,
    });

    let frames = fixture.frames();
    let progress: Vec<_> = frames
        .iter()
        .filter_map(|event| match event {
            ServerEvent::UpdateProgress {
                job_id,
                step,
                status,
                ..
            } => Some((job_id.clone(), step.clone(), status.clone())),
            _ => None,
        })
        .collect();
    // running then done for the binary, and one each for the two skips.
    assert_eq!(
        progress,
        vec![
            (job.id.clone(), "binary".to_string(), RUNNING.to_string()),
            (job.id.clone(), "binary".to_string(), DONE.to_string()),
            (job.id.clone(), "agents".to_string(), SKIPPED.to_string()),
            (
                job.id.clone(),
                "migrations".to_string(),
                SKIPPED.to_string()
            ),
        ]
    );

    let last = frames.last().expect("a job always finishes");
    let ServerEvent::UpdateFinished {
        job_id,
        status,
        restart_required,
        job: record,
    } = last
    else {
        unreachable!("the last frame is the finish, not {last:?}")
    };
    assert_eq!(job_id, &job.id);
    assert_eq!(status, COMPLETE);
    assert!(restart_required);
    assert_eq!(record["id"], job.id);
    assert_eq!(record["steps"][0]["status"], DONE);
    assert!(record["restart_hint"].is_string());
}

/// The update frames are about the machine, not about a run: `/ws` gets them
/// and a per-run subscription does not.
#[test]
fn an_update_frame_belongs_to_no_run() {
    let progress = ServerEvent::UpdateProgress {
        job_id: "update-1-1".to_string(),
        step: "binary".to_string(),
        status: RUNNING.to_string(),
        detail: "running `scoop update`".to_string(),
    };
    assert_eq!(progress.run_id(), "");
    assert!(!progress.is_for_run("run-1"));
    let finished = ServerEvent::UpdateFinished {
        job_id: "update-1-1".to_string(),
        status: COMPLETE.to_string(),
        restart_required: false,
        job: serde_json::json!({}),
    };
    assert_eq!(finished.run_id(), "");
    assert!(!finished.is_for_run("run-1"));
    let json = serde_json::to_value(&finished).expect("a frame serializes");
    assert_eq!(json["type"], "update_finished");
}

// ─── The store ────────────────────────────────────────────────────────────────

/// One update at a time. The second request is told which one is going rather
/// than starting a second package-manager upgrade of the same binary.
#[test]
fn a_second_update_is_refused_while_one_is_running() {
    let fixture = Fixture::runnable("", 0);
    let first = fixture.jobs.start().expect("the first starts");
    let refused = fixture.jobs.start().expect_err("the second is refused");
    assert_eq!(refused, first);
    // Once it finishes, another may start.
    fixture.jobs.finish(&first, false, &fixture.events);
    let second = fixture.jobs.start().expect("the next one starts");
    assert_ne!(second, first);
}

/// Ids are distinct even within one second, which is the case a wall clock
/// alone gets wrong.
#[test]
fn two_jobs_started_in_the_same_second_get_different_ids() {
    let fixture = Fixture::runnable("", 0);
    let mut ids = Vec::new();
    for _ in 0..3 {
        let id = fixture.jobs.start().expect("nothing is running");
        fixture.jobs.finish(&id, false, &fixture.events);
        ids.push(id);
    }
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "{ids:?}");
}

/// The history is bounded, and it is the oldest that goes.
#[test]
fn the_job_history_is_capped_and_drops_the_oldest_first() {
    let fixture = Fixture::runnable("", 0);
    let mut ids = Vec::new();
    for _ in 0..KEEP_JOBS + 2 {
        let id = fixture.jobs.start().expect("nothing is running");
        fixture.jobs.finish(&id, false, &fixture.events);
        ids.push(id);
    }
    let kept = fixture.jobs.all();
    assert_eq!(kept.len(), KEEP_JOBS);
    assert!(fixture.jobs.get(&ids[0]).is_none(), "the oldest aged out");
    assert!(
        fixture
            .jobs
            .get(ids.last().expect("there are ids"))
            .is_some(),
        "the newest is kept"
    );
}

/// An id nobody minted has no job, which is what the poll route turns into a
/// 404.
#[test]
fn an_unknown_job_id_is_not_found() {
    let fixture = Fixture::runnable("", 0);
    assert!(fixture.jobs.get("update-never-1").is_none());
}

/// A job that aged out from under its own running task is left alone rather
/// than re-inserted: it went because newer ones arrived, and putting a stale
/// record back would be worse than losing it.
#[test]
fn a_job_that_aged_out_is_not_resurrected_by_its_own_task() {
    let fixture = Fixture::runnable("", 0);
    fixture.jobs.step(
        "update-gone-1",
        STEPS[0],
        DONE,
        "x".to_string(),
        &fixture.events,
    );
    fixture.jobs.finish("update-gone-1", true, &fixture.events);
    assert!(fixture.jobs.all().is_empty());
}

/// A step name that is not one of the three changes nothing. Unreachable from
/// the route, which only ever passes [`STEPS`]; asserted so the guard cannot be
/// removed as dead.
#[test]
fn a_step_name_the_job_does_not_carry_changes_nothing() {
    let fixture = Fixture::runnable("", 0);
    let id = fixture.jobs.start().expect("nothing is running");
    fixture
        .jobs
        .step(&id, "not-a-step", DONE, "x".to_string(), &fixture.events);
    let job = fixture.jobs.get(&id).expect("still there");
    assert!(job.steps.iter().all(|step| step.status == PENDING));
}

/// `spawn` hands back an id straight away and the work happens behind it. That
/// is the whole reason the route answers `202`.
#[tokio::test]
async fn spawn_answers_with_an_id_and_runs_the_work_behind_it() {
    let mut fixture = Fixture::runnable("", 0);
    let id = fixture
        .jobs
        .spawn(
            ApplyRequest {
                binary: true,
                agents: false,
                migrations: false,
            },
            &fixture.events,
        )
        .expect("nothing else is running");
    // The record exists the moment the route answers, whether or not the work
    // behind it has got anywhere yet - which is what a console polls.
    assert!(
        fixture.jobs.get(&id).is_some(),
        "recorded before it is done"
    );
    let finished = loop {
        match fixture.rx.recv().await.expect("the sender is alive") {
            ServerEvent::UpdateFinished { job_id, status, .. } => break (job_id, status),
            _ => continue,
        }
    };
    assert_eq!(finished, (id.clone(), COMPLETE.to_string()));
    assert_eq!(fixture.jobs.get(&id).expect("still there").status, COMPLETE);
    assert_eq!(fixture.recorder.ran().len(), 2, "both planned commands ran");
}

/// A store built without a runner refuses to run anything, loudly. A no-op
/// default would let such a server report a successful update that ran nothing.
#[tokio::test]
async fn the_default_store_cannot_run_an_upgrade_command() {
    let jobs = UpdateJobs::default();
    assert!(format!("{jobs:?}").contains("UpdateJobs"));
    let env = (jobs.env)();
    let e = (env.runner)(&["scoop".to_string()]).expect_err("it refuses");
    assert!(e.to_string().contains("without a way to run"), "{e}");
}

/// The channel a plan-less caller names still reaches the planner: asserted so
/// the fixture's Scoop path is the method it claims to be, rather than a path
/// that happens to detect as something else on some runner.
#[test]
fn the_fixture_path_detects_as_the_method_its_test_assumes() {
    let method = crate::commands::update::detect(
        std::path::Path::new("/opt/scoop/apps/leviath/current/lev.exe"),
        None,
        None,
        Some(Channel::Stable),
    );
    assert_eq!(method.id(), "scoop");
    let nowhere = crate::commands::update::detect(
        std::path::Path::new("/nowhere/at/all/lev"),
        None,
        None,
        None,
    );
    assert_eq!(nowhere.id(), "unknown");
}

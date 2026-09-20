//! Tests for the self-update and daemon-link answers.
//!
//! These are mappers over what the update planner and the control client
//! already know, so they are exercised directly against built plans rather than
//! through a server: what is asserted is that every shape the planner can
//! produce has an answer here, including the ones a real machine only reaches on
//! somebody else's operating system.

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};

use super::super::update::{DaemonStatus, UpdateInfo, UpdateJob};
use crate::commands::update::detect::InstallMethod;
use crate::commands::update::latest::LatestCheck;
use crate::commands::update::{ConfigState, UpdatePlan};

/// A plan with the given install method and nothing else going on.
fn plan_with(method: InstallMethod) -> UpdatePlan {
    let binary = crate::commands::update::binary_step(&method);
    UpdatePlan {
        method,
        binary,
        agents: Vec::new(),
        migrations: Vec::new(),
        config: ConfigState::Unreadable("no config here".to_string()),
    }
}

/// Ask the schema about one plan.
async fn ask(plan: &UpdatePlan, latest: &LatestCheck, query: &str) -> serde_json::Value {
    let schema = Schema::build(
        Probe {
            info: UpdateInfo::from_plan(plan, "9.9.9", latest),
        },
        EmptyMutation,
        EmptySubscription,
    )
    .finish();
    let answer = schema.execute(Request::new(query)).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).expect("data serializes")
}

/// A root handing out one update plan.
struct Probe {
    info: UpdateInfo,
}

#[async_graphql::Object]
impl Probe {
    /// The plan under test.
    async fn update(&self) -> &UpdateInfo {
        &self.info
    }
}

/// Every install method has its own answer, and each carries its channel where
/// one is knowable.
///
/// A machine only ever reports one of these, so the others are unreachable there
/// and reachable here: a Windows install is a shape this schema has to describe
/// from a Mac.
#[tokio::test]
async fn every_install_method_has_an_answer() {
    let cases = [
        (
            InstallMethod::Homebrew {
                formula: "leviath".to_string(),
            },
            "HOMEBREW",
        ),
        (
            InstallMethod::Scoop {
                package: "leviath".to_string(),
            },
            "SCOOP",
        ),
        (InstallMethod::Cargo, "CARGO"),
        (
            InstallMethod::Script {
                channel: crate::commands::update::detect::Channel::Stable,
            },
            "SCRIPT",
        ),
        (
            InstallMethod::Unknown {
                path: std::path::PathBuf::from("/opt/somewhere/lev"),
            },
            "UNKNOWN",
        ),
    ];
    for (method, expected) in cases {
        let json = ask(
            &plan_with(method),
            &LatestCheck::default(),
            "{ update { version installMethod channel configError } }",
        )
        .await;
        assert_eq!(json["update"]["installMethod"], expected);
        assert_eq!(json["update"]["version"], "9.9.9");
        // The config could not be read in these plans, and the answer says so
        // rather than reporting a plan with no migrations as a clean one.
        assert_eq!(json["update"]["configError"], "no config here");
    }
}

/// The binary step is either commands to run or something to tell somebody, and
/// the two are different types rather than a field to check for null.
#[tokio::test]
async fn the_binary_step_is_commands_or_advice() {
    let query = r#"{ update { binary {
        __typename
        ... on UpgradeByCommand { commands shell }
        ... on UpgradeByAdvice { message }
    } } }"#;

    let brewed = ask(
        &plan_with(InstallMethod::Homebrew {
            formula: "leviath-alpha".to_string(),
        }),
        &LatestCheck::default(),
        query,
    )
    .await;
    assert_eq!(brewed["update"]["binary"]["__typename"], "UpgradeByCommand");
    let commands = brewed["update"]["binary"]["commands"]
        .as_array()
        .expect("commands");
    assert!(
        commands.len() >= 2,
        "the refresh and the upgrade: {commands:?}"
    );
    // The one-line form is what a person pastes into a shell, and it is the
    // same sequence joined the way it behaves.
    let shell = brewed["update"]["binary"]["shell"]
        .as_str()
        .expect("a line");
    assert!(shell.contains(" && "), "{shell}");
    assert!(shell.contains("leviath-alpha"), "{shell}");

    let unknown = ask(
        &plan_with(InstallMethod::Unknown {
            path: std::path::PathBuf::from("/opt/elsewhere/lev"),
        }),
        &LatestCheck::default(),
        query,
    )
    .await;
    assert_eq!(unknown["update"]["binary"]["__typename"], "UpgradeByAdvice");
    assert!(
        unknown["update"]["binary"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "it says what to do instead"
    );
}

/// What the last check found travels as three fields that are null together.
///
/// Not checked yet, checking switched off and the check failed are one answer to
/// a client: nothing to show. Reporting two of the three and not the last would
/// let a console render "up to date" from an answer that said no such thing.
#[tokio::test]
async fn the_latest_check_is_three_fields_that_move_together() {
    let query = "{ update { latest updateAvailable checkedAt } }";
    let unchecked = ask(
        &plan_with(InstallMethod::Cargo),
        &LatestCheck::default(),
        query,
    )
    .await;
    assert!(unchecked["update"]["latest"].is_null());
    assert!(unchecked["update"]["updateAvailable"].is_null());
    assert!(unchecked["update"]["checkedAt"].is_null());

    let checked = ask(
        &plan_with(InstallMethod::Cargo),
        &LatestCheck {
            latest: Some("10.0.0".to_string()),
            update_available: Some(true),
            checked_at: Some(1_788_924_523),
        },
        query,
    )
    .await;
    assert_eq!(checked["update"]["latest"], "10.0.0");
    assert_eq!(checked["update"]["updateAvailable"], true);
    assert_eq!(checked["update"]["checkedAt"], 1_788_924_523);
}

/// The blueprints and the migrations come through with what would happen to
/// each.
#[tokio::test]
async fn the_agents_and_migrations_say_what_would_happen() {
    let mut plan = plan_with(InstallMethod::Cargo);
    let agent = crate::bundled::BUNDLED_AGENTS
        .first()
        .expect("this build ships blueprints");
    plan.agents = vec![
        (agent, crate::bundled::AgentAction::Install),
        (
            agent,
            crate::bundled::AgentAction::Update {
                from: "0.0.1".to_string(),
            },
        ),
        (agent, crate::bundled::AgentAction::Modified),
        (agent, crate::bundled::AgentAction::UpToDate),
    ];
    plan.migrations = crate::commands::update::MIGRATIONS.iter().collect();
    plan.config = crate::commands::update::plan(
        &crate::commands::update::UpdateArgs {
            check: true,
            ..Default::default()
        },
        &crate::commands::update::UpdateEnv::for_planning_offline(),
    )
    .config;

    let json = ask(
        &plan,
        &LatestCheck::default(),
        "{ update { configError agents { name version change changes preselected }
             migrations { name description } } }",
    )
    .await;
    assert!(
        json["update"]["configError"].is_null(),
        "a config that reads has nothing to report"
    );
    let agents = json["update"]["agents"].as_array().expect("agents");
    assert_eq!(agents.len(), 4);
    assert_eq!(agents[0]["name"], agent.name);
    assert_eq!(agents[0]["changes"], true, "installing is a change");
    // A copy somebody edited is a change and is not pre-checked: overwriting it
    // would throw that work away.
    assert_eq!(agents[2]["changes"], true);
    assert_eq!(agents[2]["preselected"], false);
    assert_eq!(agents[3]["changes"], false, "already current");
    assert!(
        agents[1]["change"]
            .as_str()
            .is_some_and(|word| word.contains("0.0.1")),
        "an update names what it replaces: {}",
        agents[1]["change"]
    );
    let migrations = json["update"]["migrations"].as_array().expect("migrations");
    assert_eq!(migrations.len(), crate::commands::update::MIGRATIONS.len());
    if let Some(first) = migrations.first() {
        assert!(first["name"].as_str().is_some_and(|name| !name.is_empty()));
    }
}

/// The daemon link is answered from what the control client already knows, so it
/// works while the daemon is down: that is the point of asking.
#[tokio::test]
async fn the_daemon_link_answers_with_no_daemon_behind_it() {
    let status = DaemonStatus::of(&crate::commands::serve::testutil::no_daemon_client());
    // True before anything has been tried: this server talks to the daemon when
    // it has something to ask, so silence is not evidence either way. Reporting
    // "not reachable" here would be a claim nobody has checked.
    assert!(status.reachable, "nothing has failed yet");
    assert!(status.version.is_none(), "nobody has introduced themselves");
    assert!(status.build.is_none());
    assert!(status.pid.is_none());
    assert_eq!(status.restarts, 0);
    assert!(status.restart_advised.is_none());
}

/// An update job comes through with each step and where it got to.
#[tokio::test]
async fn an_update_job_carries_its_steps() {
    let jobs = crate::commands::serve::update_job::UpdateJobs::default();
    let id = jobs.start().expect("nothing else is running");
    let job = UpdateJob::from(jobs.get(&id).expect("the job is recorded"));
    assert_eq!(job.id, id);
    assert_eq!(job.status, "running");
    // Three steps, always: a step that was not asked for reads as skipped rather
    // than being absent, so a client renders the same rows whatever was asked.
    assert_eq!(job.steps.len(), 3);
    assert!(job.steps.iter().all(|step| !step.step.is_empty()));
    assert!(job.steps.iter().all(|step| !step.status.is_empty()));
}

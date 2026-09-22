//! Self-update, and who is on the other end of the control socket.
//!
//! Both are about this machine rather than about a run. The plan and the daemon
//! status are one value each; the update runs are a listing, because a server
//! that has been up for months has done more of them than one page holds.

use async_graphql::{ComplexObject, Context, Enum, SimpleObject, Union};
use leviath_graphql_derive::mirror;
use leviath_runtime::control_socket::ControlResponse;

use super::super::scalars::Timestamp;
use super::machine::JournalHealth;

/// Who is on the other end of the control socket.
#[mirror]
#[derive(Debug, SimpleObject)]
#[graphql(complex)]
pub(crate) struct DaemonStatus {
    /// Whether the last attempt to reach the daemon worked.
    ///
    /// True before anything has been tried, which is the honest answer: this
    /// server talks to the daemon when it has something to ask, so silence is not
    /// evidence either way. False means a request failed and none has succeeded
    /// since. Reads keep working through both, because the run store is on disk;
    /// what needs the daemon is acting on a run.
    ///
    /// For whether the live frames are flowing, watch `daemonLink` on a
    /// subscription: that one is reported by the stream itself.
    pub(crate) reachable: bool,
    /// The daemon's version, once it has said. Null before any daemon has
    /// introduced itself.
    pub(crate) version: Option<String>,
    /// Its build id, which is what tells a restart from an upgrade.
    pub(crate) build: Option<String>,
    /// Its process id.
    pub(crate) pid: Option<i32>,
    /// Which tool credentials the daemon can see, by name. Names only: no
    /// value crosses the wire.
    ///
    /// Null means no daemon has said, which is an older one or none reached
    /// yet. An empty list is the other answer: asked, and it sees none.
    pub(crate) tool_env: Option<Vec<String>>,
    /// How many times the daemon behind this link has changed process since
    /// this server started.
    pub(crate) restarts: i32,
    /// Present when the daemon and this server run different code, with what to
    /// do about it. Requests keep working while the two still understand each
    /// other, which is why this is advice rather than an error.
    pub(crate) restart_advised: Option<String>,
}

impl DaemonStatus {
    /// What the control client knows, as the schema says it.
    ///
    /// Takes the two readings rather than the client, so this stays a mapping
    /// with nothing to arrange: a daemon that has introduced itself, and one that
    /// runs different code, are both states a caller can hand over and neither
    /// needs a live socket to describe.
    pub(crate) fn of(
        link: leviath_runtime::control_socket::LinkStatus,
        mismatch: Option<leviath_runtime::control_socket::CodeMismatch>,
    ) -> Self {
        Self {
            reachable: link.reachable,
            version: link.daemon.as_ref().map(|daemon| daemon.version.clone()),
            build: link.daemon.as_ref().map(|daemon| daemon.build.clone()),
            pid: link
                .daemon
                .as_ref()
                .map(|daemon| i32::try_from(daemon.pid).unwrap_or(i32::MAX)),
            tool_env: link
                .daemon
                .as_ref()
                .and_then(|daemon| daemon.tool_env.clone()),
            restarts: i32::try_from(link.restarts).unwrap_or(i32::MAX),
            restart_advised: mismatch.map(|mismatch| mismatch.to_string()),
        }
    }
}

/// What only the daemon itself can answer.
#[ComplexObject]
impl DaemonStatus {
    /// Whether the daemon is still recording what its runs do.
    ///
    /// Null when the daemon cannot be reached, because this is its own reading
    /// and no other copy of it exists - `reachable` beside this says whether
    /// that is why. Everything else about a run is read from disk and keeps
    /// working while the daemon is down; this does not.
    ///
    /// Worth asking on any page that shows runs as healthy. A daemon whose
    /// journal is refusing writes serves every field here exactly as it did
    /// before, and a run whose journal record cannot be written is failed
    /// rather than carried on. It costs a control call, so it is asked only
    /// where it is selected.
    async fn journal(&self, ctx: &Context<'_>) -> Option<JournalHealth> {
        let state = ctx.data_unchecked::<super::super::super::types::AppState>();
        match state.control.list().await {
            Ok(ControlResponse::List { health, .. }) => Some(JournalHealth::of(&health.journal)),
            Ok(_) | Err(_) => None,
        }
    }
}

/// How this copy of Leviath was installed, which decides how it upgrades.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum InstallMethod {
    /// Homebrew, under a formula that carries the channel.
    Homebrew,
    /// Scoop, under a package that carries the channel the same way.
    Scoop,
    /// `cargo install`, so the binary was compiled here.
    Cargo,
    /// The hosted install script, or something else that dropped a plain binary
    /// where that script puts one.
    Script,
    /// Somewhere no supported installer writes. `upgrade.message` says where.
    Unknown,
}

/// Commands that would upgrade the binary.
///
/// A list rather than one command because a package manager will not see a
/// release published minutes ago until its own index is refreshed, so the
/// refresh and the upgrade only make sense together.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct UpgradeByCommand {
    /// Each command's own words, in order. They stop at the first failure.
    pub(crate) commands: Vec<String>,
    /// The same sequence as one line, joined with `&&`: what to paste into a
    /// shell to do it by hand.
    pub(crate) shell: String,
}

/// There is nothing to run, and this is why.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct UpgradeByAdvice {
    /// What to tell the person instead.
    pub(crate) message: String,
}

/// How the binary would be upgraded.
#[mirror]
#[derive(Debug, Union)]
pub(crate) enum BinaryUpgrade {
    /// By running commands.
    Command(UpgradeByCommand),
    /// By telling somebody something.
    Advice(UpgradeByAdvice),
}

/// One bundled blueprint, and what an update would do to it.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct UpdateBlueprintEntry {
    /// The blueprint's name.
    pub(crate) name: String,
    /// The version this build ships.
    pub(crate) version: String,
    /// What would happen to the installed copy, in words.
    pub(crate) change: String,
    /// Whether that is a change at all, rather than "already current".
    pub(crate) has_changes: bool,
    /// Whether an update would install it without being asked. A copy somebody
    /// edited is not, because overwriting it would throw that work away.
    pub(crate) preselected: bool,
}

/// One config migration that applies to the config as it stands.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct UpdateMigration {
    /// The migration's name.
    pub(crate) name: String,
    /// What it changes.
    pub(crate) description: String,
}

/// What an update would do, and whether there is anything newer to get.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct UpdatePlan {
    /// The version running now.
    pub(crate) version: String,
    /// How this copy was installed.
    pub(crate) install_method: InstallMethod,
    /// The release channel this install tracks, where that is knowable. Null for
    /// an install method that does not carry one.
    pub(crate) channel: Option<String>,
    /// The newest version there is. Null means nothing to show, which covers
    /// three cases that are one answer to a client: not checked yet, checking
    /// switched off, and the check failed.
    pub(crate) latest: Option<String>,
    /// Whether that is newer than this. Null for the same three cases.
    pub(crate) update_available: Option<bool>,
    /// When the check last answered. Null for the same three cases.
    pub(crate) checked_at: Option<Timestamp>,
    /// How the binary would be upgraded.
    pub(crate) binary: BinaryUpgrade,
    /// The bundled blueprints, and what would happen to each.
    pub(crate) blueprints: Vec<UpdateBlueprintEntry>,
    /// The config migrations that apply.
    pub(crate) migrations: Vec<UpdateMigration>,
    /// Why the config could not be read, when it could not. The plan is still
    /// honest about the binary and the blueprints; it is the migrations it
    /// cannot judge.
    pub(crate) config_error: Option<String>,
}

impl UpdatePlan {
    /// Describe a plan and the last answer to "is there anything newer".
    pub(crate) fn from_plan(
        plan: &crate::commands::update::UpdatePlan,
        version: &str,
        latest: &crate::commands::update::latest::LatestCheck,
    ) -> Self {
        use crate::commands::update::BinaryStep;
        use crate::commands::update::detect::InstallMethod as Core;

        Self {
            version: version.to_string(),
            install_method: match &plan.method {
                Core::Homebrew { .. } => InstallMethod::Homebrew,
                Core::Scoop { .. } => InstallMethod::Scoop,
                Core::Cargo => InstallMethod::Cargo,
                Core::Script { .. } => InstallMethod::Script,
                Core::Unknown { .. } => InstallMethod::Unknown,
            },
            channel: plan
                .method
                .channel()
                .map(|channel| channel.id().to_string()),
            latest: latest.latest.clone(),
            update_available: latest.update_available,
            checked_at: latest
                .checked_at
                .map(|at| Timestamp(i64::try_from(at).unwrap_or(i64::MAX))),
            binary: match &plan.binary {
                BinaryStep::Run(commands) => BinaryUpgrade::Command(UpgradeByCommand {
                    commands: commands.iter().map(|argv| argv.join(" ")).collect(),
                    shell: crate::commands::update::render_commands(commands),
                }),
                BinaryStep::Advise(message) => BinaryUpgrade::Advice(UpgradeByAdvice {
                    message: message.clone(),
                }),
            },
            blueprints: plan
                .agents
                .iter()
                .map(|(bundled, action)| UpdateBlueprintEntry {
                    name: bundled.name.to_string(),
                    version: bundled.version.to_string(),
                    change: action.label(bundled.version),
                    has_changes: action.is_change(),
                    preselected: action.preselect(),
                })
                .collect(),
            migrations: plan
                .migrations
                .iter()
                .map(|migration| UpdateMigration {
                    name: migration.name.to_string(),
                    description: migration.description.to_string(),
                })
                .collect(),
            config_error: match &plan.config {
                crate::commands::update::ConfigState::Unreadable(e) => Some(e.clone()),
                crate::commands::update::ConfigState::Loaded(_) => None,
            },
        }
    }
}

/// One step of an update run.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct UpdateJobStep {
    /// Which step this is.
    pub(crate) step: UpdateStep,
    /// Where it got to.
    pub(crate) status: UpdateStepStatus,
    /// What happened, in words.
    pub(crate) detail: String,
}

/// The steps an update runs, in order.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::Enum)]
pub(crate) enum UpdateStep {
    /// Leviath itself. First, because what the other steps do is decided by
    /// what the new binary ships.
    Binary,
    /// The bundled blueprints in the agents directory.
    Blueprints,
    /// Keys in the reader's own blueprints that changed name.
    Keys,
    /// The config file.
    Migrations,
}

/// Where one step of an update got to.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::Enum)]
pub(crate) enum UpdateStepStatus {
    /// Not reached yet.
    Pending,
    /// Happening now.
    Running,
    /// Did what it set out to.
    Done,
    /// The request did not ask for it, or it had nothing to do.
    Skipped,
    /// The reader's to carry out, with the reason in `detail`. Neither success
    /// nor failure: nothing was done and nothing went wrong.
    Advised,
    /// Tried, and did not manage it.
    Failed,
}

/// Where an update run as a whole got to.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::Enum)]
pub(crate) enum UpdateJobStatus {
    /// Still going.
    Running,
    /// Every step finished and none failed.
    Complete,
    /// At least one step failed.
    Failed,
}

/// One update run.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct UpdateJob {
    /// The job's id, which `updateJob` and `node` both take. Unique to this
    /// server: the jobs live in memory, so nothing answers to it after a
    /// restart.
    ///
    /// It carries the second the job was started, so ordering by it is
    /// ordering by when it ran.
    #[graphql(owned)]
    #[filter(orderable)]
    pub(crate) id: async_graphql::ID,
    /// Where the run as a whole got to.
    pub(crate) status: UpdateJobStatus,
    /// Each step, in the order they run. A step that was not asked for is
    /// `SKIPPED` rather than absent, so a client renders the same rows
    /// whatever the request was.
    pub(crate) steps: Vec<UpdateJobStep>,
}

impl super::super::connection::Paged for UpdateJob {
    const NAME: &'static str = "UpdateJob";
}

impl From<super::super::super::update_job::UpdateJob> for UpdateJob {
    fn from(job: super::super::super::update_job::UpdateJob) -> Self {
        use super::super::super::update_job::{JobStatus, Step, StepStatus};
        Self {
            id: async_graphql::ID(job.id),
            status: match job.status {
                JobStatus::Running => UpdateJobStatus::Running,
                JobStatus::Complete => UpdateJobStatus::Complete,
                JobStatus::Failed => UpdateJobStatus::Failed,
            },
            steps: job
                .steps
                .into_iter()
                .map(|step| UpdateJobStep {
                    step: match step.step {
                        Step::Binary => UpdateStep::Binary,
                        Step::Agents => UpdateStep::Blueprints,
                        Step::Keys => UpdateStep::Keys,
                        Step::Migrations => UpdateStep::Migrations,
                    },
                    status: match step.status {
                        StepStatus::Pending => UpdateStepStatus::Pending,
                        StepStatus::Running => UpdateStepStatus::Running,
                        StepStatus::Done => UpdateStepStatus::Done,
                        StepStatus::Skipped => UpdateStepStatus::Skipped,
                        StepStatus::Advised => UpdateStepStatus::Advised,
                        StepStatus::Failed => UpdateStepStatus::Failed,
                    },
                    detail: step.detail,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
#[path = "update_tests.rs"]
mod tests;

//! The settings that say how a run behaves rather than what it does: where
//! tools execute, what is nudged, what is tracked, what is summarized.
//!
//! Every one of these cascades. A stage's value wins over the blueprint's, and
//! the blueprint's over the machine's config, each field on its own. So a value
//! here is what this level declared, and null means "whatever the level above
//! says". `Stage.effective` is where the resolved answer lives.

use std::sync::Arc;

use async_graphql::{Enum, Object, SimpleObject};
use leviath_graphql_derive::mirror;

use leviath_core::Blueprint as CoreBlueprint;

use super::count;

/// Whether a stage is sent back round when it answers with text instead of
/// calling a tool.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum NudgePolicy {
    /// Take the level above.
    Inherit,
    /// Send it back round with the nudge.
    Nudge,
    /// Take the text as the answer.
    NeverNudge,
}

impl From<Option<bool>> for NudgePolicy {
    fn from(declared: Option<bool>) -> Self {
        match declared {
            None => Self::Inherit,
            Some(true) => Self::Nudge,
            Some(false) => Self::NeverNudge,
        }
    }
}

/// What happens when a model answers with text before calling any tool.
///
/// Each field cascades on its own, so a level that sets only `max` inherits the
/// policy and the text. Unset everywhere, the nudge fires, except for a stage
/// with checkpoints, whose text is its work product.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct NudgeConfig {
    /// Whether the nudge fires.
    pub(crate) policy: NudgePolicy,
    /// How many text-only answers are nudged before the text is taken as final.
    pub(crate) max: Option<i32>,
    /// The nudge itself. `{stage}` and `{regions}` are filled in.
    pub(crate) text: Option<String>,
}

impl From<&leviath_core::blueprint::NudgeConfig> for NudgeConfig {
    fn from(nudge: &leviath_core::blueprint::NudgeConfig) -> Self {
        Self {
            policy: NudgePolicy::from(nudge.enabled),
            max: nudge.max.map(count),
            text: nudge.text.clone(),
        }
    }
}

/// Whether taint tracking runs.
///
/// Two states, not three: a manifest can ask for tracking, and it cannot ask for
/// less than the machine already insists on. There is no way to spell "off".
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum TaintTracking {
    /// Take the machine's own setting.
    Inherit,
    /// Track, whatever the machine's setting.
    Track,
}

/// What this level asks of the taint layer.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct BlueprintSecurity {
    /// Whether content that came from outside is followed through the run, so a
    /// tool call carrying it can be held.
    pub(crate) taint_tracking: TaintTracking,
}

impl From<&leviath_core::taint::SecurityConfig> for BlueprintSecurity {
    fn from(security: &leviath_core::taint::SecurityConfig) -> Self {
        Self {
            taint_tracking: match security.taint_tracking {
                true => TaintTracking::Track,
                false => TaintTracking::Inherit,
            },
        }
    }
}

/// Where a stage's tools run.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum SandboxKind {
    /// On the host, which is the default and an explicit opt-out.
    None,
    /// Under fresh Linux namespaces. Linux only.
    Namespace,
    /// Inside a container.
    Container,
}

impl From<leviath_core::sandbox::SandboxKind> for SandboxKind {
    fn from(kind: leviath_core::sandbox::SandboxKind) -> Self {
        use leviath_core::sandbox::SandboxKind as Core;
        match kind {
            Core::None => Self::None,
            Core::Namespace => Self::Namespace,
            Core::Container => Self::Container,
        }
    }
}

/// What happens when the sandbox cannot be established.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum SandboxUnavailable {
    /// The spawn fails. The safe default for code you did not write.
    Error,
    /// A warning, and the tools run on the host after all.
    Warn,
}

impl From<leviath_core::sandbox::OnUnavailable> for SandboxUnavailable {
    fn from(policy: leviath_core::sandbox::OnUnavailable) -> Self {
        use leviath_core::sandbox::OnUnavailable as Core;
        match policy {
            Core::Error => Self::Error,
            Core::Warn => Self::Warn,
        }
    }
}

/// Where tools execute.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct SandboxConfig {
    /// Which kind of isolation.
    pub(crate) kind: SandboxKind,
    /// The container image, for a container sandbox.
    pub(crate) image: Option<String>,
    /// The container engine, when the manifest names one rather than letting the
    /// daemon pick what is on the machine.
    pub(crate) engine: Option<String>,
    /// Whether the sandbox reaches the network.
    pub(crate) allow_network: bool,
    /// Paths mounted into it, as written.
    pub(crate) mounts: Vec<String>,
    /// Whether one container is kept warm across the run's stages rather than
    /// built per call. It is still torn down when the run ends.
    pub(crate) keep_warm: bool,
    /// What happens when it cannot be established.
    pub(crate) on_unavailable: SandboxUnavailable,
}

impl From<&leviath_core::sandbox::ToolSandboxConfig> for SandboxConfig {
    fn from(sandbox: &leviath_core::sandbox::ToolSandboxConfig) -> Self {
        Self {
            kind: SandboxKind::from(sandbox.kind),
            image: sandbox.image.clone(),
            engine: sandbox.engine.clone(),
            allow_network: sandbox.network,
            mounts: sandbox.mounts.clone(),
            keep_warm: sandbox.keep_warm,
            on_unavailable: SandboxUnavailable::from(sandbox.on_unavailable),
        }
    }
}

/// The scripts a stage runs at points in its own lifecycle.
///
/// Each names a Rhai file relative to the blueprint. The function a script has to
/// define is named for the field it is given as, so one file may back several
/// hooks. A stage that declares none costs nothing: no file is read and no
/// engine is built.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct StageHooks {
    /// As the stage is entered, before its first inference.
    pub(crate) on_stage_enter: Option<String>,
    /// When the stage finishes, before its transitions are considered.
    pub(crate) on_stage_exit: Option<String>,
    /// With the context assembled, before the request goes out.
    pub(crate) before_inference: Option<String>,
    /// With the model's answer in hand, before it reaches the context.
    pub(crate) after_inference: Option<String>,
    /// With the model's tool calls, before the permission and taint layers see
    /// them, so a hook can narrow what runs and never widen it.
    pub(crate) on_tool_call: Option<String>,
    /// Once, when the run finishes successfully.
    pub(crate) on_completion: Option<String>,
    /// Once, when the run finishes in error.
    pub(crate) on_error: Option<String>,
}

impl From<&leviath_core::blueprint::StageHooks> for StageHooks {
    fn from(hooks: &leviath_core::blueprint::StageHooks) -> Self {
        Self {
            on_stage_enter: hooks.on_stage_enter.clone(),
            on_stage_exit: hooks.on_stage_exit.clone(),
            before_inference: hooks.before_inference.clone(),
            after_inference: hooks.after_inference.clone(),
            on_tool_call: hooks.on_tool_call.clone(),
            on_completion: hooks.on_completion.clone(),
            on_error: hooks.on_error.clone(),
        }
    }
}

/// What the blueprint would like to run without being asked.
///
/// A request rather than a grant: the machine's own policy decides, and this is
/// what an operator reads when they are deciding whether to write it in.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct SafeCommands {
    /// Tools the blueprint would like allowed outright, by the names the manifest
    /// used. Names rather than `Tool`s: a request may name an MCP server's tool,
    /// a group token, or one this machine does not have, and it is still the
    /// request the author wrote.
    pub(crate) tools: Vec<String>,
    /// Shell command lines it would like allowed outright.
    pub(crate) shell: Vec<String>,
}

impl From<&leviath_core::blueprint::SafeCommandsConfig> for SafeCommands {
    fn from(safe: &leviath_core::blueprint::SafeCommandsConfig) -> Self {
        Self {
            tools: safe.tools.clone(),
            shell: safe.shell.clone(),
        }
    }
}

/// When a run is stopped for going round in circles.
///
/// For the degenerate read loop: the same call again and again, or a long run of
/// read-only calls with nothing produced between them.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct RepetitionDetection {
    /// Whether detection runs. Null inherits, and unset everywhere it is on.
    pub(crate) enabled: Option<bool>,
    /// How many times one call with one set of arguments may repeat before a
    /// nudge.
    pub(crate) max_repeat_calls: Option<i32>,
    /// How many read-only calls may run in a row with nothing produced between
    /// them.
    pub(crate) max_readonly_streak: Option<i32>,
}

impl From<&leviath_core::blueprint::RepetitionDetectionConfig> for RepetitionDetection {
    fn from(detection: &leviath_core::blueprint::RepetitionDetectionConfig) -> Self {
        Self {
            enabled: detection.enabled,
            max_repeat_calls: detection.max_repeat_calls.map(count),
            max_readonly_streak: detection.max_readonly_streak.map(count),
        }
    }
}

/// The resolver state behind the `FileTrackingConfig` type.
pub(crate) struct FileTrackingConfig {
    /// The blueprint the region name resolves in.
    blueprint: Arc<CoreBlueprint>,
    /// The tracking block as the blueprint wrote it.
    tracking: leviath_core::blueprint::FileTrackingConfig,
}

/// Keeping the files a run reads and writes in one region.
///
/// So a tool result can point at the region rather than repeating a file the
/// context already holds.
#[mirror]
#[Object]
impl FileTrackingConfig {
    /// The key-value region the files are synced to.
    ///
    /// Null where no layout in this blueprint declares that name, which is file
    /// tracking with nowhere to write. `regionName` carries the name either way.
    async fn region(&self) -> Option<super::super::blueprint::Region> {
        super::refs::region(&self.blueprint, &self.tracking.region)
    }

    /// The region name the blueprint wrote, verbatim.
    async fn region_name(&self) -> &str {
        &self.tracking.region
    }

    /// Whether a read updates it.
    async fn track_reads(&self) -> bool {
        self.tracking.track_reads
    }

    /// Whether a write updates it. An edit does not: its arguments are the old
    /// and new text, so the file's new body is not there to record without
    /// reading it again.
    async fn track_writes(&self) -> bool {
        self.tracking.track_writes
    }

    /// The most tokens one file may take in the region before it is truncated.
    async fn max_file_tokens(&self) -> Option<i32> {
        self.tracking.max_file_tokens.map(count)
    }
}

impl FileTrackingConfig {
    /// Describe the tracking block against the blueprint that holds it.
    pub(crate) fn of(
        blueprint: &Arc<CoreBlueprint>,
        tracking: &leviath_core::blueprint::FileTrackingConfig,
    ) -> Self {
        Self {
            blueprint: Arc::clone(blueprint),
            tracking: tracking.clone(),
        }
    }
}

/// The model that summarizes a region when it fills.
///
/// Its own model on purpose: compaction is cheap, frequent and not the work, so
/// a run on an expensive model usually summarizes on a small one.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct CompactionConfig {
    /// The provider that serves the summarizer.
    pub(crate) provider: String,
    /// The model it runs on.
    pub(crate) model: String,
    /// What the summarizer is told it is for.
    pub(crate) system_prompt: Option<String>,
    /// The template the region's content is wrapped in.
    pub(crate) user_prompt_template: Option<String>,
    /// The longest summary it may return.
    pub(crate) max_summary_tokens: i32,
    /// How much it may wander. A summary wants little.
    pub(crate) temperature: f64,
}

impl From<&leviath_core::lifecycle::CompactionConfig> for CompactionConfig {
    fn from(compaction: &leviath_core::lifecycle::CompactionConfig) -> Self {
        Self {
            provider: compaction.provider.clone(),
            model: compaction.model.clone(),
            system_prompt: compaction.system_prompt.clone(),
            user_prompt_template: compaction.user_prompt_template.clone(),
            max_summary_tokens: count(compaction.max_summary_tokens),
            temperature: f64::from(compaction.temperature),
        }
    }
}

/// What happens to a fan-out when one worker fails.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum WorkerFailurePolicy {
    /// The others finish, and the merge stage sees what came back.
    Continue,
    /// The whole fan-out fails with the worker.
    FailAll,
}

impl From<&leviath_core::blueprint::WorkerFailurePolicy> for WorkerFailurePolicy {
    fn from(policy: &leviath_core::blueprint::WorkerFailurePolicy) -> Self {
        use leviath_core::blueprint::WorkerFailurePolicy as Core;
        match policy {
            Core::Continue => Self::Continue,
            Core::FailAll => Self::FailAll,
        }
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;

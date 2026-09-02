//! One stage of a blueprint: what the agent does, in what mode, with what.
//!
//! [`Stage`] is the unit an agent occupies at any moment, and [`StageMode`] is
//! what makes stages differ in kind rather than degree - an autonomous stage
//! loops on tools, a fan-out stage splits into workers, an interaction-points
//! stage stops for a person. The per-mode configuration lives beside the mode
//! it configures rather than in a flat bag on `Stage`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::ValidationError;
use crate::layout::ContextLayout;

// The sibling sections, reached through the parent's glob re-exports so a type
// moving between them does not touch this import list.
use super::*;

/// Configuration for automatic file tracking in context regions.
///
/// When configured, read_file/write_file results are automatically synced to a
/// HashMap region, and tool results reference the system prompt instead of
/// duplicating content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTrackingConfig {
    /// Name of the HashMap region to sync files to
    pub region: String,
    /// Auto-update on read_file
    #[serde(default = "default_true_val")]
    pub track_reads: bool,
    /// Auto-update on write_file. (`edit_file` is not tracked: its arguments are
    /// `old_str`/`new_str`, so the post-edit file body isn't available without a
    /// re-read.)
    #[serde(default = "default_true_val")]
    pub track_writes: bool,
    /// Truncate files larger than this token count in context
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_tokens: Option<usize>,
}

fn default_true_val() -> bool {
    true
}

/// Configuration for repetition detection in the inference loop.
///
/// Controls thresholds for detecting degenerate read loops where agents
/// call the same tool repeatedly without productive action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepetitionDetectionConfig {
    /// Maximum times the same tool+args combo may repeat before a nudge.
    pub max_repeat_calls: Option<usize>,
    /// Maximum consecutive read-only calls with no productive calls in between.
    pub max_readonly_streak: Option<usize>,
    /// Whether detection is enabled. Default: true.
    pub enabled: Option<bool>,
}

/// Configuration for routing tool results to specific context window regions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultRouting {
    /// Default region for tool results (default: "tool_results")
    pub default_region: String,
    /// Per-tool overrides: tool_name → region_name
    pub tool_overrides: HashMap<String, String>,
    /// Whether to keep tool results (true) or discard after use (false)
    pub persist: bool,
    /// Max tokens per tool result (truncate if larger)
    pub max_result_tokens: Option<usize>,
    /// Per-tool ceilings, by canonical tool name, overriding
    /// [`Self::max_result_tokens`] for those tools.
    ///
    /// One number for a whole stage cannot fit a stage that both greps (small,
    /// wants all of it) and reads files (potentially enormous, wants a cap):
    /// picking a number for the second starves the first. Keyed by canonical
    /// name for the same reason [`Self::tool_overrides`] is - `bash` is an
    /// alias of `shell`, and a literal lookup would silently miss.
    #[serde(default)]
    pub tool_max_result_tokens: HashMap<String, usize>,
}

impl Default for ToolResultRouting {
    fn default() -> Self {
        Self {
            default_region: "tool_results".to_string(),
            tool_overrides: HashMap::new(),
            persist: true,
            max_result_tokens: None,
            tool_max_result_tokens: HashMap::new(),
        }
    }
}

/// Interaction mode for a stage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum StageMode {
    /// Runs without user input, fully autonomous
    #[default]
    Autonomous,

    /// Requires user input before starting
    Interactive,

    /// Can receive input at defined points during execution
    InteractivePoints {
        /// Points where user input can be requested
        points: Vec<InteractionPoint>,
    },

    /// Splits work into JSON items and runs them across parallel in-process
    /// sub-agent workers, then optionally merges before transitioning.
    FanOut {
        /// Fan-out configuration (worker source, concurrency, failure policy).
        config: FanOutConfig,
    },

    /// Produces the run's final output and nothing else.
    ///
    /// Sugar over three settings the manifest parser applies on the author's
    /// behalf: `submit_output` is added to `available_tools`, `require_output`
    /// is turned on, and `allow_complete` defaults to true (an output stage is
    /// normally the last thing a run does). Everything it does is expressible
    /// without the mode; naming it says what the stage is *for*.
    Output,
}

impl PartialEq for StageMode {
    #[inline(never)]
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Autonomous, Self::Autonomous)
            | (Self::Interactive, Self::Interactive)
            | (Self::Output, Self::Output) => true,
            (Self::InteractivePoints { points: a }, Self::InteractivePoints { points: b }) => {
                a == b
            }
            (Self::FanOut { config: a }, Self::FanOut { config: b }) => a == b,
            _ => false,
        }
    }
}
impl Eq for StageMode {}

/// How a fan-out stage handles worker failures.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFailurePolicy {
    /// Run the merge/next stage with the successful workers; failures are
    /// reported into the consolidated results.
    #[default]
    Continue,
    /// Any worker failure routes the fan-out stage down its `error` edge.
    FailAll,
}

/// Configuration for a [`StageMode::FanOut`] stage.
///
/// Exactly one of `worker_agent` / `worker_stage` / `worker_query` selects the
/// worker's agent type (validated when the blueprint's graph is checked).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FanOutConfig {
    /// A separate registered/installed blueprint run as the worker agent type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_agent: Option<String>,
    /// A stage in *this* blueprint (self-as-agent-type); must be marked
    /// `allow_as_worker = true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_stage: Option<String>,
    /// Discovery hint matched against installed agent types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_query: Option<String>,
    /// Optional stage that reconciles worker results before transitioning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_stage: Option<String>,
    /// Most workers running at once. Defaults to [`DEFAULT_MAX_WORKERS`].
    ///
    /// `0` means unlimited: every work item starts as soon as the split has
    /// produced it, and the daemon's inference pool (`[limits]
    /// max_concurrent_inferences`) is what paces the requests. Read it through
    /// [`Self::worker_cap`] rather than comparing against zero by hand.
    #[serde(default = "default_max_workers")]
    pub max_workers: usize,
    /// How to handle worker failures.
    #[serde(default)]
    pub on_worker_failure: WorkerFailurePolicy,
    /// Prompt that produces the JSON array of work items (one per worker).
    #[serde(default)]
    pub split_prompt: String,

    /// Context region the consolidated worker report is written to. `None`
    /// means `conversation`.
    ///
    /// Worth naming when the results are bulky. `conversation` is a sliding
    /// window carrying the message history, so a large report competes with the
    /// turns around it and can be evicted by them. A region of its own gets a
    /// budget of its own, and that budget is what each worker's share is
    /// divided from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results_region: Option<String>,

    /// Most work items the split may produce. `None` means however many it
    /// produces (a manifest spells that `max_items = 0`, or leaves the key
    /// out).
    ///
    /// Distinct from `max_workers`, which caps how many run *at once*. This
    /// caps how many there are at all, which is what bounds both the run's cost
    /// and each worker's share of the results region: split a hundred ways and
    /// every worker's contribution is a hundredth of the space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_items: Option<usize>,

    /// How many times this stage is asked again when it ends without having
    /// called the fan-out tool, before it is let through without workers.
    /// `None` means [`DEFAULT_FAN_OUT_ATTEMPTS`].
    ///
    /// Starting the workers is the whole job of a fan-out stage, so a model
    /// that answers in prose instead is asked again. The budget is bounded
    /// because a run must never be stranded over a thing the model will not do,
    /// and it is separate from `max_revisits` because those are different
    /// questions: "how many times may the graph re-enter this stage" and "how
    /// many times do we ask a model that has not done what the stage is for".
    /// Borrowing the first for the second is how a routing setting silently
    /// multiplies an inference bill.
    ///
    /// Raise it for a small or local model that needs more than a nudge; `0`
    /// lets the stage through on its first refusal, which is the right setting
    /// when an empty fan-out is an acceptable outcome and the retries are not
    /// worth their prompts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<usize>,
}

impl FanOutConfig {
    /// The concurrency cap as an option: `Some(n)` for `max_workers = n`,
    /// `None` when the stage is unlimited (`max_workers = 0`).
    ///
    /// The runtime and the API both want the question answered this way, and
    /// answering it in one place keeps "zero is unlimited" from being restated
    /// wherever the number is read.
    pub fn worker_cap(&self) -> Option<usize> {
        (self.max_workers > 0).then_some(self.max_workers)
    }
}

/// Times a fan-out stage is asked again to start its workers before it is let
/// through without them, when it sets no `max_attempts`.
///
/// Matches [`DEFAULT_GATE_ATTEMPTS`](crate::blueprint::DEFAULT_GATE_ATTEMPTS)
/// and the missing-output budget, for the same reason all three are bounded: a
/// model that cannot produce the one thing its stage is for should cost a fixed
/// number of prompts, not an open-ended retry.
pub const DEFAULT_FAN_OUT_ATTEMPTS: usize = 3;

/// `max_workers` when a fan-out stage does not set one.
///
/// Thirty rather than the four this started at. Four made a fan-out that split
/// ten ways run in three waves, and the wait for the last wave was the wait a
/// person saw. The inference pool caps concurrent model requests either way, so
/// a wide fan-out over a narrow pool queues at the provider rather than at the
/// stage.
pub const DEFAULT_MAX_WORKERS: usize = 30;

/// Default `max_workers` when unspecified.
fn default_max_workers() -> usize {
    DEFAULT_MAX_WORKERS
}

/// Style of interaction at an interaction point.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionStyle {
    /// Free-form text answer (default).
    #[default]
    FreeText,
    /// User picks one option from a list.
    MultipleChoice,
    /// Simple yes/no confirmation.
    Confirm,
}

impl PartialEq for InteractionStyle {
    #[inline(never)]
    fn eq(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}
impl Eq for InteractionStyle {}

/// What an interaction point does when the run is unattended (`--yolo`).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UnattendedPolicy {
    /// Resolve the point as approved without opening a prompt. The default:
    /// `--yolo` means nobody is watching, and a checkpoint nobody can answer
    /// would park the run for as long as the daemon lives.
    #[default]
    AutoApprove,

    /// Open the prompt anyway and wait for a person, even under `--yolo`. For a
    /// checkpoint whose whole purpose is a human decision - a plan the user
    /// signs off before any code is written. The run waits until somebody
    /// answers; set `[limits] interaction_timeout_secs` to bound that wait.
    Ask,
}

/// A point where a stage can request user input.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InteractionPoint {
    /// Unique name for this interaction point
    pub name: String,

    /// Prompt to show the user
    pub prompt: String,

    /// Whether input is required (vs optional). A presentation hint carried on
    /// the prompt - not to be confused with [`InteractionPoint::unattended`],
    /// which decides whether the prompt is raised at all in a `--yolo` run.
    pub required: bool,

    /// What this point does when nobody is watching. Defaults to
    /// [`UnattendedPolicy::AutoApprove`]; set `unattended = "ask"` to hold the
    /// run for a person even under `--yolo`.
    #[serde(default)]
    pub unattended: UnattendedPolicy,

    /// Style of interaction (free text, multiple choice, confirm)
    #[serde(default)]
    pub style: InteractionStyle,

    /// Options for MultipleChoice style
    #[serde(default)]
    pub options: Vec<String>,

    /// Directives keyed by option label.
    ///
    /// When the user picks an option present in this map (e.g. "Revise - I'll
    /// describe changes"), the mapped directive text is injected into the
    /// agent's conversation context and the stage re-runs inference IN-STAGE
    /// (bounded by a revision cap) instead of falling through to a stage
    /// transition. The directive tells the agent what to do next - e.g. call
    /// `ask_user_text` to learn what to change, or `edit_document` to let the
    /// user edit the plan directly - so the routing decision is deterministic
    /// (code) while the actual input capture is an agent tool call.
    #[serde(default, alias = "followups")]
    pub directives: HashMap<String, String>,

    /// Options that, when selected, immediately abort the run: the engine
    /// marks the run cancelled and stops with no further inference and no
    /// transition resolution. Matched against the selected option label with
    /// the same dash/whitespace normalization used for directive lookup.
    #[serde(default)]
    pub abort_options: Vec<String>,

    /// Options that, when selected, open the stage's most recent output (e.g.
    /// the plan) in an editable field so the user can modify it directly. The
    /// engine issues the edit interaction itself and injects the edited text
    /// back into context - deterministic, with no dependence on the model
    /// choosing to call an edit tool. Matched with the same normalization.
    #[serde(default)]
    pub edit_options: Vec<String>,

    /// Optional pinned region to hold this point's authoritative document (e.g.
    /// `"plan"`). When set, each time the point is presented the current
    /// document - the produced text, or the user's direct edit - *replaces* that
    /// region's content, so later revisions and downstream stages build on the
    /// current version rather than regenerating from the task. `None` ⇒ the
    /// document lives only in the rolling conversation / output.
    #[serde(default)]
    pub document_region: Option<String>,
}

/// Script-backed lifecycle hooks for a stage: `[stages.<name>.hooks]`.
///
/// Each field names a `.rhai` file, resolved relative to the blueprint
/// directory exactly as a custom region's script is. Every hook is optional and
/// an agent that declares none pays nothing - no file is read and no engine is
/// built.
///
/// The hook a script implements is the function it defines, named for the
/// field: a file given as `on_stage_enter` must define `fn on_stage_enter(ctx)`.
/// One file may back several hooks by defining several functions.
///
/// Hooks are a **return-value contract**, like region hooks: Rhai passes
/// arguments by value, so mutating `ctx` in place does nothing and the script
/// must return its decision. See `leviath_scripting::stage_hook`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StageHooks {
    /// Fires as the agent enters the stage, before its first inference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_stage_enter: Option<String>,
    /// Fires when the stage finishes, before transition evaluation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_stage_exit: Option<String>,
    /// Fires with the context assembled, before the request is dispatched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_inference: Option<String>,
    /// Fires with the model's response in hand, before it reaches context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_inference: Option<String>,
    /// Fires with the model's tool calls, before the policy and taint layers
    /// see them - so a hook can narrow what runs, never widen it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_tool_call: Option<String>,
    /// Fires once when the run finishes successfully.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_completion: Option<String>,
    /// Fires once when the run finishes in error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_error: Option<String>,
}

impl StageHooks {
    /// Whether any hook is declared. The whole feature is skipped when not -
    /// no file read, no compile, no engine.
    pub fn is_empty(&self) -> bool {
        self.on_stage_enter.is_none()
            && self.on_stage_exit.is_none()
            && self.before_inference.is_none()
            && self.after_inference.is_none()
            && self.on_tool_call.is_none()
            && self.on_completion.is_none()
            && self.on_error.is_none()
    }

    /// Every script path this stage declares, with the hook it backs.
    ///
    /// Returned as pairs rather than a set because the same file may back more
    /// than one hook, and the caller needs to know which function to look for.
    pub fn declared(&self) -> Vec<(&'static str, &str)> {
        let mut out = Vec::new();
        if let Some(p) = self.on_stage_enter.as_deref() {
            out.push(("on_stage_enter", p));
        }
        if let Some(p) = self.on_stage_exit.as_deref() {
            out.push(("on_stage_exit", p));
        }
        if let Some(p) = self.before_inference.as_deref() {
            out.push(("before_inference", p));
        }
        if let Some(p) = self.after_inference.as_deref() {
            out.push(("after_inference", p));
        }
        if let Some(p) = self.on_tool_call.as_deref() {
            out.push(("on_tool_call", p));
        }
        if let Some(p) = self.on_completion.as_deref() {
            out.push(("on_completion", p));
        }
        if let Some(p) = self.on_error.as_deref() {
            out.push(("on_error", p));
        }
        out
    }
}

/// A single execution stage in an agent's workflow.
///
/// Stages allow an agent to use different models or configurations for
/// different phases of work. For example, a coding agent might have:
/// - Analyze stage: fast model for understanding requirements
/// - Implement stage: powerful model for code generation
/// - Review stage: critique model for checking quality
///
/// Each stage can have its own context layout (memory structure), allowing
/// different stages to have different region configurations optimized for
/// their specific needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stage {
    /// Name of this stage
    pub name: String,

    /// Description of what this stage does
    pub description: Option<String>,

    /// Model to use for this stage
    pub model: ModelConfig,

    /// Which tools are available in this stage
    pub available_tools: Vec<String>,

    /// Human-in-the-loop tools (`ask_user_*`, `present_for_review`,
    /// `edit_document`) that survive an unattended run.
    ///
    /// An unattended run - `lev run --yolo`, or a child of one - drops every
    /// blocking human tool from the stage's advertised set, because a call to
    /// one can only park the agent until the daemon dies. Naming a
    /// tool here says this stage genuinely needs a person and the run should
    /// wait for one anyway, for as long as it takes; set `[limits]
    /// interaction_timeout_secs` to bound that wait.
    ///
    /// Entries must also appear in `available_tools` - listing a tool the stage
    /// can't call in the first place is a validation error, not a silent no-op.
    /// Matched verbatim against `available_tools` (this crate has no alias
    /// table), so write the name the same way in both.
    #[serde(default)]
    pub required_tools: Vec<String>,

    /// MCP servers whose whole tool set this stage may use.
    ///
    /// `available_tools` is an exact-match list, which makes a server's tools
    /// the author's problem to enumerate - and a server's tool list is not the
    /// author's to know. It is whatever that server advertises today. A tool
    /// added to the server later is simply never offered, with nothing said,
    /// so the stage quietly cannot do a thing its author believed it could.
    ///
    /// Naming the server instead defers the question to spawn, when the answer
    /// is known. Resolved against what the server actually advertises then,
    /// and merged with whatever `available_tools` names, so the two can be
    /// mixed freely.
    ///
    /// Kept separate from `available_tools` rather than spelled as a wildcard
    /// in it, for two reasons. That field's contract is exact-match and every
    /// consumer reads it that way; and the advertised name of an MCP tool does
    /// not reliably carry its server, so there is no prefix a wildcard could
    /// match on (see `unique_advertised_name` in `leviath-mcp`).
    #[serde(default)]
    pub available_connectors: Vec<String>,

    /// Maximum iterations for this stage
    pub max_iterations: Option<usize>,

    /// Interaction mode (autonomous or interactive)
    #[serde(default)]
    pub mode: StageMode,

    /// Optional stage-specific context layout
    /// If None, uses the blueprint's global layout
    pub context_layout: Option<ContextLayout>,

    /// Regions this stage does not carry in its prompt
    /// (`[stages.<name>.context] hide = [...]`). Hidden, not destroyed: the
    /// content is kept and every other stage sees it as before. The cheap way
    /// to drop one large region from a stage that its own instructions never
    /// read, without re-declaring the whole layout for that stage.
    #[serde(default)]
    pub context_hide: Vec<String>,

    /// Custom configuration for this stage
    pub config: HashMap<String, serde_json::Value>,

    /// Per-tool permission overrides for this stage.
    /// Keys: tool name. Values: "allow" | "ask" | "deny".
    /// Narrower than agent-level, wider than launch flags.
    #[serde(default)]
    pub tool_permissions: HashMap<String, String>,

    /// If true, don't advance to the next stage until all children spawned
    /// during this stage have completed.
    #[serde(default)]
    pub requires_children: bool,

    /// Directed transitions from this stage (None = linear/next-in-list)
    pub transitions: Option<HashMap<String, TransitionEdge>>,

    /// Max times this stage can be re-entered (revisits, not counting first visit)
    pub max_revisits: Option<usize>,

    /// Custom prompt for transition decisions (overrides default)
    pub transition_prompt: Option<String>,

    /// Whether this stage accepts mid-run user messages.
    /// When true, messages sent to the agent are injected into context
    /// between inference calls. Default: true.
    #[serde(default = "crate::default_true")]
    pub accepts_messages: bool,

    /// Whether the LLM may end the run at this stage instead of naming a
    /// transition target - e.g. a review stage that approves the work
    /// needs no further stage. When true, `prompt_llm_transition`'s query
    /// offers an explicit "DONE" response that resolves to a terminal
    /// (no-transition) outcome instead of forcing the single/first
    /// available edge.
    #[serde(default)]
    pub allow_complete: bool,

    /// Whether this stage may be used as a fan-out `worker_stage` - i.e. run as
    /// an in-process sub-agent worker entered at this stage. Off by default so a
    /// blueprint author must explicitly opt a stage in to being fanned into
    /// (you can only fan out into a stage designed for it).
    #[serde(default)]
    pub allow_as_worker: bool,

    /// Whether this stage means to offer human-in-the-loop tools (`ask_user_*`,
    /// `present_for_review`, `edit_document`) while running autonomously.
    ///
    /// Grants nothing and changes no runtime behavior: it only records the
    /// author's intent, so `lev validate` stops flagging a deliberate choice.
    /// An autonomous stage that calls one of those tools with nobody attached
    /// parks in `WaitingInput` until someone kills the run, which is almost
    /// always a mistake and occasionally exactly what was wanted (an agent
    /// driven from the dashboard, say). Off by default so the flag has to be
    /// written down.
    #[serde(default)]
    pub allow_blocking_tools: bool,

    /// Whether this stage also advertises every Rhai tool installed in the
    /// global tools directory (`~/.leviath/tools/`), on top of the names in
    /// `available_tools`.
    ///
    /// `available_tools` is an exact-match allowlist, so a tool that a previous
    /// run installed with `install_tool` is invisible to every stage that does
    /// not already name it. This flag is how a stage says "and whatever has
    /// been installed since": the daemon resolves the global inventory at spawn
    /// (and again on each `dynamic_tools` refresh) and appends the names to the
    /// grant list. Only scripts whose file lives in the global directory count;
    /// a same-named script in the agent's own `tools/` or the run workdir is
    /// never granted this way, so repository content cannot ride in under a
    /// global grant. Off by default: a stage has to opt in to running code it
    /// did not list.
    #[serde(default)]
    pub available_global_tools: bool,

    /// Per-stage taint/security override. `None` inherits the agent-level
    /// `Blueprint.security` (which in turn inherits the global config toggle).
    /// Set `taint_tracking = false` here to opt a single stage out, or `true`
    /// to opt it in independently of the agent/global setting.
    #[serde(default)]
    pub security: Option<crate::taint::SecurityConfig>,

    /// Per-stage override for the batch-tool-calls system-prompt hint. `None`
    /// inherits the agent-level `Blueprint.batch_tool_hint` (which in turn
    /// inherits the global config toggle). Set `false` to opt a sequential stage
    /// out, or `true` to opt it in independently of the agent/global setting.
    #[serde(default)]
    pub batch_tool_hint: Option<bool>,

    /// Per-stage override for the platform shell hint. `None` inherits the
    /// agent-level `Blueprint.shell_hint` (which in turn inherits the global
    /// config toggle). A stage that grants no shell tool never emits the hint
    /// regardless, so this is for opting a shell-granting stage out.
    #[serde(default)]
    pub shell_hint: Option<bool>,

    /// Per-stage empty-response nudge settings. Each field independently
    /// inherits the agent-level `Blueprint.nudge` (which in turn inherits the
    /// global config's `[nudge]` section). A stage whose deliverable is text -
    /// a planner, a briefing writer - sets `enabled = false` here so it is
    /// never told to "use your tools". See [`resolve_nudge`].
    #[serde(default)]
    pub nudge: Option<NudgeConfig>,

    /// Per-stage sandbox override. `None` inherits the agent-level
    /// `Blueprint.sandbox` (which in turn inherits the global default = host).
    /// Set a tighter sandbox here to isolate a single stage - e.g. run analysis
    /// on the host but implementation in a networkless container.
    #[serde(default)]
    pub sandbox: Option<crate::sandbox::ToolSandboxConfig>,

    /// Optional routing configuration for tool results.
    /// When set, tool results are routed to the configured region(s) instead
    /// of the default "conversation" region.
    #[serde(default)]
    pub tool_result_routing: Option<ToolResultRouting>,

    /// What shape this stage's final output should take, narrowing the
    /// agent-level `[agent.output]`. Whoever starts the run overrides both.
    ///
    /// Declaring a shape does not by itself demand one: pair it with
    /// [`Self::require_output`] (or `mode = "output"`, which sets that for you)
    /// when the stage must not finish without submitting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<crate::output::OutputSpec>,

    /// Whether this stage must call `submit_output` before it transitions.
    ///
    /// Unlike [`Self::required_tools`], which only keeps a blocking human tool
    /// advertised through an unattended run and is never checked afterwards,
    /// this *is* enforced: a stage that finishes without submitting is nudged
    /// and re-run, bounded, and then allowed through with the run's
    /// `output_forced` flag set. A missing output never strands a run.
    ///
    /// `mode = "output"` turns this on. Setting it on an ordinary stage is the
    /// way to demand a deliverable from a stage that also does other work, e.g.
    /// a fan-out worker whose summary its merge stage depends on.
    #[serde(default)]
    pub require_output: bool,
    /// Script-backed lifecycle hooks: `[stages.<name>.hooks]`. Absent ⇒ none,
    /// and nothing about the scripting engine is touched for this stage.
    #[serde(default, skip_serializing_if = "StageHooks::is_empty")]
    pub hooks: StageHooks,
}

impl Stage {
    /// Create a new stage with the specified configuration.
    pub fn new(name: String, model: ModelConfig) -> Self {
        Self {
            name,
            description: None,
            model,
            available_tools: Vec::new(),
            available_connectors: Vec::new(),
            required_tools: Vec::new(),
            max_iterations: None,
            mode: StageMode::Autonomous,
            context_layout: None,
            context_hide: Vec::new(),
            config: HashMap::new(),
            tool_permissions: HashMap::new(),
            requires_children: false,
            transitions: None,
            max_revisits: None,
            transition_prompt: None,
            accepts_messages: true,
            allow_complete: false,
            allow_as_worker: false,
            allow_blocking_tools: false,
            available_global_tools: false,
            security: None,
            batch_tool_hint: None,
            shell_hint: None,
            nudge: None,
            sandbox: None,
            tool_result_routing: None,
            output: None,
            require_output: false,
            hooks: StageHooks::default(),
        }
    }

    /// Add tools to this stage.
    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.available_tools = tools;
        self
    }

    /// Set the interaction mode for this stage.
    pub fn with_mode(mut self, mode: StageMode) -> Self {
        self.mode = mode;
        self
    }

    /// Set a stage-specific context layout.
    pub fn with_context_layout(mut self, layout: ContextLayout) -> Self {
        self.context_layout = Some(layout);
        self
    }

    /// Set the description for this stage.
    pub fn with_description(mut self, description: String) -> Self {
        self.description = Some(description);
        self
    }

    /// Validate that this stage is well-formed.
    pub(super) fn validate(&self) -> std::result::Result<(), ValidationError> {
        if self.name.is_empty() {
            return Err(ValidationError::Stage {
                stage: "(empty)".to_string(),
                message: "stage name cannot be empty".to_string(),
            });
        }

        // A `required_tools` entry the stage can't call is dead text: it looks
        // like it keeps a tool through an unattended run, and keeps nothing.
        // Rejected rather than ignored so the typo surfaces at `lev validate`
        // instead of at 3am in a `--yolo` run.
        for tool in &self.required_tools {
            if !self.available_tools.contains(tool) {
                return Err(ValidationError::Stage {
                    stage: self.name.clone(),
                    message: format!(
                        "required_tools entry '{}' is not in available_tools - a tool the \
                         stage cannot call can't be kept through an unattended run",
                        tool
                    ),
                });
            }
        }

        // A stage told to produce a final output that cannot call the tool would
        // burn its whole re-entry budget being nudged toward a tool it was never
        // offered, then give up. `mode = "output"` grants the tool at parse time,
        // so reaching this means someone set `require_output` by hand.
        if self.require_output && !self.available_tools.iter().any(|t| t == SUBMIT_OUTPUT_TOOL) {
            return Err(ValidationError::Stage {
                stage: self.name.clone(),
                message: format!(
                    "require_output is set but '{SUBMIT_OUTPUT_TOOL}' is not in available_tools - \
                     the stage cannot produce the output it is required to produce"
                ),
            });
        }

        // Validate stage-specific context layout if present
        if let Some(layout) = &self.context_layout {
            layout.validate()?;
        }

        Ok(())
    }
}

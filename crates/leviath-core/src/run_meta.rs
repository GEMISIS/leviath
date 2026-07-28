//! Plain, serializable run-state data types.
//!
//! These are pure data (`serde`-derived structs/enums plus trivial constructors)
//! with no filesystem or async dependencies, so they can be named by both
//! `leviath-cli` and the `leviath-runtime` engine. All on-disk IO for
//! these types (reading/writing `meta.json`, run directories, snapshots, etc.)
//! lives in `leviath_cli::runstate`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Current status of a background run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Starting,
    Running,
    WaitingInput,
    Complete,
    /// All required stages done; agent still accepts optional follow-up input.
    /// Shown as "Complete" in the dashboard — no kill option, input still enabled.
    CompleteInteractive,
    Error,
    Cancelled,
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunStatus::Starting => write!(f, "Starting"),
            RunStatus::Running => write!(f, "Running"),
            RunStatus::WaitingInput => write!(f, "WaitingInput"),
            RunStatus::Complete => write!(f, "Complete"),
            RunStatus::CompleteInteractive => write!(f, "CompleteInteractive"),
            RunStatus::Error => write!(f, "Error"),
            RunStatus::Cancelled => write!(f, "Cancelled"),
        }
    }
}

/// Metadata for a single background agent run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunMeta {
    pub run_id: String,
    pub agent_name: String,
    /// Absolute path to the agent manifest directory
    pub agent_path: String,
    pub task: String,
    pub model: Option<String>,
    /// PID of the worker process
    pub pid: u32,
    pub status: RunStatus,
    pub current_stage: String,
    pub stage_index: usize,
    pub num_stages: usize,
    pub iteration: usize,
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    /// Cumulative tokens read from provider cache.
    #[serde(default)]
    pub cached_tokens: usize,
    /// Cumulative tokens written to provider cache.
    #[serde(default)]
    pub cache_write_tokens: usize,
    /// Total number of tool calls made across all iterations.
    #[serde(default)]
    pub tool_calls: usize,
    /// Absolute path to the working directory for tool execution
    pub workdir: String,
    /// Unix timestamp (seconds)
    pub started_at: i64,
    /// Unix timestamp (seconds)
    pub updated_at: i64,
    pub error: Option<String>,
    /// Short human-readable title generated from the task prompt (None until generated).
    #[serde(default)]
    pub title: Option<String>,
    /// Custom key-value pairs from the spawn request (API metadata).
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    /// Webhook URL to POST on agent completion/error.
    #[serde(default)]
    pub callback_url: Option<String>,
    /// Optional shared secret used to HMAC-SHA256 sign the webhook body
    /// (`X-Leviath-Signature` header) so the receiver can verify authenticity.
    ///
    /// Persisted, because the daemon must still be able to sign a webhook for a
    /// run it reloaded after a restart. **Never serve it** — strip it with
    /// [`RunMeta::redacted`] before any of this struct leaves the process. See
    /// that method for what went wrong.
    #[serde(default)]
    pub callback_secret: Option<String>,
    /// Links sub-agent runs to their parent run.
    #[serde(default)]
    pub parent_run_id: Option<String>,
    /// Run-ids of this agent's direct sub-agents (sub-agent-tool spawns and
    /// fan-out workers). Persisted so the daemon can rebuild the exact
    /// parent→children tree on restart rather than reload children as orphans.
    #[serde(default)]
    pub children: Vec<String>,
    /// This agent's depth in the sub-agent tree (0 for a top-level run).
    /// Persisted so a reloaded child enforces its remaining spawn-depth budget.
    #[serde(default)]
    pub depth: usize,
    /// The sub-agent depth cap this agent imposes on its own children
    /// (0 when it has none). Restores `SubAgentChildren::max_child_depth`.
    #[serde(default)]
    pub max_child_depth: usize,
    /// Why this run may have produced nothing useful — see [`RunFlags`].
    #[serde(default)]
    pub flags: RunFlags,
}

/// Post-hoc diagnosis of a run's productivity, persisted in `meta.json` so a
/// harness (or the dashboard) can tell an empty run from a successful one
/// without inspecting the workspace or parsing logs.
///
/// Issue #107: 13/300 SWE-bench runs completed their whole stage pipeline and
/// produced no file changes at all. Nothing on disk said so, or said why.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RunFlags {
    /// Paths passed to file-modifying tools that succeeded, in first-touch
    /// order. Capped at [`MAX_TRACKED_MODIFIED_FILES`]; `modified_file_count`
    /// keeps the true total.
    #[serde(default)]
    pub modified_files: Vec<String>,
    /// Total successful file-modifying tool calls across the run (uncapped).
    #[serde(default)]
    pub modified_file_count: usize,
    /// The run reached a terminal status having modified nothing.
    #[serde(default)]
    pub empty_output: bool,
    /// How many stages exhausted their `max_iterations`.
    #[serde(default)]
    pub max_iterations_hit: usize,
    /// How many transitions proceeded past an unsatisfied gate because the
    /// gate's re-run budget ran out.
    #[serde(default)]
    pub gates_forced: usize,
    /// The working directory disappeared mid-run.
    #[serde(default)]
    pub workspace_lost: bool,
}

/// How many distinct modified paths [`RunFlags`] records before it stops
/// growing (the count keeps rising). Bounds `meta.json` for a long run.
pub const MAX_TRACKED_MODIFIED_FILES: usize = 200;

impl RunFlags {
    /// Record a successful modifying tool call on `path`.
    pub fn record_modification(&mut self, path: &str) {
        self.modified_file_count += 1;
        if self.modified_files.len() < MAX_TRACKED_MODIFIED_FILES
            && !self.modified_files.iter().any(|p| p == path)
        {
            self.modified_files.push(path.to_string());
        }
    }
}

impl RunMeta {
    /// This run's metadata with the webhook signing secret removed, for anything
    /// that leaves the process.
    ///
    /// `GET /api/agents`, `/api/agents/{id}` and `/api/agents/{id}/children` all
    /// serialized `RunMeta` whole, so any holder of the API token could read
    /// every run's `callback_secret` — the key that authenticates Leviath's
    /// webhooks to their receivers. Mirrors the `RedactedConfig` pattern the
    /// `/api/config` handler already uses correctly.
    ///
    /// Returns an owned copy rather than mutating in place so a caller cannot
    /// accidentally redact the record the daemon still needs for signing.
    #[must_use]
    pub fn redacted(&self) -> Self {
        Self {
            callback_secret: None,
            ..self.clone()
        }
    }

    pub fn new(
        run_id: String,
        agent_name: String,
        agent_path: String,
        task: String,
        model: Option<String>,
        workdir: String,
        num_stages: usize,
    ) -> Self {
        let now = now_secs();
        Self {
            run_id,
            agent_name,
            agent_path,
            task,
            model,
            pid: 0,
            status: RunStatus::Starting,
            current_stage: String::new(),
            stage_index: 0,
            num_stages,
            iteration: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            tool_calls: 0,
            workdir,
            started_at: now,
            updated_at: now,
            error: None,
            title: None,
            metadata: HashMap::new(),
            callback_url: None,
            callback_secret: None,
            parent_run_id: None,
            children: Vec::new(),
            depth: 0,
            max_child_depth: 0,
            flags: RunFlags::default(),
        }
    }

    pub fn touch(&mut self) {
        self.updated_at = now_secs();
    }
}

/// One content entry within a region, captured at snapshot time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegionEntrySnapshot {
    pub content: String,
    pub tokens: usize,
    /// The entry's role/kind, so a snapshot round-trips faithfully when the
    /// daemon reloads it on restart. Defaults to `Text` for older snapshots.
    #[serde(default)]
    pub kind: crate::region::EntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Key for HashMap region entries (file paths, section names, etc.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// Per-region token snapshot written by the background worker after each inference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegionSnapshot {
    pub name: String,
    /// Stringified kind: "pinned", "temporary", "clearable", "sliding", "compacting", "history"
    pub kind: String,
    pub current_tokens: usize,
    pub max_tokens: usize,
    /// Actual content entries stored in this region (empty for zero-token regions).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<RegionEntrySnapshot>,
}

/// Snapshot of the full context window, written to `context.json` alongside `meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextSnapshot {
    pub stage_name: String,
    pub total_tokens: usize,
    pub max_tokens: usize,
    pub regions: Vec<RegionSnapshot>,
}

/// Status of an individual stage within a run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StageRunStatus {
    Pending,
    Active,
    WaitingInput,
    Complete,
    Error,
}

impl std::fmt::Display for StageRunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StageRunStatus::Pending => write!(f, "Pending"),
            StageRunStatus::Active => write!(f, "Active"),
            StageRunStatus::WaitingInput => write!(f, "WaitingInput"),
            StageRunStatus::Complete => write!(f, "Complete"),
            StageRunStatus::Error => write!(f, "Error"),
        }
    }
}

/// Metadata record for a single stage within a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageRecord {
    pub name: String,
    pub index: usize,
    pub status: StageRunStatus,
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    /// Tokens read from provider cache in this stage.
    #[serde(default)]
    pub cached_tokens: usize,
    /// Unix timestamp (seconds); None until the stage starts.
    pub started_at: Option<i64>,
    /// Unix timestamp (seconds); None until the stage ends.
    pub ended_at: Option<i64>,
}

impl StageRecord {
    pub fn new(name: String, index: usize) -> Self {
        Self {
            name,
            index,
            status: StageRunStatus::Pending,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            started_at: None,
            ended_at: None,
        }
    }
}

/// Current Unix time in seconds (saturating to 0 before the epoch).
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn sample_meta() -> RunMeta {
        RunMeta::new(
            "run-1".to_string(),
            "agent".to_string(),
            "/agents/agent".to_string(),
            "do the thing".to_string(),
            Some("claude-sonnet-4-6".to_string()),
            "/work".to_string(),
            3,
        )
    }

    /// The webhook signing secret must not survive into anything served over
    /// the API — `GET /api/agents` used to hand it to any token holder.
    #[test]
    fn redacted_drops_the_callback_secret_and_keeps_everything_else() {
        let mut m = sample_meta();
        m.callback_secret = Some("shhh".to_string());
        m.callback_url = Some("https://example.com/hook".to_string());

        let r = m.redacted();
        assert_eq!(r.callback_secret, None);
        // The URL is not a secret and stays: a caller needs to see where its own
        // webhook was pointed.
        assert_eq!(r.callback_url.as_deref(), Some("https://example.com/hook"));
        assert_eq!(r.run_id, m.run_id);
        assert_eq!(r.task, m.task);

        // Serializing the redacted form must not mention it at all — a `None`
        // that still emitted `"callback_secret": null` would be fine, but an
        // assertion on the wire format is what a reviewer actually checks.
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("shhh"), "{json}");

        // ...and the original is untouched, because the daemon still needs it to
        // sign the webhook for a run it reloaded after a restart.
        assert_eq!(m.callback_secret.as_deref(), Some("shhh"));
    }

    #[test]
    fn run_meta_new_sets_defaults() {
        let m = sample_meta();
        assert_eq!(m.run_id, "run-1");
        assert_eq!(m.agent_name, "agent");
        assert_eq!(m.agent_path, "/agents/agent");
        assert_eq!(m.task, "do the thing");
        assert_eq!(m.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(m.workdir, "/work");
        assert_eq!(m.num_stages, 3);
        assert_eq!(m.pid, 0);
        assert_eq!(m.status, RunStatus::Starting);
        assert_eq!(m.stage_index, 0);
        assert_eq!(m.iteration, 0);
        assert_eq!(m.prompt_tokens, 0);
        assert_eq!(m.completion_tokens, 0);
        assert_eq!(m.cached_tokens, 0);
        assert_eq!(m.cache_write_tokens, 0);
        assert_eq!(m.tool_calls, 0);
        assert!(m.error.is_none());
        assert!(m.title.is_none());
        assert!(m.metadata.is_empty());
        assert!(m.callback_url.is_none());
        assert!(m.callback_secret.is_none());
        assert!(m.parent_run_id.is_none());
        assert!(m.children.is_empty());
        assert_eq!(m.depth, 0);
        assert_eq!(m.max_child_depth, 0);
        assert!(m.current_stage.is_empty());
        assert_eq!(m.started_at, m.updated_at);
    }

    #[test]
    fn run_meta_touch_advances_updated_at() {
        let mut m = sample_meta();
        m.updated_at = 0;
        m.touch();
        assert!(m.updated_at > 0);
    }

    #[test]
    fn run_meta_serde_roundtrip() {
        let mut m = sample_meta();
        m.status = RunStatus::Running;
        m.metadata.insert("k".to_string(), "v".to_string());
        m.title = Some("A title".to_string());
        m.callback_secret = Some("shh".to_string());
        m.parent_run_id = Some("parent-1".to_string());
        m.children = vec!["child-a".to_string(), "child-b".to_string()];
        m.depth = 2;
        m.max_child_depth = 5;
        let json = serde_json::to_string(&m).unwrap();
        let back: RunMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.run_id, m.run_id);
        assert_eq!(back.status, RunStatus::Running);
        assert_eq!(back.metadata.get("k").map(String::as_str), Some("v"));
        assert_eq!(back.title.as_deref(), Some("A title"));
        assert_eq!(back.callback_secret.as_deref(), Some("shh"));
        assert_eq!(back.parent_run_id.as_deref(), Some("parent-1"));
        assert_eq!(
            back.children,
            vec!["child-a".to_string(), "child-b".to_string()]
        );
        assert_eq!(back.depth, 2);
        assert_eq!(back.max_child_depth, 5);
    }

    #[test]
    fn run_status_display_all_variants() {
        assert_eq!(RunStatus::Starting.to_string(), "Starting");
        assert_eq!(RunStatus::Running.to_string(), "Running");
        assert_eq!(RunStatus::WaitingInput.to_string(), "WaitingInput");
        assert_eq!(RunStatus::Complete.to_string(), "Complete");
        assert_eq!(
            RunStatus::CompleteInteractive.to_string(),
            "CompleteInteractive"
        );
        assert_eq!(RunStatus::Error.to_string(), "Error");
        assert_eq!(RunStatus::Cancelled.to_string(), "Cancelled");
    }

    #[test]
    fn run_status_serde_snake_case_roundtrip() {
        for s in [
            RunStatus::Starting,
            RunStatus::Running,
            RunStatus::WaitingInput,
            RunStatus::Complete,
            RunStatus::CompleteInteractive,
            RunStatus::Error,
            RunStatus::Cancelled,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let back: RunStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, s);
        }
        assert_eq!(
            serde_json::to_string(&RunStatus::WaitingInput).unwrap(),
            "\"waiting_input\""
        );
    }

    #[test]
    fn context_snapshot_serde_roundtrip() {
        let snap = ContextSnapshot {
            stage_name: "plan".to_string(),
            total_tokens: 42,
            max_tokens: 100,
            regions: vec![RegionSnapshot {
                name: "history".to_string(),
                kind: "sliding".to_string(),
                current_tokens: 10,
                max_tokens: 50,
                entries: vec![RegionEntrySnapshot {
                    content: "hi".to_string(),
                    tokens: 1,
                    kind: crate::region::EntryKind::UserMessage,
                    metadata: Some(serde_json::json!({"a": 1})),
                    key: Some("k".to_string()),
                }],
            }],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ContextSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.stage_name, "plan");
        assert_eq!(back.regions.len(), 1);
        assert_eq!(back.regions[0].entries.len(), 1);
        assert_eq!(back.regions[0].entries[0].content, "hi");
        assert_eq!(back.regions[0].entries[0].key.as_deref(), Some("k"));
    }

    #[test]
    fn region_snapshot_skips_empty_entries_in_json() {
        let snap = RegionSnapshot {
            name: "r".to_string(),
            kind: "pinned".to_string(),
            current_tokens: 0,
            max_tokens: 0,
            entries: vec![],
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("entries"));
    }

    #[test]
    fn stage_run_status_display_all_variants() {
        assert_eq!(StageRunStatus::Pending.to_string(), "Pending");
        assert_eq!(StageRunStatus::Active.to_string(), "Active");
        assert_eq!(StageRunStatus::WaitingInput.to_string(), "WaitingInput");
        assert_eq!(StageRunStatus::Complete.to_string(), "Complete");
        assert_eq!(StageRunStatus::Error.to_string(), "Error");
    }

    #[test]
    fn run_flags_record_modification_dedups_paths_and_caps_the_list() {
        let mut flags = RunFlags::default();
        flags.record_modification("src/a.rs");
        flags.record_modification("src/a.rs");
        flags.record_modification("src/b.rs");
        assert_eq!(flags.modified_file_count, 3);
        assert_eq!(flags.modified_files, vec!["src/a.rs", "src/b.rs"]);

        // Past the cap the count keeps rising but the list stops growing, so a
        // long run can't bloat meta.json.
        for i in 0..MAX_TRACKED_MODIFIED_FILES {
            flags.record_modification(&format!("f{i}.rs"));
        }
        assert_eq!(flags.modified_files.len(), MAX_TRACKED_MODIFIED_FILES);
        assert_eq!(flags.modified_file_count, 3 + MAX_TRACKED_MODIFIED_FILES);
    }

    #[test]
    fn run_meta_flags_default_for_older_files() {
        // meta.json written before #107 has no `flags` key at all.
        let mut meta = RunMeta::new(
            "r".to_string(),
            "a".to_string(),
            "/p".to_string(),
            "t".to_string(),
            None,
            "/w".to_string(),
            1,
        );
        meta.flags.empty_output = true;
        let json = serde_json::to_string(&meta).unwrap();
        let stripped = json.replace(r#","flags":{"modified_files":[],"modified_file_count":0,"empty_output":true,"max_iterations_hit":0,"gates_forced":0,"workspace_lost":false}"#, "");
        assert!(!stripped.contains("flags"));
        let back: RunMeta = serde_json::from_str(&stripped).unwrap();
        assert_eq!(back.flags, RunFlags::default());
    }

    #[test]
    fn stage_record_new_and_serde_roundtrip() {
        let rec = StageRecord::new("analyze".to_string(), 2);
        assert_eq!(rec.name, "analyze");
        assert_eq!(rec.index, 2);
        assert_eq!(rec.status, StageRunStatus::Pending);
        assert_eq!(rec.prompt_tokens, 0);
        assert_eq!(rec.completion_tokens, 0);
        assert_eq!(rec.cached_tokens, 0);
        assert!(rec.started_at.is_none());
        assert!(rec.ended_at.is_none());

        let json = serde_json::to_string(&rec).unwrap();
        let back: StageRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "analyze");
        assert_eq!(back.status, StageRunStatus::Pending);
    }
}

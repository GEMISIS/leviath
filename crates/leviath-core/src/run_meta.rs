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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Links sub-agent runs to their parent run.
    #[serde(default)]
    pub parent_run_id: Option<String>,
}

impl RunMeta {
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
            parent_run_id: None,
        }
    }

    pub fn touch(&mut self) {
        self.updated_at = now_secs();
    }
}

/// One content entry within a region, captured at snapshot time.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        assert!(m.parent_run_id.is_none());
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
        let json = serde_json::to_string(&m).unwrap();
        let back: RunMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.run_id, m.run_id);
        assert_eq!(back.status, RunStatus::Running);
        assert_eq!(back.metadata.get("k").map(String::as_str), Some("v"));
        assert_eq!(back.title.as_deref(), Some("A title"));
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

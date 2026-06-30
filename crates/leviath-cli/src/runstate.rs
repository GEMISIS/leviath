//! On-disk run state for background agent executions.
//!
//! Each run lives under ~/.leviath/runs/<run-id>/ with:
//! - `meta.json`    — run metadata, updated atomically (tmp + rename)
//! - `output.log`  — append-only combined worker stdout (legacy/fallback)
//! - `stages.json` — index of per-stage records
//! - `stages/<idx>/output.log` — readable agent output for that stage
//! - `stages/<idx>/logs.log`   — operational events + tool activity
//! - `stages/<idx>/context.json` — context snapshot for that stage
//!
//! The dashboard's activity log is persisted separately at:
//! - `~/.leviath/dashboard.log` — never cleared, appended across sessions

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
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

/// Atomically write a context snapshot for the run.
pub fn write_context_snapshot(run_id: &str, snap: &ContextSnapshot) -> anyhow::Result<()> {
    let path = run_dir(run_id).join("context.json");
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(snap)?;
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read the context snapshot for a run, if present.
pub fn read_context_snapshot(run_id: &str) -> Option<ContextSnapshot> {
    let path = run_dir(run_id).join("context.json");
    let json = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&json).ok()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Directory where all run state is stored.
pub fn runs_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".leviath")
        .join("runs")
}

/// Directory for a specific run.
pub fn run_dir(run_id: &str) -> PathBuf {
    runs_dir().join(run_id)
}

/// Path to the persistent dashboard activity log (~/.leviath/dashboard.log).
pub fn dashboard_log_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".leviath")
        .join("dashboard.log")
}

/// Append a timestamped line to the persistent dashboard activity log.
/// Silently ignores I/O errors — the dashboard log is best-effort.
pub fn append_dashboard_log(msg: &str) {
    use std::io::Write;
    let path = dashboard_log_path();
    // Ensure the parent directory exists (first-run case).
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(file, "{} {}", timestamp, msg);
    }
}

/// Generate a unique run ID: "<agent_name>-<timestamp>-<suffix>".
pub fn new_run_id(agent_name: &str) -> String {
    let now = now_secs();
    // 4-char pseudo-random suffix from the lower bits of a stack address
    let suffix = format!("{:04x}", (now & 0xffff) ^ (now >> 16 & 0xffff));
    let safe_name = agent_name.replace(|c: char| !c.is_alphanumeric() && c != '-', "-");
    format!("{}-{}-{}", safe_name, now, suffix)
}

/// Create the run directory and write initial metadata.
pub fn create_run(meta: &RunMeta) -> anyhow::Result<()> {
    let dir = run_dir(&meta.run_id);
    std::fs::create_dir_all(&dir)?;

    // Set restrictive permissions on the run directory (Unix only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        let _ = std::fs::set_permissions(&dir, perms);
    }

    write_meta(meta)
}

/// Atomically write run metadata (write to tmp, then rename).
pub fn write_meta(meta: &RunMeta) -> anyhow::Result<()> {
    let dir = run_dir(&meta.run_id);
    let tmp_path = dir.join("meta.json.tmp");
    let final_path = dir.join("meta.json");

    let json = serde_json::to_string_pretty(meta)?;
    std::fs::write(&tmp_path, &json)?;
    std::fs::rename(&tmp_path, &final_path)?;

    Ok(())
}

/// Read run metadata for a given run ID.
pub fn read_meta(run_id: &str) -> anyhow::Result<RunMeta> {
    let path = run_dir(run_id).join("meta.json");
    let json = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&json)?)
}

/// List all runs, sorted by started_at descending (most recent first).
/// Silently skips any runs whose metadata cannot be read.
pub fn list_runs() -> Vec<RunMeta> {
    let dir = runs_dir();
    if !dir.exists() {
        return Vec::new();
    }

    let mut runs = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let run_id = entry.file_name().to_string_lossy().to_string();
            if let Ok(meta) = read_meta(&run_id) {
                runs.push(meta);
            }
        }
    }

    runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    runs
}

/// Read the last `max_bytes` of any file on disk, returning UTF-8 text.
/// If the file is smaller than `max_bytes` the whole file is returned.
/// Partial UTF-8 at the truncation boundary is handled by skipping to the
/// first newline.  Returns an empty string on any I/O error.
pub fn tail_file(path: &std::path::Path, max_bytes: u64) -> String {
    if !path.exists() {
        return String::new();
    }

    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return String::new(),
    };

    let file_size = metadata.len();
    if file_size <= max_bytes {
        return std::fs::read_to_string(path).unwrap_or_default();
    }

    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };

    let offset = file_size - max_bytes;
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return String::new();
    }

    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);

    // Skip to the first newline so we don't emit a partial line at the start.
    if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
        String::from_utf8_lossy(&buf[nl + 1..]).to_string()
    } else {
        String::from_utf8_lossy(&buf).to_string()
    }
}

/// Read the last `max_bytes` of a run's combined output log (legacy).
#[allow(dead_code)]
pub fn tail_log(run_id: &str, max_bytes: u64) -> String {
    tail_file(&run_dir(run_id).join("output.log"), max_bytes)
}

// ─── Per-stage persistence ────────────────────────────────────────────────────

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

/// Directory for per-stage files within a run.
pub fn stage_dir(run_id: &str, stage_idx: usize) -> PathBuf {
    run_dir(run_id).join("stages").join(stage_idx.to_string())
}

/// Atomically write the stages index for a run.
pub fn write_stages_index(run_id: &str, stages: &[StageRecord]) -> anyhow::Result<()> {
    let path = run_dir(run_id).join("stages.json");
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(stages)?;
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read the stages index for a run, or return an empty vec on any error.
pub fn read_stages_index(run_id: &str) -> Vec<StageRecord> {
    let path = run_dir(run_id).join("stages.json");
    let json = match std::fs::read_to_string(&path) {
        Ok(j) => j,
        Err(_) => return Vec::new(),
    };
    serde_json::from_str(&json).unwrap_or_default()
}

/// Ensure the per-stage directory exists (called before first write).
fn ensure_stage_dir(run_id: &str, stage_idx: usize) {
    let dir = stage_dir(run_id, stage_idx);
    let _ = std::fs::create_dir_all(&dir);
}

/// Append a line of readable agent output to the per-stage output log.
pub fn append_stage_output(run_id: &str, stage_idx: usize, text: &str) {
    use std::io::Write;
    ensure_stage_dir(run_id, stage_idx);
    let path = stage_dir(run_id, stage_idx).join("output.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{}", text);
    }
}

/// Append a line of operational/tool-activity log to the per-stage logs file.
pub fn append_stage_log(run_id: &str, stage_idx: usize, text: &str) {
    use std::io::Write;
    ensure_stage_dir(run_id, stage_idx);
    let path = stage_dir(run_id, stage_idx).join("logs.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{}", text);
    }
}

/// Atomically write a context snapshot for a specific stage.
pub fn write_stage_context(
    run_id: &str,
    stage_idx: usize,
    snap: &ContextSnapshot,
) -> anyhow::Result<()> {
    ensure_stage_dir(run_id, stage_idx);
    let path = stage_dir(run_id, stage_idx).join("context.json");
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(snap)?;
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read the context snapshot for a specific stage, if present.
pub fn read_stage_context(run_id: &str, stage_idx: usize) -> Option<ContextSnapshot> {
    let path = stage_dir(run_id, stage_idx).join("context.json");
    let json = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&json).ok()
}

/// Read the last `max_bytes` of the readable output log for a specific stage.
pub fn tail_stage_output(run_id: &str, stage_idx: usize, max_bytes: u64) -> String {
    tail_file(&stage_dir(run_id, stage_idx).join("output.log"), max_bytes)
}

/// Read the last `max_bytes` of the operational log for a specific stage.
pub fn tail_stage_log(run_id: &str, stage_idx: usize, max_bytes: u64) -> String {
    tail_file(&stage_dir(run_id, stage_idx).join("logs.log"), max_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── RunStatus ──────────────────────────────────────────────────────────

    #[test]
    fn run_status_serde_roundtrip() {
        for status in [
            RunStatus::Starting,
            RunStatus::Running,
            RunStatus::WaitingInput,
            RunStatus::Complete,
            RunStatus::CompleteInteractive,
            RunStatus::Error,
            RunStatus::Cancelled,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: RunStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, back);
        }
    }

    #[test]
    fn run_status_display() {
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
    fn run_status_snake_case_serialization() {
        let json = serde_json::to_string(&RunStatus::WaitingInput).unwrap();
        assert_eq!(json, "\"waiting_input\"");
        let json = serde_json::to_string(&RunStatus::CompleteInteractive).unwrap();
        assert_eq!(json, "\"complete_interactive\"");
    }

    // ─── StageRunStatus ─────────────────────────────────────────────────────

    #[test]
    fn stage_run_status_serde_roundtrip() {
        for status in [
            StageRunStatus::Pending,
            StageRunStatus::Active,
            StageRunStatus::WaitingInput,
            StageRunStatus::Complete,
            StageRunStatus::Error,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: StageRunStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, back);
        }
    }

    #[test]
    fn stage_run_status_display() {
        assert_eq!(StageRunStatus::Pending.to_string(), "Pending");
        assert_eq!(StageRunStatus::Active.to_string(), "Active");
        assert_eq!(StageRunStatus::WaitingInput.to_string(), "WaitingInput");
        assert_eq!(StageRunStatus::Complete.to_string(), "Complete");
        assert_eq!(StageRunStatus::Error.to_string(), "Error");
    }

    // ─── RunMeta ────────────────────────────────────────────────────────────

    #[test]
    fn run_meta_new_defaults() {
        let meta = RunMeta::new(
            "run-1".into(),
            "agent".into(),
            "/path".into(),
            "do stuff".into(),
            Some("gpt-4".into()),
            "/work".into(),
            3,
        );
        assert_eq!(meta.run_id, "run-1");
        assert_eq!(meta.agent_name, "agent");
        assert_eq!(meta.task, "do stuff");
        assert_eq!(meta.model.as_deref(), Some("gpt-4"));
        assert_eq!(meta.num_stages, 3);
        assert_eq!(meta.status, RunStatus::Starting);
        assert_eq!(meta.pid, 0);
        assert_eq!(meta.stage_index, 0);
        assert!(meta.error.is_none());
        assert!(meta.title.is_none());
        assert!(meta.metadata.is_empty());
        assert!(meta.callback_url.is_none());
        assert!(meta.parent_run_id.is_none());
    }

    #[test]
    fn run_meta_serde_roundtrip() {
        let meta = RunMeta::new(
            "test-run".into(),
            "test-agent".into(),
            "/agents/test".into(),
            "run tests".into(),
            None,
            "/tmp".into(),
            2,
        );
        let json = serde_json::to_string_pretty(&meta).unwrap();
        let back: RunMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.run_id, "test-run");
        assert_eq!(back.agent_name, "test-agent");
        assert_eq!(back.num_stages, 2);
        assert!(back.model.is_none());
    }

    #[test]
    fn run_meta_touch_updates_timestamp() {
        let mut meta = RunMeta::new(
            "r".into(),
            "a".into(),
            "/p".into(),
            "t".into(),
            None,
            "/w".into(),
            1,
        );
        let before = meta.updated_at;
        // Touch should update (or at least not decrease) updated_at
        meta.touch();
        assert!(meta.updated_at >= before);
    }

    #[test]
    fn run_meta_optional_fields_deserialize() {
        // Simulate a meta.json without optional fields (e.g., from older version)
        let json = serde_json::json!({
            "run_id": "r1",
            "agent_name": "a",
            "agent_path": "/p",
            "task": "t",
            "model": null,
            "pid": 123,
            "status": "running",
            "current_stage": "init",
            "stage_index": 0,
            "num_stages": 1,
            "iteration": 0,
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "workdir": "/w",
            "started_at": 1000,
            "updated_at": 1000,
            "error": null
        });
        let meta: RunMeta = serde_json::from_value(json).unwrap();
        assert_eq!(meta.cached_tokens, 0);
        assert!(meta.title.is_none());
        assert!(meta.metadata.is_empty());
        assert!(meta.callback_url.is_none());
        assert!(meta.parent_run_id.is_none());
    }

    // ─── StageRecord ────────────────────────────────────────────────────────

    #[test]
    fn stage_record_new_defaults() {
        let rec = StageRecord::new("analyze".into(), 2);
        assert_eq!(rec.name, "analyze");
        assert_eq!(rec.index, 2);
        assert_eq!(rec.status, StageRunStatus::Pending);
        assert_eq!(rec.prompt_tokens, 0);
        assert_eq!(rec.completion_tokens, 0);
        assert_eq!(rec.cached_tokens, 0);
        assert!(rec.started_at.is_none());
        assert!(rec.ended_at.is_none());
    }

    #[test]
    fn stage_record_serde_roundtrip() {
        let mut rec = StageRecord::new("build".into(), 0);
        rec.status = StageRunStatus::Complete;
        rec.prompt_tokens = 100;
        rec.started_at = Some(1000);
        rec.ended_at = Some(2000);

        let json = serde_json::to_string(&rec).unwrap();
        let back: StageRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "build");
        assert_eq!(back.status, StageRunStatus::Complete);
        assert_eq!(back.prompt_tokens, 100);
        assert_eq!(back.started_at, Some(1000));
    }

    // ─── RegionSnapshot / ContextSnapshot ───────────────────────────────────

    #[test]
    fn region_snapshot_serde_roundtrip() {
        let snap = RegionSnapshot {
            name: "system".into(),
            kind: "pinned".into(),
            current_tokens: 100,
            max_tokens: 500,
            entries: vec![RegionEntrySnapshot {
                content: "You are helpful".into(),
                tokens: 3,
                metadata: None,
            }],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: RegionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "system");
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].content, "You are helpful");
    }

    #[test]
    fn region_snapshot_empty_entries_omitted() {
        let snap = RegionSnapshot {
            name: "empty".into(),
            kind: "temporary".into(),
            current_tokens: 0,
            max_tokens: 100,
            entries: vec![],
        };
        let json = serde_json::to_value(&snap).unwrap();
        assert!(json.get("entries").is_none());
    }

    #[test]
    fn context_snapshot_serde_roundtrip() {
        let snap = ContextSnapshot {
            stage_name: "analyze".into(),
            total_tokens: 500,
            max_tokens: 8192,
            regions: vec![RegionSnapshot {
                name: "history".into(),
                kind: "sliding".into(),
                current_tokens: 300,
                max_tokens: 2000,
                entries: vec![],
            }],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ContextSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.stage_name, "analyze");
        assert_eq!(back.total_tokens, 500);
        assert_eq!(back.regions.len(), 1);
    }

    // ─── tail_file ──────────────────────────────────────────────────────────

    #[test]
    fn tail_file_nonexistent_returns_empty() {
        let path = std::path::Path::new("/tmp/nonexistent-leviath-test-file.txt");
        assert_eq!(tail_file(path, 1024), "");
    }

    #[test]
    fn tail_file_small_file_returns_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.txt");
        std::fs::write(&path, "line1\nline2\nline3\n").unwrap();
        let result = tail_file(&path, 1024);
        assert_eq!(result, "line1\nline2\nline3\n");
    }

    #[test]
    fn tail_file_large_file_returns_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.txt");
        let content = "abcdefghij\n".repeat(100); // 1100 bytes
        std::fs::write(&path, &content).unwrap();
        let result = tail_file(&path, 50);
        // Should be less than 50 bytes, starting from a line boundary
        assert!(result.len() <= 50);
        assert!(result.ends_with('\n'));
    }

    // ─── new_run_id ─────────────────────────────────────────────────────────

    #[test]
    fn new_run_id_contains_agent_name() {
        let id = new_run_id("my-agent");
        assert!(id.starts_with("my-agent-"));
    }

    #[test]
    fn new_run_id_sanitizes_special_chars() {
        let id = new_run_id("agent with spaces!");
        assert!(!id.contains(' '));
        assert!(!id.contains('!'));
    }

    // ─── write_meta / read_meta roundtrip ───────────────────────────────────

    #[test]
    fn write_and_read_meta_roundtrip() {
        // Use the real runs_dir so write_meta/read_meta work
        let meta = RunMeta::new(
            "test-roundtrip-unit".into(),
            "test-agent".into(),
            "/agents/test".into(),
            "unit test".into(),
            Some("model-x".into()),
            "/tmp".into(),
            2,
        );

        create_run(&meta).unwrap();
        let back = read_meta(&meta.run_id).unwrap();
        assert_eq!(back.run_id, "test-roundtrip-unit");
        assert_eq!(back.agent_name, "test-agent");
        assert_eq!(back.task, "unit test");
        assert_eq!(back.model.as_deref(), Some("model-x"));

        // Cleanup
        let _ = std::fs::remove_dir_all(run_dir(&meta.run_id));
    }

    // ─── write_stages_index / read_stages_index roundtrip ───────────────────

    #[test]
    fn write_and_read_stages_index_roundtrip() {
        let run_id = "test-stages-idx-unit";
        let dir = run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let stages = vec![
            StageRecord::new("init".into(), 0),
            StageRecord::new("process".into(), 1),
        ];
        write_stages_index(run_id, &stages).unwrap();
        let back = read_stages_index(run_id);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].name, "init");
        assert_eq!(back[1].name, "process");

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_stages_index_missing_returns_empty() {
        let back = read_stages_index("nonexistent-run-12345");
        assert!(back.is_empty());
    }

    // ─── write/read context snapshot ────────────────────────────────────────

    #[test]
    fn write_and_read_context_snapshot_roundtrip() {
        let run_id = "test-ctx-snap-unit";
        let dir = run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let snap = ContextSnapshot {
            stage_name: "test".into(),
            total_tokens: 42,
            max_tokens: 8192,
            regions: vec![],
        };
        write_context_snapshot(run_id, &snap).unwrap();
        let back = read_context_snapshot(run_id).unwrap();
        assert_eq!(back.stage_name, "test");
        assert_eq!(back.total_tokens, 42);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_context_snapshot_missing_returns_none() {
        assert!(read_context_snapshot("nonexistent-ctx-run").is_none());
    }

    // ─── stage_dir / append_stage_output / append_stage_log ─────────────────

    #[test]
    fn stage_dir_path_structure() {
        let path = stage_dir("run-abc", 2);
        assert!(path.ends_with("stages/2"));
        assert!(path.to_str().unwrap().contains("run-abc"));
    }

    #[test]
    fn append_and_tail_stage_output() {
        let run_id = "test-stage-output-unit";
        append_stage_output(run_id, 0, "line 1");
        append_stage_output(run_id, 0, "line 2");
        let output = tail_stage_output(run_id, 0, 4096);
        assert!(output.contains("line 1"));
        assert!(output.contains("line 2"));

        let _ = std::fs::remove_dir_all(run_dir(run_id));
    }

    #[test]
    fn append_and_tail_stage_log() {
        let run_id = "test-stage-log-unit";
        append_stage_log(run_id, 0, "event A");
        append_stage_log(run_id, 0, "event B");
        let log = tail_stage_log(run_id, 0, 4096);
        assert!(log.contains("event A"));
        assert!(log.contains("event B"));

        let _ = std::fs::remove_dir_all(run_dir(run_id));
    }

    // ─── write/read stage context ───────────────────────────────────────────

    #[test]
    fn write_and_read_stage_context_roundtrip() {
        let run_id = "test-stage-ctx-unit";
        let snap = ContextSnapshot {
            stage_name: "stage-0".into(),
            total_tokens: 100,
            max_tokens: 4096,
            regions: vec![],
        };
        write_stage_context(run_id, 0, &snap).unwrap();
        let back = read_stage_context(run_id, 0).unwrap();
        assert_eq!(back.stage_name, "stage-0");

        let _ = std::fs::remove_dir_all(run_dir(run_id));
    }

    #[test]
    fn read_stage_context_missing_returns_none() {
        assert!(read_stage_context("nonexistent-run", 99).is_none());
    }
}

//! On-disk run state for background agent executions.
//!
//! Each run lives under ~/.leviath/runs/<run-id>/ with:
//! - `meta.json`: run metadata, updated atomically (tmp + rename)
//! - `output.log`: append-only log of all agent output

use serde::{Deserialize, Serialize};
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
    /// Absolute path to the working directory for tool execution
    pub workdir: String,
    /// Unix timestamp (seconds)
    pub started_at: i64,
    /// Unix timestamp (seconds)
    pub updated_at: i64,
    pub error: Option<String>,
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
            workdir,
            started_at: now,
            updated_at: now,
            error: None,
        }
    }

    pub fn touch(&mut self) {
        self.updated_at = now_secs();
    }
}

/// Per-region token snapshot written by the background worker after each inference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionSnapshot {
    pub name: String,
    /// Stringified kind: "pinned", "temporary", "clearable", "sliding", "compacting", "history"
    pub kind: String,
    pub current_tokens: usize,
    pub max_tokens: usize,
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

    runs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    runs
}

/// Append a line to the run's output log.
#[allow(dead_code)]
pub fn append_log(run_id: &str, msg: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let path = run_dir(run_id).join("output.log");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{}", msg)?;
    Ok(())
}

/// Read the last `max_bytes` of the output log (or the whole file if smaller).
pub fn tail_log(run_id: &str, max_bytes: u64) -> String {
    let path = run_dir(run_id).join("output.log");
    if !path.exists() {
        return String::new();
    }

    let metadata = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(_) => return String::new(),
    };

    let file_size = metadata.len();
    if file_size <= max_bytes {
        return std::fs::read_to_string(&path).unwrap_or_default();
    }

    // Read the last max_bytes
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };

    let offset = file_size - max_bytes;
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return String::new();
    }

    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);

    // Find first newline to avoid partial line at start
    if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
        String::from_utf8_lossy(&buf[nl + 1..]).to_string()
    } else {
        String::from_utf8_lossy(&buf).to_string()
    }
}

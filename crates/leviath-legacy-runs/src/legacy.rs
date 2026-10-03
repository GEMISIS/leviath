//! Reading every file of a run directory in the old layout.

use std::path::{Path, PathBuf};

use leviath_core::files::{BLOBS_DIR, RUN_FILE};
use leviath_core::run_meta::{ContextSnapshot, RunMeta, RunStatus, StageRecord};
use leviath_runtime::runfile::codec::MAGIC;
use leviath_runtime::spec::names::Digest;
use serde::Deserialize;

use crate::journal::{self, Folded, JournalRecord, RunIdentity};
use crate::report::BlueprintSource;
use crate::{ConvertEnv, ConvertError};

/// An old blueprint manifest inside its agent directory.
const MANIFEST_FILENAME: &str = "agent.leviath";

/// The run's metadata: status, timings, totals.
pub(crate) const META_FILE: &str = "meta.json";

/// The run's latest context-window snapshot, for a run with no journal.
const CONTEXT_FILE: &str = "context.json";

/// The per-stage ledger.
const STAGES_FILE: &str = "stages.json";

/// A fan-out parent's waiting state.
const FANOUT_FILE: &str = "fanout.json";

/// The interaction point a paused run waited on.
const INTERACTIONS_FILE: &str = "interactions.json";

/// The question a Leviath 0.1.0 worker was waiting on a person to answer.
const QUESTION_FILE: &str = "pending.json";

/// The copy of the blueprint the run executed.
pub(crate) const BLUEPRINT_SNAPSHOT_FILE: &str = "blueprint.leviath";

/// The journal, at the name the run file has now: the two are told apart by
/// their first bytes.
const ARCHIVE_FILE: &str = RUN_FILE;

pub(crate) fn is_legacy(run_dir: &Path) -> bool {
    run_dir.join(META_FILE).is_file() && !is_run_file(&run_dir.join(ARCHIVE_FILE))
}

/// Whether the file at `path` starts as a run file does, read no further
/// than that.
pub(crate) fn is_run_file(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; MAGIC.len()];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok_and(|()| head == *MAGIC)
}

/// The metadata an old run directory holds, when it holds any that reads.
pub(crate) fn meta(run_dir: &Path) -> Option<RunMeta> {
    json_file(&run_dir.join(META_FILE)).ok().flatten()
}

/// A fan-out parent's waiting state, as `fanout.json` holds it.
#[derive(Debug, Deserialize)]
pub(crate) struct FanOutFile {
    pub(crate) max_workers: usize,
    #[serde(default)]
    pub(crate) pending: Vec<WorkItemFile>,
    #[serde(default)]
    pub(crate) active: Vec<(String, String)>,
    #[serde(default)]
    pub(crate) summaries: Vec<(String, String)>,
    #[serde(default)]
    pub(crate) failures: Vec<(String, String)>,
    #[serde(default)]
    pub(crate) paused: bool,
    #[serde(default)]
    pub(crate) parts: Vec<serde_json::Value>,
    #[serde(default)]
    pub(crate) origin: serde_json::Value,
}

/// One queued work item.
#[derive(Debug, Deserialize)]
pub(crate) struct WorkItemFile {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) context: serde_json::Value,
}

/// An open interaction point, as `interactions.json` holds it.
#[derive(Debug, Deserialize)]
pub(crate) struct PointFile {
    pub(crate) cursor: usize,
    pub(crate) round: usize,
    pub(crate) body: String,
}

/// The blueprint the run ran.
#[derive(Debug)]
pub(crate) struct BlueprintFile {
    pub(crate) text: String,
    pub(crate) source: BlueprintSource,
    /// The directory its scripts are read from, when one exists.
    pub(crate) script_dir: Option<PathBuf>,
    /// The bytes of the `agent.toml` the installed agent was migrated to,
    /// when there is one: what a fan-out worker of the run is resolved from.
    pub(crate) migrated: Option<Vec<u8>>,
}

/// Everything an old run directory holds.
#[derive(Debug)]
pub(crate) struct LegacyRun {
    pub(crate) dir: PathBuf,
    /// The metadata the journal started with.
    pub(crate) header: RunMeta,
    pub(crate) records: Vec<JournalRecord>,
    pub(crate) folded: Folded,
    pub(crate) stages: Vec<StageRecord>,
    pub(crate) fanout: Option<FanOutFile>,
    pub(crate) point: Option<PointFile>,
    /// The question a Leviath 0.1.0 worker left in `pending.json`, as the
    /// run's log names it, when there is one.
    pub(crate) question: Option<String>,
    pub(crate) final_output: Option<String>,
    pub(crate) blueprint: BlueprintFile,
    /// Whether the run's record says it was not empty where a listing now
    /// would call it so: every earlier release showed what the record said.
    pub(crate) not_empty: bool,
    /// The run as every earlier release listed it: its `meta.json` (see
    /// [`listed`]).
    pub(crate) listed: RunMeta,
    /// The stored parts under `blobs/`, by digest, with their sizes. They
    /// stay where they are, and the run file names them.
    pub(crate) blobs: Vec<(Digest, u64)>,
    /// Files under `blobs/` whose names are not a digest, which the run file
    /// does not name.
    pub(crate) stray_blobs: Vec<String>,
}

impl LegacyRun {
    /// The run's metadata, as every earlier release listed it.
    pub(crate) fn meta(&self) -> &RunMeta {
        &self.listed
    }

    /// The context as the journal first recorded it, which is what the run
    /// was seeded with.
    pub(crate) fn first_context(&self) -> Option<&ContextSnapshot> {
        self.records.iter().find_map(|r| match r {
            JournalRecord::ContextCheckpoint { snapshot, .. } => Some(snapshot),
            JournalRecord::Checkpoint { context, .. } => Some(context),
            _ => None,
        })
    }

    pub(crate) fn read(dir: &Path, env: &ConvertEnv<'_>) -> Result<Self, ConvertError> {
        let journal_path = dir.join(ARCHIVE_FILE);
        let journal = std::fs::read(&journal_path).ok();
        if journal.as_deref().is_some_and(|j| j.starts_with(MAGIC)) {
            return Err(ConvertError::AlreadyConverted { path: journal_path });
        }
        let meta_path = dir.join(META_FILE);
        let meta: RunMeta = json_file(&meta_path)?.ok_or_else(|| ConvertError::NotARun {
            path: dir.to_path_buf(),
            why: format!("it has no {META_FILE}"),
        })?;
        let not_empty = said_not_empty(&meta);
        let mut records = match journal {
            Some(bytes) => journal_records(&journal_path, &bytes)?,
            None => records_without_journal(dir, meta.clone())?,
        };
        if not_empty {
            kept_not_empty(&mut records);
        }
        let Some(JournalRecord::Header { meta: header, .. }) = records.first() else {
            return Err(ConvertError::Unreadable {
                path: journal_path,
                why: "the journal does not start with its header".into(),
            });
        };
        let header = (**header).clone();
        let folded = journal::fold(&records).expect("a journal that starts with its header folds");
        let (blobs, stray_blobs) = blobs(&dir.join(BLOBS_DIR));
        Ok(Self {
            stages: json_file(&dir.join(STAGES_FILE))?.unwrap_or_default(),
            fanout: json_file(&dir.join(FANOUT_FILE))?,
            point: json_file(&dir.join(INTERACTIONS_FILE))?,
            question: question(&dir.join(QUESTION_FILE)),
            final_output: std::fs::read_to_string(dir.join(leviath_core::FINAL_OUTPUT_FILE)).ok(),
            blueprint: blueprint(dir, &meta, env),
            not_empty,
            listed: listed(meta, &folded.meta, not_empty),
            dir: dir.to_path_buf(),
            header,
            records,
            folded,
            blobs,
            stray_blobs,
        })
    }
}

/// The run as every earlier release listed it: what `meta.json` says, which
/// was written after the journal's last record. A run that was not finished
/// was brought back at start from its journal, which a crash can leave a
/// step ahead of `meta.json`, so its stage, iteration and totals are the
/// journal's. A run whose record says it was not empty keeps saying so.
fn listed(mut meta: RunMeta, journal: &RunMeta, not_empty: bool) -> RunMeta {
    meta.flags.no_output_tools |= not_empty;
    if matches!(
        meta.status,
        RunStatus::Complete | RunStatus::Error | RunStatus::Cancelled
    ) {
        return meta;
    }
    meta.current_stage.clone_from(&journal.current_stage);
    meta.stage_index = journal.stage_index;
    meta.iteration = journal.iteration;
    meta.prompt_tokens = journal.prompt_tokens;
    meta.completion_tokens = journal.completion_tokens;
    meta.cached_tokens = journal.cached_tokens;
    meta.cache_write_tokens = journal.cache_write_tokens;
    meta.tool_calls = journal.tool_calls;
    meta.cost_usd = journal.cost_usd;
    meta.cost_priced_usd = journal.cost_priced_usd;
    meta.cost_is_exact = journal.cost_is_exact;
    meta.unpriced_calls = journal.unpriced_calls;
    meta
}

/// The question in a Leviath 0.1.0 `pending.json` at `path`, as a log names
/// it: its prompt, or the file itself when it does not read.
fn question(path: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct Question {
        prompt: String,
    }
    let text = std::fs::read_to_string(path).ok()?;
    Some(match serde_json::from_str::<Question>(&text) {
        Ok(q) => format!("the question {:?}", q.prompt),
        Err(_) => format!("a question in {QUESTION_FILE} that does not read"),
    })
}

/// Whether `meta` says its run was not empty where a listing now would call
/// it so: it stopped having changed no file and handed back no answer, and
/// still says otherwise. A record from before runs kept their flags says so
/// too, having none.
fn said_not_empty(meta: &RunMeta) -> bool {
    let f = &meta.flags;
    let stopped = matches!(
        meta.status,
        RunStatus::Complete
            | RunStatus::CompleteInteractive
            | RunStatus::Error
            | RunStatus::Cancelled
    );
    stopped
        && !f.empty_output
        && f.modified_file_count == 0
        && !f.no_output_tools
        && meta.final_output.is_none()
}

/// Mark every record of a run whose record says it was not empty as one
/// that could not have changed a file, so it is not called empty for
/// changing none: what it can change was not always recorded, and every
/// earlier release went by what the record said.
fn kept_not_empty(records: &mut [JournalRecord]) {
    for r in records {
        if let JournalRecord::Header { meta, .. }
        | JournalRecord::Progress { meta, .. }
        | JournalRecord::Checkpoint { meta, .. } = r
        {
            meta.flags.no_output_tools = true;
        }
    }
}

/// Parse a JSON file, or `None` when there is no such file.
fn json_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, ConvertError> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| ConvertError::Unreadable {
            path: path.to_path_buf(),
            why: e.to_string(),
        })
}

fn journal_records(path: &Path, bytes: &[u8]) -> Result<Vec<JournalRecord>, ConvertError> {
    let unreadable = |why: String| ConvertError::Unreadable {
        path: path.to_path_buf(),
        why,
    };
    if !bytes.starts_with(journal::MAGIC) {
        return Err(unreadable(
            "it is neither an LVR1 journal nor a run file".into(),
        ));
    }
    journal::read(bytes).map_err(unreadable)
}

/// A run from before the journal: its header is `meta.json` and its one
/// context checkpoint is `context.json`.
fn records_without_journal(dir: &Path, meta: RunMeta) -> Result<Vec<JournalRecord>, ConvertError> {
    let at = meta.updated_at;
    let identity = RunIdentity {
        run_id: meta.run_id.clone(),
        machine_id: String::new(),
        world_id: String::new(),
        created_at: meta.started_at,
    };
    let mut records = vec![JournalRecord::Header {
        identity,
        meta: Box::new(meta),
    }];
    let context: Option<ContextSnapshot> = json_file(&dir.join(CONTEXT_FILE))?;
    records.extend(context.map(|snapshot| JournalRecord::ContextCheckpoint { snapshot, at }));
    Ok(records)
}

/// The run's stored parts, by digest with their sizes, and the names of any
/// file there that is not one.
type Blobs = (Vec<(Digest, u64)>, Vec<String>);

fn blobs(dir: &Path) -> Blobs {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (Vec::new(), Vec::new());
    };
    let mut found = Vec::new();
    let mut stray = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        match (Digest::new(name.as_str()), entry.metadata()) {
            (Ok(digest), Ok(meta)) if meta.is_file() => found.push((digest, meta.len())),
            _ => stray.push(name),
        }
    }
    found.sort();
    stray.sort();
    (found, stray)
}

/// The directories the run's agent may be installed in: the one its run
/// named, then the installed agents directory's.
fn installed(meta: &RunMeta, env: &ConvertEnv<'_>) -> Vec<PathBuf> {
    let from_path = PathBuf::from(&meta.agent_path);
    let from_path = match from_path.extension().is_some_and(|e| e == "leviath") {
        true => from_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default(),
        false => from_path,
    };
    let mut out = vec![from_path];
    out.extend(env.agents_dir.iter().map(|d| d.join(&meta.agent_name)));
    out
}

/// Where an installed agent directory keeps its old manifest: beside its
/// files, or under `legacy/` once the blueprint was migrated to an
/// `agent.toml`.
fn manifests_in(dir: &Path) -> [PathBuf; 2] {
    [
        dir.join(MANIFEST_FILENAME),
        dir.join(crate::write::LEGACY_DIR).join(MANIFEST_FILENAME),
    ]
}

/// The blueprint the run ran: its own copy, else the installed one, else
/// none, and the run is read from what it recorded.
fn blueprint(dir: &Path, meta: &RunMeta, env: &ConvertEnv<'_>) -> BlueprintFile {
    let dirs = installed(meta, env);
    let found = dirs.iter().find_map(|d| {
        manifests_in(d)
            .into_iter()
            .find(|p| p.is_file())
            .map(|p| (d.clone(), p))
    });
    let script_dir = found.as_ref().map(|(d, _)| d.clone());
    let migrated = dirs
        .iter()
        .find_map(|d| std::fs::read(d.join(leviath_blueprint::FILE_NAME)).ok());
    let snapshot = dir.join(BLUEPRINT_SNAPSHOT_FILE);
    if let Ok(text) = std::fs::read_to_string(&snapshot) {
        return BlueprintFile {
            text,
            source: BlueprintSource::Snapshot,
            script_dir,
            migrated,
        };
    }
    if let Some((_, path)) = &found
        && let Ok(text) = std::fs::read_to_string(path)
    {
        return BlueprintFile {
            text,
            source: BlueprintSource::Installed(path.clone()),
            script_dir,
            migrated,
        };
    }
    let mut tried = vec![snapshot];
    tried.extend(dirs.iter().flat_map(|d| manifests_in(d)));
    let why = ConvertError::NoBlueprint {
        tried: tried.clone(),
    }
    .to_string();
    BlueprintFile {
        text: String::new(),
        source: BlueprintSource::Recorded { tried, why },
        script_dir: None,
        migrated: None,
    }
}

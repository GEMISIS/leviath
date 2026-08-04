//! A portable, self-contained record of an entire agent run.
//!
//! A run archive is a single append-only file that captures everything about a
//! run - who owns it (which machine, which world/daemon instance), its metadata,
//! every inference and tool batch, inbound messages, and the evolving context
//! window - with enough fidelity that copying the file to another machine lets a
//! daemon **continue the run where it left off** (LLM non-determinism aside) or
//! replay it for debugging.
//!
//! ## Layout
//!
//! ```text
//! MAGIC ("LVR1") | version (u16 BE) | frame*
//! frame := len (u64 BE) | JSON-encoded RunRecord
//! ```
//!
//! The framing is binary and codec-agnostic (a future release can swap the JSON
//! payload for a compact binary codec without changing readers that only seek by
//! frame length). The first record is always a [`RunRecord::Header`].
//!
//! ## Portability / future migration
//!
//! [`RunIdentity`] records which machine + world/daemon instance owns a run, and
//! [`RunRecord::OwnershipChanged`] records a handoff. This is deliberately more
//! than today needs: the format is meant to eventually let a run start on one
//! machine, pause, and resume on another - including a machine declining a run
//! whose tools it lacks and waiting for a capable host. That scheduling logic
//! isn't built yet; the format simply reserves room for it (ownership handoffs
//! are first-class, the version field gates changes, and new record variants can
//! be added without disturbing the frame layout).
//!
//! ## Efficiency
//!
//! Context windows are the bulk of a run. Rather than snapshot the whole window
//! on every step, a writer emits an occasional full [`RunRecord::ContextCheckpoint`]
//! and, between checkpoints, small [`RunRecord::ContextDiff`] records describing
//! only what changed (the common case between inferences is a pure append to one
//! region). [`diff_context`]/[`apply_delta`] compute and replay those diffs, and
//! [`fold`] reconstructs the current state from the whole journal.

use std::io::{self, Read, Write};
use std::ops::ControlFlow;

use serde::{Deserialize, Serialize};

use crate::run_meta::{ContextSnapshot, RegionEntrySnapshot, RegionSnapshot, RunMeta, RunStatus};

/// File magic identifying a leviath run archive (`b"LVR1"`).
pub const RUN_ARCHIVE_MAGIC: &[u8; 4] = b"LVR1";

/// The archive format version this build writes.
pub const RUN_ARCHIVE_VERSION: u16 = 1;

/// Identity + ownership of a run.
///
/// `machine_id` + `world_id` make a run unambiguously attributable even when
/// several daemons share a filesystem and might otherwise pick the same
/// `run_id` - a daemon can read a run's owner before deciding whether to resume
/// or leave it alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunIdentity {
    /// The run's id (its directory/file name).
    pub run_id: String,
    /// Stable fingerprint of the machine that owns the run.
    pub machine_id: String,
    /// Id of the specific world/daemon instance that owns the run.
    pub world_id: String,
    /// Unix seconds when the archive was created.
    pub created_at: i64,
}

/// A conversation message as recorded in the archive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageRecord {
    /// `"user"` / `"assistant"` / `"tool"` / `"system"`.
    pub role: String,
    /// The message text.
    pub content: String,
}

/// A single tool call and (once executed) its result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// The tool-call id.
    pub id: String,
    /// The tool name.
    pub name: String,
    /// The JSON arguments, stringified.
    pub arguments: String,
    /// The result text, once the tool has run (`None` while pending).
    pub result: Option<String>,
    /// Opaque provider token that must be replayed with this call (Gemini's
    /// `thought_signature`). Carried so a restored batch can rebuild the exact
    /// assistant turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

/// The outbound request of one inference (a provider-agnostic digest - enough to
/// reproduce/debug the call without depending on `leviath-providers`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InferenceRequestRecord {
    /// The model the request targeted.
    pub model: String,
    /// System-block texts, in order.
    pub system: Vec<String>,
    /// The conversation messages sent.
    pub messages: Vec<MessageRecord>,
    /// The tool names offered to the model.
    pub tool_names: Vec<String>,
    /// The temperature used.
    pub temperature: f32,
    /// The max output tokens requested.
    pub max_tokens: usize,
}

/// The response of one inference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InferenceResponseRecord {
    /// The assistant's text.
    pub content: String,
    /// Any tool calls the model requested.
    pub tool_calls: Vec<ToolCallRecord>,
    /// Prompt tokens billed.
    pub prompt_tokens: usize,
    /// Completion tokens billed.
    pub completion_tokens: usize,
    /// Tokens read from provider cache.
    pub cached_tokens: usize,
    /// Tokens written to provider cache.
    pub cache_write_tokens: usize,
}

/// A per-region change within a [`ContextDelta`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RegionDelta {
    /// A new region, or a region whose kind/max changed or whose entries were
    /// rewritten in a non-append way - carried in full.
    Set(RegionSnapshot),
    /// Entries appended to an existing region (the common between-inference
    /// case). The region's kind/max are unchanged.
    Append {
        /// The region name.
        name: String,
        /// The entries appended after the previously-recorded ones.
        entries: Vec<RegionEntrySnapshot>,
        /// The region's new token count.
        current_tokens: usize,
    },
    /// An existing region emptied of entries.
    Clear {
        /// The region name.
        name: String,
    },
    /// A region that no longer exists.
    Remove {
        /// The region name.
        name: String,
    },
}

/// The change to a context window since the previously-recorded snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextDelta {
    /// The window's stage name at this point.
    pub stage_name: String,
    /// The window's total token count at this point.
    pub total_tokens: usize,
    /// The window's max token budget at this point.
    pub max_tokens: usize,
    /// Per-region changes.
    pub regions: Vec<RegionDelta>,
}

/// One entry in the run journal. Folding the sequence reconstructs the run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RunRecord {
    /// The run's identity + static metadata. Always the first record.
    Header {
        /// Ownership/identity.
        identity: RunIdentity,
        /// The run metadata at archive-creation time.
        meta: Box<RunMeta>,
    },
    /// Ownership handed to a different world/machine (e.g. resumed elsewhere).
    OwnershipChanged {
        /// The new owning machine.
        machine_id: String,
        /// The new owning world/daemon instance.
        world_id: String,
        /// Unix seconds.
        at: i64,
    },
    /// One inference: what went out and what came back.
    Inference {
        /// The stage the agent was in.
        stage: String,
        /// The stage-local iteration index.
        iteration: usize,
        /// The request digest.
        request: InferenceRequestRecord,
        /// The response.
        response: InferenceResponseRecord,
        /// Unix seconds.
        at: i64,
    },
    /// A batch of tool calls, written when the batch is dispatched to the tool
    /// lane - before anything runs. Calls the dispatcher already resolved inline
    /// (context tools, refusals, gate denials) carry `result: Some(..)`; lane
    /// calls start at `result: None` and are completed by matching
    /// [`RunRecord::ToolCallDone`] records as each call finishes. A batch still
    /// pending at fold time surfaces as [`FoldedRun::pending_batch`] so a
    /// crash-resume can replay executed calls instead of re-running them.
    ToolBatch {
        /// The calls (inline results pre-filled; lane calls pending).
        calls: Vec<ToolCallRecord>,
        /// Unix seconds.
        at: i64,
        /// The stage index the batch was dispatched in.
        #[serde(default)]
        stage_index: usize,
        /// The stage-local iteration that produced the batch - the batch key
        /// (one batch per iteration).
        #[serde(default)]
        iteration: usize,
        /// The assistant text of the turn that issued the calls.
        #[serde(default)]
        response: String,
    },
    /// One tool call of the pending batch finished; its result.
    ToolCallDone {
        /// The iteration of the [`RunRecord::ToolBatch`] this belongs to.
        iteration: usize,
        /// The tool-call id.
        call_id: String,
        /// The result text.
        result: String,
        /// Unix seconds.
        at: i64,
    },
    /// A full context-window snapshot that subsequent diffs rebase on.
    ContextCheckpoint {
        /// The full window snapshot.
        snapshot: ContextSnapshot,
        /// Unix seconds.
        at: i64,
    },
    /// A context-window change since the previous snapshot/diff.
    ContextDiff {
        /// The delta.
        delta: ContextDelta,
        /// Unix seconds.
        at: i64,
    },
    /// An inbound message.
    Message {
        /// The message.
        message: MessageRecord,
        /// Unix seconds.
        at: i64,
    },
    /// A run-status change.
    StatusChanged {
        /// The new status.
        status: RunStatus,
        /// Unix seconds.
        at: i64,
    },
    /// A full resumable checkpoint: the updated metadata + the full window, so a
    /// reader can continue without folding the whole journal.
    Checkpoint {
        /// The run metadata as of this checkpoint.
        meta: Box<RunMeta>,
        /// The full window snapshot as of this checkpoint.
        context: ContextSnapshot,
        /// Unix seconds.
        at: i64,
    },
    /// A step forward: the updated metadata plus a *diff* of the context window
    /// since the previous point. This is the compact per-tick record the writer
    /// emits between full checkpoints - meta is small, and the context (the bulk)
    /// is carried as a [`ContextDelta`] rather than a full snapshot.
    Progress {
        /// The run metadata as of this step.
        meta: Box<RunMeta>,
        /// The context change since the previous recorded point.
        delta: ContextDelta,
        /// Unix seconds.
        at: i64,
    },
}

// ─── context diffing ────────────────────────────────────────────────────────

/// Whether `prev` is a prefix of `next` (same entries, in order, at the front).
fn is_prefix(prev: &[RegionEntrySnapshot], next: &[RegionEntrySnapshot]) -> bool {
    prev.len() <= next.len() && next[..prev.len()] == *prev
}

/// Compute the minimal-ish [`ContextDelta`] turning `prev` into `next`. Regions
/// that only grew at the tail become a compact `Append`; everything else is
/// carried as a `Set`/`Clear`/`Remove`.
pub fn diff_context(prev: &ContextSnapshot, next: &ContextSnapshot) -> ContextDelta {
    let mut regions = Vec::new();
    for nr in &next.regions {
        match prev.regions.iter().find(|r| r.name == nr.name) {
            None => regions.push(RegionDelta::Set(nr.clone())),
            Some(pr) => {
                if pr == nr {
                    // unchanged - emit nothing
                } else if nr.entries.is_empty() && !pr.entries.is_empty() {
                    regions.push(RegionDelta::Clear {
                        name: nr.name.clone(),
                    });
                } else if pr.kind == nr.kind
                    && pr.max_tokens == nr.max_tokens
                    && is_prefix(&pr.entries, &nr.entries)
                {
                    regions.push(RegionDelta::Append {
                        name: nr.name.clone(),
                        entries: nr.entries[pr.entries.len()..].to_vec(),
                        current_tokens: nr.current_tokens,
                    });
                } else {
                    regions.push(RegionDelta::Set(nr.clone()));
                }
            }
        }
    }
    for pr in &prev.regions {
        if !next.regions.iter().any(|r| r.name == pr.name) {
            regions.push(RegionDelta::Remove {
                name: pr.name.clone(),
            });
        }
    }
    ContextDelta {
        stage_name: next.stage_name.clone(),
        total_tokens: next.total_tokens,
        max_tokens: next.max_tokens,
        regions,
    }
}

/// Apply a [`ContextDelta`] to `base` in place. Lenient: a delta referencing a
/// region that isn't present is skipped rather than erroring, so folding never
/// fails on a malformed diff.
pub fn apply_delta(base: &mut ContextSnapshot, delta: &ContextDelta) {
    base.stage_name = delta.stage_name.clone();
    base.total_tokens = delta.total_tokens;
    base.max_tokens = delta.max_tokens;
    for region_delta in &delta.regions {
        match region_delta {
            RegionDelta::Set(snapshot) => {
                match base.regions.iter_mut().find(|r| r.name == snapshot.name) {
                    Some(existing) => *existing = snapshot.clone(),
                    None => base.regions.push(snapshot.clone()),
                }
            }
            RegionDelta::Append {
                name,
                entries,
                current_tokens,
            } => {
                if let Some(region) = base.regions.iter_mut().find(|r| &r.name == name) {
                    region.entries.extend(entries.iter().cloned());
                    region.current_tokens = *current_tokens;
                }
            }
            RegionDelta::Clear { name } => {
                if let Some(region) = base.regions.iter_mut().find(|r| &r.name == name) {
                    region.entries.clear();
                    region.current_tokens = 0;
                }
            }
            RegionDelta::Remove { name } => {
                base.regions.retain(|r| &r.name != name);
            }
        }
    }
}

// ─── codec ──────────────────────────────────────────────────────────────────

/// Write the archive preamble (magic + version). Call once at file start.
pub fn write_archive_start(w: &mut dyn Write, version: u16) -> io::Result<()> {
    w.write_all(RUN_ARCHIVE_MAGIC)?;
    w.write_all(&version.to_be_bytes())?;
    Ok(())
}

/// Read + validate the archive preamble, returning the format version.
pub fn read_archive_start(r: &mut dyn Read) -> io::Result<u16> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if &magic != RUN_ARCHIVE_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a leviath run archive (bad magic)",
        ));
    }
    let mut version = [0u8; 2];
    r.read_exact(&mut version)?;
    Ok(u16::from_be_bytes(version))
}

/// Append one framed record. The frame length is a `u64` so it can never
/// overflow the prefix (a `RunRecord` always serializes to JSON).
pub fn write_record(w: &mut dyn Write, record: &RunRecord) -> io::Result<()> {
    let payload = serde_json::to_vec(record).expect("a RunRecord always serializes to JSON");
    let len = payload.len() as u64;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&payload)?;
    Ok(())
}

/// Fill `buf` from `r`, returning `false` on a clean end-of-stream (zero bytes
/// available at the call) and erroring only on a *partial* read (truncation).
fn read_exact_or_eof(r: &mut dyn Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => {
                if filled == 0 {
                    return Ok(false); // clean EOF at a record boundary
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated run-archive frame",
                ));
            }
            n => filled += n,
        }
    }
    Ok(true)
}

/// Read the next framed record, or `None` at a clean end-of-stream.
pub fn read_record(r: &mut dyn Read) -> io::Result<Option<RunRecord>> {
    let mut len_bytes = [0u8; 8];
    if !read_exact_or_eof(r, &mut len_bytes)? {
        return Ok(None);
    }
    let len = u64::from_be_bytes(len_bytes) as usize;
    let mut payload = vec![0u8; len];
    if !read_exact_or_eof(r, &mut payload)? {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated run-archive frame",
        ));
    }
    let record = serde_json::from_slice(&payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(record))
}

/// Read the whole archive: validate the preamble, then read every record.
pub fn read_archive(r: &mut dyn Read) -> io::Result<(u16, Vec<RunRecord>)> {
    let version = read_archive_start(r)?;
    let mut records = Vec::new();
    while let Some(record) = read_record(r)? {
        records.push(record);
    }
    Ok((version, records))
}

/// Read the archive tolerantly: validate the preamble strictly, then read records
/// until a clean end-of-stream **or the first unreadable frame**, returning the
/// records collected so far.
///
/// A crash while the persistence lane is appending a record can leave a partial
/// final frame (a truncated length prefix or payload). The strict [`read_archive`]
/// would reject the whole file for that torn tail - and once a fallback-resume
/// appends fresh records *past* the torn bytes, the archive would stay unreadable
/// forever. This variant instead stops at the torn tail and keeps everything valid
/// before it, so recovery can still fold the archive to its last intact point. The
/// preamble is still validated strictly, so a file that isn't a run archive at all
/// still errors rather than folding to nothing.
pub fn read_archive_lenient(r: &mut dyn Read) -> io::Result<(u16, Vec<RunRecord>)> {
    let version = read_archive_start(r)?;
    let mut records = Vec::new();
    // A torn/invalid frame ends the read early with whatever preceded it, rather
    // than propagating the error.
    while let Ok(Some(record)) = read_record(r) {
        records.push(record);
    }
    Ok((version, records))
}

// ─── fold ───────────────────────────────────────────────────────────────────

/// A tool batch that was dispatched but whose results never reached the context
/// window - what a crash-resume must replay instead of re-running. `calls` carry
/// every result recorded before the crash ([`RunRecord::ToolCallDone`] merged
/// in); a call still at `result: None` genuinely never finished.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingToolBatch {
    /// The stage index the batch was dispatched in.
    pub stage_index: usize,
    /// The stage-local iteration that produced the batch.
    pub iteration: usize,
    /// The assistant text of the turn that issued the calls.
    pub response: String,
    /// The calls, with every recorded result merged in.
    pub calls: Vec<ToolCallRecord>,
}

/// The state reconstructed from a run journal - enough to resume or inspect the
/// run at its latest recorded point.
#[derive(Debug, Clone, PartialEq)]
pub struct FoldedRun {
    /// The run's current owner/identity.
    pub identity: RunIdentity,
    /// The latest run metadata.
    pub meta: RunMeta,
    /// The reconstructed current context window.
    pub context: ContextSnapshot,
    /// The recorded inbound messages, in order.
    pub messages: Vec<MessageRecord>,
    /// Number of inferences recorded.
    pub inference_count: usize,
    /// Number of tool calls recorded.
    pub tool_call_count: usize,
    /// A dispatched tool batch whose results never made it into the context
    /// window (the run crashed mid-batch). `None` when the run has no batch in
    /// flight or the batch's turn already landed in `context`.
    pub pending_batch: Option<PendingToolBatch>,
}

/// Whether `context` already contains the assistant turn of `batch` - i.e. the
/// batch completed and `apply_tool_results` landed it before the crash, so there
/// is nothing to replay. Matched by the first call id, which is unique per batch.
pub fn context_contains_batch(context: &ContextSnapshot, batch: &PendingToolBatch) -> bool {
    let Some(first_id) = batch.calls.first().map(|c| c.id.as_str()) else {
        return false;
    };
    context.regions.iter().any(|region| {
        region.entries.iter().any(|entry| {
            matches!(
                &entry.kind,
                crate::region::EntryKind::AssistantTurn { tool_calls }
                    if tool_calls.iter().any(|tc| tc.id == first_id)
            )
        })
    })
}

/// Reconstruct a run's current state from its journal. Returns `None` if the
/// records don't start with a [`RunRecord::Header`].
pub fn fold(records: &[RunRecord]) -> Option<FoldedRun> {
    let mut iter = records.iter();
    let (identity, meta) = match iter.next() {
        Some(RunRecord::Header { identity, meta }) => (identity.clone(), (**meta).clone()),
        _ => return None,
    };
    let mut folded = FoldedRun {
        identity,
        meta,
        context: ContextSnapshot {
            stage_name: String::new(),
            total_tokens: 0,
            max_tokens: 0,
            regions: Vec::new(),
        },
        messages: Vec::new(),
        inference_count: 0,
        tool_call_count: 0,
        pending_batch: None,
    };
    for record in iter {
        match record {
            RunRecord::Header { identity, meta } => {
                folded.identity = identity.clone();
                folded.meta = (**meta).clone();
            }
            RunRecord::OwnershipChanged {
                machine_id,
                world_id,
                ..
            } => {
                folded.identity.machine_id = machine_id.clone();
                folded.identity.world_id = world_id.clone();
            }
            RunRecord::Inference { .. } => folded.inference_count += 1,
            RunRecord::ToolBatch {
                calls,
                stage_index,
                iteration,
                response,
                ..
            } => {
                folded.tool_call_count += calls.len();
                // A later batch replaces an earlier one - only the newest can
                // still be in flight.
                folded.pending_batch = Some(PendingToolBatch {
                    stage_index: *stage_index,
                    iteration: *iteration,
                    response: response.clone(),
                    calls: calls.clone(),
                });
            }
            RunRecord::ToolCallDone {
                iteration,
                call_id,
                result,
                ..
            } => {
                // Fill the matching pending call; a stale record for a replaced
                // batch (iteration mismatch) is ignored.
                if let Some(batch) = folded
                    .pending_batch
                    .as_mut()
                    .filter(|b| b.iteration == *iteration)
                    && let Some(call) = batch.calls.iter_mut().find(|c| c.id == *call_id)
                {
                    call.result = Some(result.clone());
                }
            }
            RunRecord::ContextCheckpoint { snapshot, .. } => folded.context = snapshot.clone(),
            RunRecord::ContextDiff { delta, .. } => apply_delta(&mut folded.context, delta),
            RunRecord::Message { message, .. } => folded.messages.push(message.clone()),
            RunRecord::StatusChanged { status, .. } => folded.meta.status = status.clone(),
            RunRecord::Checkpoint { meta, context, .. } => {
                folded.meta = (**meta).clone();
                folded.context = context.clone();
            }
            RunRecord::Progress { meta, delta, .. } => {
                folded.meta = (**meta).clone();
                apply_delta(&mut folded.context, delta);
            }
        }
    }
    // The batch is only pending if it was never applied. Two applied signals: a
    // later inference moved the iteration on (even if a sliding window has since
    // evicted the turn), or the batch's assistant turn is already in the folded
    // window (the Progress carrying it landed before the crash).
    if let Some(batch) = &folded.pending_batch
        && (folded.meta.iteration != batch.iteration
            || context_contains_batch(&folded.context, batch))
    {
        folded.pending_batch = None;
    }
    Some(folded)
}

/// A run's context window at one recorded point in time, with the metadata
/// (stage, iteration, status, …) in effect then. Produced by [`replay_points`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunPoint {
    /// The run metadata at this point.
    pub meta: RunMeta,
    /// The full context window at this point.
    pub context: ContextSnapshot,
    /// Unix seconds this point was recorded.
    pub at: i64,
}

/// One replayed point, lent to a [`visit_points`] visitor rather than handed
/// over. Borrowing is the whole purpose: see that function.
#[derive(Debug)]
pub struct PointRef<'a> {
    /// Position in the timeline, counting only records that produce a point.
    /// Stable for a given journal prefix, because the journal is append-only -
    /// which is what makes it usable as a pagination cursor.
    pub index: usize,
    /// Unix seconds this point was recorded.
    pub at: i64,
    /// The run metadata in effect at this point.
    pub meta: &'a RunMeta,
    /// The full context window at this point.
    pub context: &'a ContextSnapshot,
}

/// Replay a run journal, calling `visit` once per record that changes the
/// context (a checkpoint, diff, or progress step), in order. Stops early if the
/// visitor returns [`ControlFlow::Break`]. Does nothing if the records don't
/// start with a [`RunRecord::Header`].
///
/// The point of lending each point instead of collecting them: replaying a run
/// means carrying one running window and mutating it, so materializing the
/// timeline costs a **full deep copy of the context window per point** - and a
/// window holds every region's entry text. On a megabyte-scale journal that is
/// hundreds of whole-window clones, which is why anything that wants a slice of
/// the timeline, or just an answer to "does any point contain this text",
/// should come through here rather than [`replay_points`].
///
/// `&mut dyn FnMut` rather than a generic parameter, deliberately: this is
/// called from a handful of places with unrelated closure types, and one
/// monomorphization keeps both the compiled size and the coverage instantiation
/// count at one - the same reasoning `execute_with_shutdown` documents in the
/// serve module.
pub fn visit_points(records: &[RunRecord], visit: &mut dyn FnMut(PointRef<'_>) -> ControlFlow<()>) {
    let mut iter = records.iter();
    let mut meta = match iter.next() {
        Some(RunRecord::Header { meta, .. }) => (**meta).clone(),
        _ => return,
    };
    let mut context = ContextSnapshot {
        stage_name: String::new(),
        total_tokens: 0,
        max_tokens: 0,
        regions: Vec::new(),
    };
    let mut index = 0usize;
    for record in iter {
        let at = match record {
            RunRecord::Header { meta: m, .. } => {
                meta = (**m).clone();
                continue;
            }
            RunRecord::StatusChanged { status, .. } => {
                meta.status = status.clone();
                continue;
            }
            RunRecord::ContextCheckpoint { snapshot, at } => {
                context = snapshot.clone();
                *at
            }
            RunRecord::ContextDiff { delta, at } => {
                apply_delta(&mut context, delta);
                *at
            }
            RunRecord::Checkpoint {
                meta: m,
                context: c,
                at,
            } => {
                meta = (**m).clone();
                context = c.clone();
                *at
            }
            RunRecord::Progress { meta: m, delta, at } => {
                meta = (**m).clone();
                apply_delta(&mut context, delta);
                *at
            }
            // Non-context records don't add a timeline point.
            RunRecord::OwnershipChanged { .. }
            | RunRecord::Inference { .. }
            | RunRecord::ToolBatch { .. }
            | RunRecord::ToolCallDone { .. }
            | RunRecord::Message { .. } => continue,
        };
        let flow = visit(PointRef {
            index,
            at,
            meta: &meta,
            context: &context,
        });
        index += 1;
        if flow.is_break() {
            return;
        }
    }
}

/// Replay a run journal into the sequence of context-window snapshots over time,
/// one [`RunPoint`] per record that changes the context (a checkpoint, diff, or
/// progress step). This is what the context-history views (TUI/CLI/API) consume
/// to show the window "at each stage and point". Returns an empty vec if the
/// records don't start with a [`RunRecord::Header`].
///
/// Materializes every point, so it deep-copies the whole context window once per
/// point. Prefer [`visit_points`] when only part of the timeline is wanted, or
/// when the answer is a predicate rather than the points themselves.
pub fn replay_points(records: &[RunRecord]) -> Vec<RunPoint> {
    let mut points = Vec::new();
    visit_points(records, &mut |point| {
        points.push(RunPoint {
            meta: point.meta.clone(),
            context: point.context.clone(),
            at: point.at,
        });
        ControlFlow::Continue(())
    });
    points
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_meta::RunStatus;

    fn identity() -> RunIdentity {
        RunIdentity {
            run_id: "run-1".to_string(),
            machine_id: "machine-a".to_string(),
            world_id: "world-x".to_string(),
            created_at: 100,
        }
    }

    fn meta() -> RunMeta {
        RunMeta::new(
            "run-1".to_string(),
            "coder".to_string(),
            "/agents/coder".to_string(),
            "do it".to_string(),
            Some("anthropic/claude".to_string()),
            "/work".to_string(),
            2,
        )
    }

    fn entry(content: &str, tokens: usize) -> RegionEntrySnapshot {
        RegionEntrySnapshot {
            content: content.to_string(),
            tokens,
            kind: crate::region::EntryKind::Text,
            metadata: None,
            key: None,
            taint: Default::default(),
        }
    }

    fn region(name: &str, entries: Vec<RegionEntrySnapshot>) -> RegionSnapshot {
        let current = entries.iter().map(|e| e.tokens).sum();
        RegionSnapshot {
            name: name.to_string(),
            kind: "clearable".to_string(),
            current_tokens: current,
            max_tokens: 1000,
            entries,
        }
    }

    fn snapshot(stage: &str, regions: Vec<RegionSnapshot>) -> ContextSnapshot {
        let total = regions.iter().map(|r| r.current_tokens).sum();
        ContextSnapshot {
            stage_name: stage.to_string(),
            total_tokens: total,
            max_tokens: 10_000,
            regions,
        }
    }

    fn header() -> RunRecord {
        RunRecord::Header {
            identity: identity(),
            meta: Box::new(meta()),
        }
    }

    /// A stable tag per region-delta shape - asserting on this avoids the
    /// uncovered `false` arm a `matches!` leaves when the assertion passes.
    /// Every arm is exercised across the diff tests below.
    fn region_delta_kind(d: &RegionDelta) -> &'static str {
        match d {
            RegionDelta::Set(_) => "set",
            RegionDelta::Append { .. } => "append",
            RegionDelta::Clear { .. } => "clear",
            RegionDelta::Remove { .. } => "remove",
        }
    }

    // ── diff / apply round-trips ──

    /// Applying `diff(a, b)` to a clone of `a` must reproduce `b`, for every
    /// region-delta shape (new, append, clear, remove, full-replace, unchanged).
    fn assert_diff_roundtrip(a: &ContextSnapshot, b: &ContextSnapshot) {
        let delta = diff_context(a, b);
        let mut base = a.clone();
        apply_delta(&mut base, &delta);
        assert_eq!(&base, b);
    }

    #[test]
    fn diff_append_only_growth_is_compact() {
        let a = snapshot("s1", vec![region("conv", vec![entry("hi", 1)])]);
        let b = snapshot(
            "s1",
            vec![region("conv", vec![entry("hi", 1), entry("there", 2)])],
        );
        let delta = diff_context(&a, &b);
        assert_eq!(region_delta_kind(&delta.regions[0]), "append");
        assert_diff_roundtrip(&a, &b);
    }

    #[test]
    fn diff_new_region_is_set() {
        let a = snapshot("s1", vec![region("conv", vec![entry("hi", 1)])]);
        let b = snapshot(
            "s1",
            vec![
                region("conv", vec![entry("hi", 1)]),
                region("plan", vec![entry("p", 3)]),
            ],
        );
        let delta = diff_context(&a, &b);
        assert!(delta.regions.iter().any(|d| region_delta_kind(d) == "set"));
        assert_diff_roundtrip(&a, &b);
    }

    #[test]
    fn diff_cleared_region() {
        let a = snapshot("s1", vec![region("conv", vec![entry("hi", 1)])]);
        let b = snapshot("s1", vec![region("conv", vec![])]);
        let delta = diff_context(&a, &b);
        assert_eq!(region_delta_kind(&delta.regions[0]), "clear");
        assert_diff_roundtrip(&a, &b);
    }

    #[test]
    fn diff_removed_region() {
        let a = snapshot(
            "s1",
            vec![
                region("conv", vec![entry("hi", 1)]),
                region("plan", vec![entry("p", 3)]),
            ],
        );
        let b = snapshot("s1", vec![region("conv", vec![entry("hi", 1)])]);
        let delta = diff_context(&a, &b);
        assert!(
            delta
                .regions
                .iter()
                .any(|d| region_delta_kind(d) == "remove")
        );
        assert_diff_roundtrip(&a, &b);
    }

    #[test]
    fn diff_non_prefix_rewrite_is_set() {
        // Entries changed at the front (not an append) → full Set.
        let a = snapshot("s1", vec![region("conv", vec![entry("old", 1)])]);
        let b = snapshot("s1", vec![region("conv", vec![entry("new", 1)])]);
        let delta = diff_context(&a, &b);
        assert_eq!(region_delta_kind(&delta.regions[0]), "set");
        assert_diff_roundtrip(&a, &b);
    }

    #[test]
    fn diff_kind_change_is_set_not_append() {
        // Same prefix entries but the region's kind changed → Set, not Append.
        let a = snapshot("s1", vec![region("conv", vec![entry("hi", 1)])]);
        let mut grown = region("conv", vec![entry("hi", 1), entry("more", 1)]);
        grown.kind = "sliding".to_string();
        let b = snapshot("s1", vec![grown]);
        let delta = diff_context(&a, &b);
        assert_eq!(region_delta_kind(&delta.regions[0]), "set");
        assert_diff_roundtrip(&a, &b);
    }

    #[test]
    fn diff_unchanged_region_emits_nothing() {
        let a = snapshot("s1", vec![region("conv", vec![entry("hi", 1)])]);
        let b = a.clone();
        let delta = diff_context(&a, &b);
        assert!(delta.regions.is_empty());
        assert_diff_roundtrip(&a, &b);
    }

    #[test]
    fn apply_delta_skips_unknown_regions_leniently() {
        // Append/Clear targeting a region not present are no-ops (not errors).
        let mut base = snapshot("s1", vec![]);
        let delta = ContextDelta {
            stage_name: "s1".to_string(),
            total_tokens: 0,
            max_tokens: 10_000,
            regions: vec![
                RegionDelta::Append {
                    name: "ghost".to_string(),
                    entries: vec![entry("x", 1)],
                    current_tokens: 1,
                },
                RegionDelta::Clear {
                    name: "ghost".to_string(),
                },
                RegionDelta::Remove {
                    name: "ghost".to_string(),
                },
            ],
        };
        apply_delta(&mut base, &delta);
        assert!(base.regions.is_empty());
    }

    // ── codec round-trips ──

    fn all_record_kinds() -> Vec<RunRecord> {
        vec![
            header(),
            RunRecord::OwnershipChanged {
                machine_id: "machine-b".to_string(),
                world_id: "world-y".to_string(),
                at: 101,
            },
            RunRecord::Inference {
                stage: "plan".to_string(),
                iteration: 0,
                request: InferenceRequestRecord {
                    model: "m".to_string(),
                    system: vec!["sys".to_string()],
                    messages: vec![MessageRecord {
                        role: "user".to_string(),
                        content: "hi".to_string(),
                    }],
                    tool_names: vec!["read_file".to_string()],
                    temperature: 0.7,
                    max_tokens: 1024,
                },
                response: InferenceResponseRecord {
                    content: "ok".to_string(),
                    tool_calls: vec![],
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                at: 102,
            },
            RunRecord::ToolBatch {
                calls: vec![ToolCallRecord {
                    id: "c1".to_string(),
                    name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                    result: Some("body".to_string()),
                    thought_signature: Some("sig".to_string()),
                }],
                at: 103,
                stage_index: 0,
                iteration: 0,
                response: "reading".to_string(),
            },
            RunRecord::ToolCallDone {
                iteration: 0,
                call_id: "c1".to_string(),
                result: "body".to_string(),
                at: 103,
            },
            RunRecord::ContextCheckpoint {
                snapshot: snapshot("plan", vec![region("conv", vec![entry("hi", 1)])]),
                at: 104,
            },
            RunRecord::ContextDiff {
                delta: ContextDelta {
                    stage_name: "plan".to_string(),
                    total_tokens: 3,
                    max_tokens: 10_000,
                    regions: vec![RegionDelta::Append {
                        name: "conv".to_string(),
                        entries: vec![entry("more", 2)],
                        current_tokens: 3,
                    }],
                },
                at: 105,
            },
            RunRecord::Message {
                message: MessageRecord {
                    role: "user".to_string(),
                    content: "another".to_string(),
                },
                at: 106,
            },
            RunRecord::StatusChanged {
                status: RunStatus::Complete,
                at: 107,
            },
            RunRecord::Checkpoint {
                meta: Box::new(meta()),
                context: snapshot("plan", vec![region("conv", vec![entry("hi", 1)])]),
                at: 108,
            },
            RunRecord::Progress {
                meta: Box::new(meta()),
                delta: ContextDelta {
                    stage_name: "plan".to_string(),
                    total_tokens: 3,
                    max_tokens: 10_000,
                    regions: vec![RegionDelta::Append {
                        name: "conv".to_string(),
                        entries: vec![entry("step", 2)],
                        current_tokens: 3,
                    }],
                },
                at: 109,
            },
        ]
    }

    #[test]
    fn archive_write_then_read_roundtrips_every_record_kind() {
        let records = all_record_kinds();
        let mut buf = Vec::new();
        write_archive_start(&mut buf, RUN_ARCHIVE_VERSION).unwrap();
        for r in &records {
            write_record(&mut buf, r).unwrap();
        }
        let (version, read) = read_archive(&mut buf.as_slice()).unwrap();
        assert_eq!(version, RUN_ARCHIVE_VERSION);
        assert_eq!(read, records);
    }

    #[test]
    fn read_archive_start_rejects_bad_magic() {
        let mut bytes: &[u8] = b"XXXX\x00\x01";
        let err = read_archive_start(&mut bytes).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_archive_start_reports_version() {
        let mut buf = Vec::new();
        write_archive_start(&mut buf, 7).unwrap();
        assert_eq!(read_archive_start(&mut buf.as_slice()).unwrap(), 7);
    }

    #[test]
    fn read_record_returns_none_at_clean_eof() {
        let empty: &[u8] = &[];
        assert!(read_record(&mut { empty }).unwrap().is_none());
    }

    #[test]
    fn read_record_errors_on_truncated_length_prefix() {
        // Two bytes where an 8-byte length is expected → partial read → error.
        let mut bytes: &[u8] = &[0, 0];
        let err = read_record(&mut bytes).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn read_record_errors_on_truncated_payload() {
        // A frame claiming 10 bytes but only 2 present after the 8-byte length.
        let mut bytes: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 10, 1, 2];
        let err = read_record(&mut bytes).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn read_record_errors_on_empty_payload_at_boundary() {
        // A non-zero length with zero payload bytes → clean EOF at the payload
        // start is still a truncation (the frame promised bytes).
        let mut bytes: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 10];
        let err = read_record(&mut bytes).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn read_record_errors_on_invalid_json_payload() {
        // A well-framed payload that isn't a valid RunRecord.
        let mut buf = Vec::new();
        let bad = b"not json";
        buf.extend_from_slice(&(bad.len() as u64).to_be_bytes());
        buf.extend_from_slice(bad);
        let err = read_record(&mut buf.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// A reader whose `read` always errors, to exercise the read error path
    /// inside `read_exact_or_eof` (distinct from a clean EOF).
    struct FailingReader;
    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("device error"))
        }
    }

    #[test]
    fn read_record_propagates_reader_errors() {
        let err = read_record(&mut FailingReader).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn read_archive_propagates_a_bad_preamble() {
        // Too short to even hold the magic → the preamble read errors.
        let mut bytes: &[u8] = b"LV";
        assert!(read_archive(&mut bytes).is_err());
    }

    #[test]
    fn read_archive_propagates_a_bad_frame() {
        // Valid preamble, then a truncated frame → the record read errors.
        let mut buf = Vec::new();
        write_archive_start(&mut buf, RUN_ARCHIVE_VERSION).unwrap();
        buf.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 5, 1, 2]); // len 5, 2 present
        let err = read_archive(&mut buf.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// A writer that fails after `ok_bytes` bytes, to exercise write error paths.
    struct FailAfter {
        remaining: usize,
    }
    impl Write for FailAfter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::other("disk full"));
            }
            let n = buf.len().min(self.remaining);
            self.remaining -= n;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn fail_after_writer_flush_is_a_noop() {
        assert!(FailAfter { remaining: 1 }.flush().is_ok());
    }

    #[test]
    fn write_archive_start_propagates_write_errors() {
        // Fail on the magic write (0 bytes allowed) and on the version write.
        assert!(write_archive_start(&mut FailAfter { remaining: 0 }, 1).is_err());
        assert!(write_archive_start(&mut FailAfter { remaining: 4 }, 1).is_err());
    }

    #[test]
    fn write_record_propagates_write_errors() {
        let rec = header();
        // Fail on the 8-byte length prefix, and (after it) on the payload.
        assert!(write_record(&mut FailAfter { remaining: 0 }, &rec).is_err());
        assert!(write_record(&mut FailAfter { remaining: 8 }, &rec).is_err());
    }

    #[test]
    fn read_archive_lenient_matches_strict_on_a_clean_archive() {
        // With no torn tail, the lenient reader returns exactly what the strict
        // reader does.
        let records = all_record_kinds();
        let mut buf = Vec::new();
        write_archive_start(&mut buf, RUN_ARCHIVE_VERSION).unwrap();
        for r in &records {
            write_record(&mut buf, r).unwrap();
        }
        let (version, read) = read_archive_lenient(&mut buf.as_slice()).unwrap();
        assert_eq!(version, RUN_ARCHIVE_VERSION);
        assert_eq!(read, records);
    }

    #[test]
    fn read_archive_lenient_keeps_valid_prefix_before_a_torn_tail() {
        // A valid preamble + two full records, then a truncated frame (a crash
        // mid-append). The strict reader would reject the whole file; the lenient
        // reader returns the two intact records and stops at the torn tail.
        let mut buf = Vec::new();
        write_archive_start(&mut buf, RUN_ARCHIVE_VERSION).unwrap();
        write_record(&mut buf, &header()).unwrap();
        write_record(
            &mut buf,
            &RunRecord::ContextCheckpoint {
                snapshot: snapshot("plan", vec![region("conv", vec![entry("hi", 1)])]),
                at: 1,
            },
        )
        .unwrap();
        // A frame claiming 10 payload bytes but only 2 present → torn tail.
        buf.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 10, 1, 2]);

        // Strict rejects the whole archive.
        assert!(read_archive(&mut buf.as_slice()).is_err());
        // Lenient keeps the valid prefix and folds cleanly.
        let (version, records) = read_archive_lenient(&mut buf.as_slice()).unwrap();
        assert_eq!(version, RUN_ARCHIVE_VERSION);
        assert_eq!(records.len(), 2);
        let folded = fold(&records).expect("prefix starts with a Header");
        assert_eq!(folded.context.regions[0].entries.len(), 1);
    }

    #[test]
    fn read_archive_lenient_still_errors_on_a_bad_preamble() {
        // The preamble is validated strictly: a file that isn't a run archive at
        // all errors rather than folding to nothing.
        let mut bad_magic: &[u8] = b"XXXX\x00\x01";
        assert!(read_archive_lenient(&mut bad_magic).is_err());
        // A truncated version (valid magic, no version bytes) also errors.
        let mut short: &[u8] = b"LVR1";
        assert!(read_archive_lenient(&mut short).is_err());
    }

    #[test]
    fn read_archive_start_errors_on_truncated_version() {
        // Valid 4-byte magic but no version bytes → the version read errors.
        let mut bytes: &[u8] = b"LVR1";
        let err = read_archive_start(&mut bytes).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    // ── fold ──

    #[test]
    fn fold_requires_a_header_first() {
        assert!(fold(&[]).is_none());
        assert!(
            fold(&[RunRecord::StatusChanged {
                status: RunStatus::Complete,
                at: 1
            }])
            .is_none()
        );
    }

    #[test]
    fn fold_reconstructs_state_from_the_journal() {
        let records = all_record_kinds();
        let folded = fold(&records).expect("has header");
        // Ownership was reassigned mid-journal.
        assert_eq!(folded.identity.machine_id, "machine-b");
        assert_eq!(folded.identity.world_id, "world-y");
        // Counters.
        assert_eq!(folded.inference_count, 1);
        assert_eq!(folded.tool_call_count, 1);
        // One inbound message recorded.
        assert_eq!(folded.messages.len(), 1);
        assert_eq!(folded.messages[0].content, "another");
        // The Progress step is the last context-affecting record: it layers its
        // append diff onto the preceding Checkpoint's window (hi + step).
        assert_eq!(folded.context.regions[0].name, "conv");
        assert_eq!(folded.context.regions[0].entries.len(), 2);
        assert_eq!(folded.context.total_tokens, 3);
        assert_eq!(folded.meta.run_id, "run-1");
        // The batch shares the meta's iteration and its turn never reached the
        // window, so it folds as pending (with the ToolCallDone merged in).
        let pending = folded.pending_batch.expect("batch never applied");
        assert_eq!(pending.calls[0].result.as_deref(), Some("body"));
    }

    #[test]
    fn fold_applies_context_diffs_over_a_checkpoint() {
        // Header → checkpoint → diff (append). The diff must layer on the checkpoint.
        let records = vec![
            header(),
            RunRecord::ContextCheckpoint {
                snapshot: snapshot("plan", vec![region("conv", vec![entry("hi", 1)])]),
                at: 1,
            },
            RunRecord::ContextDiff {
                delta: ContextDelta {
                    stage_name: "plan".to_string(),
                    total_tokens: 3,
                    max_tokens: 10_000,
                    regions: vec![RegionDelta::Append {
                        name: "conv".to_string(),
                        entries: vec![entry("there", 2)],
                        current_tokens: 3,
                    }],
                },
                at: 2,
            },
        ];
        let folded = fold(&records).unwrap();
        assert_eq!(folded.context.regions[0].entries.len(), 2);
        assert_eq!(folded.context.total_tokens, 3);
    }

    #[test]
    fn fold_later_header_updates_identity_and_meta() {
        // A second Header (unusual, but tolerated) refreshes identity + meta.
        let mut second_meta = meta();
        second_meta.status = RunStatus::Running;
        let records = vec![
            header(),
            RunRecord::Header {
                identity: RunIdentity {
                    run_id: "run-1".to_string(),
                    machine_id: "machine-c".to_string(),
                    world_id: "world-z".to_string(),
                    created_at: 200,
                },
                meta: Box::new(second_meta),
            },
        ];
        let folded = fold(&records).unwrap();
        assert_eq!(folded.identity.machine_id, "machine-c");
        assert_eq!(folded.meta.status, RunStatus::Running);
    }

    #[test]
    fn fold_progress_applies_meta_and_context_diff() {
        let mut advanced = meta();
        advanced.status = RunStatus::Running;
        advanced.iteration = 5;
        let records = vec![
            header(),
            RunRecord::ContextCheckpoint {
                snapshot: snapshot("plan", vec![region("conv", vec![entry("hi", 1)])]),
                at: 1,
            },
            RunRecord::Progress {
                meta: Box::new(advanced),
                delta: ContextDelta {
                    stage_name: "plan".to_string(),
                    total_tokens: 3,
                    max_tokens: 10_000,
                    regions: vec![RegionDelta::Append {
                        name: "conv".to_string(),
                        entries: vec![entry("there", 2)],
                        current_tokens: 3,
                    }],
                },
                at: 2,
            },
        ];
        let folded = fold(&records).unwrap();
        assert_eq!(folded.meta.iteration, 5);
        assert_eq!(folded.meta.status, RunStatus::Running);
        assert_eq!(folded.context.regions[0].entries.len(), 2);
    }

    // ── pending tool batch (fold) ──

    fn call(id: &str, result: Option<&str>) -> ToolCallRecord {
        ToolCallRecord {
            id: id.to_string(),
            name: "shell".to_string(),
            arguments: "{}".to_string(),
            result: result.map(str::to_string),
            thought_signature: None,
        }
    }

    fn batch(iteration: usize, calls: Vec<ToolCallRecord>) -> RunRecord {
        RunRecord::ToolBatch {
            calls,
            at: 10,
            stage_index: 0,
            iteration,
            response: "running tools".to_string(),
        }
    }

    /// An entry whose kind is the assistant turn that issued `call_ids`.
    fn turn_entry(call_ids: &[&str]) -> RegionEntrySnapshot {
        let mut e = entry("turn", 1);
        e.kind = crate::region::EntryKind::AssistantTurn {
            tool_calls: call_ids
                .iter()
                .map(|id| crate::region::SerializedToolCall {
                    id: id.to_string(),
                    name: "shell".to_string(),
                    arguments: serde_json::Value::Null,
                    thought_signature: None,
                })
                .collect(),
        };
        e
    }

    #[test]
    fn fold_surfaces_a_pending_batch_with_merged_results() {
        // meta().iteration is 0, matching the batch, and the context has no
        // assistant turn for it - so the batch is genuinely pending. c1's
        // ToolCallDone merges in; c2 keeps its dispatch-time inline result; c3
        // stays pending.
        let records = vec![
            header(),
            batch(
                0,
                vec![
                    call("c1", None),
                    call("c2", Some("inline")),
                    call("c3", None),
                ],
            ),
            RunRecord::ToolCallDone {
                iteration: 0,
                call_id: "c1".to_string(),
                result: "ran".to_string(),
                at: 11,
            },
        ];
        let folded = fold(&records).unwrap();
        let pending = folded.pending_batch.expect("batch is pending");
        assert_eq!(pending.iteration, 0);
        assert_eq!(pending.response, "running tools");
        assert_eq!(pending.calls[0].result.as_deref(), Some("ran"));
        assert_eq!(pending.calls[1].result.as_deref(), Some("inline"));
        assert_eq!(pending.calls[2].result, None);
        assert_eq!(folded.tool_call_count, 3);
    }

    #[test]
    fn fold_keeps_only_the_latest_batch_and_ignores_stale_done_records() {
        // The second batch replaces the first; a ToolCallDone for the replaced
        // iteration is ignored, as is one naming a call the batch doesn't have.
        let mut advanced = meta();
        advanced.iteration = 1;
        let records = vec![
            header(),
            batch(0, vec![call("c1", None)]),
            RunRecord::Progress {
                meta: Box::new(advanced),
                delta: ContextDelta {
                    stage_name: "plan".to_string(),
                    total_tokens: 0,
                    max_tokens: 10_000,
                    regions: vec![],
                },
                at: 11,
            },
            batch(1, vec![call("c2", None)]),
            RunRecord::ToolCallDone {
                iteration: 0,
                call_id: "c1".to_string(),
                result: "stale".to_string(),
                at: 12,
            },
            RunRecord::ToolCallDone {
                iteration: 1,
                call_id: "unknown".to_string(),
                result: "nowhere to land".to_string(),
                at: 13,
            },
        ];
        let folded = fold(&records).unwrap();
        let pending = folded.pending_batch.expect("latest batch is pending");
        assert_eq!(pending.iteration, 1);
        assert_eq!(pending.calls.len(), 1);
        assert_eq!(pending.calls[0].id, "c2");
        assert_eq!(pending.calls[0].result, None, "stale/unknown dones ignored");
    }

    #[test]
    fn fold_clears_a_batch_once_the_iteration_moves_on() {
        // A later inference bumped meta.iteration past the batch: the batch was
        // applied (even if a sliding window evicted the turn), nothing to replay.
        let mut advanced = meta();
        advanced.iteration = 1;
        let records = vec![
            header(),
            batch(0, vec![call("c1", Some("done"))]),
            RunRecord::Progress {
                meta: Box::new(advanced),
                delta: ContextDelta {
                    stage_name: "plan".to_string(),
                    total_tokens: 0,
                    max_tokens: 10_000,
                    regions: vec![],
                },
                at: 11,
            },
        ];
        assert_eq!(fold(&records).unwrap().pending_batch, None);
    }

    #[test]
    fn fold_clears_a_batch_whose_turn_already_landed_in_the_window() {
        // Same iteration, but the context already holds the batch's assistant
        // turn: apply_tool_results ran before the crash, nothing to replay.
        let records = vec![
            header(),
            batch(0, vec![call("c1", Some("done"))]),
            RunRecord::ContextCheckpoint {
                snapshot: snapshot("plan", vec![region("conv", vec![turn_entry(&["c1"])])]),
                at: 11,
            },
        ];
        assert_eq!(fold(&records).unwrap().pending_batch, None);
    }

    #[test]
    fn context_contains_batch_matches_only_the_batch_turn() {
        let pending = PendingToolBatch {
            stage_index: 0,
            iteration: 0,
            response: String::new(),
            calls: vec![call("c1", None)],
        };
        // A window with an unrelated turn does not match.
        let other = snapshot("plan", vec![region("conv", vec![turn_entry(&["zz"])])]);
        assert!(!context_contains_batch(&other, &pending));
        // The batch's own turn matches by its first call id.
        let own = snapshot(
            "plan",
            vec![region("conv", vec![turn_entry(&["c1", "c2"])])],
        );
        assert!(context_contains_batch(&own, &pending));
        // A batch with no calls can never match.
        let empty = PendingToolBatch {
            calls: vec![],
            ..pending
        };
        assert!(!context_contains_batch(&own, &empty));
    }

    #[test]
    fn old_shape_tool_batch_json_still_parses() {
        // Archives written before the batch-journal fields existed carry
        // ToolBatch records without stage_index/iteration/response (and calls
        // without thought_signature); serde defaults fill them in.
        let json = br#"{"ToolBatch":{"calls":[{"id":"c1","name":"shell","arguments":"{}","result":"ok"}],"at":9}}"#;
        let mut buf = Vec::new();
        buf.extend_from_slice(&(json.len() as u64).to_be_bytes());
        buf.extend_from_slice(json);
        let record = read_record(&mut buf.as_slice()).unwrap().unwrap();
        assert_eq!(
            record,
            RunRecord::ToolBatch {
                calls: vec![call("c1", Some("ok"))],
                at: 9,
                stage_index: 0,
                iteration: 0,
                response: String::new(),
            }
        );
    }

    // ── replay_points (context-window history) ──

    /// Three context changes, so a windowing caller has something to page over.
    fn three_point_records() -> Vec<RunRecord> {
        let mut running = meta();
        running.status = RunStatus::Running;
        vec![
            header(),
            RunRecord::ContextCheckpoint {
                snapshot: snapshot("plan", vec![region("conv", vec![entry("first", 1)])]),
                at: 10,
            },
            RunRecord::ContextDiff {
                delta: ContextDelta {
                    stage_name: "plan".to_string(),
                    total_tokens: 2,
                    max_tokens: 10_000,
                    regions: vec![RegionDelta::Append {
                        name: "conv".to_string(),
                        entries: vec![entry("second", 1)],
                        current_tokens: 2,
                    }],
                },
                at: 20,
            },
            RunRecord::Progress {
                meta: Box::new(running),
                delta: ContextDelta {
                    stage_name: "code".to_string(),
                    total_tokens: 3,
                    max_tokens: 10_000,
                    regions: vec![RegionDelta::Append {
                        name: "conv".to_string(),
                        entries: vec![entry("third", 1)],
                        current_tokens: 3,
                    }],
                },
                at: 30,
            },
        ]
    }

    #[test]
    fn visit_points_indexes_points_in_order_and_carries_the_running_window() {
        let records = three_point_records();
        let mut seen: Vec<(usize, i64, usize)> = Vec::new();
        visit_points(&records, &mut |point| {
            seen.push((
                point.index,
                point.at,
                point.context.regions[0].entries.len(),
            ));
            ControlFlow::Continue(())
        });
        // Index counts points, not records - the Header produces none.
        assert_eq!(seen, vec![(0, 10, 1), (1, 20, 2), (2, 30, 3)]);
    }

    /// The reason this function exists: a caller wanting one window, or an
    /// answer to "does any point match", must be able to stop.
    #[test]
    fn visit_points_stops_at_the_first_break() {
        let records = three_point_records();
        let mut visits = 0;
        visit_points(&records, &mut |point| {
            visits += 1;
            if point.index == 1 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        assert_eq!(
            visits, 2,
            "stopped at the breaking point, did not run the third"
        );
    }

    #[test]
    fn visit_points_without_a_header_visits_nothing() {
        let mut visits = 0;
        visit_points(&[], &mut |_| {
            visits += 1;
            ControlFlow::Continue(())
        });
        visit_points(
            &[RunRecord::ContextCheckpoint {
                snapshot: snapshot("plan", vec![]),
                at: 1,
            }],
            &mut |_| {
                visits += 1;
                ControlFlow::Continue(())
            },
        );
        assert_eq!(visits, 0);
    }

    /// `replay_points` is now a thin collector over `visit_points`, so this
    /// pins the two together: if the reimplementation ever drifts, the borrowed
    /// walk and the materialized one stop agreeing here first.
    #[test]
    fn visit_points_and_replay_points_agree() {
        for records in [
            three_point_records(),
            vec![header()],
            vec![],
            vec![RunRecord::Message {
                message: MessageRecord {
                    role: "user".to_string(),
                    content: "x".to_string(),
                },
                at: 1,
            }],
        ] {
            let collected: Vec<RunPoint> = {
                let mut out = Vec::new();
                visit_points(&records, &mut |point| {
                    out.push(RunPoint {
                        meta: point.meta.clone(),
                        context: point.context.clone(),
                        at: point.at,
                    });
                    ControlFlow::Continue(())
                });
                out
            };
            assert_eq!(collected, replay_points(&records));
        }
    }

    #[test]
    fn replay_points_requires_a_header() {
        assert!(replay_points(&[]).is_empty());
        assert!(
            replay_points(&[RunRecord::Message {
                message: MessageRecord {
                    role: "user".to_string(),
                    content: "x".to_string(),
                },
                at: 1,
            }])
            .is_empty()
        );
    }

    #[test]
    fn replay_points_emits_a_snapshot_per_context_change() {
        // Header (no point) → checkpoint (point 1) → status (no point, but tracked)
        // → progress diff (point 2). Non-context records don't add points.
        let mut running = meta();
        running.status = RunStatus::Running;
        let records = vec![
            header(),
            RunRecord::Inference {
                stage: "plan".to_string(),
                iteration: 0,
                request: InferenceRequestRecord {
                    model: "m".to_string(),
                    system: vec![],
                    messages: vec![],
                    tool_names: vec![],
                    temperature: 0.7,
                    max_tokens: 10,
                },
                response: InferenceResponseRecord {
                    content: "ok".to_string(),
                    tool_calls: vec![],
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                at: 1,
            },
            batch(0, vec![call("c1", None)]),
            RunRecord::ToolCallDone {
                iteration: 0,
                call_id: "c1".to_string(),
                result: "ran".to_string(),
                at: 1,
            },
            RunRecord::ContextCheckpoint {
                snapshot: snapshot("plan", vec![region("conv", vec![entry("hi", 1)])]),
                at: 2,
            },
            RunRecord::StatusChanged {
                status: RunStatus::Running,
                at: 3,
            },
            RunRecord::Progress {
                meta: Box::new(running),
                delta: ContextDelta {
                    stage_name: "implement".to_string(),
                    total_tokens: 3,
                    max_tokens: 10_000,
                    regions: vec![RegionDelta::Append {
                        name: "conv".to_string(),
                        entries: vec![entry("more", 2)],
                        current_tokens: 3,
                    }],
                },
                at: 4,
            },
        ];
        let points = replay_points(&records);
        assert_eq!(points.len(), 2, "one point per context change");
        // First point: the checkpoint window.
        assert_eq!(points[0].at, 2);
        assert_eq!(points[0].context.regions[0].entries.len(), 1);
        // Second point: the progress diff layered on, with the running status
        // carried from the StatusChanged + the progress meta.
        assert_eq!(points[1].at, 4);
        assert_eq!(points[1].context.regions[0].entries.len(), 2);
        assert_eq!(points[1].context.stage_name, "implement");
        assert_eq!(points[1].meta.status, RunStatus::Running);
    }

    #[test]
    fn replay_points_handles_context_diff_and_a_later_header() {
        // A standalone ContextDiff is a point; a second Header refreshes meta
        // without adding a point.
        let mut relabeled = meta();
        relabeled.agent_name = "renamed".to_string();
        let records = vec![
            header(),
            RunRecord::ContextCheckpoint {
                snapshot: snapshot("plan", vec![region("conv", vec![entry("hi", 1)])]),
                at: 1,
            },
            RunRecord::Header {
                identity: identity(),
                meta: Box::new(relabeled),
            },
            RunRecord::ContextDiff {
                delta: ContextDelta {
                    stage_name: "plan".to_string(),
                    total_tokens: 3,
                    max_tokens: 10_000,
                    regions: vec![RegionDelta::Append {
                        name: "conv".to_string(),
                        entries: vec![entry("more", 2)],
                        current_tokens: 3,
                    }],
                },
                at: 2,
            },
        ];
        let points = replay_points(&records);
        assert_eq!(points.len(), 2); // checkpoint + diff (header adds no point)
        assert_eq!(points[1].context.regions[0].entries.len(), 2);
        // The later Header's meta is in effect at the diff point.
        assert_eq!(points[1].meta.agent_name, "renamed");
    }

    #[test]
    fn replay_points_over_a_full_checkpoint() {
        // A `Checkpoint` (full meta+context) is also a point.
        let records = vec![
            header(),
            RunRecord::Checkpoint {
                meta: Box::new(meta()),
                context: snapshot("review", vec![region("conv", vec![entry("x", 4)])]),
                at: 9,
            },
        ];
        let points = replay_points(&records);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].context.stage_name, "review");
        assert_eq!(points[0].context.regions[0].entries[0].tokens, 4);
    }
}

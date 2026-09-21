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

use serde::{Deserialize, Serialize};

use crate::run_meta::{ContextSnapshot, RegionEntrySnapshot, RegionSnapshot, RunMeta, RunStatus};

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
    /// The tool-call id, as the provider assigned it.
    ///
    /// Correlation, not identity: a provider may reuse one across a retry or a
    /// reissue, so two attempts can arrive under one id. What tells them apart
    /// is [`execution_id`](Self::execution_id).
    pub id: String,
    /// This attempt's own id, minted at dispatch.
    ///
    /// Empty in a journal written before executions had identity, where the
    /// provider's id was all there was: a reader treats an empty one as "this
    /// attempt was not identified" rather than as an attempt with a blank name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub execution_id: String,
    /// The tool name.
    pub name: String,
    /// The JSON arguments, stringified.
    pub arguments: String,
    /// The result, once the tool has run (`None` while pending). Text and
    /// any stored parts the tool produced; a plain string on the wire when
    /// it is text alone, which is what every journal written before parts
    /// existed holds.
    pub result: Option<crate::region::EntryContent>,
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

/// Which provider call a [`RunRecord::InferenceUsage`] belongs to.
///
/// A run bills for more than its stage turns, and the three auxiliary kinds are
/// invisible in every other surface: they do not appear in the stage ledger and
/// nothing else names them. Recording which kind spent the tokens is what turns
/// a total into an explanation - "this run cost double what its stages did
/// because its edges compact" is a sentence the journal can now support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceKind {
    /// An ordinary stage turn: the agent thinking or calling tools. The default
    /// so a journal written before this field existed reads back as stage work,
    /// which is what every record in one is.
    #[default]
    Stage,
    /// A region-summarizing call, from memory pressure or an edge transform.
    Compaction,
    /// The one-off call that names the run.
    Title,
    /// A call asking the model which stage to move to next.
    Routing,
}

impl InferenceKind {
    /// A short stable label, for logs and wire formats that want a string.
    pub fn label(&self) -> &'static str {
        match self {
            InferenceKind::Stage => "stage",
            InferenceKind::Compaction => "compaction",
            InferenceKind::Title => "title",
            InferenceKind::Routing => "routing",
        }
    }

    /// Whether this call is stage work the agent asked for, as opposed to
    /// machinery the runtime ran on its behalf.
    pub fn is_stage_work(&self) -> bool {
        matches!(self, InferenceKind::Stage)
    }
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
    /// What one provider call cost, written as it lands.
    ///
    /// [`RunRecord::Progress`] carries cumulative counters, so two calls between
    /// two ticks are indistinguishable downstream: their sum arrives as one
    /// number, and a chart of it shows a spike no single call ever made. This
    /// record is per call, so token-over-time telemetry is exact and "no request
    /// ever exceeded the window" is provable from the journal rather than
    /// inferred from region-size checkpoints.
    ///
    /// Deliberately lighter than [`RunRecord::Inference`], which carries the
    /// full request and response: every call re-sends the whole window, so
    /// journaling those bodies for each of them would multiply the file by the
    /// context size. The window is already recoverable from the surrounding
    /// [`RunRecord::ContextCheckpoint`] and [`RunRecord::ContextDiff`] records.
    InferenceUsage {
        /// Which kind of call this was.
        #[serde(default)]
        kind: InferenceKind,
        /// The stage the run was in. Empty for a call with no stage of its own
        /// (the title call, which runs once at spawn).
        stage: String,
        /// The stage-local iteration index.
        iteration: usize,
        /// The provider that served the call.
        provider: String,
        /// The model the call targeted.
        model: String,
        /// Prompt tokens billed.
        prompt_tokens: usize,
        /// Completion tokens billed.
        completion_tokens: usize,
        /// Tokens read from provider cache.
        cached_tokens: usize,
        /// Tokens written to provider cache.
        cache_write_tokens: usize,
        /// What this one call cost in USD, when it could be established.
        ///
        /// Per call rather than only per run, because the model can change
        /// mid-run: a stage that fails over to a second provider has spent at
        /// two different rates, and a run-level total cannot be re-derived from
        /// `meta.model` afterwards. Recorded here, the journal answers "which
        /// call cost that" without re-pricing anything.
        ///
        /// `None` means unpriced - the provider reported no cost and no rates
        /// were known - never that the call was free.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        /// Whether `cost_usd` is the provider's own figure rather than one
        /// computed from published rates. Absent when there is no cost.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_reported_by_provider: Option<bool>,
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
        /// The stay in that stage the batch was dispatched during, as minted
        /// when the run entered it.
        ///
        /// The correlation key a reader wants, where the index and the iteration
        /// are only where it sat: a stage entered three times has three stays,
        /// and the index is the same for all of them. Empty in a journal written
        /// before visits had identity, and in a world with no stage ledger.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        visit_id: String,
        /// The provider attempt whose answer asked for these calls, as minted
        /// before that request went out.
        ///
        /// Empty in a journal written before attempts had identity, and on a
        /// batch no answer asked for. A reader must not fall back to the nearest
        /// attempt in time: a failover means the answer came from a different
        /// provider than the attempt before it went to.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        requested_by: String,
        /// The assistant text of the turn that issued the calls.
        #[serde(default)]
        response: String,
    },
    /// One tool call of the pending batch finished; its result.
    ToolCallDone {
        /// The iteration of the [`RunRecord::ToolBatch`] this belongs to.
        iteration: usize,
        /// The tool-call id, as the provider assigned it. Correlation; see
        /// [`ToolCallRecord::id`].
        call_id: String,
        /// The attempt this completes, as minted at dispatch. Empty in a journal
        /// written before executions had identity.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        execution_id: String,
        /// The result: text and any stored parts.
        ///
        /// For an indeterminate outcome this is the stand-in a resume put in the
        /// window, not something the tool returned. The outcome is what tells
        /// the two apart, and a reader showing this text as the tool's answer
        /// would be inventing one.
        result: crate::region::EntryContent,
        /// How the attempt ended.
        ///
        /// `None` in a journal written before outcomes were recorded, where a
        /// completion record said only that the call finished and a failure was
        /// text inside the result. A reader must not invent one of the five
        /// states for such a record.
        ///
        /// Recorded so far only where it cannot be read off the result at all:
        /// an execution a resume gave up on, which no later reader could
        /// distinguish from one that finished.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<crate::execution::ToolOutcome>,
        /// Unix seconds.
        at: i64,
    },
    /// Files one tool execution produced, recorded as it produced them.
    ///
    /// `output.json` holds the artifacts of the run's *latest* answer, and a
    /// later submission replaces it whole: a file an earlier submission produced
    /// leaves no trace there at all, and nothing in that file says which call
    /// made any of it. This record is written by the dispatcher that was handling
    /// the call, so an artifact is attributable for as long as the journal
    /// exists, superseded submissions included.
    ArtifactsProduced {
        /// The execution that produced them, as minted at dispatch.
        execution_id: String,
        /// The files, exactly as the answer recorded them.
        artifacts: Vec<crate::output::Artifact>,
        /// Unix seconds.
        at: i64,
    },
    /// A question this run put to a person, and what came back.
    ///
    /// The only record that a run stopped for someone. Without it an approved
    /// call is indistinguishable from one no policy ever stopped, and the scope
    /// a person chose - this call, this stage, the rest of the run - is gone the
    /// moment the tool reads its answer.
    Interaction {
        /// The request id the hub minted, which is what an answer arriving over
        /// the API or from `lev respond` carries.
        request_id: String,
        /// What was asked for.
        kind: crate::interaction::InteractionKind,
        /// The tool an approval was for. `None` for every other kind.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
        /// The question as the person saw it.
        prompt: String,
        /// The stage the run was in when it asked.
        stage: String,
        /// How it ended.
        settlement: crate::interaction::Settlement,
        /// Unix seconds when the question was asked.
        asked_at: i64,
        /// Unix seconds when it settled.
        at: i64,
    },
    /// One trip to a provider, whether or not it produced an answer.
    ///
    /// The usage records say what the calls that worked cost. These say what the
    /// run spent getting them, which is the half a retry or a failover otherwise
    /// leaves no trace of at all.
    InferenceAttempt(AttemptRecord),
    /// One provider judged unusable, and the model being tried instead.
    InferenceFailover(FailoverRecord),
    /// A full context-window snapshot that subsequent diffs rebase on.
    ContextCheckpoint {
        /// The full window snapshot.
        snapshot: ContextSnapshot,
        /// Unix seconds.
        at: i64,
    },
    /// Why a region changed, recorded as it changed.
    ///
    /// The snapshots beside this say what the window held; they cannot say what
    /// moved it, and a region that lost its plan looks identical whether a
    /// compaction took it, a stage-edge transform cleared it, or the model
    /// called `context_delete`. Carries no content: the snapshot recorded on the
    /// same tick already holds the window, so repeating the text here would
    /// double the journal to say nothing new.
    ContextChange {
        /// The region that changed.
        region: String,
        /// What changed it.
        cause: crate::ContextCause,
        /// Entries the change added.
        entries_added: usize,
        /// Entries it removed, the eviction the change itself triggered
        /// included.
        entries_removed: usize,
        /// How the region's token count moved; negative when it shrank.
        token_delta: i64,
        /// Unix seconds.
        at: i64,
    },
    /// One committed transaction against the context window: what moved it, the
    /// window it started from and the window it produced, and every region it
    /// touched.
    ///
    /// The record a debugger joins on. A change carries the window's
    /// [revision](crate::run_meta::revision) either side, so it is anchored to
    /// exact content rather than to a moment, and it carries every region of the
    /// transaction at once - a compaction that summarised one region and emptied
    /// another is one record, not two events that share a second.
    ///
    /// Carries no content, for the reason [`RunRecord::ContextChange`] gives:
    /// the snapshot recorded on the same tick holds the text, and the per-region
    /// digests here are what tell a reader whether it needs to go and read it.
    ContextTransaction {
        /// The window's revision before the transaction.
        revision_before: String,
        /// The window's revision after it.
        revision_after: String,
        /// What made the change.
        cause: crate::ContextCause,
        /// Every region the transaction touched, in the order the write path
        /// named them.
        regions: Vec<RegionCommit>,
        /// The tool execution that committed it, as minted at dispatch. Empty
        /// where nothing knew of one: a write outside any tool call, and a tool
        /// whose results the batch applies rather than the call itself.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        execution_id: String,
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

/// What the diff needs to know about a region as it was.
///
/// Two questions per region: did anything change, and if so, did it change
/// only by appending at the tail? A full previous snapshot answers them by
/// comparing entries; a retained [`RegionDigest`] answers them from per-entry
/// hashes. Everything else about the diff - which regions are new, which
/// went away, which were cleared - is the same algorithm over either.
trait PriorRegion {
    /// The region's name, which is what pairs it with its successor.
    fn name(&self) -> &str;
    /// Whether `next` is this region, unchanged.
    fn unchanged(&self, next: &RegionSnapshot) -> bool;
    /// Whether `next` kept this region's kind and budget and only appended.
    fn appended_to(&self, next: &RegionSnapshot) -> bool;
    /// How many entries this region held.
    fn entry_count(&self) -> usize;
}

impl PriorRegion for RegionSnapshot {
    fn name(&self) -> &str {
        &self.name
    }

    fn unchanged(&self, next: &RegionSnapshot) -> bool {
        self == next
    }

    fn appended_to(&self, next: &RegionSnapshot) -> bool {
        self.kind == next.kind
            && self.max_tokens == next.max_tokens
            && is_prefix(&self.entries, &next.entries)
    }

    fn entry_count(&self) -> usize {
        self.entries.len()
    }
}

/// The delta turning the regions `prev` describes into `next`.
///
/// Regions that only grew at the tail become a compact `Append`; everything
/// else is carried as a `Set`/`Clear`/`Remove`.
fn diff_regions<P: PriorRegion>(prev: &[P], next: &ContextSnapshot) -> ContextDelta {
    let mut regions = Vec::new();
    for nr in &next.regions {
        match prev.iter().find(|r| r.name() == nr.name) {
            None => regions.push(RegionDelta::Set(nr.clone())),
            Some(pr) => {
                if pr.unchanged(nr) {
                    // unchanged - emit nothing
                } else if nr.entries.is_empty() && pr.entry_count() > 0 {
                    regions.push(RegionDelta::Clear {
                        name: nr.name.clone(),
                    });
                } else if pr.appended_to(nr) {
                    regions.push(RegionDelta::Append {
                        name: nr.name.clone(),
                        entries: nr.entries[pr.entry_count()..].to_vec(),
                        current_tokens: nr.current_tokens,
                    });
                } else {
                    regions.push(RegionDelta::Set(nr.clone()));
                }
            }
        }
    }
    for pr in prev {
        if !next.regions.iter().any(|r| r.name == pr.name()) {
            regions.push(RegionDelta::Remove {
                name: pr.name().to_string(),
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

/// Compute the minimal-ish [`ContextDelta`] turning `prev` into `next`. Regions
/// that only grew at the tail become a compact `Append`; everything else is
/// carried as a `Set`/`Clear`/`Remove`.
pub fn diff_context(prev: &ContextSnapshot, next: &ContextSnapshot) -> ContextDelta {
    diff_regions(&prev.regions, next)
}

// ─── digest-based diffing ───────────────────────────────────────────────────
//
// `diff_context` needs the previous snapshot only to answer two questions per
// region: "did anything change?" and "did it change by appending at the tail?".
// A per-entry fingerprint answers both, so the writer can retain this digest
// instead of a full copy of every live run's context window (which doubled the
// per-run resident cost of the persistence lane).

/// Fingerprint of one region: everything `diff_context` compares except the
/// entry contents themselves, which are folded into per-entry hashes.
#[derive(Debug, Clone, PartialEq)]
pub struct RegionDigest {
    /// The region name.
    pub name: String,
    /// The region's stringified kind.
    pub kind: String,
    /// The region's token count at digest time.
    pub current_tokens: usize,
    /// The region's token budget at digest time.
    pub max_tokens: usize,
    /// One hash per entry, in order.
    pub entries: Vec<u64>,
}

/// Fingerprint of a whole context window, cheap to retain per live run.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ContextDigest {
    /// Per-region fingerprints, in snapshot order.
    pub regions: Vec<RegionDigest>,
}

impl ContextDigest {
    /// This fingerprint folded into one opaque hex string, for a record that
    /// needs to name a window rather than compare it region by region.
    ///
    /// Folded from the per-entry hashes the coalescing lane already computes, so
    /// two windows that this digest calls identical fold to the same string and
    /// nothing hashes the context twice.
    pub fn fingerprint(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for region in &self.regions {
            region.name.hash(&mut hasher);
            region.kind.hash(&mut hasher);
            region.current_tokens.hash(&mut hasher);
            region.max_tokens.hash(&mut hasher);
            region.entries.hash(&mut hasher);
        }
        format!("{:016x}", hasher.finish())
    }
}

/// Hash one region entry. Every field participates: two entries that differ
/// anywhere must digest differently, or a real change would be recorded as
/// "unchanged" and the folded archive would silently drift from the run.
fn entry_digest(entry: &RegionEntrySnapshot) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    entry.content.hash(&mut hasher);
    entry.tokens.hash(&mut hasher);
    entry.key.hash(&mut hasher);
    // kind / metadata / taint are small enums and values without a Hash impl;
    // their serialized form is tiny next to `content` and hashes faithfully.
    serde_json::to_string(&entry.kind)
        .expect("EntryKind always serializes")
        .hash(&mut hasher);
    serde_json::to_string(&entry.metadata)
        .expect("entry metadata always serializes")
        .hash(&mut hasher);
    serde_json::to_string(&entry.taint)
        .expect("taint always serializes")
        .hash(&mut hasher);
    hasher.finish()
}

/// Compute the retained fingerprint of `snapshot`.
pub fn digest_context(snapshot: &ContextSnapshot) -> ContextDigest {
    ContextDigest {
        regions: snapshot
            .regions
            .iter()
            .map(|r| RegionDigest {
                name: r.name.clone(),
                kind: r.kind.clone(),
                current_tokens: r.current_tokens,
                max_tokens: r.max_tokens,
                entries: r.entries.iter().map(entry_digest).collect(),
            })
            .collect(),
    }
}

/// Whether `prev`'s entry hashes are a prefix of `next`'s entries.
fn is_prefix_digest(prev: &[u64], next: &[RegionEntrySnapshot]) -> bool {
    prev.len() <= next.len()
        && prev
            .iter()
            .zip(next)
            .all(|(hash, entry)| *hash == entry_digest(entry))
}

impl PriorRegion for RegionDigest {
    fn name(&self) -> &str {
        &self.name
    }

    fn unchanged(&self, next: &RegionSnapshot) -> bool {
        self.kind == next.kind
            && self.max_tokens == next.max_tokens
            && self.current_tokens == next.current_tokens
            && self.entries.len() == next.entries.len()
            && is_prefix_digest(&self.entries, &next.entries)
    }

    fn appended_to(&self, next: &RegionSnapshot) -> bool {
        self.kind == next.kind
            && self.max_tokens == next.max_tokens
            && is_prefix_digest(&self.entries, &next.entries)
    }

    fn entry_count(&self) -> usize {
        self.entries.len()
    }
}

/// [`diff_context`] against a retained [`ContextDigest`] instead of a full
/// previous snapshot. The same algorithm over a different idea of "before":
/// unchanged regions emit nothing, tail growth becomes `Append`, everything
/// else `Set`/`Clear`/`Remove`.
pub fn diff_context_digest(prev: &ContextDigest, next: &ContextSnapshot) -> ContextDelta {
    diff_regions(&prev.regions, next)
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

mod attempt;
mod codec;
mod executions;
mod points;
mod transaction;

pub use attempt::{
    AttemptOutcome, AttemptRecord, CaptureStatus, FailoverRecord, ModelInput, RequestDigest, Retry,
};
pub use codec::{
    Frame, Frames, RUN_ARCHIVE_MAGIC, RUN_ARCHIVE_VERSION, read_archive, read_archive_lenient,
    read_archive_start, read_frame, read_record, write_archive_start, write_record,
};
pub use executions::{Execution, SeekRead, read_archive_executions, read_result_at};
pub use points::{PointRef, RunPoint, replay_points, visit_archive_points, visit_points};
pub use transaction::{
    ContextChangeRecord, IndexedChange, RegionCommit, RegionTransition, read_archive_changes,
};

// ─── fold ───────────────────────────────────────────────────────────────────

/// A tool batch that was dispatched but whose results never reached the context
/// window - what a crash-resume must replay instead of re-running. `calls` carry
/// every result recorded before the crash ([`RunRecord::ToolCallDone`] merged
/// in); a call still at `result: None` genuinely never finished.
///
/// Only a batch that had work in the tool lane becomes one. A batch the
/// dispatcher resolved entirely by itself - context tools, refusals, gate
/// denials - is re-issued instead: a replay lands recorded results in the
/// conversation without redoing a context tool's write, so replaying one would
/// put `context_write: ok` over a region that never received the content.
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

/// One provider call's cost, as folded out of the journal.
///
/// The flattened form of [`RunRecord::InferenceUsage`], so a consumer walking a
/// One question this run asked a person, folded out of the journal.
///
/// The record that a run stopped for somebody. A reader listing these can say
/// which calls a person allowed, at what scope, and which ones nobody answered
/// - none of which is recoverable from the tool results alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InteractionRecord {
    /// The request id the hub minted.
    pub request_id: String,
    /// What was asked for.
    pub kind: crate::interaction::InteractionKind,
    /// The tool an approval was for.
    pub tool: Option<String>,
    /// The question as the person saw it.
    pub prompt: String,
    /// The stage the run was in when it asked.
    pub stage: String,
    /// How it ended.
    pub settlement: crate::interaction::Settlement,
    /// Unix seconds when it was asked.
    pub asked_at: i64,
    /// Unix seconds when it settled.
    pub at: i64,
}

/// folded run does not have to match the record enum to read a number.
// `Eq` is not derivable once a cost is present: `f64` has no total equality.
// `PartialEq` is what the tests compare with anyway.
#[derive(Debug, Clone, PartialEq)]
pub struct InferenceUsageRecord {
    /// Which kind of call this was.
    pub kind: InferenceKind,
    /// The stage the run was in, empty for the title call.
    pub stage: String,
    /// The stage-local iteration index.
    pub iteration: usize,
    /// The provider that served the call.
    pub provider: String,
    /// The model the call targeted.
    pub model: String,
    /// Prompt tokens billed.
    pub prompt_tokens: usize,
    /// Completion tokens billed.
    pub completion_tokens: usize,
    /// Tokens read from provider cache.
    pub cached_tokens: usize,
    /// Tokens written to provider cache.
    pub cache_write_tokens: usize,
    /// What this call cost in USD, when it could be established at all.
    /// `None` is unpriced, never free.
    pub cost_usd: Option<f64>,
    /// Whether `cost_usd` came from the provider rather than from rates.
    pub cost_reported_by_provider: Option<bool>,
    /// Unix seconds.
    pub at: i64,
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
    /// Per-call usage, in the order the calls landed.
    ///
    /// The point of keeping every entry rather than a running sum: a sum is
    /// already available from [`RunMeta`], and what it cannot answer is whether
    /// any single call exceeded the window, or which kind of call the spend went
    /// to.
    pub inference_usage: Vec<InferenceUsageRecord>,
    /// Number of tool calls recorded.
    pub tool_call_count: usize,
    /// Every question this run put to a person, in the order it asked them.
    pub interactions: Vec<InteractionRecord>,
    /// Every trip this run made to a provider, in order, the failed ones
    /// included. Read beside `inference_usage`, which holds only the calls that
    /// produced an answer, this is what the retries cost.
    pub attempts: Vec<AttemptRecord>,
    /// Every move from one provider to another, in order.
    pub failovers: Vec<FailoverRecord>,
    /// Every committed change to the window, in the order they landed.
    ///
    /// Read beside the window itself, this is the half a snapshot cannot carry:
    /// which path in the runtime moved a region, rather than only what it holds
    /// now. Empty for a journal that records no causes, and for the write paths
    /// that cannot name one.
    pub context_changes: Vec<ContextChangeRecord>,
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
        inference_usage: Vec::new(),
        tool_call_count: 0,
        interactions: Vec::new(),
        attempts: Vec::new(),
        failovers: Vec::new(),
        context_changes: Vec::new(),
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
            // A folded run carries the state a resume needs and the totals a
            // reader asks for; which execution made which file is neither, and
            // the executions listing is where it is answered.
            RunRecord::ArtifactsProduced { .. } => {}
            RunRecord::InferenceAttempt(attempt) => folded.attempts.push(attempt.clone()),
            RunRecord::InferenceFailover(failover) => folded.failovers.push(failover.clone()),
            RunRecord::Interaction {
                request_id,
                kind,
                tool,
                prompt,
                stage,
                settlement,
                asked_at,
                at,
            } => folded.interactions.push(InteractionRecord {
                request_id: request_id.clone(),
                kind: kind.clone(),
                tool: tool.clone(),
                prompt: prompt.clone(),
                stage: stage.clone(),
                settlement: settlement.clone(),
                asked_at: *asked_at,
                at: *at,
            }),
            RunRecord::InferenceUsage {
                kind,
                stage,
                iteration,
                provider,
                model,
                prompt_tokens,
                completion_tokens,
                cached_tokens,
                cache_write_tokens,
                cost_usd,
                cost_reported_by_provider,
                at,
            } => {
                // Counted alongside the heavy variant: both name one provider
                // call, and a consumer asking "how many calls" should not have
                // to know which of the two the writer chose.
                folded.inference_count += 1;
                folded.inference_usage.push(InferenceUsageRecord {
                    kind: *kind,
                    stage: stage.clone(),
                    iteration: *iteration,
                    provider: provider.clone(),
                    model: model.clone(),
                    prompt_tokens: *prompt_tokens,
                    completion_tokens: *completion_tokens,
                    cached_tokens: *cached_tokens,
                    cache_write_tokens: *cache_write_tokens,
                    cost_usd: *cost_usd,
                    cost_reported_by_provider: *cost_reported_by_provider,
                    at: *at,
                });
            }
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
                //
                // A batch whose every call already carried a result at dispatch
                // had nothing in the tool lane, so there is nothing a resume
                // could finish: it is re-issued, and re-issuing is the only
                // correct thing to do with it. Replaying it instead would land a
                // turn saying `context_write: ok` over a region the write never
                // reached, because a replay puts recorded results in the
                // conversation and does not redo a context tool's write.
                folded.pending_batch =
                    calls
                        .iter()
                        .any(|call| call.result.is_none())
                        .then(|| PendingToolBatch {
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
            RunRecord::ContextChange { .. } | RunRecord::ContextTransaction { .. } => {
                // Both record a committed change, and a reader asking why a
                // region moved should not have to know which shape the build
                // that wrote the journal used. `change_of` is the one place that
                // knows, so the two can never be folded into different stories.
                folded
                    .context_changes
                    .extend(transaction::change_of(record));
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

#[cfg(test)]
mod tests {
    use super::*;
    // The tests are the only writers through the trait; the codec is its own
    // module and writes through its own.
    use crate::ContextCause;
    use crate::run_meta::RunStatus;
    use std::io::{self, Read, Write};
    use std::ops::ControlFlow;

    fn identity() -> RunIdentity {
        RunIdentity {
            run_id: "run-1".to_string(),
            machine_id: "machine-a".to_string(),
            world_id: "world-x".to_string(),
            created_at: 100,
        }
    }

    /// The instant the fixture pretends it is, on every construction.
    ///
    /// Arbitrary, and deliberately not the wall clock: `RunMeta::new` stamps
    /// `started_at`/`updated_at` from it, and these tests build the fixture
    /// once to write and again to compare against. Two reads straddling a
    /// second boundary produced two unequal `RunMeta`s, which failed whichever
    /// round-trip assertion happened to span the tick.
    const FIXTURE_NOW: i64 = 1_700_000_000;

    fn meta() -> RunMeta {
        let mut meta = RunMeta::new(
            "run-1".to_string(),
            "coder".to_string(),
            "/agents/coder".to_string(),
            "do it".to_string(),
            Some("anthropic/claude".to_string()),
            "/work".to_string(),
            2,
        );
        meta.started_at = FIXTURE_NOW;
        meta.updated_at = FIXTURE_NOW;
        meta
    }

    /// Two constructions of the fixture are equal however much time passes
    /// between them. This is the property every round-trip assertion in this
    /// module rests on, and the one a wall-clock stamp quietly broke.
    #[test]
    fn the_fixture_does_not_move_with_the_clock() {
        let first = meta();
        let mut later = meta();
        // Rather than sleeping across a real second boundary, move the clock
        // the way it would have moved: an unpinned fixture differs by exactly
        // this, and a pinned one is rebuilt identically.
        assert_eq!(first, later, "the fixture is rebuilt identically");
        later.started_at += 1;
        assert_ne!(
            first, later,
            "and the comparison is sensitive to the field that used to drift"
        );
    }

    fn entry(content: &str, tokens: usize) -> RegionEntrySnapshot {
        RegionEntrySnapshot {
            content: content.into(),
            tokens,
            kind: crate::region::EntryKind::Text,
            metadata: None,
            key: None,
            taint: Default::default(),
            reasoning: None,
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
            description: None,
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

    // ── streaming point replay ──

    /// Frame `records` exactly as `run.lvr` stores them.
    fn framed(records: &[RunRecord]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_archive_start(&mut buf, RUN_ARCHIVE_VERSION).unwrap();
        for record in records {
            write_record(&mut buf, record).unwrap();
        }
        buf
    }

    /// Collect `(index, at, total_tokens)` per visited point, or the stream
    /// error. One closure shared by every streamed test, including the
    /// bad-preamble one whose visitor never runs.
    fn try_collect_streamed(bytes: &[u8]) -> io::Result<Vec<(usize, i64, usize)>> {
        let mut seen = Vec::new();
        visit_archive_points(&mut &bytes[..], &mut |p| {
            seen.push((p.index, p.at, p.context.total_tokens));
            ControlFlow::Continue(())
        })?;
        Ok(seen)
    }

    /// Collect `(index, at, total_tokens)` per visited point.
    fn collect_streamed(bytes: &[u8]) -> Vec<(usize, i64, usize)> {
        try_collect_streamed(bytes).unwrap()
    }

    #[test]
    fn visit_archive_points_matches_visit_points() {
        let records = vec![
            header(),
            RunRecord::ContextCheckpoint {
                snapshot: snapshot("s1", vec![region("conv", vec![entry("hi", 1)])]),
                at: 10,
            },
            RunRecord::StatusChanged {
                status: RunStatus::Running,
                at: 11,
            },
            RunRecord::Progress {
                meta: Box::new(meta()),
                delta: diff_context(
                    &snapshot("s1", vec![region("conv", vec![entry("hi", 1)])]),
                    &snapshot(
                        "s1",
                        vec![region("conv", vec![entry("hi", 1), entry("more", 2)])],
                    ),
                ),
                at: 12,
            },
        ];
        let mut in_memory = Vec::new();
        visit_points(&records, &mut |p| {
            in_memory.push((p.index, p.at, p.context.total_tokens));
            ControlFlow::Continue(())
        });
        assert_eq!(collect_streamed(&framed(&records)), in_memory);
        assert_eq!(in_memory.len(), 2, "checkpoint + progress = two points");
    }

    #[test]
    fn visit_archive_points_rejects_a_bad_preamble() {
        assert!(try_collect_streamed(b"not an archive at all").is_err());
    }

    #[test]
    fn visit_archive_points_is_lenient_about_a_torn_tail() {
        let records = vec![
            header(),
            RunRecord::ContextCheckpoint {
                snapshot: snapshot("s1", vec![region("conv", vec![entry("hi", 1)])]),
                at: 10,
            },
        ];
        let mut bytes = framed(&records);
        // A torn frame: a length prefix promising more than exists.
        bytes.extend_from_slice(&1000u64.to_be_bytes());
        bytes.extend_from_slice(b"partial");
        assert_eq!(collect_streamed(&bytes).len(), 1, "points before the tear");
    }

    #[test]
    fn visit_archive_points_visits_nothing_without_a_header() {
        let records = vec![RunRecord::ContextCheckpoint {
            snapshot: snapshot("s1", vec![region("conv", vec![entry("hi", 1)])]),
            at: 10,
        }];
        assert!(collect_streamed(&framed(&records)).is_empty());
        // And an archive with no records at all visits nothing.
        assert!(collect_streamed(&framed(&[])).is_empty());
    }

    #[test]
    fn visit_archive_points_stops_on_break() {
        let records = vec![
            header(),
            RunRecord::ContextCheckpoint {
                snapshot: snapshot("s1", vec![region("conv", vec![entry("a", 1)])]),
                at: 10,
            },
            RunRecord::ContextCheckpoint {
                snapshot: snapshot("s1", vec![region("conv", vec![entry("b", 2)])]),
                at: 11,
            },
        ];
        let bytes = framed(&records);
        let mut seen = 0;
        visit_archive_points(&mut &bytes[..], &mut |_| {
            seen += 1;
            ControlFlow::Break(())
        })
        .unwrap();
        assert_eq!(seen, 1);
    }

    // ── digest-based diffing ──
    //
    // `diff_context_digest(digest(a), b)` must produce the same delta as
    // `diff_context(a, b)` for every shape: the persistence lane retains only
    // the digest, and any divergence would silently corrupt the archive.

    fn assert_digest_matches_full_diff(a: &ContextSnapshot, b: &ContextSnapshot) {
        let via_digest = diff_context_digest(&digest_context(a), b);
        assert_eq!(via_digest, diff_context(a, b));
        // And the digest-produced delta still round-trips.
        let mut base = a.clone();
        apply_delta(&mut base, &via_digest);
        assert_eq!(&base, b);
    }

    /// The folded fingerprint says the same thing the digest does, in one
    /// string: two windows the digest calls identical fold together, and any
    /// change the digest notices moves it.
    #[test]
    fn the_folded_fingerprint_follows_the_digest_it_is_folded_from() {
        let a = snapshot("s1", vec![region("conv", vec![entry("hi", 1)])]);
        let same = snapshot("s1", vec![region("conv", vec![entry("hi", 1)])]);
        let grown = snapshot(
            "s1",
            vec![region("conv", vec![entry("hi", 1), entry("there", 2)])],
        );
        let renamed = snapshot("s1", vec![region("plan", vec![entry("hi", 1)])]);
        let print = |snap: &ContextSnapshot| digest_context(snap).fingerprint();
        assert_eq!(print(&a).len(), 16);
        assert_eq!(print(&a), print(&same));
        assert_ne!(print(&a), print(&grown));
        assert_ne!(print(&a), print(&renamed));
        // An empty window still fingerprints, so a record never has to choose
        // between a fingerprint and a window that held nothing.
        assert_eq!(ContextDigest::default().fingerprint().len(), 16);
    }

    #[test]
    fn digest_diff_append_only_growth_is_compact() {
        let a = snapshot("s1", vec![region("conv", vec![entry("hi", 1)])]);
        let b = snapshot(
            "s1",
            vec![region("conv", vec![entry("hi", 1), entry("there", 2)])],
        );
        let delta = diff_context_digest(&digest_context(&a), &b);
        assert_eq!(region_delta_kind(&delta.regions[0]), "append");
        assert_digest_matches_full_diff(&a, &b);
    }

    #[test]
    fn digest_diff_new_cleared_removed_and_rewritten_regions() {
        let a = snapshot(
            "s1",
            vec![
                region("conv", vec![entry("hi", 1)]),
                region("gone", vec![entry("bye", 1)]),
                region("wiped", vec![entry("w", 1)]),
                region("rewritten", vec![entry("old", 1)]),
            ],
        );
        let b = snapshot(
            "s1",
            vec![
                region("conv", vec![entry("hi", 1)]),
                region("wiped", vec![]),
                region("rewritten", vec![entry("new", 1)]),
                region("fresh", vec![entry("f", 2)]),
            ],
        );
        let delta = diff_context_digest(&digest_context(&a), &b);
        let kinds: Vec<_> = delta.regions.iter().map(region_delta_kind).collect();
        assert_eq!(kinds, vec!["clear", "set", "set", "remove"]);
        assert_digest_matches_full_diff(&a, &b);
    }

    #[test]
    fn digest_diff_unchanged_region_emits_nothing() {
        let a = snapshot("s1", vec![region("conv", vec![entry("hi", 1)])]);
        let delta = diff_context_digest(&digest_context(&a), &a.clone());
        assert!(delta.regions.is_empty());
        assert_digest_matches_full_diff(&a, &a.clone());
    }

    #[test]
    fn digest_diff_kind_change_is_set_not_append() {
        let a = snapshot("s1", vec![region("conv", vec![entry("hi", 1)])]);
        let mut grown = region("conv", vec![entry("hi", 1), entry("more", 1)]);
        grown.kind = "sliding".to_string();
        let b = snapshot("s1", vec![grown]);
        let delta = diff_context_digest(&digest_context(&a), &b);
        assert_eq!(region_delta_kind(&delta.regions[0]), "set");
        assert_digest_matches_full_diff(&a, &b);
    }

    /// A token-count change with identical entries is still an (empty) Append
    /// carrying the new count, exactly as the full diff records it.
    #[test]
    fn digest_diff_token_recount_is_an_empty_append() {
        let a = snapshot("s1", vec![region("conv", vec![entry("hi", 1)])]);
        let mut recounted = region("conv", vec![entry("hi", 1)]);
        recounted.current_tokens = 42;
        let b = snapshot("s1", vec![recounted]);
        let delta = diff_context_digest(&digest_context(&a), &b);
        assert_eq!(region_delta_kind(&delta.regions[0]), "append");
        assert_digest_matches_full_diff(&a, &b);
    }

    /// Every field of an entry participates in its digest: a change anywhere
    /// must change the hash, or a real edit would fold as "unchanged".
    #[test]
    fn entry_digest_covers_every_field() {
        let base = entry("text", 1);
        let variants = [
            entry("other", 1),
            entry("text", 2),
            RegionEntrySnapshot {
                key: Some("k".to_string()),
                ..entry("text", 1)
            },
            RegionEntrySnapshot {
                metadata: Some(serde_json::json!({"a": 1})),
                ..entry("text", 1)
            },
            RegionEntrySnapshot {
                kind: crate::region::EntryKind::ToolResult {
                    tool_call_id: "c1".to_string(),
                    tool_name: "shell".to_string(),
                    is_error: false,
                },
                ..entry("text", 1)
            },
        ];
        let base_hash = entry_digest(&base);
        for variant in &variants {
            assert_ne!(
                entry_digest(variant),
                base_hash,
                "field change must change the digest: {variant:?}"
            );
        }
        // And digesting the same entry twice is stable.
        assert_eq!(entry_digest(&base), entry_digest(&entry("text", 1)));
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
            RunRecord::InferenceUsage {
                kind: InferenceKind::Compaction,
                stage: "plan".to_string(),
                iteration: 2,
                provider: "anthropic".to_string(),
                model: "claude-sonnet-5".to_string(),
                prompt_tokens: 7000,
                completion_tokens: 70,
                cached_tokens: 12,
                cache_write_tokens: 34,
                cost_usd: None,
                cost_reported_by_provider: None,
                at: 102,
            },
            RunRecord::InferenceAttempt(AttemptRecord {
                id: "a0001".to_string(),
                stage: "plan".to_string(),
                attempt: 1,
                provider: "anthropic".to_string(),
                model: "claude-sonnet-5".to_string(),
                outcome: AttemptOutcome::Failed {
                    kind: "server-error".to_string(),
                    transient: true,
                    capacity: false,
                    next: Retry::SameModel,
                },
                duration_ms: 1_200,
                backoff_ms: 0,
                digest: RequestDigest {
                    system_hash: 99,
                    messages: 4,
                    tools: 1,
                    max_tokens: 1024,
                    temperature: 0.7,
                },
                model_input: Some(ModelInput {
                    capture_status: CaptureStatus::Retained,
                    request: Some(serde_json::json!({ "model": "claude-sonnet-5" })),
                    bytes: 31,
                    source_context_digest: "0123456789abcdef".to_string(),
                    parameters: [("temperature".to_string(), serde_json::json!(0.7))]
                        .into_iter()
                        .collect(),
                    tool_catalog_version: "fedcba9876543210".to_string(),
                    assembly_version: "1".to_string(),
                }),
                at: 102,
            }),
            RunRecord::InferenceFailover(FailoverRecord {
                stage: "plan".to_string(),
                iteration: 2,
                from_provider: "anthropic".to_string(),
                from_model: "claude-sonnet-5".to_string(),
                to_provider: "openai".to_string(),
                to_model: "gpt-5.5".to_string(),
                reason: "unreachable".to_string(),
                kind: "timeout".to_string(),
                at: 102,
            }),
            RunRecord::ToolBatch {
                // Dispatched to the lane, so it goes out pending and the
                // completion record below is what fills it. A call the
                // dispatcher had already answered would not be waiting on
                // anything, and a resume has nothing to finish for one.
                calls: vec![ToolCallRecord {
                    execution_id: String::new(),
                    id: "c1".to_string(),
                    name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                    result: None,
                    thought_signature: Some("sig".to_string()),
                }],
                at: 103,
                stage_index: 0,
                iteration: 0,
                visit_id: String::new(),
                requested_by: String::new(),
                response: "reading".to_string(),
            },
            RunRecord::ToolCallDone {
                execution_id: String::new(),
                outcome: None,
                iteration: 0,
                call_id: "c1".to_string(),
                result: "body".to_string().into(),
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

    /// The preamble round-trips the version it was written with. Previously
    /// checked with an arbitrary 7; that now names a framing generation this
    /// build cannot read, and is refused - see
    /// `an_archive_from_a_newer_format_is_refused_with_both_versions_named`.
    #[test]
    fn read_archive_start_reports_version() {
        let mut buf = Vec::new();
        write_archive_start(&mut buf, RUN_ARCHIVE_VERSION).unwrap();
        assert_eq!(
            read_archive_start(&mut buf.as_slice()).unwrap(),
            RUN_ARCHIVE_VERSION
        );
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

    /// A torn *length prefix* is where a nonsense `u64` comes from, and the
    /// lenient reader exists precisely to survive a torn tail. Taking the
    /// length at its word would turn a crash-truncated archive into an
    /// allocation of that size - during daemon recovery, the one moment this
    /// reader is there to keep working.
    #[test]
    fn an_absurd_frame_length_is_an_error_not_an_allocation() {
        let mut buf = Vec::new();
        write_archive_start(&mut buf, RUN_ARCHIVE_VERSION).unwrap();
        write_record(&mut buf, &header()).unwrap();
        // A crash mid-append that left a garbage length behind.
        buf.extend_from_slice(&u64::MAX.to_be_bytes());

        let err = read_archive(&mut buf.as_slice())
            .expect_err("the strict reader must refuse an impossible frame");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");

        // And the lenient reader folds back to the intact record before it,
        // which is the behaviour recovery depends on.
        let (_, records) = read_archive_lenient(&mut buf.as_slice()).unwrap();
        assert_eq!(records, vec![header()]);
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
        // Counters. Two inferences: the fixture carries one record of each
        // kind, and both name one provider call.
        assert_eq!(folded.inference_count, 2);
        assert_eq!(folded.inference_usage.len(), 1);
        assert_eq!(folded.tool_call_count, 1);
        // The trips to the provider, and the one move to another. Kept beside
        // the usage rather than merged into it: the usage record is the invoice
        // for the call that worked, and these are what it took to get it.
        assert_eq!(folded.attempts.len(), 1);
        assert_eq!(folded.attempts[0].attempt, 1);
        assert_eq!(folded.attempts[0].digest.messages, 4);
        assert_eq!(folded.failovers.len(), 1);
        assert_eq!(folded.failovers[0].from_provider, "anthropic");
        assert_eq!(folded.failovers[0].to_provider, "openai");
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

    /// A submitted answer needs no record type of its own: `Progress` and
    /// `Checkpoint` both replace the whole `RunMeta`, so it folds along with
    /// everything else and a crash-resume finds the answer already there.
    #[test]
    fn fold_carries_a_submitted_final_output_through_progress() {
        let mut answered = meta();
        answered.final_output = Some(
            crate::output::FinalOutput::new(
                "renamed two helpers",
                Some("markdown".to_string()),
                "summary".to_string(),
                9,
            )
            .descriptor(),
        );
        answered.output_request = Some(crate::output::OutputSpec {
            format: Some("a2ui".to_string()),
            ..Default::default()
        });
        let records = vec![
            header(),
            RunRecord::Progress {
                meta: Box::new(answered),
                delta: ContextDelta {
                    stage_name: "summary".to_string(),
                    total_tokens: 0,
                    max_tokens: 10_000,
                    regions: vec![],
                },
                at: 2,
            },
        ];
        let folded = fold(&records).unwrap();
        let output = folded.meta.final_output.expect("the answer folded through");
        // The descriptor, not the bytes: the answer itself is a sidecar file,
        // so what folds is the record of it.
        assert_eq!(output.bytes, "renamed two helpers".len());
        assert_eq!(output.stage, "summary");
        assert_eq!(
            folded.meta.output_request.and_then(|s| s.format).as_deref(),
            Some("a2ui")
        );
    }

    // ── pending tool batch (fold) ──

    fn call(id: &str, result: Option<&str>) -> ToolCallRecord {
        ToolCallRecord {
            execution_id: String::new(),
            id: id.to_string(),
            name: "shell".to_string(),
            arguments: "{}".to_string(),
            result: result.map(Into::into),
            thought_signature: None,
        }
    }

    fn batch(iteration: usize, calls: Vec<ToolCallRecord>) -> RunRecord {
        RunRecord::ToolBatch {
            calls,
            at: 10,
            stage_index: 0,
            iteration,
            visit_id: String::new(),
            requested_by: String::new(),
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
                execution_id: String::new(),
                outcome: None,
                iteration: 0,
                call_id: "c1".to_string(),
                result: "ran".to_string().into(),
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

    /// Folding a journal gathers why each region moved, in order.
    ///
    /// The snapshots beside these say what a region held; only this says what
    /// moved it, and a reader asking "what emptied the plan" has nothing else
    /// to go on.
    #[test]
    fn folding_gathers_why_each_region_moved() {
        use crate::ContextCause;

        let moved = |region: &str, before: usize, after: usize, added| RegionCommit {
            region: region.to_string(),
            digest_before: format!("rg1-{before:032x}"),
            digest_after: format!("rg1-{after:032x}"),
            tokens_before: before * 10,
            tokens_after: after * 10,
            entries_before: before,
            entries_after: after,
            entries_added: added,
        };
        let committed = |cause, regions, at| RunRecord::ContextTransaction {
            revision_before: format!("cw1-{at:032x}"),
            revision_after: format!("cw1-{:032x}", at + 1),
            cause,
            regions,
            execution_id: String::new(),
            at,
        };
        let records = vec![
            header(),
            committed(ContextCause::Seed, vec![moved("plan", 0, 1, 1)], 20),
            committed(
                ContextCause::ToolResult,
                vec![moved("conversation", 0, 2, 2)],
                21,
            ),
            // A compaction is the case the record exists for: it summarises one
            // region into another and empties the first, and the two halves are
            // one transaction rather than two events that share a second.
            committed(
                ContextCause::Compaction,
                vec![moved("plan", 6, 0, 0), moved("plan_history", 0, 1, 1)],
                22,
            ),
        ];
        let folded = fold(&records).expect("a journal with a header folds");

        assert_eq!(folded.context_changes.len(), 3);
        assert_eq!(folded.context_changes[0].regions[0].region, "plan");
        assert_eq!(folded.context_changes[0].cause, ContextCause::Seed);
        assert_eq!(folded.context_changes[0].regions[0].entries_added, 1);
        assert_eq!(folded.context_changes[0].regions[0].token_delta, 10);
        assert_eq!(folded.context_changes[0].at, 20);
        assert_eq!(
            folded.context_changes[0].revision_after.as_deref(),
            Some(format!("cw1-{:032x}", 21).as_str()),
            "a transaction names the window it produced"
        );
        assert_eq!(folded.context_changes[1].cause, ContextCause::ToolResult);
        let compacted = &folded.context_changes[2];
        assert_eq!(compacted.cause, ContextCause::Compaction);
        assert_eq!(compacted.regions.len(), 2, "both halves, in one record");
        assert_eq!(compacted.regions[0].entries_removed, 6);
        assert_eq!(
            compacted.regions[0].token_delta, -60,
            "a region that shrank reads as a loss, not as an absence"
        );
        assert_eq!(compacted.regions[1].region, "plan_history");
        // A change is not a turn: neither counter above moves for one.
        assert_eq!(folded.inference_count, 0);
        assert_eq!(folded.tool_call_count, 0);
    }

    /// Folding a journal gathers every question the run asked, in order.
    ///
    /// The fold is how a reader lists them, and the only reason the record is
    /// worth writing: a granted approval leaves nothing else behind.
    #[test]
    fn folding_gathers_every_question_the_run_asked() {
        use crate::interaction::{ApprovalScope, InteractionKind, Settlement};

        let asked = |id: &str, settlement: Settlement, at: i64| RunRecord::Interaction {
            request_id: id.to_string(),
            kind: InteractionKind::ToolApproval,
            tool: Some("shell".to_string()),
            prompt: format!("Run {id}?"),
            stage: "plan".to_string(),
            settlement,
            asked_at: at,
            at: at + 1,
        };
        let records = vec![
            header(),
            asked(
                "approve-1",
                Settlement::Answered {
                    approved: Some(true),
                    scope: Some(ApprovalScope::Stage),
                    choice: Some(0),
                    text: None,
                    feedback: None,
                },
                20,
            ),
            asked("approve-2", Settlement::TimedOut, 30),
            asked("approve-3", Settlement::Cancelled, 40),
        ];
        let folded = fold(&records).expect("a journal with a header folds");

        assert_eq!(folded.interactions.len(), 3);
        assert_eq!(folded.interactions[0].request_id, "approve-1");
        assert_eq!(folded.interactions[0].tool.as_deref(), Some("shell"));
        assert_eq!(folded.interactions[0].stage, "plan");
        assert_eq!(folded.interactions[0].prompt, "Run approve-1?");
        assert_eq!(folded.interactions[0].asked_at, 20);
        assert_eq!(folded.interactions[0].at, 21);
        assert!(matches!(
            folded.interactions[0].settlement,
            Settlement::Answered {
                approved: Some(true),
                scope: Some(ApprovalScope::Stage),
                ..
            }
        ));
        // The three ways one can end stay three, because a caller cannot tell
        // them apart from the answer it was handed.
        assert_eq!(folded.interactions[1].settlement, Settlement::TimedOut);
        assert_eq!(folded.interactions[2].settlement, Settlement::Cancelled);
        // And a question is not a tool call, however much it looks like one.
        assert_eq!(folded.tool_call_count, 0);
    }

    /// A journal of single-region change records folds, in order, with the
    /// causes intact.
    ///
    /// This is the shape a journal written before transactions holds, and the
    /// fold has to keep reading it: a run paused by one build and resumed by
    /// another has a journal of both shapes, and a reader that understood only
    /// the newer one would report a run whose history began halfway through.
    #[test]
    fn folding_reads_a_journal_of_single_region_changes() {
        let changed = |region: &str, cause: ContextCause, added, removed, delta, at| {
            RunRecord::ContextChange {
                region: region.to_string(),
                cause,
                entries_added: added,
                entries_removed: removed,
                token_delta: delta,
                at,
            }
        };
        let records = vec![
            header(),
            changed("plan", ContextCause::Seed, 1, 0, 40, 10),
            changed("plan", ContextCause::ContextTool, 1, 1, -5, 20),
            changed("plan", ContextCause::Compaction, 0, 3, -120, 30),
        ];
        let folded = fold(&records).expect("a journal with a header folds");

        let causes: Vec<ContextCause> = folded.context_changes.iter().map(|c| c.cause).collect();
        assert_eq!(
            causes,
            vec![
                ContextCause::Seed,
                ContextCause::ContextTool,
                ContextCause::Compaction
            ]
        );
        let compaction = &folded.context_changes[2];
        assert_eq!(compaction.regions.len(), 1, "one region is all it recorded");
        assert_eq!(compaction.regions[0].region, "plan");
        assert_eq!(compaction.regions[0].entries_added, 0);
        assert_eq!(compaction.regions[0].entries_removed, 3);
        assert_eq!(compaction.regions[0].token_delta, -120);
        assert_eq!(compaction.at, 30);
        // And nothing is invented around them: these records named no window and
        // digested no region, so a reader is told so rather than guessing.
        assert_eq!(compaction.revision_before, None);
        assert_eq!(compaction.revision_after, None);
        assert_eq!(compaction.regions[0].digest_after, None);
        assert_eq!(compaction.regions[0].tokens_after, None);
        // A change is not a point in the timeline: the snapshot beside it
        // already carries the window it produced.
        assert!(replay_points(&records).is_empty());
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
                execution_id: String::new(),
                outcome: None,
                iteration: 0,
                call_id: "c1".to_string(),
                result: "stale".to_string().into(),
                at: 12,
            },
            RunRecord::ToolCallDone {
                execution_id: String::new(),
                outcome: None,
                iteration: 1,
                call_id: "unknown".to_string(),
                result: "nowhere to land".to_string().into(),
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

    /// A folded run carries no artifact records.
    ///
    /// They are neither state a resume needs nor a total anyone asks a folded run
    /// for: which execution made which file is a question about the run's
    /// executions, and the executions listing is where it is answered. Folding
    /// them into anything here would be a second answer to disagree with that one.
    #[test]
    fn folding_passes_over_the_files_an_execution_produced() {
        let records = vec![
            header(),
            RunRecord::ArtifactsProduced {
                execution_id: "x1".to_string(),
                artifacts: vec![crate::output::Artifact {
                    name: "report".to_string(),
                    path: "out/report.md".to_string(),
                    mime_type: crate::mime::MimeType::parse("text/markdown").expect("a type"),
                    size: 12,
                    sha256: "beef".to_string(),
                }],
                at: 30,
            },
        ];
        let folded = fold(&records).expect("a journal with a header folds");
        assert_eq!(folded.tool_call_count, 0, "a file is not a call");
        assert!(folded.context_changes.is_empty());
        assert!(folded.pending_batch.is_none());
    }

    /// A batch every call of which was resolved at dispatch is not pending.
    ///
    /// It had nothing in the tool lane, so a resume has nothing to finish - and
    /// replaying it would be worse than re-issuing it. A replay lands the
    /// recorded results in the conversation without redoing a context tool's
    /// write, so the restored turn would say `context_write: ok` over a region
    /// that never received the content.
    #[test]
    fn fold_does_not_make_a_batch_it_resolved_itself_pending() {
        let records = vec![header(), batch(0, vec![call("c1", Some("wrote the plan"))])];
        assert_eq!(fold(&records).unwrap().pending_batch, None);
        assert_eq!(
            fold(&records).unwrap().tool_call_count,
            1,
            "it is still a call the run made"
        );
        // And such a batch retires the one before it: only the newest batch can
        // still be in flight, however the newest one was resolved.
        let after = vec![
            header(),
            batch(0, vec![call("c1", None)]),
            batch(0, vec![call("c2", Some("wrote the plan"))]),
        ];
        assert_eq!(fold(&after).unwrap().pending_batch, None);
    }

    #[test]
    fn fold_clears_a_batch_once_the_iteration_moves_on() {
        // A later inference bumped meta.iteration past the batch: the batch was
        // applied (even if a sliding window evicted the turn), nothing to replay.
        let mut advanced = meta();
        advanced.iteration = 1;
        let records = vec![
            header(),
            batch(0, vec![call("c1", None)]),
            RunRecord::ToolCallDone {
                execution_id: String::new(),
                outcome: None,
                iteration: 0,
                call_id: "c1".to_string(),
                result: "done".to_string().into(),
                at: 10,
            },
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
            batch(0, vec![call("c1", None)]),
            RunRecord::ToolCallDone {
                execution_id: String::new(),
                outcome: None,
                iteration: 0,
                call_id: "c1".to_string(),
                result: "done".to_string().into(),
                at: 10,
            },
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
                visit_id: String::new(),
                requested_by: String::new(),
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
        {
            let mut count = |_: PointRef<'_>| {
                visits += 1;
                ControlFlow::Continue(())
            };

            // A well-formed journal first, with the *same* visitor. Without
            // this the test would pass against a visitor that can never run at
            // all, which is exactly the reassurance it is not meant to give.
            visit_points(&three_point_records(), &mut count);
            // Neither of these starts with a Header, so neither is a replayable
            // journal and neither may produce a point.
            visit_points(&[], &mut count);
            visit_points(
                &[RunRecord::ContextCheckpoint {
                    snapshot: snapshot("plan", vec![]),
                    at: 1,
                }],
                &mut count,
            );
        }
        assert_eq!(visits, 3, "only the well-formed journal produced points");
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
                execution_id: String::new(),
                outcome: None,
                iteration: 0,
                call_id: "c1".to_string(),
                result: "ran".to_string().into(),
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

    /// Each kind has to survive the wire under its own name: the label is what
    /// a consumer groups a token chart by, so a rename that silently reordered
    /// the enum would re-attribute somebody's spend.
    #[test]
    fn every_inference_kind_has_a_distinct_label_and_serialized_name() {
        let all = [
            (InferenceKind::Stage, "stage"),
            (InferenceKind::Compaction, "compaction"),
            (InferenceKind::Title, "title"),
            (InferenceKind::Routing, "routing"),
        ];
        for (kind, label) in all {
            assert_eq!(kind.label(), label);
            assert_eq!(serde_json::to_value(kind).unwrap(), label);
        }
        let labels: std::collections::HashSet<_> = all.iter().map(|(k, _)| k.label()).collect();
        assert_eq!(labels.len(), all.len(), "labels must not collide");
    }

    /// Only stage turns are work the agent asked for. The other three are
    /// machinery the runtime ran on its behalf, which is the split anything
    /// reporting "what did my agent actually do" needs.
    #[test]
    fn only_a_stage_turn_counts_as_stage_work() {
        assert!(InferenceKind::Stage.is_stage_work());
        for kind in [
            InferenceKind::Compaction,
            InferenceKind::Title,
            InferenceKind::Routing,
        ] {
            assert!(
                !kind.is_stage_work(),
                "{kind:?} is machinery, not stage work"
            );
        }
    }

    /// A journal written before the field existed has to read back as stage
    /// work rather than failing to parse - every record in one is a stage turn,
    /// because nothing else was ever written.
    #[test]
    fn a_usage_record_without_a_kind_reads_back_as_stage_work() {
        let json = serde_json::json!({
            "InferenceUsage": {
                "stage": "plan",
                "iteration": 1,
                "provider": "anthropic",
                "model": "claude-sonnet-5",
                "prompt_tokens": 10,
                "completion_tokens": 2,
                "cached_tokens": 0,
                "cache_write_tokens": 0,
                "at": 5,
            }
        });
        // Compared whole rather than destructured: the point is that the
        // missing field defaults and every present one still lands, and a
        // destructure that pulled out `kind` alone would pass even if the rest
        // had been dropped.
        let record: RunRecord = serde_json::from_value(json).unwrap();
        assert_eq!(
            record,
            RunRecord::InferenceUsage {
                kind: InferenceKind::Stage,
                stage: "plan".to_string(),
                iteration: 1,
                provider: "anthropic".to_string(),
                model: "claude-sonnet-5".to_string(),
                prompt_tokens: 10,
                completion_tokens: 2,
                cached_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: None,
                cost_reported_by_provider: None,
                at: 5,
            }
        );
    }

    /// The point of the record. Folding keeps every call in order, so a
    /// consumer can ask what any single one cost - which the cumulative
    /// counters cannot answer, because two calls between two ticks arrive as
    /// their sum.
    #[test]
    fn folding_keeps_each_call_separate_instead_of_summing_them() {
        let usage = |kind, prompt, at| RunRecord::InferenceUsage {
            kind,
            stage: "plan".to_string(),
            iteration: 1,
            provider: "anthropic".to_string(),
            model: "claude-sonnet-5".to_string(),
            prompt_tokens: prompt,
            completion_tokens: 1,
            cached_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: None,
            cost_reported_by_provider: None,
            at,
        };
        // The shape the issue reported: a compaction call and a stage call
        // landing between the same two progress ticks.
        let folded = fold(&[
            header(),
            usage(InferenceKind::Compaction, 7000, 1),
            usage(InferenceKind::Stage, 21_000, 2),
        ])
        .unwrap();

        assert_eq!(folded.inference_count, 2);
        let seen: Vec<_> = folded
            .inference_usage
            .iter()
            .map(|u| (u.kind, u.prompt_tokens))
            .collect();
        assert_eq!(
            seen,
            vec![
                (InferenceKind::Compaction, 7000),
                (InferenceKind::Stage, 21_000)
            ]
        );
        // The whole reason this exists: their sum is 28k, and a reader of the
        // cumulative counter alone would see one 28k request and reasonably ask
        // whether a 32k window had been violated. Neither call came close.
        assert!(
            folded
                .inference_usage
                .iter()
                .all(|u| u.prompt_tokens < 32_000),
            "no single call exceeded the window, and the journal can now prove it"
        );
    }

    /// Every way an attempt can end, and every way the loop can follow a
    /// failure, under the name a reader outside this build actually sees. These
    /// names are the archive's wire format, so they are pinned here rather than
    /// left to whatever the variant happens to be called in Rust.
    #[test]
    fn an_attempts_outcomes_and_follow_ups_keep_their_wire_names() {
        for (outcome, json) in [
            (AttemptOutcome::Succeeded, "\"succeeded\"".to_string()),
            (
                AttemptOutcome::Failed {
                    kind: "timeout".to_string(),
                    transient: true,
                    capacity: false,
                    next: Retry::Reported,
                },
                "{\"failed\":{\"kind\":\"timeout\",\"transient\":true,\"capacity\":false,\
                 \"next\":\"reported\"}}"
                    .to_string(),
            ),
        ] {
            let wire = serde_json::to_string(&outcome).expect("an outcome serializes");
            assert_eq!(wire, json);
            assert_eq!(
                serde_json::from_str::<AttemptOutcome>(&wire).expect("and reads back"),
                outcome
            );
        }
        for (next, json) in [
            (Retry::Reported, "\"reported\""),
            (Retry::SameModel, "\"same_model\""),
            (Retry::RenewedFiles, "\"renewed_files\""),
        ] {
            assert_eq!(serde_json::to_string(&next).expect("serializes"), json);
            assert_eq!(
                serde_json::from_str::<Retry>(json).expect("and reads back"),
                next
            );
        }
    }

    /// The heavy variant and the light one both name one provider call, so a
    /// consumer counting calls should not have to know which the writer chose.
    #[test]
    fn both_inference_record_kinds_count_as_one_call_each() {
        let records = all_record_kinds();
        let folded = fold(&records).unwrap();
        let written = records
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    RunRecord::Inference { .. } | RunRecord::InferenceUsage { .. }
                )
            })
            .count();
        assert_eq!(folded.inference_count, written);
        assert_eq!(folded.inference_usage.len(), 1);
    }

    /// Replaying a journal has to land on exactly the state the run was in.
    ///
    /// Evictable regions are where a replay drifts: a folded `logs` region
    /// holding entries the live agent had already lost, or fewer than it
    /// held. This walks a region through the mutations a temporary
    /// region actually performs (append, evict-oldest, evict-and-append in one
    /// step, clear) and checks the digest -> delta -> apply chain reproduces
    /// every intermediate state exactly.
    #[test]
    fn probe_replay_matches_every_step() {
        fn region(name: &str, entries: &[(&str, usize)]) -> RegionSnapshot {
            RegionSnapshot {
                name: name.to_string(),
                kind: "temporary".to_string(),
                current_tokens: entries.iter().map(|(_, t)| *t).sum(),
                max_tokens: 1000,
                entries: entries
                    .iter()
                    .map(|(c, t)| RegionEntrySnapshot {
                        content: (*c).into(),
                        tokens: *t,
                        key: None,
                        kind: crate::region::EntryKind::Text,
                        metadata: None,
                        taint: crate::taint::TaintLevel::Public,
                        reasoning: None,
                    })
                    .collect(),
                description: None,
            }
        }

        fn snap(entries: &[(&str, usize)]) -> ContextSnapshot {
            let r = region("logs", entries);
            ContextSnapshot {
                stage_name: "s".to_string(),
                total_tokens: r.current_tokens,
                max_tokens: 1000,
                regions: vec![r],
            }
        }

        let steps: Vec<ContextSnapshot> = vec![
            snap(&[]),
            snap(&[("a", 10)]),
            snap(&[("a", 10), ("b", 20)]),
            snap(&[("a", 10), ("b", 20), ("c", 30)]),
            // Evict oldest.
            snap(&[("b", 20), ("c", 30)]),
            // Evict and append in one step - what a temporary region does when
            // an add pushes it over its bound.
            snap(&[("c", 30), ("d", 40)]),
            // Several evictions at once.
            snap(&[("d", 40)]),
            // Repeated identical content, the case a content hash cannot tell
            // apart by value alone.
            snap(&[("d", 40), ("x", 5)]),
            snap(&[("x", 5), ("x", 5)]),
            snap(&[("x", 5)]),
            snap(&[]),
        ];

        // Fold exactly as the lane writes and the reader replays: retain a
        // digest, diff the next snapshot against it, apply to the running base.
        let mut base = steps[0].clone();
        let mut digest = digest_context(&steps[0]);
        for (i, next) in steps.iter().enumerate().skip(1) {
            let delta = diff_context_digest(&digest, next);
            apply_delta(&mut base, &delta);
            assert_eq!(
                base, *next,
                "step {i}: replay drifted from the live state\n  delta was {:?}",
                delta.regions
            );
            digest = digest_context(next);
        }
    }

    /// One frame from a later build, written the way a later build would write
    /// it: correctly framed, with a variant name this one has never heard of.
    fn trailing_message() -> RunRecord {
        RunRecord::Message {
            message: MessageRecord {
                role: "user".to_string(),
                content: "after the unknown".to_string(),
            },
            at: 9,
        }
    }

    /// Returns the archive and the size of the unknown frame's payload, so a
    /// caller can assert on the exact `Frame` it expects back.
    fn archive_with_an_unknown_record() -> (Vec<u8>, usize) {
        let mut buf = Vec::new();
        write_archive_start(&mut buf, RUN_ARCHIVE_VERSION).unwrap();
        write_record(&mut buf, &header()).unwrap();
        let payload =
            serde_json::to_vec(&serde_json::json!({ "SomethingNew": { "whatever": 1 } })).unwrap();
        buf.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        buf.extend_from_slice(&payload);
        write_record(&mut buf, &trailing_message()).unwrap();
        (buf, payload.len())
    }

    /// A record's variant name, read off its serialized form. Avoids a match
    /// whose unreached arms would be uncovered, and pins the wire spelling.
    fn record_kind(record: &RunRecord) -> String {
        serde_json::to_value(record)
            .expect("a RunRecord always serializes")
            .as_object()
            .expect("externally tagged, so an object")
            .keys()
            .next()
            .expect("with exactly one key")
            .clone()
    }

    /// The forward-compatibility guarantee, and the reason it is worth having:
    /// a reader that stopped at the first unknown record and returned the
    /// prefix would truncate the journal for every older reader, so a build
    /// predating `InferenceUsage` would read a 0.3.10 journal as "header, then
    /// nothing" - with no error.
    #[test]
    fn an_unknown_record_kind_is_stepped_over_not_treated_as_the_end() {
        let (buf, _) = archive_with_an_unknown_record();
        let (version, records) = read_archive_lenient(&mut buf.as_slice()).unwrap();
        assert_eq!(version, RUN_ARCHIVE_VERSION);
        let kinds: Vec<String> = records.iter().map(record_kind).collect();
        assert_eq!(
            kinds,
            vec!["Header".to_string(), "Message".to_string()],
            "the header, and the readable record after the gap"
        );
        assert_eq!(records[1], trailing_message(), "intact, not just present");
    }

    /// The streaming reader skips the same way the buffering one does. It has
    /// its own loop, so "both apply the rule" is a claim that needs checking
    /// rather than assuming.
    #[test]
    fn the_streaming_reader_also_steps_over_an_unknown_record() {
        // A context record *after* the unknown frame: the only way to reach it
        // is to step over that frame, so the point count is the proof.
        let (mut buf, _) = archive_with_an_unknown_record();
        write_record(
            &mut buf,
            &RunRecord::ContextCheckpoint {
                snapshot: ContextSnapshot {
                    stage_name: "s".to_string(),
                    total_tokens: 1,
                    max_tokens: 10,
                    regions: vec![],
                },
                at: 11,
            },
        )
        .unwrap();
        let mut points = 0usize;
        visit_archive_points(&mut buf.as_slice(), &mut |_point| {
            points += 1;
            ControlFlow::Continue(())
        })
        .expect("a valid preamble");
        assert_eq!(points, 1, "the walk got past the unknown frame");
    }

    /// The frame reader distinguishes "cannot parse this" from "cannot find
    /// the end of this". Only the second is fatal, and the difference is what
    /// makes stepping over the first safe.
    #[test]
    fn a_frame_reports_whether_its_payload_was_readable() {
        let (buf, unknown_bytes) = archive_with_an_unknown_record();
        let mut r = buf.as_slice();
        read_archive_start(&mut r).unwrap();

        let mut frames = Vec::new();
        while let Some(frame) = read_frame(&mut r).expect("no torn frames here") {
            frames.push(frame);
        }
        assert_eq!(
            frames,
            vec![
                Frame::Record(Box::new(header())),
                // Stepped over, and it says how far.
                Frame::Unreadable {
                    bytes: unknown_bytes
                },
                Frame::Record(Box::new(trailing_message())),
            ],
            "one frame per record, with the unreadable one accounted for rather than ending the read"
        );
    }

    /// A torn tail still ends the read. Skipping is for frames whose bytes are
    /// all present; a truncated one has no known length to step over.
    #[test]
    fn a_torn_frame_still_ends_the_read() {
        let mut buf = Vec::new();
        write_archive_start(&mut buf, RUN_ARCHIVE_VERSION).unwrap();
        write_record(&mut buf, &header()).unwrap();
        // A length prefix promising more than follows.
        buf.extend_from_slice(&999u64.to_be_bytes());
        buf.extend_from_slice(b"not enough");

        let (_, records) = read_archive_lenient(&mut buf.as_slice()).unwrap();
        assert_eq!(records.len(), 1, "everything intact before the tear");
    }

    /// An archive from a future framing generation is refused rather than
    /// misread. Reading it anyway would not fail cleanly - it would take
    /// whatever the length prefixes happened to say and produce nonsense.
    #[test]
    fn an_archive_from_a_newer_format_is_refused_with_both_versions_named() {
        let mut buf = Vec::new();
        write_archive_start(&mut buf, RUN_ARCHIVE_VERSION + 1).unwrap();
        write_record(&mut buf, &header()).unwrap();

        let err = read_archive_lenient(&mut buf.as_slice()).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains(&(RUN_ARCHIVE_VERSION + 1).to_string()),
            "{message}"
        );
        assert!(message.contains("upgrade leviath"), "{message}");
    }

    /// An older archive is read normally: framing has not changed under it, so
    /// the only difference is which record kinds it happens to contain.
    #[test]
    fn an_archive_from_an_older_format_still_reads() {
        let mut buf = Vec::new();
        write_archive_start(&mut buf, 0).unwrap();
        write_record(&mut buf, &header()).unwrap();
        let (version, records) = read_archive_lenient(&mut buf.as_slice()).unwrap();
        assert_eq!(version, 0);
        assert_eq!(records.len(), 1);
    }
}

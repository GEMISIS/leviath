//! Replaying a run journal into the context window it held at each point.
//!
//! Separate from the records themselves because this is the one reader that
//! carries state: every other consumer looks at a record and is done, while a
//! point replay holds a running window and a running [`RunMeta`] and folds each
//! record into them. [`PointFolder`] is that state, and it is shared by the
//! in-memory and the streaming walk so the two cannot disagree about what a
//! record means.

use std::io::{self, Read};
use std::ops::ControlFlow;

use serde::{Deserialize, Serialize};

use crate::run_meta::{ContextSnapshot, RunMeta};

use super::{Frame, RunRecord, apply_delta, read_archive_start, read_frame};

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
    let Some(mut folder) = (match iter.next() {
        Some(first) => PointFolder::start(first),
        None => None,
    }) else {
        return;
    };
    for record in iter {
        if folder.push(record, visit).is_break() {
            return;
        }
    }
}

/// Streaming [`visit_points`] over a framed archive: validate the preamble,
/// then read one record at a time and fold it into the running window - so a
/// multi-megabyte `run.lvr` is walked holding one record and one window in
/// memory, instead of the whole parsed journal (`read_archive` materializes
/// every record first, typically 2-4x the file's bytes as structs).
///
/// Errors only on a bad preamble. Like [`super::read_archive_lenient`], a torn or
/// unreadable frame ends the walk with the points already visited: the tail of
/// a live run's journal can legitimately be mid-append.
pub fn visit_archive_points(
    r: &mut dyn Read,
    visit: &mut dyn FnMut(PointRef<'_>) -> ControlFlow<()>,
) -> io::Result<()> {
    read_archive_start(r)?;
    // The first record has to be a Header, and a Header is a kind every build
    // knows - so an unreadable frame here means this is not a foldable archive.
    let mut folder = match read_frame(r) {
        Ok(Some(Frame::Record(first))) => match PointFolder::start(&first) {
            Some(folder) => folder,
            None => return Ok(()),
        },
        _ => return Ok(()),
    };
    // A record kind from a later build is stepped over rather than ending the
    // walk: it carries no context change this build can apply, and everything
    // after it still does.
    while let Ok(Some(frame)) = read_frame(r) {
        let Frame::Record(record) = frame else {
            continue;
        };
        if folder.push(&record, visit).is_break() {
            return Ok(());
        }
    }
    Ok(())
}

/// The running state of a point replay: the metadata and window in effect,
/// folded record by record. Shared by [`visit_points`] (in-memory records) and
/// [`visit_archive_points`] (streamed records) so the two can never disagree
/// about what a record means.
struct PointFolder {
    meta: RunMeta,
    context: ContextSnapshot,
    index: usize,
}

impl PointFolder {
    /// Start a replay from the first record, which must be the Header -
    /// anything else means this isn't a run journal, and the replay visits
    /// nothing (`None`).
    fn start(first: &RunRecord) -> Option<Self> {
        match first {
            RunRecord::Header { meta, .. } => Some(Self {
                meta: (**meta).clone(),
                context: ContextSnapshot {
                    stage_name: String::new(),
                    total_tokens: 0,
                    max_tokens: 0,
                    regions: Vec::new(),
                },
                index: 0,
            }),
            _ => None,
        }
    }

    /// Fold one record; when it produces a timeline point, lend it to `visit`.
    fn push(
        &mut self,
        record: &RunRecord,
        visit: &mut dyn FnMut(PointRef<'_>) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        let at = match record {
            RunRecord::Header { meta: m, .. } => {
                self.meta = (**m).clone();
                return ControlFlow::Continue(());
            }
            RunRecord::StatusChanged { status, .. } => {
                self.meta.status = status.clone();
                return ControlFlow::Continue(());
            }
            RunRecord::ContextCheckpoint { snapshot, at } => {
                self.context = snapshot.clone();
                *at
            }
            RunRecord::ContextDiff { delta, at } => {
                apply_delta(&mut self.context, delta);
                *at
            }
            RunRecord::Checkpoint {
                meta: m,
                context: c,
                at,
            } => {
                self.meta = (**m).clone();
                self.context = c.clone();
                *at
            }
            RunRecord::Progress { meta: m, delta, at } => {
                self.meta = (**m).clone();
                apply_delta(&mut self.context, delta);
                *at
            }
            // Non-context records don't add a timeline point. Usage included:
            // it says what a call cost, not what the window then held, and
            // emitting a point per call would double the timeline with entries
            // whose context is identical to their neighbour's. The same goes for
            // an attempt and a failover, only more so: a call refused four times
            // would contribute four points showing the window it was refused
            // with, which is the window its neighbours already show.
            RunRecord::OwnershipChanged { .. }
            | RunRecord::Inference { .. }
            | RunRecord::InferenceAttempt(_)
            | RunRecord::InferenceFailover(_)
            | RunRecord::InferenceUsage { .. }
            | RunRecord::ToolBatch { .. }
            | RunRecord::ToolCallDone { .. }
            | RunRecord::ArtifactsProduced { .. }
            | RunRecord::Interaction { .. }
            // A change record adds no point either, and for the plainest
            // reason: it carries no content. The snapshot or diff recorded
            // beside it is the window this change produced, so a point here
            // would show that same window twice.
            | RunRecord::ContextChange { .. }
            | RunRecord::ContextTransaction { .. }
            | RunRecord::Message { .. } => return ControlFlow::Continue(()),
        };
        let flow = visit(PointRef {
            index: self.index,
            at,
            meta: &self.meta,
            context: &self.context,
        });
        self.index += 1;
        flow
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

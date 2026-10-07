//! Cached run history: the recorded context points and the stage-visit
//! timeline derived from them.
//!
//! The `,`/`.` history keys would otherwise re-read and re-replay the entire
//! run file on every keypress; the stage explorer needs the same data plus
//! real visit counts (the stage ledger holds one record per stage, so a
//! revisit is not a row of its own there). This module loads
//! the run file once per run (through an injectable loader, so tests count
//! reads), derives the visit timeline, and refreshes only when the run file
//! has changed, checked on a tick-based TTL while something is looking at it.

use leviath_runtime::runfile::history::RunPoint;

/// One contiguous stay in a stage, derived from the recorded points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StageVisit {
    pub(super) stage: String,
    /// Unix seconds of the first point recorded in this visit.
    pub(super) entered_at: i64,
    /// Unix seconds of the first point of the *next* visit (`None` for the
    /// last visit - it may still be running).
    pub(super) left_at: Option<i64>,
    /// Highest iteration observed during the visit.
    pub(super) iterations: usize,
    /// Index into the points vec of the visit's first point - what the
    /// timeline jumps the context view to.
    pub(super) first_point: usize,
}

/// The cached history of one run.
#[derive(Debug, Clone, Default)]
pub(super) struct RunHistoryCache {
    pub(super) run_id: String,
    pub(super) points: Vec<RunPoint>,
    pub(super) visits: Vec<StageVisit>,
    /// Tick the run file was last found unchanged (or loaded), for the TTL.
    pub(super) checked_at_tick: u64,
    /// The run file's stat when the points were read, so a reload happens only
    /// when it has grown: a finished run's file is read once.
    pub(super) stamp: Option<crate::runstate::FileStamp>,
    /// Each edge the run took, `(from, to)`, from its run file's transition
    /// records. `None` when nothing recorded them.
    pub(super) transitions: Option<Vec<(String, String)>>,
}

impl RunHistoryCache {
    /// The edges the run took, in order: as its run file recorded them, or
    /// when none are recorded, read off its visits, where each
    /// move from one stage to the next stands for the edge between them.
    pub(super) fn taken(&self) -> Vec<(String, String)> {
        match &self.transitions {
            Some(taken) => taken.clone(),
            None => self
                .visits
                .windows(2)
                .map(|pair| (pair[0].stage.clone(), pair[1].stage.clone()))
                .collect(),
        }
    }
}

impl super::state::Dashboard {
    /// Keep `history`, read off `run_id`'s run file at `stamp`, as the
    /// cached history. A run switch drops any browsed position along with
    /// the old run file.
    pub(super) fn keep_history(
        &mut self,
        run_id: &str,
        stamp: Option<crate::runstate::FileStamp>,
        history: crate::runstate::RunHistory,
    ) {
        if self.history.as_ref().is_some_and(|h| h.run_id != run_id) {
            self.context_history_idx = None;
        }
        let crate::runstate::RunHistory {
            points,
            transitions,
        } = history;
        let visits = derive_visits(&points);
        self.history = Some(RunHistoryCache {
            run_id: run_id.to_string(),
            points,
            visits,
            checked_at_tick: self.tick_count,
            stamp,
            transitions,
        });
    }

    /// The stat the held history of `run_id` was read at; `None` when the
    /// history held is another run's, or there is none.
    pub(super) fn held_history(&self, run_id: Option<&str>) -> super::run_loader::Held {
        self.history
            .as_ref()
            .filter(|h| Some(h.run_id.as_str()) == run_id)
            .map(|h| h.stamp)
    }

    /// Whether the draw loop reads `run_id`'s history itself: where no
    /// loader thread does (tests), and for a run file small enough to replay
    /// between two frames. A long run's history is a replay of megabytes of
    /// steps, which would hold up the first frame of the detail view; the
    /// view draws without it and the loader thread hands it over
    /// ([`Self::adopt_history`]).
    pub(super) fn history_on_draw_loop(&self, run_id: &str) -> bool {
        self.run_feed.is_none()
            || (self.history_stamp)(run_id).is_none_or(|s| s.len <= DRAW_LOOP_HISTORY_BYTES)
    }

    /// Whether the detail view is waiting on the loader thread for the
    /// history of its run, which it holds none of yet. The dashboard then
    /// looks for it again soon rather than a whole tick later.
    pub(super) fn owes_history(&self) -> bool {
        let Some(selected) = self.selected_agent().map(|a| a.id.as_str()) else {
            return false;
        };
        self.detail_view
            && !self.history_on_draw_loop(selected)
            && self.held_history(Some(selected)).is_none()
    }

    /// Take the newest history the loader thread read, if one landed;
    /// whether one did.
    pub(super) fn take_fed_history(&mut self) -> bool {
        let Some(history) = self
            .run_feed
            .as_mut()
            .and_then(super::run_loader::RunFeed::take_history)
        else {
            return false;
        };
        self.adopt_history(history);
        true
    }

    /// Take a history the loader thread read. One the cache already holds,
    /// read at the same stat, changes nothing.
    pub(super) fn adopt_history(&mut self, loaded: super::run_loader::LoadedHistory) {
        let held = self
            .history
            .as_ref()
            .is_some_and(|h| h.run_id == loaded.run_id && h.stamp == loaded.stamp);
        if !held {
            self.keep_history(&loaded.run_id, loaded.stamp, loaded.history);
        }
    }

    /// The window of the stage the detail view has selected, when the run
    /// has left it: the last point of the run's history taken in that stage.
    /// `None` for the stage the run is in (its window is the live one), and
    /// for a run whose history is not loaded.
    pub(super) fn selected_stage_context(
        &self,
        agent: &super::types::DashboardAgent,
    ) -> Option<leviath_core::run_meta::ContextSnapshot> {
        let stage = agent.stages.get(self.selected_stage)?;
        if stage.name == agent.stage {
            return None;
        }
        self.history
            .as_ref()
            .filter(|h| h.run_id == agent.id)?
            .points
            .iter()
            .rev()
            .find(|p| p.meta.current_stage == stage.name)
            .map(|p| p.context.clone())
    }
}

/// The largest run file whose history the draw loop replays itself: a few
/// milliseconds of work.
const DRAW_LOOP_HISTORY_BYTES: u64 = 1024 * 1024;

/// Look at the run file no more often than this many ticks (~1s at the 100ms
/// tick rate), and read it again only if it changed.
pub(super) const HISTORY_TTL_TICKS: u64 = 10;

/// Derive the visit timeline: a new visit starts at every point whose
/// `current_stage` differs from the previous point's.
pub(super) fn derive_visits(points: &[RunPoint]) -> Vec<StageVisit> {
    let mut visits: Vec<StageVisit> = Vec::new();
    for (idx, point) in points.iter().enumerate() {
        let stage = point.meta.current_stage.clone();
        match visits.last_mut() {
            Some(last) if last.stage == stage => {
                last.iterations = last.iterations.max(point.meta.iteration);
            }
            other => {
                if let Some(prev) = other {
                    prev.left_at = Some(point.at);
                }
                visits.push(StageVisit {
                    stage,
                    entered_at: point.at,
                    left_at: None,
                    iterations: point.meta.iteration,
                    first_point: idx,
                });
            }
        }
    }
    visits
}

/// `HH:MM:SS` in local time.
pub(super) fn clock(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

/// How many times each stage name appears in the visit timeline.
pub(super) fn visit_count(visits: &[StageVisit], stage: &str) -> usize {
    visits.iter().filter(|v| v.stage == stage).count()
}

/// The last visit of `stage`, if any.
pub(super) fn last_visit<'a>(visits: &'a [StageVisit], stage: &str) -> Option<&'a StageVisit> {
    visits.iter().rev().find(|v| v.stage == stage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::run_meta::{RunMeta, RunStatus};

    fn point(stage: &str, iteration: usize, at: i64) -> RunPoint {
        let mut meta = RunMeta::new(
            "r".to_string(),
            "a".to_string(),
            "/p".to_string(),
            "t".to_string(),
            None,
            "/w".to_string(),
            3,
        );
        meta.current_stage = stage.to_string();
        meta.iteration = iteration;
        meta.status = RunStatus::Running;
        RunPoint {
            meta,
            context: leviath_core::run_meta::ContextSnapshot {
                stage_name: stage.to_string(),
                total_tokens: 0,
                max_tokens: 100,
                regions: Vec::new(),
            },
            at,
        }
    }

    #[test]
    fn visits_split_on_stage_change_and_count_revisits() {
        let points = vec![
            point("plan", 1, 10),
            point("plan", 2, 20),
            point("implement", 1, 30),
            point("review", 1, 40),
            point("implement", 1, 50), // the revisit the stage ledger has no row for
            point("implement", 2, 60),
        ];
        let visits = derive_visits(&points);
        assert_eq!(visits.len(), 4);
        assert_eq!(visits[0].stage, "plan");
        assert_eq!(visits[0].entered_at, 10);
        assert_eq!(visits[0].left_at, Some(30));
        assert_eq!(visits[0].iterations, 2);
        assert_eq!(visits[0].first_point, 0);

        assert_eq!(visits[3].stage, "implement");
        assert_eq!(visits[3].first_point, 4);
        assert_eq!(visits[3].left_at, None, "still running");
        assert_eq!(visits[3].iterations, 2);

        assert_eq!(visit_count(&visits, "implement"), 2);
        assert_eq!(visit_count(&visits, "plan"), 1);
        assert_eq!(visit_count(&visits, "never"), 0);

        assert_eq!(last_visit(&visits, "implement").unwrap().first_point, 4);
        assert!(last_visit(&visits, "never").is_none());
    }

    #[test]
    fn an_empty_archive_derives_an_empty_timeline() {
        assert!(derive_visits(&[]).is_empty());
    }
}

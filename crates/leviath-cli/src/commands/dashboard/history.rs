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

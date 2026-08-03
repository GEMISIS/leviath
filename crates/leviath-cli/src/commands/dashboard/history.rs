//! Cached run history: the archived context points and the stage-visit
//! timeline derived from them.
//!
//! The `,`/`.` history keys used to re-read and re-replay the entire
//! `run.lvr` archive on every keypress; the stage explorer needs the same
//! data plus real visit counts (stages.json holds one record per stage,
//! rewritten in place, so revisits are invisible there). This module loads
//! the archive once per run (through an injectable loader, so tests count
//! reads), derives the visit timeline, and refreshes on a tick-based TTL
//! only while something is actually looking at it.

use leviath_core::run_archive::RunPoint;

/// One contiguous stay in a stage, derived from the archived points.
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

/// The cached archive of one run.
#[derive(Debug, Clone, Default)]
pub(super) struct RunHistoryCache {
    pub(super) run_id: String,
    pub(super) points: Vec<RunPoint>,
    pub(super) visits: Vec<StageVisit>,
    /// Tick the points were loaded at, for the TTL.
    pub(super) loaded_at_tick: u64,
}

/// Reload no more often than this many ticks (~1s at the 100ms tick rate).
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
            point("implement", 1, 50), // the revisit stages.json cannot show
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

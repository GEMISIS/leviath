//! Whether a graph can finish: the warnings a run is started with when some
//! stage it can reach has no way to end the run.
//!
//! These are warnings, not refusals. A graph whose stages loop is sometimes
//! what its author meant (a run someone stops by hand, a loop a revisit cap
//! breaks with an error), so the run starts, and every surface that starts or
//! shows it says loudly which stages can never reach an end.

use std::collections::{BTreeSet, VecDeque};

use super::{EdgeCondition, RunGraph, StageDef, StageMode};
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};

impl RunGraph {
    /// Everything about the graph that may keep a run from ever finishing,
    /// worded for the person starting it. `at` is where the graph sits in the
    /// request, as for [`validate`](Self::validate). Empty for a graph that
    /// can always end.
    pub fn warnings(&self, at: &SpecPath) -> SpawnIssues {
        let mut warnings = SpawnIssues::new();
        self.self_loops(at, &mut warnings);
        self.dead_loops(at, &mut warnings);
        warnings
    }

    /// A stage with an edge back to itself and no `max_revisits`: nothing
    /// stops it taking that edge for ever.
    fn self_loops(&self, at: &SpecPath, warnings: &mut SpawnIssues) {
        let mut seen = BTreeSet::new();
        for edge in self.edges.iter().filter(|e| e.from == e.to) {
            let Some(stage) = self.stage(edge.from.as_str()) else {
                continue;
            };
            if stage.max_revisits.is_some() || !seen.insert(stage.name.as_str()) {
                continue;
            }
            warnings.push(
                SpawnIssue::new(
                    at.field("stages").key(stage.name.as_str()),
                    IssueCode::MayNeverFinish,
                    format!(
                        "stage '{}' has an edge back to itself and no max_revisits, so nothing \
                         stops it looping",
                        stage.name
                    ),
                )
                .hint("set max_revisits on the stage so the loop has to end"),
            );
        }
    }

    /// The stages a run can reach from its entry but from which no path
    /// leads to a stage that ends the run. A run in one of them goes round
    /// until it is stopped, or until a `max_revisits` cap runs out and the
    /// run fails.
    fn dead_loops(&self, at: &SpecPath, warnings: &mut SpawnIssues) {
        let Some(entry) = self.entry_stage() else {
            return;
        };
        let reachable = self.reachable_from(entry);
        let ending = self.can_end();
        let trapped: Vec<&str> = self
            .stages
            .iter()
            .map(|s| s.name.as_str())
            .filter(|name| reachable.contains(name) && !ending.contains(name))
            .collect();
        if trapped.is_empty() {
            return;
        }
        let listed = trapped
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let message = match ending.contains(entry.name.as_str()) {
            false => format!(
                "this run can never finish: no stage it can reach ends the run, so it loops \
                 through {listed} until it is stopped"
            ),
            true => format!(
                "this run may never finish: once it enters {listed}, no path leads to a stage \
                 that ends the run"
            ),
        };
        warnings.push(
            SpawnIssue::new(at.field("edges"), IssueCode::MayNeverFinish, message)
                .hint(
                    "give one of these stages a way out: a stage with no `always` or \
                     `llm_choice` edge leaving it, or allow_complete = true",
                )
                .known(trapped),
        );
    }

    /// The stages a run started at `entry` can reach.
    fn reachable_from<'a>(&'a self, entry: &'a StageDef) -> BTreeSet<&'a str> {
        let mut seen = BTreeSet::from([entry.name.as_str()]);
        let mut queue = VecDeque::from([entry]);
        while let Some(stage) = queue.pop_front() {
            for next in self.next_stages(stage) {
                if seen.insert(next.name.as_str()) {
                    queue.push_back(next);
                }
            }
        }
        seen
    }

    /// The stages from which some path reaches a stage that ends the run.
    fn can_end(&self) -> BTreeSet<&str> {
        let mut ending: BTreeSet<&str> = self
            .stages
            .iter()
            .filter(|s| self.ends_run(s))
            .map(|s| s.name.as_str())
            .collect();
        loop {
            let before = ending.len();
            for stage in &self.stages {
                if self
                    .next_stages(stage)
                    .iter()
                    .any(|n| ending.contains(n.name.as_str()))
                {
                    ending.insert(stage.name.as_str());
                }
            }
            if ending.len() == before {
                return ending;
            }
        }
    }

    /// Whether a run can end in `stage`: no edge the run follows when the
    /// stage ends normally leaves it, or the model may end the run from it.
    /// A fan-out stage with a merge stage hands its run on to that stage, so
    /// it never ends one itself.
    fn ends_run(&self, stage: &StageDef) -> bool {
        let merges = matches!(&stage.mode, StageMode::FanOut(f) if f.merge_stage.is_some());
        let followed = self
            .edges_from(stage.name.as_str())
            .any(|e| matches!(e.when, EdgeCondition::Always | EdgeCondition::LlmChoice));
        !merges && (stage.allow_complete || !followed)
    }

    /// The stages a run in `stage` can move to next: along any of its
    /// edges, and to its merge stage when it is a fan-out with one.
    fn next_stages<'a>(&'a self, stage: &'a StageDef) -> Vec<&'a StageDef> {
        let merge = match &stage.mode {
            StageMode::FanOut(f) => f.merge_stage.as_ref().and_then(|m| self.stage(m.as_str())),
            _ => None,
        };
        self.edges_from(stage.name.as_str())
            .filter_map(|e| self.stage(e.to.as_str()))
            .chain(merge)
            .collect()
    }
}

#[cfg(test)]
#[path = "ends_tests.rs"]
mod tests;

//! What a reader computes from a committed region change.

use super::*;

fn commit(entries_before: usize, entries_after: usize, added: usize) -> RegionCommit {
    RegionCommit {
        region: "conversation".to_string(),
        digest_before: "a".to_string(),
        digest_after: "b".to_string(),
        tokens_before: 100,
        tokens_after: 40,
        entries_before,
        entries_after,
        entries_added: added,
    }
}

/// A region that took one entry and lost two to its own eviction reads as
/// one added, two removed, and the tokens it shed.
#[test]
fn an_eviction_shows_up_in_the_counts_either_side() {
    let t = RegionTransition::from(commit(5, 4, 1));
    assert_eq!(t.entries_added, 1);
    assert_eq!(t.entries_removed, 2);
    assert_eq!(t.token_delta, -60);
    assert_eq!(t.digest_before.as_deref(), Some("a"));
    assert_eq!(t.entries_after, Some(4));
}

/// Counts that do not close report no removal rather than an enormous one.
#[test]
fn counts_that_do_not_close_report_no_removal() {
    assert_eq!(RegionTransition::from(commit(1, 9, 0)).entries_removed, 0);
}

/// An execution with no ending is in flight; one with an ending is not.
#[test]
fn an_execution_without_an_ending_is_unfinished() {
    let mut e = Execution {
        id: "x1".to_string(),
        call_id: "c1".to_string(),
        tool: "read_file".to_string(),
        arguments: "{}".to_string(),
        stage_index: 0,
        iteration: 0,
        visit_id: String::new(),
        requested_by: String::new(),
        artifacts: Vec::new(),
        dispatched_at: 1,
        position: 1,
        ended_at: None,
        result_position: None,
        outcome: None,
    };
    assert!(e.unfinished());
    e.ended_at = Some(2);
    assert!(!e.unfinished());
}

mod step_points {
    use std::ops::ControlFlow;

    use super::super::step_points;
    use crate::spec::names::{RegionName, StageName};
    use crate::state::context::{EntryKind, EntryMeta, EntryState, RegionState};
    use crate::state::{Cursor, RunState, StateDelta};

    fn entry(text: &str) -> EntryState {
        EntryState {
            text: text.into(),
            parts: vec![],
            tokens: 3,
            timestamp: 10,
            kind: EntryKind::Text,
            meta: EntryMeta::None,
            key: None,
            reasoning: None,
        }
    }

    fn region<'a>(state: &'a mut RunState, name: &str) -> &'a mut RegionState {
        let found = state
            .context
            .regions
            .iter_mut()
            .find(|r| r.name.as_str() == name);
        found.unwrap()
    }

    fn texts(state: &RunState, name: &str) -> Vec<String> {
        let found = state
            .context
            .regions
            .iter()
            .find(|r| r.name.as_str() == name);
        found.map_or_else(Vec::new, |r| {
            r.entries.iter().map(|e| e.text.to_string()).collect()
        })
    }

    fn enter(state: &mut RunState, stage: &str) {
        state.cursor = Cursor {
            stage: StageName::new(stage).unwrap(),
            visit: format!("{stage}-1"),
            iteration: 0,
        };
    }

    /// Every state `step_points` hands over for the step from `before` to
    /// `after`, with `before` advanced to `after`.
    fn points(before: &mut RunState, after: &mut RunState) -> Vec<RunState> {
        after.seq = before.seq + 1;
        let delta = StateDelta::between(before, after, 20, vec![]);
        let mut seen = Vec::new();
        let flow = step_points(&delta, before, &mut |s| {
            seen.push(s.clone());
            ControlFlow::Continue(())
        });
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(
            before, &*after,
            "the step is applied whatever is handed over"
        );
        seen
    }

    /// The step that stores a stage's output and moves on lists that output
    /// under the stage that made it, without what entering the next stage
    /// rewrote; the stage entered then has the window as the step left it.
    #[test]
    fn a_stage_keeps_what_it_wrote_on_its_way_out() {
        let mut before = crate::state::tests::base();
        let mut after = before.clone();
        region(&mut after, "conversation")
            .entries
            .push(entry("the render"));
        region(&mut after, "conversation").current_tokens = 3;
        // Entering `build` replaces the instructions.
        region(&mut after, "system").entries = vec![entry("now build")];
        enter(&mut after, "build");

        let mut seen = points(&mut before, &mut after);

        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].cursor.stage.as_str(), "plan");
        assert_eq!(texts(&seen[0], "conversation"), ["the render"]);
        assert_eq!(region(&mut seen[0], "conversation").current_tokens, 3);
        assert_eq!(
            texts(&seen[0], "system"),
            ["be good"],
            "not the next stage's"
        );
        assert_eq!(seen[1], after);
    }

    /// A region the stage entered dropped still shows under the stage that
    /// had it, and one it added does not.
    #[test]
    fn the_stage_left_keeps_its_own_layout() {
        let mut before = crate::state::tests::base();
        let mut after = before.clone();
        region(&mut after, "conversation")
            .entries
            .push(entry("handed on"));
        after
            .context
            .regions
            .retain(|r| r.name.as_str() != "system");
        after.context.regions.push(RegionState {
            name: RegionName::new("scratch").unwrap(),
            max_tokens: 50,
            current_tokens: 3,
            needs_message_compaction: false,
            taint: None,
            entries: vec![entry("fresh")],
        });
        enter(&mut after, "build");

        let seen = points(&mut before, &mut after);

        assert_eq!(texts(&seen[0], "system"), ["be good"]);
        assert!(texts(&seen[0], "scratch").is_empty());
        assert_eq!(texts(&seen[1], "scratch"), ["fresh"]);
    }

    /// A step that stays in its stage is one point, as is a move that added
    /// nothing; a move that left the window alone is none.
    #[test]
    fn only_a_move_that_wrote_something_is_listed_twice() {
        let mut before = crate::state::tests::base();
        let mut after = before.clone();
        region(&mut after, "conversation")
            .entries
            .push(entry("a turn"));
        assert_eq!(points(&mut before, &mut after).len(), 1);

        let mut after = before.clone();
        region(&mut after, "system").entries = vec![entry("now build")];
        enter(&mut after, "build");
        let seen = points(&mut before, &mut after);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].cursor.stage.as_str(), "build");

        let mut after = before.clone();
        enter(&mut after, "ship");
        assert!(points(&mut before, &mut after).is_empty());
    }

    /// A reader that has seen enough stops the step at the first state.
    #[test]
    fn a_reader_can_stop_at_the_stage_left() {
        let mut before = crate::state::tests::base();
        let mut after = before.clone();
        region(&mut after, "conversation")
            .entries
            .push(entry("the render"));
        enter(&mut after, "build");
        let delta = StateDelta::between(&before, &after, 20, vec![]);
        let mut seen = 0;
        let flow = step_points(&delta, &mut before, &mut |_| {
            seen += 1;
            ControlFlow::Break(())
        });
        assert_eq!(flow, ControlFlow::Break(()));
        assert_eq!(seen, 1);
    }
}

//! What the world decides about each tool call before its batch is run.
//!
//! A tool service says how a call is judged ([`ToolService::decide`]), but the
//! judging happens in the world, on the tick, against state the run holds as
//! components: what a person has granted it ([`ToolGrants`]) and what it has
//! written so far ([`WriteLedger`]). A call that needs a person's approval is
//! asked on the approval lane and the batch waits in the world until the
//! answer comes back. The task that runs the batch is handed only the calls
//! and what was decided for each ([`DecidedCall`]): it runs the ones allowed,
//! asks the questions the model put, and reports the refusals.
//!
//! [`ToolService::decide`]: super::ToolService::decide

use std::collections::HashSet;

use bevy_ecs::prelude::*;
use leviath_core::interaction::ApprovalScope;

/// The world's verdict on one tool call, from [`ToolService::decide`].
///
/// [`ToolService::decide`]: super::ToolService::decide
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolVerdict {
    /// Run it, charging the run `charge` bytes against its write budget now:
    /// every call in a batch is decided before any runs, so a write charged
    /// only once it ran would let two writes in one batch both pass a budget
    /// neither had spent.
    Run {
        /// Bytes the call declares it writes.
        charge: u64,
    },
    /// It is a question to a person (`ask_user_*`, `present_for_review`):
    /// put to one when `attended`, answered by the run itself otherwise.
    Interact {
        /// Whether a person answers it.
        attended: bool,
    },
    /// Refused; the text is the call's result.
    Refuse(String),
    /// A person has to approve it first.
    Ask {
        /// What an approval for the stage or the run is remembered under.
        keys: Vec<String>,
        /// Bytes charged to the run if it is approved.
        charge: u64,
    },
}

/// A decided call, as the task that runs the batch receives it.
#[derive(Debug, Clone)]
pub struct DecidedCall {
    /// The call.
    pub call: leviath_providers::ToolCall,
    /// What the world decided.
    pub decision: Decision,
}

/// What the world decided for a call it handed on: a [`ToolVerdict`] with any
/// approval already answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Run it.
    Run,
    /// Ask the question it puts, of a person when `attended`.
    Interact {
        /// Whether a person answers it.
        attended: bool,
    },
    /// Report this as its result without running it.
    Refuse(String),
}

/// What a tool service may read about the run when it decides a call.
pub struct DecideCtx<'a> {
    /// What a person has approved for the run, and for its stage.
    pub grants: &'a ToolGrants,
    /// What the run has written so far, the calls already decided in this
    /// batch included.
    pub written: u64,
    /// The stage the run is in, by index in its graph.
    pub stage_index: usize,
}

/// The approvals a person granted a run beyond the call they were asked
/// about: keys allowed for the rest of the run, and for the stage it is in.
///
/// A stage grant ends when the run moves to a different stage, and survives
/// re-entering the same one: a `plan -> plan` revision loop is the work the
/// person approved, and re-prompting through it would make the scope useless
/// on exactly the stages that revise.
#[derive(Component, Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolGrants {
    /// Keys allowed for the rest of the run.
    run: HashSet<String>,
    /// Keys allowed while the run stays in the stage at `stage_index`.
    stage: HashSet<String>,
    /// The stage `stage` was granted under.
    stage_index: Option<usize>,
}

impl ToolGrants {
    /// Whether `key` has been granted, for the run or the stage.
    pub fn granted(&self, key: &str) -> bool {
        self.run.contains(key) || self.stage.contains(key)
    }

    /// The run entered the stage at `index`: a stage grant made elsewhere ends.
    pub fn enter_stage(&mut self, index: usize) {
        if self.stage_index != Some(index) {
            self.stage_index = Some(index);
            self.stage.clear();
        }
    }

    /// Remember `keys` at the scope a person approved them for. `Once`, no
    /// scope and an empty key list remember nothing: a call that cannot be
    /// characterized is one a later call must not inherit.
    pub fn grant(&mut self, scope: Option<ApprovalScope>, keys: &[String]) {
        let into = match scope {
            Some(ApprovalScope::Stage) => &mut self.stage,
            Some(ApprovalScope::Run) => &mut self.run,
            Some(ApprovalScope::Once) | None => return,
        };
        into.extend(keys.iter().cloned());
    }
}

/// What a run has written, against its `[limits]` write ceilings: the bytes
/// its calls declared when they were allowed, plus what its executors
/// measured once they ran.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteLedger {
    /// Bytes written so far.
    pub written: u64,
}

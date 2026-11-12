//! The stage record of a run file in binary layout 7, which builds before
//! a stage record counted its resubmissions wrote.
//!
//! Layout 7 and layout 8 differ in one record: a stage's, which layout 8
//! gave a `resubmissions` count. The state holds the stage records in its
//! ledger, and a delta holds one in a ledger change, so both are read as
//! layout 8's ([`crate::layout8`]) holding this record in place of its own.
//!
//! Layouts 2 to 7 kept the same stage record, so they read their states and
//! deltas this way too. Layouts 2 and 3 also kept a fan-out in progress in a
//! shape of their own (see [`crate::layout3`]), which is why the state and
//! the change take the fan-out they hold as a parameter.
//!
//! No build counted resubmissions before, so an upgraded stage has none.

use std::collections::BTreeMap;

use leviath_runtime::spec::names::{ModelRef, StageName};
use leviath_runtime::state::{self as state, Clock, StageStatus};
use serde::Deserialize;

use crate::layout8::{Spend, VisitRecord};

/// The fingerprint in the header of every layout-7 run file.
pub(crate) const FINGERPRINT: [u8; 32] = [
    0xbd, 0x94, 0xbd, 0x2a, 0x40, 0xcd, 0x42, 0xad, 0xbc, 0xf8, 0xbc, 0x88, 0x05, 0x69, 0x45, 0x3f,
    0xa2, 0x45, 0x47, 0x55, 0xf5, 0x5c, 0x05, 0x02, 0x84, 0x66, 0xac, 0x52, 0x7d, 0x62, 0x2d, 0x69,
];

/// One stage's record, with layouts 2 to 7's fields.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(crate) struct StageRecord {
    stage: StageName,
    status: StageStatus,
    entered: bool,
    spend: Spend,
    models: Vec<ModelRef>,
    visits: Vec<VisitRecord>,
    region_tokens: BTreeMap<String, u64>,
    first_call_prompt_tokens: Option<u64>,
    runaway_warned: bool,
    output_cap_raised: bool,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    clock: Clock,
}

impl From<StageRecord> for state::StageRecord {
    /// The same record, with no resubmissions counted.
    fn from(r: StageRecord) -> Self {
        Self {
            stage: r.stage,
            status: r.status,
            entered: r.entered,
            spend: r.spend.into(),
            models: r.models,
            visits: r.visits.into_iter().map(Into::into).collect(),
            region_tokens: r.region_tokens,
            first_call_prompt_tokens: r.first_call_prompt_tokens,
            runaway_warned: r.runaway_warned,
            output_cap_raised: r.output_cap_raised,
            resubmissions: 0,
            started_at: r.started_at,
            ended_at: r.ended_at,
            clock: r.clock,
        }
    }
}

/// A run's state as layouts 4 to 7 wrote it: layout 8's, with this layout's
/// stage records, holding a fan-out in progress as `F`.
pub(crate) type RunState<F = state::FanOutState> = crate::layout8::RunState<F, StageRecord>;

/// One changed field of a state as layouts 4 to 7 wrote it.
#[cfg(test)]
pub(crate) type Change<F = state::FanOutState> = crate::layout8::Change<F, StageRecord>;

/// One step of a run's history as layouts 4 to 7 wrote it.
pub(crate) type StateDelta<F = state::FanOutState> = crate::layout8::StateDelta<F, StageRecord>;

#[cfg(test)]
#[path = "layout7_tests.rs"]
mod tests;

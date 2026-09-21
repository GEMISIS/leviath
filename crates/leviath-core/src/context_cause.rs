//! Why a context window changed.
//!
//! A run journal already says what every region held at each point: the
//! snapshots and diffs rebuild the window turn by turn. What they cannot say is
//! what moved it. A region that lost the plan it was holding looks identical
//! whether a compaction summarised it away, a stage-edge transform cleared it,
//! or the model called `context_delete` on it - and those three are a bug in
//! three different places.
//!
//! A [`ContextCause`] is that missing half, recorded beside each change as
//! [`RunRecord::ContextChange`](crate::run_archive::RunRecord::ContextChange).
//! It is deliberately not the runtime's `WriteOrigin`, which answers a
//! different question (whether a region hook's refusal has a tool result to be
//! reported back through): "the model asked for this" and
//! "this is what the model asking looks like in the journal" are not the same
//! fact, and one enum answering both would have to lie about one of them.
//!
//! # Adding a variant
//!
//! A cause names a *path through the runtime*, not a shape of edit. Two paths
//! that both append to the conversation stay two causes if a debugger would
//! ask which of them ran. A path with no variant of its own is left
//! unattributed - no record at all - rather than folded into the nearest
//! neighbour, because a history that mislabels is worse than one that admits a
//! gap.

use serde::{Deserialize, Serialize};

/// What changed a region of a context window.
///
/// Passed explicitly at every write that records one; there is no default,
/// because the one thing this must never do is guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextCause {
    /// A region seeded from the blueprint or from caller input, at spawn or on
    /// entry to a stage that declares its own layout.
    Seed,
    /// A message delivered into the running agent (`lev message`, the API's
    /// message route) landing in the region that accepts messages.
    Message,
    /// The model's own reply, recorded as the assistant turn it was.
    ModelReply,
    /// A tool's answer landing in a region: the conversation by default,
    /// wherever `[stages.<name>.tool_results]` sends it, or the region a tool
    /// writes on purpose (`submit_output` mirroring the answer it was given into
    /// `final_output`).
    ToolResult,
    /// A part the model produced, kept somewhere other than the conversation:
    /// routed by `[stages.<name>.output_routing]`, attached by a mime tool, or
    /// emitted as an artifact.
    ProducedPart,
    /// A compacting region summarising itself: the summary landing in its
    /// history region, and the source region being emptied behind it.
    Compaction,
    /// A stage-edge transform carrying, summarising or clearing a region as a
    /// run moves from one stage to the next, or as a child is seeded from its
    /// parent.
    Transform,
    /// A `context_*` or `todo_*` tool the model called: a write, an append, a
    /// release, a checklist item.
    ContextTool,
    /// A region's own `on_write`/`on_overflow` script, or a stage hook, writing
    /// on the region's behalf.
    Hook,
    /// A fan-out worker: the sources its window is seeded with, and the report
    /// it hands back to its parent.
    FanOut,
    /// An interaction point: the document it publishes for review, and the
    /// selection, directive or edit a person's answer puts in the conversation.
    Interaction,
    /// A resume rebuilding the window from the journal, region by region,
    /// before the run carries on.
    Resume,
    /// The runtime's own bookkeeping: a `[System]` nudge, a watchdog note, the
    /// record a transition choice leaves behind.
    Framework,
}

impl ContextCause {
    /// This cause's name in the journal and on every wire built from it.
    ///
    /// Spelled out rather than derived from the variant name so that renaming a
    /// variant cannot silently rewrite history that is already on disk.
    pub fn wire(&self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::Message => "message",
            Self::ModelReply => "model_reply",
            Self::ToolResult => "tool_result",
            Self::ProducedPart => "produced_part",
            Self::Compaction => "compaction",
            Self::Transform => "transform",
            Self::ContextTool => "context_tool",
            Self::Hook => "hook",
            Self::FanOut => "fan_out",
            Self::Interaction => "interaction",
            Self::Resume => "resume",
            Self::Framework => "framework",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant, so the two tests below cover the whole enum rather than
    /// whichever variants somebody remembered.
    const ALL: &[ContextCause] = &[
        ContextCause::Seed,
        ContextCause::Message,
        ContextCause::ModelReply,
        ContextCause::ToolResult,
        ContextCause::ProducedPart,
        ContextCause::Compaction,
        ContextCause::Transform,
        ContextCause::ContextTool,
        ContextCause::Hook,
        ContextCause::FanOut,
        ContextCause::Interaction,
        ContextCause::Resume,
        ContextCause::Framework,
    ];

    /// The wire name is what a journal written today will still be read by
    /// years from now, so it is pinned here rather than trusted to the derive.
    #[test]
    fn each_cause_serializes_as_its_wire_name() {
        for cause in ALL {
            assert_eq!(
                serde_json::to_value(cause).expect("a plain enum serializes"),
                serde_json::Value::String(cause.wire().to_string()),
                "{cause:?}"
            );
            let back: ContextCause = serde_json::from_value(serde_json::json!(cause.wire()))
                .expect("its own wire name parses back");
            assert_eq!(&back, cause);
        }
    }

    /// Two causes sharing a wire name would make the journal ambiguous in
    /// exactly the way the whole record exists to avoid.
    #[test]
    fn no_two_causes_share_a_wire_name() {
        let mut seen = std::collections::HashSet::new();
        for cause in ALL {
            assert!(seen.insert(cause.wire()), "{cause:?} repeats a wire name");
        }
        assert_eq!(seen.len(), ALL.len());
    }
}

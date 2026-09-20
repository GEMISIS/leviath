//! The checkpoints a stage raises, and what each answer does.

use async_graphql::{Enum, SimpleObject};

/// What a checkpoint does when nobody is watching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum UnattendedPolicy {
    /// Taken as approved, and the run carries on.
    AutoApprove,
    /// The run holds for a person even under a yolo profile. For the checkpoint
    /// whose whole purpose is a human decision.
    Ask,
}

impl From<leviath_core::blueprint::UnattendedPolicy> for UnattendedPolicy {
    fn from(policy: leviath_core::blueprint::UnattendedPolicy) -> Self {
        use leviath_core::blueprint::UnattendedPolicy as Core;
        match policy {
            Core::AutoApprove => Self::AutoApprove,
            Core::Ask => Self::Ask,
        }
    }
}

/// What the person is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum InteractionPointStyle {
    /// Anything they type.
    FreeText,
    /// One of the options.
    MultipleChoice,
    /// Yes or no.
    Confirm,
}

impl From<&leviath_core::blueprint::InteractionStyle> for InteractionPointStyle {
    fn from(style: &leviath_core::blueprint::InteractionStyle) -> Self {
        use leviath_core::blueprint::InteractionStyle as Core;
        match style {
            Core::FreeText => Self::FreeText,
            Core::MultipleChoice => Self::MultipleChoice,
            Core::Confirm => Self::Confirm,
        }
    }
}

/// One option, and what the stage is told when it is picked.
#[derive(Debug, SimpleObject)]
pub(crate) struct DirectiveEntry {
    /// The option label this applies to.
    pub(crate) option: String,
    /// What the stage is told to do next. It re-runs in place rather than
    /// transitioning, so the decision is the runtime's and the work is the
    /// agent's.
    pub(crate) instruction: String,
}

/// A checkpoint a stage raises, where the run waits for a person.
#[derive(Debug, SimpleObject)]
pub(crate) struct InteractionPoint {
    /// The point's name, unique within the stage.
    pub(crate) name: String,
    /// What the person is asked.
    pub(crate) prompt: String,
    /// Whether an answer is expected rather than optional. A presentation hint,
    /// not to be confused with `unattended`, which decides whether the question
    /// is raised at all.
    pub(crate) required: bool,
    /// What happens when nobody is watching.
    pub(crate) unattended: UnattendedPolicy,
    /// What the person is asked for.
    pub(crate) style: InteractionPointStyle,
    /// The options, for a multiple choice.
    pub(crate) options: Vec<String>,
    /// Options that send the stage back round with an instruction instead of
    /// letting it transition.
    pub(crate) directives: Vec<DirectiveEntry>,
    /// Options that cancel the run outright, with no further inference.
    pub(crate) abort_options: Vec<String>,
    /// Options that open the stage's last output for the person to edit, and
    /// feed the edit back into the context.
    pub(crate) edit_options: Vec<String>,
    /// The region holding this point's authoritative document, by name. Each
    /// time the point is raised, the current document replaces that region, so a
    /// later revision builds on the current version rather than starting over.
    pub(crate) document_region: Option<String>,
}

impl From<&leviath_core::blueprint::InteractionPoint> for InteractionPoint {
    fn from(point: &leviath_core::blueprint::InteractionPoint) -> Self {
        let mut directives: Vec<DirectiveEntry> = point
            .directives
            .iter()
            .map(|(option, instruction)| DirectiveEntry {
                option: option.clone(),
                instruction: instruction.clone(),
            })
            .collect();
        // The manifest's own table is a hash map, so an unsorted list would
        // reorder between two reads of one blueprint.
        directives.sort_by(|a, b| a.option.cmp(&b.option));
        Self {
            name: point.name.clone(),
            prompt: point.prompt.clone(),
            required: point.required,
            unattended: UnattendedPolicy::from(point.unattended),
            style: InteractionPointStyle::from(&point.style),
            options: point.options.clone(),
            directives,
            abort_options: point.abort_options.clone(),
            edit_options: point.edit_options.clone(),
            document_region: point.document_region.clone(),
        }
    }
}

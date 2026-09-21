//! The checkpoints a stage raises, and what each answer does.

use std::sync::Arc;

use async_graphql::{Enum, Object, SimpleObject};

use leviath_core::Blueprint as CoreBlueprint;

use super::super::blueprint::Region;

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
    /// run's.
    pub(crate) instruction: String,
}

/// The resolver state behind the `InteractionPoint` type.
pub(crate) struct InteractionPoint {
    /// The blueprint the document region resolves in.
    blueprint: Arc<CoreBlueprint>,
    /// The point as the stage wrote it.
    point: leviath_core::blueprint::InteractionPoint,
}

/// A checkpoint a stage raises, where the run waits for a person.
#[Object]
impl InteractionPoint {
    /// The point's name, unique within the stage.
    async fn name(&self) -> &str {
        &self.point.name
    }

    /// What the person is asked.
    async fn prompt(&self) -> &str {
        &self.point.prompt
    }

    /// Whether an answer is expected rather than optional. A presentation hint,
    /// not to be confused with `unattended`, which decides whether the question
    /// is raised at all.
    async fn required(&self) -> bool {
        self.point.required
    }

    /// What happens when nobody is watching.
    async fn unattended(&self) -> UnattendedPolicy {
        UnattendedPolicy::from(self.point.unattended)
    }

    /// What the person is asked for.
    async fn style(&self) -> InteractionPointStyle {
        InteractionPointStyle::from(&self.point.style)
    }

    /// The options, for a multiple choice.
    async fn options(&self) -> &[String] {
        &self.point.options
    }

    /// Options that send the stage back round with an instruction instead of
    /// letting it transition, sorted by option so two reads of one blueprint
    /// cannot disagree about the order.
    async fn directives(&self) -> Vec<DirectiveEntry> {
        let mut directives: Vec<DirectiveEntry> = self
            .point
            .directives
            .iter()
            .map(|(option, instruction)| DirectiveEntry {
                option: option.clone(),
                instruction: instruction.clone(),
            })
            .collect();
        directives.sort_by(|a, b| a.option.cmp(&b.option));
        directives
    }

    /// Options that cancel the run outright, with no further inference.
    async fn abort_options(&self) -> &[String] {
        &self.point.abort_options
    }

    /// Options that open the stage's last output for the person to edit, and
    /// feed the edit back into the context.
    async fn edit_options(&self) -> &[String] {
        &self.point.edit_options
    }

    /// The region holding this point's authoritative document. Each time the
    /// point is raised, the current document replaces that region, so a later
    /// revision builds on the current version rather than starting over.
    ///
    /// Null when the point names none, and also when it names a region no layout
    /// in this blueprint declares. `documentRegionName` tells those apart.
    async fn document_region(&self) -> Option<Region> {
        super::refs::region(&self.blueprint, self.point.document_region.as_deref()?)
    }

    /// The document region's name, verbatim. Null when the point names none.
    async fn document_region_name(&self) -> Option<&str> {
        self.point.document_region.as_deref()
    }
}

impl InteractionPoint {
    /// Describe one checkpoint against the blueprint that holds it.
    pub(crate) fn of(
        blueprint: &Arc<CoreBlueprint>,
        point: &leviath_core::blueprint::InteractionPoint,
    ) -> Self {
        Self {
            blueprint: Arc::clone(blueprint),
            point: point.clone(),
        }
    }
}

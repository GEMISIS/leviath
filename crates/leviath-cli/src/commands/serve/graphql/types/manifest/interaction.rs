//! The checkpoints a stage raises, and what each answer does.

use std::sync::Arc;

use async_graphql::{Enum, Object, SimpleObject};
use leviath_graphql_derive::mirror;

use crate::commands::serve::core::blueprints::ParsedBlueprint;

use super::super::blueprint::Region;

/// What a checkpoint does when nobody is watching.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum UnattendedPolicy {
    /// Taken as approved, and the run carries on.
    AutoApprove,
    /// The run holds for a person even under a yolo profile. For the checkpoint
    /// whose whole purpose is a human decision.
    Ask,
}

impl From<leviath_runtime::spec::graph::UnattendedPoint> for UnattendedPolicy {
    fn from(policy: leviath_runtime::spec::graph::UnattendedPoint) -> Self {
        use leviath_runtime::spec::graph::UnattendedPoint as Core;
        match policy {
            Core::AutoApprove => Self::AutoApprove,
            Core::Ask => Self::Ask,
        }
    }
}

/// What the person is asked for.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum InteractionPointStyle {
    /// Anything they type.
    FreeText,
    /// One of the options.
    MultipleChoice,
    /// Yes or no.
    Confirm,
}

impl From<&leviath_runtime::spec::graph::AnswerStyle> for InteractionPointStyle {
    fn from(style: &leviath_runtime::spec::graph::AnswerStyle) -> Self {
        use leviath_runtime::spec::graph::AnswerStyle as Core;
        match style {
            Core::FreeText => Self::FreeText,
            Core::MultipleChoice => Self::MultipleChoice,
            Core::Confirm => Self::Confirm,
        }
    }
}

/// One option, and what the stage is told when it is picked.
#[mirror(list)]
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
    blueprint: Arc<ParsedBlueprint>,
    /// The point as the stage wrote it.
    point: leviath_runtime::spec::graph::InteractionPointDef,
}

/// A checkpoint a stage raises, where the run waits for a person.
#[mirror(list)]
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
        self.point
            .directives
            .iter()
            .map(|(option, instruction)| DirectiveEntry {
                option: option.clone(),
                instruction: instruction.clone(),
            })
            .collect()
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
        super::refs::region(
            &self.blueprint,
            self.point.document_region.as_ref()?.as_str(),
        )
    }

    /// The document region's name, verbatim. Null when the point names none.
    async fn document_region_name(&self) -> Option<&str> {
        self.point
            .document_region
            .as_ref()
            .map(|name| name.as_str())
    }
}

impl InteractionPoint {
    /// Describe one checkpoint against the blueprint that holds it.
    pub(crate) fn of(
        blueprint: &Arc<ParsedBlueprint>,
        point: &leviath_runtime::spec::graph::InteractionPointDef,
    ) -> Self {
        Self {
            blueprint: Arc::clone(blueprint),
            point: point.clone(),
        }
    }
}

#[cfg(test)]
#[path = "interaction_tests.rs"]
mod tests;

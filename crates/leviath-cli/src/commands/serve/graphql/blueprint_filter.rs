//! `BlueprintFilter`: which installed blueprints a listing is about.
//!
//! The same convention the run filter follows, on the sibling listing:
//! combinators over a filter of the same type, one scalar filter per queryable
//! field, and every field set in one object having to hold. Two listings, one
//! shape to learn.
//!
//! A blueprint catalogue is read whole from the agent directories and is
//! name-sorted by discovery, so the matcher below is all the filtering there
//! is: there is no index to consult and no tree to walk.

use super::super::types::BlueprintInfo;
use super::filters::{StringFilter, Text};

/// Which installed blueprints a listing is about.
///
/// Every field set has to hold, so a filter object is an `and` of its own
/// fields. `and`, `or` and `not` take filters of this same type.
#[derive(Debug, Default, async_graphql::InputObject)]
pub(crate) struct BlueprintFilter {
    /// Every one of these has to match.
    pub(crate) and: Option<Vec<BlueprintFilter>>,
    /// At least one of these has to match. An empty list matches no blueprint,
    /// which is what an alternation with no alternatives selects.
    pub(crate) or: Option<Vec<BlueprintFilter>>,
    /// This must not match.
    pub(crate) not: Option<Box<BlueprintFilter>>,
    /// Case-insensitive prefix match on the blueprint name, for a search box.
    /// The same thing as `name: { startsWith: }`, spelled shorter.
    pub(crate) query: Option<String>,
    /// The blueprint's name, which is also its key: one name is one installed
    /// blueprint.
    pub(crate) name: Option<StringFilter>,
    /// Exactly these names.
    ///
    /// Passed on the filter a request makes rather than inside a combinator,
    /// this also says which blueprints to read, and a name nothing is
    /// installed under lands in `missing` instead of failing the request.
    pub(crate) names: Option<Vec<String>>,
    /// The version the manifest declares.
    pub(crate) version: Option<StringFilter>,
    /// The description the manifest declares.
    pub(crate) description: Option<StringFilter>,
}

/// A compiled blueprint filter: what the listing consults, once per
/// blueprint.
#[derive(Debug)]
pub(crate) enum BlueprintMatcher {
    /// Every one of these matches.
    All(Vec<BlueprintMatcher>),
    /// At least one of these matches.
    Any(Vec<BlueprintMatcher>),
    /// This one does not match.
    Not(Box<BlueprintMatcher>),
    /// The blueprint is one of these names.
    Names(Vec<String>),
    /// The blueprint's name.
    Name(Text),
    /// The blueprint's declared version.
    Version(Text),
    /// The blueprint's declared description.
    Description(Text),
}

impl BlueprintMatcher {
    /// Whether this blueprint is kept.
    pub(crate) fn matches(&self, info: &BlueprintInfo) -> bool {
        match self {
            Self::All(parts) => parts.iter().all(|part| part.matches(info)),
            Self::Any(parts) => parts.iter().any(|part| part.matches(info)),
            Self::Not(inner) => !inner.matches(info),
            Self::Names(names) => names.iter().any(|name| name == &info.name),
            Self::Name(text) => text.matches(&info.name),
            Self::Version(text) => text.matches(&info.version),
            Self::Description(text) => text.matches(&info.description),
        }
    }

    /// This matcher's contribution to the cursor's filter digest, so a walk
    /// cannot change what it is filtering halfway through.
    ///
    /// The digest only has to be a deterministic function of the filter, and
    /// the derived rendering of an already-normalized tree is exactly that.
    pub(crate) fn digest_part(&self) -> String {
        format!("{self:?}")
    }

    /// Whether this matcher constrains anything at all.
    pub(crate) fn is_empty(&self) -> bool {
        matches!(self, Self::All(parts) if parts.is_empty())
    }
}

impl BlueprintFilter {
    /// The names this filter reads directly, when it names any.
    ///
    /// Only the filter a request passes: a `names` inside a combinator is an
    /// ordinary membership test over the catalogue, with nothing to report as
    /// missing.
    pub(crate) fn exact_names(&self) -> Option<Vec<String>> {
        self.names.clone()
    }

    /// Compile this filter, and everything nested under it, into a matcher.
    pub(crate) fn compiled(self) -> BlueprintMatcher {
        let mut parts: Vec<BlueprintMatcher> = Vec::new();
        for nested in self.and.into_iter().flatten() {
            parts.push(nested.compiled());
        }
        if let Some(alternatives) = self.or {
            parts.push(BlueprintMatcher::Any(
                alternatives
                    .into_iter()
                    .map(BlueprintFilter::compiled)
                    .collect(),
            ));
        }
        if let Some(inner) = self.not {
            parts.push(BlueprintMatcher::Not(Box::new(inner.compiled())));
        }
        if let Some(prefix) = self.query {
            parts.push(BlueprintMatcher::Name(Text {
                starts_with: Some(prefix),
                ..Text::default()
            }));
        }
        if let Some(names) = self.names {
            parts.push(BlueprintMatcher::Names(names));
        }
        if let Some(text) = self.name {
            parts.push(BlueprintMatcher::Name(text.compiled()));
        }
        if let Some(text) = self.version {
            parts.push(BlueprintMatcher::Version(text.compiled()));
        }
        if let Some(text) = self.description {
            parts.push(BlueprintMatcher::Description(text.compiled()));
        }
        BlueprintMatcher::All(parts)
    }
}

#[cfg(test)]
#[path = "blueprint_filter_tests.rs"]
mod tests;

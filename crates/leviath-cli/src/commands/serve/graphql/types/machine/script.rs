//! One script this machine has registered, as the schema describes it.

use async_graphql::{ID, SimpleObject};

/// One registered script.
#[derive(Debug, SimpleObject)]
pub(crate) struct Script {
    /// `script:<kind>:<name>` for a script every blueprint gets, and
    /// `script:<kind>@<blueprint>:<name>` for one blueprint's own.
    ///
    /// The kind and the owning blueprint as well as the name, because a name is
    /// unique only within its kind and the directory it came from: one machine
    /// can hold a global `tool` called `summarise` and a blueprint's own `tool`
    /// of that name, and they are two scripts.
    #[graphql(owned)]
    pub(crate) id: ID,
    /// Which registry it belongs to: a tool, a hook, a validator, a mime check
    /// or a provider.
    pub(crate) kind: String,
    /// Its name, unique within that kind and the directory it came from.
    pub(crate) name: String,
    /// Where it was found: the directory kind this script was read from.
    pub(crate) found_at: String,
    /// The blueprint whose directory it came from, for a blueprint-scoped
    /// script.
    pub(crate) blueprint: Option<String>,
}

impl Script {
    /// Describe one script this machine has registered.
    pub(crate) fn from_item(item: super::super::super::super::scripts::ScriptItem) -> Self {
        Self {
            id: super::super::super::node::script_id(&item.kind, item.agent.as_deref(), &item.name),
            kind: item.kind,
            name: item.name,
            found_at: item.source,
            blueprint: item.agent,
        }
    }
}

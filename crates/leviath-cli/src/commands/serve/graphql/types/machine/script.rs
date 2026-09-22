//! One script this machine has registered, as the schema describes it.

use async_graphql::{Context, ID, Object};
use leviath_graphql_derive::mirror;

use super::super::super::error::IntoGraphql;
use super::super::super::script_ref::{ScriptKind, ScriptScope};

/// One registered script, as a listing found it.
#[derive(Debug)]
pub(crate) struct Script {
    /// Its node id.
    pub(crate) id: ID,
    /// Which registry it belongs to.
    pub(crate) kind: ScriptKind,
    /// Its name within that registry.
    pub(crate) name: String,
    /// Whose directory it was read from.
    pub(crate) scope: ScriptScope,
    /// The blueprint whose directory it came from.
    pub(crate) blueprint_name: Option<String>,
    /// The file on disk.
    pub(crate) path: String,
    /// The same file relative to the directory that names it.
    pub(crate) relative_path: Option<String>,
    /// Whether something loads this file as this kind.
    pub(crate) is_declared: bool,
    /// Whether it compiles right now.
    pub(crate) compiles: Option<bool>,
    /// Why it does not compile.
    pub(crate) compile_error: Option<String>,
}

/// One registered script: a Rhai file this machine loads at some extension
/// point, and what it is for.
///
/// A script is addressed by its kind, its name and the blueprint whose
/// directory it came from, never by name alone: one machine can hold a global
/// `tool` called `summarise` and a blueprint's own `tool` of that name, and
/// they are two files.
#[mirror(list)]
#[Object]
impl Script {
    /// `script:<kind>:<name>` for a script every blueprint gets, and
    /// `script:<kind>@<blueprint>:<name>` for one blueprint's own.
    ///
    /// The kind and the owning blueprint as well as the name, because a name is
    /// unique only within its kind and the directory it came from.
    #[filter(orderable)]
    pub(crate) async fn id(&self) -> ID {
        self.id.clone()
    }

    /// Which registry it belongs to: a tool, a hook, a validator, a mime check
    /// or a provider.
    async fn kind(&self) -> ScriptKind {
        self.kind
    }

    /// Its name, unique within that kind and the directory it came from.
    /// `/`-separated for a file in a subdirectory.
    #[filter(orderable)]
    async fn name(&self) -> &str {
        &self.name
    }

    /// Whose directory it was read from.
    async fn scope(&self) -> ScriptScope {
        self.scope
    }

    /// The blueprint whose directory it came from, for a blueprint-scoped
    /// script.
    #[filter(orderable)]
    async fn blueprint_name(&self) -> Option<&str> {
        self.blueprint_name.as_deref()
    }

    /// The file on disk, absolute.
    async fn path(&self) -> &str {
        &self.path
    }

    /// The same file relative to the directory whose rows or manifest name it,
    /// which is the spelling that goes there. Null for a global tool or a
    /// provider, which nothing names by path.
    async fn relative_path(&self) -> Option<&str> {
        self.relative_path.as_deref()
    }

    /// Whether something loads this file as this kind: a `tools/` directory
    /// the daemon scans, or a manifest that names it. False for a file nothing
    /// has claimed yet.
    async fn is_declared(&self) -> bool {
        self.is_declared
    }

    /// Whether it compiles right now. Null for a file nothing has claimed,
    /// because nothing says which compiler such a file is for.
    async fn compiles(&self) -> Option<bool> {
        self.compiles
    }

    /// Why it does not compile, when it does not.
    async fn compile_error(&self) -> Option<&str> {
        self.compile_error.as_deref()
    }

    /// The script's source.
    ///
    /// Read from disk when this field is selected and not before, so a listing
    /// costs one directory walk rather than one file read per script. A file
    /// nothing has claimed has no kind to address it by, and asking for its
    /// source is refused rather than compiled as a guess.
    #[filter(skip)]
    async fn content(&self, ctx: &Context<'_>) -> async_graphql::Result<String> {
        let state = ctx.data_unchecked::<super::super::super::super::types::AppState>();
        let source = super::super::super::super::scripts::read_one(
            &state.current_config(),
            self.kind.wire(),
            &self.name,
            self.blueprint_name.as_deref(),
        )
        .gql()?;
        Ok(source.content)
    }
}

impl super::super::super::connection::Paged for Script {
    const NAME: &'static str = "Script";
}

impl Script {
    /// Describe one script this machine has registered.
    pub(crate) fn from_item(item: super::super::super::super::scripts::ScriptItem) -> Self {
        Self {
            id: super::super::super::node::script_id(&item.kind, item.agent.as_deref(), &item.name),
            kind: ScriptKind::from_wire(&item.kind),
            name: item.name,
            scope: ScriptScope::from_wire(&item.source),
            blueprint_name: item.agent,
            path: item.path,
            relative_path: item.relative_path,
            is_declared: item.declared,
            compiles: item.compiles,
            compile_error: item.error,
        }
    }
}

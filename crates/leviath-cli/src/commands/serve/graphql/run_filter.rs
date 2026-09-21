//! `RunFilter`: which runs a listing is about.
//!
//! One input object, shaped like the run it selects. A field on `Run` that is
//! worth querying by has a field here of the matching scalar filter, so
//! "runs older than this", "runs that worked longer than that" and every
//! combination nobody predicted are questions the schema already answers.
//!
//! The combinators are what make it a query language rather than a fixed list:
//! `and`, `or` and `not` take filters of this same type, so a predicate nests
//! as deeply as a client needs. Every field set in one filter object has to
//! hold, which is what makes `and` the default and leaves `or` to say
//! otherwise.
//!
//! A filter compiles into a [`RunMatcher`] before any run is read, and the
//! matcher is what the listing consults. Compiling is also where the request
//! is refused: a page size over the cap, a digest pin that does not match what
//! is installed, more ids than one request may name.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_graphql::{Enum, ID, InputObject};

use super::super::core::error::ServeError;
use super::super::core::runs::predicate::{MatchContext, RunPredicate};
use super::super::core::runs::{self as run_core, ParentFilter, RunSelection, SortKey, Source};
use super::super::types::{AppState, status_matches};
use super::filters::{
    BooleanFilter, DecimalFilter, Flag, IntFilter, Ordered, StringFilter, Text, TimestampFilter,
};
use super::inputs::BlueprintInput;
use super::scalars::Decimal;
use super::types::run::RunStatus;
use super::types::run_detail::WaitReasonKind;
use crate::runstate::RunMeta;

/// Sort order for the run listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum RunSort {
    /// Newest spawns first.
    #[graphql(name = "STARTED_AT")]
    Started,
    /// Most recently changed first.
    #[graphql(name = "UPDATED_AT")]
    Updated,
    /// Runs that moved most recently first.
    #[graphql(name = "LAST_PROGRESS_AT")]
    LastProgress,
}

impl From<RunSort> for SortKey {
    fn from(sort: RunSort) -> Self {
        match sort {
            RunSort::Started => SortKey::Started,
            RunSort::Updated => SortKey::Updated,
            RunSort::LastProgress => SortKey::LastProgress,
        }
    }
}

/// Where a run search looks.
///
/// `META` and `FILES` answer from what is already parsed in memory. The other
/// three read files per run, so they are what a client offers as a "search
/// inside runs" toggle rather than paying for on every keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum SearchScope {
    /// Run metadata: title, blueprint name, status, task, caller metadata.
    Meta,
    /// The run's own record of the files it changed.
    Files,
    /// Region contents on disk.
    Context,
    /// Log lines on disk.
    Logs,
    /// The crash-resume journal.
    Journal,
}

impl From<SearchScope> for Source {
    fn from(scope: SearchScope) -> Self {
        match scope {
            SearchScope::Meta => Source::Meta,
            SearchScope::Files => Source::Files,
            SearchScope::Context => Source::Context,
            SearchScope::Logs => Source::Logs,
            SearchScope::Journal => Source::Journal,
        }
    }
}

impl SearchScope {
    /// The word this scope goes on the wire as, for the cursor's filter
    /// digest.
    ///
    /// The digest is taken over the request's own spelling, so a cursor
    /// cannot be carried from one search to a different one that happens to
    /// parse to the same set.
    fn as_str(self) -> &'static str {
        match self {
            SearchScope::Meta => "meta",
            SearchScope::Files => "files",
            SearchScope::Context => "context",
            SearchScope::Logs => "logs",
            SearchScope::Journal => "journal",
        }
    }
}

/// Which part of the run tree a listing is about.
///
/// One three-way choice, because a run either was started by another run or
/// was not. `ALL` is every run on the machine, sub-agents included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum RunScope {
    /// Every run, sub-agents included.
    All,
    /// Only runs nobody started.
    TopLevel,
    /// Only runs somebody started, at any depth.
    SubAgents,
}

/// Which runs a listing is about.
///
/// Every field set has to hold, so a filter object is an `and` of its own
/// fields. `and`, `or` and `not` take filters of this same type, so a client
/// composes whatever predicate it needs rather than waiting for a field that
/// spells it.
///
/// `query`, `queryIn`, `sort` and `ascending` describe the listing rather than
/// a run, so they are read from the filter a request passes and refused inside
/// a combinator, where there is no one listing for them to describe.
#[derive(Debug, Default, InputObject)]
pub(crate) struct RunFilter {
    /// Every one of these has to match.
    pub(crate) and: Option<Vec<RunFilter>>,
    /// At least one of these has to match. An empty list matches no run, which
    /// is what an alternation with no alternatives selects.
    pub(crate) or: Option<Vec<RunFilter>>,
    /// This must not match.
    pub(crate) not: Option<Box<RunFilter>>,
    /// Exactly these runs, by id.
    ///
    /// Passed on the filter a request makes rather than inside a combinator,
    /// this also says which runs to read: the listing fetches them by id and
    /// reports the ones it cannot find in `missing`, instead of walking the
    /// store. Everything else in the filter still applies to what comes back.
    pub(crate) ids: Option<Vec<ID>>,
    /// Only runs in this status.
    pub(crate) status: Option<RunStatus>,
    /// Runs in any of these statuses. Set beside `status`, the two name one
    /// set and a run in any of them matches.
    pub(crate) status_in: Option<Vec<RunStatus>>,
    /// The run's human title, absent until the titling pass lands one. A run
    /// with no title yet matches nothing here.
    pub(crate) title: Option<StringFilter>,
    /// The initial ask the run was given.
    pub(crate) task: Option<StringFilter>,
    /// The blueprint name the run recorded.
    pub(crate) blueprint_name: Option<StringFilter>,
    /// Only runs of this installed blueprint, matched on the name each run
    /// recorded.
    ///
    /// A `digest` on it pins the revision installed now, and the request fails
    /// where that is something else: runs are matched by name whatever is
    /// installed, so the pin is how a client asking "this agent's runs" finds
    /// out the agent has been edited under it.
    pub(crate) blueprint: Option<BlueprintInput>,
    /// Runs where some stage ran an inference on a provider this accepts.
    ///
    /// The question is about the run's stages, so one stage matching keeps the
    /// run. That makes `ne` "a stage ran on something else" rather than "no
    /// stage ran on this"; wrap the filter in `not` for the second. A run that
    /// has billed no call, and a run recorded before Leviath kept this, match
    /// nothing here.
    pub(crate) stage_provider: Option<StringFilter>,
    /// Runs where some stage ran an inference on a model this accepts, matched
    /// as the serving provider spells it.
    ///
    /// Quantified over the run's stages exactly as `stageProvider` is, with
    /// the same reading of `ne`. Set beside `stageProvider`, the two are
    /// separate conditions rather than one pair: a run whose first stage ran
    /// on one provider and whose second ran on another provider's model
    /// satisfies both.
    pub(crate) stage_model: Option<StringFilter>,
    /// Whether the run was spawned unattended.
    pub(crate) unattended: Option<BooleanFilter>,
    /// The yolo profile the run named at spawn. A run that named none matches
    /// nothing here.
    pub(crate) yolo_profile_name: Option<StringFilter>,
    /// Why a parked run is parked. A run that is not parked matches nothing
    /// here.
    pub(crate) wait_reason: Option<WaitReasonKind>,
    /// Runs parked for any of these reasons. Set beside `waitReason`, the two
    /// name one set.
    pub(crate) wait_reason_in: Option<Vec<WaitReasonKind>>,
    /// When the run was spawned, in unix epoch seconds.
    pub(crate) started_at: Option<TimestampFilter>,
    /// When the run last changed state, in unix epoch seconds.
    pub(crate) updated_at: Option<TimestampFilter>,
    /// Wall-clock seconds since the run started. "Older than an hour" is
    /// `{ gt: 3600 }`.
    pub(crate) age_secs: Option<IntFilter>,
    /// Seconds the run spent actually working, parked time excluded.
    pub(crate) working_secs: Option<IntFilter>,
    /// What the run has spent, in US dollars. A run whose spend is unknown
    /// because some call in it went unpriced matches nothing here.
    pub(crate) cost_usd: Option<DecimalFilter>,
    /// Which part of the run tree to include.
    pub(crate) scope: Option<RunScope>,
    /// Direct children of this run id. A run id naming nothing gives an empty
    /// page rather than an error: a run with no children yet is a normal
    /// answer.
    pub(crate) parent: Option<ID>,
    /// Every run under this one, at any depth, and not the run itself. The
    /// flat read of a fan-out: nesting `children` walks one level per request,
    /// and this walks the whole subtree one page at a time.
    pub(crate) descendant_of: Option<ID>,
    /// Case-insensitive substring. No regex, no boolean operators.
    pub(crate) query: Option<String>,
    /// Where to look. Defaults to metadata and files, which are free.
    pub(crate) query_in: Option<Vec<SearchScope>>,
    /// Newest spawns or most recently active first.
    pub(crate) sort: Option<RunSort>,
    /// Oldest first when true.
    pub(crate) ascending: Option<bool>,
}

/// A compiled filter: what the listing consults, once per run.
///
/// The tree mirrors the filter that produced it, with each field already
/// resolved to the shape the comparison runs in - a blueprint pin checked, a
/// status enum turned into the daemon's own word, a 32-bit bound widened.
#[derive(Debug)]
pub(crate) enum RunMatcher {
    /// Every one of these matches.
    All(Vec<RunMatcher>),
    /// At least one of these matches.
    Any(Vec<RunMatcher>),
    /// This one does not match.
    Not(Box<RunMatcher>),
    /// The run is one of these ids.
    Ids(Vec<String>),
    /// The run is in one of these statuses, in the daemon's own spelling.
    Statuses(Vec<String>),
    /// The run is parked for one of these reasons.
    WaitReasons(Vec<WaitReasonKind>),
    /// The run is in this part of the tree.
    Scope(RunScope),
    /// The run's parent is this one.
    Parent(String),
    /// The run is somewhere under this one.
    DescendantOf(String),
    /// The blueprint name the run recorded.
    BlueprintName(Text),
    /// The run's title.
    Title(Text),
    /// The run's task.
    Task(Text),
    /// The yolo profile the run named.
    YoloProfileName(Text),
    /// Some stage of the run ran on a provider this accepts.
    StageProvider(Text),
    /// Some stage of the run ran on a model this accepts.
    StageModel(Text),
    /// Whether the run is unattended.
    Unattended(Flag),
    /// When the run started.
    StartedAt(Ordered<i64>),
    /// When the run last changed.
    UpdatedAt(Ordered<i64>),
    /// How old the run is.
    AgeSecs(Ordered<i64>),
    /// How long the run has worked.
    WorkingSecs(Ordered<i64>),
    /// What the run has spent.
    CostUsd(Ordered<Decimal>),
}

impl RunPredicate for RunMatcher {
    fn matches(&self, meta: &RunMeta, ctx: &MatchContext) -> bool {
        match self {
            Self::All(parts) => parts.iter().all(|part| part.matches(meta, ctx)),
            Self::Any(parts) => parts.iter().any(|part| part.matches(meta, ctx)),
            Self::Not(inner) => !inner.matches(meta, ctx),
            Self::Ids(ids) => ids.iter().any(|id| id == &meta.run_id),
            Self::Statuses(words) => words.iter().any(|word| status_matches(&meta.status, word)),
            Self::WaitReasons(kinds) => meta
                .waiting_on
                .as_ref()
                .is_some_and(|reason| kinds.contains(&WaitReasonKind::of(reason))),
            Self::Scope(scope) => match scope {
                RunScope::All => true,
                RunScope::TopLevel => meta.parent_run_id.is_none(),
                RunScope::SubAgents => meta.parent_run_id.is_some(),
            },
            Self::Parent(id) => meta.parent_run_id.as_deref() == Some(id.as_str()),
            Self::DescendantOf(root) => ctx
                .subtrees
                .get(root)
                .is_some_and(|under| under.contains(&meta.run_id)),
            Self::BlueprintName(text) => text.matches(&meta.agent_name),
            Self::Title(text) => text.matches_option(meta.title.as_deref()),
            Self::Task(text) => text.matches(&meta.task),
            Self::YoloProfileName(text) => text.matches_option(meta.yolo_profile.as_deref()),
            // Over the run's own record, which the listing has already parsed.
            // The per-stage lists live in `stages.json`, and consulting that
            // would be a second file opened for every run on the machine.
            Self::StageProvider(text) => meta
                .stage_models
                .iter()
                .any(|used| text.matches(&used.provider)),
            Self::StageModel(text) => meta
                .stage_models
                .iter()
                .any(|used| text.matches(&used.model)),
            Self::Unattended(flag) => flag.matches(meta.yolo),
            Self::StartedAt(bounds) => bounds.matches(&meta.started_at),
            Self::UpdatedAt(bounds) => bounds.matches(&meta.updated_at),
            Self::AgeSecs(bounds) => bounds.matches(&(meta.age_secs(ctx.now) as i64)),
            Self::WorkingSecs(bounds) => {
                bounds.matches(&(meta.active_runtime_secs(ctx.now) as i64))
            }
            Self::CostUsd(bounds) => bounds.matches_option(meta.cost_usd.map(Decimal)),
        }
    }

    /// The compiled tree, rendered.
    ///
    /// The digest only has to be a deterministic function of the filter, and
    /// the derived rendering of an already-normalized tree is exactly that.
    fn digest_part(&self) -> String {
        format!("{self:?}")
    }

    fn subtree_roots(&self, out: &mut Vec<String>) {
        match self {
            Self::All(parts) | Self::Any(parts) => {
                for part in parts {
                    part.subtree_roots(out);
                }
            }
            Self::Not(inner) => inner.subtree_roots(out),
            Self::DescendantOf(root) => out.push(root.clone()),
            Self::Ids(_)
            | Self::Statuses(_)
            | Self::WaitReasons(_)
            | Self::Scope(_)
            | Self::Parent(_)
            | Self::BlueprintName(_)
            | Self::Title(_)
            | Self::Task(_)
            | Self::YoloProfileName(_)
            | Self::StageProvider(_)
            | Self::StageModel(_)
            | Self::Unattended(_)
            | Self::StartedAt(_)
            | Self::UpdatedAt(_)
            | Self::AgeSecs(_)
            | Self::WorkingSecs(_)
            | Self::CostUsd(_) => {}
        }
    }
}

impl RunFilter {
    /// Turn the filter into the selection both surfaces list from.
    ///
    /// Rejections happen here, before anything is read: a page size over the
    /// cap, more ids than one request may name, a digest pin that does not
    /// match what is installed, and a listing-level field set where there is
    /// no listing to describe.
    pub(crate) async fn selection(
        mut self,
        state: &AppState,
        first: i32,
    ) -> Result<RunSelection, ServeError> {
        let limit = page_size(first)?;
        // The four that describe the listing rather than a run are read here
        // and are gone from the filter by the time it compiles, which is what
        // makes the same fields a refusal inside a combinator.
        let sort = self.sort.take();
        let ascending = self.ascending.take();
        let query = self.query.take();
        let scopes = self.query_in.take().unwrap_or_default();
        // An `ids` on the filter a request makes says which runs to read, not
        // only which to keep, so it travels beside the matcher as well as in
        // it.
        let ids = match self.ids {
            Some(ref ids) => Some(id_strings(ids)?),
            None => None,
        };

        let sources_raw = match scopes.is_empty() {
            true => String::new(),
            false => scopes
                .iter()
                .map(|scope| scope.as_str())
                .collect::<Vec<_>>()
                .join(","),
        };
        let sources = match scopes.is_empty() {
            true => vec![Source::Meta, Source::Files],
            false => scopes.into_iter().map(Source::from).collect(),
        };

        let matcher = compile(self, state).await?;
        // An empty filter contributes nothing to the cursor's digest, so an
        // unfiltered GraphQL walk and an unfiltered REST one mint the same
        // cursors and either can resume the other.
        let predicate: Option<Arc<dyn RunPredicate>> = match matcher {
            RunMatcher::All(ref parts) if parts.is_empty() => None,
            other => Some(Arc::new(other)),
        };

        Ok(RunSelection {
            limit,
            statuses: Vec::new(),
            sort: sort.unwrap_or(RunSort::Started).into(),
            descending: !ascending.unwrap_or(false),
            q: query,
            sources,
            sources_raw,
            fields: None,
            ids,
            since: None,
            parent: ParentFilter::Any,
            blueprint: None,
            predicate,
        })
    }

    /// The same selection, unpaged: every run the filter matches.
    ///
    /// For an export, whose answer is a file rather than a response, so the
    /// page cap has nothing left to protect. It is built through the listing's
    /// own path all the same, so every other bound the listing enforces still
    /// holds.
    pub(crate) async fn everything(self, state: &AppState) -> Result<RunSelection, ServeError> {
        let mut selection = self.selection(state, 1).await?;
        selection.limit = usize::MAX;
        Ok(selection)
    }
}

/// The ids in a list, checked against the cap on how many one request may
/// name.
fn id_strings(ids: &[ID]) -> Result<Vec<String>, ServeError> {
    if ids.len() > run_core::MAX_IDS {
        return Err(ServeError::BadRequest(format!(
            "`ids` names {} runs; at most {} may be named at once",
            ids.len(),
            run_core::MAX_IDS
        )));
    }
    Ok(ids.iter().map(|id| id.to_string()).collect())
}

/// Check a requested page size against the cap.
///
/// Refused rather than clamped. REST clamps because a query string is often
/// hand-written and a clamped answer is still useful; a GraphQL client builds
/// its query in code, and silently getting 200 of the 500 it asked for is the
/// kind of bug that only shows up as missing rows much later.
pub(crate) fn page_size(first: i32) -> Result<usize, ServeError> {
    match usize::try_from(first) {
        Ok(0) | Err(_) => Err(ServeError::BadRequest(
            "`first` must be at least 1; omit it for the default".to_string(),
        )),
        Ok(n) if n > run_core::MAX_LIMIT => Err(ServeError::BadRequest(format!(
            "`first` may be at most {}, the server's page-size cap",
            run_core::MAX_LIMIT
        ))),
        Ok(n) => Ok(n),
    }
}

/// Compile one filter object, and everything nested under it, into a matcher.
///
/// Boxed because the type is its own argument: a filter holds filters, so the
/// future this returns holds a future of its own type.
fn compile<'a>(
    filter: RunFilter,
    state: &'a AppState,
) -> Pin<Box<dyn Future<Output = Result<RunMatcher, ServeError>> + Send + 'a>> {
    Box::pin(async move {
        let mut parts: Vec<RunMatcher> = Vec::new();
        listing_level_fields(&filter)?;

        for nested in filter.and.into_iter().flatten() {
            parts.push(compile(nested, state).await?);
        }
        if let Some(alternatives) = filter.or {
            let mut compiled = Vec::with_capacity(alternatives.len());
            for nested in alternatives {
                compiled.push(compile(nested, state).await?);
            }
            parts.push(RunMatcher::Any(compiled));
        }
        if let Some(inner) = filter.not {
            parts.push(RunMatcher::Not(Box::new(compile(*inner, state).await?)));
        }

        if let Some(ref ids) = filter.ids {
            parts.push(RunMatcher::Ids(id_strings(ids)?));
        }
        let statuses: Vec<String> = filter
            .status
            .into_iter()
            .chain(filter.status_in.into_iter().flatten())
            .map(|status| status.wire().to_string())
            .collect();
        if !statuses.is_empty() {
            parts.push(RunMatcher::Statuses(statuses));
        }
        let reasons: Vec<WaitReasonKind> = filter
            .wait_reason
            .into_iter()
            .chain(filter.wait_reason_in.into_iter().flatten())
            .collect();
        if !reasons.is_empty() {
            parts.push(RunMatcher::WaitReasons(reasons));
        }

        if let Some(blueprint) = filter.blueprint {
            let name = blueprint.installed(state).await?;
            parts.push(RunMatcher::BlueprintName(Text {
                eq: Some(name),
                ..Text::default()
            }));
        }
        if let Some(text) = filter.blueprint_name {
            parts.push(RunMatcher::BlueprintName(text.compiled()));
        }
        if let Some(text) = filter.title {
            parts.push(RunMatcher::Title(text.compiled()));
        }
        if let Some(text) = filter.task {
            parts.push(RunMatcher::Task(text.compiled()));
        }
        if let Some(text) = filter.yolo_profile_name {
            parts.push(RunMatcher::YoloProfileName(text.compiled()));
        }
        if let Some(text) = filter.stage_provider {
            parts.push(RunMatcher::StageProvider(text.compiled()));
        }
        if let Some(text) = filter.stage_model {
            parts.push(RunMatcher::StageModel(text.compiled()));
        }
        if let Some(flag) = filter.unattended {
            parts.push(RunMatcher::Unattended(flag.compiled()));
        }
        if let Some(bounds) = filter.started_at {
            parts.push(RunMatcher::StartedAt(bounds.compiled()));
        }
        if let Some(bounds) = filter.updated_at {
            parts.push(RunMatcher::UpdatedAt(bounds.compiled()));
        }
        if let Some(bounds) = filter.age_secs {
            parts.push(RunMatcher::AgeSecs(bounds.compiled()));
        }
        if let Some(bounds) = filter.working_secs {
            parts.push(RunMatcher::WorkingSecs(bounds.compiled()));
        }
        if let Some(bounds) = filter.cost_usd {
            parts.push(RunMatcher::CostUsd(bounds.compiled()));
        }
        if let Some(scope) = filter.scope {
            parts.push(RunMatcher::Scope(scope));
        }
        if let Some(parent) = filter.parent {
            parts.push(RunMatcher::Parent(parent.to_string()));
        }
        if let Some(root) = filter.descendant_of {
            parts.push(RunMatcher::DescendantOf(root.to_string()));
        }

        Ok(RunMatcher::All(parts))
    })
}

/// Refuse the fields that describe the listing rather than a run.
///
/// The request's own filter has had them taken off it before it compiles, so
/// anything still carrying one is nested inside a combinator, where a search
/// or a sort has no listing to apply to.
fn listing_level_fields(filter: &RunFilter) -> Result<(), ServeError> {
    let named: Vec<&str> = [
        filter.query.is_some().then_some("query"),
        filter.query_in.is_some().then_some("queryIn"),
        filter.sort.is_some().then_some("sort"),
        filter.ascending.is_some().then_some("ascending"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if named.is_empty() {
        return Ok(());
    }
    Err(ServeError::BadRequest(format!(
        "{} describe the listing rather than a run, so they belong on the filter the request \
         passes rather than inside `and`, `or` or `not`",
        named.join(" and ")
    )))
}

#[cfg(test)]
#[path = "run_filter_tests.rs"]
mod tests;

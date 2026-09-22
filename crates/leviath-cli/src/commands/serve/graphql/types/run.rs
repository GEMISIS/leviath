//! `Run`: one run of a blueprint, and the values that hang off it.
//!
//! A run is read from the shared index, so the object below holds the same
//! `Arc<RunMeta>` the REST listing holds: a page of fifty is fifty pointer
//! copies, not fifty parses. Fields that only need what is already in memory
//! resolve without touching the disk, which is what makes a selection set the
//! cheaper way to ask.

use std::sync::Arc;

use async_graphql::{Context, Enum, ID, Object};
use leviath_graphql_derive::mirror;

use super::super::super::blocking::blocking;
use super::super::super::core::blueprints;
use super::super::super::core::error::ServeError;
use super::super::super::core::{files, history};
use super::super::super::types::AppState;
use super::super::connection::{
    Connection, Paged, PositionQuery, Total, position_order, position_page,
};
use super::super::error::IntoGraphql;
use super::super::filter::{MatchCx, run_relations};
use super::super::paging::digest::canonical;
use super::super::paging::order::{OrderDirection, Term};
use super::super::paging::page::{page, weight};
use super::super::scalars::{BigInt, Cursor, Decimal, Timestamp};
use super::blueprint::Blueprint;
use super::interaction::{Interaction, InteractionFilter, InteractionOrder, InteractionOutput};
use super::run_detail::{
    Artifact, ArtifactFilter, ArtifactOrder, BlobEntry, BlobEntryFilter, BlobEntryOrder,
    ContextWindow, FinalOutput, RunFlags, StageModelUse, StageRecord, StageRecordFilter,
    StageRecordOrder, WaitReason,
};
use super::run_files::{FileEntry, FileEntryFilter, FileListingExtras, FileSource, FileWindow};
use crate::commands::serve::cursor;
use crate::runstate::RunMeta;
use support::{
    BoundedPageArgs, ContextSnapshotPoint, ContextSnapshotPointFilter, CurrentStage,
    LogStageOptions, LogStream, MetadataEntry, RunTreeStatus, as_i32, bounded_page, signed,
    snapshot_point, tail_logs, unfiltered_history,
};
pub(crate) use support::{CostBreakdown, TokenUsage, WorkingClock};

mod reads;
mod support;

/// Where a run's own file reads are reported, for the test that counts them.
pub(crate) use reads::counted;
#[cfg(test)]
pub(crate) use reads::record_file_reads;

impl Paged for Run {
    const NAME: &'static str = "Run";
}

/// The lifecycle states a run moves through.
///
/// One state per variant of the daemon's own `RunStatus`, so the two cannot
/// drift: the conversion below is exhaustive and a new daemon state will not
/// compile until it is named here.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum RunStatus {
    /// Spawned but not yet running.
    Starting,
    /// Moving: inferring, calling tools, transitioning.
    Running,
    /// Parked: on a prompt somebody has to answer, or holding for children.
    WaitingInput,
    /// Paused by `lev pause`; resumes with `lev resume`.
    Paused,
    /// Finished with an answer or a terminal state.
    Complete,
    /// Every required stage finished but the run still accepts messages.
    CompleteInteractive,
    /// Unrecoverable failure; `Run.error` carries what went wrong.
    Error,
    /// Stopped from outside. Nothing went wrong; somebody decided.
    Cancelled,
    /// A state this build has no name for, which is what a newer daemon's new
    /// state looks like from here.
    ///
    /// Only ever reached through a live frame, where the status arrives as the
    /// daemon's own word rather than as a value this build chose. A run read
    /// from disk is parsed into one of the states above or not read at all.
    Unknown,
}

impl From<&leviath_core::run_meta::RunStatus> for RunStatus {
    fn from(status: &leviath_core::run_meta::RunStatus) -> Self {
        use leviath_core::run_meta::RunStatus as Daemon;
        match status {
            Daemon::Starting => Self::Starting,
            Daemon::Running => Self::Running,
            Daemon::WaitingInput => Self::WaitingInput,
            Daemon::Paused => Self::Paused,
            Daemon::Complete => Self::Complete,
            Daemon::CompleteInteractive => Self::CompleteInteractive,
            Daemon::Error => Self::Error,
            Daemon::Cancelled => Self::Cancelled,
        }
    }
}

impl RunStatus {
    /// The state one of the daemon's own words names.
    ///
    /// The live frames carry the word rather than a parsed state, and the
    /// daemon on the other end of the socket may be a newer build than this
    /// one. [`Unknown`](Self::Unknown) is what a word this build does not know
    /// becomes, so one new state does not cost a subscriber the whole frame.
    pub(crate) fn from_wire(word: &str) -> Self {
        use leviath_core::run_meta::RunStatus as Daemon;
        [
            Daemon::Starting,
            Daemon::Running,
            Daemon::WaitingInput,
            Daemon::Paused,
            Daemon::Complete,
            Daemon::CompleteInteractive,
            Daemon::Error,
            Daemon::Cancelled,
        ]
        .iter()
        .find(|status| status.wire() == word)
        .map_or(Self::Unknown, Self::from)
    }
}

/// The resolver state behind the `Run` type.
///
/// Holds the shared `RunMeta` and the wall-clock second the request was
/// answered at, so every duration in one response is measured from one
/// instant rather than drifting field by field.
pub(crate) struct Run {
    /// The run's record, shared with the index rather than copied.
    pub(crate) meta: Arc<RunMeta>,
    /// Daemon time when this request was answered.
    pub(crate) now: i64,
}

/// One execution of a blueprint: the work itself, from the task it was given
/// to whatever it finally produced.
///
/// The `id` is the handle everything else in this API names a run by, and it
/// outlives the run: a finished run is still readable, with its stages, its
/// spend, its tool calls and the context it was holding. A run that spawns
/// sub-agents is the `parent` of their runs, so a whole tree hangs off one id.
///
/// Every duration on a run is measured against the moment the request was
/// answered, so the numbers in one response agree with each other rather than
/// each being taken at its own instant.
#[mirror]
#[Object]
impl Run {
    /// Globally unique run id, and the id every REST route names this run by.
    pub(crate) async fn id(&self) -> ID {
        ID(self.meta.run_id.clone())
    }

    /// The name of the blueprint this run was spawned from.
    async fn blueprint_name(&self) -> &str {
        &self.meta.agent_name
    }

    /// Human title, set by the titling pass; null until it lands.
    #[filter(orderable)]
    async fn title(&self) -> Option<&str> {
        self.meta.title.as_deref()
    }

    /// Why titling failed, when it did.
    async fn title_error(&self) -> Option<&str> {
        self.meta.title_error.as_deref()
    }

    /// The lifecycle state: the filter and subscription vocabulary.
    async fn status(&self) -> RunStatus {
        RunStatus::from(&self.meta.status)
    }

    /// Present only when the status is `ERROR`.
    async fn error(&self) -> Option<&str> {
        self.meta.error.as_deref()
    }

    /// The initial ask this run was given.
    async fn task(&self) -> &str {
        &self.meta.task
    }

    /// Inference turns in the current stage, reset on entering a new one.
    async fn iteration(&self) -> i32 {
        as_i32(self.meta.iteration)
    }

    /// How many tool calls the run has made.
    ///
    /// A count, not the calls themselves: `executions` is the list, with what
    /// each one was and how it ended.
    async fn tool_call_count(&self) -> i32 {
        as_i32(self.meta.tool_calls)
    }

    /// Unix epoch seconds, as the daemon stores it.
    #[filter(orderable)]
    async fn started_at(&self) -> Timestamp {
        Timestamp(self.meta.started_at)
    }

    /// Last state change, unix epoch seconds.
    #[filter(orderable)]
    async fn updated_at(&self) -> Timestamp {
        Timestamp(self.meta.updated_at)
    }

    /// When the run last actually moved. Age a wedged run against this.
    #[filter(orderable)]
    async fn last_progress_at(&self) -> Option<Timestamp> {
        self.meta.last_progress_at.map(Timestamp)
    }

    /// Seconds since `startedAt`, wall-clock.
    async fn age_secs(&self) -> BigInt {
        BigInt(self.meta.age_secs(self.now) as i64)
    }

    /// Seconds actually working, parked time excluded.
    async fn working_secs(&self) -> BigInt {
        BigInt(self.meta.active_runtime_secs(self.now) as i64)
    }

    /// The working span in progress; null while the clock is stopped.
    async fn active(&self) -> Option<WorkingClock> {
        self.meta.active.map(|clock| WorkingClock {
            banked_secs: as_i32(clock.banked_secs as usize),
            since: clock.since.map(Timestamp),
        })
    }

    /// Token roll-up for the whole run.
    async fn usage(&self) -> TokenUsage {
        TokenUsage {
            prompt_tokens: BigInt(self.meta.prompt_tokens as i64),
            completion_tokens: BigInt(self.meta.completion_tokens as i64),
            cached_tokens: BigInt(self.meta.cached_tokens as i64),
            cache_write_tokens: BigInt(self.meta.cache_write_tokens as i64),
        }
    }

    /// Spend roll-up for the whole run.
    async fn cost(&self) -> CostBreakdown {
        CostBreakdown {
            cost_usd: self.meta.cost_usd.map(Decimal),
            cost_priced_usd: Decimal(self.meta.cost_priced_usd),
            cost_is_exact: self.meta.cost_is_exact,
            unpriced_calls: as_i32(self.meta.unpriced_calls),
        }
    }

    /// Whether the run was spawned unattended, so approvals resolve without
    /// a person.
    async fn unattended(&self) -> bool {
        self.meta.yolo
    }

    /// The yolo profile this run was spawned with, when it named one.
    async fn yolo_profile_name(&self) -> Option<&str> {
        self.meta.yolo_profile.as_deref()
    }

    /// The working directory the run executes in.
    async fn workdir(&self) -> &str {
        &self.meta.workdir
    }

    /// The run that spawned this one; null for a top-level run.
    ///
    /// `{ isNull: true }` on the filter is what "only the runs nobody started"
    /// is asked with, and an id here is what "this run's direct children" is.
    async fn parent_id(&self) -> Option<ID> {
        self.meta.parent_run_id.clone().map(ID)
    }

    /// Every run above this one, root first: the breadcrumb of the fan-out
    /// this run is part of, and empty for a run nobody started.
    ///
    /// The flat read of a subtree is a filter on this. `ancestorIds: { has:
    /// "<id>" }` selects every run under that one at any depth, which nesting
    /// `children` can only do one level per request.
    #[filter(with = "run_relations::ancestor_ids")]
    async fn ancestor_ids(&self, ctx: &Context<'_>) -> Vec<ID> {
        let state = ctx.data_unchecked::<AppState>();
        let snapshot = state.caches.run_index.snapshot().await;
        // One step per run above this one, which is three at most, rather than
        // a map of the whole store per run on the page.
        let mut chain: Vec<ID> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut above = self.meta.parent_run_id.clone();
        while let Some(parent) = above {
            // A record that somehow names one of its own descendants stops the
            // walk here rather than sending it round for ever.
            if !seen.insert(parent.clone()) {
                break;
            }
            above = snapshot
                .get(&parent)
                .and_then(|meta| meta.parent_run_id.clone());
            chain.push(ID(parent));
        }
        // Walked upwards, reported downwards: root first, so the list reads as
        // a breadcrumb and `has` finds an ancestor wherever it sits.
        chain.reverse();
        chain
    }

    /// What this run's stages actually ran on, provider and model together, in
    /// the order the run first reached each pair.
    ///
    /// From the run's own record, so filtering on it opens nothing: `{ some: {
    /// provider: { eq: "anthropic" } } }` is "this run used Anthropic
    /// somewhere". The per-stage breakdown is on `stages`, which reads a file.
    async fn stage_models(&self) -> Vec<StageModelUse> {
        self.meta
            .stage_models
            .iter()
            .map(|used| StageModelUse {
                provider: used.provider.clone(),
                model: used.model.clone(),
            })
            .collect()
    }

    /// The blueprint this run executed.
    ///
    /// The run's own snapshot of the manifest, taken at spawn, so it answers
    /// for the run even after the installed blueprint is edited or deleted.
    /// For a run recorded before snapshots existed there is no copy, and this
    /// falls back to the installed file: `blueprint.source` says which, and
    /// `blueprintDigest` is set only for a run that carries its own.
    ///
    /// Null, with an error naming the file, when neither can be read. Nullable
    /// on purpose: one unreadable blueprint in a page of fifty runs must not
    /// cost a client the other forty-nine.
    #[filter(skip)]
    async fn blueprint(&self, ctx: &Context<'_>) -> async_graphql::Result<Option<Blueprint>> {
        let state = ctx.data_unchecked::<AppState>();
        counted(&self.meta.run_id);
        let meta = Arc::clone(&self.meta);
        // One `meta.json`-sized read, off the async runtime: a selection set
        // that asks fifty runs for their blueprints is fifty small reads, and
        // the parse behind them is shared by digest.
        let manifest = blocking(move || {
            blueprints::manifest_for_run(&blueprints::run_dir(&meta.run_id), &meta)
        })
        .await
        .gql()?;
        let parsed = state.caches.blueprints.parse(&manifest).gql()?;
        Ok(Some(Blueprint {
            parsed,
            digest: manifest.digest,
            source: manifest.source.into(),
        }))
    }

    /// The digest of the manifest this run executed, lowercase hex SHA-256.
    ///
    /// Recorded at spawn. Compare it with the installed blueprint's digest to
    /// tell "this run executed what is installed now" from "this run executed
    /// something else". Null for a run recorded before snapshots existed,
    /// where the answer is unknown rather than "the same".
    async fn blueprint_digest(&self) -> Option<&str> {
        self.meta.blueprint_digest.as_deref()
    }

    /// Why this run is parked, and what would unblock it.
    ///
    /// Present only while the status is `WAITING_INPUT`. A run waiting on a
    /// person also carries the prompt itself; a run parked on its own workers
    /// carries only this, and needs nobody.
    async fn wait_reason(&self) -> Option<WaitReason> {
        self.meta.waiting_on.as_ref().map(WaitReason::from)
    }

    /// Post-hoc diagnostics: an empty or degraded run told from a healthy one
    /// without reading logs.
    async fn flags(&self) -> RunFlags {
        RunFlags::from(&self.meta.flags)
    }

    /// The run's stage ledger: what each stage cost, and how often it ran.
    ///
    /// Bounded by the blueprint's stage count, so counting it is free and
    /// reading the whole thing costs one file. Keyset-paged like every listing
    /// in this schema even so, for the one client with a blueprint that
    /// declares hundreds of stages.
    ///
    /// A filter on this asks about the ledger rather than about one page of it:
    /// `stages: { some: { status: { eq: ERROR } } }` on `runs` is "every run
    /// that failed a stage", and it costs the one file per run it reaches.
    #[filter(io, with = "run_relations::stages_of", ty = "Vec<StageRecord>")]
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn stages(
        &self,
        #[graphql(desc = "Which stages to include. Omitted means all of them.")] filter: Option<
            StageRecordFilter,
        >,
        #[graphql(desc = "Sort keys, in priority order. Omitted means declared order.")]
        order_by: Option<Vec<StageRecordOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<StageRecord>> {
        let items = run_relations::stage_records(&self.meta.run_id).await;
        let terms = match order_by {
            Some(asked) if !asked.is_empty() => {
                asked.into_iter().map(StageRecordOrder::term).collect()
            }
            _ => vec![Term {
                field: super::run_detail::StageRecordOrderField::Index,
                direction: OrderDirection::Asc,
            }],
        };
        bounded_page(
            "stages",
            &self.meta.run_id,
            items,
            filter,
            terms,
            BoundedPageArgs { first, after },
            |s: &StageRecord| s.name.clone(),
        )
        .await
    }

    /// The run's context window as it stands right now.
    ///
    /// Null for a run that has not written one yet, and for a finished run
    /// whose window was never persisted. Region contents are their own field,
    /// so asking for the shape of the window does not read its text.
    #[filter(io)]
    async fn context(&self) -> Option<ContextWindow> {
        counted(&self.meta.run_id);
        let run_id = self.meta.run_id.clone();
        let snapshot = blocking(move || crate::runstate::read_context_snapshot(&run_id)).await;
        snapshot.map(|snapshot| ContextWindow {
            snapshot: Arc::new(snapshot),
        })
    }

    /// The answer this run submitted. Null until something is submitted.
    #[filter(io)]
    async fn final_output(&self) -> Option<FinalOutput> {
        counted(&self.meta.run_id);
        let run_id = self.meta.run_id.clone();
        blocking(move || crate::runstate::read_final_output(&run_id))
            .await
            .map(FinalOutput::from)
    }

    /// This run's direct children, paged.
    ///
    /// The run listing with this run preset as the parent, so it takes the same
    /// filter, the same sort keys and the same keyset cursor, and a level that
    /// has more is a non-null `cursor` rather than a flag. Nest the field to
    /// walk deeper, one level per nesting; for a flat read of the whole subtree
    /// use `runs(filter: { ancestorIds: { has: "<id>" } })`.
    #[filter(skip)]
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn children(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which of this run's children to list. Omitted means all of them.")]
        filter: Option<RunFilter>,
        #[graphql(desc = "Sort keys, in priority order. Omitted means newest first.")]
        order_by: Option<Vec<RunOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<Run, super::super::query::runs::RunListingExtras>> {
        super::super::query::runs::children_of(
            ctx,
            &self.meta.run_id,
            filter,
            order_by,
            first,
            after,
        )
        .await
    }

    /// How deep and how wide this run's sub-agent tree is, without walking it.
    ///
    /// The roll-up covers the whole subtree, which is the figure a fan-out is
    /// judged by: a parent that spent little and whose fifty workers spent a
    /// great deal is not a cheap run.
    #[filter(skip)]
    async fn tree_status(&self, ctx: &Context<'_>) -> RunTreeStatus {
        let state = ctx.data_unchecked::<AppState>();
        let snapshot = state.caches.run_index.snapshot().await;
        let mut depth = 0;
        let mut descendants = 0;
        let mut prompt_tokens = 0usize;
        let mut completion_tokens = 0usize;
        let mut cached_tokens = 0usize;
        let mut cache_write_tokens = 0usize;
        // Breadth-first over the index's own parent map, which is why this is a
        // walk of the subtree rather than of the store.
        let mut level: Vec<String> = vec![self.meta.run_id.clone()];
        while !level.is_empty() {
            let mut next = Vec::new();
            for run_id in &level {
                for child in snapshot.under(Some(run_id.as_str())) {
                    descendants += 1;
                    prompt_tokens += child.prompt_tokens;
                    completion_tokens += child.completion_tokens;
                    cached_tokens += child.cached_tokens;
                    cache_write_tokens += child.cache_write_tokens;
                    next.push(child.run_id.clone());
                }
            }
            if !next.is_empty() {
                depth += 1;
            }
            level = next;
        }
        RunTreeStatus {
            rollup: TokenUsage {
                prompt_tokens: BigInt((self.meta.prompt_tokens + prompt_tokens) as i64),
                completion_tokens: BigInt((self.meta.completion_tokens + completion_tokens) as i64),
                cached_tokens: BigInt((self.meta.cached_tokens + cached_tokens) as i64),
                cache_write_tokens: BigInt(
                    (self.meta.cache_write_tokens + cache_write_tokens) as i64,
                ),
            },
            depth: as_i32(depth),
            descendant_count: as_i32(descendants),
        }
    }

    /// A stage's logs.
    ///
    /// `stage` picks one stage by index or every stage in order; omitted, it
    /// means the stage the run is on now. Neither reads more than `tailBytes`
    /// from the end of each stream, and the cap is the server's, because
    /// naming every stage multiplies whatever the client asks for by the
    /// stage count.
    ///
    /// Empty is not null: a stage that has written nothing, an index no stage
    /// answers to and a file this server cannot read all read as no text.
    #[filter(skip)]
    async fn logs(
        &self,
        #[graphql(
            desc = "One stage by index or every stage; omitted means the stage the run is \
                           on now."
        )]
        stage: Option<LogStageOptions>,
        #[graphql(desc = "Which stream to read.", default_with = "LogStream::Output")]
        stream: LogStream,
        #[graphql(desc = "Bytes to read from the end of each stream.")] tail_bytes: Option<i32>,
    ) -> async_graphql::Result<String> {
        tail_logs(&self.meta.run_id, stage, stream, tail_bytes).await
    }

    /// The binary parts this run holds.
    ///
    /// Metadata only: the bytes are behind each entry's `url`, a short-lived
    /// signed link the byte route verifies. Bytes never ride a query answer,
    /// and a page can put that link straight into an `<img src>`. Empty for a
    /// run whose part store is missing rather than merely bare, the same as an
    /// empty one.
    ///
    /// A filter on this asks about the parts rather than about one page of
    /// them, and about the record rather than the link: `blobs: { some: {
    /// mimeType: { startsWith: "image/" } } }` is "every run holding an image".
    #[filter(io, with = "run_relations::blobs_of", ty = "Vec<BlobEntry>")]
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn blobs(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which parts to include. Omitted means all of them.")] filter: Option<
            BlobEntryFilter,
        >,
        #[graphql(desc = "Sort keys, in priority order. Omitted means declared order.")]
        order_by: Option<Vec<BlobEntryOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<BlobEntry>> {
        let state = ctx.data_unchecked::<AppState>();
        let items: Vec<BlobEntry> = run_relations::stored_parts(&self.meta.run_id)
            .await
            .into_iter()
            .map(|blob| {
                let url = super::run_detail::blob_link(state, &self.meta.run_id, &blob);
                BlobEntry::of(blob, url)
            })
            .collect();
        let terms = match order_by {
            Some(asked) if !asked.is_empty() => {
                asked.into_iter().map(BlobEntryOrder::term).collect()
            }
            _ => vec![Term {
                field: super::run_detail::BlobEntryOrderField::Sha256,
                direction: OrderDirection::Asc,
            }],
        };
        bounded_page(
            "blobs",
            &self.meta.run_id,
            items,
            filter,
            terms,
            BoundedPageArgs { first, after },
            |b: &BlobEntry| b.sha256.clone(),
        )
        .await
    }

    /// The files this run handed back beside its answer.
    ///
    /// Same as `blobs`: metadata here, bytes behind a signed link, and a filter
    /// on the records rather than on the link or on one page of them. The run's
    /// own submission record holds these, so filtering on them reads nothing.
    #[filter(with = "run_relations::artifacts_of", ty = "Vec<Artifact>")]
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn artifacts(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which files to include. Omitted means all of them.")] filter: Option<
            ArtifactFilter,
        >,
        #[graphql(desc = "Sort keys, in priority order. Omitted means declared order.")]
        order_by: Option<Vec<ArtifactOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<Artifact>> {
        let state = ctx.data_unchecked::<AppState>();
        let items: Vec<Artifact> = self
            .meta
            .final_output
            .as_ref()
            .map(|output| output.artifacts.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|artifact| super::run_detail::artifact(state, &self.meta.run_id, artifact))
            .collect();
        let terms = match order_by {
            Some(asked) if !asked.is_empty() => {
                asked.into_iter().map(ArtifactOrder::term).collect()
            }
            _ => vec![Term {
                field: super::run_detail::ArtifactOrderField::Name,
                direction: OrderDirection::Asc,
            }],
        };
        bounded_page(
            "artifacts",
            &self.meta.run_id,
            items,
            filter,
            terms,
            BoundedPageArgs { first, after },
            |a: &Artifact| a.name.clone(),
        )
        .await
    }

    /// A short-lived signed link to one file in this run's working directory.
    ///
    /// The byte route verifies the signature, so this works in an `<img src>` or
    /// a download link, where a header cannot be set. It is good for a few
    /// minutes and for that one path.
    ///
    /// Minted without resolving the path, so a file this run never wrote is a
    /// 404 from the byte route rather than nothing here.
    #[filter(skip)]
    async fn file_url(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The file, relative to the run's working directory.")] path: String,
        #[graphql(desc = "Offer it as a download rather than inline.", default = false)]
        download: bool,
    ) -> String {
        let state = ctx.data_unchecked::<AppState>();
        let route = format!("/api/agents/{}/files/raw", self.meta.run_id);
        let mut query = vec![("path", path.as_str())];
        if download {
            query.push(("download", "1"));
        }
        super::super::super::signed_url::signed_path(
            &state.signer,
            &route,
            &query,
            leviath_core::duration::now_secs(),
        )
    }

    /// The run that started this one. Null for a run nobody started.
    ///
    /// A lookup in the index's own map rather than a read: the parent is already
    /// in memory, so walking up a fan-out costs nothing per level.
    #[filter(with = "run_relations::parent_of")]
    async fn parent(&self, ctx: &Context<'_>) -> Option<Run> {
        let state = ctx.data_unchecked::<AppState>();
        let parent_id = self.meta.parent_run_id.as_deref()?;
        let snapshot = state.caches.run_index.snapshot().await;
        snapshot.get(parent_id).map(|meta| Run {
            meta: Arc::clone(meta),
            now: self.now,
        })
    }

    /// The stage the run is in, by name and position. Null before the first
    /// stage is entered.
    async fn current_stage(&self) -> Option<CurrentStage> {
        let name = self.meta.current_stage.clone();
        (!name.is_empty()).then(|| CurrentStage {
            name,
            index: i32::try_from(self.meta.stage_index).unwrap_or(i32::MAX),
            of: i32::try_from(self.meta.num_stages).unwrap_or(i32::MAX),
        })
    }

    /// What the run is parked on, if anything. Null when nothing is waiting.
    ///
    /// The daemon holds these in memory, so this is one round trip to it rather
    /// than a read of the run store. A run in `WAITING_INPUT` whose ask has just
    /// been answered by somebody else answers null, which is the truth by then.
    #[filter(skip)]
    async fn open_interaction(
        &self,
        ctx: &Context<'_>,
    ) -> async_graphql::Result<Option<Interaction>> {
        let state = ctx.data_unchecked::<AppState>();
        let open = super::super::super::core::spawn::open_interactions(state)
            .await
            .gql()?;
        Ok(open
            .into_iter()
            .find(|(run_id, _)| run_id == &self.meta.run_id)
            .map(|(run_id, request)| InteractionOutput::open(run_id, request)))
    }

    /// The run's files: what it recorded changing, or what is in its working
    /// directory now.
    ///
    /// Two different questions. `MODIFIED` is the run's own record, which is
    /// free but capped at record time and a claim about the run rather than about
    /// the disk. `WORKDIR` is the truth, one directory level per request: that
    /// bound is the answer to a repository with a `node_modules` in it.
    #[filter(skip)]
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn files(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The directory to list. Null lists the working directory's root.")]
        path: Option<String>,
        #[graphql(
            desc = "Which question to answer.",
            default_with = "FileSource::Modified"
        )]
        source: FileSource,
        #[graphql(desc = "Which entries to include. Omitted means all of them.")] filter: Option<
            FileEntryFilter,
        >,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<FileEntry, FileListingExtras>> {
        let state = ctx.data_unchecked::<AppState>();
        let registry = state.current_config().mime_registry_or_defaults();
        let meta = Arc::clone(&self.meta);
        let dir = path.map(std::path::PathBuf::from);
        // Dot-prefixed entries are always included now: a filter on `name` is
        // how a client leaves them out, rather than a second argument saying
        // the same thing a second way.
        let listed =
            blocking(move || files::listing(&meta, source.into(), dir.as_deref(), true, &registry))
                .await
                .gql()?;
        let (items, extras) = super::run_files::split(listed);
        let limit = page(first, files::MAX_LISTING_ENTRIES, "the files page cap").gql()?;
        let filter = filter.unwrap_or_default();
        let rendered = canonical(&filter).gql()?;
        let digest = cursor::filter_digest(&["files", &self.meta.run_id, rendered.as_str()]);
        let cx = MatchCx::at(leviath_core::duration::now_secs());
        let walked = position_page(
            items,
            &filter,
            &cx,
            PositionQuery {
                digest: &digest,
                after: after.as_ref().map(|token| token.0.as_str()),
                descending: false,
                limit,
            },
        )
        .await
        .gql()?;
        Ok(
            Connection::plain(walked.items, walked.cursor, Total::known(walked.total))
                .with_extras(extras),
        )
    }

    /// One window of one of the run's files, as text.
    ///
    /// At most a megabyte per read, because the answer travels inside this one.
    /// A larger file is read a window at a time: pass `nextOffset` back as
    /// `offset`, and the windows concatenate into the file. For bytes rather than
    /// text, and for anything that is not text at all, mint a `fileUrl` instead.
    #[filter(skip)]
    async fn file_content(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The file, relative to the run's working directory.")] path: String,
        #[graphql(desc = "Byte offset to start at.", default = 0)] offset: i32,
    ) -> async_graphql::Result<FileWindow> {
        let state = ctx.data_unchecked::<AppState>();
        let registry = state.current_config().mime_registry_or_defaults();
        let offset = u64::try_from(offset)
            .map_err(|_| ServeError::BadRequest("`offset` cannot be negative".to_string()))
            .gql()?;
        let meta = Arc::clone(&self.meta);
        let read = blocking(move || files::read(&meta, &path, offset, false, &registry))
            .await
            .gql()?;
        match read {
            files::FileRead::Window(window) => Ok(FileWindow::from(window)),
            // A directory has no text to return, and this field promises text.
            // `files` is the field that answers what is in one.
            files::FileRead::Listing(listed) => Err(ServeError::BadRequest(format!(
                "'{}' is a directory; read `files` for what is in it",
                listed.path
            )))
            .gql(),
        }
    }

    /// What this run did: every attempt at every tool call, in order, paged.
    ///
    /// Read from the run's journal, so it holds the attempts a context window no
    /// longer shows: a call a gate refused, one that failed and was reissued, one
    /// a restart cut off. Results are not on the page; each execution fetches its
    /// own, because one result can be a whole file.
    #[filter(skip)]
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn executions(
        &self,
        #[graphql(desc = "Which executions to include. Omitted means all of them.")] filter: Option<
            super::execution::ToolExecutionFilter,
        >,
        #[graphql(desc = "Sort key and direction. Omitted means dispatch order.")] order_by: Option<
            Vec<super::execution::ToolExecutionOrder>,
        >,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<super::execution::ToolExecution>> {
        super::execution::executions(self.meta.run_id.clone(), filter, order_by, first, after).await
    }

    /// Every question this run put to a person and got an outcome for, in the
    /// order it asked them, paged.
    ///
    /// The only record that this run stopped for somebody. `executions` says
    /// what the run tried; this says what it needed a person for, and what
    /// came back - including a scope that only ever lived here, since a
    /// granted approval reads no differently from a call no policy ever
    /// stopped once the tool has read it.
    ///
    /// A question is written down when it settles, so one the run is parked on
    /// right now is not here yet: `openInteraction` carries that one while it
    /// is open. A run reading as `WAITING_INPUT` with nothing on its last page
    /// has asked something nobody has answered, rather than asked nothing.
    ///
    /// Empty for an unattended run, which asks nobody: `--yolo` answers before
    /// the question reaches a person, so an empty list on a run that plainly
    /// did something dangerous means exactly that.
    #[filter(skip)]
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn interactions(
        &self,
        #[graphql(
            desc = "Which of the run's own settled asks to include. Omitted means all of \
                           them."
        )]
        filter: Option<InteractionFilter>,
        #[graphql(desc = "Sort key and direction. Omitted means the order the run asked them.")]
        order_by: Option<Vec<InteractionOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<Interaction>> {
        super::interaction::interactions(self.meta.run_id.clone(), filter, order_by, first, after)
            .await
    }

    /// Every trip this run made to a provider, in the order it made them,
    /// paged.
    ///
    /// What `usage` and `cost` cannot say. They are per call that worked, so a
    /// call refused three times and answered on the fourth is billed once and
    /// reads here as the four trips it was. Each entry names the provider and
    /// model asked, how the attempt ended, what the loop did next, and how long
    /// the run waited before it; `failover` carries the move where the stage gave
    /// up on a provider.
    ///
    /// Empty for a run whose journal holds no attempt records.
    #[filter(skip)]
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn inferences(
        &self,
        #[graphql(desc = "Which attempts to include. Omitted means all of them.")] filter: Option<
            super::inference::InferenceAttemptFilter,
        >,
        #[graphql(desc = "Sort key and direction. Omitted means the order they were made.")]
        order_by: Option<Vec<super::inference::InferenceAttemptOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<super::inference::InferenceAttempt>> {
        super::inference::inferences(self.meta.run_id.clone(), filter, order_by, first, after).await
    }

    /// Every committed change to this run's context window, in the order they
    /// landed, paged.
    ///
    /// `contextHistory` serves the window snapshots: what every region held at
    /// each point. This serves the changes: which path through the runtime moved
    /// the window, so a region that lost its plan to a compaction reads
    /// differently from one a stage-edge transform cleared and one the model
    /// deleted. No content, since the snapshot on the same tick already holds the
    /// text.
    ///
    /// Each entry is one transaction, which may touch several regions, and names
    /// the window's revision either side of it - so `contextSnapshot` takes you
    /// to exactly what it started from and what it produced.
    ///
    /// Empty for a run whose journal holds no change records, and for writes
    /// whose path cannot name a cause.
    #[filter(skip)]
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn context_changes(
        &self,
        #[graphql(desc = "Which changes to include. Omitted means all of them.")] filter: Option<
            super::context_change::ContextChangeFilter,
        >,
        #[graphql(desc = "Sort key and direction. Omitted means the order they landed.")]
        order_by: Option<Vec<super::context_change::ContextChangeOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<super::context_change::ContextChange>> {
        super::context_change::context_changes(
            self.meta.run_id.clone(),
            filter,
            order_by,
            first,
            after,
        )
        .await
    }

    /// Snapshots of this run's context window over the run, paged.
    ///
    /// What the window held at each point, not why it changed: `contextChanges`
    /// is the reasons. Each point here carries a whole window, so this is paged
    /// harder than the run listing is: ask for the regions you render rather
    /// than every point's every region. Chronological by default, which is also
    /// the cheaper direction to read.
    ///
    /// Unfiltered, only the page's own windows are read, exactly as
    /// `GET /api/agents/{id}/context/history` reads them. A `filter` is a
    /// question about each point, and answering it means opening that point's
    /// window, so a filtered page reads the run's whole history to decide what
    /// is on it. Page first and filter in the client where the history is long.
    #[filter(skip)]
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn context_history(
        &self,
        #[graphql(desc = "Which points to include. Omitted means all of them.")] filter: Option<
            ContextSnapshotPointFilter,
        >,
        #[graphql(desc = "Sort key and direction. Omitted means chronological.")] order_by: Option<
            Vec<ContextHistoryOrder>,
        >,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<ContextSnapshotPoint>> {
        let limit = page(
            first,
            history::HISTORY_MAX_LIMIT,
            "the history page cap: each point carries a whole context window",
        )
        .gql()?;
        let filter = filter.unwrap_or_default();
        let rendered = canonical(&filter).gql()?;
        let digest =
            cursor::filter_digest(&["context_history", &self.meta.run_id, rendered.as_str()]);
        let descending = order_by
            .unwrap_or_default()
            .first()
            .is_some_and(|term| term.direction.descending());

        let query = PositionQuery {
            digest: &digest,
            after: after.as_ref().map(|token| token.0.as_str()),
            descending,
            limit,
        };
        let walked = match rendered.is_empty() {
            true => unfiltered_history(&self.meta.run_id, query).await.gql()?,
            false => {
                let run_id = self.meta.run_id.clone();
                let points = blocking(move || history::every_window(&run_id)).await;
                let items: Vec<ContextSnapshotPoint> =
                    points.into_iter().map(snapshot_point).collect();
                let cx = MatchCx::at(leviath_core::duration::now_secs());
                position_page(items, &filter, &cx, query).await.gql()?
            }
        };
        Ok(Connection::plain(
            walked.items,
            walked.cursor,
            Total::known(walked.total),
        ))
    }

    /// One window this run held, by its revision.
    ///
    /// A historical read, and an immutable one: a revision is derived from a
    /// window's contents and the journal it is looked up in is append-only, so
    /// this resolves to exactly the content the revision was minted from. No
    /// later write can change what a revision means, and asking for one can never
    /// answer with what the run holds now - `context` is the field for that.
    ///
    /// Take a revision from `ContextChange.revisionBefore` or `revisionAfter` to
    /// see the window either side of a change, or from `ContextWindow.revision`.
    ///
    /// Null when this run never held that window, which is also what a revision
    /// from another run looks like. Where the run held the same content more than
    /// once, this is the first time it did; the content is identical either way.
    #[filter(skip)]
    async fn context_snapshot(
        &self,
        #[graphql(desc = "The window's revision.")] revision: String,
    ) -> Option<ContextSnapshotPoint> {
        let run_id = self.meta.run_id.clone();
        let point = blocking(move || history::at_revision(&run_id, &revision)).await;
        point.map(snapshot_point)
    }

    /// A short-lived signed link to one stored part's bytes.
    ///
    /// The same kind of link `fileUrl` mints, and minted without asking the
    /// store whether the hash is held. `blobs` carries one per part already and
    /// nulls it for bytes that are gone; this is for a hash a client holds, and
    /// for the download form of a part it is showing inline.
    #[filter(skip)]
    async fn blob_url(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The part, by content hash.")] sha256: String,
        #[graphql(desc = "Offer it as a download rather than inline.", default = false)]
        download: bool,
    ) -> String {
        let state = ctx.data_unchecked::<AppState>();
        signed(
            state,
            &format!("/api/agents/{}/blobs/{sha256}", self.meta.run_id),
            download,
        )
    }

    /// A short-lived signed link to one artifact's bytes.
    ///
    /// Minted without reading what the run produced, so an unknown name is a
    /// 404 from the byte route rather than nothing here.
    #[filter(skip)]
    async fn artifact_url(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The artifact, by name.")] name: String,
        #[graphql(desc = "Offer it as a download rather than inline.", default = false)]
        download: bool,
    ) -> String {
        let state = ctx.data_unchecked::<AppState>();
        signed(
            state,
            &format!("/api/agents/{}/artifacts/{name}", self.meta.run_id),
            download,
        )
    }

    /// Whether `sendMessage` reaches this run where it stands.
    ///
    /// Read from the stage the run is in, so it is the answer for now rather than
    /// for the blueprint as a whole. Null when the run's blueprint cannot be read:
    /// unknown is not the same as no, and a console that greyed out its box on a
    /// failed read would be wrong half the time.
    #[filter(skip)]
    async fn accepts_messages(&self, ctx: &Context<'_>) -> Option<bool> {
        let state = ctx.data_unchecked::<AppState>();
        let meta = Arc::clone(&self.meta);
        let manifest = blocking(move || {
            blueprints::manifest_for_run(&blueprints::run_dir(&meta.run_id), &meta)
        })
        .await
        .ok()?;
        let parsed = state.caches.blueprints.parse(&manifest).ok()?;
        let stage = match self.meta.current_stage.is_empty() {
            // Before the first stage is entered, the answer is the entry
            // stage's: that is the stage a message would arrive in.
            true => {
                let entry = parsed.resolve_entry_stage_name();
                parsed.stages.iter().find(|stage| stage.name == entry)
            }
            false => parsed
                .stages
                .iter()
                .find(|stage| stage.name == self.meta.current_stage),
        };
        stage.map(|stage| stage.accepts_messages)
    }

    /// Caller-supplied metadata from spawn. Values are always strings.
    ///
    /// Labels for whoever started the run, such as a ticket or a tenant. They
    /// are searched by `filter.query` and read back unchanged, and nothing in
    /// the run reads them: not a typed extension point.
    ///
    /// Sorted by key: the daemon keeps these in a hash map, and a listing
    /// whose order changes between two identical requests is a diff nobody
    /// can read.
    async fn metadata(&self) -> Vec<MetadataEntry> {
        let mut entries: Vec<MetadataEntry> = self
            .meta
            .metadata
            .iter()
            .map(|(key, value)| MetadataEntry {
                key: key.clone(),
                value: value.clone(),
            })
            .collect();
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        entries
    }
}

position_order!(
    ContextHistoryOrder,
    ContextHistoryOrderField,
    Sequence,
    "The one sort key `contextHistory` may be ordered by.",
    "Where this point sits among the run's own, in the order it was recorded."
);

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;

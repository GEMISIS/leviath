//! `Run`: one run of a blueprint, and the values that hang off it.
//!
//! A run is read from the shared index, so the object below holds the same
//! `Arc<RunMeta>` the REST listing holds: a page of fifty is fifty pointer
//! copies, not fifty parses. Fields that only need what is already in memory
//! resolve without touching the disk, which is what makes a selection set the
//! cheaper way to ask.

use std::sync::Arc;

use async_graphql::{Context, Enum, ID, Object, SimpleObject};

use super::super::super::blocking::blocking;
use super::super::super::core::blueprints;
use super::super::super::core::error::ServeError;
use super::super::super::core::{files, history};
use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::scalars::{BigInt, Cursor, Decimal, Timestamp};
use super::blueprint::Blueprint;
use super::run_detail::{
    Artifact, BlobEntry, ContextWindow, FinalOutput, RunFlags, StageRecord, WaitReason,
};
use super::run_files::{FileListing, FileSource, FileWindow};
use crate::runstate::RunMeta;

/// The lifecycle states a run moves through.
///
/// One state per variant of the daemon's own `RunStatus`, so the two cannot
/// drift: the conversion below is exhaustive and a new daemon state will not
/// compile until it is named here.
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
    /// The daemon's own spelling of this state.
    ///
    /// The filters read `meta.json`, which stores these words, so a GraphQL
    /// enum value has to become one before it can filter anything. Going
    /// through the daemon's own `wire()` keeps one spelling for one state
    /// across both surfaces.
    pub(crate) fn wire(self) -> &'static str {
        use leviath_core::run_meta::RunStatus as Daemon;
        match self {
            Self::Starting => Daemon::Starting.wire(),
            Self::Running => Daemon::Running.wire(),
            Self::WaitingInput => Daemon::WaitingInput.wire(),
            Self::Paused => Daemon::Paused.wire(),
            Self::Complete => Daemon::Complete.wire(),
            Self::CompleteInteractive => Daemon::CompleteInteractive.wire(),
            Self::Error => Daemon::Error.wire(),
            Self::Cancelled => Daemon::Cancelled.wire(),
        }
    }
}

/// Token counts for a run, a stage, or a subtree roll-up.
#[derive(Debug, SimpleObject)]
pub(crate) struct TokenUsage {
    /// Input tokens, cached ones included.
    pub(crate) prompt_tokens: BigInt,
    /// Output tokens.
    pub(crate) completion_tokens: BigInt,
    /// Counted within `promptTokens`, not on top of it. Do not add them twice.
    pub(crate) cached_tokens: BigInt,
    /// Tokens written to the provider's cache.
    pub(crate) cache_write_tokens: BigInt,
}

/// Spend for a run, a stage, or a subtree roll-up.
#[derive(Debug, SimpleObject)]
pub(crate) struct CostBreakdown {
    /// Null is unknown, never free: some call in the run went unpriced.
    pub(crate) cost_usd: Option<Decimal>,
    /// The priced subtotal, kept even while `costUsd` is null.
    pub(crate) cost_priced_usd: Decimal,
    /// True when every priced call carried the provider's own figure.
    pub(crate) cost_is_exact: bool,
    /// Calls no provider priced.
    pub(crate) unpriced_calls: i32,
}

/// Elapsed working time: banked spans plus the one in progress.
#[derive(Debug, SimpleObject)]
pub(crate) struct WorkingClock {
    /// Seconds banked by spans that have ended.
    pub(crate) banked_secs: i32,
    /// When the span in progress began; null while the clock is stopped.
    pub(crate) since: Option<Timestamp>,
}

/// One caller-supplied metadata entry on a run.
#[derive(Debug, SimpleObject)]
pub(crate) struct MetadataEntry {
    /// The key.
    pub(crate) key: String,
    /// The value. Always a string.
    pub(crate) value: String,
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
    async fn started_at(&self) -> Timestamp {
        Timestamp(self.meta.started_at)
    }

    /// Last state change, unix epoch seconds.
    async fn updated_at(&self) -> Timestamp {
        Timestamp(self.meta.updated_at)
    }

    /// When the run last actually moved. Age a wedged run against this.
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
    async fn parent_id(&self) -> Option<&str> {
        self.meta.parent_run_id.as_deref()
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
    async fn blueprint(&self, ctx: &Context<'_>) -> async_graphql::Result<Option<Blueprint>> {
        let state = ctx.data_unchecked::<AppState>();
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
    /// Bounded by the blueprint's stage count, so it is a list rather than a
    /// connection. Read from the run's own `stages.json` only when selected.
    /// Null, with an error, when that file will not read: one run's broken
    /// ledger must not cost a client the page around it.
    async fn stages(&self) -> Option<Vec<StageRecord>> {
        let run_id = self.meta.run_id.clone();
        let records = blocking(move || crate::runstate::read_stages_index(&run_id)).await;
        Some(records.iter().map(StageRecord::from).collect())
    }

    /// The run's context window as it stands right now.
    ///
    /// Null for a run that has not written one yet, and for a finished run
    /// whose window was never persisted. Region contents are their own field,
    /// so asking for the shape of the window does not read its text.
    async fn context(&self) -> Option<ContextWindow> {
        let run_id = self.meta.run_id.clone();
        let snapshot = blocking(move || crate::runstate::read_context_snapshot(&run_id)).await;
        snapshot.map(|snapshot| ContextWindow {
            snapshot: Arc::new(snapshot),
        })
    }

    /// The answer this run submitted. Null until something is submitted.
    async fn final_output(&self) -> Option<FinalOutput> {
        let run_id = self.meta.run_id.clone();
        blocking(move || crate::runstate::read_final_output(&run_id))
            .await
            .map(FinalOutput::from)
    }

    /// This run's direct children, paged.
    ///
    /// A connection rather than an array: a two-hundred-worker fan-out would
    /// otherwise be one unbounded response. Nest the field to walk deeper, one
    /// level per nesting, and read `pageInfo.hasNextPage` to see a level that
    /// was cut. For a flat read of a whole subtree, `runs(filter: { parent: })`
    /// is the same walk one page at a time.
    async fn children(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Page size for this level.", default = 50)] first: i32,
        #[graphql(desc = "How many of this run's children to skip.", default = 0)] skip: i32,
    ) -> async_graphql::Result<ChildConnection> {
        let state = ctx.data_unchecked::<AppState>();
        let limit = super::super::run_filter::page_size(first).gql()?;
        let skip = usize::try_from(skip)
            .map_err(|_| ServeError::BadRequest("`skip` cannot be negative".to_string()))
            .gql()?;
        // The index already holds the parent-to-children map, so a level is a
        // lookup rather than a scan of every run.
        let snapshot = state.caches.run_index.snapshot().await;
        let children: Vec<Arc<RunMeta>> = snapshot
            .under(Some(self.meta.run_id.as_str()))
            .cloned()
            .collect();
        let total = i32::try_from(children.len()).unwrap_or(i32::MAX);
        let page: Vec<Arc<RunMeta>> = children.iter().skip(skip).take(limit).cloned().collect();
        let has_next_page = children.len() > skip.saturating_add(page.len());
        let now = self.now;
        Ok(ChildConnection {
            edges: page
                .into_iter()
                .map(|meta| ChildEdge {
                    node: Run { meta, now },
                })
                .collect(),
            total,
            has_next_page,
        })
    }

    /// How deep and how wide this run's sub-agent tree is, without walking it.
    ///
    /// The roll-up covers the whole subtree, which is the figure a fan-out is
    /// judged by: a parent that spent little and whose fifty workers spent a
    /// great deal is not a cheap run.
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
    /// `stageIndex` picks one stage, `allStages` reads every stage in order, and
    /// neither reads more than `tail` bytes from the end of each stream. The cap
    /// is the server's, because `allStages` multiplies whatever the client asks
    /// for by the stage count.
    ///
    /// Empty is not null: a stage that has written nothing, an index no stage
    /// answers to and a file this server cannot read all read as no text.
    async fn logs(
        &self,
        #[graphql(desc = "One stage by index; omitted means the stage the run is on now.")]
        stage_index: Option<i32>,
        #[graphql(
            desc = "Every stage's logs in order, instead of one stage's.",
            default = false
        )]
        all_stages: bool,
        #[graphql(
            desc = "Read the operational log rather than the output stream.",
            default = false
        )]
        operational: bool,
        #[graphql(desc = "Bytes to read from the end of each stream.")] tail_bytes: Option<i32>,
    ) -> async_graphql::Result<String> {
        let selector = match (stage_index, all_stages) {
            (Some(_), true) => {
                return Err(ServeError::BadRequest(
                    "`stageIndex` names one stage and `allStages` names all of them, so they \
                     cannot be combined"
                        .to_string(),
                ))
                .gql();
            }
            (Some(index), false) => crate::runstate::StageSelector::Index(
                usize::try_from(index)
                    .map_err(|_| {
                        ServeError::BadRequest("`stageIndex` cannot be negative".to_string())
                    })
                    .gql()?,
            ),
            (None, true) => crate::runstate::StageSelector::All,
            (None, false) => crate::runstate::StageSelector::Current,
        };
        let stream = match operational {
            true => crate::runstate::LogStream::Operational,
            false => crate::runstate::LogStream::Output,
        };
        let bytes = match tail_bytes {
            None => DEFAULT_LOG_TAIL_BYTES,
            Some(asked) => u64::try_from(asked)
                .map_err(|_| ServeError::BadRequest("`tailBytes` cannot be negative".to_string()))
                .gql()?
                .min(MAX_LOG_TAIL_BYTES),
        };
        let run_id = self.meta.run_id.clone();
        Ok(
            blocking(move || crate::runstate::tail_run_logs(&run_id, selector, stream, bytes))
                .await,
        )
    }

    /// The binary parts this run holds.
    ///
    /// Metadata only: the bytes are behind each entry's `url`, a short-lived
    /// signed link the byte route verifies. Bytes never ride a query answer,
    /// and a page can put that link straight into an `<img src>`.
    async fn blobs(&self, ctx: &Context<'_>) -> Option<Vec<BlobEntry>> {
        let state = ctx.data_unchecked::<AppState>();
        let run_id = self.meta.run_id.clone();
        let stored = blocking(move || crate::blobs::list(&run_id)).await?;
        let now = leviath_core::duration::now_secs();
        Some(
            stored
                .into_iter()
                .map(|blob| {
                    let url = blob.stored.then(|| {
                        super::super::super::signed_url::signed_path(
                            &state.signer,
                            &format!("/api/agents/{}/blobs/{}", self.meta.run_id, blob.sha256),
                            &[],
                            now,
                        )
                    });
                    BlobEntry {
                        sha256: blob.sha256,
                        mime_type: blob.mime_type,
                        name: blob.name,
                        size: BigInt(blob.size as i64),
                        width: blob.width.and_then(|w| i32::try_from(w).ok()),
                        height: blob.height.and_then(|h| i32::try_from(h).ok()),
                        duration_ms: blob.duration_ms.and_then(|d| i32::try_from(d).ok()),
                        tokens: as_i32(blob.tokens),
                        regions: blob.regions,
                        stored: blob.stored,
                        url,
                    }
                })
                .collect(),
        )
    }

    /// The files this run handed back beside its answer.
    ///
    /// Same as `blobs`: metadata here, bytes behind a signed link.
    async fn artifacts(&self, ctx: &Context<'_>) -> Vec<Artifact> {
        let state = ctx.data_unchecked::<AppState>();
        self.meta
            .final_output
            .as_ref()
            .map(|output| output.artifacts.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|artifact| super::run_detail::artifact(state, &self.meta.run_id, artifact))
            .collect()
    }

    /// A short-lived signed link to one file in this run's working directory.
    ///
    /// The byte route verifies the signature, so this works in an `<img src>` or
    /// a download link, where a header cannot be set. It is good for a few
    /// minutes and for that one path.
    ///
    /// Minted without resolving the path, so a file this run never wrote is a
    /// 404 from the byte route rather than nothing here.
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
    async fn interaction(
        &self,
        ctx: &Context<'_>,
    ) -> async_graphql::Result<Option<super::super::events::InteractionRequest>> {
        let state = ctx.data_unchecked::<AppState>();
        let open = super::super::super::core::spawn::open_interactions(state)
            .await
            .gql()?;
        Ok(open
            .into_iter()
            .find(|(run_id, _)| run_id == &self.meta.run_id)
            .map(|(_, request)| super::super::events::InteractionRequest::from(request)))
    }

    /// The run's files: what it recorded changing, or what is in its working
    /// directory now.
    ///
    /// Two different questions. `MODIFIED` is the run's own record, which is
    /// free but capped at record time and a claim about the run rather than about
    /// the disk. `WORKDIR` is the truth, one directory level per request: that
    /// bound is the answer to a repository with a `node_modules` in it.
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
        #[graphql(desc = "Include dot-prefixed entries.", default = false)] hidden: bool,
    ) -> async_graphql::Result<FileListing> {
        let state = ctx.data_unchecked::<AppState>();
        let registry = state.current_config().mime_registry_or_defaults();
        let meta = Arc::clone(&self.meta);
        let dir = path.map(std::path::PathBuf::from);
        let listed = blocking(move || {
            files::listing(&meta, source.into(), dir.as_deref(), hidden, &registry)
        })
        .await
        .gql()?;
        Ok(FileListing::from(listed))
    }

    /// One window of one of the run's files, as text.
    ///
    /// At most a megabyte per read, because the answer travels inside this one.
    /// A larger file is read a window at a time: pass `nextOffset` back as
    /// `offset`, and the windows concatenate into the file. For bytes rather than
    /// text, and for anything that is not text at all, mint a `fileUrl` instead.
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
    async fn executions(
        &self,
        #[graphql(desc = "Page size.", default = 50)] first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<super::execution::ToolExecutionConnection> {
        super::execution::page(self.meta.run_id.clone(), first, after).await
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
    /// right now is not here yet: `interaction` carries that one while it is
    /// open. A run reading as `WAITING_INPUT` with nothing on its last page has
    /// asked something nobody has answered, rather than asked nothing.
    ///
    /// Empty for an unattended run, which asks nobody: `--yolo` answers before
    /// the question reaches a person, so an empty list on a run that plainly
    /// did something dangerous means exactly that.
    async fn interactions(
        &self,
        #[graphql(desc = "Page size.", default = 50)] first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<super::interaction::InteractionConnection> {
        super::interaction::page(self.meta.run_id.clone(), first, after).await
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
    async fn inferences(
        &self,
        #[graphql(desc = "Page size.", default = 50)] first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<super::inference::InferenceAttemptConnection> {
        super::inference::page(self.meta.run_id.clone(), first, after).await
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
    async fn context_changes(
        &self,
        #[graphql(desc = "Page size.", default = 50)] first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<super::context_change::ContextChangeConnection> {
        super::context_change::page(self.meta.run_id.clone(), first, after).await
    }

    /// Snapshots of this run's context window over the run, paged.
    ///
    /// What the window held at each point, not why it changed: `contextChanges`
    /// is the reasons. Each point here carries a whole window, so this is paged
    /// harder than the run listing is: ask for the regions you render rather
    /// than every point's every region. Chronological by default, which is also
    /// the cheaper direction to read.
    async fn context_history(
        &self,
        #[graphql(desc = "Page size.", default = 50)] first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
        #[graphql(desc = "Newest first instead of chronological.", default = false)]
        descending: bool,
    ) -> async_graphql::Result<ContextHistoryConnection> {
        let limit = usize::try_from(first)
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| ServeError::BadRequest("`first` must be at least 1".to_string()))
            .gql()?;
        if limit > history::HISTORY_MAX_LIMIT {
            return Err(ServeError::BadRequest(format!(
                "`first` may be at most {}, the history page cap: each point carries a whole \
                 context window",
                history::HISTORY_MAX_LIMIT
            )))
            .gql();
        }
        let order = match descending {
            true => "desc",
            false => "asc",
        };
        let run_id = self.meta.run_id.clone();
        let cursor = after.map(|cursor| cursor.0);
        let page = blocking(move || {
            let spec = history::HistorySpec::resolve(
                &run_id,
                Some(limit),
                Some(order),
                cursor.as_deref(),
            )?;
            history::page(&run_id, &spec)
        })
        .await
        .gql()?;
        let total = i32::try_from(page.total).unwrap_or(i32::MAX);
        let end_cursor = page.next_cursor.clone().map(Cursor);
        Ok(ContextHistoryConnection {
            edges: page
                .points
                .into_iter()
                .map(|point| ContextHistoryEdge {
                    cursor: Cursor(format!("{}", point.at)),
                    node: ContextSnapshotPoint {
                        at: Timestamp(point.at),
                        stage: point.meta.current_stage.clone(),
                        window: ContextWindow {
                            snapshot: Arc::new(point.context),
                        },
                    },
                })
                .collect(),
            page_info: super::super::connection::PageInfo {
                end_cursor: end_cursor.clone(),
                has_next_page: end_cursor.is_some(),
            },
            total,
        })
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
    async fn context_snapshot(
        &self,
        #[graphql(desc = "The window's revision.")] revision: String,
    ) -> Option<ContextSnapshotPoint> {
        let run_id = self.meta.run_id.clone();
        let point = blocking(move || history::at_revision(&run_id, &revision)).await;
        point.map(|point| ContextSnapshotPoint {
            at: Timestamp(point.at),
            stage: point.meta.current_stage.clone(),
            window: ContextWindow {
                snapshot: Arc::new(point.context),
            },
        })
    }

    /// A short-lived signed link to one stored part's bytes.
    ///
    /// The same kind of link `fileUrl` mints, and minted without asking the
    /// store whether the hash is held. `blobs` carries one per part already and
    /// nulls it for bytes that are gone; this is for a hash a client holds, and
    /// for the download form of a part it is showing inline.
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

/// One child run with nothing else attached.
///
/// No cursor: a level is read from the index's parent map in listing order, and
/// `skip` walks it. A keyset cursor would promise stability across a tree that
/// is growing under the reader, which is not something this can offer.
#[derive(SimpleObject)]
pub(crate) struct ChildEdge {
    /// The child run.
    pub(crate) node: Run,
}

/// One page of a run's direct children.
#[derive(SimpleObject)]
pub(crate) struct ChildConnection {
    /// The children on this page.
    pub(crate) edges: Vec<ChildEdge>,
    /// How many direct children this run has.
    pub(crate) total: i32,
    /// Whether another page follows.
    pub(crate) has_next_page: bool,
}

/// How deep and how wide a run's sub-agent tree is.
#[derive(Debug, SimpleObject)]
pub(crate) struct RunTreeStatus {
    /// Token roll-up over the whole subtree, this run included.
    pub(crate) rollup: TokenUsage,
    /// The deepest nesting below this run.
    pub(crate) depth: i32,
    /// How many runs are below it, at any depth.
    pub(crate) descendant_count: i32,
}

/// How much of a log stream is read when the client does not say.
const DEFAULT_LOG_TAIL_BYTES: u64 = 32 * 1024;

/// The most one request reads from each stream.
///
/// `allStages` multiplies whatever is asked for by the stage count, so the cap
/// is the server's rather than the client's.
const MAX_LOG_TAIL_BYTES: u64 = 1024 * 1024;

/// Narrow a daemon counter to the 32 bits GraphQL's `Int` carries.
///
/// These are iteration and tool-call counters, which a run reaches in the
/// thousands at most. Saturating rather than wrapping: if one ever did run
/// away, a client should read an implausible ceiling rather than a small
/// number that looks fine.
fn as_i32(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;

/// A signed link to one of a run's byte routes.
///
/// One place mints these, so the fields that hand them out cannot drift apart on
/// what a link looks like or how long it lasts.
fn signed(state: &AppState, route: &str, download: bool) -> String {
    let query: &[(&str, &str)] = match download {
        true => &[("download", "1")],
        false => &[],
    };
    super::super::super::signed_url::signed_path(
        &state.signer,
        route,
        query,
        leviath_core::duration::now_secs(),
    )
}

/// Where a run is, in its blueprint's own terms.
#[derive(Debug, SimpleObject)]
pub(crate) struct CurrentStage {
    /// The stage's name, as the blueprint spells it.
    pub(crate) name: String,
    /// Its position, counting from zero.
    pub(crate) index: i32,
    /// How many stages the blueprint has.
    pub(crate) of: i32,
}

/// One point in a run's history: the whole context window, as it stood.
#[derive(SimpleObject)]
pub(crate) struct ContextSnapshotPoint {
    /// When the window looked like this.
    pub(crate) at: Timestamp,
    /// The stage the run was in. Empty before the first stage is entered.
    pub(crate) stage: String,
    /// The window itself. Region contents are their own field, so asking for the
    /// shape of a hundred windows does not read a hundred windows' text.
    ///
    /// Its `revision` is this point's stable name: pass it to `contextSnapshot`
    /// to come back to exactly this content, whatever the run does next.
    pub(crate) window: ContextWindow,
}

/// One point with its cursor.
#[derive(SimpleObject)]
pub(crate) struct ContextHistoryEdge {
    /// The point.
    pub(crate) node: ContextSnapshotPoint,
    /// Cursor for this edge.
    pub(crate) cursor: Cursor,
}

/// A paged history of one run's context window.
#[derive(SimpleObject)]
pub(crate) struct ContextHistoryConnection {
    /// This page's points, in the order asked for.
    pub(crate) edges: Vec<ContextHistoryEdge>,
    /// Where the next page starts.
    pub(crate) page_info: super::super::connection::PageInfo,
    /// How many points the run's journal holds altogether.
    pub(crate) total: i32,
}

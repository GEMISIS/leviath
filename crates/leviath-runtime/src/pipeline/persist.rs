//! Per-agent snapshot writing and interaction-status reflection.

use super::*;

// ─── Persistence (per-agent snapshot writing) ────────────────────────────────

/// How long an agent may go without a snapshot before one is written purely to
/// refresh `updated_at`.
///
/// The watermark below debounces on *progress*, which means a run that is busy
/// but not progressing (one long inference, or a genuinely wedged one) writes
/// nothing at all. Observers then cannot tell "working" from "dead", because
/// `updated_at` looks equally old in both cases. A periodic beat makes a stale
/// timestamp mean something.
pub(crate) const PERSIST_HEARTBEAT_SECS: i64 = 30;

/// Longest log line the event broadcast carries; the on-disk stage logs keep
/// the full line. 8 KB shows any tool banner or error whole while keeping the
/// (never-shrinking) broadcast ring's worst-case floor at ring-size x this.
pub(crate) const BROADCAST_LOG_LINE_MAX_BYTES: usize = 8 * 1024;

/// Clone `line` for the event broadcast, truncated to
/// [`BROADCAST_LOG_LINE_MAX_BYTES`] on a char boundary with a marker so a
/// reader knows to fetch the stage log for the rest.
fn truncate_log_line(line: &str) -> String {
    if line.len() <= BROADCAST_LOG_LINE_MAX_BYTES {
        return line.to_string();
    }
    let cut = leviath_core::text::floor_char_boundary(line, BROADCAST_LOG_LINE_MAX_BYTES);
    format!(
        "{} [truncated {} bytes]",
        line.split_at(cut).0,
        line.len() - cut
    )
}

/// Debounce watermark: the (iteration, stage index, status) last persisted for an
/// agent. A snapshot is written only when one of these changes, so the world
/// writes on meaningful progress rather than every tick. `None` until the first
/// snapshot, so a freshly-spawned agent is always written once.
#[derive(Component, Default)]
pub struct PersistWatermark {
    last: Option<(usize, usize, leviath_core::run_meta::RunStatus)>,
    /// When the last snapshot was written, for the heartbeat above.
    last_written_at: Option<i64>,
    /// When the watermark itself last changed - that is, when the agent last
    /// actually moved.
    ///
    /// `last_written_at` cannot answer that: the heartbeat advances it whether
    /// or not anything happened, which is the whole point of the heartbeat and
    /// exactly why `meta.json`'s `updated_at` is not evidence of progress: a
    /// wedged run keeps a fresh one. This is the timestamp `lev ps` ages its
    /// rows against.
    last_progress_at: Option<i64>,
    /// The taint audit already on disk, as `(stage index, event count)`.
    ///
    /// The audit file is only rewritten when the gate recorded a new event.
    /// Without it every snapshot re-serializes the whole (append-only) log,
    /// an O(events) allocation per tick that grows with the run.
    last_taint: Option<(usize, usize)>,
    /// The run's `(title, title_error)` as of the last snapshot.
    ///
    /// A title arrives on its own schedule: it is generated beside the run's
    /// first turn and can land after the run's last move, changing nothing
    /// [`Self::last`] tracks. Without this it sits in memory until the next
    /// heartbeat, which a finished run is unloaded before reaching, and the
    /// name is lost. Compared by reference below; only a write clones.
    last_title: Option<(Option<String>, Option<String>)>,
}

impl PersistWatermark {
    /// Unix seconds when this agent last made progress (iteration, stage, or
    /// status changed). `None` before the first snapshot.
    pub(crate) fn last_progress_at(&self) -> Option<i64> {
        self.last_progress_at
    }

    /// The run status the last dispatched snapshot carried, if any - the proof
    /// that a given status has reached the persistence lane. Unloading
    /// decisions key on this: an entity may only be slimmed or paged out once
    /// the state being dropped is known to be on its way to disk.
    pub(crate) fn persisted_status(&self) -> Option<leviath_core::run_meta::RunStatus> {
        self.last.as_ref().map(|(_, _, status)| status.clone())
    }

    /// Move both stamps back to `at`, so a test can reach the heartbeat window
    /// without sleeping through it.
    #[cfg(test)]
    pub(crate) fn backdate(&mut self, at: i64) {
        self.last_written_at = Some(at);
        self.last_progress_at = Some(at);
    }

    /// Stamp the watermark as though a snapshot with `status` was dispatched,
    /// so unload tests can drive [`Self::persisted_status`] without running the
    /// full persistence schedule.
    #[cfg(test)]
    pub(crate) fn stamp_status(&mut self, status: leviath_core::run_meta::RunStatus) {
        self.last = Some((0, 0, status));
    }
}

/// The sending end of the persistence I/O lane (the receiving end is drained by
/// `persistence_bridge::persistence_worker`).
#[derive(Resource)]
pub(crate) struct PersistenceStage(pub UnboundedSender<PersistMsg>);

/// What `reflect_interaction_status` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type ReflectInteractionStatusQuery = (
    Entity,
    &'static mut AgentState,
    Option<&'static AwaitingInteraction>,
    Option<&'static mut StageProgress>,
);

/// Copy the scripts a run could not use onto its flags.
///
/// Folded in at persist time rather than written when the validator failed: the
/// tool path holds a `&` borrow of the component and has no world access, so the
/// component records the names and this is where they meet the run's flags.
///
/// Overwritten rather than appended, because the component holds the whole set -
/// the last write is the complete answer, and appending would repeat a script on
/// every heartbeat.
fn fold_broken_scripts(
    flags: &mut crate::persistence::RunOutcomeFlags,
    validators: Option<&crate::components::OutputValidators>,
) {
    if let Some(validators) = validators {
        flags.0.broken_scripts = validators.broken_names();
    }
}

/// Persistence-dispatch system: for each agent carrying run metadata whose
/// (iteration, stage, status) has changed since its last snapshot, build the
/// `meta.json` + `context.json` value snapshot and hand it to the persistence
/// lane. Fire-and-forget - no result to collect; the single-worker lane keeps a
/// given agent's writes ordered. Agents without [`RunMetadata`] aren't persisted.
/// Interaction-status reflection system: mirror the shared [`InteractionHub`]'s
/// open requests into agent status so a blocked agent shows as `Waiting` (and
/// the dashboard / `lev ps` surface its prompt) instead of a silent `Active`.
///
/// An agent's `ask_user_*` / tool-approval / plan-approval call blocks deep in
/// the async tool lane, invisible to the ECS - which otherwise leaves the agent
/// `Active` with meta.json written `running`, so the dashboard (gated on
/// `WaitingInput`) never shows the prompt and the run looks frozen. This system
/// closes that gap: an agent whose id has an open hub request flips
/// `Active → Waiting` (tagged [`AwaitingInteraction`]); when the request clears
/// it flips back `Waiting → Active`. No-op when the world has no hub resource
/// (test worlds).
///
/// Agents parked by the engine rather than by a prompt - fan-out parents
/// ([`FanOutWaiting`]) and stages holding for sub-agents
/// ([`WaitingForChildren`]) - are excluded. Their `Waiting` belongs to whoever
/// set it, and the clearing arm below would otherwise walk them back to `Active`
/// the moment an unrelated prompt of theirs resolved, un-parking a run whose
/// children are still going.
///
/// The wait is also kept off the stage clock. A prompt waits for a person for
/// as long as it takes, and `stuck_after_minutes` measures the agent, not the
/// person: when the prompt resolves, the stage's `stage_started_at` moves
/// forward by however long it was parked, so the next `detect_stuck_stage` sees
/// the same elapsed time it would have seen had the answer been instant.
pub(crate) fn reflect_interaction_status(
    hub: Option<Res<InteractionHub>>,
    mut agents: Query<
        ReflectInteractionStatusQuery,
        (Without<FanOutWaiting>, Without<WaitingForChildren>),
    >,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    let Some(hub) = hub else { return };
    let pending: std::collections::HashSet<String> =
        hub.pending().into_iter().map(|(id, _)| id).collect();
    let now = chrono::Utc::now().timestamp();
    for (entity, mut state, marked, progress) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        match (pending.contains(&state.agent_id), marked.is_some()) {
            // Newly blocked on a prompt: surface it as Waiting.
            (true, false) => {
                if state.status == AgentStatus::Active {
                    state.status = AgentStatus::Waiting;
                    commands.entity(entity).insert(AwaitingInteraction);
                    if let Some(mut progress) = progress {
                        progress.waiting_since = Some(now);
                    }
                }
            }
            // Request cleared (answered / cancelled): return to Active, unless
            // the agent has since reached a terminal status.
            (false, true) => {
                commands.entity(entity).remove::<AwaitingInteraction>();
                if state.status == AgentStatus::Waiting {
                    state.status = AgentStatus::Active;
                }
                if let Some(mut progress) = progress {
                    credit_wait_to_stage_clock(&mut progress, now);
                }
            }
            _ => {}
        }
    }
}

/// Move the stage clock past a wait on a person that has just ended.
///
/// `stage_started_at` is stamped lazily by `detect_stuck_stage`, so a stage
/// that parked before its first inference has no clock yet and nothing to
/// credit. Only a wait that actually started (`waiting_since` set) counts.
fn credit_wait_to_stage_clock(progress: &mut StageProgress, now: i64) {
    let Some(since) = progress.waiting_since.take() else {
        return;
    };
    let waited = (now - since).max(0);
    if let Some(started) = progress.stage_started_at.as_mut() {
        *started += waited;
    }
}

/// Reconcile a [`StageLedger`]'s per-stage `status` + timestamps against the
/// agent's current stage index and status.
///
/// The cursor stage takes the mapped agent status and is marked entered. Every
/// other stage is judged on whether it has *ever* been entered, not on where it
/// sits relative to the cursor: one the run has been in and left is `Complete`,
/// one it has not is `Pending` while the run is live and
/// [`Skipped`](leviath_core::run_meta::StageRunStatus::Skipped) once the run is
/// over.
///
/// Position cannot stand in for "has run": that only holds for a linear
/// blueprint. A graph reaches its stages in whatever order its edges describe,
/// so a branch the run went past without taking would be filed as `Complete`
/// with an empty `region_tokens` - and since that map holds the high-water mark
/// each region reached rather than what the stage itself added, an empty one in
/// the middle of the sequence makes the next real stage look like it wrote
/// every region from nothing.
///
/// `started_at`/`ended_at` are stamped once and never overwritten, so repeated
/// calls are idempotent.
///
/// `running` is the run's working clock (see
/// [`clock_runs`](leviath_core::run_meta::clock_runs)), passed in rather than
/// derived from `status` because the agent status alone cannot tell a stage
/// parked on a person from one parked on its own sub-agents. The cursor stage
/// tracks it; every other stage's clock is stopped, because a stage the run has
/// left is not working no matter what the run is doing.
pub(crate) fn reconcile_stage_ledger(
    ledger: &mut StageLedger,
    cursor_index: usize,
    status: &AgentStatus,
    now: i64,
    running: bool,
) {
    use leviath_core::run_meta::StageRunStatus;
    let active = crate::persistence::stage_status_from(status);
    let run_is_over = super::is_terminal_status(status);
    for rec in ledger.0.iter_mut() {
        if rec.index == cursor_index {
            rec.entered = true;
            if rec.started_at.is_none() {
                rec.started_at = Some(now);
            }
            rec.active.get_or_insert_default().observe(now, running);
            // The visit in progress keeps the same clock as the stage it is
            // part of. Opening one is `enter_stage`'s job, not this function's:
            // a visit conjured here would have a boundary at a persist tick
            // rather than at the transition, which is the misattribution the
            // per-visit split exists to remove.
            rec.observe_visit(now, running);
            if active == StageRunStatus::Complete && rec.ended_at.is_none() {
                rec.ended_at = Some(now);
            }
            // The run stopped in this stage, so the visit it was on stopped too.
            // Left open, the last visit of every finished run would read as
            // still running and grow without bound for whoever renders it.
            if run_is_over {
                rec.close_visit(now);
            }
            rec.status = active.clone();
            continue;
        }
        // Billed tokens count as evidence as well as the flag. Reconcile runs
        // on the persist tick rather than on stage entry, so resting "did this
        // run" entirely on having been observed as the cursor would report a
        // stage that somehow slipped between two ticks as never entered - and
        // calling a stage that did work `Skipped` is a worse error than the one
        // being fixed. A stage with tokens against its name ran.
        // Not the cursor, so not working, whatever the run is doing.
        rec.active.get_or_insert_default().observe(now, false);
        rec.entered |= rec.prompt_tokens > 0 || rec.completion_tokens > 0;
        if !rec.entered {
            rec.status = match run_is_over {
                true => StageRunStatus::Skipped,
                false => StageRunStatus::Pending,
            };
            continue;
        }
        // Entered earlier and not the current stage, so it has been left. A
        // stage that loops back becomes the cursor again and is re-marked.
        rec.status = StageRunStatus::Complete;
        if rec.ended_at.is_none() {
            rec.ended_at = Some(now);
        }
        // Belt and braces for the visit `enter_stage` did not close: a run
        // whose stage record shows tokens it never appeared as the cursor for
        // (the case the `entered` line above exists for) would otherwise leave
        // a visit open on a stage the run is demonstrably not in.
        rec.close_visit(now);
    }
}

/// What `dispatch_persistence` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type PersistenceQuery = (
    Entity,
    &'static RunMetadata,
    &'static AgentState,
    &'static ContextWindow,
    &'static StageCursor,
    &'static TokenTotals,
    &'static mut PersistWatermark,
    Option<&'static mut StageLedger>,
    Option<&'static mut StageIoBuffer>,
    Option<&'static crate::taint::TaintGate>,
    Option<&'static crate::components::ParentRef>,
    Option<&'static crate::components::SubAgentChildren>,
    Option<&'static crate::fanout::FanOutWaiting>,
    (
        Option<&'static crate::interaction_points::AwaitingInteractionPoint>,
        Option<&'static crate::interaction_points::InteractionPointCursor>,
        Option<&'static crate::interaction_points::InteractionPointRounds>,
        Option<&'static crate::persistence::RunOutcomeFlags>,
        Option<&'static crate::components::OutputValidators>,
        Option<&'static crate::persistence::FinalOutput>,
        // The remaining reasons a run can be parked. Read here because this is
        // where they are queryable, and recorded on `meta.json` so a client
        // does not have to reconstruct them from what it can see.
        Option<&'static crate::gate_prompt::AwaitingGatePrompt>,
        Option<&'static super::WaitingForChildren>,
        Option<&'static crate::components::AwaitingInteraction>,
        Option<&'static super::PausedForSetup>,
        // Optional so a world that builds agents by hand (tests, embedded
        // hosts) still persists; those runs simply keep no working clock and
        // fall back to wall-clock age when read.
        Option<&'static mut crate::persistence::RunClock>,
    ),
);

/// Hand each agent's current state to the persistence lane, which writes it to
/// disk off the schedule thread.
///
/// Coalescing lives here rather than in the lane: an agent whose digest has not
/// changed since its last send is skipped, so a world full of idle runs costs
/// nothing per tick.
pub(crate) fn dispatch_persistence(
    mut agents: Query<PersistenceQuery>,
    stage: Res<PersistenceStage>,
    hub: Option<Res<InteractionHub>>,
    sink: Option<Res<crate::host::WorldEventSink>>,
) {
    crate::tick_scope::clear();
    for (
        entity,
        md,
        state,
        window,
        cursor,
        totals,
        mut watermark,
        mut ledger,
        buffer,
        taint_gate,
        parent_ref,
        children,
        fan_out_waiting,
        (
            awaiting_point,
            ip_cursor,
            ip_rounds,
            outcome_flags,
            validators,
            final_output,
            gate_prompt,
            waiting_for_children,
            awaiting_interaction,
            paused_for_setup,
            clock,
        ),
    ) in agents.iter_mut()
    {
        crate::tick_scope::enter(entity);
        let now = chrono::Utc::now().timestamp();

        let status = crate::persistence::run_status_from(&state.status);
        // The parking markers, gathered here because this is where they are
        // queryable, and recorded on `meta.json` so a client does not have to
        // reconstruct them from what it can see.
        //
        // `interaction` is left for the write path below. Naming which prompt is
        // holding the run costs a scan of the hub, and it only ever refines a
        // wait that the other markers have already established - so the clock,
        // which runs every tick, does not pay for it.
        let mut parked = leviath_core::run_meta::WaitMarkers {
            gate_prompt: gate_prompt.is_some_and(|g| g.0 > 0),
            interaction_point: awaiting_point.is_some(),
            fan_out_outstanding: fan_out_waiting.map(|f| f.outstanding()),
            // The count needs each child's status, which this query cannot
            // reach; the listing computes it live. Recording the reason without
            // the number is the honest half.
            children_outstanding: waiting_for_children
                .map(|_| children.map(|c| c.children.len()).unwrap_or(0)),
            interaction: None,
            awaiting_interaction: awaiting_interaction.is_some(),
            needs_setup: paused_for_setup.map(|p| leviath_core::run_meta::SetupNeeded {
                blocker: p.blocker,
                remedy: p.remedy.clone(),
            }),
        };

        // Advance the run's working clock every tick, not only on the writes
        // below: what it measures is time, and a run that pauses and resumes
        // between two heartbeats would otherwise have both transitions land on
        // the same reading and cancel out.
        let running = leviath_core::run_meta::clock_runs(
            &status,
            leviath_core::run_meta::wait_reason_from(
                matches!(state.status, AgentStatus::Waiting | AgentStatus::Paused),
                &parked,
            )
            .as_ref(),
        );
        let active = clock.map(|mut clock| {
            clock.0.observe(now, running);
            clock.0
        });

        // Reconcile the stage ledger every persist tick so status/timestamps track
        // the agent regardless of whether the run-level watermark changed.
        if let Some(ledger) = ledger.as_deref_mut() {
            reconcile_stage_ledger(ledger, cursor.index, &state.status, now, running);
        }

        // Always flush any buffered per-stage output/log lines.
        let (output_appends, log_appends) = match buffer {
            Some(mut buf) => (
                std::mem::take(&mut buf.output),
                std::mem::take(&mut buf.logs),
            ),
            None => (Vec::new(), Vec::new()),
        };
        let has_appends = !output_appends.is_empty() || !log_appends.is_empty();

        let current = (state.iteration, cursor.index, status);
        let watermark_changed = watermark.last.as_ref() != Some(&current);
        // Deliberately *not* folded into `current`: a title is not the agent
        // moving, so it must not advance `last_progress_at` and make a wedged
        // run look alive. It only earns a write.
        let title_now = (md.title.as_deref(), md.title_error.as_deref());
        let title_changed = watermark
            .last_title
            .as_ref()
            .map(|(t, e)| (t.as_deref(), e.as_deref()))
            != Some(title_now);
        // Beat even when nothing changed, so `updated_at` distinguishes a run
        // that is slow from one that nothing is driving.
        let due_for_heartbeat = watermark
            .last_written_at
            .is_none_or(|at| now.saturating_sub(at) >= PERSIST_HEARTBEAT_SECS);
        if !watermark_changed && !title_changed && !has_appends && !due_for_heartbeat {
            continue; // nothing meaningful changed, nothing buffered, beat not due
        }

        // Stream each buffered line to WS subscribers as a `Log` event (in
        // addition to the disk append below). No-op in worlds without the sink
        // (test / `lev run`); a zero-subscriber `send` error is ignored.
        //
        // Truncated for the broadcast only - the full line still reaches the
        // stage log on disk. The ring retains every slot's strings until the
        // slot is overwritten, so an assistant's whole multi-KB turn broadcast
        // per line made the ring a multi-MB permanent floor after any busy run.
        if let Some(sink) = &sink {
            for (_idx, line) in output_appends.iter().chain(log_appends.iter()) {
                // `Res<T>` derefs to `T` in bevy_ecs 0.19; it is not a tuple struct.
                let _ = sink.0.send(crate::host::WorldEvent::Log {
                    run_id: md.run_id.clone(),
                    agent_id: state.agent_id.clone(),
                    line: truncate_log_line(line),
                });
            }
        }

        // Buffered lines with no real progress and no heartbeat due: journal
        // just the lines. The full path below deep-clones the whole context
        // window per snapshot, and tool activity buffers lines several times
        // per iteration - snapshotting on each batch multiplied the lane's
        // biggest allocation by the run's tool traffic for no new state.
        if !watermark_changed && !title_changed && !due_for_heartbeat {
            let _ = stage.0.send(PersistMsg::StageLines {
                run_id: md.run_id.clone(),
                output_appends,
                log_appends,
            });
            continue;
        }

        if watermark_changed {
            watermark.last = Some(current);
            watermark.last_progress_at = Some(now);
        }
        if title_changed {
            watermark.last_title = Some((md.title.clone(), md.title_error.clone()));
        }
        watermark.last_written_at = Some(now);

        // Tree links, for a deterministic restart-time rebuild of the graph.
        let depth = parent_ref.map(|p| p.depth).unwrap_or(0);
        let max_child_depth = children.map(|c| c.max_child_depth).unwrap_or(0);
        let mut flags = outcome_flags.cloned().unwrap_or_default();
        fold_broken_scripts(&mut flags, validators);
        // Read the progress stamp *after* the update above, so a write that
        // carried progress reports `now` and a heartbeat-only write reports
        // whenever the run last moved. That difference is the whole signal: it is
        // what lets an observer reading `meta.json` tell a slow run from a wedged
        // one, which `updated_at` (which is `now` either way) cannot.
        // Now name the prompt, on the path that writes it.
        parked.interaction = hub.as_ref().and_then(|h| {
            h.pending()
                .into_iter()
                .find(|(agent_id, _)| *agent_id == state.agent_id)
                .map(|(_, req)| req.kind)
        });
        let meta = build_run_meta(
            crate::persistence::RunMetaSources {
                md,
                state,
                totals,
                flags: &flags,
                final_output,
                parked,
            },
            crate::persistence::RunPosition {
                stage_index: cursor.index,
                now_secs: now,
                last_progress_at: watermark.last_progress_at(),
                depth,
                max_child_depth,
                active,
            },
        );
        let context = build_context_snapshot(window, &state.current_stage);
        let stages = ledger.as_deref().map(|l| l.0.clone()).unwrap_or_default();
        // Persist the taint gate's audit log (per-stage) when it gained events
        // since the last write, so security decisions are inspectable after
        // the fact. The log is append-only, so an unchanged (stage, count)
        // means the file on disk is already current - re-serializing the whole
        // log every heartbeat was an O(events) allocation that grew with the
        // run.
        let taint_audit = taint_gate
            .filter(|g| !g.audit_log().is_empty())
            .and_then(|g| {
                let key = (cursor.index, g.audit_log().len());
                if watermark.last_taint == Some(key) {
                    return None;
                }
                watermark.last_taint = Some(key);
                Some((
                    cursor.index,
                    serde_json::to_string(g.audit_log())
                        .expect("GateEvent slice always serializes"),
                ))
            });
        // A parent parked mid fan-out: persist its waiting state so the
        // split/merge resumes after a restart (removed once it's no longer
        // waiting - see the writer).
        let fanout = fan_out_waiting
            .map(|w| serde_json::to_string(&w.to_state()).expect("FanOutState always serializes"));
        // An agent parked at a stage-boundary interaction point: persist the open
        // point (cursor/round + the reviewed document) so a restart re-presents the
        // same prompt rather than dropping it and re-inferring. The
        // document comes from the open request in the hub - which is present by the
        // time `reflect_interaction_status` (running just before this system) has
        // flipped the agent to `Waiting`. If the request isn't registered yet, skip
        // this tick; the next persist captures it (removing any stale sidecar).
        let interactions = awaiting_point.and_then(|_| {
            let request = hub
                .as_ref()?
                .pending()
                .into_iter()
                .find(|(aid, req)| aid == &state.agent_id && req.id.contains("-point-"))?;
            let ip_state = crate::interaction_points::InteractionPointState {
                cursor: ip_cursor.map_or(0, |c| c.0),
                round: ip_rounds.map_or(0, |r| r.0),
                body: request.1.body.unwrap_or_default(),
            };
            Some(serde_json::to_string(&ip_state).expect("InteractionPointState always serializes"))
        });
        // Always carry the answer's bytes when the agent holds them; the
        // persistence lane decides whether they still need writing.
        //
        // Skipping here on a watermark advanced when the job is *built* would
        // assume every job built gets written, and the lane does not promise
        // that: it coalesces queued snapshots per run and keeps only the
        // newest. A run that finishes inside one persistence window would have
        // the job carrying the body dropped as superseded, while every later
        // job carries `None` and still rewrites `meta.json` with the
        // descriptor - leaving the descriptor and the sidecar permanently
        // disagreeing, which `read_final_output` reads as "no answer".
        //
        // The skip lives in the lane instead, past the coalescing, where "did
        // this get written" is a fact rather than an assumption; it stops a
        // heartbeat rewriting a quarter-megabyte file every thirty seconds.
        // The cost here is one clone of the answer per snapshot, on a path
        // that already deep-clones the whole context window.
        let final_output_body = final_output.map(|o| o.0.content.clone());
        let _ = stage.0.send(PersistMsg::Snapshot(Box::new(PersistJob {
            run_id: md.run_id.clone(),
            meta,
            context,
            stages,
            output_appends,
            log_appends,
            taint_audit,
            final_output: final_output_body,
            fanout,
            interactions,
        })));
    }
}

#[cfg(test)]
mod broken_script_tests {
    use super::fold_broken_scripts;
    use crate::components::OutputValidators;
    use crate::persistence::RunOutcomeFlags;

    /// A run with no validators leaves the flag alone rather than clearing it,
    /// which is what an agent naming no validator looks like.
    #[test]
    fn no_validators_leaves_the_flags_untouched() {
        let mut flags = RunOutcomeFlags::default();
        flags.0.broken_scripts = vec!["kept.rhai".to_string()];
        fold_broken_scripts(&mut flags, None);
        assert_eq!(flags.0.broken_scripts, vec!["kept.rhai".to_string()]);
    }

    /// The names the component collected reach the run's flags, which is what
    /// puts them on `meta.json`, `lev ps`, the API and the dashboard.
    #[test]
    fn the_components_names_reach_the_flags() {
        let validators = OutputValidators::new(std::collections::HashMap::new());
        validators.note_broken("shape.rhai");
        validators.note_broken("other.rhai");

        let mut flags = RunOutcomeFlags::default();
        fold_broken_scripts(&mut flags, Some(&validators));

        assert_eq!(
            flags.0.broken_scripts,
            vec!["other.rhai".to_string(), "shape.rhai".to_string()],
            "sorted, so two writes of the same set match"
        );
    }

    /// Overwritten, not appended: the component holds the whole set, so a
    /// heartbeat that folds again must not repeat what it folded last time.
    #[test]
    fn folding_twice_does_not_repeat_a_script() {
        let validators = OutputValidators::new(std::collections::HashMap::new());
        validators.note_broken("shape.rhai");

        let mut flags = RunOutcomeFlags::default();
        fold_broken_scripts(&mut flags, Some(&validators));
        fold_broken_scripts(&mut flags, Some(&validators));

        assert_eq!(flags.0.broken_scripts, vec!["shape.rhai".to_string()]);
    }

    /// A healthy run says so by carrying an empty list.
    #[test]
    fn a_run_with_working_validators_records_nothing() {
        let validators = OutputValidators::new(std::collections::HashMap::new());
        let mut flags = RunOutcomeFlags::default();
        fold_broken_scripts(&mut flags, Some(&validators));
        assert!(flags.0.broken_scripts.is_empty());
    }
}

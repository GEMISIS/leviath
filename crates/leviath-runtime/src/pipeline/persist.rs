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
    /// exactly why `meta.json`'s `updated_at` is not evidence of progress. Issue
    /// #184 was reported on the strength of a fresh `updated_at`, so this is the
    /// timestamp `lev ps` ages its rows against.
    last_progress_at: Option<i64>,
    /// The answer already on disk, as `(submitted_at, bytes)`.
    ///
    /// The sidecar is only rewritten when this differs from what the agent
    /// holds. Without it the heartbeat would rewrite a quarter-megabyte file
    /// every thirty seconds for the whole life of a long run, to no effect.
    last_output: Option<(i64, usize)>,
    /// The taint audit already on disk, as `(stage index, event count)`.
    ///
    /// The audit file is only rewritten when the gate recorded a new event.
    /// Without it every snapshot re-serialized the whole (append-only) log,
    /// an O(events) allocation per tick that grew with the run.
    last_taint: Option<(usize, usize)>,
}

impl PersistWatermark {
    /// Unix seconds when this agent last made progress (iteration, stage, or
    /// status changed). `None` before the first snapshot.
    pub fn last_progress_at(&self) -> Option<i64> {
        self.last_progress_at
    }

    /// Move both stamps back to `at`, so a test can reach the heartbeat window
    /// without sleeping through it.
    #[cfg(test)]
    pub(crate) fn backdate(&mut self, at: i64) {
        self.last_written_at = Some(at);
        self.last_progress_at = Some(at);
    }
}

/// The sending end of the persistence I/O lane (the receiving end is drained by
/// `persistence_bridge::persistence_worker`).
#[derive(Resource)]
pub struct PersistenceStage(pub UnboundedSender<PersistMsg>);

/// Persistence-dispatch system: for each agent carrying run metadata whose
/// (iteration, stage, status) has changed since its last snapshot, build the
/// `meta.json` + `context.json` value snapshot and hand it to the persistence
/// lane. Fire-and-forget - no result to collect; the single-worker lane keeps a
/// given agent's writes ordered. Agents without [`RunMetadata`] aren't persisted.
#[allow(clippy::type_complexity)]
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
pub fn reflect_interaction_status(
    hub: Option<Res<InteractionHub>>,
    mut agents: Query<
        (Entity, &mut AgentState, Option<&AwaitingInteraction>),
        (Without<FanOutWaiting>, Without<WaitingForChildren>),
    >,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    let Some(hub) = hub else { return };
    let pending: std::collections::HashSet<String> =
        hub.pending().into_iter().map(|(id, _)| id).collect();
    for (entity, mut state, marked) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        match (pending.contains(&state.agent_id), marked.is_some()) {
            // Newly blocked on a prompt: surface it as Waiting.
            (true, false) => {
                if state.status == AgentStatus::Active {
                    state.status = AgentStatus::Waiting;
                    commands.entity(entity).insert(AwaitingInteraction);
                }
            }
            // Request cleared (answered / cancelled): return to Active, unless
            // the agent has since reached a terminal status.
            (false, true) => {
                commands.entity(entity).remove::<AwaitingInteraction>();
                if state.status == AgentStatus::Waiting {
                    state.status = AgentStatus::Active;
                }
            }
            _ => {}
        }
    }
}

/// Reconcile a [`StageLedger`]'s per-stage `status` + timestamps against the
/// agent's current stage index and status: stages before the cursor are
/// `Complete`, the cursor stage takes the mapped agent status, later stages stay
/// `Pending`. `started_at`/`ended_at` are stamped once and never overwritten, so
/// repeated calls are idempotent.
pub(crate) fn reconcile_stage_ledger(
    ledger: &mut StageLedger,
    cursor_index: usize,
    status: &AgentStatus,
    now: i64,
) {
    use leviath_core::run_meta::StageRunStatus;
    let active = crate::persistence::stage_status_from(status);
    for rec in ledger.0.iter_mut() {
        match rec.index.cmp(&cursor_index) {
            std::cmp::Ordering::Less => {
                if rec.started_at.is_none() {
                    rec.started_at = Some(now);
                }
                rec.status = StageRunStatus::Complete;
                if rec.ended_at.is_none() {
                    rec.ended_at = Some(now);
                }
            }
            std::cmp::Ordering::Equal => {
                if rec.started_at.is_none() {
                    rec.started_at = Some(now);
                }
                if active == StageRunStatus::Complete && rec.ended_at.is_none() {
                    rec.ended_at = Some(now);
                }
                rec.status = active.clone();
            }
            std::cmp::Ordering::Greater => {
                rec.status = StageRunStatus::Pending;
            }
        }
    }
}

#[allow(clippy::type_complexity)]
pub fn dispatch_persistence(
    mut agents: Query<(
        Entity,
        &RunMetadata,
        &AgentState,
        &ContextWindow,
        &StageCursor,
        &TokenTotals,
        &mut PersistWatermark,
        Option<&mut StageLedger>,
        Option<&mut StageIoBuffer>,
        Option<&crate::taint::TaintGate>,
        Option<&crate::components::ParentRef>,
        Option<&crate::components::SubAgentChildren>,
        Option<&crate::fanout::FanOutWaiting>,
        (
            Option<&crate::interaction_points::AwaitingInteractionPoint>,
            Option<&crate::interaction_points::InteractionPointCursor>,
            Option<&crate::interaction_points::InteractionPointRounds>,
            Option<&crate::persistence::RunOutcomeFlags>,
            Option<&crate::persistence::FinalOutput>,
        ),
    )>,
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
        (awaiting_point, ip_cursor, ip_rounds, outcome_flags, final_output),
    ) in agents.iter_mut()
    {
        crate::tick_scope::enter(entity);
        let now = chrono::Utc::now().timestamp();

        // Reconcile the stage ledger every persist tick so status/timestamps track
        // the agent regardless of whether the run-level watermark changed.
        if let Some(ledger) = ledger.as_deref_mut() {
            reconcile_stage_ledger(ledger, cursor.index, &state.status, now);
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

        let status = crate::persistence::run_status_from(&state.status);
        let current = (state.iteration, cursor.index, status);
        let watermark_changed = watermark.last.as_ref() != Some(&current);
        // Beat even when nothing changed, so `updated_at` distinguishes a run
        // that is slow from one that nothing is driving.
        let due_for_heartbeat = watermark
            .last_written_at
            .is_none_or(|at| now.saturating_sub(at) >= PERSIST_HEARTBEAT_SECS);
        if !watermark_changed && !has_appends && !due_for_heartbeat {
            continue; // nothing meaningful changed, nothing buffered, beat not due
        }

        // Stream each buffered line to WS subscribers as a `Log` event (in
        // addition to the disk append below). No-op in worlds without the sink
        // (test / `lev run`); a zero-subscriber `send` error is ignored.
        if let Some(sink) = &sink {
            for (_idx, line) in output_appends.iter().chain(log_appends.iter()) {
                // `Res<T>` derefs to `T` in bevy_ecs 0.19; it is not a tuple struct.
                let _ = sink.0.send(crate::host::WorldEvent::Log {
                    run_id: md.run_id.clone(),
                    agent_id: state.agent_id.clone(),
                    line: line.clone(),
                });
            }
        }

        // Buffered lines with no real progress and no heartbeat due: journal
        // just the lines. The full path below deep-clones the whole context
        // window per snapshot, and tool activity buffers lines several times
        // per iteration - snapshotting on each batch multiplied the lane's
        // biggest allocation by the run's tool traffic for no new state.
        if !watermark_changed && !due_for_heartbeat {
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
        watermark.last_written_at = Some(now);

        // Tree links, for a deterministic restart-time rebuild of the graph.
        let depth = parent_ref.map(|p| p.depth).unwrap_or(0);
        let max_child_depth = children.map(|c| c.max_child_depth).unwrap_or(0);
        let flags = outcome_flags.cloned().unwrap_or_default();
        // Read the progress stamp *after* the update above, so a write that
        // carried progress reports `now` and a heartbeat-only write reports
        // whenever the run last moved. That difference is the whole signal: it is
        // what lets an observer reading `meta.json` tell a slow run from a wedged
        // one, which `updated_at` (which is `now` either way) cannot.
        let meta = build_run_meta(
            md,
            state,
            totals,
            &flags,
            cursor.index,
            now,
            watermark.last_progress_at(),
            depth,
            max_child_depth,
            final_output,
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
        // same prompt rather than dropping it and re-inferring (issue #38). The
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
        // Send the answer's bytes only when they are not already on disk. The
        // stamp-and-size pair identifies a submission: `submit_output` replaces
        // rather than appends, so a new answer always carries a later stamp.
        let output_key = final_output.map(|o| (o.0.submitted_at, o.0.content.len()));
        let final_output_body = match output_key != watermark.last_output {
            true => {
                watermark.last_output = output_key;
                final_output.map(|o| o.0.content.clone())
            }
            false => None,
        };
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

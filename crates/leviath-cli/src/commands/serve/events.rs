//! The WebSocket event vocabulary: what `/ws` and `/ws/agents/{id}` send.
//!
//! Split out of `types.rs` because this is a wire contract rather than an
//! internal shape. Every variant here is something a client matches on by its
//! `type` tag, so a change to one is a change to the API, and
//! `API_CAPABILITIES` in `config_types.rs` is where that gets announced.

use serde::Serialize;

use super::types::FinalOutputResp;

/// Events broadcast to WebSocket subscribers.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    /// A run's spend passed a figure the operator asked to be told about.
    ///
    /// Sent once per threshold per run. The point is to arrive while the run is
    /// still going: a run that quietly spent far more than intended looked, from
    /// outside, exactly like one making ordinary progress.
    AgentSpend {
        /// The agent's live id in the world.
        agent_id: String,
        /// The durable run id, and what a per-run subscription filters on.
        run_id: String,
        /// The figure that was crossed, in dollars.
        threshold_usd: f64,
        /// What the run has spent so far, in dollars.
        total_usd: f64,
        /// Whether every call behind that total could be priced. When false the
        /// run has spent at least this, and more by an unknown amount, so it
        /// must not be shown as a final figure.
        ///
        /// A complete total can still be a reconstruction from published rates
        /// rather than the provider's own figure; that is a separate question,
        /// and `cost_is_exact` on the run record is what answers it.
        complete: bool,
        /// The stage that was running when it crossed. The full per-stage
        /// breakdown is in the run's `stages.json`.
        stage: String,
    },

    /// Where a run stands, re-sent whenever any of it changes.
    AgentStatus {
        /// The agent's live id in the world.
        agent_id: String,
        /// The durable run id, and what a per-run subscription filters on.
        run_id: String,
        /// The run's status, as `RunStatus` renders it.
        status: String,
        /// The stage it is in, by name.
        stage: String,
        /// Inference turns taken in that stage, reset on entering a new one.
        iteration: usize,
        /// Tool calls made across the whole run.
        #[serde(default)]
        tool_calls: usize,
        /// Whether a client may send this run a mid-run message. False for a
        /// stage that declared `accepts_messages = false`, and for a run that
        /// has finished.
        accepts_messages: bool,
        /// Why the run is parked, when it is.
        ///
        /// Omitted for a run that is moving. Without it a subscriber sees a
        /// run turn `waiting` or `paused` and has to fetch the run to learn
        /// whether somebody is needed, which is the guess this vocabulary
        /// exists to remove.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wait_reason: Option<leviath_core::run_meta::WaitReason>,
        /// The run's generated title, once it has one.
        ///
        /// [`RunRenamed`](Self::RunRenamed) is what announces the rename the
        /// moment it happens; this is the same fact carried on every later
        /// status frame, so a client that connected or reconnected after that
        /// moment picks the name up without fetching the run.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// The run acquired a title, or had the one it was showing replaced.
    ///
    /// A run is created untitled and named a moment later, once a model has
    /// shortened its prompt into one. That is the one field guaranteed to
    /// change shortly after a run starts and then never again, and the title is
    /// the run's name in every list, notification and tab - so a client without
    /// this frame either polls each new run or shows the wrong name until
    /// something unrelated makes it re-read.
    RunRenamed {
        /// The agent's live id in the world.
        agent_id: String,
        /// The durable run id.
        run_id: String,
        /// The title the run now goes by.
        title: String,
    },
    /// How full the run's context window is, after a turn changed it.
    ContextUpdate {
        /// The agent's live id in the world.
        agent_id: String,
        /// The durable run id.
        run_id: String,
        /// Tokens held across every region.
        total_tokens: usize,
        /// The whole window's budget.
        max_tokens: usize,
    },
    /// One log line, as also written to the stage's log files.
    Log {
        /// The agent's live id in the world.
        agent_id: String,
        /// The durable run id.
        run_id: String,
        /// The line, without a trailing newline. Long lines are truncated for
        /// the broadcast; the on-disk stage log keeps the whole thing.
        line: String,
    },
    /// The run is blocked on a person, and this is what it is asking.
    InteractionNeeded {
        /// The agent's live id in the world.
        agent_id: String,
        /// The durable run id.
        run_id: String,
        /// The prompt, forwarded as the runtime serialized it, so a new kind of
        /// request needs no server release.
        request: serde_json::Value,
    },
    /// A run started, including one spawned as a child of another.
    AgentSpawned {
        /// The agent's live id in the world.
        agent_id: String,
        /// The durable run id.
        run_id: String,
        /// The parent's agent id when this is a sub-agent, `None` at the root.
        parent_id: Option<String>,
        /// The blueprint it was spawned from.
        blueprint: String,
    },
    /// A run reached a terminal status.
    AgentCompleted {
        /// The agent's live id in the world.
        agent_id: String,
        /// The durable run id.
        run_id: String,
        /// The terminal status, as `RunStatus` renders it.
        status: String,
        /// The run's *error*, if it failed. Named `result` since before a run
        /// could produce one; kept for the consumers that read it.
        result: Option<String>,
        /// What the run handed back. This is the answer.
        #[serde(skip_serializing_if = "Option::is_none")]
        final_output: Option<FinalOutputResp>,
    },
    /// Running token totals for the whole run, after an inference landed.
    Tokens {
        /// The agent's live id in the world.
        agent_id: String,
        /// The durable run id.
        run_id: String,
        /// Input tokens billed so far.
        prompt_tokens: usize,
        /// Output tokens billed so far.
        completion_tokens: usize,
        /// Input tokens served from the provider's prompt cache, counted within
        /// `prompt_tokens` rather than on top of it.
        #[serde(default)]
        cached_tokens: usize,
        /// Tokens written into the provider's prompt cache.
        #[serde(default)]
        cache_write_tokens: usize,
    },
    /// The run entered a new stage.
    ///
    /// The initial stage arrives as [`AgentSpawned`](Self::AgentSpawned), not
    /// as one of these: there is no stage to come from.
    StageTransition {
        /// The agent's live id in the world.
        agent_id: String,
        /// The durable run id.
        run_id: String,
        /// The stage being left.
        from: String,
        /// The stage being entered.
        to: String,
        /// How many times the destination stage has been entered, this entry
        /// included.
        iteration: usize,
    },
    /// A tool call was handed to the async tool lane.
    ///
    /// Calls that resolve inline (context tools, refusals, gate blocks) never
    /// reach the lane, so they produce no start and no finish; they still
    /// count towards the tool total on [`AgentStatus`](Self::AgentStatus).
    ToolCallStarted {
        /// The agent's live id in the world.
        agent_id: String,
        /// The durable run id.
        run_id: String,
        /// The provider-assigned call id, which pairs this with its finish.
        call_id: String,
        /// The tool's name.
        tool: String,
    },
    /// A lane-executed tool call returned, paired with its start by `call_id`.
    ToolCallFinished {
        /// The agent's live id in the world.
        agent_id: String,
        /// The durable run id.
        run_id: String,
        /// The provider-assigned call id, matching the start event's.
        call_id: String,
        /// The tool's name.
        tool: String,
        /// Whether the call took effect. False for an `[error]`, `[blocked]`
        /// or `[unavailable]` result.
        ok: bool,
        /// The result, flattened to one line and truncated.
        summary: String,
    },
    /// A step of an update started by `POST /api/update` changed.
    ///
    /// About the machine rather than about a run, like
    /// [`DaemonLink`](Self::DaemonLink) - so a per-run subscription does not
    /// receive one, and `/ws` does.
    ///
    /// Sent as it happens, because that is the whole reason the route answers
    /// with a job id instead of holding the request open: a `brew upgrade` is a
    /// download and an install, and a console that can only say "updating" for
    /// a minute is a console that cannot say whether anything is happening.
    UpdateProgress {
        /// The job this is about, as `POST /api/update` answered with.
        job_id: String,
        /// `binary`, `agents` or `migrations`.
        step: String,
        /// `running`, `done`, `skipped`, `advised` or `failed`.
        status: String,
        /// One line about what just happened, ready to print.
        detail: String,
    },
    /// An update started by `POST /api/update` reached a terminal status.
    ///
    /// Carries the whole job record rather than a summary, so a client that
    /// connected mid-run or dropped a frame renders the result without a
    /// follow-up request.
    UpdateFinished {
        /// The job that finished.
        job_id: String,
        /// `complete` if every step that ran succeeded, `failed` otherwise.
        status: String,
        /// Whether the binary on disk is now newer than the processes serving
        /// this. Both this server and the daemon keep running the old build
        /// until they are restarted, so a console that reported the version it
        /// can see would be telling the truth in the least useful way possible.
        restart_required: bool,
        /// The same record `GET /api/update/jobs/{id}` returns.
        job: serde_json::Value,
    },

    /// This server's own link to the daemon changed.
    ///
    /// Sent when the daemon's event stream drops and when it is back, and once
    /// on connect so a subscriber that arrives mid-outage learns what it is
    /// looking at. It is about no run in particular, so every subscription
    /// receives it, per-run ones included: a run's events simply stop while
    /// the daemon is down, and this is what says why.
    DaemonLink {
        /// Whether the server is receiving the daemon's events right now.
        connected: bool,
        /// The daemon on the other end, once it has said who it is.
        #[serde(skip_serializing_if = "Option::is_none")]
        daemon: Option<leviath_runtime::control_socket::DaemonIdentity>,
        /// Whether the daemon behind the link is a different process than the
        /// one before it - a restart this server lived through and its clients
        /// need not.
        restarted: bool,
        /// Present when the daemon and this server run different code, with
        /// the remedy: restart `lev serve` to match. Requests keep working
        /// while the two still understand each other; one that fails for this
        /// reason answers 502 with the same text.
        #[serde(skip_serializing_if = "Option::is_none")]
        restart_advised: Option<String>,
    },
}

impl ServerEvent {
    /// The run id this event belongs to, for per-run subscription filtering.
    /// Every event names one except the three about the machine rather than a
    /// run - [`DaemonLink`](Self::DaemonLink), and the two an update sends.
    pub fn run_id(&self) -> &str {
        match self {
            ServerEvent::AgentStatus { run_id, .. }
            | ServerEvent::RunRenamed { run_id, .. }
            | ServerEvent::ContextUpdate { run_id, .. }
            | ServerEvent::Log { run_id, .. }
            | ServerEvent::InteractionNeeded { run_id, .. }
            | ServerEvent::AgentSpawned { run_id, .. }
            | ServerEvent::AgentCompleted { run_id, .. }
            | ServerEvent::Tokens { run_id, .. }
            | ServerEvent::StageTransition { run_id, .. }
            | ServerEvent::ToolCallStarted { run_id, .. }
            | ServerEvent::ToolCallFinished { run_id, .. }
            | ServerEvent::AgentSpend { run_id, .. } => run_id,
            ServerEvent::DaemonLink { .. }
            | ServerEvent::UpdateProgress { .. }
            | ServerEvent::UpdateFinished { .. } => "",
        }
    }

    /// Whether a subscription filtered to `run_id` should receive this event:
    /// its own run's events, and the ones about no run at all.
    ///
    /// The update frames are deliberately not in that second group. A link
    /// event explains why a run's events stopped arriving, which is something
    /// a per-run subscriber has to know; an update happening on the machine is
    /// not about the run it is watching, and `/ws` is where a console watches
    /// for it.
    pub fn is_for_run(&self, run_id: &str) -> bool {
        matches!(self, ServerEvent::DaemonLink { .. }) || self.run_id() == run_id
    }

    /// The [`DaemonLink`](Self::DaemonLink) event for what `control` currently
    /// knows, given whether the event stream is up and whether the daemon
    /// behind it just changed.
    pub(super) fn daemon_link(
        control: &leviath_runtime::control_socket::ControlClient,
        connected: bool,
        restarted: bool,
    ) -> Self {
        ServerEvent::DaemonLink {
            connected,
            daemon: control.link().daemon,
            restarted,
            restart_advised: control.code_mismatch().map(|m| m.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The link event is about no run: it filters as the empty run id, and a
    /// per-run subscription still receives it.
    #[test]
    fn a_daemon_link_event_is_for_every_subscription() {
        let link = ServerEvent::DaemonLink {
            connected: false,
            daemon: None,
            restarted: false,
            restart_advised: None,
        };
        assert_eq!(link.run_id(), "");
        assert!(link.is_for_run("run-1"));
        let log = ServerEvent::Log {
            agent_id: "a".to_string(),
            run_id: "run-1".to_string(),
            line: "x".to_string(),
        };
        assert!(log.is_for_run("run-1"));
        assert!(!log.is_for_run("run-2"));
        // Absent fields stay absent on the wire.
        let json = serde_json::to_value(&link).unwrap();
        assert_eq!(json["type"], "daemon_link");
        assert!(json.get("daemon").is_none());
        assert!(json.get("restart_advised").is_none());
    }

    #[test]
    fn server_event_agent_status_serialization() {
        let event = ServerEvent::AgentStatus {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            status: "running".to_string(),
            stage: "implement".to_string(),
            iteration: 5,
            tool_calls: 12,
            accepts_messages: true,
            wait_reason: None,
            title: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"agent_status\""));
        assert!(json.contains("\"agent_id\":\"coder\""));
        assert!(json.contains("\"iteration\":5"));
        assert!(json.contains("\"tool_calls\":12"));
    }

    #[test]
    fn server_event_tokens_serialization() {
        let event = ServerEvent::Tokens {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            prompt_tokens: 5000,
            completion_tokens: 1200,
            cached_tokens: 200,
            cache_write_tokens: 100,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"tokens\""));
        assert!(json.contains("\"prompt_tokens\":5000"));
        assert!(json.contains("\"cached_tokens\":200"));
        assert!(json.contains("\"cache_write_tokens\":100"));
    }

    #[test]
    fn server_event_agent_spawned_serialization() {
        let event = ServerEvent::AgentSpawned {
            agent_id: "coder".to_string(),
            run_id: "run-456".to_string(),
            parent_id: Some("run-123".to_string()),
            blueprint: "coder".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"agent_spawned\""));
        assert!(json.contains("\"parent_id\":\"run-123\""));
    }

    #[test]
    fn server_event_agent_completed_serialization() {
        let event = ServerEvent::AgentCompleted {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            status: "complete".to_string(),
            result: Some("success".to_string()),
            final_output: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"agent_completed\""));
    }

    #[test]
    fn server_event_context_update_serialization() {
        let event = ServerEvent::ContextUpdate {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            total_tokens: 10000,
            max_tokens: 200000,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"context_update\""));
        assert!(json.contains("\"total_tokens\":10000"));
    }

    #[test]
    fn server_event_interaction_needed_serialization() {
        let event = ServerEvent::InteractionNeeded {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            request: serde_json::json!({"prompt": "approve?"}),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"interaction_needed\""));
    }

    #[test]
    fn server_event_log_serialization() {
        let event = ServerEvent::Log {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            line: "doing work".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"log\""));
        assert!(json.contains("\"line\":\"doing work\""));
    }

    #[test]
    fn server_event_run_id_covers_every_variant() {
        let cases: Vec<(ServerEvent, &str)> = vec![
            (
                ServerEvent::AgentStatus {
                    agent_id: "a".to_string(),
                    run_id: "r1".to_string(),
                    status: "active".to_string(),
                    stage: "s".to_string(),
                    iteration: 0,
                    tool_calls: 0,
                    accepts_messages: false,
                    wait_reason: None,
                    title: None,
                },
                "r1",
            ),
            (
                ServerEvent::RunRenamed {
                    agent_id: "a".to_string(),
                    run_id: "r1b".to_string(),
                    title: "A short name".to_string(),
                },
                "r1b",
            ),
            (
                ServerEvent::ContextUpdate {
                    agent_id: "a".to_string(),
                    run_id: "r2".to_string(),
                    total_tokens: 1,
                    max_tokens: 2,
                },
                "r2",
            ),
            (
                ServerEvent::Log {
                    agent_id: "a".to_string(),
                    run_id: "r3".to_string(),
                    line: "l".to_string(),
                },
                "r3",
            ),
            (
                ServerEvent::InteractionNeeded {
                    agent_id: "a".to_string(),
                    run_id: "r4".to_string(),
                    request: serde_json::Value::Null,
                },
                "r4",
            ),
            (
                ServerEvent::AgentSpawned {
                    agent_id: "a".to_string(),
                    run_id: "r5".to_string(),
                    parent_id: None,
                    blueprint: "b".to_string(),
                },
                "r5",
            ),
            (
                ServerEvent::AgentCompleted {
                    agent_id: "a".to_string(),
                    run_id: "r6".to_string(),
                    status: "complete".to_string(),
                    result: None,
                    final_output: None,
                },
                "r6",
            ),
            (
                ServerEvent::Tokens {
                    agent_id: "a".to_string(),
                    run_id: "r7".to_string(),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                "r7",
            ),
            (
                ServerEvent::StageTransition {
                    agent_id: "a".to_string(),
                    run_id: "r8".to_string(),
                    from: "plan".to_string(),
                    to: "implement".to_string(),
                    iteration: 1,
                },
                "r8",
            ),
            (
                ServerEvent::ToolCallStarted {
                    agent_id: "a".to_string(),
                    run_id: "r9".to_string(),
                    call_id: "c1".to_string(),
                    tool: "read_file".to_string(),
                },
                "r9",
            ),
            (
                ServerEvent::ToolCallFinished {
                    agent_id: "a".to_string(),
                    run_id: "r10".to_string(),
                    call_id: "c1".to_string(),
                    tool: "read_file".to_string(),
                    ok: true,
                    summary: "ok".to_string(),
                },
                "r10",
            ),
            (
                ServerEvent::AgentSpend {
                    agent_id: "a".to_string(),
                    run_id: "r11".to_string(),
                    threshold_usd: 25.0,
                    total_usd: 27.5,
                    complete: true,
                    stage: "analyze".to_string(),
                },
                "r11",
            ),
        ];
        for (ev, want) in cases {
            assert_eq!(ev.run_id(), want);
        }
    }
}

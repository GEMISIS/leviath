//! `lev msg` / `lev cancel` — control operations on a running agent in the
//! shared-world daemon.
//!
//! Both send a control request over the daemon socket and report the boolean
//! outcome. The request/response cores are tested here; the socket-path
//! resolution + connect live in the binary behind [`crate::dispatch::RiskyExecutors`].

use anyhow::bail;
use leviath_core::interaction::{
    ApprovalScope, InteractionKind, InteractionRequest, InteractionResponse,
};
use leviath_runtime::control_socket::{ControlClient, ControlRequest, ControlResponse};

/// Arguments for `lev msg`.
#[derive(clap::Args, Debug, Clone)]
pub struct MsgArgs {
    /// The target agent id.
    pub agent_id: String,
    /// The message to deliver.
    pub content: String,
}

/// Arguments for `lev cancel`.
#[derive(clap::Args, Debug, Clone)]
pub struct CancelArgs {
    /// The run id to cancel.
    pub run_id: String,
}

/// Arguments for `lev respond` — answer a pending `ask_user` interaction the
/// daemon is holding, or (with no `request_id`) list the open interactions.
#[derive(clap::Args, Debug, Clone)]
pub struct RespondArgs {
    /// The interaction request id to answer. Omit to list open interactions.
    pub request_id: Option<String>,
    /// Free-text (or edited) answer value.
    pub value: Option<String>,
    /// Answer a multiple-choice interaction by 0-based option index.
    #[arg(long)]
    pub choice: Option<usize>,
    /// Approve a tool-approval / confirm interaction.
    #[arg(long, conflicts_with = "deny")]
    pub approve: bool,
    /// Deny a tool-approval / confirm interaction.
    #[arg(long)]
    pub deny: bool,
    /// With `--approve`, allow the tool for the rest of the session.
    #[arg(long)]
    pub session: bool,
}

/// Send `request` and report the boolean outcome: `ok` prints `applied_msg`, a
/// `false` outcome the `not_found_msg`. A non-`Ok` response or a connect failure
/// is an error.
async fn send_bool(
    client: &ControlClient,
    request: ControlRequest,
    applied_msg: &str,
    not_found_msg: &str,
) -> anyhow::Result<()> {
    match client.request(&request).await {
        Ok(ControlResponse::Ok { ok: true }) => {
            println!("{applied_msg}");
            Ok(())
        }
        Ok(ControlResponse::Ok { ok: false }) => bail!("{not_found_msg}"),
        Ok(other) => bail!("unexpected daemon response: {other:?}"),
        Err(e) => bail!("the leviath daemon is not reachable ({e}); start it with `lev daemon`"),
    }
}

/// `lev msg`: deliver a message to a running agent.
pub async fn send_message(client: &ControlClient, args: &MsgArgs) -> anyhow::Result<()> {
    send_bool(
        client,
        ControlRequest::Message {
            agent_id: args.agent_id.clone(),
            content: args.content.clone(),
            target_region: None,
        },
        "message delivered",
        "no agent accepted the message",
    )
    .await
}

/// `lev cancel`: cancel a run.
pub async fn cancel_run(client: &ControlClient, args: &CancelArgs) -> anyhow::Result<()> {
    send_bool(
        client,
        ControlRequest::Cancel {
            run_id: args.run_id.clone(),
        },
        "cancelled",
        "no such run",
    )
    .await
}

/// A short human label for an interaction kind (used by the `lev respond` list).
fn kind_label(kind: &InteractionKind) -> &'static str {
    match kind {
        InteractionKind::FreeText => "free-text",
        InteractionKind::MultipleChoice => "choice",
        InteractionKind::Confirm => "confirm",
        InteractionKind::ToolApproval => "tool-approval",
        InteractionKind::EditText => "edit-text",
    }
}

/// Render one open interaction as a multi-line listing entry.
fn format_interaction(agent_id: &str, req: &InteractionRequest) -> String {
    let mut s = format!(
        "{}  [{}]  agent={}  stage={}\n  {}",
        req.id,
        kind_label(&req.kind),
        agent_id,
        req.stage_name,
        req.prompt
    );
    for (i, opt) in req.options.iter().enumerate() {
        s.push_str(&format!("\n    {i}) {opt}"));
    }
    if let Some(tool) = &req.tool_name {
        s.push_str(&format!("\n    tool: {tool}"));
    }
    s
}

/// Build the [`InteractionResponse`] implied by the CLI flags. Approve/deny wins,
/// then an explicit `--choice`, otherwise a free-text value (empty if omitted).
fn build_response(request_id: &str, args: &RespondArgs) -> InteractionResponse {
    if args.approve || args.deny {
        let scope = if args.session {
            ApprovalScope::Session
        } else {
            ApprovalScope::Once
        };
        InteractionResponse::approval(request_id, args.approve, scope)
    } else if let Some(index) = args.choice {
        InteractionResponse::choice(request_id, index)
    } else {
        InteractionResponse::text(request_id, args.value.clone().unwrap_or_default())
    }
}

/// List the interactions the daemon is currently holding.
async fn list_interactions(client: &ControlClient) -> anyhow::Result<()> {
    match client.request(&ControlRequest::ListInteractions).await {
        Ok(ControlResponse::Interactions { interactions }) => {
            if interactions.is_empty() {
                println!("no open interactions");
            } else {
                for (agent_id, req) in &interactions {
                    println!("{}", format_interaction(agent_id, req));
                }
            }
            Ok(())
        }
        Ok(other) => bail!("unexpected daemon response: {other:?}"),
        Err(e) => bail!("the leviath daemon is not reachable ({e}); start it with `lev daemon`"),
    }
}

/// `lev respond`: answer a pending interaction, or list open ones when no
/// `request_id` is given.
pub async fn respond(client: &ControlClient, args: &RespondArgs) -> anyhow::Result<()> {
    match &args.request_id {
        None => list_interactions(client).await,
        Some(request_id) => {
            send_bool(
                client,
                ControlRequest::AnswerInteraction {
                    response: build_response(request_id, args),
                },
                "answered",
                "no such open interaction",
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_runtime::control_socket::{ControlId, bind_control_listener, control_id};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::task::JoinHandle;

    /// Bind a control listener at a fresh id under `dir` and serve one canned
    /// response, returning the id clients connect to and the server task.
    fn fake_daemon(dir: &std::path::Path, response_line: String) -> (ControlId, JoinHandle<()>) {
        let id = control_id(dir);
        let mut listener = bind_control_listener(&id).unwrap();
        let handle = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let _request = lines.next_line().await.unwrap();
            write_half
                .write_all(response_line.as_bytes())
                .await
                .unwrap();
            write_half.write_all(b"\n").await.unwrap();
        });
        (id, handle)
    }

    /// Run `op` against a fake daemon that replies `response_line`.
    async fn with_daemon<F, Fut>(response_line: impl Into<String>, op: F) -> anyhow::Result<()>
    where
        F: FnOnce(ControlClient) -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<()>>,
    {
        let dir = tempfile::tempdir().unwrap();
        let (id, server) = fake_daemon(dir.path(), response_line.into());
        let result = op(ControlClient::new(id)).await;
        server.await.unwrap();
        result
    }

    fn msg_args() -> MsgArgs {
        MsgArgs {
            agent_id: "a".to_string(),
            content: "hi".to_string(),
        }
    }

    #[tokio::test]
    async fn message_applied() {
        let r = with_daemon(r#"{"result":"ok","ok":true}"#, |c| async move {
            send_message(&c, &msg_args()).await
        })
        .await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn message_not_delivered() {
        let r = with_daemon(r#"{"result":"ok","ok":false}"#, |c| async move {
            send_message(&c, &msg_args()).await
        })
        .await;
        assert!(r.unwrap_err().to_string().contains("no agent accepted"));
    }

    #[tokio::test]
    async fn cancel_applied() {
        let r = with_daemon(r#"{"result":"ok","ok":true}"#, |c| async move {
            cancel_run(
                &c,
                &CancelArgs {
                    run_id: "r".to_string(),
                },
            )
            .await
        })
        .await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn cancel_unknown_run() {
        let r = with_daemon(r#"{"result":"ok","ok":false}"#, |c| async move {
            cancel_run(
                &c,
                &CancelArgs {
                    run_id: "r".to_string(),
                },
            )
            .await
        })
        .await;
        assert!(r.unwrap_err().to_string().contains("no such run"));
    }

    #[tokio::test]
    async fn unexpected_response_is_an_error() {
        let r = with_daemon(r#"{"result":"spawned","run_id":"x"}"#, |c| async move {
            cancel_run(
                &c,
                &CancelArgs {
                    run_id: "r".to_string(),
                },
            )
            .await
        })
        .await;
        assert!(r.unwrap_err().to_string().contains("unexpected"));
    }

    #[tokio::test]
    async fn not_reachable_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let client = ControlClient::new(control_id(&dir.path().join("no-daemon")));
        let err = cancel_run(
            &client,
            &CancelArgs {
                run_id: "r".to_string(),
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not reachable"));
    }

    // ─── lev respond ──────────────────────────────────────────────────────────

    fn respond_args() -> RespondArgs {
        RespondArgs {
            request_id: Some("q1".to_string()),
            value: None,
            choice: None,
            approve: false,
            deny: false,
            session: false,
        }
    }

    #[test]
    fn build_response_free_text_uses_value_or_empty() {
        let with_value = build_response(
            "q1",
            &RespondArgs {
                value: Some("hello".to_string()),
                ..respond_args()
            },
        );
        assert_eq!(with_value, InteractionResponse::text("q1", "hello"));
        // Missing value → empty string.
        assert_eq!(
            build_response("q1", &respond_args()),
            InteractionResponse::text("q1", "")
        );
    }

    #[test]
    fn build_response_choice_selects_index() {
        let r = build_response(
            "q1",
            &RespondArgs {
                choice: Some(2),
                ..respond_args()
            },
        );
        assert_eq!(r, InteractionResponse::choice("q1", 2));
    }

    #[test]
    fn build_response_approve_and_deny_and_session_scope() {
        let approved = build_response(
            "q1",
            &RespondArgs {
                approve: true,
                ..respond_args()
            },
        );
        assert_eq!(
            approved,
            InteractionResponse::approval("q1", true, ApprovalScope::Once)
        );
        let session = build_response(
            "q1",
            &RespondArgs {
                approve: true,
                session: true,
                ..respond_args()
            },
        );
        assert_eq!(
            session,
            InteractionResponse::approval("q1", true, ApprovalScope::Session)
        );
        let denied = build_response(
            "q1",
            &RespondArgs {
                deny: true,
                ..respond_args()
            },
        );
        assert_eq!(
            denied,
            InteractionResponse::approval("q1", false, ApprovalScope::Once)
        );
    }

    #[test]
    fn kind_label_covers_every_kind() {
        for (kind, label) in [
            (InteractionKind::FreeText, "free-text"),
            (InteractionKind::MultipleChoice, "choice"),
            (InteractionKind::Confirm, "confirm"),
            (InteractionKind::ToolApproval, "tool-approval"),
            (InteractionKind::EditText, "edit-text"),
        ] {
            assert_eq!(kind_label(&kind), label);
        }
    }

    #[test]
    fn format_interaction_renders_options_and_tool() {
        let mut req = InteractionRequest::multiple_choice(
            "q1",
            "Pick",
            vec!["a".to_string(), "b".to_string()],
            "plan",
        );
        req.tool_name = Some("bash".to_string());
        let out = format_interaction("agent-x", &req);
        assert!(out.contains("q1  [choice]  agent=agent-x  stage=plan"));
        assert!(out.contains("Pick"));
        assert!(out.contains("0) a"));
        assert!(out.contains("1) b"));
        assert!(out.contains("tool: bash"));
    }

    #[tokio::test]
    async fn respond_answers_an_interaction() {
        let r = with_daemon(r#"{"result":"ok","ok":true}"#, |c| async move {
            respond(&c, &respond_args()).await
        })
        .await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn respond_reports_no_open_interaction() {
        let r = with_daemon(r#"{"result":"ok","ok":false}"#, |c| async move {
            respond(&c, &respond_args()).await
        })
        .await;
        assert!(r.unwrap_err().to_string().contains("no such open"));
    }

    #[tokio::test]
    async fn respond_lists_open_interactions() {
        let req = InteractionRequest::free_text("q1", "What now?", "plan", true);
        let line = serde_json::to_string(&ControlResponse::Interactions {
            interactions: vec![("agent-a".to_string(), req)],
        })
        .unwrap();
        let r = with_daemon(line, |c| async move {
            respond(
                &c,
                &RespondArgs {
                    request_id: None,
                    ..respond_args()
                },
            )
            .await
        })
        .await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn respond_lists_when_none_open() {
        let line = serde_json::to_string(&ControlResponse::Interactions {
            interactions: vec![],
        })
        .unwrap();
        let r = with_daemon(line, |c| async move {
            respond(
                &c,
                &RespondArgs {
                    request_id: None,
                    ..respond_args()
                },
            )
            .await
        })
        .await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn respond_list_rejects_unexpected_response() {
        let r = with_daemon(r#"{"result":"ok","ok":true}"#, |c| async move {
            respond(
                &c,
                &RespondArgs {
                    request_id: None,
                    ..respond_args()
                },
            )
            .await
        })
        .await;
        assert!(r.unwrap_err().to_string().contains("unexpected"));
    }

    #[tokio::test]
    async fn respond_list_errors_when_daemon_absent() {
        let dir = tempfile::tempdir().unwrap();
        let client = ControlClient::new(control_id(&dir.path().join("no-daemon")));
        let err = respond(
            &client,
            &RespondArgs {
                request_id: None,
                ..respond_args()
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not reachable"));
    }
}

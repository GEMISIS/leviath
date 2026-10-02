//! What a daemon that is still starting tells the clients that reach it.
//!
//! A daemon binds its control socket first, so a second one is refused at
//! once, and then does its start-up work: providers, MCP servers, and on the
//! first start after an upgrade, backing up the home and converting what an
//! earlier release left in it. That can take a while, and a client that
//! connects meanwhile is answered rather than left waiting in silence: every
//! request but `authenticate` gets [`ControlResponse::Starting`] with the
//! step under way and how far along it is, and nothing reaches the world. A
//! [`ControlClient`](super::ControlClient) waits that out, showing each
//! answer to whoever is waiting, and sends its request again once the daemon
//! is ready.
//!
//! The start-up steps write to a [`StartupBoard`]; the control channel reads
//! it. Once the host is serving, the [`ControlGate`] sends every new
//! connection to it instead.

use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedSender;

use super::{ControlToken, DaemonIdentity};
use crate::host::{ControlOp, WorldEvent};

/// The start-up step a daemon is on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupProgress {
    /// What it is doing, in a few words (`converting runs`).
    pub step: String,
    /// How many of the step's items are done.
    #[serde(default)]
    pub done: u64,
    /// How many items the step has; 0 for a step that is not counted.
    #[serde(default)]
    pub total: u64,
    /// Where the step's work goes, when that is worth saying (the backup
    /// directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl std::fmt::Display for StartupProgress {
    /// `converting runs 412/982`, or the step alone when it is not counted.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = match self.total {
            0 => String::new(),
            total => format!(" {}/{total}", self.done),
        };
        write!(f, "{}{count}", self.step)
    }
}

/// Where the daemon's start-up steps say what they are doing, for the
/// control channel to read. Cloning shares it.
#[derive(Debug, Clone, Default)]
pub struct StartupBoard(Arc<Mutex<StartupProgress>>);

impl StartupBoard {
    /// Begin `step`, with `total` items to go (0 when it is not counted).
    pub fn begin(&self, step: &str, total: u64) {
        let mut now = leviath_core::sync::lock(&self.0);
        *now = StartupProgress {
            step: step.to_string(),
            done: 0,
            total,
            detail: None,
        };
    }

    /// Say where the current step's work goes.
    pub fn detail(&self, detail: impl Into<String>) {
        leviath_core::sync::lock(&self.0).detail = Some(detail.into());
    }

    /// `done` of the current step's items are finished.
    pub fn done(&self, done: u64) {
        leviath_core::sync::lock(&self.0).done = done;
    }

    /// The step under way now.
    pub fn current(&self) -> StartupProgress {
        leviath_core::sync::lock(&self.0).clone()
    }
}

/// How long the accept loop waits after an accept fails.
const ACCEPT_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

/// What a served connection needs from the host.
type HostEnds = (UnboundedSender<ControlOp>, broadcast::Sender<WorldEvent>);

/// Where the daemon's accept loop sends each connection: to the start-up
/// answer until [`open`](Self::open) is called, to the host after.
#[derive(Clone)]
pub struct ControlGate {
    board: StartupBoard,
    host: Arc<OnceLock<HostEnds>>,
}

impl ControlGate {
    /// A gate whose start-up answers come from `board`.
    pub fn new(board: StartupBoard) -> Self {
        Self {
            board,
            host: Arc::default(),
        }
    }

    /// The host is serving: every connection from now on reaches it.
    pub fn open(&self, op_tx: UnboundedSender<ControlOp>, events: broadcast::Sender<WorldEvent>) {
        let _ = self.host.set((op_tx, events));
    }

    /// Accept connections on `listener` for the daemon's life, each served
    /// through this gate on a task of its own. A peer the listener refuses
    /// (another user) is skipped, and an accept that fails (the process out
    /// of file descriptors, say) is tried again after a pause rather than
    /// leaving a running daemon nobody can reach.
    pub fn accept_all(
        &self,
        mut listener: super::ControlListener,
        token: ControlToken,
        identity: DaemonIdentity,
    ) -> tokio::task::JoinHandle<()> {
        let gate = self.clone();
        tokio::spawn(async move {
            loop {
                let accepted = listener.accept().await;
                let pause = accepted
                    .as_ref()
                    .map_or(ACCEPT_RETRY, |_| std::time::Duration::ZERO);
                accepted.into_iter().flatten().for_each(|stream| {
                    let (gate, token, identity) = (gate.clone(), token.clone(), identity.clone());
                    tokio::spawn(async move {
                        let _ = gate.serve(stream, Some(token), identity).await;
                    });
                });
                tokio::time::sleep(pause).await;
            }
        })
    }

    /// Serve one accepted connection: through the host once the gate is
    /// open, else with the start-up answer to every request.
    pub async fn serve<S>(
        &self,
        stream: S,
        token: Option<ControlToken>,
        identity: DaemonIdentity,
    ) -> std::io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let (op_tx, events, starting) = match self.host.get() {
            Some((op_tx, events)) => (op_tx.clone(), events.clone(), None),
            None => (
                tokio::sync::mpsc::unbounded_channel().0,
                broadcast::channel(1).0,
                Some(&self.board),
            ),
        };
        super::handle_connection_capped(
            stream,
            op_tx,
            events,
            token,
            identity,
            super::MAX_REQUEST_BYTES,
            starting,
        )
        .await
    }
}

#[cfg(test)]
#[path = "startup_tests.rs"]
mod tests;

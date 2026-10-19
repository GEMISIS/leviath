//! What keeps a WebSocket honest, shared by `/ws` and `/ws/graphql`.
//!
//! A peer that goes away without closing - a sleeping laptop, a killed tab, a
//! NAT that dropped the mapping - looks exactly like a quiet one. The server
//! pings on a cadence and gives up on a peer that stops answering, and every
//! frame it sends has a deadline, so a dead peer costs a minute at most rather
//! than a receiver and its buffers until TCP keepalive fires hours later.

use axum::extract::ws::{Message, WebSocket};
use futures_util::SinkExt;
use futures_util::stream::SplitSink;
use tokio::time::{Instant, Interval};

/// How often the server pings an idle-or-not connection. Browsers answer pings
/// automatically, so a live client never trips the deadline below.
pub(super) const WS_PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);

/// How long the oldest unanswered ping may stay unanswered before the peer is
/// declared dead.
///
/// Deliberately a separate knob from [`WS_PING_INTERVAL`] rather than "gone by
/// the next ping": the cadence answers "how often do we check", the deadline
/// answers "how long do we wait", and conflating them meant a single dropped
/// pong - one lost packet, one paused tab that resumed a moment late -
/// disconnected an otherwise healthy client. At three times the cadence a peer
/// gets three chances to answer before it is dropped.
pub(super) const WS_PONG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How long one outbound frame may take to send+flush before the peer is
/// declared dead. A peer that stopped reading without closing (a backgrounded
/// tab, a sleeping laptop, a NAT that dropped the mapping) leaves `send()`
/// pending forever otherwise - the task then never polls `recv()` either, so
/// it holds its broadcast receiver and ~256 KB of frame buffers until TCP
/// keepalive fires hours later.
pub(super) const WS_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Cap tungstenite's outbound frame buffering per connection. Without it a
/// wedged peer's connection buffers frames without bound while waiting for
/// the send timeout to notice.
pub(super) const WS_MAX_WRITE_BUFFER: usize = 256 * 1024;

/// The ping cadence, pong deadline and send deadline of one connection.
#[derive(Clone, Copy)]
pub(super) struct WsTiming {
    /// How often a ping goes out.
    pub(super) ping_every: std::time::Duration,
    /// How long the oldest unanswered ping may wait for its pong.
    pub(super) pong_timeout: std::time::Duration,
    /// How long one frame may take to go out.
    pub(super) send_timeout: std::time::Duration,
}

/// What every real connection runs with.
pub(super) const WS_TIMING: WsTiming = WsTiming {
    ping_every: WS_PING_INTERVAL,
    pong_timeout: WS_PONG_TIMEOUT,
    send_timeout: WS_SEND_TIMEOUT,
};

/// The outbound half of a server-side socket.
pub(super) type WsSink = SplitSink<WebSocket, Message>;

/// Send one frame, bounded by `timeout`. `false` means the peer is dead or
/// wedged (the send errored, or its flush never completed in time).
pub(super) async fn send_within(
    sink: &mut WsSink,
    timeout: std::time::Duration,
    msg: Message,
) -> bool {
    matches!(
        tokio::time::timeout(timeout, sink.send(msg)).await,
        Ok(Ok(()))
    )
}

/// The ping side of one connection: when the next ping is due, and whether
/// the last ones were answered.
pub(super) struct Heartbeat {
    /// The cadence and the deadlines.
    timing: WsTiming,
    /// When the next ping goes out.
    ping: Interval,
    /// When the oldest still-unanswered ping went out, or `None` when every
    /// ping so far has been answered. A pong clears it; the deadline is
    /// measured from it, so answering late still costs nothing as long as it
    /// arrives inside the pong deadline.
    unanswered_since: Option<Instant>,
}

impl Heartbeat {
    /// The first ping goes out only after a full interval: a fresh connection
    /// is known live, and pinging in the handshake's shadow confuses simple
    /// clients.
    pub(super) fn new(timing: WsTiming) -> Self {
        let mut ping =
            tokio::time::interval_at(Instant::now() + timing.ping_every, timing.ping_every);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Self {
            timing,
            ping,
            unanswered_since: None,
        }
    }

    /// Wait until the next ping is due.
    pub(super) async fn due(&mut self) {
        self.ping.tick().await;
    }

    /// The peer answered.
    pub(super) fn answered(&mut self) {
        self.unanswered_since = None;
    }

    /// Ping the peer, or say it is gone.
    ///
    /// A ping left unanswered past the deadline, and a ping that cannot be
    /// sent, both mean the peer is gone without a Close. Anything short of
    /// the deadline just gets another ping: a peer that missed one and
    /// answers the next is a live peer.
    pub(super) async fn ping(&mut self, sink: &mut WsSink) -> bool {
        let overdue = self
            .unanswered_since
            .is_some_and(|since| since.elapsed() >= self.timing.pong_timeout);
        if overdue
            || !send_within(
                sink,
                self.timing.send_timeout,
                Message::Ping(Vec::new().into()),
            )
            .await
        {
            return false;
        }
        self.unanswered_since.get_or_insert_with(Instant::now);
        true
    }
}

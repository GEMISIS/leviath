//! The daemon's event stream, and what a subscriber that lost it is sent
//! again.
//!
//! The world broadcasts each [`WorldEvent`] once, to whoever is subscribed at
//! that moment. A gateway whose stream drops is away for as long as its
//! reconnect takes, and when the drop was the daemon dying, the new daemon
//! restores the runs that were going and drives them at once: a run whose
//! model answers straight away finishes inside that gap, its completion goes
//! to nobody, and its webhook never fires.
//!
//! So every event the world sends is numbered and kept, the last [`KEPT`] of
//! them, by an [`EventLog`] that starts recording before the world first
//! runs. A subscription opens with [`ControlResponse::Subscribed`], naming
//! this daemon's session and the number it starts after. A subscriber that
//! comes back asks to pick up from there ([`ControlRequest::Resubscribe`]):
//! the same daemon sends what came after that number, and a different daemon,
//! one that started since, sends everything it has kept, since none of it was
//! seen. A first subscription is sent only what happens from then on, so a
//! gateway that starts against a running daemon is not told again about runs
//! that finished before it was there.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, watch};

use super::{ControlResponse, write_line};
use crate::host::WorldEvent;

/// How many events a daemon keeps for subscribers that come back. A gateway
/// reconnects within a second or so, and the busiest daemon sends a few
/// hundred events in that time.
pub const KEPT: usize = 4096;

/// Where a subscription had got to: the daemon session it was reading, and
/// the number of the last event it was sent there. Handed back to
/// [`ControlClient::subscribe_from`](super::ControlClient::subscribe_from) to
/// pick the stream up again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventCursor {
    /// The daemon session the numbers belong to.
    pub session: String,
    /// The last event number seen.
    pub after: u64,
}

/// Every event the world sends, numbered, the last [`KEPT`] of them kept.
/// Cloning shares it.
#[derive(Clone)]
pub struct EventLog(Arc<Shared>);

struct Shared {
    /// Names this daemon's numbering. Random per log, so numbers from a
    /// daemon that has since been replaced are never read as this one's.
    session: String,
    kept: Mutex<Kept>,
    /// Bumped on every event and when the world closes, to wake subscribers.
    moved: watch::Sender<u64>,
}

#[derive(Default)]
struct Kept {
    events: VecDeque<(u64, WorldEvent)>,
    /// The number of the last event recorded; 0 before the first.
    last: u64,
    /// The world has stopped sending: no event will follow the kept ones.
    closed: bool,
}

impl Default for EventLog {
    fn default() -> Self {
        Self::new()
    }
}

impl EventLog {
    /// An empty log under a fresh session. Nothing is recorded until
    /// [`record`](Self::record) is given the world's sender; a subscriber
    /// that arrives first waits for the events like any other.
    pub fn new() -> Self {
        use rand::RngExt as _;
        let bytes: [u8; 16] = rand::rng().random();
        Self(Arc::new(Shared {
            session: bytes.iter().map(|b| format!("{b:02x}")).collect(),
            kept: Mutex::default(),
            moved: watch::channel(0).0,
        }))
    }

    /// A log recording `events` from now on.
    pub fn recording(events: &broadcast::Sender<WorldEvent>) -> Self {
        let log = Self::new();
        log.record(events);
        log
    }

    /// Record everything `events` sends from now on, until the world drops
    /// its sender. Subscribes before returning, so an event sent after this
    /// call is recorded; the copying happens on a task of its own. Events
    /// that overran the world's channel before the task read them are lost,
    /// as they would be to any other subscriber, but each still takes its
    /// number: the next event's number jumps, and a subscriber reading the
    /// numbers sees the gap instead of a stream that looks whole.
    pub fn record(&self, events: &broadcast::Sender<WorldEvent>) {
        let mut rx = events.subscribe();
        let log = self.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => log.push(event),
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "event recorder fell behind the world");
                        log.skip(missed);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            log.close();
        });
    }

    /// This log's session.
    pub fn session(&self) -> &str {
        &self.0.session
    }

    /// How many streams are open on this log.
    pub fn subscribers(&self) -> usize {
        self.0.moved.receiver_count()
    }

    fn push(&self, event: WorldEvent) {
        let last = {
            let mut kept = leviath_core::sync::lock(&self.0.kept);
            kept.last += 1;
            let seq = kept.last;
            kept.events.push_back((seq, event));
            let excess = kept.events.len().saturating_sub(KEPT);
            kept.events.drain(..excess);
            seq
        };
        self.0.moved.send_replace(last);
    }

    /// Spend `missed` numbers on events that were never recorded.
    fn skip(&self, missed: u64) {
        let mut kept = leviath_core::sync::lock(&self.0.kept);
        kept.last = kept.last.saturating_add(missed);
    }

    fn close(&self) {
        let last = {
            let mut kept = leviath_core::sync::lock(&self.0.kept);
            kept.closed = true;
            kept.last
        };
        self.0.moved.send_replace(last);
    }

    /// The number a subscription starts after: the newest event for a first
    /// subscription, which is sent only what follows; where it left off for
    /// one coming back to this session; and the start for one coming back
    /// from another session, which saw none of these.
    fn start(&self, from: Option<&EventCursor>) -> u64 {
        let last = leviath_core::sync::lock(&self.0.kept).last;
        match from {
            None => last,
            Some(cursor) if cursor.session == self.0.session => cursor.after.min(last),
            Some(_) => 0,
        }
    }

    /// The kept events numbered after `after`, and whether the world has
    /// closed.
    fn since(&self, after: u64) -> (Vec<(u64, WorldEvent)>, bool) {
        let kept = leviath_core::sync::lock(&self.0.kept);
        // Numbers rise but are not consecutive (a lagging recorder skips
        // some), so the first one wanted is found by search, and a live
        // subscriber still reads one event without walking the rest.
        let first = kept.events.partition_point(|(seq, _)| *seq <= after);
        let events = kept.events.iter().skip(first).cloned().collect();
        (events, kept.closed)
    }
}

/// One event as a stream line: the event's own fields, with its number
/// beside them as `seq`. A client that does not know the field ignores it.
fn numbered_line(seq: u64, event: &WorldEvent) -> String {
    let mut value = serde_json::to_value(event).expect("WorldEvent serializes");
    value["seq"] = seq.into();
    let mut line = value.to_string();
    line.push('\n');
    line
}

/// Stream events from `log` to a subscribed client until it disconnects or
/// the world closes, starting after where `from` says it left off (see
/// [`EventLog::start`]). The first line says where it starts.
///
/// The read half is watched alongside the writes: a subscriber that hangs up
/// is otherwise only noticed when the *next* event's write fails, and an idle
/// daemon may not produce one for hours - each such half-dead connection
/// would park a task here for the daemon's life (serve's polling loop
/// re-subscribes every 500ms after a drop, and the ACP client subscribes once
/// per prompt turn, so these accumulate fast).
pub(super) async fn stream_events<R, W>(
    read: &mut tokio::io::Lines<BufReader<R>>,
    write: &mut W,
    log: EventLog,
    from: Option<EventCursor>,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // Watched before the first read of the log, so an event recorded in
    // between wakes the loop rather than waiting for the next one.
    let mut moved = log.0.moved.subscribe();
    let mut after = log.start(from.as_ref());
    let opened = ControlResponse::Subscribed {
        session: log.session().to_string(),
        after,
    };
    write_line(write, &opened).await;
    loop {
        let (events, closed) = log.since(after);
        for (seq, event) in events {
            if write
                .write_all(numbered_line(seq, &event).as_bytes())
                .await
                .is_err()
            {
                return Ok(()); // client hung up
            }
            after = seq;
        }
        if closed {
            return Ok(());
        }
        tokio::select! {
            // The log holds the sender, and this task holds the log, so the
            // wait only ever ends in a change.
            _ = moved.changed() => {}
            line = read.next_line() => match line {
                // A subscriber has nothing left to say; any line it does send
                // is ignored chatter, not a request.
                Ok(Some(_)) => {}
                // EOF or a read error: the client is gone.
                Ok(None) | Err(_) => return Ok(()),
            },
        }
    }
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;

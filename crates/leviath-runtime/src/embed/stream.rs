//! The in-process event stream an embedder consumes.

use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::host::WorldEvent;

/// An async stream of [`WorldEvent`]s from an embedded world.
///
/// Wraps the host's broadcast channel: a slow consumer that falls more than
/// the channel capacity behind skips the missed events (with a warning)
/// rather than erroring, and the stream ends (`None`) when the world shuts
/// down. Implements [`futures_core::Stream`], and offers an inherent
/// [`next`](Self::next) so the common loop needs no extra imports:
///
/// ```ignore
/// while let Some(event) = events.next().await {
///     // ...
/// }
/// ```
pub struct EventStream {
    inner: BroadcastStream<WorldEvent>,
}

impl EventStream {
    pub(crate) fn new(rx: tokio::sync::broadcast::Receiver<WorldEvent>) -> Self {
        Self {
            inner: BroadcastStream::new(rx),
        }
    }

    /// The next event, or `None` once the world has shut down.
    pub async fn next(&mut self) -> Option<WorldEvent> {
        use futures_core::Stream;
        std::future::poll_fn(|cx| std::pin::Pin::new(&mut *self).poll_next(cx)).await
    }
}

impl futures_core::Stream for EventStream {
    type Item = WorldEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        loop {
            match std::pin::Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(event))) => return Poll::Ready(Some(event)),
                Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(n)))) => {
                    tracing::warn!("event stream lagged, skipped {n} events");
                    continue; // resubscribed at the live edge; keep polling
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(line: &str) -> WorldEvent {
        WorldEvent::Log {
            run_id: "r".to_string(),
            agent_id: "a".to_string(),
            line: line.to_string(),
        }
    }

    #[tokio::test]
    async fn yields_events_then_ends_when_the_sender_drops() {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let mut stream = EventStream::new(rx);
        tx.send(log("one")).unwrap();
        tx.send(log("two")).unwrap();
        assert_eq!(stream.next().await, Some(log("one")));
        assert_eq!(stream.next().await, Some(log("two")));
        drop(tx);
        assert_eq!(stream.next().await, None);
    }

    #[tokio::test]
    async fn skips_over_a_lag_instead_of_erroring() {
        // Capacity 1: sending twice before reading overwrites the first event,
        // which surfaces as a Lagged error the stream must swallow.
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        let mut stream = EventStream::new(rx);
        tx.send(log("dropped")).unwrap();
        tx.send(log("kept")).unwrap();
        assert_eq!(stream.next().await, Some(log("kept")));
        drop(tx);
        assert_eq!(stream.next().await, None);
    }
}

//! Driving a future to its end from inside the world.
//!
//! Resolving and binding a run are async, because a host may need to wait on
//! the network to answer them, and the daemon hands its slow parts (reading
//! the config, starting a sandbox) to tokio's blocking pool. A fan-out worker
//! is started, and an unloaded run paged back in, from inside a tick, where
//! nothing can be awaited; [`block_on`] runs the future to its end there
//! instead, parking the thread while the blocking pool works.

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

/// Wakes the thread that is waiting on a future.
struct Unpark(std::thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Run `future` to its end on this thread, parking whenever it has to wait.
///
/// The future runs outside tokio's cooperative budget. Called from inside a
/// runtime task, as daemon start calls it, the caller's budget would otherwise
/// cover every await in here: binding every unfinished run at start waits on
/// the blocking pool once per run, and past 128 of those tokio answers
/// `Pending` with a wake that only its own scheduler delivers, so this thread
/// would park for ever.
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(tokio::task::unconstrained(future));
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A future that has to wait once, and wakes itself.
    struct WaitsOnce(bool);

    impl Future for WaitsOnce {
        type Output = u8;
        fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u8> {
            match self.0 {
                true => Poll::Ready(7),
                false => {
                    self.0 = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        }
    }

    #[test]
    fn a_future_that_waits_is_driven_to_its_end() {
        assert_eq!(block_on(WaitsOnce(false)), 7);
        assert_eq!(block_on(async { 3 }), 3);
    }

    /// Called inside a runtime task, as daemon start calls it, a future that
    /// waits on more blocking-pool work than tokio's per-poll budget allows
    /// (128) still finishes: binding every unfinished run at start is one
    /// such future, and each bind hops to the blocking pool.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn more_blocking_hops_than_the_poll_budget_still_finish() {
        let hops = block_on(async {
            let mut done = 0;
            for _ in 0..300 {
                done += tokio::task::spawn_blocking(|| 1).await.unwrap();
            }
            done
        });
        assert_eq!(hops, 300);
    }
}

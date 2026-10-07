//! Driving a future to its end from inside the world.
//!
//! Resolving and binding a run are async, because a host may need to wait on
//! the network to answer them. The daemon's answers are local, so the futures
//! finish without ever having to wait. A fan-out worker is started, and an
//! unloaded run paged back in, from inside a tick, where nothing can be
//! awaited; [`block_on`] runs the future to its end there instead.

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
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
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
}

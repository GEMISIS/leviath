//! Filesystem work off the async runtime.
//!
//! Every handler here is `async`, and tokio runs them on a small pool of
//! worker threads. A `read_dir` over a thousand runs, or a TOML parse of every
//! installed blueprint, holds one of those threads for its whole duration, and
//! a burst of requests that all do it holds all of them: the requests then
//! wait on each other though nothing they touch is shared. The blocking pool
//! exists for exactly this work.

/// Run `f` on tokio's blocking pool and hand back what it returns.
///
/// A panic inside `f` is re-raised here rather than turned into an error:
/// nothing cancels a blocking task, so a `JoinError` can only be a panic, and
/// swallowing one would turn a bug into an empty listing.
pub(super) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => value,
        Err(joined) => std::panic::resume_unwind(joined.into_panic()),
    }
}

#[cfg(test)]
mod tests {
    use super::blocking;

    #[tokio::test]
    async fn hands_back_what_the_closure_returns() {
        assert_eq!(blocking(|| 40 + 2).await, 42);
    }

    #[tokio::test]
    #[should_panic(expected = "from the blocking pool")]
    async fn a_panic_inside_the_closure_surfaces_in_the_caller() {
        blocking(|| panic!("from the blocking pool")).await;
    }
}

//! A tool batch that panics still reports.
//!
//! The tool lane runs each batch on a task of its own and reports what the
//! batch returns. A batch whose executor panics returns nothing, so its agent
//! would wait in `AwaitingTools` for results that never come. [`guarded`]
//! wraps a batch so a panic, while the batch is being built or while it runs,
//! becomes an `[error]` result for each of its calls: the batch reports like
//! any other, and the model reads that its calls failed.

use std::any::Any;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::tool_bridge::{BoxedToolExec, ToolExecFuture, ToolResult};

/// `exec`, with a panic in it reported as an `[error]` result for each of
/// `call_ids`.
pub(crate) fn guarded(exec: BoxedToolExec, call_ids: Vec<String>) -> BoxedToolExec {
    Box::new(
        move || match std::panic::catch_unwind(AssertUnwindSafe(exec)) {
            Ok(running) => Box::pin(Guarded { running, call_ids }),
            Err(payload) => {
                let results = lost(&call_ids, payload.as_ref());
                Box::pin(std::future::ready(results))
            }
        },
    )
}

/// A running batch, watched for a panic.
struct Guarded {
    running: ToolExecFuture,
    call_ids: Vec<String>,
}

impl Future for Guarded {
    type Output = Vec<ToolResult>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        match std::panic::catch_unwind(AssertUnwindSafe(|| this.running.as_mut().poll(cx))) {
            Ok(polled) => polled,
            Err(payload) => Poll::Ready(lost(&this.call_ids, payload.as_ref())),
        }
    }
}

/// The result each call of a batch that panicked gets.
fn lost(call_ids: &[String], payload: &(dyn Any + Send)) -> Vec<ToolResult> {
    let why = leviath_core::panic_message(payload);
    tracing::error!(%why, "a tool batch panicked; each of its calls is reported failed");
    call_ids
        .iter()
        .map(|id| {
            (
                id.clone(),
                format!("[error] the tool call did not finish: its task panicked ({why})").into(),
            )
        })
        .collect()
}

#[cfg(test)]
#[path = "tool_guard_tests.rs"]
mod tests;

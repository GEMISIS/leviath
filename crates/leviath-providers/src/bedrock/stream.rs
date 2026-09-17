//! Bedrock's streamed answer, event by event.
//!
//! `ConverseStream` sends a message as content blocks, each opened, filled
//! by deltas and closed, then a stop reason and the usage. The frames are
//! binary (see `super::eventstream`), so this is its own `Stream` over the
//! bytes rather than a framer plugged into the shared text one; what it
//! yields is the same [`StreamChunk`] the shared collector folds back into
//! one response.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;
use leviath_net::read_caps::{STREAM_FRAME_CAP, frame_within_cap};
use serde_json::{Value, json};

use super::convert::{parse_stop_reason, parse_usage, reasoning_blob};
use super::eventstream::{self, Frame};
use crate::failure::FailureKind;
use crate::provider::stream::ByteStream;
use crate::provider::{ProviderError, Result, StreamChunk, ToolCallDelta};

/// The bytes of a `converse-stream` response, read as chunks.
pub(super) struct ConverseStream {
    inner: ByteStream,
    buffer: Vec<u8>,
    /// The most a frame may declare before the stream fails;
    /// [`STREAM_FRAME_CAP`] in production, smaller in the test that hits it.
    frame_cap: usize,
    /// Who the bytes are from, for the cap error.
    peer: String,
    state: EventState,
    /// Set once an error has been yielded, so the stream ends after it.
    finished: bool,
}

/// What the mapper carries between events.
#[derive(Default)]
pub(super) struct EventState {
    /// The tool block being streamed, so an argument delta that names no
    /// index lands on it.
    open_tool_block: Option<usize>,
    /// Reasoning blocks being assembled, by content block index. Emitted
    /// once, whole, when the message stops.
    reasoning: BTreeMap<usize, ReasoningBlock>,
}

/// One reasoning block as its deltas arrive.
#[derive(Default)]
struct ReasoningBlock {
    text: String,
    signature: Option<String>,
    redacted: Option<Value>,
}

impl ReasoningBlock {
    /// The block as Bedrock would have sent it buffered, which is the shape
    /// a later request replays.
    fn into_block(self) -> Value {
        match self.redacted {
            Some(redacted) => json!({ "reasoningContent": { "redactedContent": redacted } }),
            None => {
                let mut text = json!({ "text": self.text });
                if let Some(signature) = self.signature {
                    text["signature"] = json!(signature);
                }
                json!({ "reasoningContent": { "reasoningText": text } })
            }
        }
    }
}

impl ConverseStream {
    /// Wrap the response bytes.
    pub(super) fn new<S>(inner: S) -> Self
    where
        S: Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    {
        Self {
            inner: Box::pin(inner),
            buffer: Vec::new(),
            frame_cap: STREAM_FRAME_CAP,
            peer: "the provider".to_string(),
            state: EventState::default(),
            finished: false,
        }
    }

    /// Name the peer the cap error reports.
    pub(super) fn sent_by(mut self, peer: String) -> Self {
        self.peer = peer;
        self
    }

    /// Lower the frame cap, so a test can overrun it with a few bytes.
    #[cfg(test)]
    pub(super) fn with_frame_cap(mut self, cap: usize) -> Self {
        self.frame_cap = cap;
        self
    }

    /// Fail the stream: the buffer is dropped and nothing follows.
    fn fail(&mut self, error: ProviderError) -> Poll<Option<Result<StreamChunk>>> {
        self.buffer.clear();
        self.finished = true;
        Poll::Ready(Some(Err(error)))
    }
}

impl Stream for ConverseStream {
    type Item = Result<StreamChunk>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        loop {
            match eventstream::decode(&mut this.buffer, this.frame_cap) {
                Err(e) => {
                    let peer = this.peer.clone();
                    return this.fail(ProviderError::InvalidResponse(format!(
                        "malformed event stream from {peer}: {e}"
                    )));
                }
                Ok(Some(frame)) => match map_frame(&frame, &mut this.state) {
                    Some(Ok(chunk)) => return Poll::Ready(Some(Ok(chunk))),
                    Some(Err(e)) => return this.fail(e),
                    None => continue,
                },
                Ok(None) => {}
            }
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    this.buffer.extend_from_slice(&bytes);
                    if let Err(msg) =
                        frame_within_cap(this.buffer.len(), this.frame_cap, &this.peer)
                    {
                        return this.fail(ProviderError::InvalidResponse(msg));
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    return this.fail(ProviderError::transport("reading the response stream", &e));
                }
                Poll::Ready(None) => {
                    // A frame that was still arriving is one that never will:
                    // the reply is cut, and saying so beats a silent end.
                    if !this.buffer.is_empty() {
                        return this.fail(ProviderError::labelled(
                            FailureKind::ConnectionDropped,
                            "reading the response stream",
                            "the event stream ended inside a frame",
                        ));
                    }
                    this.finished = true;
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// A chunk carrying nothing but what is set on it.
fn chunk() -> StreamChunk {
    StreamChunk {
        delta: String::new(),
        tool_calls: Vec::new(),
        tokens: None,
        finish_reason: None,
        reasoning: None,
        parts: Vec::new(),
    }
}

/// What a frame means: a chunk, an error the stream carried, or nothing.
pub(super) fn map_frame(frame: &Frame, state: &mut EventState) -> Option<Result<StreamChunk>> {
    match frame.message_type() {
        Some("event") => {
            let payload: Value = match serde_json::from_slice(&frame.payload) {
                Ok(v) => v,
                Err(e) => {
                    return Some(Err(ProviderError::InvalidResponse(format!(
                        "an event's payload is not JSON: {e}"
                    ))));
                }
            };
            map_event(frame.event_type().unwrap_or(""), &payload, state).map(Ok)
        }
        Some("exception") => {
            let message = serde_json::from_slice::<Value>(&frame.payload)
                .ok()
                .and_then(|v| {
                    v.get("message")
                        .and_then(|m| m.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "(no message)".to_string());
            Some(Err(exception_error(
                frame.exception_type().unwrap_or("unknownException"),
                &message,
            )))
        }
        // Bedrock reached, and reporting a failure of its own mid-stream.
        Some("error") => Some(Err(ProviderError::labelled(
            FailureKind::ServerError,
            "reading the response stream",
            &format!(
                "{}: {}",
                frame.header_str(":error-code").unwrap_or("error"),
                frame.header_str(":error-message").unwrap_or("(no message)")
            ),
        ))),
        _ => None,
    }
}

/// The content-block index an event names, when it names one.
fn block_index(json: &Value) -> Option<usize> {
    json.get("contentBlockIndex")
        .and_then(|v| v.as_u64())
        .map(|i| i as usize)
}

/// One event as a chunk, or `None` for one that carries nothing yet.
fn map_event(event: &str, json: &Value, state: &mut EventState) -> Option<StreamChunk> {
    match event {
        "contentBlockStart" => {
            let call = json.pointer("/start/toolUse")?;
            // The number the event carries, else the one after the last
            // tool block: a second call must not overwrite the first.
            let index = block_index(json)
                .unwrap_or_else(|| state.open_tool_block.map_or(0, |open| open + 1));
            state.open_tool_block = Some(index);
            Some(StreamChunk {
                tool_calls: vec![ToolCallDelta {
                    index,
                    id: Some(
                        call.get("toolUseId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ),
                    name: Some(
                        call.get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ),
                    arguments_delta: String::new(),
                    thought_signature: None,
                }],
                ..chunk()
            })
        }
        "contentBlockDelta" => {
            let delta = json.get("delta")?;
            if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                return Some(StreamChunk {
                    delta: text.to_string(),
                    ..chunk()
                });
            }
            if let Some(input) = delta.pointer("/toolUse/input").and_then(|t| t.as_str()) {
                return Some(StreamChunk {
                    tool_calls: vec![ToolCallDelta {
                        index: block_index(json).or(state.open_tool_block).unwrap_or(0),
                        id: None,
                        name: None,
                        arguments_delta: input.to_string(),
                        thought_signature: None,
                    }],
                    ..chunk()
                });
            }
            if let Some(reasoning) = delta.get("reasoningContent") {
                let block = state
                    .reasoning
                    .entry(block_index(json).unwrap_or(0))
                    .or_default();
                if let Some(text) = reasoning.get("text").and_then(|t| t.as_str()) {
                    block.text.push_str(text);
                }
                if let Some(signature) = reasoning.get("signature").and_then(|s| s.as_str()) {
                    block.signature = Some(signature.to_string());
                }
                if let Some(redacted) = reasoning.get("redactedContent") {
                    block.redacted = Some(redacted.clone());
                }
            }
            None
        }
        "messageStop" => {
            let blocks: Vec<Value> = std::mem::take(&mut state.reasoning)
                .into_values()
                .map(ReasoningBlock::into_block)
                .collect();
            Some(StreamChunk {
                finish_reason: Some(parse_stop_reason(
                    json.get("stopReason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("end_turn"),
                )),
                reasoning: reasoning_blob(&blocks),
                ..chunk()
            })
        }
        "metadata" => Some(StreamChunk {
            tokens: Some(parse_usage(json.get("usage"))),
            ..chunk()
        }),
        "messageStart" | "contentBlockStop" => None,
        other => {
            tracing::debug!(event = other, "unrecognised event from Bedrock's stream");
            None
        }
    }
}

/// The error an in-stream exception frame stands for.
///
/// Throttling is the capacity refusal the retry loop already knows. A
/// server-side failure carries the status AWS documents for it, so the
/// shared transient check reads it as one. Anything else is the request's
/// own fault and permanent.
pub(super) fn exception_error(kind: &str, message: &str) -> ProviderError {
    match kind {
        "throttlingException" => ProviderError::RateLimitExceeded {
            retry_after_secs: None,
        },
        "internalServerException" => server_error(500, kind, message),
        "serviceUnavailableException" => server_error(503, kind, message),
        "modelStreamErrorException" => server_error(424, kind, message),
        _ => ProviderError::ApiError(format!(
            "[{}] HTTP 400: {kind}: {message} - {}",
            FailureKind::BadRequest.label(),
            FailureKind::BadRequest.remedy()
        )),
    }
}

/// A server-side failure, labelled and carrying its documented status.
fn server_error(status: u16, kind: &str, message: &str) -> ProviderError {
    ProviderError::ApiError(format!(
        "[{}] HTTP {status}: {kind}: {message} - {}",
        FailureKind::ServerError.label(),
        FailureKind::ServerError.remedy()
    ))
}

#[cfg(test)]
mod tests {
    use super::super::eventstream::HeaderValue;
    use super::super::eventstream::fixtures::{encode, event, exception};
    use super::*;
    use crate::provider::{FinishReason, collect_stream};
    use tokio_stream::StreamExt;

    /// A stream over `frames`, delivered as one byte chunk each.
    fn stream(frames: Vec<Vec<u8>>) -> ConverseStream {
        ConverseStream::new(tokio_stream::iter(
            frames.into_iter().map(|f| Ok(bytes::Bytes::from(f))),
        ))
    }

    async fn collect(s: ConverseStream) -> Result<crate::provider::InferenceResponse> {
        collect_stream(Box::pin(s)).await
    }

    #[tokio::test]
    async fn a_text_reply_folds_into_one_response() {
        let frames = vec![
            event("messageStart", r#"{"role":"assistant"}"#),
            event(
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"text":"Hel"}}"#,
            ),
            event(
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"text":"lo"}}"#,
            ),
            event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
            event("messageStop", r#"{"stopReason":"end_turn"}"#),
            event(
                "metadata",
                r#"{"usage":{"inputTokens":10,"outputTokens":5,"totalTokens":18,"cacheReadInputTokens":2,"cacheWriteInputTokens":1}}"#,
            ),
        ];
        let response = collect(stream(frames)).await.unwrap();
        assert_eq!(response.content, "Hello");
        assert_eq!(response.finish_reason, FinishReason::Complete);
        assert_eq!(response.tokens_used.prompt_tokens, 10);
        assert_eq!(response.tokens_used.cached_tokens, 2);
        assert_eq!(response.tokens_used.total_tokens, 18);
        assert_eq!(response.reasoning, None);
    }

    #[tokio::test]
    async fn a_tool_call_is_assembled_from_its_deltas() {
        let frames = vec![
            event(
                "contentBlockStart",
                r#"{"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"t1","name":"read"}}}"#,
            ),
            event(
                "contentBlockDelta",
                r#"{"contentBlockIndex":1,"delta":{"toolUse":{"input":"{\"path\":"}}}"#,
            ),
            // No index: lands on the open block.
            event(
                "contentBlockDelta",
                r#"{"delta":{"toolUse":{"input":"\"a\"}"}}}"#,
            ),
            // A second call with no index takes the next number.
            event("contentBlockStart", r#"{"start":{"toolUse":{}}}"#),
            event("messageStop", r#"{"stopReason":"tool_use"}"#),
        ];
        let response = collect(stream(frames)).await.unwrap();
        assert_eq!(response.finish_reason, FinishReason::ToolCall);
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].id, "t1");
        assert_eq!(response.tool_calls[0].name, "read");
        assert_eq!(response.tool_calls[0].arguments, json!({ "path": "a" }));
        assert_eq!(response.tool_calls[1].id, "");
    }

    #[tokio::test]
    async fn reasoning_deltas_are_gathered_into_one_blob_at_the_stop() {
        let frames = vec![
            event(
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"thi"}}}"#,
            ),
            event(
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"nk"}}}"#,
            ),
            event(
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":"sig"}}}"#,
            ),
            event(
                "contentBlockDelta",
                r#"{"contentBlockIndex":1,"delta":{"reasoningContent":{"redactedContent":"AAAA"}}}"#,
            ),
            event(
                "contentBlockDelta",
                r#"{"contentBlockIndex":2,"delta":{"text":"answer"}}"#,
            ),
            event("messageStop", r#"{"stopReason":"end_turn"}"#),
        ];
        let response = collect(stream(frames)).await.unwrap();
        assert_eq!(response.content, "answer");
        let blob: Value = serde_json::from_str(&response.reasoning.unwrap()).unwrap();
        assert_eq!(
            blob["bedrock"],
            json!([
                { "reasoningContent": { "reasoningText": { "text": "think", "signature": "sig" } } },
                { "reasoningContent": { "redactedContent": "AAAA" } }
            ])
        );
    }

    #[tokio::test]
    async fn a_reasoning_block_without_a_signature_carries_none() {
        let frames = vec![
            event(
                "contentBlockDelta",
                r#"{"delta":{"reasoningContent":{"text":"x"}}}"#,
            ),
            event("messageStop", r#"{"stopReason":"end_turn"}"#),
        ];
        let response = collect(stream(frames)).await.unwrap();
        let blob: Value = serde_json::from_str(&response.reasoning.unwrap()).unwrap();
        assert_eq!(
            blob["bedrock"][0]["reasoningContent"]["reasoningText"],
            json!({ "text": "x" })
        );
    }

    #[tokio::test]
    async fn unknown_events_and_message_types_carry_nothing() {
        let _guard = crate::test_support::always_on_tracing_guard();
        let frames = vec![
            event("somethingNew", r#"{}"#),
            event("contentBlockDelta", r#"{"delta":{"citation":{}}}"#),
            event("contentBlockDelta", r#"{}"#),
            event("contentBlockStart", r#"{"start":{}}"#),
            encode(
                &[(":message-type", HeaderValue::String("ping".to_string()))],
                b"",
            ),
            encode(&[], b""),
            event("messageStop", r#"{}"#),
        ];
        let response = collect(stream(frames)).await.unwrap();
        assert_eq!(response.content, "");
        assert_eq!(response.finish_reason, FinishReason::Complete);
    }

    #[tokio::test]
    async fn exception_frames_become_the_matching_errors() {
        let throttled = stream(vec![exception(
            "throttlingException",
            r#"{"message":"slow down"}"#,
        )]);
        let err = collect(throttled).await.unwrap_err();
        assert!(err.retry_advice().capacity);
        assert!(err.is_transient());

        let invalid = stream(vec![exception(
            "validationException",
            r#"{"message":"bad field"}"#,
        )]);
        let err = collect(invalid).await.unwrap_err();
        assert!(err.to_string().contains("bad field"), "{err}");
        assert_eq!(err.failure_kind(), Some(FailureKind::BadRequest));
        assert!(!err.is_transient());

        let internal = stream(vec![exception("internalServerException", "not json")]);
        let err = collect(internal).await.unwrap_err();
        assert!(err.to_string().contains("(no message)"), "{err}");
        assert_eq!(err.failure_kind(), Some(FailureKind::ServerError));
        assert!(err.is_transient());

        let unavailable = exception_error("serviceUnavailableException", "x");
        assert!(unavailable.is_transient());
        let model_stream = exception_error("modelStreamErrorException", "x");
        assert!(model_stream.to_string().contains("424"));

        let untyped = stream(vec![encode(
            &[(
                ":message-type",
                HeaderValue::String("exception".to_string()),
            )],
            b"{}",
        )]);
        let err = collect(untyped).await.unwrap_err();
        assert!(err.to_string().contains("unknownException"), "{err}");
    }

    #[tokio::test]
    async fn an_error_frame_reads_as_the_servers_own_failure() {
        let frames = vec![encode(
            &[
                (":message-type", HeaderValue::String("error".to_string())),
                (
                    ":error-code",
                    HeaderValue::String("InternalError".to_string()),
                ),
                (":error-message", HeaderValue::String("boom".to_string())),
            ],
            b"",
        )];
        let err = collect(stream(frames)).await.unwrap_err();
        assert!(err.to_string().contains("InternalError: boom"), "{err}");
        assert_eq!(err.failure_kind(), Some(FailureKind::ServerError));
        let bare = stream(vec![encode(
            &[(":message-type", HeaderValue::String("error".to_string()))],
            b"",
        )]);
        let err = collect(bare).await.unwrap_err();
        assert!(err.to_string().contains("error: (no message)"), "{err}");
    }

    #[tokio::test]
    async fn a_payload_that_is_not_json_fails_the_stream() {
        let err = collect(stream(vec![event("messageStart", "{not json")]))
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("Invalid response"), "{err}");
        assert!(!err.is_transient());
    }

    #[tokio::test]
    async fn frames_split_across_chunks_arrive_whole() {
        let mut bytes = event("contentBlockDelta", r#"{"delta":{"text":"whole"}}"#);
        bytes.extend(event("messageStop", r#"{"stopReason":"end_turn"}"#));
        let chunks: Vec<Vec<u8>> = bytes.chunks(7).map(<[u8]>::to_vec).collect();
        let response = collect(stream(chunks)).await.unwrap();
        assert_eq!(response.content, "whole");
    }

    #[tokio::test]
    async fn a_corrupt_frame_ends_the_stream_with_one_error() {
        let mut bad = event("contentBlockDelta", r#"{"delta":{"text":"x"}}"#);
        bad[9] ^= 0xff;
        let mut s = stream(vec![bad]).sent_by("bedrock.test".to_string());
        let first = s.next().await.unwrap().unwrap_err();
        assert!(first.to_string().contains("bedrock.test"), "{first}");
        assert!(first.to_string().contains("prelude"), "{first}");
        assert!(s.next().await.is_none());
    }

    #[tokio::test]
    async fn a_stream_that_ends_inside_a_frame_is_a_dropped_connection() {
        let whole = event("contentBlockDelta", r#"{"delta":{"text":"x"}}"#);
        let torn = whole[..whole.len() - 3].to_vec();
        let err = collect(stream(vec![torn])).await.unwrap_err();
        assert_eq!(err.failure_kind(), Some(FailureKind::ConnectionDropped));
        assert!(err.to_string().contains("inside a frame"), "{err}");
    }

    #[tokio::test]
    async fn an_empty_stream_ends_cleanly_without_a_finish() {
        let err = collect(stream(vec![])).await.unwrap_err();
        assert!(err.to_string().contains("before the model said"), "{err}");
    }

    #[tokio::test]
    async fn a_frame_past_the_cap_fails_by_its_declared_length_or_its_bytes() {
        // A prelude declaring more than the cap, before the rest arrives.
        let huge = event("messageStart", &"x".repeat(200));
        let prelude = huge[..12].to_vec();
        let mut s = stream(vec![prelude])
            .with_frame_cap(64)
            .sent_by("peer".to_string());
        let err = s.next().await.unwrap().unwrap_err();
        assert!(err.to_string().contains("do not describe"), "{err}");
        // Bytes past the cap before any prelude can be read.
        let mut s = stream(vec![vec![0u8; 8], vec![0u8; 100]]).with_frame_cap(64);
        let err = s.next().await.unwrap().unwrap_err();
        assert!(err.to_string().contains("exceeded"), "{err}");
    }

    #[tokio::test]
    async fn a_transport_failure_is_reported_as_one() {
        let failing = tokio_stream::iter(vec![Err(crate::provider::malformed_url_error())]);
        let err = collect(ConverseStream::new(failing)).await.unwrap_err();
        assert_eq!(
            err.unavailable_reason(),
            Some(crate::provider::UnavailableReason::Unreachable),
            "{err}"
        );
    }
}

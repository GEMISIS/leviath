//! Putting a streamed answer back together.
//!
//! The runtime asks for a whole turn: an agent cannot act on half a sentence,
//! so nothing downstream ever sees a partial answer. Streaming is here for the
//! socket, not for the agent - a buffered call sends nothing back until the
//! model has finished thinking, and a connection that has been silent for
//! minutes is one that a NAT, a VPN or a corporate proxy will close as dead.
//!
//! So this module is the exact inverse of the default
//! [`Provider::infer_stream`](super::Provider::infer_stream), which takes a
//! finished response apart into one chunk.

use super::*;

/// Fold a chunk stream back into the single response the runtime works with.
///
/// The exact inverse of [`Provider::infer_stream`]'s default, which takes a
/// response apart into one chunk. The runtime wants a whole turn - it has no
/// use for a half-written sentence - so streaming is about *how the bytes
/// arrive*, not about handing anything half-finished to an agent: a streamed
/// call keeps a socket that is visibly working, where a buffered one goes
/// silent for as long as the model takes to think and gets cut by anything on
/// the path that reaps idle connections.
///
/// The three inputs are disjoint (see [`TokenUsage::prompt_tokens`]), so usage
/// chunks are summed field by field rather than replacing one another:
/// Anthropic reports its input counts on `message_start` and its output count
/// on `message_delta`, so taking the last chunk would report a whole turn's
/// prompt as zero.
///
/// A stream that ends without a finish reason is an answer that stopped early,
/// which is a transport failure and not a complete turn. Calling it
/// [`FinishReason::Complete`] would hand the agent a truncated answer as if the
/// model had meant to stop there.
pub async fn collect_stream(
    stream: Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>,
) -> Result<InferenceResponse> {
    use tokio_stream::StreamExt;

    let mut stream = stream;
    let mut content = String::new();
    // Keyed and ordered by the index the provider assigned, because the deltas
    // for two calls interleave and a `Vec` in arrival order would not be the
    // order the model asked for them in.
    let mut calls: std::collections::BTreeMap<usize, PartialToolCall> =
        std::collections::BTreeMap::new();
    let mut tokens = TokenUsage::new(0, 0, 0, 0);
    let mut finish_reason = None;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        content.push_str(&chunk.delta);
        for delta in chunk.tool_calls {
            let call = calls.entry(delta.index).or_default();
            // The opening delta carries the id, the name and the signature; the
            // ones after it carry argument text and nothing else. Assigning
            // only what arrived keeps a later empty delta from erasing them.
            if let Some(id) = delta.id {
                call.id = id;
            }
            if let Some(name) = delta.name {
                call.name = name;
            }
            if let Some(signature) = delta.thought_signature {
                call.thought_signature = Some(signature);
            }
            call.arguments.push_str(&delta.arguments_delta);
        }
        if let Some(usage) = chunk.tokens {
            tokens = merge_usage(tokens, usage);
        }
        if let Some(reason) = chunk.finish_reason {
            finish_reason = Some(reason);
        }
    }

    let Some(finish_reason) = finish_reason else {
        return Err(ProviderError::labelled(
            FailureKind::ConnectionDropped,
            "reading the response stream",
            "the stream ended before the model said it had finished",
        ));
    };

    Ok(InferenceResponse {
        content,
        tool_calls: calls.into_values().map(PartialToolCall::finish).collect(),
        tokens_used: tokens,
        finish_reason,
    })
}

/// One tool call being assembled from its deltas by [`collect_stream`].
#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    /// Argument JSON as text, because it arrives in fragments that are each
    /// invalid on their own. Parsed once, at the end.
    arguments: String,
    thought_signature: Option<String>,
}

impl PartialToolCall {
    fn finish(self) -> ToolCall {
        ToolCall {
            id: self.id,
            name: self.name,
            // Same rule as the buffered path: empty text is `{}`, text that is
            // not JSON is kept as text so a call cut off mid-argument is
            // reported rather than run with nothing.
            arguments: crate::provider::parse_tool_arguments(&self.arguments),
            thought_signature: self.thought_signature,
        }
    }
}

/// Add one usage chunk onto the running total for a streamed call.
///
/// Every count adds, and a reported cost replaces: the counts arrive split
/// across chunks and each names a different slice of the same call, while the
/// cost, when a provider states one at all, is the whole call's price and comes
/// once.
fn merge_usage(into: TokenUsage, chunk: TokenUsage) -> TokenUsage {
    TokenUsage::new(
        into.prompt_tokens.saturating_add(chunk.prompt_tokens),
        into.cached_tokens.saturating_add(chunk.cached_tokens),
        into.cache_write_tokens
            .saturating_add(chunk.cache_write_tokens),
        into.completion_tokens
            .saturating_add(chunk.completion_tokens),
    )
    .with_reported_cost(chunk.reported_cost_usd.or(into.reported_cost_usd))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── collect_stream ─────────────────────────────────────────────────────

    /// A stream of chunks, for driving [`collect_stream`] without a socket.
    fn chunks(items: Vec<StreamChunk>) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>> {
        Box::pin(tokio_stream::iter(items.into_iter().map(Ok)))
    }

    fn text_chunk(text: &str) -> StreamChunk {
        StreamChunk {
            delta: text.to_string(),
            tool_calls: Vec::new(),
            tokens: None,
            finish_reason: None,
        }
    }

    /// The whole point of the fold: a turn arrives in pieces and the runtime is
    /// handed one finished answer, indistinguishable from a buffered one.
    #[tokio::test]
    async fn collect_stream_reassembles_text_and_a_tool_call() {
        let stream = chunks(vec![
            text_chunk("Let me "),
            text_chunk("check that."),
            StreamChunk {
                delta: String::new(),
                tool_calls: vec![ToolCallDelta {
                    index: 0,
                    id: Some("call-1".to_string()),
                    name: Some("read_file".to_string()),
                    arguments_delta: "{\"path\":".to_string(),
                    thought_signature: Some("sig-abc".to_string()),
                }],
                tokens: None,
                finish_reason: None,
            },
            // The rest of the arguments, and nothing else: an argument fragment
            // is not valid JSON on its own, which is why they are buffered as
            // text and parsed once at the end.
            StreamChunk {
                delta: String::new(),
                tool_calls: vec![ToolCallDelta {
                    index: 0,
                    id: None,
                    name: None,
                    arguments_delta: "\"a.txt\"}".to_string(),
                    thought_signature: None,
                }],
                tokens: None,
                finish_reason: None,
            },
            StreamChunk {
                delta: String::new(),
                tool_calls: Vec::new(),
                tokens: None,
                finish_reason: Some(FinishReason::ToolCall),
            },
        ]);

        let response = collect_stream(stream).await.expect("a complete stream");

        assert_eq!(response.content, "Let me check that.");
        assert_eq!(response.tool_calls.len(), 1);
        let call = &response.tool_calls[0];
        assert_eq!(call.id, "call-1");
        assert_eq!(call.name, "read_file");
        assert_eq!(call.arguments, serde_json::json!({ "path": "a.txt" }));
        // The signature arrives once, on the delta that opens the call, and a
        // later delta carrying `None` must not wipe it: Gemini 3.x refuses a
        // call replayed without it, so losing it here costs the run its next
        // turn rather than failing anything here.
        assert_eq!(call.thought_signature.as_deref(), Some("sig-abc"));
        assert_eq!(response.finish_reason, FinishReason::ToolCall);
    }

    /// A tool call whose arguments stopped arriving mid-JSON (the reply hit its
    /// output cap) is handed on as the text it had, not as `{}`: the runtime
    /// turns that shape into a refusal that tells the model what happened.
    #[tokio::test]
    async fn collect_stream_keeps_a_cut_off_tool_call_argument_as_text() {
        let stream = chunks(vec![
            StreamChunk {
                delta: String::new(),
                tool_calls: vec![ToolCallDelta {
                    index: 0,
                    id: Some("call-1".to_string()),
                    name: Some("write_file".to_string()),
                    arguments_delta: "{\"path\": \"report.md\", \"content\": \"# Title".to_string(),
                    thought_signature: None,
                }],
                tokens: None,
                finish_reason: None,
            },
            StreamChunk {
                delta: String::new(),
                tool_calls: Vec::new(),
                tokens: None,
                finish_reason: Some(FinishReason::TokenLimit),
            },
        ]);

        let response = collect_stream(stream).await.expect("a complete stream");

        assert_eq!(response.finish_reason, FinishReason::TokenLimit);
        assert_eq!(
            response.tool_calls[0].arguments,
            serde_json::json!("{\"path\": \"report.md\", \"content\": \"# Title")
        );
    }

    /// Usage chunks add up rather than replacing one another.
    ///
    /// Anthropic reports its input counts on `message_start` and its output
    /// count on `message_delta`, so keeping only the last chunk would report
    /// every streamed turn's prompt as zero - and with the three input counts
    /// disjoint and priced differently, that is a bill, not a display bug.
    #[tokio::test]
    async fn collect_stream_adds_usage_across_chunks() {
        let stream = chunks(vec![
            StreamChunk {
                delta: String::new(),
                tool_calls: Vec::new(),
                tokens: Some(TokenUsage::new(100, 20, 5, 0)),
                finish_reason: None,
            },
            StreamChunk {
                delta: String::new(),
                tool_calls: Vec::new(),
                tokens: Some(TokenUsage::new(0, 0, 0, 42).with_reported_cost(Some(0.25))),
                finish_reason: Some(FinishReason::Complete),
            },
        ]);

        let response = collect_stream(stream).await.expect("a complete stream");

        assert_eq!(response.tokens_used.prompt_tokens, 100);
        assert_eq!(response.tokens_used.cached_tokens, 20);
        assert_eq!(response.tokens_used.cache_write_tokens, 5);
        assert_eq!(response.tokens_used.completion_tokens, 42);
        assert_eq!(response.tokens_used.total_tokens, 167);
        // The price the provider stated survives the fold. It is the only
        // figure that is not arithmetic on published rates, and for a gateway
        // model we hold no rates for it is the only one at all.
        assert_eq!(response.tokens_used.reported_cost_usd, Some(0.25));
    }

    /// A stream that stops before the model says it has finished is a dropped
    /// connection, not a short answer.
    ///
    /// Reporting `Complete` would hand the agent a truncated turn as though the
    /// model had chosen to stop there - the run would carry on from half a
    /// sentence, and nothing downstream could tell. As a transport failure it
    /// is transient, so the dispatch loop simply retries it.
    #[tokio::test]
    async fn collect_stream_refuses_a_stream_that_stopped_early() {
        let err = collect_stream(chunks(vec![text_chunk("half a sen")]))
            .await
            .expect_err("no finish reason means no finished turn");

        assert_eq!(err.failure_kind(), Some(FailureKind::ConnectionDropped));
        assert!(
            err.is_transient(),
            "so the call is retried rather than lost"
        );
    }

    /// An error mid-stream is the caller's, verbatim: it already carries the
    /// kind and the remedy, and re-wrapping it would bury both.
    #[tokio::test]
    async fn collect_stream_propagates_a_mid_stream_failure() {
        let items: Vec<Result<StreamChunk>> = vec![
            Ok(text_chunk("started")),
            Err(ProviderError::labelled(
                FailureKind::Timeout,
                "reading the response stream",
                "nothing more arrived",
            )),
        ];
        let stream: Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>> =
            Box::pin(tokio_stream::iter(items));

        let err = collect_stream(stream).await.expect_err("the stream failed");

        assert_eq!(err.failure_kind(), Some(FailureKind::Timeout));
    }

    /// Chunk counts accumulate across a stream; two large reports must clamp
    /// rather than overflow the running total.
    #[test]
    fn merged_usage_saturates_instead_of_aborting() {
        let a = TokenUsage::new(usize::MAX, 0, 0, 0);
        let b = TokenUsage::new(1, 0, 0, 1);
        let merged = merge_usage(a, b);
        assert_eq!(merged.prompt_tokens, usize::MAX);
        assert_eq!(merged.total_tokens, usize::MAX);
    }
}

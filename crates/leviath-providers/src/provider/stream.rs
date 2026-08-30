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
use leviath_net::read_caps::{STREAM_FRAME_CAP, frame_within_cap};

/// The raw bytes a provider's streaming endpoint sends back.
///
/// Boxed as a trait object rather than kept generic. In production this is
/// always `reqwest`'s `bytes_stream()`; tests inject dozens of distinct mock
/// stream types, and a generic `impl<S> Stream` makes `cargo llvm-cov`
/// instrument each monomorphized `poll_next` separately, leaving some
/// artificially "uncovered" even though the shared logic is fully exercised.
/// Boxing collapses all of that into one concrete `poll_next`.
pub type ByteStream =
    Pin<Box<dyn Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send>>;

/// Cut one frame off the front of the buffer, if a whole one has arrived.
///
/// `None` means "not yet, poll for more bytes"; `Some(None)` means the frame
/// said the stream is over; `Some(Some(item))` is a chunk or an error the
/// provider delivered inside the stream. A framer that consumed a frame with
/// nothing in it (a keepalive, a `ping`) may answer `None` and be asked
/// again.
pub type FrameFn = Box<dyn FnMut(&mut String) -> Option<Option<Result<StreamChunk>>> + Send>;

/// What to make of whatever is left in the buffer once the bytes stop.
///
/// Most wire formats end every frame with a delimiter, so a leftover is a
/// torn frame and there is nothing to do. Ollama's NDJSON does not promise a
/// trailing newline, so its last line can only be read here.
pub type FlushFn = Box<dyn FnMut(&mut String) -> Option<StreamChunk> + Send>;

/// The bytes of a character the transport cut in half, held until the rest
/// arrives.
///
/// `bytes_stream()` hands over whatever the socket had, and the socket does
/// not know where a character ends: a four-byte emoji, a CJK character or a
/// dash inside a delta is routinely split over two chunks. Checking each
/// chunk on its own and dropping the ones that failed lost the whole chunk
/// around the split, silently. Decoding the longest valid prefix and carrying
/// the rest into the next chunk loses nothing; bytes that could never be
/// UTF-8 become U+FFFD, so a peer that sends garbage is visible in the
/// output rather than absent from it.
#[derive(Default)]
pub(crate) struct Utf8Carry {
    tail: Vec<u8>,
}

impl Utf8Carry {
    /// Decode `bytes` (after whatever was carried) onto `out`, keeping an
    /// incomplete trailing character back for the next call.
    pub(crate) fn push(&mut self, bytes: &[u8], out: &mut String) {
        let owned;
        let mut input: &[u8] = if self.tail.is_empty() {
            bytes
        } else {
            self.tail.extend_from_slice(bytes);
            owned = std::mem::take(&mut self.tail);
            &owned
        };
        loop {
            match std::str::from_utf8(input) {
                Ok(text) => {
                    out.push_str(text);
                    return;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    // The prefix was checked by the failed call above.
                    out.push_str(&String::from_utf8_lossy(&input[..valid]));
                    match e.error_len() {
                        Some(bad) => {
                            out.push('\u{FFFD}');
                            input = &input[valid + bad..];
                        }
                        None => {
                            self.tail = input[valid..].to_vec();
                            return;
                        }
                    }
                }
            }
        }
    }

    /// How many bytes are held back, so a frame cap can count them.
    pub(crate) fn pending(&self) -> usize {
        self.tail.len()
    }

    /// The bytes are over: a character still waiting for its end is not
    /// coming, so mark it rather than lose it.
    pub(crate) fn finish(&mut self, out: &mut String) {
        if !self.tail.is_empty() {
            self.tail.clear();
            out.push('\u{FFFD}');
        }
    }
}

/// A byte stream cut into [`StreamChunk`]s by a provider-specific framer.
///
/// One `poll_next` for every provider: one buffer, one UTF-8 handling, one
/// transport-error mapping, with only the framing call differing per provider.
pub struct FramedStream {
    inner: ByteStream,
    buffer: String,
    /// A character the last chunk ended in the middle of.
    carry: Utf8Carry,
    parse: FrameFn,
    flush: Option<FlushFn>,
    /// The most `buffer` may hold between frames before the stream fails;
    /// [`STREAM_FRAME_CAP`] in production, smaller in the test that hits it.
    frame_cap: usize,
    /// Who the bytes are from, for the cap error. The host, once a provider
    /// has named it with [`FramedStream::sent_by`].
    peer: String,
}

impl FramedStream {
    /// Wrap `inner`, framing it with `parse` and, at the end, `flush`.
    pub fn new<S>(inner: S, parse: FrameFn, flush: Option<FlushFn>) -> Self
    where
        S: Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    {
        Self {
            inner: Box::pin(inner),
            buffer: String::new(),
            carry: Utf8Carry::default(),
            parse,
            flush,
            frame_cap: STREAM_FRAME_CAP,
            peer: "the provider".to_string(),
        }
    }

    /// Name the peer the cap error reports; every provider passes the host
    /// it connected to.
    pub fn sent_by(mut self, peer: String) -> Self {
        self.peer = peer;
        self
    }

    /// Lower the frame cap, so a test can overrun it with a few kilobytes.
    #[cfg(test)]
    pub fn with_frame_cap(mut self, cap: usize) -> Self {
        self.frame_cap = cap;
        self
    }
}

impl Stream for FramedStream {
    type Item = Result<StreamChunk>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(item) = (this.parse)(&mut this.buffer) {
                return std::task::Poll::Ready(item);
            }
            match this.inner.as_mut().poll_next(cx) {
                std::task::Poll::Ready(Some(Ok(bytes))) => {
                    this.carry.push(&bytes, &mut this.buffer);
                    // Checked after the frames were cut (the top of the
                    // loop), so what is measured is one partial frame: a peer
                    // that never sends a boundary, not a fast one. The bytes
                    // held back for the next chunk are part of that frame.
                    if let Err(msg) = frame_within_cap(
                        this.buffer.len() + this.carry.pending(),
                        this.frame_cap,
                        &this.peer,
                    ) {
                        this.buffer.clear();
                        return std::task::Poll::Ready(Some(Err(ProviderError::InvalidResponse(
                            msg,
                        ))));
                    }
                }
                std::task::Poll::Ready(Some(Err(e))) => {
                    return std::task::Poll::Ready(Some(Err(ProviderError::transport(
                        "reading the response stream",
                        &e,
                    ))));
                }
                std::task::Poll::Ready(None) => {
                    // The bytes are over: a frame that arrived whole with the
                    // last of them, then whatever the format does with a tail.
                    this.carry.finish(&mut this.buffer);
                    if let Some(item) = (this.parse)(&mut this.buffer) {
                        return std::task::Poll::Ready(item);
                    }
                    if let Some(flush) = this.flush.as_mut()
                        && let Some(chunk) = flush(&mut this.buffer)
                    {
                        return std::task::Poll::Ready(Some(Ok(chunk)));
                    }
                    return std::task::Poll::Ready(None);
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

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
    /// A peer that streams forever without a frame boundary is stopped at
    /// the cap with an error naming it, and the buffer it grew is let go.
    #[tokio::test]
    async fn a_frame_that_never_closes_fails_at_the_cap() {
        use tokio_stream::StreamExt as _;
        // 100 chunks of 100 bytes, no `\n\n` anywhere: one frame, 10 KB.
        let chunks = (0..100)
            .map(|_| Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from(vec![b'x'; 100])));
        let mut stream = FramedStream::new(
            tokio_stream::iter(chunks),
            Box::new(|_buffer: &mut String| None),
            None,
        )
        .sent_by("api.example".to_string())
        .with_frame_cap(4096);

        let err = stream
            .next()
            .await
            .expect("the stream yields the cap error")
            .expect_err("it is an error");
        assert_eq!(
            err.to_string(),
            "Invalid response: stream frame exceeded 4096 bytes from api.example"
        );
        assert!(stream.buffer.is_empty(), "the oversized frame is released");
    }

    // ─── UTF-8 across chunk boundaries ──────────────────────────────────────

    /// A framed stream over `chunks`, cut on newlines: every line is one text
    /// chunk, and a last line with no newline is flushed at the end.
    fn line_framed(chunks: Vec<Vec<u8>>) -> FramedStream {
        let bytes = chunks
            .into_iter()
            .map(|c| Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from(c)));
        FramedStream::new(
            tokio_stream::iter(bytes),
            Box::new(|buffer: &mut String| {
                let idx = buffer.find('\n')?;
                let line: String = buffer.drain(..=idx).collect();
                Some(Some(Ok(text_chunk(line.trim_end_matches('\n')))))
            }),
            Some(Box::new(|buffer: &mut String| {
                if buffer.is_empty() {
                    None
                } else {
                    Some(text_chunk(&std::mem::take(buffer)))
                }
            })),
        )
    }

    async fn deltas(mut stream: FramedStream) -> Vec<String> {
        use tokio_stream::StreamExt as _;
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            out.push(item.expect("a text chunk").delta);
        }
        out
    }

    /// The transport cuts wherever it likes, including through a character.
    /// reqwest's `bytes_stream()` hands over whatever the socket had, so a
    /// four-byte emoji is routinely two bytes in one chunk and two in the
    /// next. Both halves have to be joined, or the run loses the whole chunk
    /// around them with no error.
    #[tokio::test]
    async fn a_character_split_across_two_chunks_arrives_whole() {
        let text = "done 🎉\n".as_bytes();
        // The emoji is bytes 5..9: split it 2+2.
        let stream = line_framed(vec![text[..7].to_vec(), text[7..].to_vec()]);
        assert_eq!(deltas(stream).await, vec!["done 🎉".to_string()]);
    }

    /// A three-byte CJK character cut 1+2, with the cut chunk carrying more
    /// text on either side: the neighbours survive with it.
    #[tokio::test]
    async fn a_three_byte_character_split_one_and_two_arrives_whole() {
        let text = "完成\n".as_bytes();
        let stream = line_framed(vec![text[..4].to_vec(), text[4..].to_vec()]);
        assert_eq!(deltas(stream).await, vec!["完成".to_string()]);
    }

    /// A stream that stops inside a character: what came before it is kept,
    /// and the torn character is marked rather than silently gone.
    #[tokio::test]
    async fn a_partial_character_at_stream_end_is_marked_not_dropped() {
        let text = "done 🎉".as_bytes();
        let stream = line_framed(vec![text[..7].to_vec()]);
        assert_eq!(deltas(stream).await, vec!["done \u{FFFD}".to_string()]);
    }

    /// Bytes that can never be UTF-8 are replaced, one marker per bad
    /// sequence, and the good text around them is kept. The frame cap counts
    /// the bytes held back for the next chunk.
    #[test]
    fn utf8_carry_replaces_invalid_bytes_and_counts_its_tail() {
        let mut carry = Utf8Carry::default();
        let mut out = String::new();
        carry.push(&[b'a', 0xFF, 0xFE, b'b'], &mut out);
        assert_eq!(out, "a\u{FFFD}\u{FFFD}b");
        assert_eq!(carry.pending(), 0);
        // One lead byte of a three-byte character is held, not decoded.
        carry.push(&[0xE5], &mut out);
        assert_eq!(out, "a\u{FFFD}\u{FFFD}b");
        assert_eq!(carry.pending(), 1);
        carry.push(&[0xAE, 0x8C], &mut out);
        assert_eq!(out, "a\u{FFFD}\u{FFFD}b完");
        assert_eq!(carry.pending(), 0);
        carry.push(&[0xF0, 0x9F], &mut out);
        carry.finish(&mut out);
        assert_eq!(out, "a\u{FFFD}\u{FFFD}b完\u{FFFD}");
        assert_eq!(carry.pending(), 0);
    }

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

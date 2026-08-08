//! Server-Sent Events framing.
//!
//! Both MCP HTTP transports carry JSON-RPC inside SSE: the streamable one may
//! answer a POST with an event stream, and the legacy one delivers every
//! server→client message that way. Only the `event:` name and the `data:`
//! payload matter here; `id:` and `retry:` are accepted and discarded.

/// One decoded SSE event.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// The `event:` name, when the server sent one.
    pub(crate) event: Option<String>,
    /// The concatenated `data:` lines.
    pub(crate) data: String,
}

/// Pull the next complete event off the front of `buffer`.
///
/// Returns `None` when the buffer does not yet hold a whole event, leaving it
/// untouched so the caller can append more bytes and retry. Events are
/// terminated by a blank line; `\r\n` is normalized, since a server behind a
/// proxy may emit either ending.
pub(crate) fn parse_sse_frame(buffer: &mut String) -> Option<SseEvent> {
    // A frame ends at the first blank line, in whichever line ending arrives.
    // Copy the frame out, then drop it and its terminator from the front: what
    // `split_frame` left as the remainder is exactly what should stay behind, so
    // the length of the terminator never has to be named.
    let (raw, remainder) = {
        let (frame, rest) = split_frame(buffer)?;
        (frame.to_string(), rest.len())
    };
    buffer.drain(..buffer.len() - remainder);

    let mut event = None;
    let mut data_lines: Vec<&str> = Vec::new();

    for line in raw.lines() {
        // A leading colon marks a comment, commonly used as a keepalive.
        if line.starts_with(':') {
            continue;
        }
        let Some((field, value)) = line.split_once(':') else {
            // A field with no colon is a name with an empty value; nothing we
            // consume has meaning without one.
            continue;
        };
        // Exactly one optional leading space is part of the framing, not data.
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data_lines.push(value),
            // `id` and `retry` are valid but irrelevant to MCP.
            _ => {}
        }
    }

    Some(SseEvent {
        event,
        // Multi-line data is rejoined with newlines, per the SSE spec.
        data: data_lines.join("\n"),
    })
}

/// The first complete frame and everything after its terminator.
fn split_frame(buffer: &str) -> Option<(&str, &str)> {
    let lf = buffer.split_once("\n\n");
    let crlf = buffer.split_once("\r\n\r\n");
    match (lf, crlf) {
        // Whichever terminator comes first wins; a `\r\n\r\n` also contains a
        // `\n\r\n`, so comparing how much text precedes each is what keeps them
        // straight. Splitting rather than locating means neither terminator's
        // length is written down, so neither can drift from the string it
        // belongs to.
        (Some(l), Some(c)) if c.0.len() <= l.0.len() => Some(c),
        (Some(l), _) => Some(l),
        (None, Some(c)) => Some(c),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> (Option<SseEvent>, String) {
        let mut buffer = input.to_string();
        let event = parse_sse_frame(&mut buffer);
        (event, buffer)
    }

    #[test]
    fn parses_a_named_event() {
        let (event, rest) = parse("event: endpoint\ndata: /messages?id=1\n\n");
        assert_eq!(
            event.unwrap(),
            SseEvent {
                event: Some("endpoint".to_string()),
                data: "/messages?id=1".to_string(),
            }
        );
        assert!(rest.is_empty(), "the frame should be consumed");
    }

    #[test]
    fn parses_an_unnamed_event() {
        let (event, _) = parse("data: {\"jsonrpc\":\"2.0\"}\n\n");
        let event = event.unwrap();
        assert_eq!(event.event, None);
        assert_eq!(event.data, "{\"jsonrpc\":\"2.0\"}");
    }

    #[test]
    fn incomplete_frame_is_left_in_the_buffer() {
        // Chunked transports hand us partial frames constantly.
        let (event, rest) = parse("data: half");
        assert!(event.is_none());
        assert_eq!(rest, "data: half", "buffer must be untouched");
    }

    #[test]
    fn only_the_first_frame_is_consumed() {
        let (event, rest) = parse("data: one\n\ndata: two\n\n");
        assert_eq!(event.unwrap().data, "one");
        assert_eq!(rest, "data: two\n\n");
    }

    #[test]
    fn handles_crlf_line_endings() {
        // Proxies in front of a server routinely rewrite line endings.
        let (event, rest) = parse("event: message\r\ndata: hi\r\n\r\n");
        assert_eq!(
            event.unwrap(),
            SseEvent {
                event: Some("message".to_string()),
                data: "hi".to_string(),
            }
        );
        assert!(rest.is_empty());
    }

    #[test]
    fn joins_multi_line_data() {
        let (event, _) = parse("data: line one\ndata: line two\n\n");
        assert_eq!(event.unwrap().data, "line one\nline two");
    }

    #[test]
    fn skips_comment_keepalives() {
        // A bare `: ping` comment is how servers keep an idle stream warm.
        let (event, _) = parse(": ping\ndata: real\n\n");
        assert_eq!(event.unwrap().data, "real");
    }

    #[test]
    fn ignores_id_and_retry_fields() {
        let (event, _) = parse("id: 42\nretry: 3000\ndata: payload\n\n");
        let event = event.unwrap();
        assert_eq!(event.data, "payload");
        assert_eq!(event.event, None);
    }

    #[test]
    fn ignores_a_field_with_no_colon() {
        let (event, _) = parse("bogus\ndata: payload\n\n");
        assert_eq!(event.unwrap().data, "payload");
    }

    #[test]
    fn keeps_only_one_leading_space_as_framing() {
        // "data:  x" is the value " x", not "x".
        let (event, _) = parse("data:  padded\n\n");
        assert_eq!(event.unwrap().data, " padded");
    }

    #[test]
    fn value_without_a_leading_space_is_taken_verbatim() {
        let (event, _) = parse("data:tight\n\n");
        assert_eq!(event.unwrap().data, "tight");
    }

    #[test]
    fn an_empty_frame_yields_empty_data() {
        let (event, _) = parse("\n\nleftover");
        let event = event.unwrap();
        assert_eq!(event.data, "");
        assert_eq!(event.event, None);
    }

    #[test]
    fn crlf_frame_before_an_lf_frame_is_split_correctly() {
        // Exercises the branch where both terminators are present and the
        // CRLF one comes first.
        let (event, rest) = parse("data: a\r\n\r\ndata: b\n\n");
        assert_eq!(event.unwrap().data, "a");
        assert_eq!(rest, "data: b\n\n");
    }

    #[test]
    fn lf_frame_before_a_crlf_frame_is_split_correctly() {
        let (event, rest) = parse("data: a\n\ndata: b\r\n\r\n");
        assert_eq!(event.unwrap().data, "a");
        assert_eq!(rest, "data: b\r\n\r\n");
    }
}

//! Server-Sent Events framing.
//!
//! The model providers that stream over SSE and both MCP HTTP transports
//! differ only in what the payload means. How bytes become events is the same
//! everywhere, so it is written once, here, to the spec: a line ends in `\n`
//! or `\r\n`, an event ends at a blank line, the space after a field's colon
//! is optional, and several `data:` lines are one payload joined with `\n`.
//! Only `event` and `data` are kept; `id`, `retry` and comments are read past.
//! A bare `\r` line ending, which the spec also allows, is not recognised: no
//! server these talk to sends one. [`Utf8Carry`] turns a body's chunks into
//! that text without breaking a character a chunk ends inside.

/// One decoded event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` name, when the server sent one.
    pub event: Option<String>,
    /// The `data:` lines, joined with `\n`. Empty for an event that carried
    /// none, such as a comment sent as a keepalive.
    pub data: String,
}

/// Cut the next complete event off the front of `buffer`.
///
/// `None` while the buffer does not yet hold a whole event, leaving it
/// untouched so the caller can append more bytes and ask again. The event and
/// its blank line are drained from the front, so what stays behind is never
/// copied.
pub fn next_event(buffer: &mut String) -> Option<SseEvent> {
    let (event, consumed) = first_event(buffer)?;
    buffer.drain(..consumed);
    Some(event)
}

/// Whatever is left in `buffer` once the bytes stop, read as an event whose
/// blank line never came; `None` when nothing but whitespace is left.
///
/// The spec discards such a tail, and most callers should too: a stream that
/// ends mid-event was cut, not finished. This is for a peer known to end its
/// last event at the end of the body instead.
pub fn final_event(buffer: &mut String) -> Option<SseEvent> {
    let rest = std::mem::take(buffer);
    if rest.trim().is_empty() {
        return None;
    }
    let mut fields = Fields::default();
    for line in rest.lines() {
        fields.read(line_text(line));
    }
    Some(fields.into_event())
}

/// The bytes of a character the transport cut in half, held until the rest
/// arrives, so a byte stream can be read as the text an event stream is.
///
/// A body stream hands over whatever the socket had, and the socket does not
/// know where a character ends: a four-byte emoji, a CJK character or a dash
/// inside a payload is routinely split over two chunks. Checking each chunk on
/// its own would fail or drop the chunk around the split. Decoding the longest
/// valid prefix and carrying the rest into the next chunk loses nothing; bytes
/// that could never be UTF-8 become U+FFFD, so a peer that sends garbage is
/// visible in the output rather than absent from it.
#[derive(Debug, Default)]
pub struct Utf8Carry {
    tail: Vec<u8>,
}

impl Utf8Carry {
    /// Decode `bytes` (after whatever was carried) onto `out`, keeping an
    /// incomplete trailing character back for the next call.
    pub fn push(&mut self, bytes: &[u8], out: &mut String) {
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
    pub fn pending(&self) -> usize {
        self.tail.len()
    }

    /// The bytes are over: a character still waiting for its end is not
    /// coming, so mark it rather than lose it.
    pub fn finish(&mut self, out: &mut String) {
        if !self.tail.is_empty() {
            self.tail.clear();
            out.push('\u{FFFD}');
        }
    }
}

/// The first complete event in `buffer`, and how many bytes it and the blank
/// line ending it take up.
fn first_event(buffer: &str) -> Option<(SseEvent, usize)> {
    let mut fields = Fields::default();
    let mut consumed = 0;
    for line in buffer.split_inclusive('\n') {
        // A line with no newline yet is still arriving, and so is its event.
        let text = line.strip_suffix('\n')?;
        consumed += line.len();
        let text = line_text(text);
        if text.is_empty() {
            return Some((fields.into_event(), consumed));
        }
        fields.read(text);
    }
    None
}

/// A line without the `\r` of a `\r\n` ending.
fn line_text(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

/// The fields of one event, as its lines are read.
#[derive(Default)]
struct Fields {
    event: Option<String>,
    data: Option<String>,
}

impl Fields {
    /// Take in one non-blank line.
    fn read(&mut self, line: &str) {
        // A leading colon marks a comment, which servers send as a keepalive.
        if line.starts_with(':') {
            return;
        }
        // A line with no colon is a field name with an empty value.
        let (name, value) = line.split_once(':').unwrap_or((line, ""));
        // Exactly one leading space is framing; any more is the value's own.
        let value = value.strip_prefix(' ').unwrap_or(value);
        match name {
            "event" => self.event = Some(value.to_string()),
            "data" => match &mut self.data {
                Some(data) => {
                    data.push('\n');
                    data.push_str(value);
                }
                None => self.data = Some(value.to_string()),
            },
            // `id` and `retry` are valid and mean nothing to these callers.
            _ => {}
        }
    }

    fn into_event(self) -> SseEvent {
        SseEvent {
            event: self.event,
            data: self.data.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> (Option<SseEvent>, String) {
        let mut buffer = input.to_string();
        let event = next_event(&mut buffer);
        (event, buffer)
    }

    fn data(input: &str) -> String {
        parse(input).0.expect("a whole event").data
    }

    #[test]
    fn a_named_event_is_read_and_consumed() {
        let (event, rest) = parse("event: endpoint\ndata: /messages?id=1\n\n");
        assert_eq!(
            event.unwrap(),
            SseEvent {
                event: Some("endpoint".to_string()),
                data: "/messages?id=1".to_string(),
            }
        );
        assert!(rest.is_empty(), "the event and its blank line are consumed");
    }

    #[test]
    fn an_unnamed_event_has_no_name() {
        let (event, _) = parse("data: {\"jsonrpc\":\"2.0\"}\n\n");
        let event = event.unwrap();
        assert_eq!(event.event, None);
        assert_eq!(event.data, "{\"jsonrpc\":\"2.0\"}");
    }

    /// The transport cuts wherever it likes: mid-field, after a line, and
    /// between the two halves of the blank line. None of those is an event yet,
    /// and none of them may lose a byte.
    #[test]
    fn an_event_split_across_chunks_waits_for_its_blank_line() {
        let mut buffer = String::new();
        for chunk in ["da", "ta: one\r", "\n", "\r", "\ndata: two\n\n"] {
            assert_eq!(next_event(&mut buffer), None, "after {buffer:?}");
            buffer.push_str(chunk);
        }
        assert_eq!(next_event(&mut buffer).unwrap().data, "one");
        assert_eq!(next_event(&mut buffer).unwrap().data, "two");
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_partial_event_is_left_in_the_buffer() {
        let (event, rest) = parse("data: half\n");
        assert!(event.is_none());
        assert_eq!(rest, "data: half\n", "the buffer must be untouched");
    }

    #[test]
    fn only_the_first_event_is_consumed() {
        let (event, rest) = parse("data: one\n\ndata: two\n\n");
        assert_eq!(event.unwrap().data, "one");
        assert_eq!(rest, "data: two\n\n");
    }

    /// Proxies in front of a server routinely rewrite line endings, and a
    /// stream may even mix them.
    #[test]
    fn crlf_and_lf_endings_frame_the_same_way() {
        let (event, rest) = parse("event: message\r\ndata: hi\r\n\r\n");
        assert_eq!(
            event.unwrap(),
            SseEvent {
                event: Some("message".to_string()),
                data: "hi".to_string(),
            }
        );
        assert!(rest.is_empty());
        assert_eq!(data("data: a\r\n\n"), "a");
        assert_eq!(data("data: a\n\r\n"), "a");
        let (event, rest) = parse("data: a\r\n\r\ndata: b\n\n");
        assert_eq!(event.unwrap().data, "a");
        assert_eq!(rest, "data: b\n\n");
    }

    #[test]
    fn multi_line_data_is_joined_with_newlines() {
        assert_eq!(
            data("data: line one\ndata: line two\n\n"),
            "line one\nline two"
        );
        assert_eq!(
            data("data\ndata: x\n\n"),
            "\nx",
            "a bare `data` is an empty line"
        );
    }

    #[test]
    fn comments_ids_and_retries_are_read_past() {
        let (event, _) = parse(": ping\nid: 42\nretry: 3000\nbogus\ndata: payload\n\n");
        assert_eq!(
            event.unwrap(),
            SseEvent {
                event: None,
                data: "payload".to_string(),
            }
        );
    }

    /// The space after the colon is optional, and only one is framing.
    #[test]
    fn exactly_one_leading_space_is_framing() {
        assert_eq!(data("data:tight\n\n"), "tight");
        assert_eq!(data("data:  padded\n\n"), " padded");
    }

    #[test]
    fn an_event_with_no_data_has_empty_data() {
        let (event, rest) = parse("\n\nleftover");
        assert_eq!(event.unwrap(), SseEvent::default());
        assert_eq!(rest, "\nleftover");
    }

    #[test]
    fn the_last_event_of_a_stream_is_read_without_its_blank_line() {
        let mut buffer = "event: done\r\ndata: a\ndata: b\r".to_string();
        assert_eq!(
            final_event(&mut buffer),
            Some(SseEvent {
                event: Some("done".to_string()),
                data: "a\nb".to_string(),
            })
        );
        assert!(buffer.is_empty(), "the tail is taken");

        let mut blank = " \r\n\n".to_string();
        assert_eq!(final_event(&mut blank), None);
        assert!(blank.is_empty());
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
        // Finishing with nothing held adds nothing.
        carry.finish(&mut out);
        assert_eq!(out, "a\u{FFFD}\u{FFFD}b完\u{FFFD}");
    }
}

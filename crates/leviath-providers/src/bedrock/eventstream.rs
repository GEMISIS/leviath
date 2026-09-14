//! The binary framing Bedrock streams a reply in.
//!
//! `ConverseStream` does not answer in server-sent events. The body is
//! `application/vnd.amazon.eventstream`: a sequence of binary messages, each
//! a prelude of two big-endian lengths and a CRC, a block of typed headers, a
//! payload, and a CRC over the whole thing. The headers name what the message
//! is (`:message-type`, `:event-type`, `:exception-type`) and the payload is
//! the JSON event. The shared [`crate::provider::stream::FramedStream`]
//! cannot carry this: it buffers text and rewrites any byte that is not
//! UTF-8, which the length words and the CRCs routinely are.
//!
//! This module is the pure half: bytes in, frames out, no I/O. The stream
//! that feeds it lives in `super::stream`.

/// One decoded message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Frame {
    /// The headers, in wire order.
    pub(super) headers: Vec<(String, HeaderValue)>,
    /// The bytes after the headers and before the trailing CRC.
    pub(super) payload: Vec<u8>,
}

/// A header value, in every type the encoding defines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HeaderValue {
    /// Types 0 and 1.
    Bool(bool),
    /// Type 2.
    I8(i8),
    /// Type 3.
    I16(i16),
    /// Type 4.
    I32(i32),
    /// Type 5.
    I64(i64),
    /// Type 6.
    Bytes(Vec<u8>),
    /// Type 7, the one every header Bedrock sends uses.
    String(String),
    /// Type 8, milliseconds since the epoch.
    Timestamp(i64),
    /// Type 9.
    Uuid([u8; 16]),
}

impl Frame {
    /// The first string header called `name`, if there is one.
    pub(super) fn header_str(&self, name: &str) -> Option<&str> {
        self.headers.iter().find_map(|(n, v)| match v {
            HeaderValue::String(s) if n == name => Some(s.as_str()),
            _ => None,
        })
    }

    /// `event`, `exception` or `error`.
    pub(super) fn message_type(&self) -> Option<&str> {
        self.header_str(":message-type")
    }

    /// Which event this is, for an `event` message.
    pub(super) fn event_type(&self) -> Option<&str> {
        self.header_str(":event-type")
    }

    /// Which exception this is, for an `exception` message.
    pub(super) fn exception_type(&self) -> Option<&str> {
        self.header_str(":exception-type")
    }
}

/// Why a buffer could not be read as a frame.
///
/// Every one of these means the bytes are not an event stream, or stopped
/// being one, and the specification says to end the stream on either CRC
/// failing. The caller does exactly that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FrameError {
    /// The CRC over the two length words did not match.
    PreludeCrc,
    /// The CRC over the whole message did not match.
    MessageCrc,
    /// The lengths do not describe a message: shorter than the fixed
    /// overhead, headers longer than the message, or longer than the cap.
    Length,
    /// A header carried a type byte the encoding does not define.
    HeaderType(u8),
    /// The header block ended in the middle of a header.
    Truncated,
    /// A header name was not UTF-8.
    HeaderName,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::PreludeCrc => write!(f, "the prelude checksum did not match"),
            FrameError::MessageCrc => write!(f, "the message checksum did not match"),
            FrameError::Length => write!(f, "the frame lengths do not describe a message"),
            FrameError::HeaderType(t) => write!(f, "a header has the unknown type {t}"),
            FrameError::Truncated => write!(f, "the header block ended inside a header"),
            FrameError::HeaderName => write!(f, "a header name is not UTF-8"),
        }
    }
}

/// The fixed bytes every message carries: two length words, the prelude CRC
/// and the message CRC.
const OVERHEAD: usize = 16;

/// The prelude: the two length words and their CRC.
const PRELUDE: usize = 12;

/// The CRC the encoding uses, which is the IEEE one gzip uses.
pub(super) fn crc32(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

/// Cut one frame off the front of `buf`, if a whole one has arrived.
///
/// `Ok(None)` means more bytes are needed and nothing was consumed. `Ok(Some)`
/// drains the frame's bytes. `Err` means the bytes are not a frame, and the
/// caller ends the stream: nothing after a corrupt prelude can be trusted to
/// start where a frame starts. `cap` bounds a message's declared length, so
/// a hostile length word cannot make the caller buffer gigabytes waiting for
/// a message that never completes.
pub(super) fn decode(buf: &mut Vec<u8>, cap: usize) -> Result<Option<Frame>, FrameError> {
    let Some(prelude) = buf.get(..PRELUDE) else {
        return Ok(None);
    };
    let total = u32::from_be_bytes(word(prelude, 0)) as usize;
    let headers_len = u32::from_be_bytes(word(prelude, 4)) as usize;
    let prelude_crc = u32::from_be_bytes(word(prelude, 8));
    // Checked before the lengths are believed: the CRC covers the two length
    // words, so a corrupt length fails here rather than as a nonsense size.
    if crc32(&prelude[..8]) != prelude_crc {
        return Err(FrameError::PreludeCrc);
    }
    if total < OVERHEAD || headers_len > total - OVERHEAD || total > cap {
        return Err(FrameError::Length);
    }
    let Some(message) = buf.get(..total) else {
        return Ok(None);
    };
    let body_end = total - 4;
    let message_crc = u32::from_be_bytes(word(message, body_end));
    if crc32(&message[..body_end]) != message_crc {
        return Err(FrameError::MessageCrc);
    }
    let headers = parse_headers(&message[PRELUDE..PRELUDE + headers_len])?;
    let payload = message[PRELUDE + headers_len..body_end].to_vec();
    buf.drain(..total);
    Ok(Some(Frame { headers, payload }))
}

/// Four bytes of `bytes` at `at`, which the caller has already bounds-checked.
fn word(bytes: &[u8], at: usize) -> [u8; 4] {
    let mut out = [0u8; 4];
    out.copy_from_slice(&bytes[at..at + 4]);
    out
}

/// The headers in a header block.
fn parse_headers(mut block: &[u8]) -> Result<Vec<(String, HeaderValue)>, FrameError> {
    let mut headers = Vec::new();
    while let Some((&name_len, rest)) = block.split_first() {
        block = rest;
        let name = String::from_utf8(take(&mut block, name_len as usize)?.to_vec())
            .map_err(|_| FrameError::HeaderName)?;
        let value = match take(&mut block, 1)?[0] {
            0 => HeaderValue::Bool(true),
            1 => HeaderValue::Bool(false),
            2 => HeaderValue::I8(take(&mut block, 1)?[0] as i8),
            3 => HeaderValue::I16(i16::from_be_bytes(fixed(take(&mut block, 2)?))),
            4 => HeaderValue::I32(i32::from_be_bytes(fixed(take(&mut block, 4)?))),
            5 => HeaderValue::I64(i64::from_be_bytes(fixed(take(&mut block, 8)?))),
            6 => {
                let len = u16::from_be_bytes(fixed(take(&mut block, 2)?)) as usize;
                HeaderValue::Bytes(take(&mut block, len)?.to_vec())
            }
            7 => {
                let len = u16::from_be_bytes(fixed(take(&mut block, 2)?)) as usize;
                HeaderValue::String(
                    String::from_utf8(take(&mut block, len)?.to_vec())
                        .map_err(|_| FrameError::HeaderName)?,
                )
            }
            8 => HeaderValue::Timestamp(i64::from_be_bytes(fixed(take(&mut block, 8)?))),
            9 => HeaderValue::Uuid(fixed(take(&mut block, 16)?)),
            other => return Err(FrameError::HeaderType(other)),
        };
        headers.push((name, value));
    }
    Ok(headers)
}

/// The first `n` bytes of `cursor`, advancing it; `Truncated` if there are
/// fewer.
fn take<'a>(cursor: &mut &'a [u8], n: usize) -> Result<&'a [u8], FrameError> {
    let (head, tail) = cursor.split_at_checked(n).ok_or(FrameError::Truncated)?;
    *cursor = tail;
    Ok(head)
}

/// `bytes` as an array of its own length, which [`take`] guaranteed.
fn fixed<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(bytes);
    out
}

/// Building frames, for the tests here and in `super::stream`.
#[cfg(test)]
pub(super) mod fixtures {
    use super::{HeaderValue, crc32};

    /// One header as its wire bytes.
    fn encode_header(name: &str, value: &HeaderValue) -> Vec<u8> {
        let mut out = vec![name.len() as u8];
        out.extend_from_slice(name.as_bytes());
        match value {
            HeaderValue::Bool(true) => out.push(0),
            HeaderValue::Bool(false) => out.push(1),
            HeaderValue::I8(v) => {
                out.push(2);
                out.push(*v as u8);
            }
            HeaderValue::I16(v) => {
                out.push(3);
                out.extend_from_slice(&v.to_be_bytes());
            }
            HeaderValue::I32(v) => {
                out.push(4);
                out.extend_from_slice(&v.to_be_bytes());
            }
            HeaderValue::I64(v) => {
                out.push(5);
                out.extend_from_slice(&v.to_be_bytes());
            }
            HeaderValue::Bytes(b) => {
                out.push(6);
                out.extend_from_slice(&(b.len() as u16).to_be_bytes());
                out.extend_from_slice(b);
            }
            HeaderValue::String(s) => {
                out.push(7);
                out.extend_from_slice(&(s.len() as u16).to_be_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            HeaderValue::Timestamp(v) => {
                out.push(8);
                out.extend_from_slice(&v.to_be_bytes());
            }
            HeaderValue::Uuid(u) => {
                out.push(9);
                out.extend_from_slice(u);
            }
        }
        out
    }

    /// A whole message, CRCs included.
    pub(in crate::bedrock) fn encode(headers: &[(&str, HeaderValue)], payload: &[u8]) -> Vec<u8> {
        let header_block: Vec<u8> = headers
            .iter()
            .flat_map(|(n, v)| encode_header(n, v))
            .collect();
        let total = 16 + header_block.len() + payload.len();
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&(total as u32).to_be_bytes());
        out.extend_from_slice(&(header_block.len() as u32).to_be_bytes());
        let prelude_crc = crc32(&out);
        out.extend_from_slice(&prelude_crc.to_be_bytes());
        out.extend_from_slice(&header_block);
        out.extend_from_slice(payload);
        let message_crc = crc32(&out);
        out.extend_from_slice(&message_crc.to_be_bytes());
        out
    }

    /// An `event` message carrying `payload` as JSON text.
    pub(in crate::bedrock) fn event(event_type: &str, payload: &str) -> Vec<u8> {
        encode(
            &[
                (":message-type", HeaderValue::String("event".to_string())),
                (":event-type", HeaderValue::String(event_type.to_string())),
                (
                    ":content-type",
                    HeaderValue::String("application/json".to_string()),
                ),
            ],
            payload.as_bytes(),
        )
    }

    /// An `exception` message of `kind` carrying `payload` as JSON text.
    pub(in crate::bedrock) fn exception(kind: &str, payload: &str) -> Vec<u8> {
        encode(
            &[
                (
                    ":message-type",
                    HeaderValue::String("exception".to_string()),
                ),
                (":exception-type", HeaderValue::String(kind.to_string())),
                (
                    ":content-type",
                    HeaderValue::String("application/json".to_string()),
                ),
            ],
            payload.as_bytes(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{encode, event};
    use super::*;

    /// The sixteen-byte message with no headers and no payload, as the
    /// specification prints it. Anchors the CRC choice: an encoder and a
    /// decoder sharing a wrong CRC would agree with each other and disagree
    /// with AWS.
    const EMPTY_MESSAGE: [u8; 16] = [
        0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x05, 0xc2, 0x48, 0xeb, 0x7d, 0x98, 0xc8,
        0xff,
    ];

    #[test]
    fn the_specifications_empty_message_decodes() {
        let mut buf = EMPTY_MESSAGE.to_vec();
        let frame = decode(&mut buf, 1024).unwrap().unwrap();
        assert!(frame.headers.is_empty());
        assert!(frame.payload.is_empty());
        assert!(buf.is_empty());
        assert_eq!(encode(&[], b""), EMPTY_MESSAGE.to_vec());
    }

    #[test]
    fn every_header_type_round_trips() {
        let headers = vec![
            ("t", HeaderValue::Bool(true)),
            ("f", HeaderValue::Bool(false)),
            ("i8", HeaderValue::I8(-3)),
            ("i16", HeaderValue::I16(-300)),
            ("i32", HeaderValue::I32(70_000)),
            ("i64", HeaderValue::I64(-5_000_000_000)),
            ("bytes", HeaderValue::Bytes(vec![1, 2, 3])),
            ("s", HeaderValue::String("hello".to_string())),
            ("ts", HeaderValue::Timestamp(1_700_000_000_000)),
            ("id", HeaderValue::Uuid([7u8; 16])),
        ];
        let mut buf = encode(&headers, b"payload");
        let frame = decode(&mut buf, 1024).unwrap().unwrap();
        let expected: Vec<(String, HeaderValue)> = headers
            .into_iter()
            .map(|(n, v)| (n.to_string(), v))
            .collect();
        assert_eq!(frame.headers, expected);
        assert_eq!(frame.payload, b"payload");
        assert_eq!(frame.header_str("s"), Some("hello"));
        // Only a string header answers by name.
        assert_eq!(frame.header_str("i32"), None);
        assert_eq!(frame.header_str("missing"), None);
    }

    #[test]
    fn the_standard_headers_are_read_by_role() {
        let mut buf = event("contentBlockDelta", "{}");
        let frame = decode(&mut buf, 1024).unwrap().unwrap();
        assert_eq!(frame.message_type(), Some("event"));
        assert_eq!(frame.event_type(), Some("contentBlockDelta"));
        assert_eq!(frame.exception_type(), None);
        let mut buf = super::fixtures::exception("throttlingException", "{}");
        let frame = decode(&mut buf, 1024).unwrap().unwrap();
        assert_eq!(frame.message_type(), Some("exception"));
        assert_eq!(frame.exception_type(), Some("throttlingException"));
        assert_eq!(frame.event_type(), None);
    }

    #[test]
    fn a_frame_split_across_pushes_waits_for_the_rest() {
        let whole = event("messageStart", r#"{"role":"assistant"}"#);
        let mut buf = Vec::new();
        buf.extend_from_slice(&whole[..5]);
        assert_eq!(decode(&mut buf, 1024).unwrap(), None);
        buf.extend_from_slice(&whole[5..20]);
        assert_eq!(decode(&mut buf, 1024).unwrap(), None);
        buf.extend_from_slice(&whole[20..]);
        let frame = decode(&mut buf, 1024).unwrap().unwrap();
        assert_eq!(frame.payload, br#"{"role":"assistant"}"#);
        assert!(buf.is_empty());
    }

    #[test]
    fn two_frames_in_one_buffer_come_out_one_at_a_time() {
        let mut buf = event("a", "1");
        buf.extend(event("b", "2"));
        let first = decode(&mut buf, 1024).unwrap().unwrap();
        assert_eq!(first.event_type(), Some("a"));
        let second = decode(&mut buf, 1024).unwrap().unwrap();
        assert_eq!(second.event_type(), Some("b"));
        assert_eq!(decode(&mut buf, 1024).unwrap(), None);
    }

    #[test]
    fn a_corrupt_prelude_checksum_is_refused() {
        let mut buf = event("a", "1");
        buf[9] ^= 0xff;
        assert_eq!(decode(&mut buf, 1024), Err(FrameError::PreludeCrc));
    }

    #[test]
    fn a_corrupt_message_checksum_is_refused() {
        let mut buf = event("a", "1");
        let last = buf.len() - 1;
        buf[last] ^= 0xff;
        assert_eq!(decode(&mut buf, 1024), Err(FrameError::MessageCrc));
    }

    /// A prelude with the given lengths and a correct prelude CRC, so the
    /// length check is what fails.
    fn prelude(total: u32, headers: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(&headers.to_be_bytes());
        let crc = crc32(&out);
        out.extend_from_slice(&crc.to_be_bytes());
        out
    }

    #[test]
    fn impossible_lengths_are_refused() {
        assert_eq!(decode(&mut prelude(15, 0), 1024), Err(FrameError::Length));
        assert_eq!(decode(&mut prelude(20, 5), 1024), Err(FrameError::Length));
        assert_eq!(decode(&mut prelude(4096, 0), 1024), Err(FrameError::Length));
        // At the cap exactly is allowed; the bytes just have not arrived.
        assert_eq!(decode(&mut prelude(1024, 0), 1024), Ok(None));
    }

    #[test]
    fn an_unknown_header_type_is_refused() {
        // A header block of one header: name "x", type 42.
        let mut buf = encode(&[], b"");
        // Rebuild by hand: the encoder cannot write an unknown type.
        let block = [1u8, b'x', 42];
        let total = 16 + block.len();
        buf.clear();
        buf.extend_from_slice(&(total as u32).to_be_bytes());
        buf.extend_from_slice(&(block.len() as u32).to_be_bytes());
        let prelude_crc = crc32(&buf);
        buf.extend_from_slice(&prelude_crc.to_be_bytes());
        buf.extend_from_slice(&block);
        let message_crc = crc32(&buf);
        buf.extend_from_slice(&message_crc.to_be_bytes());
        assert_eq!(decode(&mut buf, 1024), Err(FrameError::HeaderType(42)));
    }

    /// A message whose header block is exactly `block`, CRCs correct.
    fn with_header_block(block: &[u8]) -> Vec<u8> {
        let total = 16 + block.len();
        let mut buf = Vec::new();
        buf.extend_from_slice(&(total as u32).to_be_bytes());
        buf.extend_from_slice(&(block.len() as u32).to_be_bytes());
        let prelude_crc = crc32(&buf);
        buf.extend_from_slice(&prelude_crc.to_be_bytes());
        buf.extend_from_slice(block);
        let message_crc = crc32(&buf);
        buf.extend_from_slice(&message_crc.to_be_bytes());
        buf
    }

    #[test]
    fn a_header_block_that_ends_mid_header_is_refused() {
        // Name length says 5 but only 2 bytes follow; then a header that ends
        // before its type byte, and one cut inside each value type.
        let torn: &[&[u8]] = &[
            &[5, b'a', b'b'],
            &[1, b'a'],
            &[1, b'a', 2],
            &[1, b'a', 3, 0],
            &[1, b'a', 4, 0],
            &[1, b'a', 5, 0],
            &[1, b'a', 6, 0],
            &[1, b'a', 6, 0, 5, 1],
            &[1, b'a', 7, 0],
            &[1, b'a', 7, 0, 9, b'x'],
            &[1, b'a', 8, 0],
            &[1, b'a', 9, 0],
        ];
        for block in torn {
            assert_eq!(
                decode(&mut with_header_block(block), 1024),
                Err(FrameError::Truncated),
                "{block:?}"
            );
        }
    }

    #[test]
    fn a_header_name_that_is_not_utf8_is_refused() {
        assert_eq!(
            decode(&mut with_header_block(&[1, 0xff, 0]), 1024),
            Err(FrameError::HeaderName)
        );
        // The same for a string value.
        assert_eq!(
            decode(&mut with_header_block(&[1, b'a', 7, 0, 1, 0xff]), 1024),
            Err(FrameError::HeaderName)
        );
    }

    #[test]
    fn fewer_than_twelve_bytes_is_not_yet_a_frame() {
        let mut buf = vec![0u8; 11];
        assert_eq!(decode(&mut buf, 1024), Ok(None));
        assert_eq!(buf.len(), 11);
    }

    #[test]
    fn every_error_has_a_sentence() {
        let errors = [
            FrameError::PreludeCrc,
            FrameError::MessageCrc,
            FrameError::Length,
            FrameError::HeaderType(3),
            FrameError::Truncated,
            FrameError::HeaderName,
        ];
        for e in errors {
            assert!(!e.to_string().is_empty());
        }
        assert!(FrameError::HeaderType(3).to_string().contains('3'));
    }
}

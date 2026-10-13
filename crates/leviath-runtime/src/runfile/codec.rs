//! The run file's byte layout.
//!
//! ```text
//! file   := MAGIC "LVR2" | fingerprint (32 bytes) | frame*
//! frame  := kind (u8) | len (u32 LE) | body (len bytes) | crc32 (u32 LE) | len (u32 LE)
//! body   := zstd(postcard(payload))
//! ```
//!
//! Every frame repeats its length after its checksum, so a reader can walk
//! the file backwards from its end: finding the last state checkpoint is a
//! few seeks, not a scan. The checksum covers the kind and the body, so a
//! frame torn by a crash mid-write is found and cut off rather than decoded.
//!
//! The fingerprint names the frame types' binary layout. A file written in
//! another layout is refused whole, by name, rather than decoded into the
//! wrong shape.

use std::fmt;
use std::io::Read as _;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// The file's first four bytes.
pub const MAGIC: &[u8; 4] = b"LVR2";

/// Bytes before the first frame.
pub const HEADER_LEN: usize = 4 + 32;

/// Bytes a frame adds around its body.
pub const FRAME_OVERHEAD: usize = 1 + 4 + 4 + 4;

/// The zstd level frames are written at: fast, and most of the win.
const ZSTD_LEVEL: i32 = 3;

/// What a frame holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    /// The run's [`RunSpec`](crate::spec::run_spec::RunSpec). Always first.
    Spec = 1,
    /// Code the run uses, by digest.
    Code = 2,
    /// A [`StateDelta`](crate::state::StateDelta).
    Delta = 4,
    /// A full [`RunState`](crate::state::RunState) checkpoint.
    State = 5,
    /// A change of which machine and daemon own the run.
    Owner = 6,
}

impl FrameKind {
    pub(crate) fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            1 => Self::Spec,
            2 => Self::Code,
            4 => Self::Delta,
            5 => Self::State,
            6 => Self::Owner,
            _ => return None,
        })
    }
}

/// Why bytes are not a readable run file or frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// The file does not start with `LVR2`.
    NotARunFile,
    /// The file was written by a build with different frame types.
    Fingerprint {
        /// The fingerprint in the file.
        found: String,
        /// This build's.
        expected: String,
    },
    /// A frame's checksum or length is wrong at this offset.
    Corrupt(u64),
    /// A whole frame names a kind this build does not know.
    UnknownKind {
        /// Where the frame starts.
        at: u64,
        /// Its kind byte.
        kind: u8,
    },
    /// A frame's payload does not decode as its kind's type.
    Decode(String),
    /// The frame at this offset decompresses to more than `limit` bytes.
    TooLarge {
        /// Where the frame starts.
        at: u64,
        /// The most a frame's payload may be.
        limit: u64,
    },
    /// A payload could not be encoded.
    Encode(String),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotARunFile => f.write_str("not a run file (no LVR2 header)"),
            Self::Fingerprint { found, expected } => write!(
                f,
                "this run file was written by a build with different run types \
                 (file {found}, this build {expected}); open it with the build that wrote it"
            ),
            Self::Corrupt(at) => write!(f, "run file is corrupt at byte {at}"),
            Self::UnknownKind { at, kind } => write!(
                f,
                "run file has a frame of unknown kind {kind} at byte {at}"
            ),
            Self::Decode(e) => write!(f, "run file frame does not decode: {e}"),
            Self::TooLarge { at, limit } => write!(
                f,
                "run file frame at byte {at} decompresses to more than {limit} bytes"
            ),
            Self::Encode(e) => write!(f, "run file frame does not encode: {e}"),
        }
    }
}

impl std::error::Error for CodecError {}

/// The header for a file whose frame types hash to `fingerprint`.
pub fn header(fingerprint: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(fingerprint);
    out
}

/// Check a file's header against this build's fingerprint.
pub fn check_header(bytes: &[u8], fingerprint: &[u8; 32]) -> Result<(), CodecError> {
    if bytes.len() < HEADER_LEN || !bytes.starts_with(MAGIC) {
        return Err(CodecError::NotARunFile);
    }
    let found = &bytes[4..HEADER_LEN];
    match found == fingerprint {
        true => Ok(()),
        false => Err(CodecError::Fingerprint {
            found: hex::encode(found),
            expected: hex::encode(fingerprint),
        }),
    }
}

/// Encode one frame.
pub fn encode<T: Serialize>(kind: FrameKind, payload: &T) -> Result<Vec<u8>, CodecError> {
    frame_of(kind, postcard::to_stdvec(payload), u32::MAX)
}

/// The frame around a payload's postcard bytes, whose compressed body is at
/// most `max_body` bytes, the most a frame's length field can say.
fn frame_of(
    kind: FrameKind,
    raw: Result<Vec<u8>, postcard::Error>,
    max_body: u32,
) -> Result<Vec<u8>, CodecError> {
    let raw = raw.map_err(|e| CodecError::Encode(e.to_string()))?;
    // Compressing bytes already in memory at a fixed, valid level only
    // fails when allocation does.
    let body = zstd::bulk::compress(&raw, ZSTD_LEVEL).expect("zstd compresses bytes in memory");
    let len = u32::try_from(body.len())
        .ok()
        .filter(|len| *len <= max_body)
        .ok_or_else(|| {
            CodecError::Encode(format!(
                "a frame body of {} bytes is over the {max_body}-byte limit",
                body.len()
            ))
        })?;
    let mut out = Vec::with_capacity(body.len() + FRAME_OVERHEAD);
    out.push(kind as u8);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body);
    let mut crc = crc32fast::Hasher::new();
    crc.update(&[kind as u8]);
    crc.update(&body);
    out.extend_from_slice(&crc.finalize().to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    Ok(out)
}

/// One frame found in a file: its kind, and where its body is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRef {
    /// What it holds.
    pub kind: FrameKind,
    /// Where the frame starts.
    pub offset: usize,
    /// Where its body starts.
    pub body: usize,
    /// Its body's length.
    pub len: usize,
}

impl FrameRef {
    /// Where the next frame starts.
    pub fn end(&self) -> usize {
        self.body + self.len + 8
    }

    /// Decode the frame's payload.
    pub fn decode<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<T, CodecError> {
        let body = bytes
            .get(self.body..self.body + self.len)
            .ok_or(CodecError::Corrupt(self.offset as u64))?;
        let raw = decompress(body, self.offset)?;
        postcard::from_bytes(&raw).map_err(|e| CodecError::Decode(e.to_string()))
    }
}

/// The most bytes one frame's payload decompresses to.
const MAX_PAYLOAD: u64 = 1 << 30;

/// The most a decode sets aside before it starts, whatever a body's header
/// claims. A header is only bytes in the file, so a small body may claim a
/// gigabyte; the buffer grows past this only as real output arrives.
const RESERVE: u64 = 16 << 20;

/// The body of the frame at `at`, decompressed: at most [`MAX_PAYLOAD`]
/// bytes, whatever its header says or leaves out.
fn decompress(body: &[u8], at: usize) -> Result<Vec<u8>, CodecError> {
    decompress_within(body, at, MAX_PAYLOAD, RESERVE)
}

/// [`decompress`], refusing more than `limit` bytes of output and setting
/// aside at most `reserve` up front.
///
/// The buffer starts at the size the zstd header gives, which every body
/// [`encode`] writes carries, so a small frame decodes into exactly the
/// room it needs. A header claiming more than `limit` is refused before
/// anything is decoded. A body that gives no size, or more output than its
/// header said (two zstd frames back to back), is stopped one byte past
/// `limit`.
fn decompress_within(
    body: &[u8],
    at: usize,
    limit: u64,
    reserve: u64,
) -> Result<Vec<u8>, CodecError> {
    let too_large = CodecError::TooLarge {
        at: at as u64,
        limit,
    };
    let declared = zstd::zstd_safe::get_frame_content_size(body).ok().flatten();
    if declared.is_some_and(|size| size > limit) {
        return Err(too_large);
    }
    let mut out = Vec::with_capacity(declared.unwrap_or(0).min(reserve) as usize);
    zstd::stream::read::Decoder::with_buffer(body)
        .and_then(|decoder| decoder.take(limit + 1).read_to_end(&mut out))
        .map_err(|e| CodecError::Decode(e.to_string()))?;
    match out.len() as u64 > limit {
        true => Err(too_large),
        false => Ok(out),
    }
}

fn u32_at(bytes: &[u8], at: usize) -> Option<usize> {
    let b = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
}

/// Read the frame that starts at `offset`, checking its checksum.
pub fn frame_at(bytes: &[u8], offset: usize) -> Result<FrameRef, CodecError> {
    let (kind_byte, len) = whole(bytes, offset).ok_or(CodecError::Corrupt(offset as u64))?;
    let kind = FrameKind::from_byte(kind_byte).ok_or(CodecError::UnknownKind {
        at: offset as u64,
        kind: kind_byte,
    })?;
    Ok(FrameRef {
        kind,
        offset,
        body: offset + 5,
        len,
    })
}

/// The kind byte and body length of a whole frame at `offset`, whatever its
/// kind: it fits in `bytes`, its trailing length repeats its length, and its
/// checksum holds. The checksum is worked out last, so a guess at a frame
/// that is not one costs no more than reading its lengths.
fn whole(bytes: &[u8], offset: usize) -> Option<(u8, usize)> {
    let kind = *bytes.get(offset)?;
    let len = u32_at(bytes, offset + 1)?;
    let body = offset + 5;
    let body_bytes = bytes.get(body..body + len)?;
    let crc = u32_at(bytes, body + len)?;
    if u32_at(bytes, body + len + 4)? != len {
        return None;
    }
    let mut h = crc32fast::Hasher::new();
    h.update(&[kind]);
    h.update(body_bytes);
    (h.finalize() as usize == crc).then_some((kind, len))
}

/// Where a walk over a file's frames stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// At the end of the file: everything after the header is whole frames.
    End,
    /// At a frame a crash left unfinished, with no whole frame after it. A
    /// writer appends one frame at a time, so the bytes from `at` on are its
    /// last write, torn, and cutting them loses nothing that was written
    /// whole.
    Torn {
        /// Where the torn frame starts.
        at: usize,
    },
    /// At a frame that does not check out, with a whole frame after it. A
    /// crash tears only the last write, so this is damage, and cutting the
    /// file here would lose every frame after it.
    Corrupt {
        /// Where the damaged frame starts.
        at: usize,
    },
    /// At a whole frame of a kind this build does not know.
    UnknownKind {
        /// Where the frame starts.
        at: usize,
        /// Its kind byte.
        kind: u8,
    },
}

impl Stop {
    /// What is wrong with the file when the walk stopped at damage. `None`
    /// at its end and at a torn last write, which a reader leaves out and
    /// only a writer cuts.
    pub fn damage(self) -> Option<CodecError> {
        match self {
            Self::End | Self::Torn { .. } => None,
            Self::Corrupt { at } => Some(CodecError::Corrupt(at as u64)),
            Self::UnknownKind { at, kind } => Some(CodecError::UnknownKind {
                at: at as u64,
                kind,
            }),
        }
    }
}

/// A file's frames, read forward from its header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    /// Every whole frame before the walk stopped, in order.
    pub frames: Vec<FrameRef>,
    /// Where those frames end.
    pub end: usize,
    /// Why the walk stopped there.
    pub stop: Stop,
}

/// Walk the frames after the header, stopping at the end or at the first
/// frame that does not read, and say which.
pub fn scan(bytes: &[u8]) -> Scan {
    let mut frames = Vec::new();
    let mut at = HEADER_LEN.min(bytes.len());
    let stop = loop {
        if at >= bytes.len() {
            break Stop::End;
        }
        match frame_at(bytes, at) {
            Ok(f) => {
                at = f.end();
                frames.push(f);
            }
            Err(CodecError::UnknownKind { kind, .. }) => break Stop::UnknownKind { at, kind },
            Err(_) => match whole_frame_after(bytes, at) {
                true => break Stop::Corrupt { at },
                false => break Stop::Torn { at },
            },
        }
    };
    Scan {
        frames,
        end: at,
        stop,
    }
}

/// Whether a whole frame, of any kind, starts anywhere after `at`.
///
/// Every offset is tried. A frame whose body was damaged still says how
/// long it is, so the next whole frame is found one frame along; a torn last
/// write is only as long as one frame.
fn whole_frame_after(bytes: &[u8], at: usize) -> bool {
    (at + 1..bytes.len()).any(|offset| whole(bytes, offset).is_some())
}

/// The frame that ends at `end`, read backwards from its trailing length.
pub fn frame_before(bytes: &[u8], end: usize) -> Result<FrameRef, CodecError> {
    let len = end
        .checked_sub(4)
        .and_then(|at| u32_at(bytes, at))
        .ok_or(CodecError::Corrupt(end as u64))?;
    let start = end
        .checked_sub(len + FRAME_OVERHEAD)
        .filter(|s| *s >= HEADER_LEN)
        .ok_or(CodecError::Corrupt(end as u64))?;
    frame_at(bytes, start)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: [u8; 32] = [7; 32];

    fn file(frames: &[(FrameKind, &str)]) -> Vec<u8> {
        let mut out = header(&FP);
        for (k, p) in frames {
            out.extend(encode(*k, &p.to_string()).unwrap());
        }
        out
    }

    #[test]
    fn frames_read_forward_and_backward() {
        let f = file(&[
            (FrameKind::Spec, "spec"),
            (FrameKind::Delta, "d1"),
            (FrameKind::State, "s1"),
            (FrameKind::Delta, "d2"),
        ]);
        check_header(&f, &FP).unwrap();
        let Scan {
            frames: all,
            end,
            stop,
        } = scan(&f);
        assert_eq!((end, stop), (f.len(), Stop::End));
        assert_eq!(stop.damage(), None);
        let kinds: Vec<FrameKind> = all.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                FrameKind::Spec,
                FrameKind::Delta,
                FrameKind::State,
                FrameKind::Delta
            ]
        );
        assert_eq!(all[0].decode::<String>(&f).unwrap(), "spec");
        assert_eq!(all[2].decode::<String>(&f).unwrap(), "s1");
        assert_eq!(
            frame_before(&f, end).unwrap().decode::<String>(&f).unwrap(),
            "d2"
        );
    }

    /// A last frame cut anywhere, or left as zeroes or junk, is a torn last
    /// write: the walk stops where it starts, and it is not damage.
    #[test]
    fn a_torn_tail_stops_the_walk_at_the_last_whole_frame() {
        let mut f = file(&[(FrameKind::Spec, "spec"), (FrameKind::Delta, "d1")]);
        let whole = f.len();
        let next = encode(FrameKind::Delta, &"a longer third payload".to_string()).unwrap();
        for cut in 1..next.len() {
            let mut torn = f.clone();
            torn.extend_from_slice(&next[..cut]);
            let s = scan(&torn);
            assert_eq!((s.frames.len(), s.end), (2, whole), "cut at {cut}");
            assert_eq!(s.stop, Stop::Torn { at: whole }, "cut at {cut}");
            assert_eq!(s.stop.damage(), None);
        }
        for tail in [&[0u8; 4096][..], b"torn"] {
            let mut torn = f.clone();
            torn.extend_from_slice(tail);
            assert_eq!(scan(&torn).stop, Stop::Torn { at: whole });
        }
        // The last frame whole but for one bad byte, with nothing after it.
        f.extend(next);
        let last = f.len() - 10;
        f[last] ^= 0xff;
        assert_eq!(scan(&f).stop, Stop::Torn { at: whole });
        assert_eq!(frame_at(&f, whole), Err(CodecError::Corrupt(whole as u64)));
    }

    /// A damaged frame with a whole frame anywhere after it is damage, not a
    /// tear: a crash tears only the last write. That holds when the damage
    /// is in the body, when it is in the length, and when the file also ends
    /// in a tear.
    #[test]
    fn a_damaged_frame_with_whole_frames_after_it_is_corrupt() {
        let f = file(&[
            (FrameKind::Spec, "spec"),
            (FrameKind::Delta, "d1"),
            (FrameKind::State, "s1"),
        ]);
        let d1 = frame_at(&f, frame_at(&f, HEADER_LEN).unwrap().end()).unwrap();
        let corrupt = Stop::Corrupt { at: d1.offset };
        for byte in [d1.body, d1.offset + 1, d1.offset] {
            let mut damaged = f.clone();
            damaged[byte] ^= 0x5a;
            let s = scan(&damaged);
            assert_eq!((s.frames.len(), s.end, s.stop), (1, d1.offset, corrupt));
            assert_eq!(s.stop.damage(), Some(CodecError::Corrupt(d1.offset as u64)));
            damaged.extend(b"torn");
            assert_eq!(scan(&damaged).stop, corrupt);
        }
    }

    /// A whole frame of a kind this build does not know stops the walk by
    /// name, in the middle of a file or at its end. After a damaged frame it
    /// still counts as whole, so the damage is not taken for a tear.
    #[test]
    fn an_unknown_kind_is_named_wherever_it_is() {
        let spec = file(&[(FrameKind::Spec, "spec")]);
        let at = spec.len();
        let mut f = spec.clone();
        f.extend(unknown_kind_frame());
        let unknown = Stop::UnknownKind { at, kind: 99 };
        assert_eq!(scan(&f).stop, unknown);
        f.extend(encode(FrameKind::Delta, &"d1".to_string()).unwrap());
        assert_eq!(scan(&f).stop, unknown);
        assert_eq!(
            unknown.damage(),
            Some(CodecError::UnknownKind {
                at: at as u64,
                kind: 99
            })
        );
        let mut damaged = spec;
        damaged.extend(encode(FrameKind::Delta, &"d1".to_string()).unwrap());
        damaged[at + 5] ^= 0xff;
        damaged.extend(unknown_kind_frame());
        assert_eq!(scan(&damaged).stop, Stop::Corrupt { at });
    }

    /// A whole frame whose kind byte is 99.
    fn unknown_kind_frame() -> Vec<u8> {
        let mut bad = encode(FrameKind::Spec, &"x".to_string()).unwrap();
        bad[0] = 99;
        let mut crc = crc32fast::Hasher::new();
        crc.update(&[99]);
        let len = bad.len() - FRAME_OVERHEAD;
        crc.update(&bad[5..5 + len]);
        bad[5 + len..9 + len].copy_from_slice(&crc.finalize().to_le_bytes());
        bad
    }

    /// A body is decompressed into a buffer the size its header gives, not
    /// one sized for the largest payload a frame may hold; a body whose
    /// header gives no size still decodes.
    #[test]
    fn a_body_decompresses_to_the_size_it_says() {
        let raw = postcard::to_stdvec(&"a payload".to_string()).unwrap();
        let sized = zstd::bulk::compress(&raw, ZSTD_LEVEL).unwrap();
        let out = decompress(&sized, 0).unwrap();
        assert_eq!(out, raw);
        assert_eq!(out.capacity(), raw.len());

        let streamed = zstd::stream::encode_all(raw.as_slice(), ZSTD_LEVEL).unwrap();
        // The probe's input gives no size.
        assert_eq!(
            zstd::zstd_safe::get_frame_content_size(&streamed).unwrap(),
            None
        );
        let mut frame = vec![FrameKind::Spec as u8];
        frame.extend_from_slice(&(streamed.len() as u32).to_le_bytes());
        frame.extend_from_slice(&streamed);
        let mut crc = crc32fast::Hasher::new();
        crc.update(&[FrameKind::Spec as u8]);
        crc.update(&streamed);
        frame.extend_from_slice(&crc.finalize().to_le_bytes());
        frame.extend_from_slice(&(streamed.len() as u32).to_le_bytes());
        let mut f = header(&FP);
        f.extend(frame);
        let read = frame_at(&f, HEADER_LEN).unwrap();
        assert_eq!(read.decode::<String>(&f).unwrap(), "a payload");
    }

    /// No body decompresses past the limit, whether its header says how big
    /// it is or not. A header claiming more is refused before decoding, and a
    /// stream that runs on is stopped one byte past the limit.
    #[test]
    fn no_body_decompresses_past_the_limit() {
        let raw = vec![7u8; 100];
        let sized = zstd::bulk::compress(&raw, ZSTD_LEVEL).unwrap();
        let streamed = zstd::stream::encode_all(raw.as_slice(), ZSTD_LEVEL).unwrap();
        let too_large = Err(CodecError::TooLarge { at: 40, limit: 99 });
        for body in [&sized, &streamed] {
            assert_eq!(decompress_within(body, 40, 100, 8).unwrap(), raw);
            assert_eq!(decompress_within(body, 40, 99, 8), too_large);
        }
        // Two zstd frames back to back: the header names the first one's
        // size, and the output runs on past it.
        let mut twice = sized.clone();
        twice.extend_from_slice(&sized);
        assert_eq!(
            decompress_within(&twice, 40, 150, 8).unwrap_err(),
            CodecError::TooLarge { at: 40, limit: 150 }
        );
        assert_eq!(decompress_within(&twice, 40, 200, 8).unwrap().len(), 200);
        assert_eq!(
            CodecError::TooLarge { at: 40, limit: 99 }.to_string(),
            "run file frame at byte 40 decompresses to more than 99 bytes"
        );
    }

    /// A frame whose payload is over the limit is refused when it is decoded.
    #[test]
    fn a_frame_over_the_limit_does_not_decode() {
        let f = file(&[(FrameKind::Spec, "spec")]);
        let frame = frame_at(&f, HEADER_LEN).unwrap();
        let body = &f[frame.body..frame.body + frame.len];
        assert_eq!(
            decompress_within(body, frame.offset, 1, RESERVE),
            Err(CodecError::TooLarge {
                at: HEADER_LEN as u64,
                limit: 1
            })
        );
    }

    /// A frame cut anywhere (in its kind, its body, its checksum or its
    /// trailing length) reads as corrupt where it starts.
    #[test]
    fn a_frame_cut_short_anywhere_is_corrupt_where_it_starts() {
        let f = file(&[(FrameKind::Spec, "a longer payload than most")]);
        let corrupt = Err(CodecError::Corrupt(HEADER_LEN as u64));
        for cut in [
            f.len() - 2,
            f.len() - 6,
            f.len() - 10,
            HEADER_LEN + 3,
            HEADER_LEN,
        ] {
            assert_eq!(frame_at(&f[..cut], HEADER_LEN), corrupt, "cut at {cut}");
        }
    }

    /// Walking backwards from a length that claims more than the file holds,
    /// or from a corrupt frame, is refused.
    #[test]
    fn a_backward_walk_through_a_bad_length_is_refused() {
        let mut f = file(&[(FrameKind::Spec, "spec")]);
        let end = f.len();
        f[end - 4..].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(frame_before(&f, end), Err(CodecError::Corrupt(end as u64)));
    }

    /// A payload that refuses to serialize, and a body over the limit, are
    /// encode errors.
    #[test]
    fn a_payload_that_cannot_be_written_is_an_encode_error() {
        struct Refuses;
        impl Serialize for Refuses {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("no"))
            }
        }
        assert_eq!(
            encode(FrameKind::Spec, &Refuses),
            Err(CodecError::Encode("Serde Serialization Error".into()))
        );
        let raw = postcard::to_stdvec(&"payload".to_string());
        let err = frame_of(FrameKind::Spec, raw, 4).unwrap_err();
        assert!(err.to_string().contains("over the 4-byte limit"));
    }

    /// A frame whose body is not where it says, or is not compressed, does
    /// not decode.
    #[test]
    fn a_frame_body_that_is_missing_or_not_compressed_does_not_decode() {
        let f = file(&[(FrameKind::Spec, "spec")]);
        let mut frame = scan(&f).frames[0];
        frame.len = f.len();
        assert_eq!(
            frame.decode::<String>(&f),
            Err(CodecError::Corrupt(HEADER_LEN as u64))
        );
        frame.body = 0;
        frame.len = 4;
        let err = frame.decode::<String>(&f).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("run file frame does not decode")
        );
    }

    #[test]
    fn a_foreign_file_or_build_is_refused_by_name() {
        assert_eq!(check_header(b"LVR1xxxx", &FP), Err(CodecError::NotARunFile));
        let err = check_header(&header(&[1; 32]), &FP).unwrap_err();
        assert!(err.to_string().contains("different run types"), "{err}");
        assert!(CodecError::NotARunFile.to_string().contains("LVR2"));
    }

    #[test]
    fn unknown_kinds_and_bad_payloads_are_named() {
        let mut f = file(&[(FrameKind::Spec, "spec")]);
        let at = f.len();
        f.extend(unknown_kind_frame());
        let unknown = CodecError::UnknownKind {
            at: at as u64,
            kind: 99,
        };
        assert_eq!(frame_at(&f, at), Err(unknown.clone()));
        assert_eq!(
            unknown.to_string(),
            format!("run file has a frame of unknown kind 99 at byte {at}")
        );
        let spec = scan(&f).frames[0];
        let err = spec.decode::<[u64; 8]>(&f).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("run file frame does not decode"),
            "{err}"
        );
        assert_eq!(
            CodecError::Corrupt(3).to_string(),
            "run file is corrupt at byte 3"
        );
        assert!(
            CodecError::Encode("e".into())
                .to_string()
                .contains("does not encode")
        );
        assert!(frame_before(&f, HEADER_LEN + 2).is_err());
        assert_eq!(frame_before(&f, 2), Err(CodecError::Corrupt(2)));
        let every = [
            FrameKind::Spec,
            FrameKind::Code,
            FrameKind::Delta,
            FrameKind::State,
            FrameKind::Owner,
        ];
        for k in every {
            assert_eq!(FrameKind::from_byte(k as u8), Some(k));
        }
    }
}

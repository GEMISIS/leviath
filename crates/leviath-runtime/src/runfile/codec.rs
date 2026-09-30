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
//! The fingerprint is a hash of the frame types' shapes. A file written by a
//! build whose types differ is refused whole, by name, rather than decoded
//! into the wrong shape.

use std::fmt;

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
    /// A stored part's bytes, by digest.
    Blob = 3,
    /// A [`StateDelta`](crate::state::StateDelta).
    Delta = 4,
    /// A full [`RunState`](crate::state::RunState) checkpoint.
    State = 5,
    /// A change of which machine and daemon own the run.
    Owner = 6,
}

impl FrameKind {
    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            1 => Self::Spec,
            2 => Self::Code,
            3 => Self::Blob,
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
    /// A frame names a kind this build does not know.
    UnknownKind(u8),
    /// A frame's payload does not decode as its kind's type.
    Decode(String),
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
                 (file {found}, this build {expected}); convert it or use the build that wrote it"
            ),
            Self::Corrupt(at) => write!(f, "run file is corrupt at byte {at}"),
            Self::UnknownKind(k) => write!(f, "run file has a frame of unknown kind {k}"),
            Self::Decode(e) => write!(f, "run file frame does not decode: {e}"),
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
    let raw = postcard::to_stdvec(payload).map_err(|e| CodecError::Encode(e.to_string()))?;
    let body =
        zstd::bulk::compress(&raw, ZSTD_LEVEL).map_err(|e| CodecError::Encode(e.to_string()))?;
    let len =
        u32::try_from(body.len()).map_err(|_| CodecError::Encode("frame over 4 GiB".into()))?;
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
        let raw =
            zstd::bulk::decompress(body, 1 << 30).map_err(|e| CodecError::Decode(e.to_string()))?;
        postcard::from_bytes(&raw).map_err(|e| CodecError::Decode(e.to_string()))
    }
}

fn u32_at(bytes: &[u8], at: usize) -> Option<usize> {
    let b: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(b) as usize)
}

/// Read the frame that starts at `offset`, checking its checksum.
pub fn frame_at(bytes: &[u8], offset: usize) -> Result<FrameRef, CodecError> {
    let corrupt = CodecError::Corrupt(offset as u64);
    let kind_byte = *bytes.get(offset).ok_or(corrupt.clone())?;
    let len = u32_at(bytes, offset + 1).ok_or(corrupt.clone())?;
    let body = offset + 5;
    let crc = u32_at(bytes, body + len).ok_or(corrupt.clone())?;
    let trailer = u32_at(bytes, body + len + 4).ok_or(corrupt.clone())?;
    let mut h = crc32fast::Hasher::new();
    h.update(&[kind_byte]);
    h.update(bytes.get(body..body + len).ok_or(corrupt.clone())?);
    if h.finalize() as usize != crc || trailer != len {
        return Err(corrupt);
    }
    let kind = FrameKind::from_byte(kind_byte).ok_or(CodecError::UnknownKind(kind_byte))?;
    Ok(FrameRef {
        kind,
        offset,
        body,
        len,
    })
}

/// Every whole frame after the header, in order, and the offset where the
/// good frames end. A torn or corrupt tail stops the walk; the caller cuts
/// the file back to that offset.
pub fn frames(bytes: &[u8]) -> (Vec<FrameRef>, usize) {
    let mut out = Vec::new();
    let mut at = HEADER_LEN.min(bytes.len());
    while at < bytes.len() {
        match frame_at(bytes, at) {
            Ok(f) => {
                at = f.end();
                out.push(f);
            }
            Err(_) => break,
        }
    }
    (out, at)
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

/// The last frame of `kind` in a file whose whole frames end at `end`,
/// found walking backwards.
pub fn last_of(bytes: &[u8], end: usize, kind: FrameKind) -> Result<Option<FrameRef>, CodecError> {
    let mut at = end;
    while at > HEADER_LEN {
        let f = frame_before(bytes, at)?;
        if f.kind == kind {
            return Ok(Some(f));
        }
        at = f.offset;
    }
    Ok(None)
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
        let (all, end) = frames(&f);
        assert_eq!(end, f.len());
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
        let state = last_of(&f, end, FrameKind::State).unwrap().unwrap();
        assert_eq!(state.decode::<String>(&f).unwrap(), "s1");
        assert_eq!(last_of(&f, end, FrameKind::Blob).unwrap(), None);
        assert_eq!(
            frame_before(&f, end).unwrap().decode::<String>(&f).unwrap(),
            "d2"
        );
    }

    #[test]
    fn a_torn_tail_stops_the_walk_at_the_last_whole_frame() {
        let mut f = file(&[(FrameKind::Spec, "spec"), (FrameKind::Delta, "d1")]);
        let whole = f.len();
        f.extend(encode(FrameKind::Delta, &"d2".to_string()).unwrap());
        f.truncate(f.len() - 3);
        let (all, end) = frames(&f);
        assert_eq!(all.len(), 2);
        assert_eq!(end, whole);
        let mut flipped = file(&[(FrameKind::Spec, "spec")]);
        let last = flipped.len() - 10;
        flipped[last] ^= 0xff;
        assert_eq!(frames(&flipped).0.len(), 0);
        assert!(matches!(
            frame_at(&flipped, HEADER_LEN),
            Err(CodecError::Corrupt(_))
        ));
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
        let mut bad = encode(FrameKind::Spec, &"x".to_string()).unwrap();
        bad[0] = 99;
        let mut crc = crc32fast::Hasher::new();
        crc.update(&[99]);
        let len = bad.len() - FRAME_OVERHEAD;
        crc.update(&bad[5..5 + len]);
        bad[5 + len..9 + len].copy_from_slice(&crc.finalize().to_le_bytes());
        let at = f.len();
        f.extend(bad);
        assert_eq!(frame_at(&f, at), Err(CodecError::UnknownKind(99)));
        assert_eq!(
            CodecError::UnknownKind(99).to_string(),
            "run file has a frame of unknown kind 99"
        );
        let spec = frames(&f).0[0];
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
        let every = [
            FrameKind::Spec,
            FrameKind::Code,
            FrameKind::Blob,
            FrameKind::Delta,
            FrameKind::State,
            FrameKind::Owner,
        ];
        for k in every {
            assert_eq!(FrameKind::from_byte(k as u8), Some(k));
        }
    }
}

//! The on-disk shape of `run.lvr`: magic, version, framing, and the reader
//! that tolerates what it does not understand.
//!
//! Separate from the rest of the archive module because it answers a different
//! question. Everything else decides what a record *means* - how a delta folds,
//! what a window looks like after it. This decides only where one record ends
//! and the next begins, which is the part that has to stay stable while the
//! record set keeps growing.

use std::io::{self, Read, Write};

use super::RunRecord;

/// File magic identifying a leviath run archive (`b"LVR1"`).
pub const RUN_ARCHIVE_MAGIC: &[u8; 4] = b"LVR1";

/// The archive format version this build writes.
///
/// Bump this only for a change to the *framing* - the preamble, the length
/// prefix, or the payload encoding. Adding a record kind is not that: frames
/// are length-prefixed and a reader skips a payload it cannot parse, so a new
/// kind is readable by an older build (it just does not know what it says).
/// Adding a field to an existing record is not that either, as long as the
/// field is `#[serde(default)]`.
///
/// What a bump means is that an older build cannot find the record boundaries
/// at all, which is why [`read_archive_start`] refuses a newer archive rather
/// than trying.
pub const RUN_ARCHIVE_VERSION: u16 = 1;

// ─── codec ──────────────────────────────────────────────────────────────────

/// Write the archive preamble (magic + version). Call once at file start.
pub fn write_archive_start(w: &mut dyn Write, version: u16) -> io::Result<()> {
    w.write_all(RUN_ARCHIVE_MAGIC)?;
    w.write_all(&version.to_be_bytes())?;
    Ok(())
}

/// Read + validate the archive preamble, returning the format version.
///
/// A version *newer* than this build understands is refused here rather than
/// read. The version marks a framing change (see [`RUN_ARCHIVE_VERSION`]), so a
/// newer archive is one whose record boundaries this build cannot find - and
/// reading it anyway would not fail cleanly, it would produce nonsense from
/// whatever the length prefixes happened to say. An older version is read
/// normally: framing has not changed under it, and unknown record kinds are
/// skipped rather than fatal.
pub fn read_archive_start(r: &mut dyn Read) -> io::Result<u16> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if &magic != RUN_ARCHIVE_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a leviath run archive (bad magic)",
        ));
    }
    let mut version = [0u8; 2];
    r.read_exact(&mut version)?;
    let version = u16::from_be_bytes(version);
    if version > RUN_ARCHIVE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "run archive is format version {version}, but this build reads up to \
                 {RUN_ARCHIVE_VERSION} - upgrade leviath to read it"
            ),
        ));
    }
    Ok(version)
}

/// Append one framed record. The frame length is a `u64` so it can never
/// overflow the prefix (a `RunRecord` always serializes to JSON).
pub fn write_record(w: &mut dyn Write, record: &RunRecord) -> io::Result<()> {
    let payload = serde_json::to_vec(record).expect("a RunRecord always serializes to JSON");
    let len = payload.len() as u64;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&payload)?;
    Ok(())
}

/// Fill `buf` from `r`, returning `false` on a clean end-of-stream (zero bytes
/// available at the call) and erroring only on a *partial* read (truncation).
fn read_exact_or_eof(r: &mut dyn Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => {
                if filled == 0 {
                    return Ok(false); // clean EOF at a record boundary
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated run-archive frame",
                ));
            }
            n => filled += n,
        }
    }
    Ok(true)
}

/// The largest a single archive frame may claim to be.
///
/// Generous by design - a record holds one context snapshot, and 256 MiB is far
/// past anything a real run writes - because this is a sanity bound on a length
/// prefix, not a size policy. What it rules out is a torn or corrupt prefix
/// being taken at its word and turned straight into an allocation.
const MAX_RECORD_BYTES: u64 = 256 * 1024 * 1024;

/// One frame off the wire: a record this build understands, or a complete
/// payload it does not.
///
/// The distinction is the whole point of the length prefix. A frame whose
/// *content* is unreadable - a record kind added by a later build - is a
/// well-formed frame whose bytes can be stepped over, and everything after it
/// is still readable. A frame that is *torn* cannot be stepped over, because
/// its length is unknown or its payload ran out, and the file ends there.
///
/// Conflating the two is what made adding a record kind a breaking change:
/// `read_archive_lenient` stopped at the first unknown record and returned the
/// prefix, silently dropping every readable record after it.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// A record this build knows how to read.
    Record(Box<RunRecord>),
    /// A complete frame whose payload this build cannot parse, stepped over.
    /// Carries its size so a caller can report what it skipped.
    Unreadable {
        /// Payload length in bytes.
        bytes: usize,
    },
}

/// Read the next frame, or `None` at a clean end-of-stream.
///
/// Errors only on a *torn* frame. An unparseable-but-complete payload comes
/// back as [`Frame::Unreadable`] with the stream positioned after it, so
/// reading can continue.
pub fn read_frame(r: &mut dyn Read) -> io::Result<Option<Frame>> {
    let Some(payload) = read_framed_payload(r)? else {
        return Ok(None);
    };
    match serde_json::from_slice(&payload) {
        Ok(record) => Ok(Some(Frame::Record(Box::new(record)))),
        // Deliberately not an error: the frame was intact and has been
        // consumed, so the only question is whether the caller wants to know.
        Err(_) => Ok(Some(Frame::Unreadable {
            bytes: payload.len(),
        })),
    }
}

/// Read the next framed record, or `None` at a clean end-of-stream.
///
/// Strict about content: a payload this build cannot parse is an error. Prefer
/// [`read_frame`] anywhere an archive written by a *different* build might be
/// read, which is every path that loads a run from disk.
pub fn read_record(r: &mut dyn Read) -> io::Result<Option<RunRecord>> {
    let Some(payload) = read_framed_payload(r)? else {
        return Ok(None);
    };
    let record = serde_json::from_slice(&payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(record))
}

/// Read one length-prefixed payload, or `None` at a clean end-of-stream.
///
/// The framing half of a frame read, shared so the strict and skipping readers
/// cannot disagree about where a record ends.
fn read_framed_payload(r: &mut dyn Read) -> io::Result<Option<Vec<u8>>> {
    let mut len_bytes = [0u8; 8];
    if !read_exact_or_eof(r, &mut len_bytes)? {
        return Ok(None);
    }
    let len = u64::from_be_bytes(len_bytes);
    // A torn tail is the reason `read_archive_lenient` exists, and a torn
    // *length prefix* is exactly where a nonsense `u64` comes from. Allocating
    // it first would abort the process on a crash-truncated archive - during
    // daemon recovery, which is the one moment the lenient reader is there to
    // survive. Rejecting it makes the frame an ordinary error, so recovery
    // folds back to the last intact record instead.
    if len > MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("run-archive frame claims {len} bytes, over the {MAX_RECORD_BYTES} cap"),
        ));
    }
    let mut payload = vec![0u8; len as usize];
    if !read_exact_or_eof(r, &mut payload)? {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated run-archive frame",
        ));
    }
    Ok(Some(payload))
}

/// Read the whole archive: validate the preamble, then read every record.
pub fn read_archive(r: &mut dyn Read) -> io::Result<(u16, Vec<RunRecord>)> {
    let version = read_archive_start(r)?;
    let mut records = Vec::new();
    while let Some(record) = read_record(r)? {
        records.push(record);
    }
    Ok((version, records))
}

/// Read the archive tolerantly: validate the preamble strictly, then read records
/// until a clean end-of-stream **or the first unreadable frame**, returning the
/// records collected so far.
///
/// A crash while the persistence lane is appending a record can leave a partial
/// final frame (a truncated length prefix or payload). The strict [`read_archive`]
/// would reject the whole file for that torn tail - and once a fallback-resume
/// appends fresh records *past* the torn bytes, the archive would stay unreadable
/// forever. This variant instead stops at the torn tail and keeps everything valid
/// before it, so recovery can still fold the archive to its last intact point. The
/// preamble is still validated strictly, so a file that isn't a run archive at all
/// still errors rather than folding to nothing.
pub fn read_archive_lenient(r: &mut dyn Read) -> io::Result<(u16, Vec<RunRecord>)> {
    let version = read_archive_start(r)?;
    let mut records = Vec::new();
    let mut skipped = 0usize;
    // A torn frame ends the read with whatever preceded it. A frame this build
    // simply does not understand is stepped over instead: it was written by a
    // later version, and everything after it is still ours to read.
    while let Ok(Some(frame)) = read_frame(r) {
        match frame {
            Frame::Record(record) => records.push(*record),
            Frame::Unreadable { .. } => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::debug!(
            skipped,
            "run archive holds record kinds this build does not know; skipped them"
        );
    }
    Ok((version, records))
}

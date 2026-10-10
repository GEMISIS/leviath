//! Reading an LVR1 journal's bytes into its records.
//!
//! The layout is the four bytes `LVR1`, a two-byte big-endian version, then
//! frames of an eight-byte big-endian length and a JSON record.

use super::JournalRecord;

/// The first bytes of an old journal.
pub const MAGIC: &[u8; 4] = b"LVR1";

/// The newest journal version there is.
const VERSION: u16 = 1;

/// The largest a frame may claim to be. A sanity bound on a length prefix,
/// so a torn one is never taken at its word and allocated.
const MAX_RECORD_BYTES: u64 = 256 * 1024 * 1024;

/// An old journal as read: its records, and what the read stepped over.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Records {
    /// Every record that read, in order.
    pub records: Vec<JournalRecord>,
    /// How many whole frames held JSON that is not a record this build
    /// knows, each stepped over.
    pub skipped: usize,
    /// How many bytes follow the last whole frame: a record a crash cut short
    /// while it was appended, left out.
    pub torn: usize,
}

/// Every record in `bytes`, an old journal.
///
/// The preamble is checked strictly. After it, a frame whose JSON is not a
/// record this build knows is stepped over, and a torn frame (a crash
/// mid-append) ends the read with the records before it.
pub fn read(bytes: &[u8]) -> Result<Vec<JournalRecord>, String> {
    read_counted(bytes).map(|read| read.records)
}

/// [`read`], counting the frames it stepped over and the torn bytes it left
/// out.
pub fn read_counted(bytes: &[u8]) -> Result<Records, String> {
    let Some((magic, rest)) = bytes.split_at_checked(MAGIC.len()) else {
        return Err("it is too short to be a journal".into());
    };
    if magic != MAGIC {
        return Err("it is not an LVR1 journal".into());
    }
    let Some((version, mut rest)) = rest.split_first_chunk::<2>() else {
        return Err("it ends before its version".into());
    };
    let version = u16::from_be_bytes(*version);
    if version > VERSION {
        return Err(format!(
            "it is journal version {version}, and this build reads up to {VERSION}"
        ));
    }
    let mut read = Records::default();
    while let Some((len, after)) = rest.split_first_chunk::<8>() {
        let len = u64::from_be_bytes(*len);
        let Some(payload) = (len <= MAX_RECORD_BYTES)
            .then_some(after)
            .and_then(|after| after.get(..len as usize))
        else {
            break;
        };
        match serde_json::from_slice::<JournalRecord>(payload) {
            Ok(record) => read.records.push(record),
            Err(_) => read.skipped += 1,
        }
        rest = &after[payload.len()..];
    }
    read.torn = rest.len();
    Ok(read)
}

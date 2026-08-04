//! The opaque pagination cursor every paginated route shares.
//!
//! **Keyset, not offset.** The collections being paged are live: runs are
//! created while a client walks the list, and finished ones can be deleted. An
//! offset into a shifting list silently skips and repeats items, and does it
//! most often at the head, which is exactly where a console looks. A cursor that
//! names *where you got to* rather than *how far in* cannot be shifted by an
//! insert or a delete elsewhere in the list.
//!
//! The key is a `(sort value, id)` pair. The id is the tie-break, and it has to
//! be there: sort values collide freely (two runs can start in the same second),
//! and a keyset walk over a non-total order loses whichever colliding item it
//! happened to resume past.
//!
//! Encoded as hex of a JSON payload. Hex rather than base64 because it is
//! URL-safe with no percent-encoding and both `hex` and `serde_json` are already
//! dependencies; base64 would buy ~30% shorter cursors and one more supply-chain
//! edge. It is opaque to clients - nothing may parse it - but a maintainer can
//! read one out of a bug report with `xxd`, which a compressed binary format
//! would not allow.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The sort value a cursor resumes from.
///
/// Externally tagged rather than `#[serde(untagged)]`: untagged tries each arm
/// in order and takes the first that parses, so a text key that happens to look
/// numeric would silently decode as an integer, and the fallthrough arm would be
/// unreachable in a way no test could distinguish.
/// Ordered so a keyset comparison is one tuple compare. A given route uses one
/// variant throughout, so the cross-variant ordering the derive produces is
/// never exercised against real data - it exists to make the derive total.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(super) enum CursorKey {
    /// A unix-seconds timestamp, for run listings.
    Int(i64),
    /// A name, for the blueprint catalog.
    Text(String),
}

/// Everything needed to resume a walk, and to prove the walk is the same one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Cursor {
    /// Payload version, so the encoding can change without a client - which
    /// must never parse this - noticing.
    pub(super) v: u8,
    /// The `sort` this cursor was minted for.
    pub(super) sort: String,
    /// The `order` this cursor was minted for.
    pub(super) order: String,
    /// First 8 hex chars of a digest over the filter params, see [`filter_digest`].
    pub(super) digest: String,
    /// The sort value of the last item on the previous page.
    pub(super) key: CursorKey,
    /// The id of the last item on the previous page - the tie-break.
    pub(super) id: String,
}

/// The current payload version.
const CURSOR_VERSION: u8 = 1;

/// Why a cursor could not be used.
///
/// A `PartialEq` enum rather than a bare string so tests can assert on the
/// specific failure with `assert_eq!`. `assert!(matches!(..))` would leave the
/// unmatched arms as regions no test enters, which the 100% coverage gate then
/// reports - and the workaround for that is usually to weaken the assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CursorError {
    /// Not valid hex - the client mangled it, or invented one.
    NotHex,
    /// Hex decoded, but the bytes are not the expected JSON payload.
    NotJson,
    /// Minted by a different (probably newer) version of this server.
    UnknownVersion(u8),
    /// Presented against a different `sort` than it was minted for.
    SortMismatch { minted: String, requested: String },
    /// Presented against a different `order` than it was minted for.
    OrderMismatch { minted: String, requested: String },
    /// Presented against a different filter set than it was minted for.
    FilterMismatch,
}

impl CursorError {
    /// A message naming what the client got wrong, for the 400 body.
    pub(super) fn message(&self) -> String {
        match self {
            CursorError::NotHex | CursorError::NotJson => {
                "Invalid cursor: pass back the `next_cursor` from the previous page unmodified"
                    .to_string()
            }
            CursorError::UnknownVersion(v) => {
                format!("Invalid cursor: unsupported cursor version {v}")
            }
            CursorError::SortMismatch { minted, requested } => format!(
                "Cursor was minted for sort={minted} but the request asks for sort={requested}; \
                 restart the walk from the first page"
            ),
            CursorError::OrderMismatch { minted, requested } => format!(
                "Cursor was minted for order={minted} but the request asks for order={requested}; \
                 restart the walk from the first page"
            ),
            CursorError::FilterMismatch => "Cursor was minted for a different set of filters; \
                 restart the walk from the first page"
                .to_string(),
        }
    }
}

/// Fold the filter params into a short digest a cursor can carry.
///
/// Changing a filter mid-walk cannot produce a meaningful continuation - the
/// cursor names a position in a list that no longer exists. Binding the filters
/// into the cursor turns that into a 400 the client can act on, instead of a
/// page of quietly wrong results. Callers pass the params in a fixed order, so
/// the digest is stable for a given filter set.
pub(super) fn filter_digest(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        // A separator, so `["ab", "c"]` and `["a", "bc"]` do not collide.
        hasher.update([0u8]);
    }
    hex::encode(hasher.finalize())
        .chars()
        .take(8)
        .collect::<String>()
}

/// Mint a cursor pointing just past `(key, id)`.
pub(super) fn encode(sort: &str, order: &str, digest: &str, key: CursorKey, id: &str) -> String {
    let cursor = Cursor {
        v: CURSOR_VERSION,
        sort: sort.to_string(),
        order: order.to_string(),
        digest: digest.to_string(),
        key,
        id: id.to_string(),
    };
    // Serializing a struct of owned primitives cannot fail; the fallback keeps
    // the signature infallible rather than making every caller handle a case
    // that has no way to happen.
    hex::encode(serde_json::to_vec(&cursor).unwrap_or_default())
}

/// Read a cursor back, checking it belongs to the walk being asked for.
pub(super) fn decode(
    raw: &str,
    sort: &str,
    order: &str,
    digest: &str,
) -> Result<Cursor, CursorError> {
    let bytes = hex::decode(raw).map_err(|_| CursorError::NotHex)?;
    let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| CursorError::NotJson)?;
    if cursor.v != CURSOR_VERSION {
        return Err(CursorError::UnknownVersion(cursor.v));
    }
    if cursor.sort != sort {
        return Err(CursorError::SortMismatch {
            minted: cursor.sort,
            requested: sort.to_string(),
        });
    }
    if cursor.order != order {
        return Err(CursorError::OrderMismatch {
            minted: cursor.order,
            requested: order.to_string(),
        });
    }
    if cursor.digest != digest {
        return Err(CursorError::FilterMismatch);
    }
    Ok(cursor)
}

impl Cursor {
    /// Does `(key, id)` fall strictly after this cursor's position, walking in
    /// `descending` order?
    ///
    /// The tie-break follows the primary direction, so this is one tuple
    /// comparison rather than a special case per direction.
    pub(super) fn precedes(&self, key: &CursorKey, id: &str, descending: bool) -> bool {
        let here = (&self.key, self.id.as_str());
        let there = (key, id);
        if descending {
            there < here
        } else {
            there > here
        }
    }
}

#[cfg(test)]
#[path = "cursor_tests.rs"]
mod tests;

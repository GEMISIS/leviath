//! Content-addressed identity for a context window, and for one region's
//! contents.
//!
//! A window is named at two moments that have to agree. The runtime names the
//! window before and after each change it records, as the change is made, and a
//! reader asks for one of those names much later. Both compute the value from
//! the same facts through this module, so the name a transaction wrote is the
//! name the snapshot holding that content answers to.
//!
//! That agreement is the whole guarantee: a revision is derived from content, so
//! it names one window forever. Nothing can be written that changes what an
//! existing revision means, and a reader asking for one either gets exactly that
//! content or is told the run never held it. A live window's revision moves as
//! the run writes, but it moves by becoming a *different* revision - the old one
//! keeps naming the content it always named.
//!
//! What a revision covers is the window's contents and the budgets it holds them
//! under: every region, in order, with its name, kind, budget and token count,
//! and a digest of its entries. The stage the run was in is deliberately not
//! part of it. A window does not know its stage name at the point a change is
//! recorded, and two stages carrying the same regions carry the same window; the
//! stage is recorded beside a snapshot rather than inside its identity.

use sha2::{Digest, Sha256};

use super::{ContextSnapshot, RegionEntrySnapshot, RegionSnapshot};

/// How many hex characters of the digest an identity carries.
///
/// 128 bits. An identity is a name, not a signature: what it has to do is never
/// collide across the windows one machine's runs hold, and the shorter form
/// keeps a transaction record - which carries four of these - small enough to
/// write on every change a busy run makes.
const DIGEST_HEX: usize = 32;

/// The fields one entry's fingerprint is taken over.
///
/// A borrowed view rather than either concrete entry type, so a live region and
/// a snapshotted one produce the same fingerprint from the same facts. Two
/// spellings of the rule would drift, and the first person to notice would be
/// someone whose revision no longer resolved.
///
/// An entry's timestamp is absent on purpose: a snapshot does not carry it, so a
/// fingerprint that used it could never be recomputed from one.
pub struct EntryFacts<'a> {
    /// The entry's parts, and the text they read as.
    pub content: &'a crate::region::EntryContent,
    /// Its token cost as counted when it was written.
    pub tokens: usize,
    /// What kind of entry it is.
    pub kind: &'a crate::region::EntryKind,
    /// Whatever structured data its writer attached.
    pub metadata: Option<&'a serde_json::Value>,
    /// Its key, in a region that keys its entries.
    pub key: Option<&'a str>,
    /// How sensitive it is.
    pub taint: crate::taint::TaintLevel,
    /// The opaque provider token this turn must be replayed with.
    pub reasoning: Option<&'a str>,
}

impl<'a> From<&'a RegionEntrySnapshot> for EntryFacts<'a> {
    fn from(entry: &'a RegionEntrySnapshot) -> Self {
        Self {
            content: &entry.content,
            tokens: entry.tokens,
            kind: &entry.kind,
            metadata: entry.metadata.as_ref(),
            key: entry.key.as_deref(),
            taint: entry.taint,
            reasoning: entry.reasoning.as_deref(),
        }
    }
}

/// The facts one region contributes to a window's revision.
pub struct RegionFacts<'a> {
    /// The region's name, which is how a write finds it.
    pub name: &'a str,
    /// Its kind, in the one word a blueprint spells it with.
    pub kind: &'a str,
    /// What it holds, in tokens.
    pub current_tokens: usize,
    /// What it is allowed to hold.
    pub max_tokens: usize,
    /// The identity of its contents, from [`region_digest`].
    pub digest: &'a str,
}

/// Feed one length-prefixed field into a digest.
///
/// The length matters: without it `("ab", "c")` and `("a", "bc")` hash alike,
/// and two windows differing only in where one region's name ended would share
/// a revision.
fn field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

/// Feed one number in.
fn number(hasher: &mut Sha256, value: u64) {
    hasher.update(value.to_be_bytes());
}

/// The leading [`DIGEST_HEX`] hex characters of a finished digest.
fn short(hasher: Sha256) -> String {
    let full = hasher.finalize();
    full.iter()
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .take(DIGEST_HEX)
        .map(|nibble| char::from_digit(u32::from(nibble), 16).unwrap_or('0'))
        .collect()
}

/// Serialize one small value canonically, for a field that is not already
/// bytes.
///
/// `serde_json` orders a map's keys, so the same value always produces the same
/// text. Every value fed through here is a tiny enum or a fragment of metadata,
/// never an entry body.
fn canonical<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// Fold one entry into a digest.
fn fingerprint(hasher: &mut Sha256, entry: &EntryFacts<'_>) {
    field(hasher, canonical(entry.content).as_bytes());
    number(hasher, entry.tokens as u64);
    field(hasher, canonical(entry.kind).as_bytes());
    field(hasher, canonical(&entry.metadata).as_bytes());
    field(hasher, canonical(&entry.key).as_bytes());
    field(hasher, canonical(&entry.taint).as_bytes());
    field(hasher, canonical(&entry.reasoning).as_bytes());
}

/// The identity of one region's contents.
///
/// Over the entries alone. A region whose budget was raised still holds what it
/// held, and a transaction records the budget and the token count beside this.
pub fn region_digest<'a>(entries: impl IntoIterator<Item = EntryFacts<'a>>) -> String {
    let mut hasher = Sha256::new();
    for entry in entries {
        fingerprint(&mut hasher, &entry);
    }
    format!("rg1-{}", short(hasher))
}

/// The identity of one snapshotted region's contents.
pub fn snapshot_region_digest(region: &RegionSnapshot) -> String {
    region_digest(region.entries.iter().map(EntryFacts::from))
}

/// A window's revision, from what it holds and the budget it holds it under.
///
/// `regions` must arrive in layout order, which is the order a snapshot lists
/// them in: the order is part of what the window is, because it is the order the
/// prompt is assembled in.
pub fn window_revision<'a>(
    total_tokens: usize,
    max_tokens: usize,
    regions: impl IntoIterator<Item = RegionFacts<'a>>,
) -> String {
    let mut hasher = Sha256::new();
    number(&mut hasher, total_tokens as u64);
    number(&mut hasher, max_tokens as u64);
    for region in regions {
        field(&mut hasher, region.name.as_bytes());
        field(&mut hasher, region.kind.as_bytes());
        number(&mut hasher, region.current_tokens as u64);
        number(&mut hasher, region.max_tokens as u64);
        field(&mut hasher, region.digest.as_bytes());
    }
    format!("cw1-{}", short(hasher))
}

/// The revision of a snapshotted window.
pub fn context_revision(snapshot: &ContextSnapshot) -> String {
    let digests: Vec<String> = snapshot
        .regions
        .iter()
        .map(snapshot_region_digest)
        .collect();
    window_revision(
        snapshot.total_tokens,
        snapshot.max_tokens,
        snapshot
            .regions
            .iter()
            .zip(&digests)
            .map(|(region, digest)| RegionFacts {
                name: &region.name,
                kind: &region.kind,
                current_tokens: region.current_tokens,
                max_tokens: region.max_tokens,
                digest,
            }),
    )
}

#[cfg(test)]
#[path = "revision_tests.rs"]
mod tests;

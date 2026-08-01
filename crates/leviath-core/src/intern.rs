//! Content interning for region entry text.
//!
//! [`ContentInterner`] is a **shareable handle** (`Clone` is cheap and shares the
//! table). The runtime inserts one as a Bevy `Resource` and clones that handle
//! into every [`crate::region`]-backing context window so agents in the same
//! World deduplicate identical entry text.
//!
//! This module has no process-global state: two independently constructed
//! interners never share allocations. That keeps tests isolated and matches
//! multi-World / multi-daemon deployment.

use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::{Arc, Mutex, Weak};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Handle to a content-addressed string table.
///
/// `Clone` shares the underlying table (via `Arc`). Construct once per ECS
/// World (as a resource) and clone into each agent context.
#[derive(Clone, Default)]
pub struct ContentInterner {
    inner: Arc<Mutex<InternerState>>,
}

#[derive(Default)]
struct InternerState {
    /// Hash → weak handles still (or recently) live under that hash.
    buckets: HashMap<u64, Vec<Weak<str>>>,
}

impl ContentInterner {
    /// Empty interner with its own private table.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(InternerState::default())),
        }
    }

    /// Intern `s`, reusing an existing allocation when this interner already
    /// holds identical text.
    pub fn intern(&self, s: impl AsRef<str>) -> InternedString {
        let s = s.as_ref();
        let hash = {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            s.hash(&mut hasher);
            hasher.finish()
        };

        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let bucket = guard.buckets.entry(hash).or_default();
        bucket.retain(|w| w.strong_count() > 0);

        for weak in bucket.iter() {
            if let Some(arc) = weak.upgrade()
                && arc.as_ref() == s
            {
                return InternedString(arc);
            }
        }

        let arc: Arc<str> = Arc::from(s);
        bucket.push(Arc::downgrade(&arc));
        InternedString(arc)
    }

    /// True when `self` and `other` share the same underlying table.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl fmt::Debug for ContentInterner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ContentInterner(..)")
    }
}

/// Interned, reference-counted entry text.
///
/// Opaque outside this module's constructors. Callers read plain `&str` via
/// [`crate::region::RegionEntry::content`].
#[derive(Clone, Eq)]
pub struct InternedString(Arc<str>);

impl InternedString {
    /// Allocate without consulting any interner table (no cross-entry sharing).
    #[inline]
    pub fn unique(s: impl AsRef<str>) -> Self {
        Self(Arc::from(s.as_ref()))
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[inline]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl PartialEq for InternedString {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.0.as_ref() == other.0.as_ref()
    }
}

impl PartialEq<str> for InternedString {
    fn eq(&self, other: &str) -> bool {
        self.0.as_ref() == other
    }
}

impl PartialEq<&str> for InternedString {
    fn eq(&self, other: &&str) -> bool {
        self.0.as_ref() == *other
    }
}

impl PartialEq<String> for InternedString {
    fn eq(&self, other: &String) -> bool {
        self.0.as_ref() == other.as_str()
    }
}

impl Hash for InternedString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.as_ref().hash(state);
    }
}

impl Deref for InternedString {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for InternedString {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for InternedString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.0.as_ref(), f)
    }
}

impl fmt::Display for InternedString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for InternedString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for InternedString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Deserialization has no interner in scope (snapshots are pure data).
        // Use a unique Arc; restore paths re-intern via [`ContentInterner`].
        let s = String::deserialize(deserializer)?;
        Ok(InternedString(Arc::from(s.as_str())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_interner_shares_allocation() {
        let i = ContentInterner::new();
        let a = i.intern("pinned");
        let b = i.intern("pinned");
        assert!(a.ptr_eq(&b));
    }

    #[test]
    fn distinct_interners_do_not_share() {
        let a = ContentInterner::new().intern("pinned");
        let b = ContentInterner::new().intern("pinned");
        assert!(!a.ptr_eq(&b));
        assert_eq!(a.as_str(), b.as_str());
    }

    #[test]
    fn clone_handle_shares_table() {
        let i1 = ContentInterner::new();
        let i2 = i1.clone();
        assert!(i1.ptr_eq(&i2));
        let a = i1.intern("x");
        let b = i2.intern("x");
        assert!(a.ptr_eq(&b));
    }

    #[test]
    fn unique_never_shares_with_interned() {
        let i = ContentInterner::new();
        let a = InternedString::unique("same");
        let b = i.intern("same");
        assert!(!a.ptr_eq(&b));
        assert_eq!(a.as_str(), b.as_str());
    }

    #[test]
    fn serde_roundtrip_preserves_text() {
        let i = ContentInterner::new();
        let a = i.intern("hello");
        let json = serde_json::to_string(&a).unwrap();
        let b: InternedString = serde_json::from_str(&json).unwrap();
        assert_eq!(a.as_str(), b.as_str());
        // deserialize has no interner — unique Arc
        assert!(!a.ptr_eq(&b));
    }
}

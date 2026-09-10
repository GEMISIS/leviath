//! A registry that can be swapped under whoever holds it.
//!
//! A run's registry is built once at spawn: the operator's rows, then the
//! blueprint's own. The tool lane, the inference dispatcher and the message
//! path each hold on to it for the life of the run. When `mime_types.toml`
//! is edited while the run is live, all of them should see the new rows,
//! and none of them can be reached from the reload to be handed a new
//! value. So they hold a cell instead: the reload stores a new registry into
//! it, and every reader's next `load` is the new one. A reader clones the
//! `Arc` out and works on a snapshot, so a swap never changes a registry
//! part-way through one operation.

use std::sync::{Arc, Mutex};

use super::MimeRegistry;

/// A shared, swappable [`MimeRegistry`].
#[derive(Debug)]
pub struct RegistryCell {
    inner: Mutex<Arc<MimeRegistry>>,
}

impl RegistryCell {
    /// A cell holding `registry`.
    pub fn new(registry: Arc<MimeRegistry>) -> Self {
        Self {
            inner: Mutex::new(registry),
        }
    }

    /// The registry as it stands now. A snapshot: a later
    /// [`store`](Self::store) does not change what this returned.
    pub fn load(&self) -> Arc<MimeRegistry> {
        crate::sync::lock(&self.inner).clone()
    }

    /// Replace the registry every later [`load`](Self::load) returns.
    pub fn store(&self, registry: Arc<MimeRegistry>) {
        *crate::sync::lock(&self.inner) = registry;
    }
}

impl Default for RegistryCell {
    fn default() -> Self {
        Self::new(Arc::new(MimeRegistry::builtin()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mime::MimeType;

    #[test]
    fn a_store_reaches_the_next_load_but_not_a_snapshot_already_taken() {
        let cell = RegistryCell::default();
        let before = cell.load();
        let obj = MimeType::parse("model/obj").unwrap();
        assert_eq!(before.info(&obj).family, "model");

        let table: toml::Table = toml::from_str("[\"model/obj\"]\nfamily = \"scene\"\n").unwrap();
        let mut next = MimeRegistry::builtin();
        next.layer(&table, "edit").unwrap();
        cell.store(Arc::new(next));

        assert_eq!(
            cell.load().info(&obj).family,
            "scene",
            "the next load sees the edit"
        );
        assert_eq!(before.info(&obj).family, "model", "the snapshot does not");
        assert!(format!("{cell:?}").contains("RegistryCell"));
    }
}

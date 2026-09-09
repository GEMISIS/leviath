//! A registry that can be swapped under whoever holds it.
//!
//! A run's registry is built once at spawn: the operator's rows, then the
//! blueprint's own. The tool lane, the inference dispatcher and the message
//! path each hold on to it for the life of the run. When `media_types.toml`
//! is edited while the run is live, all of them should see the new rows,
//! and none of them can be reached from the reload to be handed a new
//! value. So they hold a cell instead: the reload stores a new registry into
//! it, and every reader's next `load` is the new one. A reader clones the
//! `Arc` out and works on a snapshot, so a swap never changes a registry
//! part-way through one operation.

use std::sync::{Arc, Mutex};

use super::MediaRegistry;

/// A shared, swappable [`MediaRegistry`].
#[derive(Debug)]
pub struct RegistryCell {
    inner: Mutex<Arc<MediaRegistry>>,
}

impl RegistryCell {
    /// A cell holding `registry`.
    pub fn new(registry: Arc<MediaRegistry>) -> Self {
        Self {
            inner: Mutex::new(registry),
        }
    }

    /// The registry as it stands now. A snapshot: a later
    /// [`store`](Self::store) does not change what this returned.
    pub fn load(&self) -> Arc<MediaRegistry> {
        crate::sync::lock(&self.inner).clone()
    }

    /// Replace the registry every later [`load`](Self::load) returns.
    pub fn store(&self, registry: Arc<MediaRegistry>) {
        *crate::sync::lock(&self.inner) = registry;
    }
}

impl Default for RegistryCell {
    fn default() -> Self {
        Self::new(Arc::new(MediaRegistry::builtin()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaType;

    #[test]
    fn a_store_reaches_the_next_load_but_not_a_snapshot_already_taken() {
        let cell = RegistryCell::default();
        let before = cell.load();
        let obj = MediaType::parse("model/obj").unwrap();
        assert_eq!(before.info(&obj).family, "model");

        let table: toml::Table = toml::from_str("[\"model/obj\"]\nfamily = \"scene\"\n").unwrap();
        let mut next = MediaRegistry::builtin();
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

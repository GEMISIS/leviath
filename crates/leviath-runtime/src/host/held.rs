//! Runs held back from a restart until the model lists they need are read.
//!
//! A daemon started while a gateway was unreachable, with no copy of that
//! gateway's model list on disk, cannot tell which models the gateway serves.
//! A run resumed then would have its stages refused (`resolve_stages` will not
//! guess), and a resume that fails marks the run crashed for good - so a
//! proxy that was down for a minute at the wrong moment would end every run
//! in flight. Instead the run is held: listed like a parked run, with its full
//! state untouched on disk, and paged back in the moment the lists it was
//! waiting on have been read.

use super::{RunListEntry, WorldHost};

impl WorldHost {
    /// Hold `run_id` until none of `awaiting` (provider names whose model list
    /// is unread) is unread any more, listing it as `entry` meanwhile.
    ///
    /// Nothing is loaded into the world, so the run holds no slot and spends
    /// nothing while it waits. `Resume`, `Message` and `Cancel` reach it the
    /// way they reach any parked run.
    pub fn hold_for_catalog(&mut self, run_id: String, entry: RunListEntry, awaiting: Vec<String>) {
        self.parked.insert(run_id.clone(), entry);
        self.held_for_catalog.insert(run_id, awaiting);
    }

    /// A handle that wakes the serve loop, for whatever reads the model lists
    /// held runs wait on: notified once a list comes back, the loop pages the
    /// waiting runs in on its next pass rather than at the next re-drive.
    pub fn catalog_waker(&self) -> std::sync::Arc<tokio::sync::Notify> {
        self.world.wake_handle()
    }

    /// The runs held for a model list, sorted: for logs and tests.
    pub fn held_for_catalog(&self) -> Vec<String> {
        let mut held: Vec<String> = self.held_for_catalog.keys().cloned().collect();
        held.sort();
        held
    }

    /// Page back in every held run whose lists have all been read.
    ///
    /// Called on every pass of the serve loop. With nothing held it is one map
    /// check. A run that comes back resumes where it stopped, as it would have
    /// had the lists been there at start. One that cannot be paged in even
    /// now is left parked, so `lev resume` can try again, and the log says
    /// so.
    pub(super) fn retry_held(&mut self) {
        if self.held_for_catalog.is_empty() {
            return;
        }
        let unread = self
            .world
            .world()
            .get_resource::<crate::pipeline::Providers>()
            .map(|providers| providers.0.unread_catalogs())
            .unwrap_or_default();
        let mut ready: Vec<String> = self
            .held_for_catalog
            .iter()
            .filter(|(_, awaiting)| !awaiting.iter().any(|p| unread.contains(p)))
            .map(|(run_id, _)| run_id.clone())
            .collect();
        ready.sort();
        for run_id in ready {
            self.held_for_catalog.remove(&run_id);
            match self.resolve_or_reload(&run_id) {
                Some(_) => tracing::info!(
                    run_id = %run_id,
                    "resumed: the model list this run was waiting on has been read"
                ),
                None => tracing::warn!(
                    run_id = %run_id,
                    "the model list this run was waiting on has been read, but the run \
                     could not be paged back in; `lev resume` tries again"
                ),
            }
        }
    }
}

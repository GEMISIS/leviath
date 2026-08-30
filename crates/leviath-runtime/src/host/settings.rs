//! The host's own `[limits]` settings, in a handle that can be shared.
//!
//! Three of the numbers [`WorldHost`] runs on come from `config.toml` and are
//! not world resources: how many dead cycles buy the tool lane some relief, how
//! long a finished run stays in the listing, and the spend figures worth an
//! event. They used to be plain fields, which meant the only way to change one
//! was to restart the daemon - the host is reachable from its own serve loop
//! and nowhere else, while a config reload happens on the spawn path, which is
//! handed the world and not the host.
//!
//! Putting them behind a shared handle is what closes that gap. The host reads
//! through it, a clone of it goes to whatever watches `config.toml`, and a
//! change lands without either side having to reach the other.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::{DEFAULT_DEAD_CYCLES_BEFORE_RELIEF, DEFAULT_FINISHED_RETENTION_SECS};

/// The host settings an operator can change while the daemon runs. Cheap to
/// clone: every clone reads and writes the same values.
#[derive(Clone, Debug)]
pub struct HostSettings(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    dead_cycles_before_relief: AtomicU32,
    finished_retention_secs: AtomicU64,
    /// Behind an `Arc` inside the lock so a reader takes a cheap snapshot
    /// rather than holding the lock across the events it emits.
    spend_notify_usd: Mutex<Arc<Vec<f64>>>,
}

impl Default for HostSettings {
    fn default() -> Self {
        Self(Arc::new(Inner {
            dead_cycles_before_relief: AtomicU32::new(DEFAULT_DEAD_CYCLES_BEFORE_RELIEF),
            finished_retention_secs: AtomicU64::new(DEFAULT_FINISHED_RETENTION_SECS),
            spend_notify_usd: Mutex::new(Arc::new(Vec::new())),
        }))
    }
}

impl HostSettings {
    /// Dead cycles tolerated before the tool lane is widened.
    pub fn dead_cycles_before_relief(&self) -> u32 {
        self.0.dead_cycles_before_relief.load(Ordering::Relaxed)
    }

    /// Set that. Read once per safety re-drive, so it applies from the next one.
    pub fn set_dead_cycles_before_relief(&self, cycles: u32) {
        self.0
            .dead_cycles_before_relief
            .store(cycles, Ordering::Relaxed);
    }

    /// How long an unloaded run stays in the listing.
    pub fn finished_retention_secs(&self) -> u64 {
        self.0.finished_retention_secs.load(Ordering::Relaxed)
    }

    /// Set that. Applied on the next prune, so shortening the window drops the
    /// rows that have already outlived it.
    pub fn set_finished_retention_secs(&self, secs: u64) {
        self.0
            .finished_retention_secs
            .store(secs, Ordering::Relaxed);
    }

    /// The spend thresholds to announce, ascending.
    pub fn spend_notify_usd(&self) -> Arc<Vec<f64>> {
        leviath_core::sync::lock(&self.0.spend_notify_usd).clone()
    }

    /// Set them, in any order. Sorted and de-duplicated here so a caller can
    /// pass a config list as written; figures that are not a positive finite
    /// number are dropped.
    pub fn set_spend_notify_usd(&self, mut thresholds: Vec<f64>) {
        thresholds.retain(|t| t.is_finite() && *t > 0.0);
        thresholds.sort_by(|a, b| a.partial_cmp(b).expect("finite, filtered above"));
        thresholds.dedup();
        *leviath_core::sync::lock(&self.0.spend_notify_usd) = Arc::new(thresholds);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_reads_what_the_original_was_told() {
        let settings = HostSettings::default();
        assert_eq!(
            settings.dead_cycles_before_relief(),
            DEFAULT_DEAD_CYCLES_BEFORE_RELIEF
        );
        assert_eq!(
            settings.finished_retention_secs(),
            DEFAULT_FINISHED_RETENTION_SECS
        );
        assert!(settings.spend_notify_usd().is_empty());

        let handed_out = settings.clone();
        settings.set_dead_cycles_before_relief(2);
        settings.set_finished_retention_secs(30);
        settings.set_spend_notify_usd(vec![5.0]);

        assert_eq!(handed_out.dead_cycles_before_relief(), 2);
        assert_eq!(handed_out.finished_retention_secs(), 30);
        assert_eq!(*handed_out.spend_notify_usd(), vec![5.0]);
    }

    #[test]
    fn spend_thresholds_are_sorted_deduped_and_filtered() {
        let settings = HostSettings::default();
        settings.set_spend_notify_usd(vec![25.0, 5.0, 5.0, 0.0, -1.0, f64::NAN]);
        assert_eq!(*settings.spend_notify_usd(), vec![5.0, 25.0]);
    }
}

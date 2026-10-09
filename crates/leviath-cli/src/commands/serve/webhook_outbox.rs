//! The completion webhooks `lev serve` still owes, kept on disk so that a
//! restart of the server does not lose one.
//!
//! A webhook is sent from this server's memory: a retry that is waiting when
//! the server stops is gone with it, and a run that finishes while no server
//! is running sends no event this server will ever see. So every delivery is
//! *settled* on disk once it is over (delivered, refused, or out of retries),
//! and a server that starts sends each finished run with a callback that has
//! no settled delivery. The delivery id is the same every time, so a receiver
//! that already has one drops the repeat.
//!
//! Only runs that finished after this outbox first existed are owed. A server
//! upgraded onto a machine with a history of runs owes none of them, rather
//! than sending every webhook that machine ever sent a second time.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use leviath_core::run_meta::RunMeta;

/// The file holding the second the outbox first existed.
const SINCE: &str = "since";

/// The directory of settled deliveries, one empty file per delivery id.
const SETTLED: &str = "settled";

/// Which completion webhooks are settled, on disk, and which this server is
/// sending right now, in memory.
#[derive(Debug)]
pub(super) struct Outbox {
    dir: PathBuf,
    /// Runs that finished before this second are not owed.
    since: i64,
    /// Delivery ids being sent by this process, so the startup sweep and a
    /// live event for the same run do not both send it.
    sending: Mutex<HashSet<String>>,
}

impl Outbox {
    /// The outbox in `dir`, made if it is not there, counting from `now` when
    /// it is new.
    ///
    /// A directory that cannot be written still gives an outbox: delivery
    /// carries on as it would without one, and only the memory across a
    /// restart is lost, which is logged once here.
    pub(super) fn open(dir: PathBuf, now: i64) -> Self {
        let since_file = dir.join(SINCE);
        let since = match std::fs::read_to_string(&since_file) {
            Ok(text) => text.trim().parse().unwrap_or(now),
            Err(_) => {
                let written = std::fs::create_dir_all(dir.join(SETTLED))
                    .and_then(|()| std::fs::write(&since_file, now.to_string()));
                if let Err(e) = written {
                    let why = format!("{}: {e}", dir.display());
                    tracing::warn!(
                        why,
                        "the webhook outbox cannot be written, so a webhook in flight is lost if lev serve restarts"
                    );
                }
                now
            }
        };
        Self {
            dir,
            since,
            sending: Mutex::new(HashSet::new()),
        }
    }

    /// Take `delivery` to send: `false` when it is settled already or this
    /// process is sending it.
    pub(super) fn claim(&self, delivery: &str) -> bool {
        if self.is_settled(delivery) {
            return false;
        }
        leviath_core::sync::lock(&self.sending).insert(delivery.to_string())
    }

    /// Record that `delivery` is over, whatever its outcome.
    pub(super) fn settle(&self, delivery: &str) {
        if let Err(e) = std::fs::write(self.marker(delivery), b"") {
            let why = e.to_string();
            tracing::warn!(
                delivery,
                why,
                "a settled webhook could not be recorded, so a restart may send it again"
            );
        }
        leviath_core::sync::lock(&self.sending).remove(delivery);
    }

    /// The finished runs in `runs` that still owe a webhook under the
    /// delivery id `delivery` gives each.
    pub(super) fn owed<'a>(
        &self,
        runs: &'a [RunMeta],
        delivery: impl Fn(&str) -> String,
    ) -> Vec<&'a RunMeta> {
        runs.iter()
            .filter(|meta| {
                meta.callback_url.is_some()
                    && crate::runstate::is_terminal_status(&meta.status)
                    && meta.updated_at >= self.since
                    && !self.is_settled(&delivery(&meta.run_id))
            })
            .collect()
    }

    fn is_settled(&self, delivery: &str) -> bool {
        self.marker(delivery).exists()
    }

    /// Where `delivery`'s settled marker lives. A delivery id is
    /// `<event>:<run id>`, and a run id is a single safe path component, so
    /// the `:` is the only character that needs replacing to make a file name
    /// on every platform.
    fn marker(&self, delivery: &str) -> PathBuf {
        self.dir.join(SETTLED).join(delivery.replace(':', "-"))
    }
}

#[cfg(test)]
#[path = "webhook_outbox_tests.rs"]
mod tests;

//! The completion webhooks `lev serve` still owes, kept on disk so that a
//! restart of the server does not lose one.
//!
//! A webhook is sent from this server's memory: a retry that is waiting when
//! the server stops is gone with it, and a run that finishes while no server
//! is running, or while the server's stream from the daemon is down or
//! falling behind, sends no event this server will ever see. So every
//! delivery is *settled* on disk once it is over (delivered, refused, or out
//! of retries), and the server sweeps for each finished run with a callback
//! that has no settled delivery: when its stream opens, when the stream says
//! events were lost, and every few minutes besides. The delivery id is the
//! same every time, so a receiver that already has one drops the repeat.
//!
//! Only runs that finished after this outbox first existed are owed. A server
//! upgraded onto a machine with a history of runs owes none of them, rather
//! than sending every webhook that machine ever sent a second time.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use leviath_core::run_meta::RunMeta;
use tokio::sync::Semaphore;

/// The file holding the second the outbox first existed.
const SINCE: &str = "since";

/// The directory of settled deliveries, one empty file per delivery id.
const SETTLED: &str = "settled";

/// How many webhooks are sent at once. A server that starts with a backlog
/// sends it at this pace rather than opening a connection per run at once.
pub(super) const AT_ONCE: usize = 8;

/// Which completion webhooks are settled, on disk, and which this server is
/// sending right now, in memory.
#[derive(Debug)]
pub(super) struct Outbox {
    dir: PathBuf,
    /// Runs that finished before this second are not owed.
    since: i64,
    /// Delivery ids being sent by this process, so a sweep and a live event
    /// for the same run do not both send it.
    sending: Mutex<HashSet<String>>,
    /// Deliveries this process settled but could not record on disk: they
    /// are not sent again while it runs, though a restart may send them.
    settled_here: Mutex<HashSet<String>>,
    /// One permit per webhook being sent.
    pub(super) permits: Arc<Semaphore>,
    /// Whether a sweep for owed webhooks is running, and whether another was
    /// asked for while it ran.
    sweep: Mutex<(bool, bool)>,
}

impl Outbox {
    /// The outbox in `dir`, made if it is not there, counting from `now` when
    /// it is new.
    ///
    /// A directory that cannot be written still gives an outbox: delivery
    /// carries on as it would without one, and only the memory across a
    /// restart is lost, which is logged once here. A marker that does not
    /// read as a second is written again from `now`; one that cannot be read
    /// at all is left as it is, and this server counts from `now`.
    pub(super) fn open(dir: PathBuf, now: i64) -> Self {
        let since_file = dir.join(SINCE);
        let since = match std::fs::read_to_string(&since_file) {
            Ok(text) => text.trim().parse().unwrap_or_else(|_| {
                let shown = since_file.display().to_string();
                tracing::warn!(file = %shown, "the webhook outbox's start was not a time, so it starts again from now");
                Self::write_since(&dir, now);
                now
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Self::write_since(&dir, now);
                now
            }
            Err(e) => {
                let why = format!("{}: {e}", since_file.display());
                tracing::warn!(why, "the webhook outbox's start could not be read, so this server counts from now");
                now
            }
        };
        Self {
            dir,
            since,
            sending: Mutex::new(HashSet::new()),
            settled_here: Mutex::new(HashSet::new()),
            permits: Arc::new(Semaphore::new(AT_ONCE)),
            sweep: Mutex::new((false, false)),
        }
    }

    /// Make the outbox's directories and record `now` as its start, saying
    /// once when that cannot be done.
    fn write_since(dir: &std::path::Path, now: i64) {
        let written = std::fs::create_dir_all(dir.join(SETTLED))
            .and_then(|()| std::fs::write(dir.join(SINCE), now.to_string()));
        if let Err(e) = written {
            let why = format!("{}: {e}", dir.display());
            tracing::warn!(
                why,
                "the webhook outbox cannot be written, so a webhook in flight is lost if lev serve restarts"
            );
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

    /// Give `delivery` up unsent and unsettled, for a later sweep to send.
    pub(super) fn release(&self, delivery: &str) {
        leviath_core::sync::lock(&self.sending).remove(delivery);
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
            leviath_core::sync::lock(&self.settled_here).insert(delivery.to_string());
        }
        leviath_core::sync::lock(&self.sending).remove(delivery);
    }

    /// Start a sweep: `false` when one is running already, which then runs
    /// once more when it is done.
    pub(super) fn begin_sweep(&self) -> bool {
        let mut sweep = leviath_core::sync::lock(&self.sweep);
        match sweep.0 {
            true => {
                sweep.1 = true;
                false
            }
            false => {
                sweep.0 = true;
                true
            }
        }
    }

    /// A sweep is done: `true` when another was asked for while it ran, so it
    /// goes again, and `false` when it is over.
    pub(super) fn end_sweep(&self) -> bool {
        let mut sweep = leviath_core::sync::lock(&self.sweep);
        let again = std::mem::take(&mut sweep.1);
        sweep.0 = again;
        again
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
            || leviath_core::sync::lock(&self.settled_here).contains(delivery)
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

//! Idempotency keys: a spawn sent under one starts at most one run on this
//! machine.
//!
//! A caller that times out waiting for a spawn cannot tell whether the run
//! started. Sending the request again under the same key is how it finds
//! out without starting a second run. The host keeps every key by what holds
//! it:
//!
//! - **A run.** The same request again is answered with that run, marked
//!   `deduplicated`, and the warnings it started with. Nothing starts, so no
//!   second `Spawned` event and no second webhook. A different request under
//!   the key is refused with a `conflict` at `delivery.idempotency_key` that
//!   names the run.
//! - **A start still under way.** The same request waits for that start and
//!   gets its answer; a different one is refused the same way.
//!
//! A start that is refused frees its key. So does deleting the run: a key is
//! held for as long as its run's record exists, and the record is looked for
//! each time the key is asked about. The key is kept in the run's record
//! too, so a host started over existing runs recalls every key they hold
//! (see [`held_keys`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tokio::sync::oneshot;

use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::launch::Idempotency;
use crate::spec::names::{Digest, IdempotencyKey, RunId};
use crate::spec::summary::Spawned;

/// Who is waiting to hear how a spawn went.
pub(super) type Reply = oneshot::Sender<Result<Spawned, SpawnIssues>>;

/// A key a run holds, as the run's record keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldKey {
    /// The key, and the digest of the request that started the run.
    pub idempotency: Idempotency,
    /// The run.
    pub run_id: RunId,
    /// What may keep the run from ever finishing, as its spawn answered.
    pub warnings: SpawnIssues,
}

/// What holds one key.
enum Claim {
    /// A start, not yet placed or refused, and everyone who sent the same
    /// request while it ran.
    Starting {
        /// The digest of the request being started.
        request: Digest,
        /// Who is waiting on its answer.
        waiting: Vec<Reply>,
    },
    /// A run.
    Held {
        /// The digest of the request that started it.
        request: Digest,
        /// The run.
        run_id: RunId,
        /// The warnings its spawn answered with.
        warnings: SpawnIssues,
    },
}

/// Every key in use on this host, by key.
#[derive(Default)]
pub(super) struct Claims(HashMap<IdempotencyKey, Claim>);

impl Claims {
    /// Take in a spawn sent under `asked`. `Some(reply)` when it should go
    /// ahead and start, holding the key while it does; `None` when it has
    /// been answered here, or will be once the start it repeats settles.
    ///
    /// `exists` says whether a run's record is still there. A key whose run
    /// has been deleted is free.
    pub(super) fn admit(
        &mut self,
        asked: &Idempotency,
        reply: Reply,
        exists: &dyn Fn(&RunId) -> bool,
    ) -> Option<Reply> {
        match self.0.get_mut(&asked.key) {
            Some(Claim::Held { run_id, .. }) if !exists(run_id) => {}
            Some(Claim::Held {
                request,
                run_id,
                warnings,
            }) => {
                let answer = match *request == asked.request {
                    true => Ok(Spawned {
                        run_id: run_id.clone(),
                        warnings: warnings.clone(),
                        deduplicated: true,
                    }),
                    false => Err(conflict(&asked.key, Some(run_id))),
                };
                let _ = reply.send(answer);
                return None;
            }
            Some(Claim::Starting { request, waiting }) => {
                match *request == asked.request {
                    true => waiting.push(reply),
                    false => {
                        let _ = reply.send(Err(conflict(&asked.key, None)));
                    }
                }
                return None;
            }
            None => {}
        }
        self.0.insert(
            asked.key.clone(),
            Claim::Starting {
                request: asked.request.clone(),
                waiting: Vec::new(),
            },
        );
        Some(reply)
    }

    /// The start under `key` has settled with `outcome`: everyone who waited
    /// on it hears the same answer, and the key stays held by the run it
    /// started, or is freed when it started none.
    pub(super) fn settle(&mut self, key: &IdempotencyKey, outcome: &Result<Spawned, SpawnIssues>) {
        if let Some(Claim::Starting { request, waiting }) = self.0.remove(key) {
            if let Ok(spawned) = outcome {
                self.0.insert(
                    key.clone(),
                    Claim::Held {
                        request,
                        run_id: spawned.run_id.clone(),
                        warnings: spawned.warnings.clone(),
                    },
                );
            }
            for reply in waiting {
                let _ = reply.send(outcome.clone().map(|first| Spawned {
                    deduplicated: true,
                    ..first
                }));
            }
        }
    }

    /// How many keys are in use.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    /// Forget every key whose run's record has gone from `runs_dir`, so the
    /// keys of deleted runs do not pile up. Starts under way are kept.
    pub(super) fn prune(&mut self, runs_dir: Option<&Path>) {
        self.0.retain(|_, claim| match claim {
            Claim::Held { run_id, .. } => record_exists(runs_dir, run_id),
            Claim::Starting { .. } => true,
        });
    }

    /// Hold every key in `held` for its run. A key already in use stays
    /// with what holds it.
    pub(super) fn recall(&mut self, held: Vec<HeldKey>) {
        for key in held {
            self.0.entry(key.idempotency.key).or_insert(Claim::Held {
                request: key.idempotency.request,
                run_id: key.run_id,
                warnings: key.warnings,
            });
        }
    }
}

/// The refusal for a request under `key` that is not the one holding it:
/// the run `holder` that does, or a start still under way.
fn conflict(key: &IdempotencyKey, holder: Option<&RunId>) -> SpawnIssues {
    let message = match holder {
        Some(run) => format!(
            "idempotency key '{key}' is held by run '{run}', which a different request started"
        ),
        None => format!(
            "idempotency key '{key}' is held by a different request whose run is still starting"
        ),
    };
    SpawnIssue::new(
        SpecPath::root().field("delivery").field("idempotency_key"),
        IssueCode::Conflict,
        message,
    )
    .got(key.as_str())
    .hint("send the request that holds the key to hear about its run, or use a new key")
    .into()
}

/// Whether the record of the run `run` is still under `runs_dir`. A host
/// with no runs directory keeps no records to delete, so every run it
/// started is still there.
pub(super) fn record_exists(runs_dir: Option<&Path>, run: &RunId) -> bool {
    runs_dir.is_none_or(|dir| {
        dir.join(run.as_str())
            .join(leviath_core::files::RUN_FILE)
            .is_file()
    })
}

/// The file a run's directory holds when its start was refused after its
/// record was written: a binding that failed, or a placement that panicked.
/// Its caller was told the run did not start, so the run holds no key.
pub const REFUSED_AT_START: &str = "refused-at-start";

/// Mark the run whose record is the file `run_file` as refused at its start
/// (see [`REFUSED_AT_START`]).
pub fn mark_refused(run_file: &Path) -> std::io::Result<()> {
    leviath_sys::perms::write_private(&run_file.with_file_name(REFUSED_AT_START), b"")
}

/// Every key a run under `runs_dir` holds, read from the runs' records:
/// what a host started over existing runs recalls with
/// [`WorldHost::recall_keys`](super::WorldHost::recall_keys).
///
/// Each run's spec is read from the front of its file. A run marked
/// [`REFUSED_AT_START`] holds no key: its record says why it never ran,
/// and its caller was told it did not start.
pub fn held_keys(runs_dir: &Path) -> Vec<HeldKey> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(runs_dir)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    dirs.sort();
    dirs.iter().filter_map(|dir| held_key(dir)).collect()
}

/// The key the run in `dir` holds, when it holds one.
fn held_key(dir: &Path) -> Option<HeldKey> {
    let spec = crate::runfile::read_spec(&dir.join(leviath_core::files::RUN_FILE)).ok()?;
    let idempotency = spec.delivery.idempotency.clone()?;
    let refused = dir.join(REFUSED_AT_START).exists();
    (!refused).then(|| HeldKey {
        idempotency,
        run_id: spec.run_id.clone(),
        warnings: spec.warnings(),
    })
}

#[cfg(test)]
#[path = "claims_tests.rs"]
mod tests;

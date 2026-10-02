//! A run's file as the bundle carries it: `run.lvr` rewritten with its
//! secrets out, the same values as `run.json`, and `request.json`, the spawn
//! request that starts the run again.
//!
//! The rewritten file is built from the scrubbed values, so it holds nothing
//! `run.json` does not. Copied into another machine's `runs/` it reads like
//! any other run: `lev run show`, `lev timeline`, `lev context`, `lev stages`
//! and `lev result` all answer from it.

use std::path::Path;

use leviath_runtime::runfile::{CheckpointPolicy, RunFileReader, RunFileWriter};
use leviath_runtime::spec::request::{SpawnRequest, SpawnSource};
use leviath_runtime::spec::run_spec::{RunSpec, SpecOrigin};
use leviath_runtime::state::{RunState, StateDelta};
use serde::{Deserialize, Serialize};

/// Everything a run file holds that a reader needs, as `run.json` writes it.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct RunJson {
    /// What the run was resolved to, the webhook's secret taken out.
    pub(super) spec: RunSpec,
    /// The state it started in.
    pub(super) start: RunState,
    /// The state after its last step.
    pub(super) state: RunState,
    /// Every step after the start.
    pub(super) steps: Vec<StateDelta>,
}

impl RunJson {
    /// The values in `reader`, or why they do not read.
    pub(super) fn read(reader: &RunFileReader) -> Result<Self, String> {
        let spec = crate::runstate::run_file::redacted_spec(reader.spec().clone());
        reader
            .state_at(0)
            .and_then(|start| {
                reader.latest_state().and_then(|state| {
                    reader
                        .deltas(1, reader.last_seq())
                        .map(|steps| (start, state, steps))
                })
            })
            .map(|(start, state, steps)| Self {
                spec,
                start,
                state,
                steps,
            })
            .map_err(|e| format!("the run file could not be read: {e}"))
    }

    /// These values written as a run file in `scratch`, with the code
    /// `reader` holds and, when `blobs` is set, its stored parts. The bytes of
    /// the new file; the file itself is removed.
    pub(super) fn rewrite(
        &self,
        reader: &RunFileReader,
        blobs: bool,
        scratch: &Path,
    ) -> Result<Vec<u8>, String> {
        let path = scratch.join(format!("lev-rage-{}.lvr", self.spec.run_id));
        let parts: Vec<_> = match blobs {
            true => reader.blob_digests().cloned().collect(),
            false => Vec::new(),
        };
        let written = reader
            .code_files()
            .and_then(|code| {
                RunFileWriter::create(
                    &path,
                    &self.spec,
                    &code,
                    &self.start,
                    CheckpointPolicy::default(),
                )
            })
            .and_then(|mut writer| {
                self.steps
                    .iter()
                    .try_for_each(|step| writer.append_delta(step))
                    .and_then(|()| {
                        parts.iter().try_for_each(|digest| {
                            reader.blob(digest).and_then(|bytes| {
                                writer
                                    .add_blob(digest, &bytes.unwrap_or_default())
                                    .map(drop)
                            })
                        })
                    })
            })
            .map_err(Box::<dyn std::error::Error>::from)
            .and_then(|()| std::fs::read(&path).map_err(Box::<dyn std::error::Error>::from));
        let _ = std::fs::remove_file(&path);
        written.map_err(|e| format!("the run file could not be rewritten: {e}"))
    }
}

/// The request that starts `spec`'s run again: the same blueprint or graph,
/// the same inputs, model and output, attended, in the directory it is
/// started from, with no webhook.
pub(super) fn request_of(spec: &RunSpec) -> SpawnRequest {
    let source = match &spec.origin {
        SpecOrigin::Blueprint { blueprint, .. } => {
            SpawnSource::Blueprint(leviath_runtime::spec::names::BlueprintRef {
                name: blueprint.name.clone(),
                digest: None,
            })
        }
        SpecOrigin::BlueprintFile { name, .. } | SpecOrigin::Recorded { name, .. } => {
            SpawnSource::Blueprint(leviath_runtime::spec::names::BlueprintRef {
                name: name.clone(),
                digest: None,
            })
        }
        SpecOrigin::Raw => SpawnSource::Raw(Box::new(spec.graph.clone())),
    };
    let mut request = SpawnRequest::new(source);
    request.inputs = spec
        .inputs
        .0
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_raw()))
        .collect();
    request.model = spec.requested_model.clone();
    request.output = spec.requested_output.clone();
    request
}

#[cfg(test)]
#[path = "replay_tests.rs"]
mod tests;

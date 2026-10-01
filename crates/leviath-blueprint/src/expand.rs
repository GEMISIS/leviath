//! Turning a blueprint reference and its inputs into a spawn request.
//!
//! A blueprint is a partial request: it fixes the graph, and the caller fills
//! in only what the graph declares as inputs. So expanding one is small. The
//! request names the blueprint rather than copying its graph, and the resolver
//! loads the graph through the host (which calls [`find`](crate::find)) and
//! checks every input against the graph's declarations there.

use std::collections::BTreeMap;

use leviath_runtime::spec::inputs::RawInput;
use leviath_runtime::spec::names::BlueprintRef;
use leviath_runtime::spec::request::{SpawnRequest, SpawnSource};

/// A request to run `reference` with `inputs`, every other field at its
/// default. A front door sets the workdir, launch policy and delivery on the
/// result before sending it.
pub fn expand(reference: BlueprintRef, inputs: BTreeMap<String, RawInput>) -> SpawnRequest {
    SpawnRequest {
        inputs,
        ..SpawnRequest::new(SpawnSource::Blueprint(reference))
    }
}

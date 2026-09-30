//! The run file: one file per run holding everything about it.
//!
//! A run file starts with the run's [`RunSpec`](crate::spec::run_spec::RunSpec),
//! the state it started from. The code and blobs it uses follow, stored once
//! each by digest. Then come [`StateDelta`](crate::state::StateDelta)s, one
//! per step, with a full [`RunState`](crate::state::RunState) checkpoint
//! every so often. The last checkpoint plus the deltas after it is where the
//! run is now; any earlier point is the spec's state with the deltas up to
//! it applied.
//!
//! [`codec`] is the byte layout.

use std::sync::OnceLock;

use sha2::Digest as _;

pub mod codec;

/// The JSON Schemas of every type a run file stores, as one document.
///
/// `cargo xtask schema` writes it to `docs/schema/run-file.schema.json`, and
/// its hash is the fingerprint every run file's header carries.
pub fn frame_schemas() -> serde_json::Value {
    serde_json::json!({
        "spec": schemars::schema_for!(crate::spec::run_spec::RunSpec),
        "state": schemars::schema_for!(crate::state::RunState),
        "delta": schemars::schema_for!(crate::state::StateDelta),
    })
}

/// The hash of [`frame_schemas`]: two builds share it exactly when their run
/// files have the same shape.
pub fn fingerprint() -> &'static [u8; 32] {
    static FP: OnceLock<[u8; 32]> = OnceLock::new();
    FP.get_or_init(|| sha2::Sha256::digest(frame_schemas().to_string().as_bytes()).into())
}

/// The JSON Schema of a spawn request, as published for outside tools and
/// agents.
pub fn spawn_request_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(crate::spec::request::SpawnRequest))
        .expect("a schema is plain JSON")
}

#[cfg(test)]
mod tests {
    /// The published schemas are the ones this build generates. Run with
    /// `LEVIATH_WRITE_SCHEMAS=1` to rewrite them after changing a type.
    #[test]
    fn the_published_schemas_match_this_build() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/schema");
        let files = [
            ("spawn-request.schema.json", super::spawn_request_schema()),
            ("run-file.schema.json", super::frame_schemas()),
        ];
        for (name, schema) in files {
            let path = dir.join(name);
            let text = format!("{}\n", serde_json::to_string_pretty(&schema).unwrap());
            if std::env::var_os("LEVIATH_WRITE_SCHEMAS").is_some() {
                std::fs::write(&path, &text).unwrap();
            }
            let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
            assert!(
                on_disk == text,
                "{name} is out of date; rerun this test with LEVIATH_WRITE_SCHEMAS=1 and commit the file"
            );
        }
    }

    #[test]
    fn the_fingerprint_is_stable_and_covers_every_frame_type() {
        assert_eq!(super::fingerprint(), super::fingerprint());
        let text = super::frame_schemas().to_string();
        for name in [
            "RunSpec",
            "RunState",
            "StateDelta",
            "ContextState",
            "PipelinePhase",
        ] {
            assert!(text.contains(name), "{name} missing");
        }
    }
}

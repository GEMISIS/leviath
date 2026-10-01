//! The one way to ask for a run.
//!
//! Every front door (the CLI, REST, GraphQL, ACP, the control socket, the
//! embed API, and the tools an agent calls) builds a [`SpawnRequest`]. It
//! either names an installed blueprint, whose graph the blueprint layer
//! fills in, or carries a whole [`RunGraph`] of its own. Either way it
//! supplies the graph's inputs as [`RawInput`]s, which the resolver checks
//! against the graph's declarations.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use leviath_core::mime::Delivery;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::graph::{OutputDef, RunGraph};
use super::inputs::RawInput;
use super::launch::{Delivery as RunDelivery, LaunchRequest};
use super::names::{BlueprintRef, MimePattern, ModelRef, RegionName};

/// A request for a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpawnRequest {
    /// What to run.
    pub source: SpawnSource,
    /// Values for the graph's declared inputs, by name.
    #[serde(default)]
    pub inputs: BTreeMap<String, RawInput>,
    /// Files for `file` inputs, and parts placed straight into regions.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// A model to run every stage on that allows the operator's choice,
    /// over the graph's own.
    #[serde(default)]
    pub model: Option<ModelRef>,
    /// The final-output shape the caller wants, over the graph's.
    #[serde(default)]
    pub output: Option<OutputDef>,
    /// The directory tools work in. `None` takes the front door's default
    /// (the CLI's current directory, the server's configured workdir).
    #[serde(default)]
    pub workdir: Option<PathBuf>,
    /// How much the run is trusted with.
    #[serde(default)]
    pub launch: LaunchRequest,
    /// Who hears about the run, and the caller's labels for it.
    #[serde(default)]
    pub delivery: RunDelivery,
}

impl SpawnRequest {
    /// A request for `source` with every other field at its default.
    pub fn new(source: SpawnSource) -> Self {
        Self {
            source,
            inputs: BTreeMap::new(),
            attachments: Vec::new(),
            model: None,
            output: None,
            workdir: None,
            launch: LaunchRequest::default(),
            delivery: RunDelivery::default(),
        }
    }

    /// This request with one more input value.
    pub fn input(mut self, name: impl Into<String>, value: RawInput) -> Self {
        self.inputs.insert(name.into(), value);
        self
    }
}

/// What a request runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SpawnSource {
    /// An installed blueprint.
    Blueprint(BlueprintRef),
    /// A whole graph, written by the caller.
    Raw(Box<RunGraph>),
}

/// A file sent with a request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    /// The file's name. A `file` input names an attachment by this.
    pub name: String,
    /// Its mime type. `None` has it sniffed from the bytes and the name.
    #[serde(default)]
    pub mime_type: Option<MimePattern>,
    /// A region to place it in directly, when no `file` input takes it.
    #[serde(default)]
    pub region: Option<RegionName>,
    /// How a model should receive it, over the region's default.
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub deliver: Option<Delivery>,
    /// Text placed beside it.
    #[serde(default)]
    pub caption: Option<String>,
    /// The bytes. Base64 in JSON and TOML.
    pub data: Bytes,
}

/// Bytes: base64 text in a readable format, raw in a binary one.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Bytes(pub Vec<u8>);

impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Bytes({} bytes)", self.0.len())
    }
}

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match s.is_human_readable() {
            true => s.serialize_str(&STANDARD.encode(&self.0)),
            false => s.serialize_bytes(&self.0),
        }
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match d.is_human_readable() {
            true => {
                let text = String::deserialize(d)?;
                STANDARD
                    .decode(text.as_bytes())
                    .map(Bytes)
                    .map_err(|e| D::Error::custom(format!("base64: {e}")))
            }
            false => Vec::<u8>::deserialize(d).map(Bytes),
        }
    }
}

impl schemars::JsonSchema for Bytes {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Bytes".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "string", "contentEncoding": "base64" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blueprint_request_reads_the_way_a_person_writes_it() {
        let req: SpawnRequest = serde_json::from_str(
            r#"{
                "source": {"blueprint": {"name": "coder"}},
                "inputs": {"task": "fix the bug", "depth": 3},
                "launch": {"unattended": "all"}
            }"#,
        )
        .unwrap();
        assert_eq!(req.inputs["depth"], RawInput::Int(3));
        let expected = SpawnRequest::new(SpawnSource::Blueprint(
            BlueprintRef::parse("coder").unwrap(),
        ))
        .input("task", RawInput::Text("fix the bug".into()))
        .input("depth", RawInput::Int(3));
        assert_eq!(req.source, expected.source);
        assert_eq!(req.inputs, expected.inputs);
    }

    #[test]
    fn misspelled_keys_are_refused() {
        let err = serde_json::from_str::<SpawnRequest>(
            r#"{"source": {"blueprint": {"name": "c"}}, "input": {}}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown field `input`"), "{err}");
    }

    #[test]
    fn attachments_carry_base64_in_json_and_raw_bytes_in_binary() {
        let a = Attachment {
            name: "a.png".into(),
            mime_type: Some(MimePattern::new("image/png").unwrap()),
            region: None,
            deliver: Some(Delivery::Native),
            caption: None,
            data: Bytes(vec![1, 2, 3]),
        };
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains("\"AQID\""), "{json}");
        assert_eq!(serde_json::from_str::<Attachment>(&json).unwrap(), a);
        let bin = postcard::to_stdvec(&a).unwrap();
        assert_eq!(postcard::from_bytes::<Attachment>(&bin).unwrap(), a);
        assert_eq!(format!("{:?}", a.data), "Bytes(3 bytes)");
        let bad = serde_json::from_str::<Bytes>("\"!!\"").unwrap_err();
        assert!(bad.to_string().contains("base64"), "{bad}");
        // Bytes written as anything but text are refused before decoding.
        assert!(serde_json::from_str::<Bytes>("[1, 2]").is_err());
    }

    #[test]
    fn a_raw_request_carries_its_whole_graph() {
        let graph = crate::spec::graph::tests::minimal();
        let req = SpawnRequest::new(SpawnSource::Raw(Box::new(graph.clone())));
        let json = serde_json::to_string(&req).unwrap();
        let back: SpawnRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source, SpawnSource::Raw(Box::new(graph)));
    }

    #[test]
    fn the_request_schema_exports() {
        let schema = schemars::schema_for!(SpawnRequest);
        let text = serde_json::to_string(&schema).unwrap();
        for word in ["source", "inputs", "attachments", "launch", "RunGraph"] {
            assert!(text.contains(word), "{word} missing from the schema");
        }
    }
}

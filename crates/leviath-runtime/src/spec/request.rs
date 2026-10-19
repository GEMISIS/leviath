//! The one way to ask for a run.
//!
//! Every front door (the CLI, REST, GraphQL, ACP, the control socket, the
//! embed API, and the tools an agent calls) builds a [`SpawnRequest`]. It
//! names an installed blueprint or (from a caller on this machine) the
//! directory of one that is not installed, whose graph the blueprint layer
//! fills in, or it carries a whole [`RunGraph`] of its own. Either way it
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
use super::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use super::launch::{Delivery as RunDelivery, Idempotency, LaunchRequest};
use super::names::{BlueprintPath, BlueprintRef, MimePattern, ModelRef, RegionName};
use super::readable::{WriteWith, pick};

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

    /// The key this request was sent under, with the digest of the request
    /// itself, when it names one.
    ///
    /// The digest is of the request as JSON with the key left out, so the
    /// same request sent again under the same key has the same digest, and
    /// any other field changed gives another. Every map in a request is
    /// ordered, so the JSON of one request is always the same text.
    ///
    /// What says nothing is left out first: every field
    /// still at a request's default, and every null, empty list and empty
    /// table. A field a later build adds is in neither form while it holds
    /// such a default, so a request sent again across an upgrade still
    /// matches. One added with any other default, such as `true`, would not.
    pub fn idempotency(&self) -> Option<Idempotency> {
        let key = self.delivery.idempotency_key.clone()?;
        let mut unkeyed = self.clone();
        unkeyed.delivery.idempotency_key = None;
        let mut value = serde_json::to_value(&unkeyed).expect("a spawn request is plain data");
        let defaults = serde_json::to_value(Self::new(self.source.clone()))
            .expect("a spawn request is plain data");
        canonical(&mut value, &defaults);
        Some(Idempotency {
            key,
            request: super::names::Digest::of(value.to_string().as_bytes()),
        })
    }

    /// Refuse what a request that arrived over the network may not ask for:
    /// a blueprint read from a directory on this machine. A front door that
    /// takes whole requests from a remote caller checks this before handing
    /// the request on; one on this machine (the CLI, the control socket, an
    /// embedding program) does not need to.
    pub fn check_remote(&self) -> Result<(), SpawnIssues> {
        let local_only = |path: SpecPath| {
            SpawnIssue::new(
                path,
                IssueCode::NotAllowed,
                "a blueprint is read from a directory only for a caller on this machine",
            )
            .hint("name an installed blueprint, or send the graph itself")
        };
        let source = SpecPath::root().field("source");
        match &self.source {
            SpawnSource::BlueprintFile(_) => Err(local_only(source.field("blueprint_file")).into()),
            SpawnSource::Blueprint(_) => Ok(()),
            // A raw graph's fan-out may name its workers' blueprint by
            // directory too, which is the same read.
            SpawnSource::Raw(graph) => {
                let mut issues = SpawnIssues::new();
                for stage in &graph.stages {
                    let super::graph::StageMode::FanOut(fan) = &stage.mode else {
                        continue;
                    };
                    for (i, worker) in fan.workers_available.iter().enumerate() {
                        if let super::graph::WorkerSource::BlueprintFile(_) = worker.runs {
                            issues.push(local_only(
                                source
                                    .field("raw")
                                    .field("stages")
                                    .key(stage.name.as_str())
                                    .field("mode")
                                    .field("fan_out")
                                    .field("workers_available")
                                    .index(i),
                            ));
                        }
                    }
                }
                issues.into_result(())
            }
        }
    }
}

/// `value` with what says nothing left out, at every depth: each key of a
/// table whose value is the one `defaults` holds at the same place, and each
/// null, empty list and empty table. A list keeps every item.
fn canonical(value: &mut serde_json::Value, defaults: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                canonical(item, &defaults[key.as_str()]);
            }
            map.retain(|key, item| !says_nothing(item) && *item != defaults[key.as_str()]);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                canonical(item, &serde_json::Value::Null);
            }
        }
        _ => {}
    }
}

/// Whether `value` is a null, an empty list or an empty table.
fn says_nothing(value: &serde_json::Value) -> bool {
    value.is_null()
        || value.as_array().is_some_and(Vec::is_empty)
        || value.as_object().is_some_and(serde_json::Map::is_empty)
}

/// What a request runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SpawnSource {
    /// An installed blueprint.
    Blueprint(BlueprintRef),
    /// A blueprint that is not installed, read from its directory on this
    /// machine. Local callers only: see [`SpawnRequest::check_remote`].
    BlueprintFile(BlueprintPath),
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
        let raw: WriteWith<Self, S> = |b, s| s.serialize_bytes(&b.0);
        let text: WriteWith<Self, S> = |b, s| s.serialize_str(&STANDARD.encode(&b.0));
        pick(s.is_human_readable(), raw, text)(self, s)
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw: fn(D) -> Result<Self, D::Error> = |d| Vec::<u8>::deserialize(d).map(Bytes);
        let text: fn(D) -> Result<Self, D::Error> = |d| {
            String::deserialize(d).and_then(|text| {
                STANDARD
                    .decode(text.as_bytes())
                    .map(Bytes)
                    .map_err(|e| D::Error::custom(format!("base64: {e}")))
            })
        };
        pick(d.is_human_readable(), raw, text)(d)
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

    /// A raw graph whose fan-out reads its worker from a directory asks for
    /// the same local read a `blueprint_file` source does.
    #[test]
    fn a_remote_raw_graph_may_not_name_a_worker_by_directory() {
        use crate::spec::graph::{StageMode, WorkerSource};
        let mut graph = crate::spec::graph::tests::minimal();
        let dir = std::env::temp_dir().join("w");
        let mut fan = crate::test_graph::fan_out_to("plan", "build");
        graph.stages[0].mode = StageMode::FanOut(fan.clone());
        let request = SpawnRequest::new(SpawnSource::Raw(Box::new(graph.clone())));
        assert!(request.check_remote().is_ok());
        fan.workers_available[0].runs =
            WorkerSource::BlueprintFile(BlueprintPath::new(dir.to_string_lossy()).unwrap());
        graph.stages[0].mode = StageMode::FanOut(fan);
        let issues = SpawnRequest::new(SpawnSource::Raw(Box::new(graph)))
            .check_remote()
            .unwrap_err();
        assert_eq!(
            issues.0[0].path.to_string(),
            "source.raw.stages.plan.mode.fan_out.workers_available[0]"
        );
        assert_eq!(issues.0[0].code, IssueCode::NotAllowed);
    }

    /// The digest is the request's without its key: the same request under
    /// the same key digests the same, whatever key it was, and any other
    /// change moves it. A request with no key has none.
    #[test]
    fn a_keyed_request_digests_everything_but_its_key() {
        let base = SpawnRequest::new(SpawnSource::Blueprint(BlueprintRef::parse("c").unwrap()))
            .input("task", RawInput::Text("fix it".into()));
        assert_eq!(base.idempotency(), None);
        let keyed = |key: &str, request: &SpawnRequest| {
            let mut request = request.clone();
            request.delivery.idempotency_key =
                Some(crate::spec::names::IdempotencyKey::new(key).unwrap());
            request.idempotency().unwrap()
        };
        let first = keyed("k-1", &base);
        assert_eq!(first.key.as_str(), "k-1");
        assert_eq!(keyed("k-1", &base), first);
        assert_eq!(keyed("k-2", &base).request, first.request);
        let other = base
            .clone()
            .input("task", RawInput::Text("fix that".into()));
        assert_ne!(keyed("k-1", &other).request, first.request);
    }

    /// What says nothing is left out of the digest's form, at any depth,
    /// and a list keeps its items.
    #[test]
    fn the_canonical_form_leaves_out_what_says_nothing() {
        let mut value = serde_json::json!({
            "same": 1, "changed": 2, "none": null, "list": [], "table": {"inner": null},
            "items": [{"gone": null, "kept": 3}, null], "flag": false
        });
        let defaults = serde_json::json!({"same": 1, "changed": 1, "flag": true});
        canonical(&mut value, &defaults);
        assert_eq!(
            value,
            serde_json::json!({"changed": 2, "items": [{"kept": 3}, null], "flag": false})
        );
    }

    /// A field a later build adds at a default that says nothing leaves the
    /// digest where it was: the same request, read with it, still matches.
    #[test]
    fn a_field_added_at_its_default_leaves_the_digest_alone() {
        let mut request =
            SpawnRequest::new(SpawnSource::Blueprint(BlueprintRef::parse("c").unwrap()));
        request.delivery.idempotency_key =
            Some(crate::spec::names::IdempotencyKey::new("k").unwrap());
        let mut written = serde_json::to_value(&request).unwrap();
        let mut with_more = written.clone();
        with_more["launch"]["a_new_list"] = serde_json::json!([]);
        with_more["a_new_option"] = serde_json::Value::Null;
        let defaults = serde_json::to_value(SpawnRequest::new(request.source.clone())).unwrap();
        canonical(&mut written, &defaults);
        canonical(&mut with_more, &defaults);
        assert_eq!(written, with_more);
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

//! `SpawnRunRequest`: the one request for a run, as GraphQL writes it.
//!
//! It mirrors the runtime's `SpawnRequest` field for field. Every value that
//! does not read (a name with a bad character, an input given twice, a graph
//! that is not a graph, a file outside the working directory) is collected as
//! a `SpawnIssue` at its own path, with this server's own refusals beside
//! them, so a caller learns everything wrong with a request in one answer. A
//! request with no issue goes to the daemon, which checks the rest the same
//! way.
//!
//! A raw graph travels as JSON in the same form a blueprint's `[graph]` table
//! and a JSON request take it, and is read by the runtime's own types. A graph
//! is some sixty nested types, and writing each again as an input object would
//! be a second copy of the format to keep in step with the first, for no check
//! the runtime's reader does not already make.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_graphql::{
    InputObject, InputValueError, InputValueResult, OneofObject, Scalar, ScalarType, Value,
};
use leviath_runtime::spec::graph::{ArtifactDef, CodeRef, OutputDef, RunGraph};
use leviath_runtime::spec::inputs::RawInput;
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use leviath_runtime::spec::launch::{
    Callback, Delivery as RunDelivery, LaunchRequest, Secret, Unattended,
};
use leviath_runtime::spec::names::{
    BlueprintName, BlueprintRef as RuntimeRef, Digest, HttpUrl, MimePattern, ModelRef, NameError,
    ProfileName, RegionName, ToolName,
};
use leviath_runtime::spec::request::{Attachment, Bytes, SpawnRequest, SpawnSource};

use super::super::super::core::attachments;
use super::super::super::core::error::ServeError;
use super::super::inputs::{BlueprintRef, KeyValueWrite, RegionRef};
use super::super::scalars::{BigInt, Json};
use super::super::types::manifest::output::ValidatorErrorPolicy;
use super::attachments::Delivery;

/// A run graph as JSON. The schema description is on the impl below.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GraphDocument(pub(crate) serde_json::Value);

/// A whole run graph, as the JSON object a blueprint's `[graph]` table reads
/// as. Only an object is taken here; what it holds is read by the runtime's
/// own graph reader, and every key it does not know comes back as an issue.
#[Scalar(name = "RunGraphDocument")]
impl ScalarType for GraphDocument {
    fn parse(value: Value) -> InputValueResult<Self> {
        match value {
            Value::Object(_) => Ok(Self(value.into_json().expect("an object is JSON"))),
            other => Err(InputValueError::expected_type(other)),
        }
    }

    fn to_value(&self) -> Value {
        Value::from_json(self.0.clone()).expect("a JSON value converts to a GraphQL one")
    }
}

/// What a run runs: an installed blueprint, or a whole graph.
#[derive(Debug, OneofObject)]
pub(crate) enum SpawnSourceWrite {
    /// An installed blueprint, optionally pinned to the revision on
    /// `BlueprintOutput.digest`. A pin that does not match what is installed
    /// is refused.
    Blueprint(BlueprintRef),
    /// A whole run graph, in the JSON form a blueprint's `[graph]` table
    /// reads as. Unknown keys are refused.
    Graph(GraphDocument),
}

/// One input value, as the request writes it. Exactly one member: the
/// declared type of the input it is for decides how it is read, so `text`
/// for a `model` input is a model name and `text` for a `choice` input is the
/// option.
#[derive(Debug, OneofObject)]
pub(crate) enum InputValueWrite {
    /// Text, for a `text`, `choice`, `file`, `path`, `model`, `blueprint`,
    /// `duration` or `url` input.
    Text(String),
    /// `true` or `false`.
    Bool(bool),
    /// A whole number, for an `int` or a `float` input.
    Int(BigInt),
    /// A number, for a `float` input.
    Float(f64),
    /// A list, for a `list` input.
    List(Vec<InputValueWrite>),
    /// Named fields, for a `record` input or a `model` written as
    /// `provider` and `model`.
    Record(Vec<InputEntryWrite>),
}

/// One named input value, or one field of a `record` value.
#[derive(Debug, InputObject)]
pub(crate) struct InputEntryWrite {
    /// The input's or the field's name.
    pub(crate) name: String,
    /// Its value.
    pub(crate) value: InputValueWrite,
}

/// Where an attachment's bytes come from.
#[derive(Debug, OneofObject)]
pub(crate) enum AttachmentContentWrite {
    /// A file inside the run's working directory, by its path relative to it.
    Path(String),
    /// The bytes themselves, base64 encoded.
    Base64(String),
}

/// A file sent with a spawn: for a `file` input to name, or placed straight
/// into a region.
#[derive(Debug, InputObject)]
pub(crate) struct SpawnAttachmentWrite {
    /// The name a `file` input names it by. Defaults to the file's own name
    /// for a `path`, and is required for `base64`.
    pub(crate) name: Option<String>,
    /// The bytes.
    pub(crate) content: AttachmentContentWrite,
    /// Its mime type. Sniffed from the bytes and the name when absent.
    pub(crate) mime_type: Option<String>,
    /// A region to place it in directly, when no `file` input takes it.
    pub(crate) region: Option<RegionRef>,
    /// How a model should receive it, over the region's default.
    pub(crate) deliver: Option<Delivery>,
    /// Text placed beside it.
    pub(crate) caption: Option<String>,
}

/// Code a graph or an output shape names.
#[derive(Debug, OneofObject)]
pub(crate) enum CodeRefWrite {
    /// A file, by its path relative to the blueprint.
    File(String),
    /// The source itself.
    Inline(String),
}

/// One file the final output must hand back.
#[derive(Debug, InputObject)]
pub(crate) struct OutputArtifactWrite {
    /// The file's name.
    pub(crate) name: String,
    /// Its mime type or pattern.
    pub(crate) mime_type: String,
    /// Whether the answer must include it.
    #[graphql(default = false)]
    pub(crate) required: bool,
    /// What it is.
    pub(crate) description: Option<String>,
}

/// The shape a caller wants the run's final output in, over the graph's.
#[derive(Debug, InputObject)]
pub(crate) struct OutputShapeWrite {
    /// The format, as an opaque label the model is told: `markdown`, `json`,
    /// a house format.
    pub(crate) format: Option<String>,
    /// How to write it.
    pub(crate) instructions: Option<String>,
    /// An example answer.
    pub(crate) example: Option<String>,
    /// A JSON Schema the answer must meet.
    pub(crate) schema: Option<Json>,
    /// Code that checks the answer.
    pub(crate) validator: Option<CodeRefWrite>,
    /// What happens when that code refuses an answer. Absent is `REJECT`.
    pub(crate) on_validator_error: Option<ValidatorErrorPolicy>,
    /// Whether a later answer may replace files an earlier one wrote.
    pub(crate) overwrite_artifacts: Option<bool>,
    /// Files the answer hands back beside its text.
    pub(crate) artifacts: Option<Vec<OutputArtifactWrite>>,
}

/// How much of a run goes ahead without a person. Exactly one member;
/// leaving `unattended` out means a person answers everything.
#[derive(Debug, OneofObject)]
pub(crate) enum UnattendedWrite {
    /// `true` waives every prompt; `false` waives none.
    All(bool),
    /// Waive the prompts the named profile in `yolo.toml` waives.
    Profile(String),
}

/// How much a run is trusted with.
#[derive(Debug, InputObject)]
pub(crate) struct LaunchWrite {
    /// How much goes ahead without a person. Refused on a server started with
    /// `--no-remote-yolo`.
    pub(crate) unattended: Option<UnattendedWrite>,
    /// Tools approved for this run without asking. Refused on a server started
    /// with `--no-remote-yolo`.
    pub(crate) allow: Option<Vec<String>>,
    /// How deep the run's tree of child runs may grow, from 0 to 255. Absent
    /// takes the graph's own.
    pub(crate) max_depth: Option<i32>,
    /// Whether region seeds that run a shell command may run at spawn. Always
    /// off on a server started with `--no-remote-seed-commands`.
    #[graphql(default = true)]
    pub(crate) seed_commands: bool,
    /// Write this run's exact requests into its journal, once per provider
    /// attempt. A captured request is the whole prompt, and every call re-sends
    /// the window, so the journal grows by roughly the context size per
    /// attempt. Read it back on `InferenceAttempt.modelInput`.
    #[graphql(default = false)]
    pub(crate) capture_model_input: bool,
}

/// Where a run posts its events, and what signs them.
///
/// The secret sits inside the URL's own object, so a secret with nothing to
/// sign for cannot be written down at all.
#[derive(Debug, InputObject)]
pub(crate) struct CallbackWrite {
    /// The URL the daemon POSTs to when the run finishes. Checked against the
    /// same outbound policy a model-supplied URL is.
    pub(crate) url: String,
    /// Shared secret for signing that webhook body. Write-only: never read
    /// back on the run.
    pub(crate) secret: Option<String>,
}

/// Who hears about a run, and the caller's labels for it.
#[derive(Debug, InputObject)]
pub(crate) struct DeliveryWrite {
    /// Where to post when the run finishes.
    pub(crate) callback: Option<CallbackWrite>,
    /// Labels for whoever started the run, such as a ticket or a tenant. The
    /// run reads none of them, and the run search looks through them.
    pub(crate) metadata: Option<Vec<KeyValueWrite>>,
}

/// Everything about a new run: what to run, the values it takes, and how far
/// it is trusted.
///
/// `spawnRun` starts it and `validateSpawn` checks it without starting
/// anything; both take exactly this, so a request checked by one is the
/// request the other starts.
#[derive(Debug, InputObject)]
pub(crate) struct SpawnRunRequest {
    /// What to run.
    pub(crate) source: SpawnSourceWrite,
    /// Values for the graph's declared inputs, by name. `BlueprintOutput.inputs`
    /// lists what a blueprint takes.
    pub(crate) inputs: Option<Vec<InputEntryWrite>>,
    /// Files for `file` inputs, and parts placed straight into regions.
    pub(crate) attachments: Option<Vec<SpawnAttachmentWrite>>,
    /// A model to run every stage on that allows the operator's choice, as
    /// `provider/model` or a bare model name.
    pub(crate) model: Option<String>,
    /// The final-output shape the caller wants, over the graph's.
    pub(crate) output: Option<OutputShapeWrite>,
    /// Where the run's tools work. Defaults to this server's own directory,
    /// and is refused outside `--workdir-root` when the operator set one.
    pub(crate) workdir: Option<String>,
    /// How much the run is trusted with.
    pub(crate) launch: Option<LaunchWrite>,
    /// Who hears about the run, and the caller's labels for it.
    pub(crate) delivery: Option<DeliveryWrite>,
}

/// What a request is read with: the largest attachment this server reads.
pub(crate) struct Policy {
    /// The largest attachment, in bytes.
    pub(crate) max_bytes: u64,
}

/// One name read through its newtype, or an issue at `at`.
fn name<T>(
    issues: &mut SpawnIssues,
    at: SpecPath,
    read: Result<T, NameError>,
    got: &str,
) -> Option<T> {
    match read {
        Ok(value) => Some(value),
        Err(e) => {
            issues.push(SpawnIssue::new(at, IssueCode::Invalid, e.to_string()).got(got));
            None
        }
    }
}

/// The value as the runtime's reader takes it, with every duplicate field
/// name of a record reported.
fn raw(value: InputValueWrite, at: &SpecPath, issues: &mut SpawnIssues) -> RawInput {
    match value {
        InputValueWrite::Text(text) => RawInput::Text(text),
        InputValueWrite::Bool(flag) => RawInput::Bool(flag),
        InputValueWrite::Int(n) => RawInput::Int(n.0),
        InputValueWrite::Float(x) => RawInput::Float(x),
        InputValueWrite::List(items) => RawInput::List(
            items
                .into_iter()
                .enumerate()
                .map(|(i, item)| raw(item, &at.index(i), issues))
                .collect(),
        ),
        InputValueWrite::Record(fields) => RawInput::Record(entries(fields, at, issues)),
    }
}

/// Named values, keyed, with a name given twice reported at its second use.
fn entries(
    given: Vec<InputEntryWrite>,
    at: &SpecPath,
    issues: &mut SpawnIssues,
) -> BTreeMap<String, RawInput> {
    let mut out = BTreeMap::new();
    for entry in given {
        let here = at.key(&entry.name);
        let value = raw(entry.value, &here, issues);
        if out.insert(entry.name.clone(), value).is_some() {
            issues.push(
                SpawnIssue::new(here, IssueCode::Duplicate, "given twice")
                    .hint("give each name once"),
            );
        }
    }
    out
}

/// What an attachment refusal is, as an issue code.
fn attachment_code(e: &ServeError) -> IssueCode {
    match e {
        ServeError::Forbidden(_) => IssueCode::NotAllowed,
        ServeError::PayloadTooLarge(_) => IssueCode::OutOfRange,
        _ => IssueCode::Unresolvable,
    }
}

/// One attachment, read.
fn attachment(
    write: SpawnAttachmentWrite,
    at: &SpecPath,
    workdir: &Path,
    policy: &Policy,
    issues: &mut SpawnIssues,
) -> Option<Attachment> {
    let (default_name, data) = match write.content {
        AttachmentContentWrite::Path(path) => {
            match attachments::read_within(&path, workdir, policy.max_bytes) {
                Ok(part) => (Some(part.name), Some(part.data)),
                Err(e) => {
                    let code = attachment_code(&e);
                    issues.push(SpawnIssue::new(
                        at.field("content").field("path"),
                        code,
                        e.to_string(),
                    ));
                    (None, None)
                }
            }
        }
        AttachmentContentWrite::Base64(text) => {
            match serde_json::from_value::<Bytes>(serde_json::Value::String(text)) {
                Ok(Bytes(bytes)) if bytes.len() as u64 > policy.max_bytes => {
                    issues.push(
                        SpawnIssue::new(
                            at.field("content").field("base64"),
                            IssueCode::OutOfRange,
                            format!("{} bytes is over this server's limit", bytes.len()),
                        )
                        .expected(format!("at most {} bytes", policy.max_bytes)),
                    );
                    (None, None)
                }
                Ok(Bytes(bytes)) => (None, Some(bytes)),
                Err(e) => {
                    issues.push(SpawnIssue::new(
                        at.field("content").field("base64"),
                        IssueCode::Invalid,
                        format!("not {e}"),
                    ));
                    (None, None)
                }
            }
        }
    };
    let file_name = write.name.or(default_name);
    if file_name.is_none() && data.is_some() {
        issues.push(
            SpawnIssue::new(
                at.field("name"),
                IssueCode::Missing,
                "an attachment sent as base64 needs a name",
            )
            .hint("name it the way a `file` input will refer to it"),
        );
    }
    let mime_type = write.mime_type.and_then(|text| {
        name(
            issues,
            at.field("mime_type"),
            MimePattern::new(text.as_str()),
            &text,
        )
    });
    let region = write.region.and_then(|region| {
        name(
            issues,
            at.field("region"),
            RegionName::new(region.name.as_str()),
            &region.name,
        )
    });
    Some(Attachment {
        name: file_name?,
        mime_type,
        region,
        deliver: write.deliver.map(leviath_core::mime::Delivery::from),
        caption: write.caption,
        data: Bytes(data?),
    })
}

/// The graph's source, read.
fn source(write: SpawnSourceWrite, issues: &mut SpawnIssues) -> Option<SpawnSource> {
    let at = SpecPath::root().field("source");
    match write {
        SpawnSourceWrite::Blueprint(reference) => {
            let at = at.field("blueprint");
            let blueprint = name(
                issues,
                at.field("name"),
                BlueprintName::new(reference.name.as_str()),
                &reference.name,
            );
            let digest = match reference.digest {
                None => Some(None),
                Some(pin) => {
                    let pin = pin.trim().to_ascii_lowercase();
                    name(issues, at.field("digest"), Digest::new(pin.as_str()), &pin).map(Some)
                }
            };
            Some(SpawnSource::Blueprint(RuntimeRef {
                name: blueprint?,
                digest: digest?,
            }))
        }
        SpawnSourceWrite::Graph(graph) => match serde_json::from_value::<RunGraph>(graph.0) {
            Ok(graph) => Some(SpawnSource::Raw(Box::new(graph))),
            Err(e) => {
                issues.push(
                    SpawnIssue::new(at.field("graph"), IssueCode::Invalid, e.to_string())
                        .hint("GET /api/schema/spawn-request describes every key a graph takes"),
                );
                None
            }
        },
    }
}

/// The output shape, read.
fn output(write: OutputShapeWrite, issues: &mut SpawnIssues) -> OutputDef {
    let at = SpecPath::root().field("output");
    OutputDef {
        format: write.format,
        instructions: write.instructions,
        example: write.example,
        schema: write
            .schema
            .map(|schema| leviath_core::JsonDoc::new(schema.0)),
        validator: write.validator.map(|code| match code {
            CodeRefWrite::File(path) => CodeRef::File(path),
            CodeRefWrite::Inline(source) => CodeRef::Inline(source),
        }),
        on_validator_error: write.on_validator_error.map(|policy| match policy {
            ValidatorErrorPolicy::Reject => leviath_core::output::OnValidatorError::Reject,
            ValidatorErrorPolicy::Accept => leviath_core::output::OnValidatorError::Accept,
        }),
        overwrite_artifacts: write.overwrite_artifacts,
        artifacts: write
            .artifacts
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .filter_map(|(i, artifact)| {
                let mime_type = name(
                    issues,
                    at.field("artifacts").index(i).field("mime_type"),
                    MimePattern::new(artifact.mime_type.as_str()),
                    &artifact.mime_type,
                )?;
                Some(ArtifactDef {
                    name: artifact.name,
                    mime_type,
                    required: artifact.required,
                    description: artifact.description,
                })
            })
            .collect(),
    }
}

/// The launch settings, read.
fn launch(write: LaunchWrite, issues: &mut SpawnIssues) -> LaunchRequest {
    let at = SpecPath::root().field("launch");
    let unattended = match write.unattended {
        None | Some(UnattendedWrite::All(false)) => Unattended::Off,
        Some(UnattendedWrite::All(true)) => Unattended::All,
        Some(UnattendedWrite::Profile(profile)) => name(
            issues,
            at.field("unattended").field("profile"),
            ProfileName::new(profile.as_str()),
            &profile,
        )
        .map_or(Unattended::Off, Unattended::Profile),
    };
    let allow_text = write.allow.unwrap_or_default();
    let allow = allow_text
        .iter()
        .enumerate()
        .filter_map(|(i, tool)| {
            name(
                issues,
                at.field("allow").index(i),
                ToolName::new(tool.as_str()),
                tool,
            )
        })
        .collect();
    let max_depth = write.max_depth.and_then(|depth| match u8::try_from(depth) {
        Ok(depth) => Some(depth),
        Err(_) => {
            issues.push(
                SpawnIssue::new(
                    at.field("max_depth"),
                    IssueCode::OutOfRange,
                    "the depth is out of range",
                )
                .expected("0 to 255")
                .got(depth.to_string()),
            );
            None
        }
    });
    LaunchRequest {
        unattended,
        allow,
        max_depth,
        seed_commands: write.seed_commands,
        capture_model_input: write.capture_model_input,
    }
}

/// The delivery settings, read.
fn delivery(write: DeliveryWrite, issues: &mut SpawnIssues) -> RunDelivery {
    let at = SpecPath::root().field("delivery");
    let callback = write.callback.and_then(|callback| {
        let at = at.field("callback").field("url");
        let url = name(
            issues,
            at,
            HttpUrl::new(callback.url.as_str()),
            &callback.url,
        )?;
        Some(Callback {
            url,
            secret: callback.secret.map(Secret::new),
        })
    });
    RunDelivery {
        callback,
        metadata: write
            .metadata
            .unwrap_or_default()
            .into_iter()
            .map(|entry| (entry.key, entry.value))
            .collect(),
    }
}

/// `workdir` made absolute against this server's own directory.
fn absolute(workdir: Option<String>) -> PathBuf {
    let here = std::env::current_dir().unwrap_or_default();
    match workdir {
        None => here,
        Some(dir) => here.join(dir),
    }
}

impl SpawnRunRequest {
    /// The runtime's request with every value that did not read left out,
    /// beside the issues with those values; or only the issues, when the
    /// request names no source to check the rest against. This server's own
    /// refusals (the workdir root, unattended runs, the callback policy) are
    /// the service layer's, made on the request this answers with.
    pub(crate) fn read(self, policy: &Policy) -> Result<(SpawnRequest, SpawnIssues), SpawnIssues> {
        let mut issues = SpawnIssues::new();
        let workdir = absolute(self.workdir);
        let source = source(self.source, &mut issues);
        let inputs = entries(
            self.inputs.unwrap_or_default(),
            &SpecPath::root().field("inputs"),
            &mut issues,
        );
        let attachments: Vec<Attachment> = self
            .attachments
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .filter_map(|(i, write)| {
                let at = SpecPath::root().field("attachments").index(i);
                attachment(write, &at, &workdir, policy, &mut issues)
            })
            .collect();
        let model = self.model.and_then(|text| {
            name(
                &mut issues,
                SpecPath::root().field("model"),
                ModelRef::parse(&text),
                &text,
            )
        });
        let output = self.output.map(|write| output(write, &mut issues));
        let launch = launch(
            self.launch.unwrap_or(LaunchWrite {
                unattended: None,
                allow: None,
                max_depth: None,
                seed_commands: true,
                capture_model_input: false,
            }),
            &mut issues,
        );
        let delivery = self
            .delivery
            .map(|write| delivery(write, &mut issues))
            .unwrap_or_default();
        let Some(source) = source else {
            return Err(issues);
        };
        let request = SpawnRequest {
            source,
            inputs,
            attachments,
            model,
            output,
            workdir: Some(workdir),
            launch,
            delivery,
        };
        Ok((request, issues))
    }
}

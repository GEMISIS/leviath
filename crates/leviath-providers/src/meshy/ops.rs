//! The Meshy operations, and the pure logic that turns an inference request
//! into a Meshy REST body and reads a finished task back.
//!
//! Kept apart from the provider itself because it is a different subject and
//! because it is where the testable decisions live: which images and hints a
//! request carries, what the create body looks like per operation, and where
//! a finished task keeps its GLB. The provider around it is the thin HTTP
//! orchestration (submit, poll, download) over these.

use crate::media::{
    data_uris as mime_data_uris, extra_bool, extra_f64, extra_i64, extra_str, request_text,
};
use serde_json::{Map, Value, json};

use crate::capabilities::ModelMime;
use crate::provider::{InferenceRequest, ProviderError, Result};

/// The most texture-prompt characters Meshy accepts.
const MAX_TEXTURE_PROMPT: usize = 800;
/// The most reference images a multi-image task takes.
const MAX_MULTI_IMAGES: usize = 4;
/// The default character height a rig assumes, in meters.
const DEFAULT_RIG_HEIGHT_METERS: f64 = 1.7;

/// One Meshy generative operation, named by the model id a stage selects.
///
/// A stage runs a Meshy operation by naming it as its model: `provider =
/// "meshy"`, `model = "multi-image-to-3d"`. Each operation reads a different
/// input (one image, several images, or a mesh) and every one produces a
/// `model/gltf-binary` part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MeshyOp {
    /// A text prompt to a textured mesh (preview then refine).
    TextTo3d,
    /// One reference image to a textured mesh.
    ImageTo3d,
    /// Up to four reference views to a single textured mesh.
    MultiImageTo3d,
    /// An existing mesh plus a text style to a re-textured mesh.
    Retexture,
    /// An existing mesh to a rigged, animation-ready mesh.
    Rig,
    /// An existing mesh to an animated mesh (rig, then apply an action).
    Animate,
}

impl MeshyOp {
    /// The operation a model id names, matching the whole id or its last
    /// segment so `meshy/rig` and `rig` both resolve.
    pub(crate) fn parse(model: &str) -> Option<Self> {
        let id = model.rsplit('/').next().unwrap_or(model);
        match id {
            "text-to-3d" => Some(Self::TextTo3d),
            "image-to-3d" => Some(Self::ImageTo3d),
            "multi-image-to-3d" => Some(Self::MultiImageTo3d),
            "retexture" => Some(Self::Retexture),
            "rig" => Some(Self::Rig),
            "animate" => Some(Self::Animate),
            _ => None,
        }
    }

    /// The canonical model id for this operation.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::TextTo3d => "text-to-3d",
            Self::ImageTo3d => "image-to-3d",
            Self::MultiImageTo3d => "multi-image-to-3d",
            Self::Retexture => "retexture",
            Self::Rig => "rig",
            Self::Animate => "animate",
        }
    }

    /// The path under the base URL that creates and lists this operation's
    /// first task.
    ///
    /// The same path serves the POST that creates a task and, with the task id
    /// appended, the GET that reads its status. For a two-phase operation this
    /// is the first phase's path (both text-to-3d phases share one endpoint;
    /// animate's first phase is a rig, and its animate phase uses the
    /// animations path directly).
    pub(crate) fn path(self) -> &'static str {
        match self {
            Self::TextTo3d => "openapi/v2/text-to-3d",
            Self::ImageTo3d => "openapi/v1/image-to-3d",
            Self::MultiImageTo3d => "openapi/v1/multi-image-to-3d",
            Self::Retexture => "openapi/v1/retexture",
            Self::Rig | Self::Animate => "openapi/v1/rigging",
        }
    }

    /// What the operation takes and hands back, in mime patterns.
    pub(crate) fn mime(self) -> ModelMime {
        match self {
            // A text prompt in, a mesh (with a preview render) out.
            Self::TextTo3d => ModelMime::new(&["text/*"], &["model/gltf-binary", "image/*"]),
            // The reference images plus an optional text texture prompt in;
            // a mesh (and, for multi-image, the preview renders) out.
            Self::ImageTo3d => {
                ModelMime::new(&["text/*", "image/*"], &["model/gltf-binary", "image/*"])
            }
            Self::MultiImageTo3d => {
                ModelMime::new(&["text/*", "image/*"], &["model/gltf-binary", "image/*"])
            }
            // A mesh plus a text style (or a style image) in, the re-textured
            // mesh out.
            Self::Retexture => ModelMime::new(
                &["text/*", "image/*", "model/gltf-binary"],
                &["model/gltf-binary"],
            ),
            // A mesh in, the rigged mesh out.
            Self::Rig => ModelMime::new(&["model/gltf-binary"], &["model/gltf-binary"]),
            // A mesh plus an optional action name in, the animated mesh out.
            Self::Animate => {
                ModelMime::new(&["text/*", "model/gltf-binary"], &["model/gltf-binary"])
            }
        }
    }

    /// The name the produced mesh part carries.
    pub(crate) fn output_name(self) -> &'static str {
        match self {
            Self::TextTo3d | Self::ImageTo3d | Self::MultiImageTo3d => "model.glb",
            Self::Retexture => "retextured.glb",
            Self::Rig => "rigged.glb",
            Self::Animate => "animated.glb",
        }
    }

    /// The Meshy create body for this operation, read from the request's
    /// hydrated parts and its `[model.parameters]` hints.
    ///
    /// An error here is a request the operation cannot run: an image
    /// operation with no image, a rig with no mesh. Naming the miss beats
    /// letting Meshy reject an empty body with a generic 400.
    pub(crate) fn build_body(self, request: &InferenceRequest) -> Result<Value> {
        match self {
            Self::ImageTo3d => {
                let image = input_images(request).into_iter().next().ok_or_else(|| {
                    ProviderError::InvalidResponse(
                        "image-to-3d needs an image in a visible region, and found none".into(),
                    )
                })?;
                let mut body = Map::new();
                body.insert("image_url".into(), json!(image));
                apply_texture(&mut body, request);
                apply_model_hints(&mut body, request);
                apply_texture_hints(&mut body, request);
                Ok(Value::Object(body))
            }
            Self::MultiImageTo3d => {
                let images: Vec<String> = input_images(request)
                    .into_iter()
                    .take(MAX_MULTI_IMAGES)
                    .collect();
                if images.is_empty() {
                    return Err(ProviderError::InvalidResponse(
                        "multi-image-to-3d needs at least one image in a visible region, and \
                         found none"
                            .into(),
                    ));
                }
                let mut body = Map::new();
                body.insert("image_urls".into(), json!(images));
                // The four cardinal preview renders come back beside the mesh,
                // so a downstream stage can judge the model without a headless
                // render of its own.
                body.insert("multi_view_thumbnails".into(), json!(true));
                apply_texture(&mut body, request);
                apply_model_hints(&mut body, request);
                apply_texture_hints(&mut body, request);
                Ok(Value::Object(body))
            }
            // The preview phase of text-to-3d: geometry from the prompt, no
            // texturing yet (the refine phase textures it).
            Self::TextTo3d => {
                let prompt = required_prompt(request, "text-to-3d")?;
                let mut body = Map::new();
                body.insert("mode".into(), json!("preview"));
                body.insert("prompt".into(), json!(prompt));
                apply_model_hints(&mut body, request);
                Ok(Value::Object(body))
            }
            Self::Retexture => {
                let mesh = input_model(request).ok_or_else(|| {
                    ProviderError::InvalidResponse(
                        "retexture needs a model/gltf-binary mesh in a visible region, and found \
                         none"
                            .into(),
                    )
                })?;
                let style = required_prompt(request, "retexture")?;
                let mut body = Map::new();
                body.insert("model_url".into(), json!(mesh));
                body.insert("text_style_prompt".into(), json!(style));
                if let Some(model) = extra_str(request, "ai_model") {
                    body.insert("ai_model".into(), json!(model));
                }
                apply_texture_hints(&mut body, request);
                Ok(Value::Object(body))
            }
            // Rig, and animate's first phase which is a rig: a mesh in.
            Self::Rig | Self::Animate => {
                let mesh = input_model(request).ok_or_else(|| {
                    ProviderError::InvalidResponse(format!(
                        "{} needs a model/gltf-binary mesh in a visible region, and found none",
                        self.id()
                    ))
                })?;
                let mut body = Map::new();
                body.insert("model_url".into(), json!(mesh));
                let height =
                    extra_f64(request, "height_meters").unwrap_or(DEFAULT_RIG_HEIGHT_METERS);
                body.insert("height_meters".into(), json!(height));
                Ok(Value::Object(body))
            }
        }
    }

    /// The refine-phase body of text-to-3d, texturing the preview task.
    pub(crate) fn text_refine_body(preview_task_id: &str, request: &InferenceRequest) -> Value {
        let mut body = Map::new();
        body.insert("mode".into(), json!("refine"));
        body.insert("preview_task_id".into(), json!(preview_task_id));
        apply_texture(&mut body, request);
        apply_texture_hints(&mut body, request);
        Value::Object(body)
    }

    /// The animate-phase body: the rigged task plus the chosen action.
    pub(crate) fn animate_body(rig_task_id: &str, action_id: i64) -> Value {
        json!({ "rig_task_id": rig_task_id, "action_id": action_id })
    }

    /// The GLB url of a finished task, or `None` when the task carries no
    /// mesh (a shape a rig and a generation express differently).
    pub(crate) fn glb_url(self, task: &Value) -> Option<String> {
        let url = match self {
            Self::TextTo3d | Self::ImageTo3d | Self::MultiImageTo3d | Self::Retexture => {
                task.get("model_urls")?.get("glb")?
            }
            Self::Rig => task.get("result")?.get("rigged_character_glb_url")?,
            Self::Animate => task.get("result")?.get("animation_glb_url")?,
        };
        url.as_str().filter(|s| !s.is_empty()).map(str::to_string)
    }

    /// The preview-render url of a finished task, when it has one to show.
    ///
    /// A rig has no new render; a generation's front-view thumbnail is a
    /// cheap image a verify stage can look at.
    pub(crate) fn preview_url(self, task: &Value) -> Option<String> {
        match self {
            Self::TextTo3d | Self::ImageTo3d | Self::MultiImageTo3d | Self::Retexture => task
                .get("thumbnail_url")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            Self::Rig | Self::Animate => None,
        }
    }
}

/// The text a generation or retexture must have, capped at Meshy's limit.
///
/// A text-to-3d with no prompt or a retexture with no style is a request Meshy
/// cannot run; naming the miss beats a generic 400.
fn required_prompt(request: &InferenceRequest, op: &str) -> Result<String> {
    let prompt: String = request_text(request)
        .chars()
        .take(MAX_TEXTURE_PROMPT)
        .collect();
    if prompt.trim().is_empty() {
        return Err(ProviderError::InvalidResponse(format!(
            "{op} needs a text prompt in a visible region, and found none"
        )));
    }
    Ok(prompt)
}

/// The animation action to apply: the `action` hint a stage set in
/// `[model.parameters]`, else the request text, defaulting to a walk.
pub(crate) fn animate_action(request: &InferenceRequest) -> String {
    let action = extra_str(request, "action").unwrap_or_else(|| request_text(request));
    match action.trim().is_empty() {
        true => "walk".to_string(),
        false => action.trim().to_string(),
    }
}

/// The first `action_id` in an animation-library listing, when it has one.
pub(crate) fn library_action_id(library: &Value) -> Option<i64> {
    library.as_array()?.first()?.get("action_id")?.as_i64()
}

/// The task id a create response reports, under its `result` key.
pub(crate) fn created_task_id(create_response: &Value) -> Result<String> {
    create_response
        .get("result")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            ProviderError::InvalidResponse(format!(
                "Meshy create response carried no task id: {create_response}"
            ))
        })
}

/// Where a polled task is: its status word and how far along it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskState {
    /// Queued or running, at this percentage.
    Running(u64),
    /// Finished; the mesh is ready to read.
    Succeeded,
    /// Ended without a mesh; the string is Meshy's reason, when it gave one.
    Failed(String),
}

/// Read a polled task's state from its status body.
///
/// An unknown status word is treated as still running rather than as a
/// failure: Meshy adding a transitional state should not abort a run that
/// would have finished, and the operation's own deadline still bounds it.
pub(crate) fn task_state(task: &Value) -> TaskState {
    match task.get("status").and_then(Value::as_str) {
        Some("SUCCEEDED") => TaskState::Succeeded,
        Some("FAILED") | Some("CANCELED") => {
            let reason = task
                .get("task_error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("Meshy reported no reason")
                .to_string();
            TaskState::Failed(reason)
        }
        _ => {
            let progress = task.get("progress").and_then(Value::as_u64).unwrap_or(0);
            TaskState::Running(progress)
        }
    }
}

/// The hydrated image parts of a request, as `data:` URIs Meshy accepts.
///
/// Only a mime block carrying its bytes counts: an unhydrated block (empty
/// `data`) is one the runtime chose to send as text, which Meshy cannot use.
fn input_images(request: &InferenceRequest) -> Vec<String> {
    mime_data_uris(request, |mime| mime.starts_with("image/"))
}

/// The first hydrated mesh part of a request, as a `data:` URI, when it has
/// one.
fn input_model(request: &InferenceRequest) -> Option<String> {
    mime_data_uris(request, |mime| mime.starts_with("model/"))
        .into_iter()
        .next()
}

/// Add the texture prompt to a create body, from the explicit hint or, when
/// that is unset, the request text, capped at Meshy's limit.
fn apply_texture(body: &mut Map<String, Value>, request: &InferenceRequest) {
    let prompt = extra_str(request, "texture_prompt").unwrap_or_else(|| request_text(request));
    let prompt: String = prompt.trim().chars().take(MAX_TEXTURE_PROMPT).collect();
    if !prompt.is_empty() {
        body.insert("texture_prompt".into(), json!(prompt));
    }
}

/// Copy the geometry and model-tier hints a stage set in `[model.parameters]`
/// into a create body, each only when present.
///
/// These are valid on every phase that produces geometry - image and text
/// generation, and a text-to-3d preview. Only fields Meshy documents are
/// forwarded, so a stale or provider-neutral hint (a deprecated `symmetry_mode`,
/// a `negative_prompt` these endpoints do not take) is dropped here rather than
/// drawing a 400 from Meshy.
fn apply_model_hints(body: &mut Map<String, Value>, request: &InferenceRequest) {
    for key in ["ai_model", "topology", "pose_mode"] {
        if let Some(value) = extra_str(request, key) {
            body.insert(key.into(), json!(value));
        }
    }
    if let Some(count) = extra_i64(request, "target_polycount") {
        body.insert("target_polycount".into(), json!(count));
    }
    for key in ["should_remesh", "ultra_mode", "moderation"] {
        if let Some(value) = extra_bool(request, key) {
            body.insert(key.into(), json!(value));
        }
    }
}

/// Copy the texturing hints into a create body, each only when present.
///
/// Kept apart from the geometry hints because a text-to-3d preview textures
/// nothing and rejects them; they belong to the image ops, the refine phase and
/// a retexture.
fn apply_texture_hints(body: &mut Map<String, Value>, request: &InferenceRequest) {
    if let Some(value) = extra_str(request, "texture_resolution") {
        body.insert("texture_resolution".into(), json!(value));
    }
    if let Some(value) = extra_bool(request, "enable_pbr") {
        body.insert("enable_pbr".into(), json!(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ContentBlock, InferenceRequest, Message, MessageContent};
    use leviath_core::mime::{BlobRef, MimeType};

    fn empty_request() -> InferenceRequest {
        InferenceRequest {
            system: Vec::new(),
            messages: Vec::new(),
            model: "multi-image-to-3d".into(),
            max_tokens: 0,
            temperature: 0.0,
            tools: Vec::new(),
            extra: Value::Null,
            request_timeout_secs: None,
        }
    }

    fn hydrated_block(mime: &str, name: &str, data: &str) -> ContentBlock {
        ContentBlock::Mime {
            part: BlobRef {
                sha256: "a".repeat(64),
                mime_type: MimeType::parse(mime).unwrap(),
                size: 3,
                width: None,
                height: None,
                duration_ms: None,
                tokens: 1,
                stand_in: format!("[{mime}] {name}"),
            },
            data: data.into(),
            name: Some(name.into()),
            deliver: None,
            remote: None,
        }
    }

    fn with_blocks(blocks: Vec<ContentBlock>) -> InferenceRequest {
        let mut request = empty_request();
        request.messages = vec![Message {
            role: "user".into(),
            content: MessageContent::Blocks(blocks),
            cache_breakpoint: false,
            reasoning: None,
        }];
        request
    }

    const ALL_OPS: [MeshyOp; 6] = [
        MeshyOp::TextTo3d,
        MeshyOp::ImageTo3d,
        MeshyOp::MultiImageTo3d,
        MeshyOp::Retexture,
        MeshyOp::Rig,
        MeshyOp::Animate,
    ];

    #[test]
    fn parse_reads_the_id_or_its_last_segment() {
        assert_eq!(MeshyOp::parse("image-to-3d"), Some(MeshyOp::ImageTo3d));
        assert_eq!(
            MeshyOp::parse("meshy/multi-image-to-3d"),
            Some(MeshyOp::MultiImageTo3d)
        );
        assert_eq!(MeshyOp::parse("rig"), Some(MeshyOp::Rig));
        assert_eq!(MeshyOp::parse("text-to-3d"), Some(MeshyOp::TextTo3d));
        assert_eq!(MeshyOp::parse("retexture"), Some(MeshyOp::Retexture));
        assert_eq!(MeshyOp::parse("meshy/animate"), Some(MeshyOp::Animate));
        assert_eq!(MeshyOp::parse("sculpt"), None);
    }

    #[test]
    fn each_operation_names_its_path_id_display_output_and_mime() {
        for op in ALL_OPS {
            assert!(!op.path().is_empty());
            assert_eq!(MeshyOp::parse(op.id()), Some(op));
            assert!(op.output_name().ends_with(".glb"));
            assert!(!op.mime().output.is_empty());
        }
        assert!(
            MeshyOp::Rig
                .mime()
                .input
                .iter()
                .all(|p| !p.starts_with("text/")),
            "a rig takes a mesh, not text"
        );
    }

    #[test]
    fn image_to_3d_builds_a_single_image_body_with_the_texture_prompt() {
        let mut request = with_blocks(vec![
            ContentBlock::Text {
                text: "a red fox".into(),
            },
            hydrated_block("image/png", "front.png", "QUJD"),
        ]);
        request.extra = json!({ "ai_model": "meshy-7", "target_polycount": 20000 });
        let body = MeshyOp::ImageTo3d.build_body(&request).unwrap();
        assert_eq!(
            body["image_url"].as_str().unwrap(),
            "data:image/png;base64,QUJD"
        );
        assert_eq!(body["texture_prompt"].as_str().unwrap(), "a red fox");
        assert_eq!(body["ai_model"].as_str().unwrap(), "meshy-7");
        assert_eq!(body["target_polycount"].as_i64().unwrap(), 20000);
    }

    #[test]
    fn an_explicit_texture_prompt_beats_the_request_text_and_is_capped() {
        let long = "x".repeat(1000);
        let mut request = with_blocks(vec![
            ContentBlock::Text {
                text: "ignored region text".into(),
            },
            hydrated_block("image/jpeg", "a.jpg", "QQ"),
        ]);
        request.extra = json!({ "texture_prompt": long });
        let body = MeshyOp::ImageTo3d.build_body(&request).unwrap();
        assert_eq!(
            body["texture_prompt"].as_str().unwrap().chars().count(),
            800
        );
    }

    #[test]
    fn multi_image_takes_up_to_four_images_and_asks_for_preview_renders() {
        let blocks: Vec<ContentBlock> = (0..6)
            .map(|i| hydrated_block("image/png", &format!("v{i}.png"), "QQ"))
            .collect();
        let body = MeshyOp::MultiImageTo3d
            .build_body(&with_blocks(blocks))
            .unwrap();
        assert_eq!(body["image_urls"].as_array().unwrap().len(), 4);
        assert!(body["multi_view_thumbnails"].as_bool().unwrap());
    }

    #[test]
    fn an_image_operation_with_no_image_is_a_named_error() {
        let err = MeshyOp::ImageTo3d.build_body(&empty_request()).unwrap_err();
        assert!(
            err.to_string().contains("image-to-3d needs an image"),
            "{err}"
        );
        let err = MeshyOp::MultiImageTo3d
            .build_body(&empty_request())
            .unwrap_err();
        assert!(err.to_string().contains("multi-image-to-3d needs"), "{err}");
    }

    #[test]
    fn rig_reads_the_mesh_and_the_height_hint() {
        let mut request = with_blocks(vec![hydrated_block("model/gltf-binary", "m.glb", "R0xC")]);
        request.extra = json!({ "height_meters": 1.9 });
        let body = MeshyOp::Rig.build_body(&request).unwrap();
        assert_eq!(
            body["model_url"].as_str().unwrap(),
            "data:model/gltf-binary;base64,R0xC"
        );
        assert_eq!(body["height_meters"].as_f64().unwrap(), 1.9);
        // A rig ignores an image where its mesh should be.
        let err = MeshyOp::Rig
            .build_body(&with_blocks(vec![hydrated_block(
                "image/png",
                "a.png",
                "QQ",
            )]))
            .unwrap_err();
        assert!(
            err.to_string().contains("rig needs a model/gltf-binary"),
            "{err}"
        );
        // With no height hint it assumes the default.
        let body = MeshyOp::Rig
            .build_body(&with_blocks(vec![hydrated_block(
                "model/gltf-binary",
                "m.glb",
                "R0xC",
            )]))
            .unwrap();
        assert_eq!(body["height_meters"].as_f64().unwrap(), 1.7);
    }

    #[test]
    fn an_unhydrated_or_wrongly_typed_block_supplies_no_input() {
        // An image block with empty data (the runtime sent it as text) does
        // not count as an image.
        let dry = hydrated_block("image/png", "a.png", "");
        assert!(
            MeshyOp::ImageTo3d
                .build_body(&with_blocks(vec![dry]))
                .is_err(),
            "an unhydrated image is no image"
        );
    }

    #[test]
    fn created_task_id_reads_the_result_or_reports_the_body() {
        assert_eq!(
            created_task_id(&json!({ "result": "task-1" })).unwrap(),
            "task-1"
        );
        let err = created_task_id(&json!({ "result": "" })).unwrap_err();
        assert!(err.to_string().contains("no task id"), "{err}");
        assert!(created_task_id(&json!({ "other": 1 })).is_err());
    }

    #[test]
    fn task_state_reads_the_status_word() {
        assert_eq!(
            task_state(&json!({ "status": "PENDING", "progress": 10 })),
            TaskState::Running(10)
        );
        assert_eq!(
            task_state(&json!({ "status": "SUCCEEDED" })),
            TaskState::Succeeded
        );
        assert_eq!(
            task_state(&json!({ "status": "IN_PROGRESS" })),
            TaskState::Running(0)
        );
        // An unknown word keeps polling rather than failing the run.
        assert_eq!(
            task_state(&json!({ "status": "QUEUED_SOMEHOW" })),
            TaskState::Running(0)
        );
        assert_eq!(
            task_state(&json!({ "status": "FAILED", "task_error": { "message": "bad mesh" } })),
            TaskState::Failed("bad mesh".into())
        );
        // A cancel with no message reports a placeholder reason.
        assert_eq!(
            task_state(&json!({ "status": "CANCELED" })),
            TaskState::Failed("Meshy reported no reason".into())
        );
    }

    #[test]
    fn glb_and_preview_urls_read_the_right_shape_per_operation() {
        let generated = json!({
            "model_urls": { "glb": "https://a/m.glb" },
            "thumbnail_url": "https://a/t.png"
        });
        assert_eq!(
            MeshyOp::MultiImageTo3d.glb_url(&generated).unwrap(),
            "https://a/m.glb"
        );
        assert_eq!(
            MeshyOp::MultiImageTo3d.preview_url(&generated).unwrap(),
            "https://a/t.png"
        );
        let rig = json!({ "result": { "rigged_character_glb_url": "https://a/r.glb" } });
        assert_eq!(MeshyOp::Rig.glb_url(&rig).unwrap(), "https://a/r.glb");
        assert_eq!(MeshyOp::Rig.preview_url(&rig), None);
        // A body missing the mesh answers None rather than a wrong url.
        assert_eq!(MeshyOp::MultiImageTo3d.glb_url(&json!({})), None);
        assert_eq!(MeshyOp::Rig.glb_url(&json!({ "result": {} })), None);
        assert_eq!(MeshyOp::Rig.glb_url(&json!({})), None);
        assert_eq!(MeshyOp::MultiImageTo3d.preview_url(&json!({})), None);
    }

    #[test]
    fn an_image_in_a_later_message_is_read_past_a_plain_text_one() {
        let mut request = empty_request();
        request.messages = vec![
            Message {
                role: "user".into(),
                content: MessageContent::Text("the task, as plain text".into()),
                cache_breakpoint: false,
                reasoning: None,
            },
            Message {
                role: "user".into(),
                content: MessageContent::Blocks(vec![hydrated_block("image/png", "v.png", "QQ")]),
                cache_breakpoint: false,
                reasoning: None,
            },
        ];
        let body = MeshyOp::ImageTo3d.build_body(&request).unwrap();
        assert_eq!(
            body["image_url"].as_str().unwrap(),
            "data:image/png;base64,QQ"
        );
    }

    #[test]
    fn request_text_joins_text_blocks_and_plain_messages() {
        let mut request = empty_request();
        request.messages = vec![
            Message {
                role: "user".into(),
                content: MessageContent::Text("plain".into()),
                cache_breakpoint: false,
                reasoning: None,
            },
            Message {
                role: "user".into(),
                content: MessageContent::Blocks(vec![ContentBlock::Text {
                    text: "block".into(),
                }]),
                cache_breakpoint: false,
                reasoning: None,
            },
        ];
        assert_eq!(request_text(&request), "plain\nblock");
    }

    #[test]
    fn request_text_excludes_a_stored_parts_pointer() {
        // Assembly emits a stored part as bytes plus a pointer text carrying its
        // stand-in. That pointer must not be read as the prompt or the action,
        // whether it lands as a plain-text message or a text block.
        let mut request = with_blocks(vec![
            ContentBlock::Text {
                text: "walk".into(),
            },
            hydrated_block("model/gltf-binary", "m.glb", "R0xC"),
            ContentBlock::Text {
                text: "[source] [model/gltf-binary] m.glb".into(),
            },
        ]);
        request.messages.push(Message {
            role: "user".into(),
            content: MessageContent::Text("[model/gltf-binary] m.glb".into()),
            cache_breakpoint: false,
            reasoning: None,
        });
        assert_eq!(request_text(&request), "walk");
        assert_eq!(animate_action(&request), "walk");
    }

    /// A system block the way assembly renders a pinned region: its text
    /// under a `## <region>` heading, tagged with the region's name.
    fn system_block(region: &str, text: &str) -> crate::SystemBlock {
        crate::SystemBlock {
            text: text.to_string(),
            cache_hint: leviath_core::CacheHint::Always,
            region: region.to_string(),
            volatility: leviath_core::Volatility::default(),
        }
    }

    #[test]
    fn request_text_falls_back_to_the_pinned_regions_in_the_system_prompt() {
        // The bundled model-to-animated-model puts the action in a pinned
        // `task` region, which assembly renders into the system prompt; the
        // messages carry only the lifted mesh. The action was invisible here,
        // so every such run animated the default walk.
        let mut request = with_blocks(vec![hydrated_block("model/gltf-binary", "m.glb", "R0xC")]);
        request.system = vec![
            system_block(
                "stage_instructions",
                "## stage_instructions\n[Stage instructions: call submit_output]",
            ),
            system_block("", "a hint that came from no region"),
            system_block("source_model", "## source_model\n[model/gltf-binary] m.glb"),
            system_block("task", "## task\nIdle 1"),
        ];
        assert_eq!(request_text(&request), "Idle 1");
        assert_eq!(animate_action(&request), "Idle 1");
        // A block without the heading is taken whole.
        request.system = vec![system_block("notes", "matte clay")];
        assert_eq!(request_text(&request), "matte clay");
    }

    #[test]
    fn message_text_outranks_the_system_prompt() {
        let mut request = with_blocks(vec![ContentBlock::Text {
            text: "walk".into(),
        }]);
        request.system = vec![system_block("task", "## task\nIdle 1")];
        assert_eq!(request_text(&request), "walk");
    }

    #[test]
    fn the_action_hint_outranks_every_region() {
        let mut request = with_blocks(vec![ContentBlock::Text {
            text: "walk".into(),
        }]);
        request.extra = json!({ "action": "Idle 3" });
        assert_eq!(animate_action(&request), "Idle 3");
    }

    #[test]
    fn common_hints_forward_only_present_documented_fields() {
        let mut request = with_blocks(vec![hydrated_block("image/png", "a.png", "QQ")]);
        request.extra = json!({
            "enable_pbr": true,
            "should_remesh": false,
            "negative_prompt": "blurry",
            "symmetry_mode": "on"
        });
        let body = MeshyOp::ImageTo3d.build_body(&request).unwrap();
        assert!(body["enable_pbr"].as_bool().unwrap());
        assert!(!body["should_remesh"].as_bool().unwrap());
        // A field Meshy does not document for this endpoint is dropped.
        assert!(body.get("negative_prompt").is_none());
        assert!(body.get("symmetry_mode").is_none());
    }

    #[test]
    fn text_to_3d_builds_a_preview_body_and_needs_a_prompt() {
        let mut request = with_blocks(vec![ContentBlock::Text {
            text: "a brass robot".into(),
        }]);
        request.extra = json!({ "target_polycount": 15000, "enable_pbr": true });
        let body = MeshyOp::TextTo3d.build_body(&request).unwrap();
        assert_eq!(body["mode"].as_str().unwrap(), "preview");
        assert_eq!(body["prompt"].as_str().unwrap(), "a brass robot");
        assert_eq!(body["target_polycount"].as_i64().unwrap(), 15000);
        // A preview textures nothing, so texture hints are withheld.
        assert!(body.get("enable_pbr").is_none());
        let err = MeshyOp::TextTo3d.build_body(&empty_request()).unwrap_err();
        assert!(
            err.to_string().contains("text-to-3d needs a text prompt"),
            "{err}"
        );
    }

    #[test]
    fn text_refine_body_textures_the_preview_task() {
        let request = with_blocks(vec![ContentBlock::Text {
            text: "matte red enamel".into(),
        }]);
        let body = MeshyOp::text_refine_body("preview-1", &request);
        assert_eq!(body["mode"].as_str().unwrap(), "refine");
        assert_eq!(body["preview_task_id"].as_str().unwrap(), "preview-1");
        assert_eq!(body["texture_prompt"].as_str().unwrap(), "matte red enamel");
    }

    #[test]
    fn retexture_needs_a_mesh_and_a_style() {
        let mut request = with_blocks(vec![
            ContentBlock::Text {
                text: "weathered bronze".into(),
            },
            hydrated_block("model/gltf-binary", "m.glb", "R0xC"),
        ]);
        request.extra = json!({ "texture_resolution": "4k", "ai_model": "meshy-7" });
        let body = MeshyOp::Retexture.build_body(&request).unwrap();
        assert_eq!(
            body["model_url"].as_str().unwrap(),
            "data:model/gltf-binary;base64,R0xC"
        );
        assert_eq!(
            body["text_style_prompt"].as_str().unwrap(),
            "weathered bronze"
        );
        assert_eq!(body["texture_resolution"].as_str().unwrap(), "4k");
        assert_eq!(body["ai_model"].as_str().unwrap(), "meshy-7");
        let no_mesh = with_blocks(vec![ContentBlock::Text { text: "x".into() }]);
        assert!(
            MeshyOp::Retexture
                .build_body(&no_mesh)
                .unwrap_err()
                .to_string()
                .contains("retexture needs a model")
        );
        let no_style = with_blocks(vec![hydrated_block("model/gltf-binary", "m.glb", "R0xC")]);
        assert!(
            MeshyOp::Retexture
                .build_body(&no_style)
                .unwrap_err()
                .to_string()
                .contains("retexture needs a text prompt")
        );
    }

    #[test]
    fn animate_rigs_first_and_reads_the_action_and_library() {
        // animate's build_body is a rig body (its first phase is a rig).
        let request = with_blocks(vec![hydrated_block("model/gltf-binary", "m.glb", "R0xC")]);
        let body = MeshyOp::Animate.build_body(&request).unwrap();
        assert_eq!(
            body["model_url"].as_str().unwrap(),
            "data:model/gltf-binary;base64,R0xC"
        );
        assert_eq!(body["height_meters"].as_f64().unwrap(), 1.7);
        assert!(
            MeshyOp::Animate
                .build_body(&empty_request())
                .unwrap_err()
                .to_string()
                .contains("animate needs a model")
        );

        // The action defaults to a walk, or reads the request text.
        assert_eq!(animate_action(&empty_request()), "walk");
        assert_eq!(
            animate_action(&with_blocks(vec![ContentBlock::Text {
                text: "run".into()
            }])),
            "run"
        );

        // animate_body pairs the rig task with the action id.
        let ab = MeshyOp::animate_body("rig-9", 42);
        assert_eq!(ab["rig_task_id"].as_str().unwrap(), "rig-9");
        assert_eq!(ab["action_id"].as_i64().unwrap(), 42);

        // The library's first action id, or None when there is none.
        assert_eq!(
            library_action_id(&json!([{ "action_id": 7, "name": "Walk" }])),
            Some(7)
        );
        assert_eq!(library_action_id(&json!([])), None);
        assert_eq!(library_action_id(&json!({ "not": "an array" })), None);
        // An entry missing an action_id yields None.
        assert_eq!(library_action_id(&json!([{ "name": "Walk" }])), None);
    }

    #[test]
    fn glb_and_preview_for_the_new_ops() {
        let generated = json!({
            "model_urls": { "glb": "https://a/m.glb" },
            "thumbnail_url": "https://a/t.png"
        });
        assert_eq!(
            MeshyOp::TextTo3d.glb_url(&generated).unwrap(),
            "https://a/m.glb"
        );
        assert_eq!(
            MeshyOp::Retexture.glb_url(&generated).unwrap(),
            "https://a/m.glb"
        );
        assert_eq!(
            MeshyOp::TextTo3d.preview_url(&generated).unwrap(),
            "https://a/t.png"
        );
        assert_eq!(
            MeshyOp::Retexture.preview_url(&generated).unwrap(),
            "https://a/t.png"
        );
        let anim = json!({ "result": { "animation_glb_url": "https://a/anim.glb" } });
        assert_eq!(
            MeshyOp::Animate.glb_url(&anim).unwrap(),
            "https://a/anim.glb"
        );
        assert_eq!(MeshyOp::Animate.preview_url(&anim), None);
        assert_eq!(MeshyOp::Animate.glb_url(&json!({ "result": {} })), None);
        assert_eq!(MeshyOp::Animate.glb_url(&json!({})), None);
    }
}

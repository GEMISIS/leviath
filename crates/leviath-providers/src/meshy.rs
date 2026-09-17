//! The Meshy provider: reference images or a mesh in, a 3D model out.
//!
//! Meshy is not a chat backend. It takes a typed input (one or more images,
//! or an existing mesh) plus a few optional hints, runs a job that takes
//! minutes, and hands back a `.glb`. It still fits the one provider seam the
//! rest of the system speaks: the stage's visible parts arrive as the
//! request's hydrated mime blocks, the produced mesh leaves as an
//! [`InferenceResponse`] part, and the runtime stores and routes it the same
//! way it stores an image a drawing model returns.
//!
//! Making it a provider is what replaces a fragile "an LLM drives an MCP
//! server in a loop" build stage with one deterministic call: submit, poll to
//! completion under the stage's own deadline, download the mesh. No tool loop,
//! no offset replies, no stand-in accounting, because there is no model in the
//! loop at all.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;
use tokio::time::{Duration, Instant, sleep};

use leviath_core::mime::{Blob, MimeType};
use leviath_net::read_caps::{JSON_BODY_CAP, read_body_capped};

use crate::capabilities::{Match, ModelCapabilities, ModelCapabilityOverride, ModelMime, Row};
use crate::pricing::TokenUsage;
use crate::provider::{
    FinishReason, InferenceRequest, InferenceResponse, ModelInfo, Provider, ProviderError,
    RateLimitConfig, Result, StreamChunk, apply_request_timeout,
};
use crate::rate_limit::RateLimiter;

mod ops;
use ops::{MeshyOp, TaskState, animate_action, created_task_id, library_action_id, task_state};

/// The default Meshy API origin.
const DEFAULT_BASE_URL: &str = "https://api.meshy.ai";
/// The mime type every Meshy operation produces.
const GLTF_BINARY: &str = "model/gltf-binary";
/// How long to wait between status polls.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// The whole-operation deadline when a stage sets no `request_timeout_secs`.
/// A Meshy job runs for minutes, so this is generous; a per-stage timeout
/// overrides it in either direction.
const DEFAULT_OP_TIMEOUT_SECS: u64 = 900;
/// The per-request bound on a single create or poll call, which are quick.
const SHORT_REQUEST_SECS: u64 = 60;
/// The per-request bound on a mesh download, which moves megabytes.
const DOWNLOAD_REQUEST_SECS: u64 = 300;

/// The models this build names for a listing, one per operation.
pub(crate) const CATALOG: &[(&str, &str)] = &[
    ("text-to-3d", "Meshy Text to 3D"),
    ("image-to-3d", "Meshy Image to 3D"),
    ("multi-image-to-3d", "Meshy Multi-Image to 3D"),
    ("retexture", "Meshy Retexture"),
    ("rig", "Meshy Rig"),
    ("animate", "Meshy Animate"),
];

/// The path that creates and lists an animation task (animate's second phase).
const ANIMATIONS_PATH: &str = "openapi/v1/animations";

/// The nominal limits for a Meshy model.
///
/// A 3D generator has no token context window in the chat sense; these numbers
/// exist so a stage running Meshy sizes its regions sensibly and the catalogue
/// invariant (context greater than output) holds. One row matches every
/// operation because they share these limits.
///
/// The window is deliberately huge because a mesh input reaches Meshy as a
/// base64 data URI, and the pre-flight token guard charges that blob its native
/// per-byte estimate - roughly a quarter-token per byte, so a few-megabyte GLB
/// is over a million "tokens". Meshy is a REST API with no token limit, so a
/// small window would refuse an ordinary rig or animate before the request ever
/// left; this covers a mesh up to the media-per-request ceiling with room to
/// spare. The regions themselves charge a stored part its short stand-in, not
/// this, so nothing is sized against it wastefully.
pub(crate) const MODELS: &[Row] = &[Row {
    matches: &[Match::Contains("")],
    temperature: false,
    tools: false,
    context: 64_000_000,
    output: 8_192,
}];

/// What [`MODELS`] says about `model`.
pub(crate) fn table_capabilities(model: &str) -> ModelCapabilities {
    crate::capabilities::lookup(MODELS, model, ModelCapabilities::default())
}

/// The mime a Meshy operation takes and produces, for a caller with no
/// provider instance in hand. Unknown ids answer text-only, like every other
/// provider's `builtin_mime` arm.
pub(crate) fn mime_for(model: &str) -> ModelMime {
    MeshyOp::parse(model).map_or_else(ModelMime::text_only, MeshyOp::mime)
}

/// A generative 3D provider backed by Meshy's REST API.
pub struct MeshyProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    rate_limiter: Option<RateLimiter>,
    capability_overrides: HashMap<String, ModelCapabilityOverride>,
    poll_interval: Duration,
    /// The operator's extra headers, sent after the bearer token on every
    /// API call to `base_url`; not on asset downloads, which go to signed
    /// URLs on another host.
    extra_headers: Vec<(String, String)>,
}

impl MeshyProvider {
    /// A provider with the default Meshy origin and no overrides.
    pub fn new(client: reqwest::Client, api_key: String) -> Self {
        Self {
            client,
            api_key,
            base_url: DEFAULT_BASE_URL.to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
            poll_interval: POLL_INTERVAL,
            extra_headers: Vec::new(),
        }
    }

    /// A provider with per-model capability overrides and an optional rate
    /// limit, the shape the registry builds.
    pub fn with_overrides(
        client: reqwest::Client,
        api_key: String,
        overrides: HashMap<String, ModelCapabilityOverride>,
        rate_limit: Option<&RateLimitConfig>,
    ) -> Self {
        Self {
            client,
            api_key,
            base_url: DEFAULT_BASE_URL.to_string(),
            rate_limiter: rate_limit.map(RateLimiter::new),
            capability_overrides: overrides,
            poll_interval: POLL_INTERVAL,
            extra_headers: Vec::new(),
        }
    }

    /// Poll faster than the production cadence, so a test's mocked job
    /// completes without a real wait.
    #[cfg(test)]
    fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Point this provider at a different origin. `None` keeps the default, so
    /// a config that says nothing sends the same request it did before.
    pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
        if let Some(url) = base_url {
            self.base_url = url.trim_end_matches('/').to_string();
        }
        self
    }

    /// Extra headers on every API call to the host, after the bearer token:
    /// what a gateway named in `with_base_url` wants of its own.
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }

    /// The full URL for a path under the base origin.
    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path)
    }

    /// Send `builder` within `timeout_secs` and hand back its response, on
    /// the same path every other provider's requests take: a dead socket is
    /// a transient transport error, a 429 paces the limiter and carries its
    /// `Retry-After`, a bad key or an empty account is [`ProviderError::Unavailable`],
    /// and any other non-2xx is an [`ProviderError::ApiError`] with the body.
    async fn send(
        &self,
        builder: reqwest::RequestBuilder,
        timeout_secs: u64,
        doing: &str,
    ) -> Result<reqwest::Response> {
        let response = apply_request_timeout(builder, Some(timeout_secs))
            .send()
            .await
            .map_err(|e| ProviderError::transport(doing, &e))?;
        crate::provider::check_http_response(response, self.rate_limiter.as_ref())
            .await
            .map_err(explain_rig_refusal)
    }

    /// [`Self::send`], then the JSON body: capped, with a malformed body the
    /// API's own fault rather than something to retry.
    async fn send_json(
        &self,
        builder: reqwest::RequestBuilder,
        timeout_secs: u64,
        doing: &str,
    ) -> Result<Value> {
        let response = self.send(builder, timeout_secs, doing).await?;
        crate::provider::decode_json(response).await
    }

    /// POST a create body and read the JSON response.
    async fn post_json(&self, url: &str, body: &Value) -> Result<Value> {
        let builder = crate::provider::with_extra_headers(
            self.client.post(url).bearer_auth(&self.api_key).json(body),
            &self.extra_headers,
        );
        self.send_json(builder, SHORT_REQUEST_SECS, "creating a Meshy task")
            .await
    }

    /// GET a task's status body.
    async fn get_json(&self, url: &str) -> Result<Value> {
        let builder = crate::provider::with_extra_headers(
            self.client.get(url).bearer_auth(&self.api_key),
            &self.extra_headers,
        );
        self.send_json(builder, SHORT_REQUEST_SECS, "polling a Meshy task")
            .await
    }

    /// Download the bytes at a signed asset URL, up to the same ceiling every
    /// provider body has.
    ///
    /// No bearer token: the URL is already signed, and the asset host is a
    /// different origin from the API.
    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let response = self
            .send(
                self.client.get(url),
                DOWNLOAD_REQUEST_SECS,
                "downloading a Meshy asset",
            )
            .await?;
        read_body_capped(response, JSON_BODY_CAP)
            .await
            .map(|b| b.to_vec())
            .map_err(ProviderError::from)
    }

    /// The absolute deadline for a whole operation, from its stage timeout.
    /// One deadline covers every phase of a multi-phase operation.
    fn deadline_for(request: &InferenceRequest) -> Instant {
        Instant::now()
            + Duration::from_secs(
                request
                    .request_timeout_secs
                    .unwrap_or(DEFAULT_OP_TIMEOUT_SECS),
            )
    }

    /// Poll a task to completion, or fail on the deadline or its own failure.
    async fn poll_to_completion(&self, status_url: &str, deadline: Instant) -> Result<Value> {
        loop {
            let task = self.get_json(status_url).await?;
            match task_state(&task) {
                TaskState::Succeeded => return Ok(task),
                TaskState::Failed(reason) => {
                    return Err(ProviderError::Other(format!("Meshy task failed: {reason}")));
                }
                TaskState::Running(_) => {}
            }
            if Instant::now() >= deadline {
                return Err(ProviderError::Other(
                    "Meshy task did not finish within its deadline".to_string(),
                ));
            }
            sleep(self.poll_interval).await;
        }
    }

    /// POST a create body to a path and return the new task's id.
    async fn create_task(&self, path: &str, body: &Value) -> Result<String> {
        let create = self.post_json(&self.url(path), body).await?;
        created_task_id(&create)
    }

    /// Download a finished task's mesh, and its preview render when it has one,
    /// as parts. The preview is best-effort: a mesh that came back is a success
    /// even if its thumbnail cannot be fetched.
    async fn download_parts(&self, op: MeshyOp, task: &Value) -> Result<Vec<Blob>> {
        let glb_url = op.glb_url(task).ok_or_else(|| {
            ProviderError::InvalidResponse(format!(
                "a finished Meshy {} task carried no GLB url",
                op.id()
            ))
        })?;
        let glb = self.get_bytes(&glb_url).await?;
        let mime = MimeType::parse(GLTF_BINARY).expect("model/gltf-binary is a valid mime type");
        let mut parts = vec![Blob::new(mime, glb).named(op.output_name())];
        if let Some(preview_url) = op.preview_url(task) {
            match self.get_bytes(&preview_url).await {
                Ok(bytes) => {
                    let png = MimeType::parse("image/png").expect("image/png is valid");
                    parts.push(Blob::new(png, bytes).named("preview.png"));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "[meshy] could not fetch the preview render");
                }
            }
        }
        Ok(parts)
    }

    /// Run a single-phase operation end to end: submit, poll, download.
    async fn run_operation(&self, op: MeshyOp, request: &InferenceRequest) -> Result<Vec<Blob>> {
        let deadline = Self::deadline_for(request);
        let task_id = self
            .create_task(op.path(), &op.build_body(request)?)
            .await?;
        let status_url = format!("{}/{task_id}", self.url(op.path()));
        let task = self.poll_to_completion(&status_url, deadline).await?;
        self.download_parts(op, &task).await
    }

    /// Text to a textured mesh: a preview task builds the geometry, then a
    /// refine task textures it. Both share one endpoint and one deadline.
    async fn run_text_to_3d(&self, request: &InferenceRequest) -> Result<Vec<Blob>> {
        let op = MeshyOp::TextTo3d;
        let deadline = Self::deadline_for(request);
        let preview_id = self
            .create_task(op.path(), &op.build_body(request)?)
            .await?;
        let preview_url = format!("{}/{preview_id}", self.url(op.path()));
        self.poll_to_completion(&preview_url, deadline).await?;

        let refine_body = MeshyOp::text_refine_body(&preview_id, request);
        let refine_id = self.create_task(op.path(), &refine_body).await?;
        let refine_url = format!("{}/{refine_id}", self.url(op.path()));
        let task = self.poll_to_completion(&refine_url, deadline).await?;
        self.download_parts(op, &task).await
    }

    /// A mesh to an animated mesh: rig it, look the requested action up in the
    /// animation library, then apply that action to the rigged model.
    async fn run_animate(&self, request: &InferenceRequest) -> Result<Vec<Blob>> {
        let op = MeshyOp::Animate;
        let deadline = Self::deadline_for(request);

        // Phase 1: rig. Animate's build_body is a rig body and its path a rig
        // path, so the first phase reuses them.
        let rig_id = self
            .create_task(op.path(), &op.build_body(request)?)
            .await?;
        let rig_url = format!("{}/{rig_id}", self.url(op.path()));
        self.poll_to_completion(&rig_url, deadline).await?;

        // Phase 2: resolve the requested action to a library action id.
        let action = animate_action(request);
        let library = self.get_library(&action).await?;
        let action_id = library_action_id(&library).ok_or_else(|| {
            ProviderError::InvalidResponse(format!("Meshy has no animation matching '{action}'"))
        })?;

        // Phase 3: animate the rigged model with that action.
        let anim_body = MeshyOp::animate_body(&rig_id, action_id);
        let anim_id = self.create_task(ANIMATIONS_PATH, &anim_body).await?;
        let anim_url = format!("{}/{anim_id}", self.url(ANIMATIONS_PATH));
        let task = self.poll_to_completion(&anim_url, deadline).await?;
        self.download_parts(op, &task).await
    }

    /// The animation library, filtered by a search term.
    async fn get_library(&self, search: &str) -> Result<Value> {
        let url = self.url(&format!(
            "{ANIMATIONS_PATH}/library?search={}",
            query_encode(search)
        ));
        let builder = crate::provider::with_extra_headers(
            self.client.get(&url).bearer_auth(&self.api_key),
            &self.extra_headers,
        );
        self.send_json(builder, SHORT_REQUEST_SECS, "listing Meshy animations")
            .await
    }
}

/// Percent-encode a query-parameter value (the animation search term).
///
/// The workspace's reqwest is built without the query-string feature, so the
/// term is encoded here rather than by `RequestBuilder::query`. Unreserved
/// characters pass through; everything else becomes `%XX`.
fn query_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[async_trait]
impl Provider for MeshyProvider {
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        let op = MeshyOp::parse(&request.model).ok_or_else(|| {
            ProviderError::InvalidResponse(format!(
                "the meshy provider has no operation named '{}'; it serves {}",
                request.model,
                CATALOG
                    .iter()
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }
        let parts = match op {
            MeshyOp::TextTo3d => self.run_text_to_3d(request).await?,
            MeshyOp::Animate => self.run_animate(request).await?,
            single => self.run_operation(single, request).await?,
        };
        let summary = format!(
            "Produced {} part(s) with meshy/{}: {}",
            parts.len(),
            op.id(),
            parts
                .iter()
                .map(|p| p.name.clone().unwrap_or_default())
                .collect::<Vec<_>>()
                .join(", ")
        );
        Ok(InferenceResponse {
            content: summary,
            tool_calls: Vec::new(),
            tokens_used: TokenUsage::new(0, 0, 0, 0),
            finish_reason: FinishReason::Complete,
            reasoning: None,
            parts,
        })
    }

    async fn infer_stream(
        &self,
        request: &InferenceRequest,
    ) -> Result<std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<StreamChunk>> + Send>>>
    {
        // The default streaming impl drops produced parts (its chunk carries
        // none), which for a provider whose whole output is a part would lose
        // the mesh. So emit one chunk that carries the parts through.
        let response = self.infer(request).await?;
        let chunk = StreamChunk {
            delta: response.content,
            tool_calls: Vec::new(),
            tokens: Some(response.tokens_used),
            finish_reason: Some(response.finish_reason),
            reasoning: None,
            parts: response.parts,
        };
        Ok(Box::pin(tokio_stream::once(Ok(chunk))))
    }

    async fn count_tokens(&self, text: &str, _model: &str) -> usize {
        leviath_core::estimate_tokens(text)
    }

    /// The six operations, straight from the compiled catalogue: Meshy has no
    /// listing endpoint, and without this arm a live `/api/models` or
    /// `lev models list` showed a configured Meshy key serving nothing.
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(CATALOG
            .iter()
            .map(|&(id, display)| {
                ModelInfo::new(id, self.name(), self.capabilities(id))
                    .named(Some(display.to_string()))
                    .with_mime(self.mime(id))
            })
            .collect())
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        self.capabilities(model).max_context_tokens
    }

    fn name(&self) -> &str {
        "meshy"
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        match self.capability_overrides.get(model) {
            Some(o) => o.apply_to(table_capabilities(model)),
            None => table_capabilities(model),
        }
    }

    fn mime(&self, model: &str) -> ModelMime {
        let base = mime_for(model);
        match self.capability_overrides.get(model) {
            Some(o) => o.apply_mime(base),
            None => base,
        }
    }

    fn serves_model(&self, model_key: &str) -> Option<String> {
        (MeshyOp::parse(model_key).is_some() || self.capability_overrides.contains_key(model_key))
            .then(|| model_key.to_string())
    }
}

/// Meshy refuses to rig a mesh it finds no body in with a 422 whose message
/// says "Pose estimation failed, please provide a valid model URL" - a remark
/// about the mesh, worded as if the request were malformed. Say what it
/// means and what fixes it, and keep Meshy's own words at the end.
fn explain_rig_refusal(err: ProviderError) -> ProviderError {
    match err {
        ProviderError::ApiError(msg) if msg.contains("Pose estimation failed") => {
            ProviderError::ApiError(format!(
                "Meshy could not rig this mesh: its pose estimation found no humanoid body \
                 to fit a skeleton to. Rigging wants one upright, roughly humanoid character \
                 with its limbs apart (an A- or T-pose) and no props merged into the body, \
                 built from several views; a single-view build, a waving or crossed arm, or \
                 a non-humanoid shape is refused here. Rebuild with more views and \
                 pose_mode = \"a-pose\", or use the unrigged mesh as it is ({msg})"
            ))
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ContentBlock, Message, MessageContent};
    use leviath_core::mime::BlobRef;
    use leviath_testkit::{spawn_mock_sequence, spawn_mock_server};
    use serde_json::json;
    use tokio_stream::StreamExt;

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    /// The rigger's refusal is reworded to say what it means; every other
    /// error, and every other message, passes through untouched.
    #[test]
    fn a_pose_estimation_refusal_is_explained() {
        let raw = "[bad-request] HTTP 422 Unprocessable Entity: {\"message\":\"Pose estimation \
                   failed, please provide a valid model URL\"}";
        let explained = explain_rig_refusal(ProviderError::ApiError(raw.to_string())).to_string();
        assert!(explained.contains("could not rig this mesh"), "{explained}");
        assert!(explained.contains("a-pose"), "{explained}");
        assert!(explained.contains(raw), "{explained}");

        let other = explain_rig_refusal(ProviderError::ApiError("HTTP 500: boom".into()));
        assert_eq!(other.to_string(), "API error: HTTP 500: boom");
        let failed = explain_rig_refusal(ProviderError::RequestFailed(
            "Pose estimation failed".into(),
        ));
        assert!(
            matches!(failed, ProviderError::RequestFailed(ref m) if m == "Pose estimation failed"),
            "{failed}"
        );
    }

    fn provider_at(url: &str) -> MeshyProvider {
        MeshyProvider::new(client(), "test-key".into())
            .with_base_url(Some(url.into()))
            .with_poll_interval(Duration::from_millis(1))
    }

    fn image_block(mime: &str, data: &str) -> ContentBlock {
        ContentBlock::Mime {
            part: BlobRef {
                sha256: "b".repeat(64),
                mime_type: MimeType::parse(mime).unwrap(),
                size: 3,
                width: None,
                height: None,
                duration_ms: None,
                tokens: 1,
                stand_in: "[img]".into(),
            },
            data: data.into(),
            name: Some("front.png".into()),
            deliver: None,
            remote: None,
        }
    }

    fn request_with(model: &str, blocks: Vec<ContentBlock>) -> InferenceRequest {
        InferenceRequest {
            system: Vec::new(),
            messages: vec![Message {
                role: "user".into(),
                content: MessageContent::Blocks(blocks),
                cache_breakpoint: false,
                reasoning: None,
            }],
            model: model.into(),
            max_tokens: 0,
            temperature: 0.0,
            tools: Vec::new(),
            extra: Value::Null,
            request_timeout_secs: None,
        }
    }

    #[test]
    fn identity_and_capabilities() {
        let p = MeshyProvider::new(client(), "k".into());
        assert_eq!(p.name(), "meshy");
        let caps = p.capabilities("image-to-3d");
        assert_eq!(caps.max_context_tokens, 64_000_000);
        assert!(caps.max_context_tokens > caps.max_output_tokens);
        assert_eq!(p.max_context_tokens("rig"), 64_000_000);
        assert!(!p.capabilities("image-to-3d").supports_tools);
        // mime is per operation.
        assert!(
            p.mime("image-to-3d")
                .accepts(&MimeType::parse("image/png").unwrap())
        );
        assert!(
            p.mime("rig")
                .produces(&MimeType::parse("model/gltf-binary").unwrap())
        );
        // an unknown model is text-only, not a panic.
        assert_eq!(p.mime("nope"), ModelMime::text_only());
        assert_eq!(
            p.serves_model("multi-image-to-3d").as_deref(),
            Some("multi-image-to-3d")
        );
        assert_eq!(p.serves_model("meshy/rig").as_deref(), Some("meshy/rig"));
        assert_eq!(p.serves_model("gpt-5"), None);
        // The provider-less mime table answers for meshy too.
        assert!(
            crate::mime_tables::builtin_mime("meshy", "image-to-3d")
                .accepts(&MimeType::parse("image/png").unwrap())
        );
    }

    #[tokio::test]
    async fn the_listing_names_every_operation_with_its_mime() {
        let p = MeshyProvider::new(client(), "k".into());
        let listed = p.list_models().await.unwrap();
        let ids: Vec<&str> = listed.iter().map(|m| m.id.as_str()).collect();
        let expected: Vec<&str> = CATALOG.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, expected);
        let rig = listed.iter().find(|m| m.id == "rig").unwrap();
        assert_eq!(rig.provider, "meshy");
        assert_eq!(rig.display_name.as_deref(), Some("Meshy Rig"));
        assert!(!rig.capabilities.supports_tools);
        assert!(
            rig.mime
                .produces(&MimeType::parse("model/gltf-binary").unwrap())
        );
    }

    #[tokio::test]
    async fn count_tokens_is_a_local_estimate() {
        let p = MeshyProvider::new(client(), "k".into());
        assert!(p.count_tokens("some prompt text here", "image-to-3d").await >= 1);
    }

    #[test]
    fn with_base_url_none_keeps_the_default_and_trailing_slash_is_trimmed() {
        let p = MeshyProvider::new(client(), "k".into()).with_base_url(None);
        assert_eq!(p.base_url, DEFAULT_BASE_URL);
        let p = MeshyProvider::new(client(), "k".into()).with_base_url(Some("http://h/".into()));
        assert_eq!(p.base_url, "http://h");
        assert_eq!(p.url("openapi/v1/rigging"), "http://h/openapi/v1/rigging");
    }

    /// The operator's extra headers reach the wire on an API call, after the
    /// bearer token.
    #[tokio::test]
    async fn extra_headers_ride_every_api_call() {
        let (url, seen) = leviath_testkit::spawn_mock_recorder(200, "OK", b"{}".to_vec()).await;
        let provider = provider_at(&url)
            .with_headers(vec![("X-Gateway-Token".to_string(), "t-1".to_string())]);
        provider
            .get_json(&format!("{url}/v1/tasks/t"))
            .await
            .unwrap();
        let request = leviath_core::sync::lock(&seen)[0].to_ascii_lowercase();
        assert!(request.contains("x-gateway-token: t-1"), "{request}");
        let own = request.find("authorization").expect("the key is sent");
        let extra = request.find("x-gateway-token").expect("the extra is sent");
        assert!(own < extra, "the bearer token comes first: {request}");
    }

    /// A create response, two polls, then the download. The succeeded body
    /// points its GLB and thumbnail at a separate one-shot asset server whose
    /// URL is known before the poll sequence is built.
    #[tokio::test]
    async fn image_to_3d_submits_polls_downloads_and_returns_the_mesh() {
        let glb = spawn_mock_server(200, "OK", b"glTF-bytes".to_vec()).await;
        let png = spawn_mock_server(200, "OK", b"png-bytes".to_vec()).await;
        let succeeded = json!({
            "status": "SUCCEEDED",
            "model_urls": { "glb": format!("{glb}/m.glb") },
            "thumbnail_url": format!("{png}/t.png")
        });
        let (api, bodies) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"task-1"}"#.to_vec()),
            (200, "OK", br#"{"status":"PENDING","progress":0}"#.to_vec()),
            (200, "OK", succeeded.to_string().into_bytes()),
        ])
        .await;
        let p = provider_at(&api);
        let req = request_with(
            "image-to-3d",
            vec![
                ContentBlock::Text {
                    text: "a brass robot".into(),
                },
                image_block("image/png", "QUJD"),
            ],
        );
        let out = p.infer(&req).await.expect("a mesh comes back");
        assert_eq!(out.parts.len(), 2, "the mesh and its preview");
        assert_eq!(out.parts[0].bytes, b"glTF-bytes");
        assert_eq!(out.parts[0].mime_type.as_str(), "model/gltf-binary");
        assert_eq!(out.parts[0].name.as_deref(), Some("model.glb"));
        assert_eq!(out.parts[1].mime_type.as_str(), "image/png");
        assert!(out.content.contains("image-to-3d"));
        // The create body carried the texture prompt and the image.
        let create = &bodies.lock().unwrap()[0];
        assert!(create.contains("a brass robot"), "{create}");
        assert!(create.contains("data:image/png;base64,QUJD"), "{create}");
    }

    #[tokio::test]
    async fn multi_image_and_rig_reach_their_endpoints() {
        // multi-image: no preview download here (thumbnail omitted), so one part.
        let glb = spawn_mock_server(200, "OK", b"mesh".to_vec()).await;
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"t"}"#.to_vec()),
            (
                200,
                "OK",
                json!({"status":"SUCCEEDED","model_urls":{"glb":format!("{glb}/m.glb")}})
                    .to_string()
                    .into_bytes(),
            ),
        ])
        .await;
        let out = provider_at(&api)
            .infer(&request_with(
                "multi-image-to-3d",
                vec![image_block("image/png", "QQ")],
            ))
            .await
            .expect("mesh");
        assert_eq!(out.parts.len(), 1);

        // rig: input a mesh, read the rigged GLB from result.*.
        let rig_glb = spawn_mock_server(200, "OK", b"rigged".to_vec()).await;
        let (api, bodies) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"r"}"#.to_vec()),
            (
                200,
                "OK",
                json!({"status":"SUCCEEDED","result":{"rigged_character_glb_url":format!("{rig_glb}/r.glb")}})
                    .to_string()
                    .into_bytes(),
            ),
        ])
        .await;
        let out = provider_at(&api)
            .infer(&request_with(
                "rig",
                vec![image_block("model/gltf-binary", "R0xC")],
            ))
            .await
            .expect("rigged mesh");
        assert_eq!(out.parts[0].bytes, b"rigged");
        assert_eq!(out.parts[0].name.as_deref(), Some("rigged.glb"));
        assert!(bodies.lock().unwrap()[0].contains("model_url"));
    }

    #[tokio::test]
    async fn a_preview_that_will_not_download_still_yields_the_mesh() {
        let glb = spawn_mock_server(200, "OK", b"mesh".to_vec()).await;
        // A thumbnail url pointing at a closed port: the download fails and is
        // skipped, the mesh part still returns.
        let succeeded = json!({
            "status": "SUCCEEDED",
            "model_urls": { "glb": format!("{glb}/m.glb") },
            "thumbnail_url": "http://127.0.0.1:1/t.png"
        });
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"t"}"#.to_vec()),
            (200, "OK", succeeded.to_string().into_bytes()),
        ])
        .await;
        let out = provider_at(&api)
            .infer(&request_with(
                "multi-image-to-3d",
                vec![image_block("image/png", "QQ")],
            ))
            .await
            .expect("mesh despite the preview miss");
        assert_eq!(out.parts.len(), 1);
    }

    #[tokio::test]
    async fn an_unknown_operation_is_refused_before_any_call() {
        let p = MeshyProvider::new(client(), "k".into());
        let err = p.infer(&request_with("sculpt", vec![])).await.unwrap_err();
        assert!(
            err.to_string().contains("no operation named 'sculpt'"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_task_that_fails_is_an_error() {
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"t"}"#.to_vec()),
            (
                200,
                "OK",
                br#"{"status":"FAILED","task_error":{"message":"unsupported mesh"}}"#.to_vec(),
            ),
        ])
        .await;
        let err = provider_at(&api)
            .infer(&request_with(
                "image-to-3d",
                vec![image_block("image/png", "QQ")],
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unsupported mesh"), "{err}");
    }

    #[tokio::test]
    async fn a_task_that_never_finishes_hits_the_deadline() {
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"t"}"#.to_vec()),
            (
                200,
                "OK",
                br#"{"status":"IN_PROGRESS","progress":5}"#.to_vec(),
            ),
            (
                200,
                "OK",
                br#"{"status":"IN_PROGRESS","progress":5}"#.to_vec(),
            ),
        ])
        .await;
        let mut req = request_with("image-to-3d", vec![image_block("image/png", "QQ")]);
        // A zero deadline trips on the first poll that is still running.
        req.request_timeout_secs = Some(0);
        let err = provider_at(&api).infer(&req).await.unwrap_err();
        assert!(err.to_string().contains("did not finish within"), "{err}");
    }

    #[tokio::test]
    async fn a_finished_task_with_no_glb_is_an_error() {
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"t"}"#.to_vec()),
            (
                200,
                "OK",
                br#"{"status":"SUCCEEDED","model_urls":{}}"#.to_vec(),
            ),
        ])
        .await;
        let err = provider_at(&api)
            .infer(&request_with(
                "image-to-3d",
                vec![image_block("image/png", "QQ")],
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("carried no GLB url"), "{err}");
    }

    #[tokio::test]
    async fn a_download_that_fails_is_an_error() {
        let succeeded = json!({
            "status": "SUCCEEDED",
            "model_urls": { "glb": "http://127.0.0.1:1/m.glb" }
        });
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"t"}"#.to_vec()),
            (200, "OK", succeeded.to_string().into_bytes()),
        ])
        .await;
        let err = provider_at(&api)
            .infer(&request_with(
                "multi-image-to-3d",
                vec![image_block("image/png", "QQ")],
            ))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("downloading a Meshy asset"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_bad_key_is_unavailable_and_a_server_error_is_an_api_error() {
        let url = spawn_mock_server(401, "Unauthorized", b"bad key".to_vec()).await;
        let err = provider_at(&url)
            .infer(&request_with(
                "image-to-3d",
                vec![image_block("image/png", "QQ")],
            ))
            .await
            .unwrap_err();
        assert!(
            err.unavailable_reason().is_some(),
            "a bad key is unavailable: {err}"
        );
        let url = spawn_mock_server(500, "Internal", b"boom".to_vec()).await;
        let err = provider_at(&url)
            .infer(&request_with(
                "image-to-3d",
                vec![image_block("image/png", "QQ")],
            ))
            .await
            .unwrap_err();
        assert!(
            err.unavailable_reason().is_none() && err.to_string().contains("HTTP 500"),
            "a 500 is a plain api error: {err}"
        );
    }

    #[tokio::test]
    async fn send_and_status_failures_on_each_call_are_reported() {
        // Create send failure: nothing listens on the port.
        let err = provider_at("http://127.0.0.1:1")
            .infer(&request_with(
                "image-to-3d",
                vec![image_block("image/png", "QQ")],
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("creating a Meshy task"), "{err}");

        // Poll send failure: the server answers the create then goes away, so
        // the follow-up status request finds a closed port.
        let (api, _b) = spawn_mock_sequence(vec![(200, "OK", br#"{"result":"t"}"#.to_vec())]).await;
        let err = provider_at(&api)
            .infer(&request_with(
                "image-to-3d",
                vec![image_block("image/png", "QQ")],
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("polling a Meshy task"), "{err}");

        // Poll non-2xx: the create is fine, the status endpoint errors.
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"t"}"#.to_vec()),
            (500, "Internal", b"boom".to_vec()),
        ])
        .await;
        let err = provider_at(&api)
            .infer(&request_with(
                "image-to-3d",
                vec![image_block("image/png", "QQ")],
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 500"), "{err}");

        // Download non-2xx: a 404 at the signed asset url.
        let dl = spawn_mock_server(404, "Not Found", b"gone".to_vec()).await;
        let succeeded = json!({
            "status": "SUCCEEDED",
            "model_urls": { "glb": format!("{dl}/m.glb") }
        });
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"t"}"#.to_vec()),
            (200, "OK", succeeded.to_string().into_bytes()),
        ])
        .await;
        let err = provider_at(&api)
            .infer(&request_with(
                "multi-image-to-3d",
                vec![image_block("image/png", "QQ")],
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 404"), "{err}");
    }

    #[tokio::test]
    async fn a_build_error_surfaces_through_infer() {
        // With no image, build_body fails inside run_operation, before any call.
        let err = provider_at("http://127.0.0.1:1")
            .infer(&request_with("image-to-3d", vec![]))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("image-to-3d needs an image"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_rate_limited_provider_still_runs_and_streaming_carries_the_part() {
        let glb = spawn_mock_server(200, "OK", b"mesh".to_vec()).await;
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"t"}"#.to_vec()),
            (
                200,
                "OK",
                json!({"status":"SUCCEEDED","model_urls":{"glb":format!("{glb}/m.glb")}})
                    .to_string()
                    .into_bytes(),
            ),
        ])
        .await;
        let limit = RateLimitConfig {
            requests_per_minute: 100,
            tokens_per_minute: 100_000,
        };
        let p = MeshyProvider::with_overrides(client(), "k".into(), HashMap::new(), Some(&limit))
            .with_base_url(Some(api))
            .with_poll_interval(Duration::from_millis(1));
        let mut stream = p
            .infer_stream(&request_with(
                "multi-image-to-3d",
                vec![image_block("image/png", "QQ")],
            ))
            .await
            .expect("stream");
        let chunk = stream.next().await.expect("one chunk").expect("ok");
        assert_eq!(chunk.parts.len(), 1);
        assert_eq!(chunk.parts[0].bytes, b"mesh");
    }

    #[tokio::test]
    async fn a_create_response_without_a_task_id_is_an_error() {
        let url = spawn_mock_server(200, "OK", br#"{"nope":true}"#.to_vec()).await;
        let err = provider_at(&url)
            .infer(&request_with(
                "image-to-3d",
                vec![image_block("image/png", "QQ")],
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no task id"), "{err}");
    }

    #[test]
    fn a_capability_override_is_merged_onto_the_table_and_the_mime() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "image-to-3d".to_string(),
            ModelCapabilityOverride {
                max_context_tokens: Some(64_000),
                input_types: Some(vec![
                    "text/*".into(),
                    "image/*".into(),
                    "application/pdf".into(),
                ]),
                ..Default::default()
            },
        );
        let p = MeshyProvider::with_overrides(client(), "k".into(), overrides, None);
        // The override narrows the window and widens the accepted types.
        assert_eq!(p.capabilities("image-to-3d").max_context_tokens, 64_000);
        assert!(
            p.mime("image-to-3d")
                .accepts(&MimeType::parse("application/pdf").unwrap())
        );
        // An override on one model is why serves_model answers for it even
        // when it is not one of the built-in operation ids.
        let mut only = HashMap::new();
        only.insert("custom-op".to_string(), ModelCapabilityOverride::default());
        let p = MeshyProvider::with_overrides(client(), "k".into(), only, None);
        assert_eq!(p.serves_model("custom-op").as_deref(), Some("custom-op"));
    }

    /// A 429 is a rate limit, not an API error: the runtime paces on it and
    /// retries after the wait, where a plain error would end the run.
    #[tokio::test]
    async fn a_rate_limit_is_reported_as_one() {
        let url = spawn_mock_server(429, "Too Many Requests", b"slow down".to_vec()).await;
        let err = provider_at(&url)
            .infer(&request_with(
                "image-to-3d",
                vec![image_block("image/png", "QQ")],
            ))
            .await
            .unwrap_err();
        let kind = format!("{err:?}");
        assert!(kind.starts_with("RateLimitExceeded"), "{kind}");
    }

    #[tokio::test]
    async fn an_unreadable_create_or_status_body_is_an_error() {
        // A 200 that is not JSON fails when the create response is parsed.
        let url = spawn_mock_server(200, "OK", b"not json".to_vec()).await;
        let err = provider_at(&url)
            .infer(&request_with(
                "image-to-3d",
                vec![image_block("image/png", "QQ")],
            ))
            .await
            .unwrap_err();
        // Malformed JSON is the API's own fault and permanent, never retried
        // as a transport blip would be.
        let kind = format!("{err:?}");
        assert!(kind.starts_with("InvalidResponse("), "{kind}");
        // A poll body that is not JSON fails the same way.
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"t"}"#.to_vec()),
            (200, "OK", b"not json".to_vec()),
        ])
        .await;
        let err = provider_at(&api)
            .infer(&request_with(
                "image-to-3d",
                vec![image_block("image/png", "QQ")],
            ))
            .await
            .unwrap_err();
        let kind = format!("{err:?}");
        assert!(kind.starts_with("InvalidResponse("), "{kind}");
    }

    #[tokio::test]
    async fn a_download_whose_body_is_truncated_is_an_error() {
        let dl = leviath_testkit::spawn_mock_server_truncated_body(200, "OK").await;
        let succeeded = json!({
            "status": "SUCCEEDED",
            "model_urls": { "glb": format!("{dl}/m.glb") }
        });
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"t"}"#.to_vec()),
            (200, "OK", succeeded.to_string().into_bytes()),
        ])
        .await;
        let err = provider_at(&api)
            .infer(&request_with(
                "multi-image-to-3d",
                vec![image_block("image/png", "QQ")],
            ))
            .await
            .unwrap_err();
        // Bytes that never arrived are a transport failure, which is retried.
        let kind = format!("{err:?}");
        assert!(kind.starts_with("RequestFailed("), "{kind}");
    }

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text { text: text.into() }
    }

    #[tokio::test]
    async fn text_to_3d_runs_preview_then_refine_and_returns_the_mesh() {
        let glb = spawn_mock_server(200, "OK", b"mesh".to_vec()).await;
        let refined = json!({
            "status": "SUCCEEDED",
            "model_urls": { "glb": format!("{glb}/m.glb") }
        });
        let (api, bodies) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"preview-1"}"#.to_vec()),
            (200, "OK", br#"{"status":"SUCCEEDED"}"#.to_vec()),
            (200, "OK", br#"{"result":"refine-1"}"#.to_vec()),
            (200, "OK", refined.to_string().into_bytes()),
        ])
        .await;
        let out = provider_at(&api)
            .infer(&request_with(
                "text-to-3d",
                vec![text_block("a brass robot")],
            ))
            .await
            .expect("a mesh from text");
        assert_eq!(out.parts.len(), 1);
        assert_eq!(out.parts[0].bytes, b"mesh");
        let b = bodies.lock().unwrap();
        let preview = &b[0];
        let refine = &b[2];
        assert!(
            preview.contains("\"mode\":\"preview\"") && preview.contains("a brass robot"),
            "{preview}"
        );
        assert!(
            refine.contains("\"mode\":\"refine\"") && refine.contains("preview-1"),
            "{refine}"
        );
    }

    #[tokio::test]
    async fn retexture_submits_polls_and_returns_the_mesh() {
        let glb = spawn_mock_server(200, "OK", b"retex".to_vec()).await;
        let (api, bodies) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"rt-1"}"#.to_vec()),
            (
                200,
                "OK",
                json!({"status":"SUCCEEDED","model_urls":{"glb":format!("{glb}/m.glb")}})
                    .to_string()
                    .into_bytes(),
            ),
        ])
        .await;
        let req = request_with(
            "retexture",
            vec![
                text_block("weathered bronze"),
                image_block("model/gltf-binary", "R0xC"),
            ],
        );
        let out = provider_at(&api)
            .infer(&req)
            .await
            .expect("retextured mesh");
        assert_eq!(out.parts[0].name.as_deref(), Some("retextured.glb"));
        let b = bodies.lock().unwrap();
        let created = &b[0];
        assert!(created.contains("text_style_prompt"), "{created}");
    }

    #[tokio::test]
    async fn animate_rigs_looks_up_the_action_and_returns_the_animation() {
        let glb = spawn_mock_server(200, "OK", b"anim".to_vec()).await;
        let done = json!({
            "status": "SUCCEEDED",
            "result": { "animation_glb_url": format!("{glb}/a.glb") }
        });
        let (api, bodies) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"rig-1"}"#.to_vec()),
            (200, "OK", br#"{"status":"SUCCEEDED"}"#.to_vec()),
            (200, "OK", br#"[{"action_id":7,"name":"Walk"}]"#.to_vec()),
            (200, "OK", br#"{"result":"anim-1"}"#.to_vec()),
            (200, "OK", done.to_string().into_bytes()),
        ])
        .await;
        let out = provider_at(&api)
            .infer(&request_with(
                "animate",
                vec![image_block("model/gltf-binary", "R0xC")],
            ))
            .await
            .expect("an animated mesh");
        assert_eq!(out.parts[0].bytes, b"anim");
        assert_eq!(out.parts[0].name.as_deref(), Some("animated.glb"));
        let b = bodies.lock().unwrap();
        let rig_created = &b[0];
        let anim_created = &b[3];
        assert!(rig_created.contains("model_url"), "{rig_created}");
        assert!(
            anim_created.contains("\"rig_task_id\":\"rig-1\"")
                && anim_created.contains("\"action_id\":7"),
            "{anim_created}"
        );
    }

    #[tokio::test]
    async fn animate_with_no_matching_action_is_an_error() {
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"rig-1"}"#.to_vec()),
            (200, "OK", br#"{"status":"SUCCEEDED"}"#.to_vec()),
            (200, "OK", br#"[]"#.to_vec()),
        ])
        .await;
        let err = provider_at(&api)
            .infer(&request_with(
                "animate",
                vec![image_block("model/gltf-binary", "R0xC")],
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no animation matching"), "{err}");
    }

    #[tokio::test]
    async fn the_animation_library_failures_are_reported() {
        // Non-2xx from the library endpoint.
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"rig-1"}"#.to_vec()),
            (200, "OK", br#"{"status":"SUCCEEDED"}"#.to_vec()),
            (500, "Internal", b"boom".to_vec()),
        ])
        .await;
        let err = provider_at(&api)
            .infer(&request_with(
                "animate",
                vec![image_block("model/gltf-binary", "R0xC")],
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 500"), "{err}");

        // A library body that is not JSON.
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"rig-1"}"#.to_vec()),
            (200, "OK", br#"{"status":"SUCCEEDED"}"#.to_vec()),
            (200, "OK", b"not json".to_vec()),
        ])
        .await;
        let err = provider_at(&api)
            .infer(&request_with(
                "animate",
                vec![image_block("model/gltf-binary", "R0xC")],
            ))
            .await
            .unwrap_err();
        let kind = format!("{err:?}");
        assert!(kind.starts_with("InvalidResponse("), "{kind}");

        // The library endpoint unreachable (server gone after the rig).
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"rig-1"}"#.to_vec()),
            (200, "OK", br#"{"status":"SUCCEEDED"}"#.to_vec()),
        ])
        .await;
        let err = provider_at(&api)
            .infer(&request_with(
                "animate",
                vec![image_block("model/gltf-binary", "R0xC")],
            ))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("listing Meshy animations"),
            "{err}"
        );
    }

    #[test]
    fn query_encode_escapes_reserved_characters() {
        assert_eq!(query_encode("walk"), "walk");
        assert_eq!(query_encode("jump kick"), "jump%20kick");
        assert_eq!(query_encode("a/b?c"), "a%2Fb%3Fc");
    }

    #[tokio::test]
    async fn text_to_3d_reports_a_failure_at_each_phase() {
        // No prompt: the build fails before any call.
        let err = provider_at("http://127.0.0.1:1")
            .infer(&request_with("text-to-3d", vec![]))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("text-to-3d needs a text prompt"),
            "{err}"
        );
        let prompt = || request_with("text-to-3d", vec![text_block("a robot")]);

        // The preview create cannot be sent.
        let err = provider_at("http://127.0.0.1:1")
            .infer(&prompt())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("creating a Meshy task"), "{err}");

        // The preview task fails.
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"p"}"#.to_vec()),
            (
                200,
                "OK",
                br#"{"status":"FAILED","task_error":{"message":"bad prompt"}}"#.to_vec(),
            ),
        ])
        .await;
        let err = provider_at(&api).infer(&prompt()).await.unwrap_err();
        assert!(err.to_string().contains("bad prompt"), "{err}");

        // The refine create errors (the server has nothing more to give).
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"p"}"#.to_vec()),
            (200, "OK", br#"{"status":"SUCCEEDED"}"#.to_vec()),
        ])
        .await;
        let err = provider_at(&api).infer(&prompt()).await.unwrap_err();
        assert!(err.to_string().contains("creating a Meshy task"), "{err}");

        // The refine task fails.
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"p"}"#.to_vec()),
            (200, "OK", br#"{"status":"SUCCEEDED"}"#.to_vec()),
            (200, "OK", br#"{"result":"r"}"#.to_vec()),
            (
                200,
                "OK",
                br#"{"status":"FAILED","task_error":{"message":"refine died"}}"#.to_vec(),
            ),
        ])
        .await;
        let err = provider_at(&api).infer(&prompt()).await.unwrap_err();
        assert!(err.to_string().contains("refine died"), "{err}");
    }

    #[tokio::test]
    async fn animate_reports_a_failure_at_each_phase() {
        // No mesh: the build fails before any call.
        let err = provider_at("http://127.0.0.1:1")
            .infer(&request_with("animate", vec![]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("animate needs a model"), "{err}");
        let mesh = || request_with("animate", vec![image_block("model/gltf-binary", "R0xC")]);

        // The rig create cannot be sent.
        let err = provider_at("http://127.0.0.1:1")
            .infer(&mesh())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("creating a Meshy task"), "{err}");

        // The rig task fails.
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"rig"}"#.to_vec()),
            (
                200,
                "OK",
                br#"{"status":"FAILED","task_error":{"message":"rig died"}}"#.to_vec(),
            ),
        ])
        .await;
        let err = provider_at(&api).infer(&mesh()).await.unwrap_err();
        assert!(err.to_string().contains("rig died"), "{err}");

        // The animate create errors after the rig and library succeed.
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"rig"}"#.to_vec()),
            (200, "OK", br#"{"status":"SUCCEEDED"}"#.to_vec()),
            (200, "OK", br#"[{"action_id":7}]"#.to_vec()),
        ])
        .await;
        let err = provider_at(&api).infer(&mesh()).await.unwrap_err();
        assert!(err.to_string().contains("creating a Meshy task"), "{err}");

        // The animate task fails.
        let (api, _b) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"result":"rig"}"#.to_vec()),
            (200, "OK", br#"{"status":"SUCCEEDED"}"#.to_vec()),
            (200, "OK", br#"[{"action_id":7}]"#.to_vec()),
            (200, "OK", br#"{"result":"anim"}"#.to_vec()),
            (
                200,
                "OK",
                br#"{"status":"FAILED","task_error":{"message":"anim died"}}"#.to_vec(),
            ),
        ])
        .await;
        let err = provider_at(&api).infer(&mesh()).await.unwrap_err();
        assert!(err.to_string().contains("anim died"), "{err}");
    }
}

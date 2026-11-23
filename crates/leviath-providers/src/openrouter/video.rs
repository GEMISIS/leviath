//! Video through the gateway: a job, not a chat turn.
//!
//! Measured against the live API (2026-11-10). A model the listing says
//! makes video answers Chat Completions with text alone: the clip is made,
//! billed as output tokens, and never returned. The clip comes from the
//! Videos API instead:
//!
//! - `POST /videos` with `model`, `prompt` and the clip's parameters answers
//!   202 with the job's `id`;
//! - `GET /videos/{id}` reports `status`: `pending`, `in_progress`, then
//!   `completed` (with `usage.cost`, in USD) or `failed` (with `error`);
//! - `GET /videos/{id}/content?index=0` with the key is the MP4.
//!
//! Read from OpenRouter's video generation guide and its public
//! `GET /videos/models` listing (2026-11-23), and not measured:
//!
//! - `frame_images: [{type: "image_url", image_url: {url}, frame_type}]`
//!   starts the clip on a picture (`frame_type: "first_frame"`) or ends it on
//!   one (`"last_frame"`);
//! - `input_references` holds the same objects without `frame_type`, as
//!   pictures to go by; sent beside `frame_images` it is ignored;
//! - each listing entry's `supported_frame_images` names the frame types the
//!   model takes, and is null for a model that takes none.
//!
//! The guide shows `https` urls only. The pictures are sent as base64 `data:`
//! urls, which the Images API documents taking in the same object.

use leviath_core::cost::{CallCost, CostUnknown};
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::OpenRouterProvider;
use super::catalog::gateway_id;
use crate::media::{self, Poll, images};
use crate::provider::{InferenceRequest, InferenceResponse, ProviderError, Result};

/// The stage parameters a video job takes as written.
const PASSED_ON: &[&str] = &["resolution", "aspect_ratio", "size"];

/// The job's body: the model under the gateway's id, the prompt, and the
/// stage's `duration` (or `seconds`), `resolution`, `aspect_ratio`, `size`,
/// `seed` and `generate_audio`. A length that does not read is refused, since
/// it is paid for.
pub(super) fn job_body(model: &str, request: &InferenceRequest) -> Result<Value> {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("prompt".into(), json!(media::request_text(request)));
    if let Some(seconds) = media::extra_seconds(request, &["duration", "seconds"])? {
        body.insert("duration".into(), json!(seconds));
    }
    for key in PASSED_ON {
        if let Some(value) = media::extra_str(request, key) {
            body.insert((*key).into(), json!(value));
        }
    }
    if let Some(seed) = media::extra_i64(request, "seed") {
        body.insert("seed".into(), json!(seed));
    }
    if let Some(audio) = request.extra.get("generate_audio").and_then(Value::as_bool) {
        body.insert("generate_audio".into(), json!(audio));
    }
    Ok(Value::Object(body))
}

/// The frame types each video model takes, by the listing's id.
pub(super) type FrameSupport = std::collections::BTreeMap<String, Vec<String>>;

/// The frame type that starts a clip on a picture.
const FIRST_FRAME: &str = "first_frame";

/// The frame type that ends a clip on a picture.
const LAST_FRAME: &str = "last_frame";

/// What `GET /videos/models` says each model's `supported_frame_images` is:
/// empty for a model that names none.
pub(super) fn frame_support(listing: &Value) -> Result<FrameSupport> {
    let rows = listing
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::InvalidResponse("Missing 'data' array".to_string()))?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let id = row.get("id")?.as_str()?.to_string();
            let frames = row
                .get("supported_frame_images")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            Some((id, frames))
        })
        .collect())
}

/// The frame types `model` takes: its own entry, or the entry whose id ends
/// in `/model` for a bare name the main listing did not resolve. `None` when
/// the listing names no such model.
pub(super) fn frames_of<'a>(support: &'a FrameSupport, model: &str) -> Option<&'a [String]> {
    support
        .get(model)
        .or_else(|| {
            support
                .iter()
                .find(|(id, _)| id.rsplit_once('/').is_some_and(|(_, bare)| bare == model))
                .map(|(_, frames)| frames)
        })
        .map(Vec::as_slice)
}

/// Put the stage's `pictures` (as urls, in the stage's order) in a job's
/// `body`. `frames` is the frame types the model's listing entry names;
/// `None`, when the listing could not be read or does not name the model, is
/// taken as a first frame alone, which most video models take.
///
/// - A model that takes a first frame is sent the first picture as its
///   `first_frame`, and the last picture as its `last_frame` when it takes
///   one and there are two or more. A picture in between is not sent:
///   OpenRouter ignores `input_references` beside `frame_images`.
/// - A model that takes no first frame is sent every picture in
///   `input_references`.
///
/// No pictures leaves `body` as it was.
pub(super) fn carry_pictures(body: &mut Value, pictures: &[String], frames: Option<&[String]>) {
    let Some(first) = pictures.first() else {
        return;
    };
    let takes = |frame: &str| match frames {
        Some(named) => named.iter().any(|f| f == frame),
        None => frame == FIRST_FRAME,
    };
    if !takes(FIRST_FRAME) {
        body["input_references"] = json!(images::image_url_items(pictures));
        return;
    }
    let last = pictures
        .last()
        .filter(|_| pictures.len() > 1 && takes(LAST_FRAME));
    let ends: Vec<String> = std::iter::once(first).chain(last).cloned().collect();
    let sent = ends.len();
    let left_out = pictures.len() - sent;
    if left_out > 0 {
        tracing::warn!(
            sent,
            left_out,
            "an OpenRouter video job takes pictures as its first and last frame only; \
             the stage's other pictures were not sent"
        );
    }
    let framed: Vec<Value> = images::image_url_items(&ends)
        .into_iter()
        .zip([FIRST_FRAME, LAST_FRAME])
        .map(|(mut item, frame)| {
            item["frame_type"] = json!(frame);
            item
        })
        .collect();
    body["frame_images"] = json!(framed);
}

impl OpenRouterProvider {
    /// The frame types the video listing names for `model`, reading the
    /// listing first when no job has yet. `None` when it cannot be read or
    /// does not name the model.
    async fn frame_types(&self, model: &str) -> Option<Vec<String>> {
        let read = || async {
            let listing = crate::provider::fetch_listing(
                &self.client,
                &format!("{}/videos/models", self.base_url),
                &self.listing_headers(),
                Some(crate::provider::SIDE_CALL_TIMEOUT_SECS),
            )
            .await?;
            frame_support(&listing)
        };
        match self.video_frames.get_or_try_init(read).await {
            Ok(support) => frames_of(support, model).map(<[String]>::to_vec),
            Err(e) => {
                tracing::debug!(error = %e, "OpenRouter's video models could not be read");
                None
            }
        }
    }

    /// Whether `request` is a video job: the model makes video, and either
    /// the stage declares video among its outputs or video is all the model
    /// makes. A model that also answers in text stays on chat for a stage
    /// that asks it for text.
    pub(super) fn is_video_job(&self, request: &InferenceRequest) -> bool {
        use crate::provider::Provider as _;
        let made = self.mime(&request.model).output;
        let makes = |family: &str| made.iter().any(|m| m.starts_with(family));
        let asked = request
            .extra
            .get(crate::provider::OUTPUT_MIMES_KEY)
            .and_then(Value::as_array)
            .is_some_and(|mimes| {
                mimes
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|m| m.starts_with("video/"))
            });
        makes("video/") && (asked || !makes("text/"))
    }

    /// Start a video job, wait for it, and keep the clip.
    pub(super) async fn video(
        &self,
        request: &InferenceRequest,
        poll_interval: Duration,
    ) -> Result<InferenceResponse> {
        let deadline = media::deadline(request);
        let model = self.listed_id(gateway_id(&request.model));
        let mut body = job_body(&model, request)?;
        if body["prompt"] == "" {
            return Err(ProviderError::unsent(format!(
                "openrouter/{model} needs a prompt: the stage handed it no text"
            )));
        }
        // The listing is read only for a stage that carries a picture, so a
        // job from text alone is the request it always was.
        let pictures = media::data_uris(request, |mime| mime.starts_with("image/"));
        if !pictures.is_empty() {
            let frames = self.frame_types(&model).await;
            carry_pictures(&mut body, &pictures, frames.as_deref());
        }
        let url = format!("{}/videos", self.base_url);
        let submit = || async {
            let created = crate::openai_compat::send_chat_request(
                &self.client,
                "openrouter",
                &url,
                &self.chat_headers(),
                &body,
                self.rate_limiter.as_ref(),
                Some(crate::provider::SIDE_CALL_TIMEOUT_SECS),
            )
            .await?;
            crate::provider::decode_json::<Value>(created)
                .await
                .and_then(|created| {
                    created
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or_else(|| {
                            ProviderError::InvalidResponse(
                                "the video request answered no id".into(),
                            )
                        })
                })
        };
        let wait = |id: String| {
            let status_url = format!("{url}/{id}");
            media::poll_until(
                "the OpenRouter video job",
                deadline,
                poll_interval,
                move || {
                    let url = status_url.clone();
                    async move {
                        let job = crate::provider::fetch_listing(
                            &self.client,
                            &url,
                            &self.listing_headers(),
                            Some(crate::provider::SIDE_CALL_TIMEOUT_SECS),
                        )
                        .await?;
                        Ok(match job.get("status").and_then(Value::as_str) {
                            Some("completed") => Poll::Done(job),
                            Some("failed" | "cancelled" | "expired") => Poll::Failed(
                                job.get("error")
                                    .and_then(Value::as_str)
                                    .unwrap_or("no reason given")
                                    .to_string(),
                            ),
                            _ => Poll::Running,
                        })
                    }
                },
            )
        };
        let step = format!("openrouter/{model}/video");
        let crate::jobs::Ran {
            id,
            value: done,
            note,
        } = crate::jobs::submit_or_resume(&step, submit, wait).await?;

        // The job is done and OpenRouter has charged for it, whatever becomes
        // of the download: the finished job carries the amount. On the
        // account's own upstream key that amount is OpenRouter's fee alone.
        let own_key = done.pointer("/usage/is_byok").and_then(Value::as_bool) == Some(true);
        let cost = match done.pointer("/usage/cost").and_then(Value::as_f64) {
            Some(_) if own_key => CallCost::Unknown(CostUnknown::UnmodelledTerms),
            Some(usd) => CallCost::Reported(usd),
            None => CallCost::Unknown(CostUnknown::NoUsage),
        };
        media::job_done(cost);

        let content = crate::provider::with_extra_headers(
            self.client
                .get(format!("{url}/{id}/content?index=0"))
                .bearer_auth(&self.api_key),
            &self.extra_headers,
        )
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .map_err(|e| ProviderError::transport("downloading a generated video", &e).after_send())?;
        let bytes = match crate::provider::check_http_response(content, None).await {
            Ok(content) => media::body_bytes(content).await,
            Err(refused) => Err(refused),
        }?;
        let parts = vec![media::typed_blob("video/mp4", bytes, "video.mp4")];
        Ok(media::response(
            crate::jobs::noted(
                media::summary(&format!("openrouter/{model}"), &parts),
                [note],
            ),
            parts,
            cost,
        ))
    }
}

#[cfg(test)]
#[path = "video_tests.rs"]
mod tests;

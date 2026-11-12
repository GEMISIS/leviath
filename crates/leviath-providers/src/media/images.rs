//! Image generation and editing over the OpenAI-shaped images routes xAI and
//! Meta both serve: `POST /images/generations`, and `POST /images/edits` when
//! the stage handed the model an image to start from. OpenRouter's Images API
//! is the same call on one path, `POST /images`, with the reference images in
//! `input_references`.
//!
//! Every result is asked for as `b64_json`, so the bytes arrive in the reply
//! rather than behind a short-lived URL on another host; a reply that sends a
//! URL anyway is downloaded.

use base64::Engine as _;
use leviath_core::mime::Blob;
use serde_json::{Value, json};

use crate::pricing::UnitPrice;
use crate::provider::{InferenceRequest, InferenceResponse, ProviderError, Result};
use crate::responses::client::Endpoint;
use leviath_core::cost::{CallCost, CostUnknown};

/// How a vendor wants a reference image in an edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditShape {
    /// xAI: `image: {url}` for one, `images: [{url}]` for several.
    #[cfg(any(feature = "xai", feature = "xai-subscription"))]
    Xai,
    /// Meta: `images: [{image_url}]`.
    #[cfg(any(feature = "openai", feature = "meta"))]
    Meta,
    /// OpenRouter: `input_references: [{type: "image_url", image_url: {url}}]`,
    /// on the same `/images` path a generation takes.
    #[cfg(feature = "openrouter")]
    OpenRouter,
}

/// The OpenAI-shaped path for a generation from a prompt alone.
#[cfg(any(
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "meta"
))]
const GENERATIONS: &str = "/images/generations";

/// The OpenAI-shaped path for an edit of the images handed over.
#[cfg(any(
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "meta"
))]
const EDITS: &str = "/images/edits";

impl EditShape {
    /// Put `references` in `body` the way this vendor takes them, and say
    /// which path the call goes to.
    fn carry(
        self,
        body: &mut serde_json::Map<String, Value>,
        references: &[String],
    ) -> &'static str {
        match self {
            #[cfg(any(feature = "xai", feature = "xai-subscription"))]
            Self::Xai => match references {
                [] => GENERATIONS,
                [one] => {
                    body.insert("image".into(), json!({ "url": one }));
                    EDITS
                }
                many => {
                    let images: Vec<Value> = many.iter().map(|u| json!({ "url": u })).collect();
                    body.insert("images".into(), json!(images));
                    EDITS
                }
            },
            #[cfg(any(feature = "openai", feature = "meta"))]
            Self::Meta => match references {
                [] => GENERATIONS,
                many => {
                    let images: Vec<Value> =
                        many.iter().map(|u| json!({ "image_url": u })).collect();
                    body.insert("images".into(), json!(images));
                    EDITS
                }
            },
            #[cfg(feature = "openrouter")]
            Self::OpenRouter => {
                if !references.is_empty() {
                    let images: Vec<Value> = references
                        .iter()
                        .map(|u| json!({ "type": "image_url", "image_url": { "url": u } }))
                        .collect();
                    body.insert("input_references".into(), json!(images));
                }
                "/images"
            }
        }
    }
}

/// Where xAI's reply reports the call's cost: `usage.cost_in_usd_ticks`, in
/// ticks of a dollar.
#[cfg(any(feature = "xai", feature = "xai-subscription"))]
pub(crate) const TICKS: (&str, f64) = ("/usage/cost_in_usd_ticks", crate::responses::TICKS_PER_USD);

/// Where OpenRouter's Images API reports the call's cost: `usage.cost`, in
/// US dollars.
#[cfg(feature = "openrouter")]
pub(crate) const USD: (&str, f64) = ("/usage/cost", 1.0);

/// What the call is for, as it is described in the summary line.
pub(crate) struct Route<'a> {
    /// `xai` or `meta`.
    pub(crate) provider: &'a str,
    /// How an edit carries its reference images.
    pub(crate) shape: EditShape,
    /// The type a result is when the reply does not say.
    pub(crate) default_mime: &'a str,
    /// Hints read from the stage's parameters and sent as written.
    pub(crate) hints: &'a [&'a str],
    /// Where the reply reports this call's cost, as a JSON pointer, and how
    /// many of its unit make a dollar ([`TICKS`], [`USD`]); `None` when what
    /// it reports is not the cost.
    pub(crate) reported: Option<(&'a str, f64)>,
    /// The per-image price, for a reply that quotes no cost.
    pub(crate) unit: Option<UnitPrice>,
    /// The token rates, for a model billed by the tokens its reply's `usage`
    /// counts (OpenAI's image models) rather than by the image.
    pub(crate) tokens: Option<crate::ModelPricing>,
    /// Whether the route takes `response_format`. OpenAI's image models
    /// always answer base64 and refuse the key (400 "Unknown parameter").
    pub(crate) response_format: bool,
    /// The `output_format` values the model is known to accept, so the
    /// stage's declared type can be asked for (see [`declared_format`]);
    /// `None` leaves the format to the provider, as every route but
    /// OpenRouter's does for now.
    pub(crate) formats: Option<&'a [String]>,
}

/// The `output_format` name an images route uses for `mime`, when it has
/// one: the five OpenRouter's Images API documents.
pub(crate) fn format_name(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpeg"),
        "image/webp" => Some("webp"),
        "image/svg+xml" => Some("svg"),
        "image/gif" => Some("gif"),
        _ => None,
    }
}

/// The format to ask for: the first type the stage declares, in its order,
/// whose name is among `accepts`. `None` when no declared type is one (a
/// wildcard, a type with no format name, one the model does not list), so
/// nothing is asked and the provider's default stands.
pub(crate) fn declared_format(
    request: &InferenceRequest,
    accepts: &[String],
) -> Option<&'static str> {
    request
        .extra
        .get(crate::provider::OUTPUT_MIMES_KEY)
        .and_then(Value::as_array)?
        .iter()
        .filter_map(Value::as_str)
        .filter_map(format_name)
        .find(|name| accepts.iter().any(|a| a == name))
}

/// Generate or edit images for `request`.
pub(crate) async fn run(
    endpoint: &Endpoint,
    route: &Route<'_>,
    request: &InferenceRequest,
) -> Result<InferenceResponse> {
    let prompt = super::request_text(request);
    if prompt.is_empty() {
        return Err(ProviderError::unsent(format!(
            "{}/{} needs a prompt: the stage handed it no text",
            route.provider, request.model
        )));
    }
    let references = super::data_uris(request, |mime| mime.starts_with("image/"));
    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("prompt".into(), json!(prompt));
    if route.response_format {
        body.insert("response_format".into(), json!("b64_json"));
    }
    if let Some(n) = super::extra_i64(request, "n") {
        body.insert("n".into(), json!(n));
    }
    for key in route.hints {
        if let Some(value) = super::extra_str(request, key) {
            body.insert((*key).into(), json!(value));
        }
    }
    // A format the stage names itself always wins.
    if !body.contains_key("output_format")
        && let Some(format) = route.formats.and_then(|a| declared_format(request, a))
    {
        body.insert("output_format".into(), json!(format));
    }
    let path = route.shape.carry(&mut body, &references);

    let response = endpoint.post_json(path, &Value::Object(body)).await?;
    let response =
        crate::provider::check_http_response(response, endpoint.rate_limiter.as_ref()).await?;
    let reply: Value = crate::provider::decode_json(response).await?;

    let mut parts: Vec<Blob> = Vec::new();
    // The bytes say what an image is. What the reply says is only the
    // fallback for bytes no row recognises: Meta's reply names
    // `output_format: "webp"` (its default) for an edit that hands back a PNG,
    // and a part typed by that label failed the stage's `image/png` artifact.
    let registry = leviath_core::mime::MimeRegistry::builtin();
    // OpenAI names the format once for the whole reply (`output_format:
    // "png"`); xAI and Meta name a type on each image.
    let reply_mime = reply
        .get("output_format")
        .and_then(Value::as_str)
        .map(|format| format!("image/{format}"));
    let entries = reply.get("data").and_then(Value::as_array);
    for (index, entry) in entries.into_iter().flatten().enumerate() {
        // xAI says `mime_type`, Meta `mime`, OpenRouter `media_type`.
        let said = entry
            .get("mime_type")
            .or_else(|| entry.get("mime"))
            .or_else(|| entry.get("media_type"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| reply_mime.clone())
            .unwrap_or_else(|| route.default_mime.to_string());
        let bytes = match (
            entry.get("b64_json").and_then(Value::as_str),
            entry.get("url").and_then(Value::as_str),
        ) {
            (Some(b64), _) => base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| {
                    ProviderError::InvalidResponse(format!("an image was not base64: {e}"))
                })?,
            (None, Some(url)) => {
                super::download(&endpoint.client, url)
                    .await
                    .map_err(ProviderError::after_send)?
                    .0
            }
            (None, None) => continue,
        };
        let mime = registry
            .sniff(&bytes)
            .map_or(said, |sniffed| sniffed.as_str().to_string());
        let name = format!("image-{}.{}", index + 1, super::extension(&mime));
        parts.push(super::blob(&mime, bytes, &name)?);
    }
    if parts.is_empty() {
        return Err(ProviderError::InvalidResponse(format!(
            "{}/{} answered with no image",
            route.provider, request.model
        )));
    }

    let reported = route.reported.and_then(|(pointer, per_usd)| {
        reply
            .pointer(pointer)
            .and_then(Value::as_f64)
            .map(|cost| cost / per_usd)
    });
    // The provider's own figure first. Then the per-image price times the
    // images that came back, for a reply that shows nothing the flat price
    // leaves out. Then the reply's tokens at the model's rates.
    let cost = match (reported, route.unit) {
        (Some(usd), _) => CallCost::Reported(usd),
        (None, Some(_)) if tiered(request, &reply) => {
            CallCost::Unknown(CostUnknown::UnmodelledTerms)
        }
        (None, Some(unit)) => super::unit_cost(Some(unit), Some(parts.len() as f64)),
        (None, None) => match token_cost(route.tokens.as_ref(), &reply) {
            Some(usd) => CallCost::Computed(usd),
            None if route.tokens.is_none() => CallCost::Unknown(CostUnknown::NoPrice),
            None => CallCost::Unknown(CostUnknown::NoUsage),
        },
    };
    let summary = super::summary(&format!("{}/{}", route.provider, request.model), &parts);
    Ok(super::response(summary, parts, cost))
}

/// Whether the call asked for, or the reply names, a quality or a size: a
/// per-image price is for the model's ordinary image, and a model that
/// takes either prices the others apart.
fn tiered(request: &InferenceRequest, reply: &Value) -> bool {
    ["quality", "size"].iter().any(|key| {
        let asked = request.extra.get(*key).and_then(Value::as_str);
        let made = reply.get(*key).and_then(Value::as_str);
        asked.or(made).is_some_and(|value| value != "auto")
    })
}

/// What a reply's `usage` costs at `rates`: input and output tokens, as the
/// route counts them (image tokens included).
pub(crate) fn token_cost(rates: Option<&crate::ModelPricing>, reply: &Value) -> Option<f64> {
    let rates = rates?;
    let usage = reply.get("usage")?;
    let count = |key: &str| usage.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    Some(
        (count("input_tokens") * rates.input_per_mtok
            + count("output_tokens") * rates.output_per_mtok)
            / 1_000_000.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::PriceUnit;
    use crate::provider::{ContentBlock, Message, MessageContent};
    use crate::responses::client::Auth;
    use leviath_core::mime::{MimeRegistry, MimeType, Part};
    use leviath_testkit::spawn_mock_sequence;

    fn endpoint(url: &str) -> Endpoint {
        Endpoint::new(reqwest::Client::new(), url, Auth::Key("k".into()))
    }

    fn request(prompt: &str, images: usize) -> InferenceRequest {
        let mut blocks = vec![ContentBlock::Text {
            text: prompt.into(),
        }];
        for i in 0..images {
            let name = format!("ref{i}.png");
            let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![1, 2]).named(&name);
            let part = Part::stored(blob.describe(&MimeRegistry::builtin())).named(&name);
            blocks.push(ContentBlock::Mime {
                part: part.blob().unwrap().clone(),
                data: "AQI=".into(),
                name: part.name.clone(),
                deliver: None,
                remote: None,
            });
        }
        InferenceRequest {
            system: vec![],
            messages: vec![Message {
                role: "user".into(),
                content: MessageContent::Blocks(blocks),
                cache_breakpoint: false,
                reasoning: None,
            }],
            model: "grok-imagine-image".into(),
            max_tokens: 0,
            temperature: 0.0,
            tools: vec![],
            extra: json!({ "n": 2, "aspect_ratio": "16:9", "ignored": "x" }),
            request_timeout_secs: None,
        }
    }

    fn route(shape: EditShape, reported: bool) -> Route<'static> {
        Route {
            provider: "xai",
            shape,
            default_mime: "image/jpeg",
            hints: &["aspect_ratio", "resolution", "quality", "size"],
            reported: reported.then_some(TICKS),
            unit: Some(UnitPrice {
                usd: 0.01,
                unit: PriceUnit::Image,
            }),
            tokens: None,
            response_format: true,
            formats: None,
        }
    }

    /// The declared type is asked for only when the model lists its format:
    /// the first declared type, in order, that maps to a listed format.
    #[test]
    fn the_declared_type_is_asked_for_only_in_a_format_the_model_lists() {
        let declaring = |mimes: Value| {
            let mut req = request("x", 0);
            req.extra = json!({ "output_mimes": mimes });
            req
        };
        let all: Vec<String> = ["png", "jpeg", "webp", "svg", "gif"]
            .map(String::from)
            .to_vec();
        for (mime, name) in [
            ("image/png", "png"),
            ("image/jpeg", "jpeg"),
            ("image/webp", "webp"),
            ("image/svg+xml", "svg"),
            ("image/gif", "gif"),
        ] {
            assert_eq!(format_name(mime), Some(name));
            assert_eq!(declared_format(&declaring(json!([mime])), &all), Some(name));
        }
        let flux = vec!["jpeg".to_string(), "png".to_string()];
        // Several declared: the first the model lists, in declared order.
        assert_eq!(
            declared_format(
                &declaring(json!(["image/webp", "image/png", "image/jpeg"])),
                &flux
            ),
            Some("png")
        );
        // A wildcard, a type with no format name, one the model does not
        // list, a model listing none, or nothing declared: nothing asked.
        for nothing in [
            json!(["image/*"]),
            json!(["image/bmp"]),
            json!(["image/webp"]),
            json!([7, "text/markdown"]),
            json!("image/png"),
        ] {
            assert_eq!(
                declared_format(&declaring(nothing.clone()), &flux),
                None,
                "{nothing}"
            );
        }
        assert_eq!(declared_format(&declaring(json!(["image/png"])), &[]), None);
        assert_eq!(declared_format(&request("x", 0), &flux), None);
    }

    /// Turned on, the declared format is sent; a format the stage names
    /// itself wins; turned off, nothing is added.
    #[tokio::test]
    async fn the_declared_format_is_sent_unless_the_stage_names_one() {
        let reply = json!({ "data": [ { "b64_json": "SlBFRw==" } ] })
            .to_string()
            .into_bytes();
        let (url, bodies) = spawn_mock_sequence(vec![
            (200, "OK", reply.clone()),
            (200, "OK", reply.clone()),
            (200, "OK", reply),
        ])
        .await;
        let flux = vec!["jpeg".to_string(), "png".to_string()];
        let mut on: Route<'_> = route(EditShape::Meta, false);
        on.hints = &["output_format"];
        on.formats = Some(&flux);
        let mut png = request("x", 0);
        png.extra = json!({ "output_mimes": ["image/png"] });
        run(&endpoint(&url), &on, &png).await.unwrap();
        let mut named = png.clone();
        named.extra["output_format"] = json!("jpeg");
        run(&endpoint(&url), &on, &named).await.unwrap();
        run(&endpoint(&url), &route(EditShape::Meta, false), &png)
            .await
            .unwrap();
        let bodies = bodies.lock().unwrap().clone();
        let format =
            |i: usize| serde_json::from_str::<Value>(&bodies[i]).unwrap()["output_format"].clone();
        assert_eq!(format(0), "png");
        assert_eq!(format(1), "jpeg");
        assert_eq!(format(2), Value::Null);
    }

    /// OpenAI's shape, as measured: no `response_format` sent, the format
    /// named once for the reply, and the cost from the tokens `usage` counts.
    #[tokio::test]
    async fn an_openai_reply_is_typed_by_its_format_and_priced_by_its_tokens() {
        let reply = json!({
            "output_format": "png",
            "data": [ { "b64_json": "UE5H" } ],
            "usage": { "input_tokens": 1_000_000, "output_tokens": 500_000 }
        });
        let (url, bodies) =
            spawn_mock_sequence(vec![(200, "OK", reply.to_string().into_bytes())]).await;
        let mut openai = route(EditShape::Meta, false);
        openai.unit = None;
        openai.response_format = false;
        openai.tokens = Some(crate::ModelPricing::flat(2.0, 8.0));
        let response = run(&endpoint(&url), &openai, &request("draw", 0))
            .await
            .expect("an image");
        assert_eq!(response.parts[0].mime_type.as_str(), "image/png");
        assert_eq!(response.tokens_used.priced_cost(None), Some(6.0));
        assert!(!bodies.lock().unwrap()[0].contains("response_format"));
        assert_eq!(token_cost(None, &reply), None);
        assert_eq!(token_cost(openai.tokens.as_ref(), &json!({})), None);
    }

    /// A flat per-image price is for the model's ordinary image. A call that
    /// asked for a quality or a size, or a reply that names one, was billed
    /// on terms the flat price leaves out; `auto` is the ordinary image.
    #[tokio::test]
    async fn an_image_at_a_quality_or_size_the_flat_price_leaves_out_is_unknown() {
        use leviath_core::cost::{CallCost, CostUnknown};
        let image = json!({ "data": [ { "b64_json": "UE5H" } ] });
        let sized = json!({ "size": "1536x1024", "data": [ { "b64_json": "UE5H" } ] });
        let auto = json!({ "quality": "auto", "data": [ { "b64_json": "UE5H" } ] });
        let (url, _) = spawn_mock_sequence(vec![
            (200, "OK", image.to_string().into_bytes()),
            (200, "OK", sized.to_string().into_bytes()),
            (200, "OK", auto.to_string().into_bytes()),
            (200, "OK", image.to_string().into_bytes()),
            (200, "OK", image.to_string().into_bytes()),
        ])
        .await;
        let flat = route(EditShape::Xai, false);
        let mut asked = request("draw", 0);
        asked.extra = json!({ "quality": "high" });
        let mut costs = Vec::new();
        for request in [&asked, &request("draw", 0), &request("draw", 0)] {
            let response = run(&endpoint(&url), &flat, request)
                .await
                .expect("an image");
            costs.push(response.tokens_used.cost(None));
        }
        assert_eq!(
            costs,
            [
                CallCost::Unknown(CostUnknown::UnmodelledTerms),
                CallCost::Unknown(CostUnknown::UnmodelledTerms),
                CallCost::Computed(0.01),
            ]
        );

        // No per-image price and no token rates: nothing to price it by.
        let mut bare = route(EditShape::Xai, false);
        bare.unit = None;
        let response = run(&endpoint(&url), &bare, &request("draw", 0))
            .await
            .expect("an image");
        assert_eq!(
            response.tokens_used.cost(None),
            CallCost::Unknown(CostUnknown::NoPrice)
        );

        // Token rates, and a reply that says nothing of the tokens it used.
        bare.tokens = Some(crate::ModelPricing::flat(2.0, 8.0));
        let response = run(&endpoint(&url), &bare, &request("draw", 0))
            .await
            .expect("an image");
        assert_eq!(
            response.tokens_used.cost(None),
            CallCost::Unknown(CostUnknown::NoUsage)
        );
    }

    #[tokio::test]
    async fn a_generation_decodes_every_image_and_prices_itself() {
        let reply = json!({
            "data": [ { "b64_json": "SlBFRw==", "mime_type": "image/jpeg" }, { "b64_json": "UE5H", "mime": "image/png" }, {} ],
            "usage": { "cost_in_usd_ticks": 400000000 }
        });
        let (url, bodies) =
            spawn_mock_sequence(vec![(200, "OK", reply.to_string().into_bytes())]).await;
        let response = run(
            &endpoint(&url),
            &route(EditShape::Xai, true),
            &request("a red square", 0),
        )
        .await
        .expect("images");
        assert_eq!(response.parts.len(), 2);
        assert_eq!(response.parts[0].bytes, b"JPEG");
        assert_eq!(response.parts[1].name.as_deref(), Some("image-2.png"));
        assert_eq!(response.tokens_used.priced_cost(None), Some(0.04));
        let body: Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
        assert_eq!(body["response_format"], "b64_json");
        assert_eq!(body["n"], 2);
        assert_eq!(body["aspect_ratio"], "16:9");
        assert!(body.get("ignored").is_none());
        assert!(response.content.contains("xai/grok-imagine-image"));
    }

    #[tokio::test]
    async fn an_edit_carries_its_references_in_each_vendors_shape() {
        let reply = json!({ "data": [ { "b64_json": "SlBFRw==" } ] })
            .to_string()
            .into_bytes();
        let (url, bodies) = spawn_mock_sequence(vec![
            (200, "OK", reply.clone()),
            (200, "OK", reply.clone()),
            (200, "OK", reply),
        ])
        .await;
        let xai = route(EditShape::Xai, false);
        run(&endpoint(&url), &xai, &request("bluer", 1))
            .await
            .unwrap();
        let priced = run(&endpoint(&url), &xai, &request("bluer", 2))
            .await
            .unwrap();
        assert_eq!(
            priced.tokens_used.priced_cost(None),
            Some(0.01),
            "the unit price, no ticks read"
        );
        let meta = run(
            &endpoint(&url),
            &route(EditShape::Meta, false),
            &request("bluer", 1),
        )
        .await
        .unwrap();
        assert_eq!(meta.parts[0].mime_type.as_str(), "image/jpeg");
        let bodies = bodies.lock().unwrap().clone();
        let one: Value = serde_json::from_str(&bodies[0]).unwrap();
        assert!(
            one["image"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png")
        );
        let two: Value = serde_json::from_str(&bodies[1]).unwrap();
        assert_eq!(two["images"].as_array().unwrap().len(), 2);
        let meta_body: Value = serde_json::from_str(&bodies[2]).unwrap();
        assert!(meta_body["images"][0]["image_url"].is_string());
    }

    #[tokio::test]
    async fn a_url_result_is_downloaded() {
        let image = leviath_testkit::spawn_mock_server_with_headers(
            200,
            "OK",
            "Content-Type: image/jpeg\r\n",
            b"JPEG".to_vec(),
        )
        .await;
        let reply = json!({ "data": [ { "url": image } ] });
        let (url, _) = spawn_mock_sequence(vec![(200, "OK", reply.to_string().into_bytes())]).await;
        let response = run(
            &endpoint(&url),
            &route(EditShape::Xai, true),
            &request("x", 0),
        )
        .await
        .unwrap();
        assert_eq!(response.parts[0].bytes, b"JPEG");
    }

    #[tokio::test]
    async fn no_prompt_no_image_bad_base64_and_refusals_are_errors() {
        let empty = run(
            &endpoint("http://127.0.0.1:1"),
            &route(EditShape::Xai, true),
            &request("", 0),
        )
        .await
        .unwrap_err();
        assert!(empty.to_string().contains("needs a prompt"));
        assert_eq!(empty.request_sent(), Some(false));
        let (url, _) = spawn_mock_sequence(vec![
            (200, "OK", b"{\"data\":[]}".to_vec()),
            (200, "OK", b"{\"data\":[{\"b64_json\":\"@@@\"}]}".to_vec()),
            (400, "Bad Request", b"moderated".to_vec()),
        ])
        .await;
        let route = route(EditShape::Xai, true);
        let none = run(&endpoint(&url), &route, &request("x", 0))
            .await
            .unwrap_err();
        assert!(none.to_string().contains("no image"));
        let bad = run(&endpoint(&url), &route, &request("x", 0))
            .await
            .unwrap_err();
        assert!(bad.to_string().contains("base64"));
        let refused = run(&endpoint(&url), &route, &request("x", 0))
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("moderated"));
    }

    #[tokio::test]
    async fn every_way_an_image_call_can_fail_is_an_error() {
        let route = route(EditShape::Xai, false);
        let req = request("x", 0);
        // Nothing listening.
        assert!(
            run(&endpoint("http://127.0.0.1:9"), &route, &req)
                .await
                .is_err()
        );
        // A body that is not JSON.
        let (garbled, _) = spawn_mock_sequence(vec![(200, "OK", b"not json".to_vec())]).await;
        assert!(run(&endpoint(&garbled), &route, &req).await.is_err());
        // An image at a URL nobody serves.
        let unreachable = json!({ "data": [ { "url": "http://127.0.0.1:9/i.png" } ] });
        let (url, _) =
            spawn_mock_sequence(vec![(200, "OK", unreachable.to_string().into_bytes())]).await;
        let err = run(&endpoint(&url), &route, &req).await.unwrap_err();
        // The download never connects, and the generation before it was
        // answered: a request went out.
        assert_eq!(err.request_sent(), Some(true), "{err}");
        // An image whose type is no type.
        let untyped = json!({ "data": [ { "b64_json": "SlBFRw==", "mime_type": "not a type" } ] });
        let (url, _) =
            spawn_mock_sequence(vec![(200, "OK", untyped.to_string().into_bytes())]).await;
        let err = run(&endpoint(&url), &route, &req).await.unwrap_err();
        assert!(err.to_string().contains("unreadable type"), "{err}");
    }
}

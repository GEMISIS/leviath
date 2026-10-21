//! The compiled catalogue against the providers it describes: who claims each
//! row's model on a bare name, and what each row says it costs.

use crate::Provider;
use crate::capabilities::builtin_catalog;

fn client() -> reqwest::Client {
    crate::provider::build_http_client(None).expect("a test client builds")
}

/// Every keyed vendor whose catalogue is compiled in, unprimed and with no
/// config, as a machine that has just been given its keys has them.
fn vendors() -> Vec<(&'static str, Box<dyn Provider>)> {
    let key = || "k".to_string();
    vec![
        (
            "anthropic",
            Box::new(crate::AnthropicProvider::new(client(), key())) as Box<dyn Provider>,
        ),
        (
            "openai",
            Box::new(crate::OpenAIProvider::new(client(), key())),
        ),
        (
            "google",
            Box::new(crate::GeminiProvider::new(client(), key())),
        ),
        (
            "meta",
            Box::new(crate::meta::MetaProvider::new(client(), key())),
        ),
        (
            "xai",
            Box::new(crate::xai::XaiProvider::new(
                client(),
                crate::xai::Auth::Key(key()),
            )),
        ),
        (
            "meshy",
            Box::new(crate::MeshyProvider::new(client(), key())),
        ),
        (
            "bedrock",
            Box::new(crate::BedrockProvider::new(client(), key())),
        ),
        (
            "openrouter",
            Box::new(crate::OpenRouterProvider::new(client(), key())),
        ),
    ]
}

/// A bare model name goes to whichever configured provider claims it. A
/// vendor claiming another vendor's model by the shape of its id sends the
/// stage somewhere that cannot run it: `google` took `muse-image-1.0` for the
/// `-image` in it, and `openai` took `muse-voice-transcribe-1.0` for the
/// `transcribe`. Every row is listed by its owner and claimed by no other
/// vendor. The gateways are left out of "no other": serving everyone's models
/// is what a gateway is for.
#[test]
fn every_compiled_model_is_claimed_by_its_own_vendor_alone() {
    let vendors = vendors();
    let gateways = ["openrouter", "bedrock"];
    for row in builtin_catalog() {
        let Some((_, owner)) = vendors.iter().find(|(n, _)| *n == row.provider) else {
            continue;
        };
        assert_eq!(
            owner.lists_model(row.id).as_deref(),
            Some(row.id),
            "{} does not list its own {}",
            row.provider,
            row.id
        );
        for (name, other) in &vendors {
            if *name == row.provider || gateways.contains(name) {
                continue;
            }
            assert_eq!(
                other.serves_model(row.id),
                None,
                "{name} claims {}/{}",
                row.provider,
                row.id
            );
        }
    }
}

/// `catalog` prices a row without a provider in hand, so it answers from the
/// shipped table directly. It has to be the figure the provider itself would
/// bill by, or a client ranks an offer on a price no run is charged.
#[test]
fn a_rows_price_is_its_providers_price() {
    let vendors = vendors();
    let mut priced = 0;
    for row in builtin_catalog() {
        let expected = match vendors.iter().find(|(n, _)| *n == row.provider) {
            Some((_, provider)) => provider.pricing(row.id),
            // The sign-in routes bill a subscription, not the call.
            None => Some(crate::ModelPricing::flat(0.0, 0.0)),
        };
        assert_eq!(row.pricing(), expected, "{}/{}", row.provider, row.id);
        priced += usize::from(expected.is_some());
    }
    assert!(priced > 0, "the table priced nothing");
}

/// The media models this build runs are the ones a client most needs a price
/// for and the per-token columns cannot give: each is priced by the token or
/// by the unit (an image, a second of video, a clip, a million characters).
#[test]
fn every_compiled_media_model_has_a_price() {
    let media: Vec<_> = builtin_catalog()
        .into_iter()
        .filter(|row| !matches!(row.provider, "meshy" | "openrouter"))
        .filter(|row| {
            let mime = crate::mime_tables::builtin_mime(row.provider, row.id);
            let makes_media = !mime.output.iter().any(|t| t.starts_with("text/"));
            let hears = mime.input.iter().any(|t| t.starts_with("audio/"));
            makes_media || hears
        })
        .collect();
    assert!(media.len() > 10, "{} media rows", media.len());
    let unpriced: Vec<String> = media
        .iter()
        .filter(|row| row.pricing().is_none())
        .map(|row| format!("{}/{}", row.provider, row.id))
        .collect();
    assert!(unpriced.is_empty(), "unpriced media models: {unpriced:?}");
}

/// A `[model_capabilities]` rate comes first wherever the provider reads one,
/// and nowhere else: a subscription stays a known zero and a gateway's row
/// stays unpriced until its listing quotes it.
#[test]
fn a_configured_rate_prices_a_row_where_its_provider_reads_one() {
    let configured = crate::ModelCapabilityOverride {
        input_per_mtok: Some(7.0),
        output_per_mtok: Some(9.0),
        ..Default::default()
    };
    let row = |provider: &str| {
        builtin_catalog()
            .into_iter()
            .find(|row| row.provider == provider)
            .unwrap_or_else(|| panic!("{provider} names a model"))
    };
    let negotiated = crate::ModelPricing::flat(7.0, 9.0);
    for provider in ["anthropic", "bedrock", "meta"] {
        assert_eq!(
            row(provider).pricing_with(Some(&configured)),
            Some(negotiated),
            "{provider}"
        );
    }
    assert_eq!(
        row("codex").pricing_with(Some(&configured)),
        Some(crate::ModelPricing::flat(0.0, 0.0))
    );
    assert_eq!(row("openrouter").pricing_with(Some(&configured)), None);
    // Half a rate card is none: the table stands.
    let half = crate::ModelCapabilityOverride {
        input_per_mtok: Some(7.0),
        ..Default::default()
    };
    let anthropic = row("anthropic");
    assert_eq!(anthropic.pricing_with(Some(&half)), anthropic.pricing());
}

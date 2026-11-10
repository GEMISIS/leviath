//! How a model's name is read: what makes two spellings the same model, and
//! what a user's model setting names once its provider prefix is off.

/// A model's identity, independent of the route it is reached by.
///
/// The same model is spelled differently depending on the provider serving it:
/// `gpt-5.5` on OpenAI is `openai/gpt-5.5` on OpenRouter, and
/// `claude-opus-5` is `anthropic/claude-opus-5`. Comparing the raw strings
/// makes one model look like two, which is what lets a route preference be read
/// as a model preference.
///
/// The last segment is the model; anything before it is the vendor namespace a
/// gateway prefixes. A local model with no slash (`qwen3.5:9b`) is already its
/// own key.
///
/// Public because every layer that compares two model names has to compare them
/// the same way: `lev validate` and the provider registry call this rather than
/// each keeping a copy of "what counts as the same model", which would not stay
/// equal.
pub fn model_key(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// The model id an `override_model` or `fallback_model` setting actually names,
/// with the leading `<provider>/` of the provider it is asked of taken off.
///
/// Both settings are bare model ids that pair with the head of
/// `provider_order`, but they
/// are easy to write qualified: `--model` and `[providers] fallback_order`
/// both take `provider/model`, an OpenRouter id such as
/// `deepseek/deepseek-v4-flash` already looks qualified, and a console that
/// lists models as `provider/id` hands the pair back in one string. Sent
/// verbatim, `provider_order = ["ollama"]` with `override_model =
/// "ollama/qwen3.8:latest"` reached Ollama as a request for a model called
/// `ollama/qwen3.8:latest`, which it does not have. The prefix names the
/// provider the setting already names, so dropping it loses nothing.
///
/// The one catalog that can legitimately start an id with its own name is
/// OpenRouter's (`openrouter/auto` is its router). Those ids have no further
/// `/`, while a mistakenly qualified OpenRouter model always does
/// (`openrouter/deepseek/deepseek-v4-flash`), which is how the two are told
/// apart. Anything that does not begin with the provider's name is returned as
/// written.
pub fn bare_user_model<'a>(provider: &str, model: &'a str) -> &'a str {
    let Some(rest) = model
        .strip_prefix(provider)
        .and_then(|rest| rest.strip_prefix('/'))
        .filter(|rest| !rest.is_empty())
    else {
        return model;
    };
    if provider == "openrouter" && !rest.contains('/') {
        return model;
    }
    rest
}

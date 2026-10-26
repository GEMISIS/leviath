//! The system prompt for a model OpenRouter's upstream caches by prefix on
//! its own, without `cache_control` markers.
//!
//! Such an upstream (Meta's Muse models are one) only reuses a prefix when
//! the request lands where the last one did, and OpenRouter keeps a request
//! there only when it carries a `prompt_cache_key`. Without one, an agent
//! creator run of 270k-750k prompt tokens read under 1% from cache; with
//! one, every call after the first read about 98% (a measured probe: 21 745
//! of 22 256 tokens on the second call, a seventh of the price).
//!
//! The prefix also has to reach the upstream as one piece. Sent as one
//! system message per block, the same key cached nothing, or only part of
//! the prompt, on the second call, and everything from the third; with the
//! stable blocks joined into one message, the second call already read
//! nearly all of it. So the stable head of the system prompt (every block
//! up to the first that moves) is sent as one system message, the blocks
//! that move as a second, and the key is the head's hash: requests that
//! share the head share the key, and so the upstream that holds it.

use leviath_core::Volatility;
use leviath_core::mime::sha256_hex;

use crate::provider::SystemBlock;

/// Whether `block` belongs to the stable head: a region declared stable, or
/// a hint or a tool preamble, the same on every call. A block with no region
/// that is not a hint (the note about tools the history calls and the stage
/// does not offer) varies from run to run, so it stays out of the head and
/// out of the key.
fn steady(block: &SystemBlock) -> bool {
    if block.region.is_empty() {
        block.is_hint()
    } else {
        block.volatility == Volatility::Stable
    }
}

/// The texts of `blocks`, trimmed, the empty ones dropped, as one message.
fn joined<'a>(blocks: impl Iterator<Item = &'a SystemBlock>) -> String {
    let texts: Vec<&str> = blocks
        .map(|block| block.text.trim())
        .filter(|text| !text.is_empty())
        .collect();
    texts.join("\n\n")
}

/// `system` as at most two system messages: the stable head, then the rest.
pub(super) fn system_messages(system: &[SystemBlock]) -> Vec<serde_json::Value> {
    let head = system.iter().take_while(|block| steady(block)).count();
    [
        joined(system.iter().take(head)),
        joined(system.iter().skip(head)),
    ]
    .into_iter()
    .filter(|text| !text.is_empty())
    .map(|text| serde_json::json!({ "role": "system", "content": text }))
    .collect()
}

/// Give the request body `fields`, for `model` with `system`, the
/// [`cache_key`] that keeps it on the upstream holding its stable head,
/// unless the stage named a `prompt_cache_key` of its own.
pub(super) fn keep_on_upstream(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    model: &str,
    system: &[SystemBlock],
) {
    if let Some(key) = cache_key(model, system) {
        fields
            .entry("prompt_cache_key")
            .or_insert(serde_json::json!(key));
    }
}

/// The `prompt_cache_key` for `model` with `system`: the hash of its stable
/// head, or `None` when there is no head to keep together.
fn cache_key(model: &str, system: &[SystemBlock]) -> Option<String> {
    let head = joined(system.iter().take_while(|block| steady(block)));
    if head.is_empty() {
        return None;
    }
    let hash = sha256_hex(format!("{model}\n{head}").as_bytes());
    let short: String = hash.chars().take(32).collect();
    Some(format!("lev-{short}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::CacheHint;

    fn block(text: &str, region: &str, volatility: Volatility) -> SystemBlock {
        SystemBlock {
            text: text.to_string(),
            cache_hint: CacheHint::Always,
            region: region.to_string(),
            volatility,
        }
    }

    /// The agent creator's shape: a hint, stable regions, then what grows
    /// and what is rewritten.
    fn creator() -> Vec<SystemBlock> {
        vec![
            block("Batch your reads.", "", Volatility::Rewritten),
            block("## task\nDesign it.", "task", Volatility::Stable),
            block("  ", "constraints", Volatility::Stable),
            block(
                "## models\n- a, b: takes text/*",
                "models",
                Volatility::Stable,
            ),
            block("## files\nmesh.obj", "files", Volatility::Grows),
            block("## kinds\n- mesh", "kinds", Volatility::Stable),
            block(
                "## stage\nStudy it.",
                "stage_instructions",
                Volatility::Rewritten,
            ),
        ]
    }

    #[test]
    fn the_stable_head_is_one_message_and_what_moves_is_another() {
        let messages = system_messages(&creator());
        assert_eq!(
            messages,
            vec![
                serde_json::json!({
                    "role": "system",
                    "content": "Batch your reads.\n\n## task\nDesign it.\n\n## models\n- a, b: takes text/*",
                }),
                serde_json::json!({
                    "role": "system",
                    "content": "## files\nmesh.obj\n\n## kinds\n- mesh\n\n## stage\nStudy it.",
                }),
            ]
        );
        // All stable, or all moving: one message; nothing: none.
        let stable = [block("a", "a", Volatility::Stable)];
        assert_eq!(system_messages(&stable).len(), 1);
        let moving = [block("a", "a", Volatility::Grows)];
        assert_eq!(system_messages(&moving)[0]["content"], "a");
        assert_eq!(system_messages(&[]), Vec::<serde_json::Value>::new());
    }

    #[test]
    fn the_unoffered_tools_note_is_not_part_of_the_head_or_the_key() {
        // All stable, so the note, last and with no region, would otherwise
        // join the head and move the key with every set of tools.
        let stable = vec![
            block("Batch your reads.", "", Volatility::Stable),
            block("## task\nDesign it.", "task", Volatility::Stable),
        ];
        let mut noted = stable.clone();
        let mut note = block("Earlier turns called: web_fetch.", "", Volatility::Stable);
        note.cache_hint = CacheHint::UntilChanged;
        noted.push(note);

        assert_eq!(
            cache_key("meta/muse-spark-1.3", &noted),
            cache_key("meta/muse-spark-1.3", &stable)
        );
        let messages = system_messages(&noted);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], system_messages(&stable)[0]);
        assert_eq!(messages[1]["content"], "Earlier turns called: web_fetch.");
    }

    #[test]
    fn the_key_is_the_stable_head_s_and_the_model_s() {
        let blocks = creator();
        let key = cache_key("meta/muse-spark-1.3", &blocks).expect("a head");
        assert!(key.starts_with("lev-"), "{key}");
        assert_eq!(key.len(), 4 + 32);
        // What moves after the head leaves the key alone.
        let mut later = blocks.clone();
        later[4].text = "## files\nmesh.obj\nlevel.obj".to_string();
        assert_eq!(cache_key("meta/muse-spark-1.3", &later), Some(key.clone()));
        // A different head, or model, is a different key.
        let mut other = blocks.clone();
        other[1].text = "## task\nDesign another.".to_string();
        assert_ne!(cache_key("meta/muse-spark-1.3", &other), Some(key.clone()));
        assert_ne!(cache_key("meta/muse-wonder", &blocks), Some(key));
        // No head, no key.
        let moving = [block("a", "a", Volatility::Grows)];
        assert_eq!(cache_key("m", &moving), None);
        assert_eq!(cache_key("m", &[]), None);
    }
}

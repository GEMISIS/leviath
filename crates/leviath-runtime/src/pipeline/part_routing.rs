//! Splitting a reply's produced parts across regions by their mime type.
//!
//! A stage declares `[stages.<name>.output_routing]` (`"image/*" = "artwork"`)
//! to send the parts a model produces somewhere other than `conversation` -
//! an image the model drew into a region a later stage reads, say, leaving the
//! turn's text where it always went. This module answers the one question both
//! recording paths ([`super::response::store_reply`] for a reply with no tool
//! calls, [`super::tool_results::apply_tool_results_with_parts`] for one with
//! them) ask: of the parts the model produced, which stay in the conversation
//! and which are routed, and to where.

use std::collections::BTreeMap;

use leviath_core::blueprint::Stage;
use leviath_core::mime::Part;

use crate::components::ContextWindow;

/// A reply's produced parts, split into those that stay in the conversation and
/// those routed to other regions.
pub(crate) struct RoutedParts {
    /// Parts no routing rule matched: they belong to the conversation turn,
    /// beside the reply's text.
    pub(crate) kept: Vec<Part>,
    /// The routed parts, grouped by target region. Ordered by region name so
    /// the same reply always writes its regions in the same order. Empty in
    /// the common case: no `output_routing`, or none of it matched.
    pub(crate) routed: Vec<(String, Vec<Part>)>,
}

/// Split `parts` by `stage`'s [`Stage::output_routing`]. A part whose mime type
/// matches a rule goes to that rule's region (most specific pattern wins); the
/// rest are kept for the conversation. With no stage, or no matching rule,
/// every part is kept.
pub(crate) fn split(stage: Option<&Stage>, parts: &[Part]) -> RoutedParts {
    let mut kept = Vec::new();
    let mut groups: BTreeMap<String, Vec<Part>> = BTreeMap::new();
    for part in parts {
        match stage.and_then(|s| s.route_for_mime(&part.mime_type)) {
            Some(region) => groups
                .entry(region.to_string())
                .or_default()
                .push(part.clone()),
            None => kept.push(part.clone()),
        }
    }
    RoutedParts {
        kept,
        routed: groups.into_iter().collect(),
    }
}

/// Write each routed group into its region as one entry. Text belongs to the
/// conversation turn, so a routed entry carries only produced parts and is
/// plain [`EntryKind::Text`](leviath_core::EntryKind::Text): a pinned region
/// lifts its stored parts into the leading user turn, and a sliding window
/// renders them as a user message, so either way the next stage's model sees
/// the bytes. A region the write cannot reach (unknown, over budget) is
/// skipped rather than fatal - the conversation still recorded the reply.
pub(crate) fn store_routed(window: &mut ContextWindow, routed: &RoutedParts) {
    for (region, parts) in &routed.routed {
        let content = leviath_core::region::EntryContent::from_parts(parts.clone());
        let tokens = content.tokens_hint();
        if let Err(e) = window.add_assistant_turn_content(
            region,
            leviath_core::EntryKind::Text,
            content,
            tokens,
            None,
        ) {
            tracing::warn!(region = %region, "[mime] produced parts not routed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::blueprint::ModelConfig;
    use leviath_core::mime::{Blob, MimeRegistry, MimeType};

    fn stored_part(mime: &str, name: &str) -> Part {
        let reg = MimeRegistry::builtin();
        let blob = Blob::new(MimeType::parse(mime).unwrap(), vec![1, 2, 3]).named(name);
        Part::stored(blob.describe(&reg)).named(name)
    }

    fn stage_routing(rules: &[(&str, &str)]) -> Stage {
        let mut stage = Stage::new(
            "draw".to_string(),
            ModelConfig::new("openrouter".to_string(), "m".to_string()),
        );
        for (pattern, region) in rules {
            stage
                .output_routing
                .insert((*pattern).to_string(), (*region).to_string());
        }
        stage
    }

    #[test]
    fn no_stage_keeps_every_part() {
        let parts = vec![stored_part("image/png", "a.png"), Part::text("hi")];
        let routed = split(None, &parts);
        assert!(routed.routed.is_empty());
        assert_eq!(routed.kept.len(), 2);
    }

    #[test]
    fn a_matching_rule_routes_the_part_and_keeps_the_rest() {
        let stage = stage_routing(&[("image/*", "artwork")]);
        let parts = vec![
            Part::text("here it is"),
            stored_part("image/png", "hero.png"),
        ];
        let routed = split(Some(&stage), &parts);
        assert!(!routed.routed.is_empty());
        assert_eq!(routed.kept.len(), 1, "the text stays in the conversation");
        assert_eq!(routed.kept[0].inline_text(), Some("here it is"));
        assert_eq!(routed.routed.len(), 1);
        let (region, parts) = &routed.routed[0];
        assert_eq!(region, "artwork");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].name.as_deref(), Some("hero.png"));
    }

    #[test]
    fn store_routed_writes_each_group_into_its_region() {
        use leviath_core::region::{Region, RegionKind};
        let mut window = ContextWindow::new(100_000);
        window.add_region(Region::new(
            "artwork".to_string(),
            RegionKind::Pinned,
            100_000,
        ));
        let routed = RoutedParts {
            kept: Vec::new(),
            routed: vec![(
                "artwork".to_string(),
                vec![stored_part("image/png", "a.png")],
            )],
        };
        store_routed(&mut window, &routed);
        let region = window.get_region("artwork").unwrap();
        assert_eq!(region.content.len(), 1);
        assert_eq!(region.content[0].content.stored_count(), 1);
    }

    #[test]
    fn store_routed_skips_a_region_the_window_does_not_carry() {
        // A target no region declares is a warning, not a panic: the reply's
        // conversation entry has already been recorded.
        let mut window = ContextWindow::new(100_000);
        let routed = RoutedParts {
            kept: Vec::new(),
            routed: vec![("ghost".to_string(), vec![stored_part("image/png", "a.png")])],
        };
        store_routed(&mut window, &routed);
        assert!(window.get_region("ghost").is_none());
    }

    #[test]
    fn parts_for_two_regions_group_by_region_name() {
        let stage = stage_routing(&[("image/*", "artwork"), ("application/pdf", "docs")]);
        let parts = vec![
            stored_part("application/pdf", "spec.pdf"),
            stored_part("image/png", "hero.png"),
        ];
        let routed = split(Some(&stage), &parts);
        assert!(routed.kept.is_empty());
        // Ordered by region name: artwork before docs.
        assert_eq!(routed.routed[0].0, "artwork");
        assert_eq!(routed.routed[1].0, "docs");
    }
}

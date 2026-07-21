//! Inter-agent context transforms: seed a freshly-spawned child agent's context
//! from its parent's, when the blueprints declare a mapping.
//!
//! A [`Blueprint`](leviath_core::Blueprint) may declare
//! [`ContextTransform`](leviath_core::blueprint::ContextTransform)s — `{from_blueprint,
//! to_blueprint, mappings}` — describing how a parent's context regions flow into
//! a child's when the parent (blueprint A) spawns a child (blueprint B). This is
//! how an agent hands work down the tree: the planner's plan region becomes the
//! implementer's task region, findings become inputs, etc.
//!
//! [`apply_context_transforms`] is invoked right after a child is spawned and
//! linked (sub-agent spawn and fan-out worker start). It looks up a transform
//! matching `(parent_blueprint → child_blueprint)` in either blueprint's
//! `transforms`, and for each [`RegionMapping`] copies the parent's `from_region`
//! into the child's `to_region`, applying the optional [`ContentTransform`].

use bevy_ecs::prelude::*;
use leviath_core::blueprint::{ContentTransform, RegionMapping};

use crate::components::ContextWindow;
use crate::pipeline::AgentBlueprint;

/// Seed `child`'s context from `parent`'s per a declared blueprint transform.
/// No-op unless both carry an [`AgentBlueprint`] with different names and a
/// matching [`ContextTransform`](leviath_core::blueprint::ContextTransform) exists.
pub fn apply_context_transforms(world: &mut World, parent: Entity, child: Entity) {
    let Some(mappings) = collect_transform_mappings(world, parent, child) else {
        return;
    };
    // Read the parent's mapped regions into owned, already-transformed content
    // (immutable borrow of the parent), then write them into the child.
    let mut writes: Vec<(String, String)> = Vec::new();
    if let Some(parent_window) = world.get::<ContextWindow>(parent) {
        for m in &mappings {
            if let Some(region) = parent_window.get_region(&m.from_region) {
                let joined = region
                    .content
                    .iter()
                    .map(|e| e.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !joined.is_empty() {
                    writes.push((
                        m.to_region.clone(),
                        apply_content_transform(&joined, &m.transform),
                    ));
                }
            }
        }
    }
    if let Some(mut child_window) = world.get_mut::<ContextWindow>(child) {
        for (to_region, content) in writes {
            let tokens = content.len() / 4 + 1;
            let _ = child_window.add_to_region(&to_region, content, tokens);
        }
    }
}

/// Find the region mappings for `parent_blueprint → child_blueprint`, searching
/// the parent's then the child's `transforms`. `None` when either lacks a
/// blueprint, they share a name (no cross-blueprint mapping), or no non-empty
/// transform matches.
fn collect_transform_mappings(
    world: &World,
    parent: Entity,
    child: Entity,
) -> Option<Vec<RegionMapping>> {
    let parent_name = world.get::<AgentBlueprint>(parent)?.0.name.clone();
    let child_name = world.get::<AgentBlueprint>(child)?.0.name.clone();
    if parent_name == child_name {
        return None;
    }
    for entity in [parent, child] {
        // Both blueprints are guaranteed present by the `?`s above.
        let bp = world
            .get::<AgentBlueprint>(entity)
            .expect("parent/child blueprint checked above");
        let found =
            bp.0.transforms
                .iter()
                .find(|t| t.from_blueprint == parent_name && t.to_blueprint == child_name)
                .map(|t| t.mappings.clone())
                .filter(|m| !m.is_empty());
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Apply a region mapping's optional content transform.
fn apply_content_transform(content: &str, transform: &Option<ContentTransform>) -> String {
    match transform {
        None | Some(ContentTransform::Direct) => content.to_string(),
        Some(ContentTransform::Extract { fields }) => extract_fields(content, fields),
        // Summarize needs an async LLM call that isn't available at spawn time;
        // fall back to a direct copy so the data still transfers. (Follow-up:
        // route through the compaction lane — tracked separately.)
        Some(ContentTransform::Summarize) => content.to_string(),
    }
}

/// Keep only the named fields of a JSON-object `content` (pretty-printed); return
/// `content` unchanged when it isn't a JSON object.
fn extract_fields(content: &str, fields: &[String]) -> String {
    match serde_json::from_str::<serde_json::Value>(content) {
        Ok(serde_json::Value::Object(map)) => {
            let filtered: serde_json::Map<String, serde_json::Value> = fields
                .iter()
                .filter_map(|f| map.get(f).map(|v| (f.clone(), v.clone())))
                .collect();
            serde_json::to_string_pretty(&serde_json::Value::Object(filtered))
                .expect("a JSON object always serializes")
        }
        _ => content.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::blueprint::{ContextTransform, RegionMapping};
    use leviath_core::{Region, RegionKind};

    fn bp_with_transforms(name: &str, transforms: Vec<ContextTransform>) -> AgentBlueprint {
        let layout = leviath_core::layout::ContextLayout::new(vec![], 10_000);
        let mut bp = leviath_core::Blueprint::new(
            name.to_string(),
            "d".to_string(),
            vec![leviath_core::Stage::new(
                "s".to_string(),
                leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
            )],
            layout,
        );
        bp.transforms = transforms;
        AgentBlueprint(bp)
    }

    fn window_with(regions: &[(&str, &str)]) -> ContextWindow {
        let mut w = ContextWindow::new(100_000);
        for (name, content) in regions {
            w.add_region(Region::new(name.to_string(), RegionKind::Clearable, 10_000));
            if !content.is_empty() {
                w.add_to_region(name, content.to_string(), 5).unwrap();
            }
        }
        w
    }

    fn mapping(from: &str, to: &str, transform: Option<ContentTransform>) -> RegionMapping {
        RegionMapping {
            from_region: from.to_string(),
            to_region: to.to_string(),
            transform,
        }
    }

    fn transform(from_bp: &str, to_bp: &str, mappings: Vec<RegionMapping>) -> ContextTransform {
        ContextTransform {
            from_blueprint: from_bp.to_string(),
            to_blueprint: to_bp.to_string(),
            mappings,
        }
    }

    // ── pure helpers ──

    #[test]
    fn apply_content_transform_variants() {
        assert_eq!(apply_content_transform("x", &None), "x");
        assert_eq!(
            apply_content_transform("x", &Some(ContentTransform::Direct)),
            "x"
        );
        assert_eq!(
            apply_content_transform("x", &Some(ContentTransform::Summarize)),
            "x"
        );
        let out = apply_content_transform(
            r#"{"a":1,"b":2}"#,
            &Some(ContentTransform::Extract {
                fields: vec!["a".to_string()],
            }),
        );
        assert!(out.contains("\"a\""));
        assert!(!out.contains("\"b\""));
    }

    #[test]
    fn extract_fields_handles_objects_missing_fields_and_non_objects() {
        // Object: keep only present named fields.
        let out = extract_fields(r#"{"a":1,"b":2}"#, &["a".to_string(), "z".to_string()]);
        assert!(out.contains("\"a\""));
        assert!(!out.contains("\"b\""));
        assert!(!out.contains("\"z\"")); // absent field skipped
        // Non-object JSON ⇒ unchanged.
        assert_eq!(extract_fields("[1,2]", &["a".to_string()]), "[1,2]");
        // Non-JSON ⇒ unchanged.
        assert_eq!(
            extract_fields("plain text", &["a".to_string()]),
            "plain text"
        );
    }

    // ── mapping resolution ──

    #[test]
    fn collect_mappings_finds_on_parent_then_child_and_rejects_mismatches() {
        let m = vec![mapping("plan", "task", None)];
        // Declared on the parent.
        let mut w = World::new();
        let p = w
            .spawn(bp_with_transforms(
                "planner",
                vec![transform("planner", "coder", m.clone())],
            ))
            .id();
        let c = w.spawn(bp_with_transforms("coder", vec![])).id();
        assert_eq!(collect_transform_mappings(&w, p, c).unwrap().len(), 1);

        // Declared on the child instead.
        let mut w2 = World::new();
        let p2 = w2.spawn(bp_with_transforms("planner", vec![])).id();
        let c2 = w2
            .spawn(bp_with_transforms(
                "coder",
                vec![transform("planner", "coder", m.clone())],
            ))
            .id();
        assert_eq!(collect_transform_mappings(&w2, p2, c2).unwrap().len(), 1);

        // Same blueprint name ⇒ no mapping.
        let mut w3 = World::new();
        let p3 = w3
            .spawn(bp_with_transforms(
                "same",
                vec![transform("same", "same", m.clone())],
            ))
            .id();
        let c3 = w3.spawn(bp_with_transforms("same", vec![])).id();
        assert!(collect_transform_mappings(&w3, p3, c3).is_none());

        // No matching transform (wrong target) ⇒ none.
        let mut w4 = World::new();
        let p4 = w4
            .spawn(bp_with_transforms(
                "planner",
                vec![transform("planner", "other", m.clone())],
            ))
            .id();
        let c4 = w4.spawn(bp_with_transforms("coder", vec![])).id();
        assert!(collect_transform_mappings(&w4, p4, c4).is_none());

        // Child missing a blueprint ⇒ none (the child `?`).
        let mut w5 = World::new();
        let p5 = w5.spawn(bp_with_transforms("planner", vec![])).id();
        let c5 = w5.spawn_empty().id();
        assert!(collect_transform_mappings(&w5, p5, c5).is_none());

        // Parent missing a blueprint ⇒ none (the parent `?`).
        let mut w6 = World::new();
        let p6 = w6.spawn_empty().id();
        let c6 = w6.spawn(bp_with_transforms("coder", vec![])).id();
        assert!(collect_transform_mappings(&w6, p6, c6).is_none());
    }

    // ── end-to-end application ──

    #[test]
    fn apply_context_transforms_copies_and_transforms_regions() {
        let mut w = World::new();
        let parent = w
            .spawn((
                bp_with_transforms(
                    "planner",
                    vec![transform(
                        "planner",
                        "coder",
                        vec![
                            mapping("plan", "task", Some(ContentTransform::Direct)),
                            mapping("empty", "unused", None), // empty ⇒ skipped
                            mapping("absent", "ghost", None), // region not in parent ⇒ skipped
                            mapping(
                                "data",
                                "inputs",
                                Some(ContentTransform::Extract {
                                    fields: vec!["keep".to_string()],
                                }),
                            ),
                        ],
                    )],
                ),
                window_with(&[
                    ("plan", "the plan"),
                    ("empty", ""),
                    ("data", r#"{"keep":1,"drop":2}"#),
                ]),
            ))
            .id();
        let child = w
            .spawn((
                bp_with_transforms("coder", vec![]),
                window_with(&[("task", ""), ("inputs", "")]),
            ))
            .id();

        apply_context_transforms(&mut w, parent, child);

        let cw = w.get::<ContextWindow>(child).unwrap();
        let task = cw.get_region("task").unwrap();
        assert!(task.current_tokens > 0);
        assert_eq!(task.content[0].content, "the plan");
        let inputs = cw.get_region("inputs").unwrap();
        assert!(inputs.content[0].content.contains("\"keep\""));
        assert!(!inputs.content[0].content.contains("\"drop\""));
    }

    #[test]
    fn apply_context_transforms_noop_without_a_matching_transform_or_windows() {
        // No transform ⇒ nothing copied.
        let mut w = World::new();
        let p = w
            .spawn((
                bp_with_transforms("a", vec![]),
                window_with(&[("plan", "x")]),
            ))
            .id();
        let c = w
            .spawn((
                bp_with_transforms("b", vec![]),
                window_with(&[("task", "")]),
            ))
            .id();
        apply_context_transforms(&mut w, p, c);
        assert_eq!(
            w.get::<ContextWindow>(c)
                .unwrap()
                .get_region("task")
                .unwrap()
                .current_tokens,
            0
        );

        // Matching transform but the parent has no window ⇒ no panic, nothing copied.
        let m = vec![mapping("plan", "task", None)];
        let mut w2 = World::new();
        let p2 = w2
            .spawn(bp_with_transforms("a", vec![transform("a", "b", m)]))
            .id(); // no window
        let c2 = w2
            .spawn((
                bp_with_transforms("b", vec![]),
                window_with(&[("task", "")]),
            ))
            .id();
        apply_context_transforms(&mut w2, p2, c2);
        assert_eq!(
            w2.get::<ContextWindow>(c2)
                .unwrap()
                .get_region("task")
                .unwrap()
                .current_tokens,
            0
        );

        // Matching transform + parent content, but the child has no window ⇒
        // no panic, nothing to write.
        let m2 = vec![mapping("plan", "task", None)];
        let mut w3 = World::new();
        let p3 = w3
            .spawn((
                bp_with_transforms("a", vec![transform("a", "b", m2)]),
                window_with(&[("plan", "content")]),
            ))
            .id();
        let c3 = w3.spawn(bp_with_transforms("b", vec![])).id(); // no window
        apply_context_transforms(&mut w3, p3, c3);
        assert!(w3.get::<ContextWindow>(c3).is_none());
    }
}

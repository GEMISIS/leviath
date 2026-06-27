//! Integration tests for context management and region lifecycle.

use leviath_core::{Region, RegionKind};
use leviath_runtime::ContextWindow;

#[test]
fn test_pinned_region_never_evicted() {
    let region = Region::new("pinned".to_string(), RegionKind::Pinned, 5000);

    // Pinned regions should never be evicted, regardless of memory pressure
    assert!(matches!(region.kind, RegionKind::Pinned));
    assert_eq!(region.max_tokens, 5000);
}

#[test]
fn test_sliding_window_configuration() {
    let region = Region::new(
        "conversation".to_string(),
        RegionKind::SlidingWindow { max_items: 10 },
        8000
    );

    match region.kind {
        RegionKind::SlidingWindow { max_items } => {
            assert_eq!(max_items, 10);
        }
        _ => panic!("Expected SlidingWindow region"),
    }
}

#[test]
fn test_temporary_region_properties() {
    let region = Region::new("temp".to_string(), RegionKind::Temporary, 10000);

    // Temporary regions should be first in line for eviction
    assert!(matches!(region.kind, RegionKind::Temporary));
}

#[test]
fn test_compacting_region_threshold() {
    let region = Region::new(
        "historical".to_string(),
        RegionKind::Compacting { threshold_tokens: 8000 },
        12000
    );

    match region.kind {
        RegionKind::Compacting { threshold_tokens } => {
            assert_eq!(threshold_tokens, 8000);
        }
        _ => panic!("Expected Compacting region"),
    }
}

#[test]
fn test_eviction_cascade_temporary_then_compacting() {
    let mut window = ContextWindow::new(10000);

    // Add a Clearable region
    let mut clearable = Region::new("scratch".to_string(), RegionKind::Clearable, 3000);
    clearable.add_entry("scratch data".to_string(), 1500).unwrap();
    window.add_region(clearable);

    // Add a Temporary region
    let mut temp = Region::new("temp".to_string(), RegionKind::Temporary, 4000);
    temp.add_entry("temp old".to_string(), 1000).unwrap();
    temp.add_entry("temp new".to_string(), 1000).unwrap();
    window.add_region(temp);

    // Add a SlidingWindow region (should never be touched)
    let mut sliding = Region::new(
        "conversation".to_string(),
        RegionKind::SlidingWindow { max_items: 5 },
        4000,
    );
    sliding.add_entry("msg 1".to_string(), 500).unwrap();
    sliding.add_entry("msg 2".to_string(), 500).unwrap();
    window.add_region(sliding);

    assert_eq!(window.current_tokens, 4500);

    // Evict with small target — should clear Clearable first
    let result = window.try_evict(1000).unwrap();
    assert!(result.tokens_freed >= 1500);

    // Clearable should be empty
    assert_eq!(window.get_region("scratch").unwrap().current_tokens, 0);

    // SlidingWindow should be untouched
    assert_eq!(window.get_region("conversation").unwrap().entry_count(), 2);
}

#[test]
fn test_schema_validation_json() {
    use leviath_core::region::{RegionSchema, ContentFormat};

    let schema = RegionSchema::new(ContentFormat::Json);

    // Valid JSON should pass
    assert!(schema.validate(r#"{"key": "value"}"#).is_ok());

    // Invalid JSON should fail
    assert!(schema.validate("not json").is_err());
}

#[test]
fn test_schema_validation_mermaid() {
    use leviath_core::region::{RegionSchema, ContentFormat};

    let schema = RegionSchema::new(ContentFormat::Mermaid);

    // Valid mermaid should pass
    assert!(schema.validate("graph TD\n  A --> B").is_ok());
    assert!(schema.validate("sequenceDiagram\n  Alice->>Bob: Hello").is_ok());

    // Invalid mermaid should fail
    assert!(schema.validate("just plain text").is_err());
}

#[test]
fn test_token_budget_enforcement() {
    let mut region = Region::new("test".to_string(), RegionKind::Pinned, 1000);

    // Adding within budget should succeed
    assert!(region.add_entry("small".to_string(), 100).is_ok());
    assert_eq!(region.current_tokens, 100);

    // Adding more within budget should succeed
    assert!(region.add_entry("medium".to_string(), 500).is_ok());
    assert_eq!(region.current_tokens, 600);

    // Exceeding budget should fail
    let result = region.add_entry("too large".to_string(), 500);
    assert!(result.is_err());
    assert_eq!(region.current_tokens, 600); // unchanged
}

#[test]
fn test_region_content_management() {
    let mut region = Region::new("test".to_string(), RegionKind::Temporary, 5000);

    // Add multiple entries
    region.add_entry("entry 1".to_string(), 100).unwrap();
    region.add_entry("entry 2".to_string(), 200).unwrap();
    region.add_entry("entry 3".to_string(), 300).unwrap();

    assert_eq!(region.entry_count(), 3);
    assert_eq!(region.current_tokens, 600);

    // Remove oldest
    let removed = region.remove_oldest().unwrap();
    assert_eq!(removed.content, "entry 1");
    assert_eq!(removed.tokens, 100);
    assert_eq!(region.entry_count(), 2);
    assert_eq!(region.current_tokens, 500);

    // Clear all
    region.clear();
    assert_eq!(region.entry_count(), 0);
    assert_eq!(region.current_tokens, 0);
}

#[test]
fn test_compacting_region_needs_compaction() {
    let mut region = Region::new(
        "findings".to_string(),
        RegionKind::Compacting { threshold_tokens: 500 },
        2000,
    );

    // Below threshold
    region.add_entry("data".to_string(), 300).unwrap();
    assert!(!region.needs_compaction());

    // Above threshold
    region.add_entry("more data".to_string(), 300).unwrap();
    assert!(region.needs_compaction());
}

#[test]
fn test_context_window_add_to_region() {
    let mut window = ContextWindow::new(10000);

    let region = Region::new("system".to_string(), RegionKind::Pinned, 2000);
    window.add_region(region);

    let region = Region::new("scratch".to_string(), RegionKind::Clearable, 3000);
    window.add_region(region);

    // Add content to existing region
    assert!(window.add_to_region("system", "Hello".to_string(), 10).is_ok());
    assert_eq!(window.current_tokens, 10);

    // Add content to non-existent region should fail
    assert!(window.add_to_region("nonexistent", "test".to_string(), 5).is_err());
}

#[test]
fn test_eviction_result_needs_compaction_when_compacting_full() {
    // Small window — compacting content nearly fills it
    let mut window = ContextWindow::new(1500);

    // Add a compacting region over its threshold
    let mut compacting = Region::new(
        "analysis".to_string(),
        RegionKind::Compacting { threshold_tokens: 1000 },
        1400,
    );
    compacting.add_entry("data block 1".to_string(), 600).unwrap();
    compacting.add_entry("data block 2".to_string(), 600).unwrap();
    window.add_region(compacting);

    assert_eq!(window.current_tokens, 1200);

    // Only 300 free, request 500 → can't free enough, should identify compacting region
    let result = window.try_evict(500).unwrap();
    assert_eq!(result.tokens_freed, 0);
    assert_eq!(result.needs_compaction, vec!["analysis"]);
}

#[test]
fn test_eviction_clears_then_identifies_compaction() {
    // Small window so after clearing, still not enough free space
    let mut window = ContextWindow::new(1800);

    // Add clearable region
    let mut clearable = Region::new("scratch".to_string(), RegionKind::Clearable, 1000);
    clearable.add_entry("scratch stuff".to_string(), 400).unwrap();
    window.add_region(clearable);

    // Add compacting region over threshold
    let mut compacting = Region::new(
        "impl".to_string(),
        RegionKind::Compacting { threshold_tokens: 800 },
        1200,
    );
    compacting.add_entry("impl data".to_string(), 900).unwrap();
    window.add_region(compacting);

    assert_eq!(window.current_tokens, 1300);

    // 500 free. Clear scratch → 900 free. Need 1000 → still short, should identify compacting.
    let result = window.try_evict(1000).unwrap();

    // Should have freed the clearable region
    assert_eq!(result.tokens_freed, 400);
    assert_eq!(window.get_region("scratch").unwrap().current_tokens, 0);

    // And identified the compacting region for compaction
    assert_eq!(result.needs_compaction, vec!["impl"]);
}

#[test]
fn test_needs_compaction_component_in_ecs() {
    use bevy_ecs::prelude::*;
    use leviath_runtime::NeedsCompaction;

    let mut world = World::new();
    let entity = world.spawn_empty().id();

    // Initially no NeedsCompaction
    assert!(world.get::<NeedsCompaction>(entity).is_none());

    // Add it
    world.entity_mut(entity).insert(NeedsCompaction {
        regions: vec!["analysis".to_string()],
    });

    let comp = world.get::<NeedsCompaction>(entity).unwrap();
    assert_eq!(comp.regions, vec!["analysis"]);

    // Remove it
    world.entity_mut(entity).remove::<NeedsCompaction>();
    assert!(world.get::<NeedsCompaction>(entity).is_none());
}

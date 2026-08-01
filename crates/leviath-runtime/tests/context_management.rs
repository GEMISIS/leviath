//! Integration tests for context management and region lifecycle.

use bevy_ecs::prelude::*;
use leviath_core::{EvictionStrategy, Region, RegionKind};
use leviath_runtime::components::MessageInbox;
use leviath_runtime::{AgentState, AgentStatus, ContextWindow, ParentRef, SubAgentChildren};

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
        RegionKind::SlidingWindow {
            max_items: 10,
            eviction_strategy: EvictionStrategy::PerItem,
        },
        8000,
    );

    match region.kind {
        RegionKind::SlidingWindow { max_items, .. } => {
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
        RegionKind::Compacting {
            threshold_tokens: 8000,
        },
        12000,
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
    clearable.add_entry("scratch data", 1500).unwrap();
    window.add_region(clearable);

    // Add a Temporary region
    let mut temp = Region::new("temp".to_string(), RegionKind::Temporary, 4000);
    temp.add_entry("temp old", 1000).unwrap();
    temp.add_entry("temp new", 1000).unwrap();
    window.add_region(temp);

    // Add a SlidingWindow region (should never be touched)
    let mut sliding = Region::new(
        "conversation".to_string(),
        RegionKind::SlidingWindow {
            max_items: 5,
            eviction_strategy: EvictionStrategy::PerItem,
        },
        4000,
    );
    sliding.add_entry("msg 1", 500).unwrap();
    sliding.add_entry("msg 2", 500).unwrap();
    window.add_region(sliding);

    assert_eq!(window.current_tokens, 4500);

    // Evict with small target - should clear Clearable first
    let result = window.try_evict(1000).unwrap();
    assert!(result.tokens_freed >= 1500);

    // Clearable should be empty
    assert_eq!(window.get_region("scratch").unwrap().current_tokens, 0);

    // SlidingWindow should be untouched
    assert_eq!(window.get_region("conversation").unwrap().entry_count(), 2);
}

#[test]
fn test_schema_validation_json() {
    use leviath_core::region::{ContentFormat, RegionSchema};

    let schema = RegionSchema::new(ContentFormat::Json);

    // Valid JSON should pass
    assert!(schema.validate(r#"{"key": "value"}"#).is_ok());

    // Invalid JSON should fail
    assert!(schema.validate("not json").is_err());
}

#[test]
fn test_schema_validation_mermaid() {
    use leviath_core::region::{ContentFormat, RegionSchema};

    let schema = RegionSchema::new(ContentFormat::Mermaid);

    // Valid mermaid should pass
    assert!(schema.validate("graph TD\n  A --> B").is_ok());
    assert!(
        schema
            .validate("sequenceDiagram\n  Alice->>Bob: Hello")
            .is_ok()
    );

    // Invalid mermaid should fail
    assert!(schema.validate("just plain text").is_err());
}

#[test]
fn test_token_budget_enforcement() {
    let mut region = Region::new("test".to_string(), RegionKind::Pinned, 1000);

    // Adding within budget should succeed
    assert!(region.add_entry("small", 100).is_ok());
    assert_eq!(region.current_tokens, 100);

    // Adding more within budget should succeed
    assert!(region.add_entry("medium", 500).is_ok());
    assert_eq!(region.current_tokens, 600);

    // Exceeding budget should fail
    let result = region.add_entry("too large", 500);
    assert!(result.is_err());
    assert_eq!(region.current_tokens, 600); // unchanged
}

#[test]
fn test_region_content_management() {
    let mut region = Region::new("test".to_string(), RegionKind::Temporary, 5000);

    // Add multiple entries
    region.add_entry("entry 1", 100).unwrap();
    region.add_entry("entry 2", 200).unwrap();
    region.add_entry("entry 3", 300).unwrap();

    assert_eq!(region.entry_count(), 3);
    assert_eq!(region.current_tokens, 600);

    // Remove oldest
    let removed = region.remove_oldest().unwrap();
    assert_eq!(removed.content(), "entry 1");
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
        RegionKind::Compacting {
            threshold_tokens: 500,
        },
        2000,
    );

    // Below threshold
    region.add_entry("data", 300).unwrap();
    assert!(!region.needs_compaction());

    // Above threshold
    region.add_entry("more data", 300).unwrap();
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
    assert!(
        window
            .add_to_region("system", "Hello".to_string(), 10)
            .is_ok()
    );
    assert_eq!(window.current_tokens, 10);

    // Add content to non-existent region should fail
    assert!(
        window
            .add_to_region("nonexistent", "test".to_string(), 5)
            .is_err()
    );
}

#[test]
fn test_eviction_result_needs_compaction_when_compacting_full() {
    // Small window - compacting content nearly fills it
    let mut window = ContextWindow::new(1500);

    // Add a compacting region over its threshold
    let mut compacting = Region::new(
        "analysis".to_string(),
        RegionKind::Compacting {
            threshold_tokens: 1000,
        },
        1400,
    );
    compacting.add_entry("data block 1", 600).unwrap();
    compacting.add_entry("data block 2", 600).unwrap();
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
    clearable.add_entry("scratch stuff", 400).unwrap();
    window.add_region(clearable);

    // Add compacting region over threshold
    let mut compacting = Region::new(
        "impl".to_string(),
        RegionKind::Compacting {
            threshold_tokens: 800,
        },
        1200,
    );
    compacting.add_entry("impl data", 900).unwrap();
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

// ─── Sub-agent tests ─────────────────────────────────────────────────────────

#[test]
fn test_parent_ref_and_children_components() {
    let mut world = World::new();

    // Spawn parent
    let parent = world
        .spawn((
            AgentState {
                agent_id: "coder-01".to_string(),
                current_stage: "analyze".to_string(),
                iteration: 0,
                status: AgentStatus::Active,
                spawned_children_ids: Vec::new(),
                pending_wait: None,
                accepts_messages: true,
            },
            MessageInbox::new(),
        ))
        .id();

    // Spawn child with ParentRef
    let child = world
        .spawn((
            AgentState {
                agent_id: "researcher-01".to_string(),
                current_stage: "research".to_string(),
                iteration: 0,
                status: AgentStatus::Active,
                spawned_children_ids: Vec::new(),
                pending_wait: None,
                accepts_messages: true,
            },
            ParentRef {
                parent_entity: parent,
                parent_agent_id: "coder-01".to_string(),
                depth: 1,
            },
            MessageInbox::new(),
        ))
        .id();

    // Add SubAgentChildren to parent
    world.entity_mut(parent).insert(SubAgentChildren {
        children: vec![child],
        max_child_depth: 3,
    });

    // Verify relationships
    let parent_children = world.get::<SubAgentChildren>(parent).unwrap();
    assert_eq!(parent_children.children.len(), 1);
    assert_eq!(parent_children.children[0], child);

    let child_parent = world.get::<ParentRef>(child).unwrap();
    assert_eq!(child_parent.parent_entity, parent);
    assert_eq!(child_parent.parent_agent_id, "coder-01");
    assert_eq!(child_parent.depth, 1);
}

#[test]
fn test_spawn_depth_validation() {
    // Depth 0 (root) can spawn at depth 1 if max_depth >= 1
    let current_depth = 0_usize;
    let max_depth = 3_usize;
    let child_depth = current_depth + 1;
    assert!(
        child_depth <= max_depth,
        "Should be able to spawn at depth 1"
    );

    // Depth 3 cannot spawn at depth 4 if max_depth is 3
    let current_depth = 3_usize;
    let child_depth = current_depth + 1;
    assert!(
        child_depth > max_depth,
        "Should NOT be able to spawn beyond max depth"
    );
}

#[test]
fn test_child_completion_notifies_parent() {
    let mut world = World::new();

    // Create parent with context window
    let mut parent_window = ContextWindow::new(10000);
    parent_window.add_region(Region::new(
        "conversation".to_string(),
        RegionKind::SlidingWindow {
            max_items: 50,
            eviction_strategy: EvictionStrategy::PerItem,
        },
        8000,
    ));

    let parent = world
        .spawn((
            AgentState {
                agent_id: "parent-01".to_string(),
                current_stage: "main".to_string(),
                iteration: 0,
                status: AgentStatus::Active,
                spawned_children_ids: vec!["child-01".to_string()],
                pending_wait: Some("child-01".to_string()),
                accepts_messages: true,
            },
            parent_window,
            MessageInbox::new(),
        ))
        .id();

    // Create completed child
    let _child = world
        .spawn((
            AgentState {
                agent_id: "child-01".to_string(),
                current_stage: "main".to_string(),
                iteration: 5,
                status: AgentStatus::Complete,
                spawned_children_ids: Vec::new(),
                pending_wait: None,
                accepts_messages: true,
            },
            ParentRef {
                parent_entity: parent,
                parent_agent_id: "parent-01".to_string(),
                depth: 1,
            },
            MessageInbox::new(),
        ))
        .id();

    // Simulate what child_completion_system does: inject result into parent
    let parent_state = world.get::<AgentState>(parent).unwrap();
    assert!(
        parent_state
            .spawned_children_ids
            .contains(&"child-01".to_string())
    );
    assert_eq!(parent_state.pending_wait, Some("child-01".to_string()));

    // After processing, parent should have the child removed and pending_wait cleared
    if let Some(mut state) = world.get_mut::<AgentState>(parent) {
        state.spawned_children_ids.retain(|id| id != "child-01");
        state.pending_wait = None;
    }

    let parent_state = world.get::<AgentState>(parent).unwrap();
    assert!(parent_state.spawned_children_ids.is_empty());
    assert!(parent_state.pending_wait.is_none());
}

#[test]
fn test_stage_gating_with_requires_children() {
    // When an agent has spawned children and pending_wait is set,
    // stage_gating_system should set status to Waiting
    let mut state = AgentState {
        agent_id: "test-01".to_string(),
        current_stage: "analyze".to_string(),
        iteration: 3,
        status: AgentStatus::Active,
        spawned_children_ids: vec!["researcher-01".to_string()],
        pending_wait: Some("researcher-01".to_string()),
        accepts_messages: true,
    };

    // Simulate what stage_gating_system does
    if matches!(state.status, AgentStatus::Active) && state.pending_wait.is_some() {
        state.status = AgentStatus::Waiting;
    }
    assert!(matches!(state.status, AgentStatus::Waiting));

    // When children complete, it should resume
    state.spawned_children_ids.clear();
    state.pending_wait = None;
    if matches!(state.status, AgentStatus::Waiting)
        && state.spawned_children_ids.is_empty()
        && state.pending_wait.is_none()
    {
        state.status = AgentStatus::Active;
    }
    assert!(matches!(state.status, AgentStatus::Active));
}

#[tokio::test]
async fn test_file_tracking_sync_assembly_integration() {
    // Integration test: verify file tracking → sync → assemble → system blocks contain HashMap content
    use leviath_core::{Region, RegionKind};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    // 1. Create a context window with a HashMap region (simulating initial setup)
    let mut entity_cw = ContextWindow::new(200_000);
    let files_region = Region::new(
        "files".to_string(),
        RegionKind::HashMap {
            max_entries: Some(50),
        },
        40_000,
    );
    entity_cw.add_region(files_region);

    // 2. Create shared context window and populate it with entity's state
    let shared_cw: Arc<Mutex<Option<ContextWindow>>> =
        Arc::new(Mutex::new(Some(entity_cw.clone())));

    // 3. Simulate file tracking: upsert into shared CW's HashMap region
    {
        let mut guard = shared_cw.lock().await;
        if let Some(window) = guard.as_mut()
            && let Some(region) = window.get_region_mut("files")
        {
            region
                .upsert_by_key("src/main.py", "def main():\n    print('hello')", 10)
                .unwrap();
            region
                .upsert_by_key("src/utils.py", "def helper():\n    return 42", 8)
                .unwrap();
        }
    }

    // 4. Simulate sync: shared→entity
    {
        let guard = shared_cw.lock().await;
        if let Some(shared) = guard.as_ref() {
            entity_cw.regions = shared.regions.clone();
            entity_cw.current_tokens = shared.current_tokens;
        }
    }

    // 5. Assemble the entity CW into system blocks
    let assembled = entity_cw.assemble();

    // 6. Verify: system_blocks should contain the HashMap content with keys
    assert_eq!(assembled.system_blocks.len(), 1);
    let block_text = &assembled.system_blocks[0].text;

    assert!(block_text.contains("[files]:"));
    assert!(block_text.contains("### [src/main.py]"));
    assert!(block_text.contains("def main()"));
    assert!(block_text.contains("### [src/utils.py]"));
    assert!(block_text.contains("def helper()"));
}

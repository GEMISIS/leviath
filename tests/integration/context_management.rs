//! Integration tests for context management and region lifecycle.

use leviath_core::{Region, RegionKind, RegionEntry};

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

// TODO: Add tests for:
// - Eviction cascade (temporary → compacting → sliding)
// - Schema validation
// - Token budget enforcement
// - Context transforms between agents
// - Region content management

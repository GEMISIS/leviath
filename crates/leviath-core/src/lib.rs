//! # Leviath Core
//!
//! Core types and traits for the Leviath agent framework.
//!
//! This crate provides the foundational types for building agents with structured,
//! hardware-inspired context window management. It includes:
//!
//! - **Regions**: Typed memory regions with different lifecycle policies
//! - **Layouts**: Complete memory maps defining how context is structured
//! - **Blueprints**: Agent definitions including stages, models, and tools
//! - **Lifecycle**: Policies for eviction, compaction, and context management

pub mod blueprint;
pub mod cache;
pub mod error;
pub mod layout;
pub mod lifecycle;
pub mod region;
pub mod taint;

pub use blueprint::{
    Blueprint, ContextTransform, EdgeTransform, Stage, StageResult, ToolFilter, ToolResultRouting,
    TransitionCondition, TransitionEdge,
};
pub use cache::{CacheBreakpoint, CacheHint};
pub use error::{Error, Result, ValidationError};
pub use layout::{ContextLayout, RegionDefinition};
pub use lifecycle::{CompactionConfig, CompactionStrategy, EvictionPolicy};
pub use region::{ContentFormat, Region, RegionEntry, RegionKind, RegionSchema};
pub use taint::{
    FilterInput, FilterMode, FilterOperation, GateDecision, GateDecisionSource, GateEvent,
    InputMode, PointerRef, RegionTaint, SecurityConfig, TaintLevel, ToolClassification,
    ToolDirection,
};

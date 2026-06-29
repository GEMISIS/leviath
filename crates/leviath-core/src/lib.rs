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
pub mod error;
pub mod layout;
pub mod lifecycle;
pub mod region;

pub use blueprint::{Blueprint, ContextTransform, Stage, ToolFilter, ToolResultRouting};
pub use error::{Error, Result};
pub use layout::{ContextLayout, RegionDefinition};
pub use lifecycle::{CompactionConfig, CompactionStrategy, EvictionPolicy};
pub use region::{ContentFormat, Region, RegionEntry, RegionKind, RegionSchema};

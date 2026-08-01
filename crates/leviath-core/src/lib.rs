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
pub mod config;
pub mod credentials;
pub mod error;
pub mod interaction;
pub mod intern;
pub mod layout;
pub mod lifecycle;
pub mod manifest;
pub mod net;
pub mod panic_payload;
pub mod paths;
pub mod policy;
pub mod read_paths;
pub mod region;
pub mod run_archive;
pub mod run_meta;
pub mod sandbox;
pub mod secrets;
pub mod taint;
pub mod telemetry;
pub mod text;

pub use blueprint::{
    Blueprint, ContextTransform, EdgeTransform, FileTrackingConfig, ReadPathsConfig,
    RepetitionDetectionConfig, Stage, StuckConfig, ToolResultRouting, TransitionCondition,
    TransitionEdge,
};
pub use cache::CacheHint;
pub use credentials::{
    CredentialStore, CredentialStoreKind, MemoryStore, mcp_account, provider_account,
};
pub use error::{Error, Result, ValidationError};
pub use intern::ContentInterner;
pub use layout::{BudgetSpec, ContextLayout, RegionDefinition};
pub use lifecycle::CompactionConfig;
pub use net::{
    ClientTimeouts, UrlRejection, check_url, checked_client, client, client_builder,
    is_restricted_addr,
};
pub use panic_payload::panic_message;
pub use paths::{
    agents_dir, canonicalize_for_match, data_dir, home_dir, is_safe_path_component, providers_dir,
    resolves_within, tools_dir,
};
pub use policy::{AllowlistRule, McpToolOverride, PolicyConfig};
pub use read_paths::{
    ReadPathDecision, ReadPathEntry, ReadPathPolicy, ReadPathSet, validate_entry_syntax,
};
pub use region::{
    ContentFormat, EntryKind, EvictionStrategy, Region, RegionEntry, RegionKind, RegionSchema,
    SerializedToolCall,
};
pub use sandbox::{OnUnavailable, SandboxKind, ToolSandboxConfig, resolve_sandbox};
pub use secrets::{
    child_env_allowed, constant_time_eq, is_secret_header, is_sensitive_env_name, redact,
    script_env_allowed,
};
pub use taint::{
    GateDecision, GateDecisionSource, GateEvent, RegionTaint, SecurityConfig, TaintLevel,
    ToolClassification, ToolDirection,
};
pub use text::{estimate_tokens, floor_char_boundary, truncate_at_boundary};

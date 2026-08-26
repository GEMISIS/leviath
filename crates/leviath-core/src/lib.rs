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
pub mod duration;
pub mod error;
pub mod interaction;
pub mod layout;
pub mod lifecycle;
pub mod manifest;
pub mod output;
pub mod panic_payload;
pub mod paths;
pub mod policy;
pub mod read_paths;
pub mod region;
pub mod run_archive;
pub mod run_meta;
pub mod sandbox;
pub mod secrets;
pub mod sync;
pub mod taint;
pub mod telemetry;
pub mod text;
pub mod write_limits;

pub use blueprint::{
    Blueprint, ContextTransform, EdgeTransform, FileTrackingConfig, NudgeConfig, ReadPathsConfig,
    RepetitionDetectionConfig, ResolvedNudge, Stage, StuckConfig, ToolResultRouting,
    TransitionCondition, TransitionEdge, resolve_nudge,
};
pub use cache::CacheHint;
pub use credentials::{
    CredentialStore, CredentialStoreKind, MemoryStore, mcp_account, provider_account,
};
pub use error::{Error, Result, ValidationError};
pub use layout::{BudgetSpec, ContextLayout, RegionDefinition};
pub use lifecycle::CompactionConfig;
pub use output::{
    FINAL_OUTPUT_FILE, FinalOutput, FinalOutputDescriptor, MAX_FINAL_OUTPUT_BYTES, OutputSpec,
    describe_spec, resolve_output_spec,
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
    SerializedToolCall, Volatility,
};
pub use sandbox::{OnUnavailable, SandboxKind, ToolSandboxConfig, resolve_sandbox};
pub use secrets::{
    ShellEnvMode, child_env_allowed, constant_time_eq, dotenv_var_allowed, is_secret_header,
    is_sensitive_env_name, redact, script_env_allowed, withheld_child_vars,
};
pub use taint::{
    GateDecision, GateDecisionSource, GateEvent, RegionTaint, SecurityConfig, TaintLevel,
    ToolClassification, ToolDirection,
};
pub use text::{estimate_tokens, floor_char_boundary, truncate_at_boundary};

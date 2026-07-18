//! Re-export of the tool-source seam, which lives in `leviath-runtime`.
//!
//! The concrete `RegistryToolCaller` + `impl StageToolSource for ToolRegistry`
//! stay in the CLI (`crate::tools`); only the traits live in the runtime so
//! the stage engine can name them without a `runtime -> cli` cycle.

pub use leviath_runtime::tool_source::{StageToolSource, ToolCaller};

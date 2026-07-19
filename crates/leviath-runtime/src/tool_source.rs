//! Decoupling seam between the stage loop and the concrete tool registry.
//!
//! The stage loop / stages / fan-out only
//! need two things from the CLI's `ToolRegistry`:
//! the set of tool definitions to advertise, and a way to execute a single tool
//! call by name. Capturing exactly that in [`StageToolSource`] keeps the stage
//! loop free of a `runtime -> cli` dependency on `ToolRegistry`. The concrete
//! `impl`s stay in `tools.rs`.

use std::sync::Arc;

use async_trait::async_trait;
use leviath_providers::Tool;

/// Executes a single tool call by name.
///
/// Kept separate from [`StageToolSource`] because fan-out workers run in
/// detached (`'static`) tasks and therefore need an *owned*, cheap-to-clone,
/// `Send + Sync` executor rather than a borrow of the registry.
#[async_trait]
pub trait ToolCaller: Send + Sync {
    /// Execute `name` with `arguments`, returning the tool's textual result
    /// (errors are rendered into the string, matching `ToolRegistry::call`).
    async fn call(&self, name: &str, arguments: serde_json::Value) -> String;
}

/// What the stage loop / stages / fan-out need from the tool registry.
pub trait StageToolSource: Send + Sync {
    /// All tool definitions to advertise to the model (built-ins + MCP +
    /// sub-agent tools). The stage loop and fan-out do their own per-stage
    /// `available_tools` filtering on top of this.
    fn all_tool_defs(&self) -> Vec<Tool>;

    /// An owned, `'static` handle for executing tool calls, used by fan-out
    /// workers spawned as detached tasks.
    fn tool_caller(&self) -> Arc<dyn ToolCaller>;
}

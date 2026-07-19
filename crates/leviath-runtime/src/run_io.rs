//! Abstraction over I/O for the stage executor.
//!
//! The trait lives in `leviath-runtime` so the stage engine can talk
//! to it without depending on the CLI. Concrete implementations
//! (`ConsoleIO`, the foreground/worker adapters, and the test `MockIO`) live in
//! `leviath-cli`.

use async_trait::async_trait;
use leviath_core::blueprint::StageResult;
use leviath_core::run_meta::RegionSnapshot;
use leviath_core::Stage;

/// Abstraction over I/O for the stage executor.
#[async_trait]
pub trait RunIO: Send {
    /// Called when entering a new stage
    async fn on_stage_enter(
        &mut self,
        stage: &Stage,
        visit_num: usize,
        provider: &str,
        model: &str,
    );

    /// Called when a stage completes
    async fn on_stage_complete(
        &mut self,
        stage_name: &str,
        result: &StageResult,
        next_stage: Option<&str>,
    );

    /// Display inference output text
    async fn on_output(&mut self, text: &str);

    /// Display token usage
    async fn on_tokens(&mut self, prompt: usize, completion: usize, cached: usize);

    /// Report a tool call and its result
    async fn on_tool_call(&mut self, tool_name: &str, tool_id: &str, result: &str);

    /// Get user input for interactive stages (returns None if not interactive)
    async fn get_user_input(&mut self, prompt: &str) -> Option<String>;

    /// Report an error
    async fn on_error(&mut self, error: &str);

    /// Report provider not configured
    async fn on_provider_missing(&mut self, provider: &str);

    /// Whether this is a background/worker context (affects snapshot writing etc.)
    fn is_background(&self) -> bool;

    /// Write a context snapshot (worker mode writes to disk, foreground is no-op)
    fn write_context_snapshot(&mut self, snapshot: &RegionSnapshot);
}

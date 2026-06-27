//! ECS components for agent state and execution.

use bevy_ecs::prelude::*;
use leviath_core::Region;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Agent execution state component.
///
/// Tracks the current state of an agent's execution, including which stage
/// it's in and iteration counts.
#[derive(Component, Debug, Clone)]
pub struct AgentState {
    /// Unique identifier for this agent instance
    pub agent_id: String,

    /// Current execution stage
    pub current_stage: String,

    /// Number of iterations in current stage
    pub iteration: usize,

    /// Agent status
    pub status: AgentStatus,
}

/// Status of an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentStatus {
    /// Agent is idle, ready for tasks
    Idle,

    /// Agent is actively working on a task
    Active,

    /// Agent is waiting for input or external event
    Waiting,

    /// Agent has completed its task
    Complete,

    /// Agent encountered an error
    Error { message: String },

    /// Agent was cancelled by the user or system
    Cancelled,
}

/// Context window component storing the agent's memory regions.
#[derive(Component, Debug, Clone)]
pub struct ContextWindow {
    /// All regions in this context window
    pub regions: Vec<Region>,

    /// Current total token usage
    pub current_tokens: usize,

    /// Maximum token budget
    pub max_tokens: usize,
}

impl ContextWindow {
    /// Create a new context window with the specified budget.
    pub fn new(max_tokens: usize) -> Self {
        Self {
            regions: Vec::new(),
            current_tokens: 0,
            max_tokens,
        }
    }

    /// Get a region by name.
    pub fn get_region(&self, name: &str) -> Option<&Region> {
        self.regions.iter().find(|r| r.name == name)
    }

    /// Get a mutable reference to a region by name.
    pub fn get_region_mut(&mut self, name: &str) -> Option<&mut Region> {
        self.regions.iter_mut().find(|r| r.name == name)
    }

    /// Add a region to this context window.
    pub fn add_region(&mut self, region: Region) {
        self.regions.push(region);
        self.current_tokens = self.calculate_tokens();
    }

    /// Add content to a specific region.
    pub fn add_to_region(&mut self, region_name: &str, content: String, tokens: usize) -> leviath_core::Result<()> {
        if let Some(region) = self.get_region_mut(region_name) {
            region.add_entry(content, tokens)?;
            self.current_tokens = self.calculate_tokens();
            Ok(())
        } else {
            Err(leviath_core::Error::RegionNotFound(region_name.to_string()))
        }
    }

    /// Calculate current token usage across all regions.
    pub fn calculate_tokens(&self) -> usize {
        self.regions.iter().map(|r| r.current_tokens).sum()
    }

    /// Check if the context window needs eviction.
    pub fn needs_eviction(&self, threshold: f32) -> bool {
        let usage_ratio = self.current_tokens as f32 / self.max_tokens as f32;
        usage_ratio >= threshold
    }

    /// Execute eviction cascade to free up space.
    /// 
    /// Returns the number of tokens freed, or an error if eviction failed.
    pub fn try_evict(&mut self, target_free_tokens: usize) -> leviath_core::Result<usize> {
        use leviath_core::RegionKind;

        let initial_tokens = self.current_tokens;
        
        // Check if we have any evictable regions
        let has_evictable = self.regions.iter().any(|r| {
            matches!(r.kind, RegionKind::Clearable | RegionKind::Temporary)
        });
        
        if !has_evictable {
            tracing::warn!(
                "Context window has no Clearable or Temporary regions. \
                 This may be intentional, but usually indicates a configuration error."
            );
        }

        // Phase 1: Clear Clearable regions (all-or-nothing)
        for region in &mut self.regions {
            if matches!(region.kind, RegionKind::Clearable) && !region.content.is_empty() {
                let freed = region.current_tokens;
                region.clear();
                self.current_tokens -= freed;
                tracing::debug!(
                    region = %region.name,
                    tokens_freed = freed,
                    "Cleared Clearable region (all-or-nothing)"
                );

                if self.max_tokens - self.current_tokens >= target_free_tokens {
                    return Ok(initial_tokens - self.current_tokens);
                }
            }
        }

        // Phase 2: Evict from Temporary regions (oldest first, one at a time)
        loop {
            let mut evicted_any = false;
            
            for region in &mut self.regions {
                if matches!(region.kind, RegionKind::Temporary) {
                    if let Some(entry) = region.remove_oldest() {
                        let freed = entry.tokens;
                        self.current_tokens -= freed;
                        evicted_any = true;
                        
                        tracing::debug!(
                            region = %region.name,
                            tokens_freed = freed,
                            "Evicted temporary region entry (oldest first)"
                        );

                        if self.max_tokens - self.current_tokens >= target_free_tokens {
                            return Ok(initial_tokens - self.current_tokens);
                        }
                    }
                }
            }
            
            if !evicted_any {
                break;
            }
        }

        // Phase 3: Compact Compacting regions
        // Note: This requires LLM access, which should be done by the caller
        // For now, we just check if compaction is needed and return an error
        for region in &self.regions {
            if region.needs_compaction() {
                return Err(leviath_core::Error::Other(
                    format!("Region '{}' needs compaction but cannot be compacted without LLM provider", region.name)
                ));
            }
        }

        // Phase 4: SlidingWindow regions are NEVER reduced
        // Phase 5: Pinned and CompactHistory regions are NEVER touched
        
        // If we get here, we couldn't free enough space
        let pinned_tokens: usize = self.regions.iter()
            .filter(|r| matches!(r.kind, RegionKind::Pinned | RegionKind::CompactHistory { .. }))
            .map(|r| r.current_tokens)
            .sum();
        
        if pinned_tokens > self.max_tokens {
            Err(leviath_core::Error::PinnedRegionsOverBudget {
                pinned_tokens,
                total_budget: self.max_tokens,
            })
        } else {
            // We freed some tokens but not enough
            Ok(initial_tokens - self.current_tokens)
        }
    }

    /// Assemble the complete prompt from all regions in order.
    pub fn assemble_prompt(&self) -> String {
        self.regions
            .iter()
            .map(|region| {
                let header = format!("## Region: {}\n", region.name);
                let content = region.content
                    .iter()
                    .map(|entry| entry.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                format!("{}{}", header, content)
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n")
    }
}

/// Task assignment component.
///
/// Represents a task that has been assigned to an agent for execution.
#[derive(Component, Debug, Clone)]
pub struct TaskAssignment {
    /// Unique task identifier
    pub task_id: String,

    /// Task description or prompt
    pub prompt: String,

    /// Task priority (higher = more important)
    pub priority: i32,

    /// Timestamp when task was assigned
    pub assigned_at: i64,
}

/// Inference result component.
///
/// Stores the result of an LLM inference call, including the response
/// and any tool calls that need to be executed.
#[derive(Component, Debug, Clone)]
pub struct InferenceResult {
    /// The model's response text
    pub response: String,

    /// Tool calls requested by the model
    pub tool_calls: Vec<ToolCall>,

    /// Tokens used in this inference
    pub tokens_used: usize,

    /// Timestamp of this inference
    pub timestamp: i64,
}

/// A tool call requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Tool identifier
    pub tool_id: String,

    /// Tool name
    pub name: String,

    /// Tool arguments
    pub arguments: serde_json::Value,
}

/// Cancellation token for interrupting running agents.
///
/// Thread-safe flag that can be checked during inference loops
/// to allow early termination of agent execution.
#[derive(Component, Debug, Clone)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    /// Create a new cancellation token (not cancelled).
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signal cancellation.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Check if cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

/// A message that can be sent to a running agent.
#[derive(Debug, Clone)]
pub struct AgentMessage {
    /// Target agent ID
    pub agent_id: String,
    /// Message content
    pub content: String,
    /// Which region to add the message to (default: "conversation")
    pub target_region: Option<String>,
    /// Priority (higher = processed sooner)
    pub priority: i32,
}

/// Inbox component for receiving messages sent to a running agent.
#[derive(Component, Debug, Clone)]
pub struct MessageInbox {
    /// Pending messages waiting to be processed
    pub messages: Vec<AgentMessage>,
}

impl MessageInbox {
    /// Create a new empty inbox.
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
        }
    }

    /// Add a message to the inbox.
    pub fn push(&mut self, msg: AgentMessage) {
        self.messages.push(msg);
        // Sort by priority descending so highest priority is first
        self.messages.sort_by(|a, b| b.priority.cmp(&a.priority));
    }

    /// Drain all messages from the inbox.
    pub fn drain_all(&mut self) -> Vec<AgentMessage> {
        std::mem::take(&mut self.messages)
    }
}

impl Default for MessageInbox {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::{Region, RegionKind};

    #[test]
    fn test_context_window_creation() {
        let window = ContextWindow::new(10000);
        assert_eq!(window.max_tokens, 10000);
        assert_eq!(window.current_tokens, 0);
    }

    #[test]
    fn test_needs_eviction() {
        let mut window = ContextWindow::new(10000);
        window.current_tokens = 9500;
        assert!(window.needs_eviction(0.9));
        
        window.current_tokens = 5000;
        assert!(!window.needs_eviction(0.9));
    }

    #[test]
    fn test_add_region() {
        let mut window = ContextWindow::new(10000);
        let region = Region::new("test".to_string(), RegionKind::Pinned, 1000);
        window.add_region(region);
        assert_eq!(window.regions.len(), 1);
    }

    #[test]
    fn test_clearable_eviction() {
        let mut window = ContextWindow::new(10000);
        let mut region = Region::new("scratch".to_string(), RegionKind::Clearable, 5000);
        region.add_entry("test content 1".to_string(), 1000).unwrap();
        region.add_entry("test content 2".to_string(), 1000).unwrap();
        window.add_region(region);
        
        assert_eq!(window.current_tokens, 2000);
        
        // Evict should clear the entire Clearable region
        let freed = window.try_evict(1000).unwrap();
        assert_eq!(freed, 2000);
        assert_eq!(window.current_tokens, 0);
    }

    #[test]
    fn test_temporary_eviction_oldest_first() {
        let mut window = ContextWindow::new(10000);
        let mut region = Region::new("temp".to_string(), RegionKind::Temporary, 5000);
        region.add_entry("old content".to_string(), 1000).unwrap();
        region.add_entry("middle content".to_string(), 1000).unwrap();
        region.add_entry("new content".to_string(), 1000).unwrap();
        window.add_region(region);
        
        assert_eq!(window.current_tokens, 3000);
        
        // Evict should remove oldest first
        let freed = window.try_evict(500).unwrap();
        assert!(freed >= 1000); // Should free at least one entry
        
        // Check that oldest was removed
        let region = window.get_region("temp").unwrap();
        assert_eq!(region.content.len(), 2);
        assert_eq!(region.content[0].content, "middle content");
    }

    #[test]
    fn test_sliding_window_never_reduced() {
        let mut window = ContextWindow::new(10000);
        let mut region = Region::new("conversation".to_string(), RegionKind::SlidingWindow { max_items: 5 }, 5000);
        region.add_entry("msg 1".to_string(), 1000).unwrap();
        region.add_entry("msg 2".to_string(), 1000).unwrap();
        region.add_entry("msg 3".to_string(), 1000).unwrap();
        window.add_region(region);
        
        let initial_count = window.get_region("conversation").unwrap().content.len();
        
        // Try to evict - should not touch SlidingWindow
        window.try_evict(1000).ok();
        
        let after_count = window.get_region("conversation").unwrap().content.len();
        assert_eq!(initial_count, after_count, "SlidingWindow should never be reduced during eviction");
    }

    #[test]
    fn test_pinned_never_touched() {
        let mut window = ContextWindow::new(10000);
        let mut region = Region::new("architecture".to_string(), RegionKind::Pinned, 3000);
        region.add_entry("architecture diagram".to_string(), 2000).unwrap();
        window.add_region(region);
        
        let initial_tokens = window.get_region("architecture").unwrap().current_tokens;
        
        // Try to evict - should not touch Pinned
        window.try_evict(1000).ok();
        
        let after_tokens = window.get_region("architecture").unwrap().current_tokens;
        assert_eq!(initial_tokens, after_tokens, "Pinned region should never be evicted");
    }

    #[test]
    fn test_eviction_cascade_order() {
        let mut window = ContextWindow::new(10000);
        
        // Add Clearable region
        let mut clearable = Region::new("scratch".to_string(), RegionKind::Clearable, 2000);
        clearable.add_entry("scratch data".to_string(), 1000).unwrap();
        window.add_region(clearable);
        
        // Add Temporary region
        let mut temporary = Region::new("temp".to_string(), RegionKind::Temporary, 3000);
        temporary.add_entry("temp data 1".to_string(), 1000).unwrap();
        temporary.add_entry("temp data 2".to_string(), 1000).unwrap();
        window.add_region(temporary);
        
        assert_eq!(window.current_tokens, 3000);
        
        // Evict with small target - should clear Clearable first
        window.try_evict(500).unwrap();
        
        // Clearable should be empty
        assert_eq!(window.get_region("scratch").unwrap().current_tokens, 0);
        
        // Temporary should still have content
        assert!(window.get_region("temp").unwrap().current_tokens > 0);
    }

    #[test]
    fn test_assemble_prompt() {
        let mut window = ContextWindow::new(10000);
        
        let mut region1 = Region::new("system".to_string(), RegionKind::Pinned, 1000);
        region1.add_entry("You are a helpful assistant.".to_string(), 100).unwrap();
        window.add_region(region1);
        
        let mut region2 = Region::new("conversation".to_string(), RegionKind::Temporary, 2000);
        region2.add_entry("User: Hello".to_string(), 50).unwrap();
        region2.add_entry("Assistant: Hi there!".to_string(), 50).unwrap();
        window.add_region(region2);
        
        let prompt = window.assemble_prompt();
        assert!(prompt.contains("## Region: system"));
        assert!(prompt.contains("You are a helpful assistant."));
        assert!(prompt.contains("## Region: conversation"));
        assert!(prompt.contains("User: Hello"));
        assert!(prompt.contains("Assistant: Hi there!"));
    }

    #[test]
    fn test_cancellation_token() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());

        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_cancellation_token_clone() {
        let token = CancellationToken::new();
        let clone = token.clone();

        token.cancel();
        assert!(clone.is_cancelled(), "Clone should share atomic state");
    }

    #[test]
    fn test_message_inbox() {
        let mut inbox = MessageInbox::new();
        assert!(inbox.messages.is_empty());

        inbox.push(AgentMessage {
            agent_id: "agent-1".to_string(),
            content: "hello".to_string(),
            target_region: None,
            priority: 0,
        });
        assert_eq!(inbox.messages.len(), 1);

        let drained = inbox.drain_all();
        assert_eq!(drained.len(), 1);
        assert!(inbox.messages.is_empty());
    }

    #[test]
    fn test_message_inbox_priority_ordering() {
        let mut inbox = MessageInbox::new();

        inbox.push(AgentMessage {
            agent_id: "a".to_string(),
            content: "low".to_string(),
            target_region: None,
            priority: 1,
        });
        inbox.push(AgentMessage {
            agent_id: "a".to_string(),
            content: "high".to_string(),
            target_region: None,
            priority: 10,
        });
        inbox.push(AgentMessage {
            agent_id: "a".to_string(),
            content: "medium".to_string(),
            target_region: None,
            priority: 5,
        });

        let msgs = inbox.drain_all();
        assert_eq!(msgs[0].content, "high");
        assert_eq!(msgs[1].content, "medium");
        assert_eq!(msgs[2].content, "low");
    }

    #[test]
    fn test_agent_status_cancelled() {
        let status = AgentStatus::Cancelled;
        match status {
            AgentStatus::Cancelled => {} // OK
            _ => panic!("Expected Cancelled"),
        }
    }
}

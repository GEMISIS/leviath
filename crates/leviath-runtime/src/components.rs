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

    /// IDs of child agents spawned by this agent
    pub spawned_children_ids: Vec<String>,

    /// If set, this agent is blocked waiting for the named child to complete
    pub pending_wait: Option<String>,

    /// Whether the current stage accepts mid-run user messages.
    /// When false, messages stay in the inbox until a stage that accepts them.
    pub accepts_messages: bool,
}

/// Reference to a parent agent, making this agent a sub-agent.
#[derive(Component, Debug, Clone)]
pub struct ParentRef {
    /// Entity of the parent agent
    pub parent_entity: Entity,

    /// Agent ID of the parent
    pub parent_agent_id: String,

    /// Depth in the agent tree (root = 0)
    pub depth: usize,
}

/// Tracks child agents spawned by this agent.
#[derive(Component, Debug, Clone)]
pub struct SubAgentChildren {
    /// Child agent entities
    pub children: Vec<Entity>,

    /// Maximum allowed sub-agent tree depth
    pub max_child_depth: usize,
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

/// Result of an eviction attempt, including tokens freed and regions needing LLM compaction.
#[derive(Debug, Clone)]
pub struct EvictionResult {
    /// Number of tokens freed by eviction phases 1-2 (Clearable + Temporary).
    pub tokens_freed: usize,
    /// Region names that need LLM-based compaction (phase 3).
    pub needs_compaction: Vec<String>,
}

/// Marker component added by the eviction system when regions need async LLM compaction.
///
/// The inference loop or engine tick checks for this component and performs
/// compaction asynchronously, since ECS systems are synchronous.
#[derive(Component, Debug, Clone)]
pub struct NeedsCompaction {
    /// Region names that need compaction.
    pub regions: Vec<String>,
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
    pub fn add_to_region(
        &mut self,
        region_name: &str,
        content: String,
        tokens: usize,
    ) -> leviath_core::Result<()> {
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
    /// Returns an `EvictionResult` with tokens freed and any regions that need
    /// LLM-based compaction. The caller is responsible for performing compaction
    /// on the listed regions (since it requires async LLM access).
    pub fn try_evict(&mut self, target_free_tokens: usize) -> leviath_core::Result<EvictionResult> {
        use leviath_core::RegionKind;

        let initial_tokens = self.current_tokens;

        // Check if we have any evictable regions
        let has_evictable = self
            .regions
            .iter()
            .any(|r| matches!(r.kind, RegionKind::Clearable | RegionKind::Temporary));

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
                    return Ok(EvictionResult {
                        tokens_freed: initial_tokens - self.current_tokens,
                        needs_compaction: Vec::new(),
                    });
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
                            return Ok(EvictionResult {
                                tokens_freed: initial_tokens - self.current_tokens,
                                needs_compaction: Vec::new(),
                            });
                        }
                    }
                }
            }

            if !evicted_any {
                break;
            }
        }

        // Phase 3: If still need space, identify Compacting regions that need compaction
        let mut needs_compaction = Vec::new();
        if self.max_tokens - self.current_tokens < target_free_tokens {
            for region in &self.regions {
                if region.needs_compaction() {
                    needs_compaction.push(region.name.clone());
                }
            }
        }

        // Phase 4: SlidingWindow regions are NEVER reduced
        // Phase 5: Pinned and CompactHistory regions are NEVER touched

        // Check for pinned regions over budget
        let pinned_tokens: usize = self
            .regions
            .iter()
            .filter(|r| {
                matches!(
                    r.kind,
                    RegionKind::Pinned | RegionKind::CompactHistory { .. }
                )
            })
            .map(|r| r.current_tokens)
            .sum();

        if pinned_tokens > self.max_tokens {
            return Err(leviath_core::Error::PinnedRegionsOverBudget {
                pinned_tokens,
                total_budget: self.max_tokens,
            });
        }

        Ok(EvictionResult {
            tokens_freed: initial_tokens - self.current_tokens,
            needs_compaction,
        })
    }

    /// Assemble structured messages from all regions for proper LLM API usage.
    ///
    /// Maps region types to appropriate message roles:
    /// - Pinned → system messages
    /// - CompactHistory → system messages (compressed knowledge)
    /// - Conversation entries → parsed as user/assistant based on prefix
    /// - Tool results → user messages with [Tool results] prefix
    /// - Other regions → user messages with region header
    pub fn assemble_messages(&self) -> Vec<leviath_providers::Message> {
        use leviath_core::CacheHint;

        let mut messages = Vec::new();
        // Track region boundaries: (start_index, cache_hint) for each region
        let mut region_boundaries: Vec<(usize, CacheHint)> = Vec::new();

        for region in &self.regions {
            if region.content.is_empty() {
                continue;
            }

            let start_idx = messages.len();
            let hint = region.kind.cache_hint();

            match &region.kind {
                leviath_core::RegionKind::Pinned
                | leviath_core::RegionKind::CompactHistory { .. } => {
                    // System-level content
                    let content = region
                        .content
                        .iter()
                        .map(|e| e.content.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    messages.push(leviath_providers::Message {
                        role: "system".to_string(),
                        content,
                        cache_breakpoint: false,
                    });
                }
                leviath_core::RegionKind::SlidingWindow { .. }
                | leviath_core::RegionKind::Compacting { .. } => {
                    // Conversation-style: parse each entry by prefix
                    for entry in &region.content {
                        let trimmed = entry.content.trim();
                        if let Some(rest) = trimmed.strip_prefix("Assistant: ") {
                            messages.push(leviath_providers::Message {
                                role: "assistant".to_string(),
                                content: rest.to_string(),
                                cache_breakpoint: false,
                            });
                        } else if let Some(rest) = trimmed.strip_prefix("User: ") {
                            messages.push(leviath_providers::Message {
                                role: "user".to_string(),
                                content: rest.to_string(),
                                cache_breakpoint: false,
                            });
                        } else {
                            messages.push(leviath_providers::Message {
                                role: "user".to_string(),
                                content: entry.content.clone(),
                                cache_breakpoint: false,
                            });
                        }
                    }
                }
                leviath_core::RegionKind::Temporary => {
                    // Tool results or temporary data
                    let content = region
                        .content
                        .iter()
                        .map(|e| e.content.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    messages.push(leviath_providers::Message {
                        role: "user".to_string(),
                        content: format!("[Tool results from {}]:\n{}", region.name, content),
                        cache_breakpoint: false,
                    });
                }
                leviath_core::RegionKind::Clearable => {
                    let content = region
                        .content
                        .iter()
                        .map(|e| e.content.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    messages.push(leviath_providers::Message {
                        role: "user".to_string(),
                        content: format!("[{}]:\n{}", region.name, content),
                        cache_breakpoint: false,
                    });
                }
            }

            // Record region boundary if it produced any messages
            if messages.len() > start_idx {
                region_boundaries.push((start_idx, hint));
            }
        }

        // Ensure there's at least one user message (LLM APIs require it)
        if !messages.iter().any(|m| m.role == "user") {
            messages.push(leviath_providers::Message {
                role: "user".to_string(),
                content: "Begin.".to_string(),
                cache_breakpoint: false,
            });
        }

        // Apply cache breakpoints at region boundaries (max 4 for Anthropic compat)
        let mut breakpoints_set = 0usize;
        const MAX_BREAKPOINTS: usize = 4;

        for (start_idx, hint) in &region_boundaries {
            if breakpoints_set >= MAX_BREAKPOINTS {
                break;
            }
            match hint {
                CacheHint::Always | CacheHint::UntilChanged => {
                    // Mark the last message of this region group
                    // Find the end: it's just before the next region's start, or end of messages
                    let region_end = region_boundaries
                        .iter()
                        .find(|(s, _)| *s > *start_idx)
                        .map(|(s, _)| s - 1)
                        .unwrap_or(messages.len() - 1);
                    if region_end < messages.len() {
                        messages[region_end].cache_breakpoint = true;
                        breakpoints_set += 1;
                    }
                }
                CacheHint::SlidingPrefix { stable_fraction } => {
                    // Find end of this region's messages
                    let region_end = region_boundaries
                        .iter()
                        .find(|(s, _)| *s > *start_idx)
                        .map(|(s, _)| *s)
                        .unwrap_or(messages.len());
                    let region_msg_count = region_end - start_idx;
                    if region_msg_count > 1 {
                        let stable_count =
                            (region_msg_count as f32 * stable_fraction).floor() as usize;
                        if stable_count > 0 {
                            let bp_idx = start_idx + stable_count - 1;
                            if bp_idx < messages.len() {
                                messages[bp_idx].cache_breakpoint = true;
                                breakpoints_set += 1;
                            }
                        }
                    }
                }
                CacheHint::Never => {}
            }
        }

        messages
    }

    /// Assemble the complete prompt from all regions in order (convenience wrapper).
    pub fn assemble_prompt(&self) -> String {
        self.assemble_messages()
            .iter()
            .map(|m| format!("[{}]: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n\n")
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
        self.messages.sort_by_key(|m| std::cmp::Reverse(m.priority));
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
        region
            .add_entry("test content 1".to_string(), 1000)
            .unwrap();
        region
            .add_entry("test content 2".to_string(), 1000)
            .unwrap();
        window.add_region(region);

        assert_eq!(window.current_tokens, 2000);

        // Evict should clear the entire Clearable region
        let result = window.try_evict(1000).unwrap();
        assert_eq!(result.tokens_freed, 2000);
        assert!(result.needs_compaction.is_empty());
        assert_eq!(window.current_tokens, 0);
    }

    #[test]
    fn test_temporary_eviction_oldest_first() {
        let mut window = ContextWindow::new(10000);
        let mut region = Region::new("temp".to_string(), RegionKind::Temporary, 5000);
        region.add_entry("old content".to_string(), 1000).unwrap();
        region
            .add_entry("middle content".to_string(), 1000)
            .unwrap();
        region.add_entry("new content".to_string(), 1000).unwrap();
        window.add_region(region);

        assert_eq!(window.current_tokens, 3000);

        // Evict should remove oldest first
        let result = window.try_evict(500).unwrap();
        assert!(result.tokens_freed >= 1000); // Should free at least one entry
        assert!(result.needs_compaction.is_empty());

        // Check that oldest was removed
        let region = window.get_region("temp").unwrap();
        assert_eq!(region.content.len(), 2);
        assert_eq!(region.content[0].content, "middle content");
    }

    #[test]
    fn test_sliding_window_never_reduced() {
        let mut window = ContextWindow::new(10000);
        let mut region = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow { max_items: 5 },
            5000,
        );
        region.add_entry("msg 1".to_string(), 1000).unwrap();
        region.add_entry("msg 2".to_string(), 1000).unwrap();
        region.add_entry("msg 3".to_string(), 1000).unwrap();
        window.add_region(region);

        let initial_count = window.get_region("conversation").unwrap().content.len();

        // Try to evict - should not touch SlidingWindow
        window.try_evict(1000).ok();

        let after_count = window.get_region("conversation").unwrap().content.len();
        assert_eq!(
            initial_count, after_count,
            "SlidingWindow should never be reduced during eviction"
        );
    }

    #[test]
    fn test_pinned_never_touched() {
        let mut window = ContextWindow::new(10000);
        let mut region = Region::new("architecture".to_string(), RegionKind::Pinned, 3000);
        region
            .add_entry("architecture diagram".to_string(), 2000)
            .unwrap();
        window.add_region(region);

        let initial_tokens = window.get_region("architecture").unwrap().current_tokens;

        // Try to evict - should not touch Pinned
        window.try_evict(1000).ok();

        let after_tokens = window.get_region("architecture").unwrap().current_tokens;
        assert_eq!(
            initial_tokens, after_tokens,
            "Pinned region should never be evicted"
        );
    }

    #[test]
    fn test_eviction_cascade_order() {
        let mut window = ContextWindow::new(10000);

        // Add Clearable region
        let mut clearable = Region::new("scratch".to_string(), RegionKind::Clearable, 2000);
        clearable
            .add_entry("scratch data".to_string(), 1000)
            .unwrap();
        window.add_region(clearable);

        // Add Temporary region
        let mut temporary = Region::new("temp".to_string(), RegionKind::Temporary, 3000);
        temporary
            .add_entry("temp data 1".to_string(), 1000)
            .unwrap();
        temporary
            .add_entry("temp data 2".to_string(), 1000)
            .unwrap();
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
        region1
            .add_entry("You are a helpful assistant.".to_string(), 100)
            .unwrap();
        window.add_region(region1);

        let mut region2 = Region::new("conversation".to_string(), RegionKind::Temporary, 2000);
        region2.add_entry("User: Hello".to_string(), 50).unwrap();
        region2
            .add_entry("Assistant: Hi there!".to_string(), 50)
            .unwrap();
        window.add_region(region2);

        let prompt = window.assemble_prompt();
        assert!(prompt.contains("You are a helpful assistant."));
        assert!(prompt.contains("User: Hello"));
        assert!(prompt.contains("Hi there!"));
    }

    #[test]
    fn test_assemble_messages() {
        let mut window = ContextWindow::new(10000);

        let mut system = Region::new("system".to_string(), RegionKind::Pinned, 1000);
        system
            .add_entry("You are a helpful assistant.".to_string(), 100)
            .unwrap();
        window.add_region(system);

        let mut conv = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow { max_items: 10 },
            5000,
        );
        conv.add_entry("User: Hello".to_string(), 50).unwrap();
        conv.add_entry("Assistant: Hi there!".to_string(), 50)
            .unwrap();
        window.add_region(conv);

        let msgs = window.assemble_messages();
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, "You are a helpful assistant.");
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[1].content, "Hello");
        assert_eq!(msgs[2].role, "assistant");
        assert_eq!(msgs[2].content, "Hi there!");
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
    fn test_eviction_result_identifies_compaction_regions() {
        // Small window so compacting region fills most of it
        let mut window = ContextWindow::new(1000);
        // Add a compacting region that's over threshold
        let mut compacting = Region::new(
            "impl".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 500,
            },
            900,
        );
        compacting
            .add_entry("lots of content".to_string(), 600)
            .unwrap();
        window.add_region(compacting);

        assert_eq!(window.current_tokens, 600);

        // Request 500 free tokens — only 400 free, can't free clearable/temporary, so compacting should be identified
        let result = window.try_evict(500).unwrap();
        assert_eq!(result.tokens_freed, 0);
        assert_eq!(result.needs_compaction, vec!["impl".to_string()]);
    }

    #[test]
    fn test_try_evict_returns_needs_compaction_when_full() {
        let mut window = ContextWindow::new(1200);

        // Fill with compacting region content above threshold
        let mut compacting = Region::new(
            "analysis".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 800,
            },
            1100,
        );
        compacting.add_entry("data 1".to_string(), 500).unwrap();
        compacting.add_entry("data 2".to_string(), 500).unwrap();
        window.add_region(compacting);

        // 200 free tokens, request 500 → needs compaction
        let result = window.try_evict(500).unwrap();
        assert_eq!(result.tokens_freed, 0);
        assert!(result.needs_compaction.contains(&"analysis".to_string()));
    }

    #[test]
    fn test_needs_compaction_component() {
        let comp = NeedsCompaction {
            regions: vec!["impl".to_string(), "analysis".to_string()],
        };
        assert_eq!(comp.regions.len(), 2);
        assert_eq!(comp.regions[0], "impl");
        assert_eq!(comp.regions[1], "analysis");
    }

    #[test]
    fn test_agent_status_cancelled() {
        let status = AgentStatus::Cancelled;
        match status {
            AgentStatus::Cancelled => {} // OK
            _ => panic!("Expected Cancelled"),
        }
    }

    #[test]
    fn test_parent_ref_component() {
        let parent_ref = super::ParentRef {
            parent_entity: Entity::from_raw(42),
            parent_agent_id: "coder-01".to_string(),
            depth: 1,
        };
        assert_eq!(parent_ref.parent_agent_id, "coder-01");
        assert_eq!(parent_ref.depth, 1);
    }

    #[test]
    fn test_children_component() {
        let children = super::SubAgentChildren {
            children: vec![Entity::from_raw(1), Entity::from_raw(2)],
            max_child_depth: 3,
        };
        assert_eq!(children.children.len(), 2);
        assert_eq!(children.max_child_depth, 3);
    }

    #[test]
    fn test_agent_state_with_children_fields() {
        let state = AgentState {
            agent_id: "test-01".to_string(),
            current_stage: "analyze".to_string(),
            iteration: 0,
            status: AgentStatus::Active,
            spawned_children_ids: vec!["child-01".to_string(), "child-02".to_string()],
            pending_wait: Some("child-01".to_string()),
            accepts_messages: true,
        };
        assert_eq!(state.spawned_children_ids.len(), 2);
        assert_eq!(state.pending_wait, Some("child-01".to_string()));
    }

    #[test]
    fn test_assemble_messages_cache_breakpoints_pinned() {
        let mut window = ContextWindow::new(10000);

        let mut system = Region::new("system".to_string(), RegionKind::Pinned, 1000);
        system
            .add_entry("You are a helpful assistant.".to_string(), 100)
            .unwrap();
        window.add_region(system);

        let mut conv = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow { max_items: 10 },
            5000,
        );
        conv.add_entry("User: Hello".to_string(), 50).unwrap();
        conv.add_entry("Assistant: Hi".to_string(), 50).unwrap();
        conv.add_entry("User: How are you?".to_string(), 50)
            .unwrap();
        conv.add_entry("Assistant: Good!".to_string(), 50).unwrap();
        window.add_region(conv);

        let msgs = window.assemble_messages();

        // System (Pinned) message should have cache_breakpoint = true
        assert!(
            msgs[0].cache_breakpoint,
            "Pinned region last message should be a cache breakpoint"
        );

        // SlidingWindow with 4 messages: stable prefix = floor(4 * 0.75) = 3
        // So index 0 = system, index 1..4 = conversation, breakpoint at index 1+3-1 = 3
        assert!(
            msgs[3].cache_breakpoint,
            "SlidingWindow stable prefix boundary should be a cache breakpoint"
        );
    }

    #[test]
    fn test_assemble_messages_max_4_breakpoints() {
        let mut window = ContextWindow::new(100000);

        // Add 5 pinned regions — only first 4 should get breakpoints
        for i in 0..5 {
            let mut region = Region::new(format!("pinned_{}", i), RegionKind::Pinned, 10000);
            region.add_entry(format!("Content {}", i), 100).unwrap();
            window.add_region(region);
        }

        // Add a compacting region
        let mut compacting = Region::new(
            "compacting".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 5000,
            },
            10000,
        );
        compacting
            .add_entry("User: hello".to_string(), 100)
            .unwrap();
        window.add_region(compacting);

        let msgs = window.assemble_messages();
        let bp_count = msgs.iter().filter(|m| m.cache_breakpoint).count();
        assert!(
            bp_count <= 4,
            "Should not exceed 4 cache breakpoints, got {}",
            bp_count
        );
    }

    #[test]
    fn test_assemble_messages_no_breakpoints_on_temporary() {
        let mut window = ContextWindow::new(10000);

        let mut temp = Region::new("temp".to_string(), RegionKind::Temporary, 5000);
        temp.add_entry("tool output".to_string(), 100).unwrap();
        window.add_region(temp);

        let msgs = window.assemble_messages();
        assert!(
            !msgs.iter().any(|m| m.cache_breakpoint),
            "Temporary regions should never get cache breakpoints"
        );
    }
}

//! Taint gate checking for tool execution.
//!
//! Implements the gate check logic that runs before outbound tool calls.
//! When taint tracking is enabled, the gate compares the relevant taint
//! level (determined by input mode) against the tool's clearance level.

use leviath_core::taint::{
    GateDecision, GateDecisionSource, GateEvent, InputMode, SecurityConfig, TaintLevel,
    ToolClassification, builtin_tool_classification,
};
use std::collections::HashMap;

use crate::components::ContextWindow;

/// The user's resolution of a blocked outbound tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateResolution {
    /// Allow this one call.
    AllowOnce,
    /// Allow this tool for the rest of the run (session allow).
    AlwaysAllow,
    /// Deny the call — it is not executed; the model gets a blocked result.
    Deny,
}

/// Injected resolver used when the gate blocks an outbound tool call.
///
/// The runtime cannot prompt the user itself (no stdin/IPC), so the CLI
/// provides an implementation that asks via the dashboard/stdin and returns
/// the user's decision. Mirrors how tool execution is injected as a closure.
#[async_trait::async_trait]
pub trait GatePrompt: Send + Sync {
    /// Ask the user how to resolve a blocked outbound call. Implementations
    /// should default to [`GateResolution::Deny`] when no answer is available.
    async fn resolve(&self, decision: &GateDecision) -> GateResolution;
}

/// Type alias for a scripted rule checker function.
/// Takes (tool_name, target, taint_level) and returns Some(script_name) if the rule allows.
pub type ScriptRuleChecker = dyn Fn(&str, Option<&str>, TaintLevel) -> Option<String>;

/// Taint gate — checks whether a tool invocation is allowed given the
/// current taint state of the context window. Attached per-agent (as an ECS
/// component) when the agent's blueprint opts into taint tracking.
#[derive(Debug, Clone, bevy_ecs::component::Component)]
pub struct TaintGate {
    /// Security configuration.
    config: SecurityConfig,
    /// Per-tool classification overrides (from agent.leviath or user policy).
    tool_overrides: HashMap<String, ToolClassification>,
    /// Audit log of gate events.
    audit_log: Vec<GateEvent>,
}

impl TaintGate {
    /// Create a new taint gate with the given security config.
    pub fn new(config: SecurityConfig) -> Self {
        Self {
            config,
            tool_overrides: HashMap::new(),
            audit_log: Vec::new(),
        }
    }

    /// Create a disabled taint gate (no tracking, no gating).
    pub fn disabled() -> Self {
        Self {
            config: SecurityConfig {
                taint_tracking: false,
                ..SecurityConfig::default()
            },
            tool_overrides: HashMap::new(),
            audit_log: Vec::new(),
        }
    }

    /// Whether taint tracking is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.taint_tracking
    }

    /// Get the security config.
    pub fn config(&self) -> &SecurityConfig {
        &self.config
    }

    /// Register a tool classification override.
    pub fn set_tool_classification(
        &mut self,
        tool_name: String,
        classification: ToolClassification,
    ) {
        self.tool_overrides.insert(tool_name, classification);
    }

    /// Get the classification for a tool (override first, then built-in default).
    pub fn tool_classification(&self, tool_name: &str) -> ToolClassification {
        self.tool_overrides
            .get(tool_name)
            .cloned()
            .unwrap_or_else(|| builtin_tool_classification(tool_name))
    }

    /// Check the gate for a traditional-mode tool invocation.
    ///
    /// In traditional mode, the overall taint (max across all regions) is
    /// compared against the tool's clearance.
    pub fn check_traditional(
        &mut self,
        agent_id: &str,
        tool_name: &str,
        window: &ContextWindow,
    ) -> GateDecision {
        if !self.config.taint_tracking {
            self.log_event(
                agent_id,
                tool_name,
                InputMode::Traditional,
                TaintLevel::Public,
                TaintLevel::Public,
                true,
                GateDecisionSource::TaintDisabled,
            );
            return GateDecision::Allowed;
        }

        let classification = self.tool_classification(tool_name);

        // Non-outbound tools always pass
        if !classification.is_outbound() {
            self.log_event(
                agent_id,
                tool_name,
                InputMode::Traditional,
                TaintLevel::Public,
                classification.clearance,
                true,
                GateDecisionSource::AutoAllow,
            );
            return GateDecision::Allowed;
        }

        // Get overall taint level
        let taint = window.overall_taint().unwrap_or(TaintLevel::Public);

        if classification.check_clearance(taint) {
            self.log_event(
                agent_id,
                tool_name,
                InputMode::Traditional,
                taint,
                classification.clearance,
                true,
                GateDecisionSource::AutoAllow,
            );
            GateDecision::Allowed
        } else {
            // Identify source regions contributing to the taint
            let source_regions: Vec<String> = window
                .taint_summary()
                .into_iter()
                .filter(|(_, level)| *level > classification.clearance)
                .map(|(name, _)| name)
                .collect();

            self.log_event(
                agent_id,
                tool_name,
                InputMode::Traditional,
                taint,
                classification.clearance,
                false,
                GateDecisionSource::UserDenied, // placeholder — caller decides
            );

            GateDecision::Blocked {
                taint_level: taint,
                clearance: classification.clearance,
                source_regions,
                tool_name: tool_name.to_string(),
            }
        }
    }

    /// Check the gate for a pointer-mode tool invocation.
    ///
    /// In pointer mode, only the taint of the referenced content is checked.
    pub fn check_pointer(
        &mut self,
        agent_id: &str,
        tool_name: &str,
        reference_taint: TaintLevel,
    ) -> GateDecision {
        if !self.config.taint_tracking {
            self.log_event(
                agent_id,
                tool_name,
                InputMode::Pointer,
                TaintLevel::Public,
                TaintLevel::Public,
                true,
                GateDecisionSource::TaintDisabled,
            );
            return GateDecision::Allowed;
        }

        let classification = self.tool_classification(tool_name);

        if !classification.is_outbound() || classification.check_clearance(reference_taint) {
            self.log_event(
                agent_id,
                tool_name,
                InputMode::Pointer,
                reference_taint,
                classification.clearance,
                true,
                GateDecisionSource::AutoAllow,
            );
            GateDecision::Allowed
        } else {
            self.log_event(
                agent_id,
                tool_name,
                InputMode::Pointer,
                reference_taint,
                classification.clearance,
                false,
                GateDecisionSource::UserDenied,
            );
            GateDecision::Blocked {
                taint_level: reference_taint,
                clearance: classification.clearance,
                source_regions: vec![],
                tool_name: tool_name.to_string(),
            }
        }
    }

    /// Check the gate for a filter-mode tool invocation.
    ///
    /// In filter mode, the taint of the source region is checked.
    pub fn check_filter(
        &mut self,
        agent_id: &str,
        tool_name: &str,
        source_region_taint: TaintLevel,
        source_region_name: &str,
    ) -> GateDecision {
        if !self.config.taint_tracking {
            self.log_event(
                agent_id,
                tool_name,
                InputMode::Filter,
                TaintLevel::Public,
                TaintLevel::Public,
                true,
                GateDecisionSource::TaintDisabled,
            );
            return GateDecision::Allowed;
        }

        let classification = self.tool_classification(tool_name);

        if !classification.is_outbound() || classification.check_clearance(source_region_taint) {
            self.log_event(
                agent_id,
                tool_name,
                InputMode::Filter,
                source_region_taint,
                classification.clearance,
                true,
                GateDecisionSource::AutoAllow,
            );
            GateDecision::Allowed
        } else {
            self.log_event(
                agent_id,
                tool_name,
                InputMode::Filter,
                source_region_taint,
                classification.clearance,
                false,
                GateDecisionSource::UserDenied,
            );
            GateDecision::Blocked {
                taint_level: source_region_taint,
                clearance: classification.clearance,
                source_regions: vec![source_region_name.to_string()],
                tool_name: tool_name.to_string(),
            }
        }
    }

    /// Check the gate with allowlist and scripted rule support.
    ///
    /// This is the full gate check that runs:
    /// 1. Basic taint vs clearance check
    /// 2. If blocked, check static allowlist rules
    /// 3. If still blocked, check scripted rules (if checker provided)
    /// 4. Return final decision
    pub fn check_with_policy(
        &mut self,
        agent_id: &str,
        tool_name: &str,
        window: &ContextWindow,
        target: Option<&str>,
        policy: &leviath_core::PolicyConfig,
        script_checker: Option<&ScriptRuleChecker>,
    ) -> GateDecision {
        let decision = self.check_traditional(agent_id, tool_name, window);

        if decision.is_allowed() {
            return decision;
        }

        // Extract taint level from the blocked decision. `decision` is
        // necessarily `Blocked` here: `GateDecision` has only two variants and
        // the `Allowed` case already returned above.
        let (taint, clearance) = decision
            .blocked_levels()
            .expect("infallible: a non-Allowed GateDecision is always Blocked");

        // Check static allowlist rules
        if let Some(rule_idx) = policy.check_allowlist(tool_name, target, taint) {
            self.log_event(
                agent_id,
                tool_name,
                InputMode::Traditional,
                taint,
                clearance,
                true,
                GateDecisionSource::AllowlistRule {
                    rule_index: rule_idx,
                },
            );
            return GateDecision::Allowed;
        }

        // Check scripted rules
        if let Some(checker) = script_checker
            && let Some(script_name) = checker(tool_name, target, taint)
        {
            self.log_event(
                agent_id,
                tool_name,
                InputMode::Traditional,
                taint,
                clearance,
                true,
                GateDecisionSource::ScriptedRule { script_name },
            );
            return GateDecision::Allowed;
        }

        decision
    }

    /// Record an allow decision from the user or allowlist.
    pub fn record_allow(
        &mut self,
        agent_id: &str,
        tool_name: &str,
        input_mode: InputMode,
        taint: TaintLevel,
        clearance: TaintLevel,
        source: GateDecisionSource,
    ) {
        self.log_event(
            agent_id, tool_name, input_mode, taint, clearance, true, source,
        );
    }

    /// Record a deny decision (the user or a default policy denied the call).
    pub fn record_deny(
        &mut self,
        agent_id: &str,
        tool_name: &str,
        input_mode: InputMode,
        taint: TaintLevel,
        clearance: TaintLevel,
        source: GateDecisionSource,
    ) {
        self.log_event(
            agent_id, tool_name, input_mode, taint, clearance, false, source,
        );
    }

    /// Apply the user's resolution of a blocked outbound call: record the
    /// audit event and, for `AlwaysAllow`, raise the tool's clearance for the
    /// rest of the run. Returns `Some((tool_id, message))` when the call is
    /// denied (and must be skipped), or `None` when it should execute.
    ///
    /// Synchronous so it can be unit-tested directly, keeping the async run-loop
    /// path that awaits the prompt as thin as possible.
    pub fn apply_resolution(
        &mut self,
        agent_id: &str,
        tool_name: &str,
        tool_id: &str,
        taint: TaintLevel,
        clearance: TaintLevel,
        resolution: GateResolution,
    ) -> Option<(String, String)> {
        match resolution {
            GateResolution::AllowOnce => {
                self.record_allow(
                    agent_id,
                    tool_name,
                    InputMode::Traditional,
                    taint,
                    clearance,
                    GateDecisionSource::UserAllowOnce,
                );
                None
            }
            GateResolution::AlwaysAllow => {
                self.record_allow(
                    agent_id,
                    tool_name,
                    InputMode::Traditional,
                    taint,
                    clearance,
                    GateDecisionSource::UserAlwaysAllow,
                );
                let mut cls = self.tool_classification(tool_name);
                cls.clearance = TaintLevel::Private;
                self.set_tool_classification(tool_name.to_string(), cls);
                None
            }
            GateResolution::Deny => {
                self.record_deny(
                    agent_id,
                    tool_name,
                    InputMode::Traditional,
                    taint,
                    clearance,
                    GateDecisionSource::UserDenied,
                );
                Some((
                    tool_id.to_string(),
                    format!(
                        "[blocked] Tool '{}' would send data at {} sensitivity, above its {} \
                         clearance. Denied by user.",
                        tool_name, taint, clearance
                    ),
                ))
            }
        }
    }

    /// Get the audit log.
    pub fn audit_log(&self) -> &[GateEvent] {
        &self.audit_log
    }

    /// Clear the audit log.
    pub fn clear_audit_log(&mut self) {
        self.audit_log.clear();
    }

    #[allow(clippy::too_many_arguments)]
    fn log_event(
        &mut self,
        agent_id: &str,
        tool_name: &str,
        input_mode: InputMode,
        taint_level: TaintLevel,
        clearance: TaintLevel,
        allowed: bool,
        decision_source: GateDecisionSource,
    ) {
        self.audit_log.push(GateEvent {
            timestamp: chrono::Utc::now().timestamp(),
            agent_id: agent_id.to_string(),
            tool_name: tool_name.to_string(),
            input_mode,
            taint_level,
            clearance,
            allowed,
            decision_source,
        });
    }
}

/// Pointer reference resolver — resolves PointerRef to actual content
/// and its taint level from a ContextWindow.
#[derive(Debug)]
pub struct PointerResolver;

/// Result of resolving a pointer reference.
#[derive(Debug, Clone)]
pub struct ResolvedPointer {
    /// The resolved content.
    pub content: String,
    /// The taint level of the resolved content.
    pub taint_level: TaintLevel,
}

/// Error when resolving a pointer reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointerError {
    /// Region not found.
    RegionNotFound(String),
    /// Chunk ID not found in region.
    ChunkNotFound { region: String, chunk_id: String },
    /// Offset range out of bounds.
    OffsetOutOfBounds {
        region: String,
        start: usize,
        end: usize,
    },
    /// Taint tracking not enabled on region.
    TaintNotEnabled(String),
}

impl std::fmt::Display for PointerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PointerError::RegionNotFound(r) => write!(f, "Region not found: {}", r),
            PointerError::ChunkNotFound { region, chunk_id } => {
                write!(f, "Chunk '{}' not found in region '{}'", chunk_id, region)
            }
            PointerError::OffsetOutOfBounds { region, start, end } => {
                write!(
                    f,
                    "Offset range {}..{} out of bounds in region '{}'",
                    start, end, region
                )
            }
            PointerError::TaintNotEnabled(r) => {
                write!(f, "Taint tracking not enabled on region '{}'", r)
            }
        }
    }
}

impl PointerResolver {
    /// Resolve a PointerRef against a ContextWindow.
    pub fn resolve(
        window: &ContextWindow,
        pointer: &leviath_core::taint::PointerRef,
    ) -> Result<ResolvedPointer, PointerError> {
        use leviath_core::taint::PointerRef;

        match pointer {
            PointerRef::ChunkId { region, chunk_id } => {
                let reg = window
                    .get_region(region)
                    .ok_or_else(|| PointerError::RegionNotFound(region.clone()))?;

                // Chunk IDs are entry metadata with "chunk_id" key
                let (idx, entry) = reg
                    .content
                    .iter()
                    .enumerate()
                    .find(|(_, e)| {
                        e.metadata
                            .as_ref()
                            .and_then(|m| m.get("chunk_id"))
                            .and_then(|v| v.as_str())
                            == Some(chunk_id.as_str())
                    })
                    .ok_or_else(|| PointerError::ChunkNotFound {
                        region: region.clone(),
                        chunk_id: chunk_id.clone(),
                    })?;

                let taint = reg
                    .taint
                    .as_ref()
                    .ok_or_else(|| PointerError::TaintNotEnabled(region.clone()))?
                    .entry_taint(idx)
                    .unwrap_or(TaintLevel::Public);

                Ok(ResolvedPointer {
                    content: entry.content.clone(),
                    taint_level: taint,
                })
            }
            PointerRef::OffsetRange { region, start, end } => {
                let reg = window
                    .get_region(region)
                    .ok_or_else(|| PointerError::RegionNotFound(region.clone()))?;

                if *start >= reg.content.len() || *end > reg.content.len() || start >= end {
                    return Err(PointerError::OffsetOutOfBounds {
                        region: region.clone(),
                        start: *start,
                        end: *end,
                    });
                }

                let content: String = reg.content[*start..*end]
                    .iter()
                    .map(|e| e.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");

                let taint = reg
                    .taint
                    .as_ref()
                    .ok_or_else(|| PointerError::TaintNotEnabled(region.clone()))?
                    .range_taint(*start, *end);

                Ok(ResolvedPointer {
                    content,
                    taint_level: taint,
                })
            }
        }
    }
}

/// Filter input resolver — resolves a FilterInput against a ContextWindow.
///
/// In a full system, this would spawn a scoped sub-agent. Here we provide
/// the resolution logic that determines the taint level and validates the
/// filter configuration.
#[derive(Debug)]
pub struct FilterResolver;

/// Result of resolving a filter input.
#[derive(Debug, Clone)]
pub struct ResolvedFilter {
    /// Content from the source region (for the sub-agent to process).
    pub source_content: String,
    /// Taint level of the source region (output inherits this).
    pub taint_level: TaintLevel,
    /// The filter operation to apply.
    pub operation: leviath_core::taint::FilterOperation,
}

/// Error when resolving a filter input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterError {
    /// Source region not found.
    RegionNotFound(String),
    /// Taint tracking not enabled on source region.
    TaintNotEnabled(String),
    /// Filter mode is disabled.
    FilterDisabled,
    /// Freeform mode not enabled (user tried freeform but config says structured).
    FreeformNotEnabled,
}

impl std::fmt::Display for FilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilterError::RegionNotFound(r) => write!(f, "Source region not found: {}", r),
            FilterError::TaintNotEnabled(r) => {
                write!(f, "Taint tracking not enabled on region '{}'", r)
            }
            FilterError::FilterDisabled => write!(f, "Filter mode is disabled"),
            FilterError::FreeformNotEnabled => {
                write!(
                    f,
                    "Freeform filter mode not enabled (config says structured)"
                )
            }
        }
    }
}

impl FilterResolver {
    /// Resolve a FilterInput against a ContextWindow.
    pub fn resolve(
        window: &ContextWindow,
        filter: &leviath_core::taint::FilterInput,
        config: &SecurityConfig,
    ) -> Result<ResolvedFilter, FilterError> {
        // Check filter mode is enabled
        if config.filter_mode.is_none() {
            return Err(FilterError::FilterDisabled);
        }

        let region = window
            .get_region(&filter.source_region)
            .ok_or_else(|| FilterError::RegionNotFound(filter.source_region.clone()))?;

        let taint = region
            .taint_level()
            .ok_or_else(|| FilterError::TaintNotEnabled(filter.source_region.clone()))?;

        let source_content: String = region
            .content
            .iter()
            .map(|e| e.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        Ok(ResolvedFilter {
            source_content,
            taint_level: taint,
            operation: filter.operation.clone(),
        })
    }
}

/// Degradation engine — manages fallback when a higher-precision input mode fails.
#[derive(Debug)]
pub struct DegradationEngine;

/// Error produced by the degradation engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DegradationError {
    /// The current mode failed and no fallback is configured.
    NoFallback { current_mode: InputMode },
    /// All modes in the degradation path have been exhausted.
    AllModesExhausted,
    /// A specific mode error with message.
    ModeError { mode: InputMode, message: String },
}

impl std::fmt::Display for DegradationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DegradationError::NoFallback { current_mode } => {
                write!(
                    f,
                    "Input mode '{}' failed: no fallback configured in degradation path",
                    current_mode
                )
            }
            DegradationError::AllModesExhausted => {
                write!(f, "All input modes in degradation path have been exhausted")
            }
            DegradationError::ModeError { mode, message } => {
                write!(f, "Input mode '{}' failed: {}", mode, message)
            }
        }
    }
}

impl DegradationEngine {
    /// Get the next mode to try after the given mode fails.
    /// Returns a descriptive error message for the user alongside the next mode.
    pub fn degrade(
        config: &SecurityConfig,
        current_mode: &InputMode,
    ) -> Result<(InputMode, String), DegradationError> {
        match config.next_fallback(current_mode) {
            Some(next) => {
                let message = format!(
                    "Input mode '{}' failed. Degrading to '{}' mode per configured degradation path.",
                    current_mode, next
                );
                Ok((next.clone(), message))
            }
            None => Err(DegradationError::NoFallback {
                current_mode: current_mode.clone(),
            }),
        }
    }

    /// Run through the full degradation path, returning the first available mode.
    pub fn first_available(config: &SecurityConfig) -> Option<InputMode> {
        config
            .degradation
            .iter()
            .find(|mode| config.mode_available(mode))
            .cloned()
    }

    /// Validate that the degradation path is valid (all modes are available).
    /// Returns modes in the path that are not available.
    pub fn validate_path(config: &SecurityConfig) -> Vec<InputMode> {
        config
            .degradation
            .iter()
            .filter(|mode| !config.mode_available(mode))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::taint::ToolDirection;
    use leviath_core::{Region, RegionKind};

    fn make_window_with_taint(taint: TaintLevel) -> ContextWindow {
        let mut window = ContextWindow::new(10000);
        let region =
            Region::new("conv".to_string(), RegionKind::Temporary, 5000).with_taint_tracking();
        window.add_region(region);
        if taint != TaintLevel::Public {
            window
                .add_tainted_to_region("conv", "data".to_string(), 10, taint)
                .unwrap();
        }
        window
    }

    #[test]
    fn gate_disabled_always_allows() {
        let mut gate = TaintGate::disabled();
        assert!(!gate.is_enabled());

        let window = make_window_with_taint(TaintLevel::Private);
        let decision = gate.check_traditional("agent-1", "shell", &window);
        assert!(decision.is_allowed());
        assert_eq!(gate.audit_log().len(), 1);
        assert_eq!(
            gate.audit_log()[0].decision_source,
            GateDecisionSource::TaintDisabled
        );
    }

    #[test]
    fn gate_allows_non_outbound_tool() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let window = make_window_with_taint(TaintLevel::Private);
        let decision = gate.check_traditional("agent-1", "read_file", &window);
        assert!(decision.is_allowed());
    }

    #[test]
    fn gate_allows_outbound_when_taint_within_clearance() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let window = make_window_with_taint(TaintLevel::Public);
        let decision = gate.check_traditional("agent-1", "shell", &window);
        assert!(decision.is_allowed());
    }

    #[test]
    fn gate_blocks_outbound_when_taint_exceeds_clearance() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let window = make_window_with_taint(TaintLevel::Private);
        let decision = gate.check_traditional("agent-1", "shell", &window);
        assert!(!decision.is_allowed());
        assert_eq!(
            decision,
            GateDecision::Blocked {
                taint_level: TaintLevel::Private,
                clearance: TaintLevel::Public,
                source_regions: vec!["conv".to_string()],
                tool_name: "shell".to_string(),
            }
        );
    }

    #[test]
    fn gate_uses_tool_override() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        gate.set_tool_classification(
            "shell".to_string(),
            ToolClassification::new(
                TaintLevel::Public,
                ToolDirection::Outbound,
                TaintLevel::Private, // relaxed clearance
            ),
        );
        let window = make_window_with_taint(TaintLevel::Private);
        let decision = gate.check_traditional("agent-1", "shell", &window);
        assert!(decision.is_allowed());
    }

    #[test]
    fn gate_blocked_identifies_source_regions() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let mut window = ContextWindow::new(10000);
        let r1 =
            Region::new("clean".to_string(), RegionKind::Temporary, 5000).with_taint_tracking();
        let r2 =
            Region::new("dirty".to_string(), RegionKind::Temporary, 5000).with_taint_tracking();
        window.add_region(r1);
        window.add_region(r2);

        window
            .add_tainted_to_region("clean", "ok".to_string(), 5, TaintLevel::Public)
            .unwrap();
        window
            .add_tainted_to_region("dirty", "secret".to_string(), 5, TaintLevel::Private)
            .unwrap();

        let decision = gate.check_traditional("agent-1", "shell", &window);
        // Only the Private "dirty" region exceeds shell's Public clearance;
        // the Public "clean" region is not reported.
        assert_eq!(
            decision,
            GateDecision::Blocked {
                taint_level: TaintLevel::Private,
                clearance: TaintLevel::Public,
                source_regions: vec!["dirty".to_string()],
                tool_name: "shell".to_string(),
            }
        );
    }

    #[test]
    fn gate_pointer_mode_allows_clean_reference() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let decision = gate.check_pointer("agent-1", "shell", TaintLevel::Public);
        assert!(decision.is_allowed());
    }

    #[test]
    fn gate_pointer_mode_blocks_tainted_reference() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let decision = gate.check_pointer("agent-1", "shell", TaintLevel::Internal);
        assert!(!decision.is_allowed());
    }

    #[test]
    fn gate_pointer_mode_non_outbound_allows() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let decision = gate.check_pointer("agent-1", "read_file", TaintLevel::Private);
        assert!(decision.is_allowed());
    }

    #[test]
    fn gate_filter_mode_allows_clean_region() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let decision = gate.check_filter("agent-1", "shell", TaintLevel::Public, "research");
        assert!(decision.is_allowed());
    }

    #[test]
    fn gate_filter_mode_blocks_tainted_region() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let decision = gate.check_filter("agent-1", "shell", TaintLevel::Private, "conversation");
        assert!(!decision.is_allowed());
        assert_eq!(
            decision,
            GateDecision::Blocked {
                taint_level: TaintLevel::Private,
                clearance: TaintLevel::Public,
                source_regions: vec!["conversation".to_string()],
                tool_name: "shell".to_string(),
            }
        );
    }

    #[test]
    fn gate_filter_mode_disabled_allows() {
        let mut gate = TaintGate::disabled();
        let decision = gate.check_filter("agent-1", "shell", TaintLevel::Private, "conversation");
        assert!(decision.is_allowed());
    }

    #[test]
    fn gate_pointer_mode_disabled_allows() {
        let mut gate = TaintGate::disabled();
        let decision = gate.check_pointer("agent-1", "shell", TaintLevel::Private);
        assert!(decision.is_allowed());
    }

    #[test]
    fn gate_audit_log_records_events() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let window = make_window_with_taint(TaintLevel::Public);

        gate.check_traditional("agent-1", "shell", &window);
        gate.check_traditional("agent-1", "read_file", &window);

        assert_eq!(gate.audit_log().len(), 2);
        assert!(gate.audit_log()[0].allowed);
        assert!(gate.audit_log()[1].allowed);
    }

    #[test]
    fn gate_clear_audit_log() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let window = make_window_with_taint(TaintLevel::Public);
        gate.check_traditional("agent-1", "shell", &window);
        assert_eq!(gate.audit_log().len(), 1);
        gate.clear_audit_log();
        assert_eq!(gate.audit_log().len(), 0);
    }

    #[test]
    fn gate_record_allow() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        gate.record_allow(
            "agent-1",
            "shell",
            InputMode::Traditional,
            TaintLevel::Private,
            TaintLevel::Public,
            GateDecisionSource::UserAllowOnce,
        );
        assert_eq!(gate.audit_log().len(), 1);
        assert!(gate.audit_log()[0].allowed);
        assert_eq!(
            gate.audit_log()[0].decision_source,
            GateDecisionSource::UserAllowOnce
        );
    }

    #[test]
    fn gate_tool_classification_returns_override() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let custom = ToolClassification::new(
            TaintLevel::Private,
            ToolDirection::Outbound,
            TaintLevel::Private,
        );
        gate.set_tool_classification("my_tool".to_string(), custom.clone());
        assert_eq!(gate.tool_classification("my_tool"), custom);
    }

    #[test]
    fn gate_tool_classification_falls_back_to_builtin() {
        let gate = TaintGate::new(SecurityConfig::default());
        let tc = gate.tool_classification("read_file");
        assert_eq!(tc.direction, ToolDirection::Inbound);
    }

    #[test]
    fn gate_new_and_config() {
        let config = SecurityConfig {
            taint_tracking: true,
            pointer_mode: true,
            ..SecurityConfig::default()
        };
        let gate = TaintGate::new(config.clone());
        assert!(gate.is_enabled());
        assert!(gate.config().pointer_mode);
    }

    // ─── PointerResolver ────────────────────────────────────────────────────

    fn make_window_with_chunks() -> ContextWindow {
        let mut window = ContextWindow::new(10000);
        let mut region =
            Region::new("research".to_string(), RegionKind::Temporary, 5000).with_taint_tracking();

        // Add entries with chunk_id metadata
        region
            .add_entry_with_metadata(
                "public search results".to_string(),
                10,
                serde_json::json!({"chunk_id": "chunk-1"}),
            )
            .unwrap();
        // Manually track taint (add_entry_with_metadata tracks as Public)

        region
            .add_entry_with_metadata(
                "private calendar data".to_string(),
                10,
                serde_json::json!({"chunk_id": "chunk-2"}),
            )
            .unwrap();

        // Override taint for second entry.
        // First entry is Public (default from add_entry_with_metadata); remove
        // both Public entries and re-add with correct taints.
        let taint = region
            .taint
            .as_mut()
            .expect("region was created with taint tracking");
        taint.clear();
        taint.add_entry(TaintLevel::Public);
        taint.add_entry(TaintLevel::Private);

        window.add_region(region);
        window
    }

    #[test]
    fn pointer_resolve_chunk_id_public() {
        let window = make_window_with_chunks();
        let pointer = leviath_core::taint::PointerRef::ChunkId {
            region: "research".into(),
            chunk_id: "chunk-1".into(),
        };
        let result = PointerResolver::resolve(&window, &pointer).unwrap();
        assert_eq!(result.content, "public search results");
        assert_eq!(result.taint_level, TaintLevel::Public);
    }

    #[test]
    fn pointer_resolve_chunk_id_private() {
        let window = make_window_with_chunks();
        let pointer = leviath_core::taint::PointerRef::ChunkId {
            region: "research".into(),
            chunk_id: "chunk-2".into(),
        };
        let result = PointerResolver::resolve(&window, &pointer).unwrap();
        assert_eq!(result.content, "private calendar data");
        assert_eq!(result.taint_level, TaintLevel::Private);
    }

    #[test]
    fn pointer_resolve_chunk_not_found() {
        let window = make_window_with_chunks();
        let pointer = leviath_core::taint::PointerRef::ChunkId {
            region: "research".into(),
            chunk_id: "nonexistent".into(),
        };
        let err = PointerResolver::resolve(&window, &pointer).unwrap_err();
        assert_eq!(
            err,
            PointerError::ChunkNotFound {
                region: "research".into(),
                chunk_id: "nonexistent".into()
            }
        );
    }

    #[test]
    fn pointer_resolve_region_not_found() {
        let window = make_window_with_chunks();
        let pointer = leviath_core::taint::PointerRef::ChunkId {
            region: "nope".into(),
            chunk_id: "chunk-1".into(),
        };
        let err = PointerResolver::resolve(&window, &pointer).unwrap_err();
        assert_eq!(err, PointerError::RegionNotFound("nope".into()));
    }

    #[test]
    fn pointer_resolve_offset_range() {
        let window = make_window_with_chunks();
        let pointer = leviath_core::taint::PointerRef::OffsetRange {
            region: "research".into(),
            start: 0,
            end: 1,
        };
        let result = PointerResolver::resolve(&window, &pointer).unwrap();
        assert_eq!(result.content, "public search results");
        assert_eq!(result.taint_level, TaintLevel::Public);
    }

    #[test]
    fn pointer_resolve_offset_range_multi() {
        let window = make_window_with_chunks();
        let pointer = leviath_core::taint::PointerRef::OffsetRange {
            region: "research".into(),
            start: 0,
            end: 2,
        };
        let result = PointerResolver::resolve(&window, &pointer).unwrap();
        assert!(result.content.contains("public search results"));
        assert!(result.content.contains("private calendar data"));
        assert_eq!(result.taint_level, TaintLevel::Private);
    }

    #[test]
    fn pointer_resolve_offset_out_of_bounds() {
        let window = make_window_with_chunks();
        let pointer = leviath_core::taint::PointerRef::OffsetRange {
            region: "research".into(),
            start: 5,
            end: 10,
        };
        let err = PointerResolver::resolve(&window, &pointer).unwrap_err();
        assert_eq!(
            err,
            PointerError::OffsetOutOfBounds {
                region: "research".into(),
                start: 5,
                end: 10
            }
        );
    }

    #[test]
    fn pointer_resolve_taint_not_enabled() {
        let mut window = ContextWindow::new(10000);
        let mut region = Region::new("data".to_string(), RegionKind::Temporary, 5000);
        region
            .add_entry_with_metadata(
                "content".to_string(),
                10,
                serde_json::json!({"chunk_id": "c1"}),
            )
            .unwrap();
        window.add_region(region);

        let pointer = leviath_core::taint::PointerRef::ChunkId {
            region: "data".into(),
            chunk_id: "c1".into(),
        };
        let err = PointerResolver::resolve(&window, &pointer).unwrap_err();
        assert_eq!(err, PointerError::TaintNotEnabled("data".into()));
    }

    #[test]
    fn pointer_error_display() {
        let err = PointerError::RegionNotFound("test".into());
        assert!(err.to_string().contains("Region not found"));

        let err = PointerError::ChunkNotFound {
            region: "r".into(),
            chunk_id: "c".into(),
        };
        assert!(err.to_string().contains("Chunk"));

        let err = PointerError::OffsetOutOfBounds {
            region: "r".into(),
            start: 1,
            end: 5,
        };
        assert!(err.to_string().contains("out of bounds"));

        let err = PointerError::TaintNotEnabled("r".into());
        assert!(err.to_string().contains("Taint tracking not enabled"));
    }

    // ─── FilterResolver ─────────────────────────────────────────────────────

    #[test]
    fn filter_resolve_success() {
        let mut window = ContextWindow::new(10000);
        let mut region =
            Region::new("research".to_string(), RegionKind::Temporary, 5000).with_taint_tracking();
        region
            .add_tainted_entry("fact 1".to_string(), 5, TaintLevel::Public)
            .unwrap();
        region
            .add_tainted_entry("fact 2".to_string(), 5, TaintLevel::Public)
            .unwrap();
        window.add_region(region);

        let config = SecurityConfig {
            filter_mode: Some(leviath_core::FilterMode::Structured),
            ..SecurityConfig::default()
        };
        let filter = leviath_core::taint::FilterInput {
            source_region: "research".into(),
            operation: leviath_core::taint::FilterOperation::Summarize,
            output_format: None,
        };

        let result = FilterResolver::resolve(&window, &filter, &config).unwrap();
        assert!(result.source_content.contains("fact 1"));
        assert!(result.source_content.contains("fact 2"));
        assert_eq!(result.taint_level, TaintLevel::Public);
    }

    #[test]
    fn filter_resolve_disabled() {
        let window = ContextWindow::new(10000);
        let config = SecurityConfig::default(); // filter_mode is None
        let filter = leviath_core::taint::FilterInput {
            source_region: "research".into(),
            operation: leviath_core::taint::FilterOperation::Summarize,
            output_format: None,
        };

        let err = FilterResolver::resolve(&window, &filter, &config).unwrap_err();
        assert_eq!(err, FilterError::FilterDisabled);
    }

    #[test]
    fn filter_resolve_region_not_found() {
        let window = ContextWindow::new(10000);
        let config = SecurityConfig {
            filter_mode: Some(leviath_core::FilterMode::Structured),
            ..SecurityConfig::default()
        };
        let filter = leviath_core::taint::FilterInput {
            source_region: "nope".into(),
            operation: leviath_core::taint::FilterOperation::Summarize,
            output_format: None,
        };

        let err = FilterResolver::resolve(&window, &filter, &config).unwrap_err();
        assert_eq!(err, FilterError::RegionNotFound("nope".into()));
    }

    #[test]
    fn filter_resolve_taint_not_enabled() {
        let mut window = ContextWindow::new(10000);
        let region = Region::new("data".to_string(), RegionKind::Temporary, 5000);
        window.add_region(region);

        let config = SecurityConfig {
            filter_mode: Some(leviath_core::FilterMode::Structured),
            ..SecurityConfig::default()
        };
        let filter = leviath_core::taint::FilterInput {
            source_region: "data".into(),
            operation: leviath_core::taint::FilterOperation::Summarize,
            output_format: None,
        };

        let err = FilterResolver::resolve(&window, &filter, &config).unwrap_err();
        assert_eq!(err, FilterError::TaintNotEnabled("data".into()));
    }

    #[test]
    fn filter_error_display() {
        assert!(
            FilterError::RegionNotFound("r".into())
                .to_string()
                .contains("region not found")
        );
        assert!(
            FilterError::TaintNotEnabled("r".into())
                .to_string()
                .contains("Taint tracking not enabled")
        );
        assert!(FilterError::FilterDisabled.to_string().contains("disabled"));
        assert!(
            FilterError::FreeformNotEnabled
                .to_string()
                .contains("Freeform")
        );
    }

    // ─── DegradationEngine ──────────────────────────────────────────────────

    #[test]
    fn degradation_full_path() {
        let config = SecurityConfig {
            pointer_mode: true,
            filter_mode: Some(leviath_core::FilterMode::Structured),
            degradation: vec![
                InputMode::Pointer,
                InputMode::Filter,
                InputMode::Traditional,
            ],
            ..SecurityConfig::default()
        };

        let (next, msg) = DegradationEngine::degrade(&config, &InputMode::Pointer).unwrap();
        assert_eq!(next, InputMode::Filter);
        assert!(msg.contains("Degrading to 'filter'"));

        let (next2, _) = DegradationEngine::degrade(&config, &InputMode::Filter).unwrap();
        assert_eq!(next2, InputMode::Traditional);

        let err = DegradationEngine::degrade(&config, &InputMode::Traditional).unwrap_err();
        assert_eq!(
            err,
            DegradationError::NoFallback {
                current_mode: InputMode::Traditional
            }
        );
    }

    #[test]
    fn degradation_skip_filter() {
        let config = SecurityConfig {
            pointer_mode: true,
            degradation: vec![InputMode::Pointer, InputMode::Traditional],
            ..SecurityConfig::default()
        };

        let (next, _) = DegradationEngine::degrade(&config, &InputMode::Pointer).unwrap();
        assert_eq!(next, InputMode::Traditional);
    }

    #[test]
    fn degradation_strict_single_mode() {
        let config = SecurityConfig {
            pointer_mode: true,
            degradation: vec![InputMode::Pointer],
            ..SecurityConfig::default()
        };

        let err = DegradationEngine::degrade(&config, &InputMode::Pointer).unwrap_err();
        assert_eq!(
            err,
            DegradationError::NoFallback {
                current_mode: InputMode::Pointer
            }
        );
    }

    #[test]
    fn degradation_first_available() {
        let config = SecurityConfig {
            pointer_mode: true,
            filter_mode: Some(leviath_core::FilterMode::Structured),
            degradation: vec![
                InputMode::Pointer,
                InputMode::Filter,
                InputMode::Traditional,
            ],
            ..SecurityConfig::default()
        };
        assert_eq!(
            DegradationEngine::first_available(&config),
            Some(InputMode::Pointer)
        );

        let config2 = SecurityConfig {
            degradation: vec![InputMode::Pointer, InputMode::Traditional],
            ..SecurityConfig::default()
        };
        // pointer_mode is false, so first available is Traditional
        assert_eq!(
            DegradationEngine::first_available(&config2),
            Some(InputMode::Traditional)
        );
    }

    #[test]
    fn degradation_validate_path() {
        let config = SecurityConfig {
            pointer_mode: false,
            filter_mode: None,
            degradation: vec![
                InputMode::Pointer,
                InputMode::Filter,
                InputMode::Traditional,
            ],
            ..SecurityConfig::default()
        };
        let unavailable = DegradationEngine::validate_path(&config);
        assert!(unavailable.contains(&InputMode::Pointer));
        assert!(unavailable.contains(&InputMode::Filter));
        assert!(!unavailable.contains(&InputMode::Traditional));
    }

    #[test]
    fn degradation_error_display() {
        let err = DegradationError::NoFallback {
            current_mode: InputMode::Pointer,
        };
        assert!(err.to_string().contains("no fallback"));

        let err = DegradationError::AllModesExhausted;
        assert!(err.to_string().contains("exhausted"));

        let err = DegradationError::ModeError {
            mode: InputMode::Filter,
            message: "sub-agent failed".into(),
        };
        assert!(err.to_string().contains("sub-agent failed"));
    }

    // ─── Policy-aware gate check ────────────────────────────────────────────

    #[test]
    fn gate_with_policy_allows_via_allowlist() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let window = make_window_with_taint(TaintLevel::Private);
        let policy = leviath_core::PolicyConfig {
            allowlist: vec![leviath_core::AllowlistRule {
                tool: "shell".into(),
                to: vec![],
                channel: vec![],
                max_sensitivity: TaintLevel::Private,
            }],
            mcp_overrides: Default::default(),
        };

        let decision = gate.check_with_policy("agent-1", "shell", &window, None, &policy, None);
        assert!(decision.is_allowed());

        // Should have logged an allowlist allow
        let last = gate.audit_log().last().unwrap();
        assert!(last.allowed);
        // The single allowlist rule (index 0) matched.
        assert_eq!(
            last.decision_source,
            GateDecisionSource::AllowlistRule { rule_index: 0 }
        );
    }

    #[test]
    fn gate_with_policy_allows_via_scripted_rule() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let window = make_window_with_taint(TaintLevel::Private);
        let policy = leviath_core::PolicyConfig::default(); // empty allowlist

        let checker = |tool: &str, _target: Option<&str>, _taint: TaintLevel| -> Option<String> {
            (tool == "shell").then(|| "company_rule.rhai".to_string())
        };

        let decision =
            gate.check_with_policy("agent-1", "shell", &window, None, &policy, Some(&checker));
        assert!(decision.is_allowed());

        let last = gate.audit_log().last().unwrap();
        assert_eq!(
            last.decision_source,
            GateDecisionSource::ScriptedRule {
                script_name: "company_rule.rhai".to_string()
            }
        );
    }

    #[test]
    fn gate_with_policy_blocks_when_no_rule_matches() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let window = make_window_with_taint(TaintLevel::Private);
        let policy = leviath_core::PolicyConfig::default();

        let decision = gate.check_with_policy("agent-1", "shell", &window, None, &policy, None);
        assert!(!decision.is_allowed());
    }

    #[test]
    fn gate_with_policy_passes_through_when_already_allowed() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let window = make_window_with_taint(TaintLevel::Public);
        let policy = leviath_core::PolicyConfig::default();

        let decision = gate.check_with_policy("agent-1", "shell", &window, None, &policy, None);
        assert!(decision.is_allowed());
    }

    #[test]
    fn gate_with_policy_target_pattern_matching() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        gate.set_tool_classification(
            "send_email".to_string(),
            ToolClassification::new(
                TaintLevel::Public,
                ToolDirection::Outbound,
                TaintLevel::Public,
            ),
        );
        let window = make_window_with_taint(TaintLevel::Private);
        let policy = leviath_core::PolicyConfig {
            allowlist: vec![leviath_core::AllowlistRule {
                tool: "send_email".into(),
                to: vec!["megan@*".into()],
                channel: vec![],
                max_sensitivity: TaintLevel::Private,
            }],
            mcp_overrides: Default::default(),
        };

        // Should match megan@ pattern
        let decision = gate.check_with_policy(
            "agent-1",
            "send_email",
            &window,
            Some("megan@work.com"),
            &policy,
            None,
        );
        assert!(decision.is_allowed());

        // Should not match bob@
        let decision2 = gate.check_with_policy(
            "agent-1",
            "send_email",
            &window,
            Some("bob@work.com"),
            &policy,
            None,
        );
        assert!(!decision2.is_allowed());
    }

    // ─── Additional PointerResolver edge-case tests ────────────────────────

    #[test]
    fn pointer_resolve_offset_range_start_equals_end() {
        let window = make_window_with_chunks();
        let pointer = leviath_core::taint::PointerRef::OffsetRange {
            region: "research".into(),
            start: 1,
            end: 1,
        };
        let err = PointerResolver::resolve(&window, &pointer).unwrap_err();
        assert_eq!(
            err,
            PointerError::OffsetOutOfBounds {
                region: "research".into(),
                start: 1,
                end: 1
            }
        );
    }

    #[test]
    fn pointer_resolve_offset_range_start_greater_than_end() {
        let window = make_window_with_chunks();
        let pointer = leviath_core::taint::PointerRef::OffsetRange {
            region: "research".into(),
            start: 1,
            end: 0,
        };
        let err = PointerResolver::resolve(&window, &pointer).unwrap_err();
        assert_eq!(
            err,
            PointerError::OffsetOutOfBounds {
                region: "research".into(),
                start: 1,
                end: 0
            }
        );
    }

    #[test]
    fn pointer_resolve_offset_range_end_exceeds_len() {
        let window = make_window_with_chunks();
        // Window has 2 entries, so end=3 is out of bounds
        let pointer = leviath_core::taint::PointerRef::OffsetRange {
            region: "research".into(),
            start: 0,
            end: 3,
        };
        let err = PointerResolver::resolve(&window, &pointer).unwrap_err();
        assert_eq!(
            err,
            PointerError::OffsetOutOfBounds {
                region: "research".into(),
                start: 0,
                end: 3
            }
        );
    }

    #[test]
    fn pointer_resolve_offset_region_not_found() {
        let window = make_window_with_chunks();
        let pointer = leviath_core::taint::PointerRef::OffsetRange {
            region: "missing".into(),
            start: 0,
            end: 1,
        };
        let err = PointerResolver::resolve(&window, &pointer).unwrap_err();
        assert_eq!(err, PointerError::RegionNotFound("missing".into()));
    }

    #[test]
    fn pointer_resolve_offset_range_taint_not_enabled() {
        let mut window = ContextWindow::new(10000);
        let mut region = Region::new("data".to_string(), RegionKind::Temporary, 5000);
        region.add_entry("entry1".to_string(), 10).unwrap();
        region.add_entry("entry2".to_string(), 10).unwrap();
        window.add_region(region);

        let pointer = leviath_core::taint::PointerRef::OffsetRange {
            region: "data".into(),
            start: 0,
            end: 1,
        };
        let err = PointerResolver::resolve(&window, &pointer).unwrap_err();
        assert_eq!(err, PointerError::TaintNotEnabled("data".into()));
    }

    // ─── Additional FilterResolver tests ───────────────────────────────────

    #[test]
    fn filter_resolve_with_private_taint() {
        let mut window = ContextWindow::new(10000);
        let mut region =
            Region::new("emails".to_string(), RegionKind::Temporary, 5000).with_taint_tracking();
        region
            .add_tainted_entry("email content".to_string(), 10, TaintLevel::Private)
            .unwrap();
        window.add_region(region);

        let config = SecurityConfig {
            filter_mode: Some(leviath_core::FilterMode::Structured),
            ..SecurityConfig::default()
        };
        let filter = leviath_core::taint::FilterInput {
            source_region: "emails".into(),
            operation: leviath_core::taint::FilterOperation::Extract {
                extract_type: "subject".into(),
            },
            output_format: Some("json".into()),
        };

        let result = FilterResolver::resolve(&window, &filter, &config).unwrap();
        assert!(result.source_content.contains("email content"));
        assert_eq!(result.taint_level, TaintLevel::Private);
        assert_eq!(result.operation.name(), "extract");
    }

    #[test]
    fn filter_resolve_with_mixed_taint_entries() {
        let mut window = ContextWindow::new(10000);
        let mut region =
            Region::new("mixed".to_string(), RegionKind::Temporary, 5000).with_taint_tracking();
        region
            .add_tainted_entry("public data".to_string(), 5, TaintLevel::Public)
            .unwrap();
        region
            .add_tainted_entry("internal data".to_string(), 5, TaintLevel::Internal)
            .unwrap();
        region
            .add_tainted_entry("private data".to_string(), 5, TaintLevel::Private)
            .unwrap();
        window.add_region(region);

        let config = SecurityConfig {
            filter_mode: Some(leviath_core::FilterMode::Structured),
            ..SecurityConfig::default()
        };
        let filter = leviath_core::taint::FilterInput {
            source_region: "mixed".into(),
            operation: leviath_core::taint::FilterOperation::Summarize,
            output_format: None,
        };

        let result = FilterResolver::resolve(&window, &filter, &config).unwrap();
        // Region taint should be max across entries
        assert_eq!(result.taint_level, TaintLevel::Private);
        assert!(result.source_content.contains("public data"));
        assert!(result.source_content.contains("private data"));
    }

    #[test]
    fn filter_resolve_with_freeform_mode() {
        let mut window = ContextWindow::new(10000);
        let mut region =
            Region::new("data".to_string(), RegionKind::Temporary, 5000).with_taint_tracking();
        region
            .add_tainted_entry("content".to_string(), 5, TaintLevel::Public)
            .unwrap();
        window.add_region(region);

        let config = SecurityConfig {
            filter_mode: Some(leviath_core::FilterMode::Freeform),
            ..SecurityConfig::default()
        };
        let filter = leviath_core::taint::FilterInput {
            source_region: "data".into(),
            operation: leviath_core::taint::FilterOperation::Custom {
                name: "my_filter".into(),
                params: Default::default(),
            },
            output_format: None,
        };

        let result = FilterResolver::resolve(&window, &filter, &config).unwrap();
        assert_eq!(result.taint_level, TaintLevel::Public);
        assert_eq!(result.operation.name(), "my_filter");
    }

    // ─── Additional DegradationEngine tests ────────────────────────────────

    #[test]
    fn degradation_empty_path() {
        let config = SecurityConfig {
            degradation: vec![],
            ..SecurityConfig::default()
        };
        let err = DegradationEngine::degrade(&config, &InputMode::Traditional).unwrap_err();
        assert_eq!(
            err,
            DegradationError::NoFallback {
                current_mode: InputMode::Traditional
            }
        );
    }

    #[test]
    fn degradation_first_available_empty_path() {
        let config = SecurityConfig {
            degradation: vec![],
            ..SecurityConfig::default()
        };
        assert_eq!(DegradationEngine::first_available(&config), None);
    }

    #[test]
    fn degradation_first_available_skips_unavailable() {
        let config = SecurityConfig {
            pointer_mode: false,
            filter_mode: None,
            degradation: vec![
                InputMode::Pointer,
                InputMode::Filter,
                InputMode::Traditional,
            ],
            ..SecurityConfig::default()
        };
        // Pointer and Filter are unavailable, so Traditional is first available
        assert_eq!(
            DegradationEngine::first_available(&config),
            Some(InputMode::Traditional)
        );
    }

    #[test]
    fn degradation_validate_path_all_available() {
        let config = SecurityConfig {
            pointer_mode: true,
            filter_mode: Some(leviath_core::FilterMode::Structured),
            degradation: vec![
                InputMode::Pointer,
                InputMode::Filter,
                InputMode::Traditional,
            ],
            ..SecurityConfig::default()
        };
        let unavailable = DegradationEngine::validate_path(&config);
        assert!(unavailable.is_empty());
    }

    #[test]
    fn degradation_validate_path_empty() {
        let config = SecurityConfig {
            degradation: vec![],
            ..SecurityConfig::default()
        };
        assert!(DegradationEngine::validate_path(&config).is_empty());
    }

    // ─── Additional TaintGate tests ────────────────────────────────────────

    #[test]
    fn gate_check_traditional_with_internal_taint() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let window = make_window_with_taint(TaintLevel::Internal);
        let decision = gate.check_traditional("agent-1", "shell", &window);
        // Internal > Public clearance for shell, so should be blocked
        assert!(!decision.is_allowed());
        assert_eq!(
            decision,
            GateDecision::Blocked {
                taint_level: TaintLevel::Internal,
                clearance: TaintLevel::Public,
                source_regions: vec!["conv".to_string()],
                tool_name: "shell".to_string(),
            }
        );
    }

    #[test]
    fn gate_pointer_mode_blocks_with_internal_taint() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let decision = gate.check_pointer("agent-1", "shell", TaintLevel::Private);
        assert!(!decision.is_allowed());
        // Pointer mode reports empty source regions.
        assert_eq!(
            decision,
            GateDecision::Blocked {
                taint_level: TaintLevel::Private,
                clearance: TaintLevel::Public,
                source_regions: vec![],
                tool_name: "shell".to_string(),
            }
        );
    }

    #[test]
    fn gate_filter_mode_non_outbound_allows() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let decision =
            gate.check_filter("agent-1", "read_file", TaintLevel::Private, "conversation");
        assert!(decision.is_allowed());
    }

    #[test]
    fn gate_audit_log_records_blocked_events() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let window = make_window_with_taint(TaintLevel::Private);

        gate.check_traditional("agent-1", "shell", &window);
        assert_eq!(gate.audit_log().len(), 1);
        assert!(!gate.audit_log()[0].allowed);
        assert_eq!(gate.audit_log()[0].tool_name, "shell");
        assert_eq!(gate.audit_log()[0].taint_level, TaintLevel::Private);
        assert_eq!(gate.audit_log()[0].input_mode, InputMode::Traditional);
    }

    #[test]
    fn gate_check_filter_blocked_includes_source_region_name() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let decision = gate.check_filter("agent-1", "shell", TaintLevel::Internal, "emails");
        assert!(!decision.is_allowed());
        assert_eq!(
            decision,
            GateDecision::Blocked {
                taint_level: TaintLevel::Internal,
                clearance: TaintLevel::Public,
                source_regions: vec!["emails".to_string()],
                tool_name: "shell".to_string(),
            }
        );
    }

    #[test]
    fn gate_with_policy_scripted_rule_non_matching_tool() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let window = make_window_with_taint(TaintLevel::Private);
        let policy = leviath_core::PolicyConfig::default();

        let checker = |_tool: &str, _target: Option<&str>, _taint: TaintLevel| -> Option<String> {
            None // never matches
        };

        let decision =
            gate.check_with_policy("agent-1", "shell", &window, None, &policy, Some(&checker));
        assert!(!decision.is_allowed());
    }

    #[test]
    fn gate_with_policy_non_outbound_skips_policy_check() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let window = make_window_with_taint(TaintLevel::Private);
        let policy = leviath_core::PolicyConfig::default();

        // read_file is non-outbound, so policy check is never reached
        let decision = gate.check_with_policy("agent-1", "read_file", &window, None, &policy, None);
        assert!(decision.is_allowed());
    }

    #[test]
    fn gate_multiple_tool_overrides() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        gate.set_tool_classification(
            "tool_a".to_string(),
            ToolClassification::new(
                TaintLevel::Public,
                ToolDirection::Outbound,
                TaintLevel::Internal,
            ),
        );
        gate.set_tool_classification(
            "tool_b".to_string(),
            ToolClassification::new(
                TaintLevel::Public,
                ToolDirection::Outbound,
                TaintLevel::Private,
            ),
        );

        let window = make_window_with_taint(TaintLevel::Private);

        // tool_a has Internal clearance — should be blocked by Private taint
        let decision_a = gate.check_traditional("agent-1", "tool_a", &window);
        assert!(!decision_a.is_allowed());

        // tool_b has Private clearance — should be allowed
        let decision_b = gate.check_traditional("agent-1", "tool_b", &window);
        assert!(decision_b.is_allowed());
    }

    // ─── apply_resolution (synchronous resolution handling) ─────────────────

    #[test]
    fn apply_resolution_allow_once_records_and_executes() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let out = gate.apply_resolution(
            "a",
            "shell",
            "call1",
            TaintLevel::Private,
            TaintLevel::Public,
            GateResolution::AllowOnce,
        );
        assert!(out.is_none()); // execute
        let allow = gate
            .audit_log()
            .iter()
            .find(|e| e.allowed)
            .expect("an allowed event should be logged");
        assert_eq!(allow.decision_source, GateDecisionSource::UserAllowOnce);
    }

    #[test]
    fn apply_resolution_always_allow_raises_clearance() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let out = gate.apply_resolution(
            "a",
            "shell",
            "call1",
            TaintLevel::Private,
            TaintLevel::Public,
            GateResolution::AlwaysAllow,
        );
        assert!(out.is_none());
        // Clearance raised so future calls of this tool auto-pass.
        assert_eq!(
            gate.tool_classification("shell").clearance,
            TaintLevel::Private
        );
        let allow = gate
            .audit_log()
            .iter()
            .find(|e| e.allowed)
            .expect("an allowed event should be logged");
        assert_eq!(allow.decision_source, GateDecisionSource::UserAlwaysAllow);
    }

    #[test]
    fn apply_resolution_deny_returns_blocked_result() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let out = gate.apply_resolution(
            "a",
            "shell",
            "call1",
            TaintLevel::Private,
            TaintLevel::Public,
            GateResolution::Deny,
        );
        let (id, msg) = out.expect("deny yields a blocked result");
        assert_eq!(id, "call1");
        assert!(msg.contains("[blocked]") && msg.contains("shell"));
        let deny = gate
            .audit_log()
            .iter()
            .find(|e| !e.allowed)
            .expect("a denied event should be logged");
        assert_eq!(deny.decision_source, GateDecisionSource::UserDenied);
    }
}

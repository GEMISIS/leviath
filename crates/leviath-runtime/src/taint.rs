//! Taint gate checking for tool execution.
//!
//! Implements the gate check logic that runs before outbound tool calls.
//! When taint tracking is enabled, the gate compares the relevant taint
//! level (determined by input mode) against the tool's clearance level.

use leviath_core::taint::{
    builtin_tool_classification, GateDecision, GateDecisionSource, GateEvent, InputMode,
    SecurityConfig, TaintLevel, ToolClassification,
};
use std::collections::HashMap;

use crate::components::ContextWindow;

/// Taint gate — checks whether a tool invocation is allowed given the
/// current taint state of the context window.
#[derive(Debug, Clone)]
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
        if let GateDecision::Blocked {
            taint_level,
            clearance,
            tool_name,
            ..
        } = &decision
        {
            assert_eq!(*taint_level, TaintLevel::Private);
            assert_eq!(*clearance, TaintLevel::Public);
            assert_eq!(tool_name, "shell");
        } else {
            panic!("Expected Blocked");
        }
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
        if let GateDecision::Blocked { source_regions, .. } = &decision {
            assert!(source_regions.contains(&"dirty".to_string()));
            assert!(!source_regions.contains(&"clean".to_string()));
        } else {
            panic!("Expected Blocked");
        }
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
        if let GateDecision::Blocked { source_regions, .. } = &decision {
            assert!(source_regions.contains(&"conversation".to_string()));
        }
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
}

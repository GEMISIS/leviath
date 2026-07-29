//! Taint gate checking for tool execution.
//!
//! Implements the gate check logic that runs before outbound tool calls.
//! When taint tracking is enabled, the gate compares the context window's
//! overall taint level against the tool's clearance level.

use leviath_core::taint::{
    GateDecision, GateDecisionSource, GateEvent, SecurityConfig, TaintLevel, ToolClassification,
    builtin_tool_classification,
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
/// `Send + Sync` so a boxed checker can live in a shared-world resource.
pub type ScriptRuleChecker = dyn Fn(&str, Option<&str>, TaintLevel) -> Option<String> + Send + Sync;

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
            },
            tool_overrides: HashMap::new(),
            audit_log: Vec::new(),
        }
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

    /// Apply the `[mcp_overrides]` section of policy.toml to this gate.
    ///
    /// Each override starts from the tool's current classification and
    /// replaces only the fields it sets, keyed the same `server.tool` way MCP
    /// tools are named at dispatch. An unrecognized `direction` string keeps
    /// the existing direction and warns, rather than silently reclassifying
    /// a security property.
    ///
    /// Called at gate construction. A later session-scoped "always allow"
    /// writes the same map through [`Self::set_tool_classification`], so the
    /// user's runtime decision still wins over the config file.
    pub fn apply_mcp_overrides(
        &mut self,
        overrides: &HashMap<String, leviath_core::policy::McpToolOverride>,
    ) {
        for (tool_name, over) in overrides {
            let mut classification = self.tool_classification(tool_name);
            if let Some(sensitivity) = over.sensitivity {
                classification.sensitivity = sensitivity;
            }
            if let Some(clearance) = over.clearance {
                classification.clearance = clearance;
            }
            if let Some(direction) = over.direction.as_deref() {
                match leviath_core::taint::ToolDirection::from_str_loose(direction) {
                    Some(parsed) => classification.direction = parsed,
                    None => tracing::warn!(
                        tool = %tool_name,
                        direction = %direction,
                        "ignoring unrecognized direction in [mcp_overrides]"
                    ),
                }
            }
            self.set_tool_classification(tool_name.clone(), classification);
        }
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
                taint,
                classification.clearance,
                false,
                GateDecisionSource::AutoBlock,
            );

            GateDecision::Blocked {
                taint_level: taint,
                clearance: classification.clearance,
                source_regions,
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
        taint: TaintLevel,
        clearance: TaintLevel,
        source: GateDecisionSource,
    ) {
        self.log_event(agent_id, tool_name, taint, clearance, true, source);
    }

    /// Record a deny decision (the user or a default policy denied the call).
    pub fn record_deny(
        &mut self,
        agent_id: &str,
        tool_name: &str,
        taint: TaintLevel,
        clearance: TaintLevel,
        source: GateDecisionSource,
    ) {
        self.log_event(agent_id, tool_name, taint, clearance, false, source);
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

    #[allow(clippy::too_many_arguments)]
    fn log_event(
        &mut self,
        agent_id: &str,
        tool_name: &str,
        taint_level: TaintLevel,
        clearance: TaintLevel,
        allowed: bool,
        decision_source: GateDecisionSource,
    ) {
        self.audit_log.push(GateEvent {
            timestamp: chrono::Utc::now().timestamp(),
            agent_id: agent_id.to_string(),
            tool_name: tool_name.to_string(),
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
        assert!(!gate.config().taint_tracking);

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
    fn apply_mcp_overrides_replaces_only_the_set_fields() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let before = gate.tool_classification("srv.notify");
        let overrides = std::collections::HashMap::from([(
            "srv.notify".to_string(),
            leviath_core::policy::McpToolOverride {
                sensitivity: Some(TaintLevel::Private),
                direction: None,
                clearance: None,
            },
        )]);
        gate.apply_mcp_overrides(&overrides);
        let after = gate.tool_classification("srv.notify");
        assert_eq!(after.sensitivity, TaintLevel::Private);
        assert_eq!(after.direction, before.direction);
        assert_eq!(after.clearance, before.clearance);
    }

    #[test]
    fn apply_mcp_overrides_parses_direction_and_clearance() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let overrides = std::collections::HashMap::from([(
            "srv.post".to_string(),
            leviath_core::policy::McpToolOverride {
                sensitivity: None,
                direction: Some("outbound".to_string()),
                clearance: Some(TaintLevel::Internal),
            },
        )]);
        gate.apply_mcp_overrides(&overrides);
        let after = gate.tool_classification("srv.post");
        assert_eq!(after.direction, ToolDirection::Outbound);
        assert_eq!(after.clearance, TaintLevel::Internal);
    }

    #[test]
    fn apply_mcp_overrides_keeps_direction_on_unrecognized_string() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let before = gate.tool_classification("srv.odd");
        let overrides = std::collections::HashMap::from([(
            "srv.odd".to_string(),
            leviath_core::policy::McpToolOverride {
                sensitivity: None,
                direction: Some("sideways".to_string()),
                clearance: None,
            },
        )]);
        gate.apply_mcp_overrides(&overrides);
        // A typo must not silently reclassify a security property.
        assert_eq!(
            gate.tool_classification("srv.odd").direction,
            before.direction
        );
    }

    #[test]
    fn session_approval_still_wins_over_an_mcp_override() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let overrides = std::collections::HashMap::from([(
            "srv.send".to_string(),
            leviath_core::policy::McpToolOverride {
                sensitivity: None,
                direction: Some("outbound".to_string()),
                clearance: Some(TaintLevel::Public),
            },
        )]);
        gate.apply_mcp_overrides(&overrides);
        // The user's later "always allow" writes the same map and replaces
        // the config-file entry.
        gate.set_tool_classification(
            "srv.send".to_string(),
            ToolClassification::new(
                TaintLevel::Public,
                ToolDirection::Outbound,
                TaintLevel::Private,
            ),
        );
        assert_eq!(
            gate.tool_classification("srv.send").clearance,
            TaintLevel::Private
        );
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
    fn gate_record_allow() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        gate.record_allow(
            "agent-1",
            "shell",
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
        };
        let gate = TaintGate::new(config.clone());
        assert!(gate.config().taint_tracking);
        assert!(gate.config().taint_tracking);
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
    fn gate_audit_log_records_blocked_events() {
        let mut gate = TaintGate::new(SecurityConfig::default());
        let window = make_window_with_taint(TaintLevel::Private);

        gate.check_traditional("agent-1", "shell", &window);
        assert_eq!(gate.audit_log().len(), 1);
        assert!(!gate.audit_log()[0].allowed);
        assert_eq!(gate.audit_log()[0].tool_name, "shell");
        assert_eq!(gate.audit_log()[0].taint_level, TaintLevel::Private);
        // A clearance block is an automatic decision, not a user denial: the
        // user's choice (if any) is logged later by `apply_resolution`.
        assert_eq!(
            gate.audit_log()[0].decision_source,
            GateDecisionSource::AutoBlock,
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

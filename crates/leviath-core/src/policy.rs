//! Policy rules for taint tracking allowlists.
//!
//! Users configure allowlist rules in `~/.config/leviath/policy.toml` to relax
//! taint gating restrictions. Rules can be static (TOML pattern matching) or
//! scripted (Rhai).

use crate::taint::TaintLevel;
use serde::{Deserialize, Serialize};

/// A static allowlist rule from the policy file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AllowlistRule {
    /// Tool name this rule applies to.
    pub tool: String,
    /// Target patterns (e.g., email addresses, Slack channels).
    /// If empty, matches any target.
    #[serde(default)]
    pub to: Vec<String>,
    /// Channel patterns (for tools like Slack).
    #[serde(default)]
    pub channel: Vec<String>,
    /// Maximum sensitivity level allowed by this rule.
    pub max_sensitivity: TaintLevel,
}

impl AllowlistRule {
    /// Check if this rule matches a given tool invocation.
    pub fn matches(&self, tool_name: &str, target: Option<&str>, taint: TaintLevel) -> bool {
        if self.tool != tool_name {
            return false;
        }

        if taint > self.max_sensitivity {
            return false;
        }

        // If no patterns specified, match any target
        if self.to.is_empty() && self.channel.is_empty() {
            return true;
        }

        // Check target against 'to' patterns
        if let Some(target_str) = target {
            if self.to.iter().any(|p| pattern_matches(p, target_str)) {
                return true;
            }
            if self.channel.iter().any(|p| pattern_matches(p, target_str)) {
                return true;
            }
        }

        // If patterns are specified but no target provided, no match
        if target.is_none() && (!self.to.is_empty() || !self.channel.is_empty()) {
            return false;
        }

        false
    }
}

/// Simple glob-like pattern matching: supports `*` as wildcard prefix/suffix.
fn pattern_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return value.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return value.starts_with(prefix);
    }
    pattern == value
}

/// A scripted rule reference (evaluated by the scripting engine).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScriptedRule {
    /// Name of the rule (typically the filename without .rhai extension).
    pub name: String,
    /// Path to the Rhai script file.
    pub path: String,
}

/// MCP tool classification override.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpToolOverride {
    /// Tool sensitivity level.
    #[serde(default)]
    pub sensitivity: Option<TaintLevel>,
    /// Tool direction.
    #[serde(default)]
    pub direction: Option<String>,
    /// Tool clearance level.
    #[serde(default)]
    pub clearance: Option<TaintLevel>,
}

/// Complete policy configuration loaded from policy.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolicyConfig {
    /// Static allowlist rules.
    #[serde(default)]
    pub allowlist: Vec<AllowlistRule>,
    /// MCP tool overrides keyed by "server_name.tool_name".
    #[serde(default)]
    pub mcp_overrides: std::collections::HashMap<String, McpToolOverride>,
}

impl PolicyConfig {
    /// Parse a policy config from TOML string.
    pub fn from_toml(content: &str) -> Result<Self, String> {
        // Parse the raw TOML
        let parsed: toml::Value =
            toml::from_str(content).map_err(|e| format!("Failed to parse policy.toml: {}", e))?;

        let mut config = PolicyConfig::default();

        // Parse [[allowlist]] array
        if let Some(allowlist_arr) = parsed.get("allowlist").and_then(|v| v.as_array()) {
            for rule_val in allowlist_arr {
                let tool = rule_val
                    .get("tool")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                let to: Vec<String> = rule_val
                    .get("to")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();

                let channel: Vec<String> = rule_val
                    .get("channel")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();

                let max_sensitivity = rule_val
                    .get("max_sensitivity")
                    .and_then(|v| v.as_str())
                    .and_then(TaintLevel::from_str_loose)
                    .unwrap_or(TaintLevel::Public);

                config.allowlist.push(AllowlistRule {
                    tool,
                    to,
                    channel,
                    max_sensitivity,
                });
            }
        }

        // Parse [mcp_overrides] section
        if let Some(overrides_table) = parsed.get("mcp_overrides").and_then(|v| v.as_table()) {
            for (server_name, server_val) in overrides_table {
                if let Some(tools_table) = server_val.get("tools").and_then(|v| v.as_table()) {
                    for (tool_name, tool_val) in tools_table {
                        let key = format!("{}.{}", server_name, tool_name);
                        let sensitivity = tool_val
                            .get("sensitivity")
                            .and_then(|v| v.as_str())
                            .and_then(TaintLevel::from_str_loose);
                        let direction = tool_val
                            .get("direction")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        let clearance = tool_val
                            .get("clearance")
                            .and_then(|v| v.as_str())
                            .and_then(TaintLevel::from_str_loose);

                        config.mcp_overrides.insert(
                            key,
                            McpToolOverride {
                                sensitivity,
                                direction,
                                clearance,
                            },
                        );
                    }
                }
            }
        }

        Ok(config)
    }

    /// Check whether any allowlist rule matches the given invocation.
    /// Returns the index of the matching rule, if any.
    pub fn check_allowlist(
        &self,
        tool_name: &str,
        target: Option<&str>,
        taint: TaintLevel,
    ) -> Option<usize> {
        self.allowlist
            .iter()
            .position(|rule| rule.matches(tool_name, target, taint))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── pattern_matches ────────────────────────────────────────────────────

    #[test]
    fn pattern_matches_exact() {
        assert!(pattern_matches("hello", "hello"));
        assert!(!pattern_matches("hello", "world"));
    }

    #[test]
    fn pattern_matches_wildcard_all() {
        assert!(pattern_matches("*", "anything"));
        assert!(pattern_matches("*", ""));
    }

    #[test]
    fn pattern_matches_wildcard_prefix() {
        assert!(pattern_matches("*@example.com", "user@example.com"));
        assert!(!pattern_matches("*@example.com", "user@other.com"));
    }

    #[test]
    fn pattern_matches_wildcard_suffix() {
        assert!(pattern_matches("megan@*", "megan@anywhere.com"));
        assert!(!pattern_matches("megan@*", "bob@anywhere.com"));
    }

    // ─── AllowlistRule::matches ──────────────────────────────────────────────

    #[test]
    fn rule_matches_tool_and_sensitivity() {
        let rule = AllowlistRule {
            tool: "send_email".into(),
            to: vec![],
            channel: vec![],
            max_sensitivity: TaintLevel::Private,
        };
        assert!(rule.matches("send_email", None, TaintLevel::Private));
        assert!(rule.matches("send_email", None, TaintLevel::Public));
        assert!(!rule.matches("other_tool", None, TaintLevel::Private));
    }

    #[test]
    fn rule_blocks_above_max_sensitivity() {
        let rule = AllowlistRule {
            tool: "send_email".into(),
            to: vec![],
            channel: vec![],
            max_sensitivity: TaintLevel::Internal,
        };
        assert!(!rule.matches("send_email", None, TaintLevel::Private));
    }

    #[test]
    fn rule_matches_target_pattern() {
        let rule = AllowlistRule {
            tool: "send_email".into(),
            to: vec!["megan@*".into(), "+17576306267".into()],
            channel: vec![],
            max_sensitivity: TaintLevel::Private,
        };
        assert!(rule.matches("send_email", Some("megan@work.com"), TaintLevel::Internal));
        assert!(rule.matches("send_email", Some("+17576306267"), TaintLevel::Internal));
        assert!(!rule.matches("send_email", Some("bob@work.com"), TaintLevel::Internal));
    }

    #[test]
    fn rule_matches_channel_pattern() {
        let rule = AllowlistRule {
            tool: "post_to_slack".into(),
            to: vec![],
            channel: vec!["#team-standup".into()],
            max_sensitivity: TaintLevel::Internal,
        };
        assert!(rule.matches("post_to_slack", Some("#team-standup"), TaintLevel::Internal));
        assert!(!rule.matches("post_to_slack", Some("#general"), TaintLevel::Internal));
    }

    #[test]
    fn rule_no_match_when_patterns_but_no_target() {
        let rule = AllowlistRule {
            tool: "send_email".into(),
            to: vec!["megan@*".into()],
            channel: vec![],
            max_sensitivity: TaintLevel::Private,
        };
        assert!(!rule.matches("send_email", None, TaintLevel::Internal));
    }

    // ─── PolicyConfig::from_toml ────────────────────────────────────────────

    #[test]
    fn parse_policy_with_allowlist() {
        let toml = r##"
[[allowlist]]
tool = "send_email"
to = ["megan@*", "+17576306267"]
max_sensitivity = "private"

[[allowlist]]
tool = "post_to_slack"
channel = ["#team-standup"]
max_sensitivity = "internal"
"##;
        let config = PolicyConfig::from_toml(toml).unwrap();
        assert_eq!(config.allowlist.len(), 2);
        assert_eq!(config.allowlist[0].tool, "send_email");
        assert_eq!(config.allowlist[0].to.len(), 2);
        assert_eq!(config.allowlist[0].max_sensitivity, TaintLevel::Private);
        assert_eq!(config.allowlist[1].tool, "post_to_slack");
        assert_eq!(config.allowlist[1].channel, vec!["#team-standup"]);
    }

    #[test]
    fn parse_policy_with_mcp_overrides() {
        let toml = r#"
[mcp_overrides."my-server".tools]
read_customer_data = { sensitivity = "private" }
search_public_docs = { sensitivity = "public" }
"#;
        let config = PolicyConfig::from_toml(toml).unwrap();
        assert_eq!(config.mcp_overrides.len(), 2);
        let cust = config
            .mcp_overrides
            .get("my-server.read_customer_data")
            .unwrap();
        assert_eq!(cust.sensitivity, Some(TaintLevel::Private));
        let docs = config
            .mcp_overrides
            .get("my-server.search_public_docs")
            .unwrap();
        assert_eq!(docs.sensitivity, Some(TaintLevel::Public));
    }

    #[test]
    fn parse_policy_mcp_override_with_direction_and_clearance() {
        // Exercises the direction/clearance branches of `[mcp_overrides]`
        // parsing, which a sensitivity-only override never reaches.
        let toml = r#"
[mcp_overrides."srv".tools]
send_email = { sensitivity = "private", direction = "egress", clearance = "public" }
"#;
        let config = PolicyConfig::from_toml(toml).unwrap();
        let ov = config.mcp_overrides.get("srv.send_email").unwrap();
        assert_eq!(ov.sensitivity, Some(TaintLevel::Private));
        assert_eq!(ov.direction.as_deref(), Some("egress"));
        assert_eq!(ov.clearance, Some(TaintLevel::Public));
    }

    #[test]
    fn parse_policy_empty() {
        let config = PolicyConfig::from_toml("").unwrap();
        assert!(config.allowlist.is_empty());
        assert!(config.mcp_overrides.is_empty());
    }

    #[test]
    fn parse_policy_invalid_toml() {
        let result = PolicyConfig::from_toml("{{invalid}}");
        assert!(result.is_err());
    }

    #[test]
    fn check_allowlist_returns_matching_index() {
        let config = PolicyConfig {
            allowlist: vec![
                AllowlistRule {
                    tool: "send_email".into(),
                    to: vec!["megan@*".into()],
                    channel: vec![],
                    max_sensitivity: TaintLevel::Private,
                },
                AllowlistRule {
                    tool: "post_to_slack".into(),
                    to: vec![],
                    channel: vec![],
                    max_sensitivity: TaintLevel::Internal,
                },
            ],
            mcp_overrides: Default::default(),
        };

        assert_eq!(
            config.check_allowlist("send_email", Some("megan@work.com"), TaintLevel::Internal),
            Some(0)
        );
        assert_eq!(
            config.check_allowlist("post_to_slack", None, TaintLevel::Internal),
            Some(1)
        );
        assert_eq!(
            config.check_allowlist("unknown", None, TaintLevel::Public),
            None
        );
    }

    // ─── Serde roundtrips ───────────────────────────────────────────────────

    #[test]
    fn allowlist_rule_serde_roundtrip() {
        let rule = AllowlistRule {
            tool: "send_email".into(),
            to: vec!["test@*".into()],
            channel: vec![],
            max_sensitivity: TaintLevel::Private,
        };
        let json = serde_json::to_string(&rule).unwrap();
        let back: AllowlistRule = serde_json::from_str(&json).unwrap();
        assert_eq!(rule, back);
    }

    #[test]
    fn scripted_rule_serde_roundtrip() {
        let rule = ScriptedRule {
            name: "company_email".into(),
            path: "~/.config/leviath/rules/company_email.rhai".into(),
        };
        let json = serde_json::to_string(&rule).unwrap();
        let back: ScriptedRule = serde_json::from_str(&json).unwrap();
        assert_eq!(rule, back);
    }

    #[test]
    fn mcp_override_serde_roundtrip() {
        let o = McpToolOverride {
            sensitivity: Some(TaintLevel::Private),
            direction: Some("outbound".into()),
            clearance: Some(TaintLevel::Internal),
        };
        let json = serde_json::to_string(&o).unwrap();
        let back: McpToolOverride = serde_json::from_str(&json).unwrap();
        assert_eq!(o, back);
    }
}

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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PolicyConfig {
    /// Static allowlist rules.
    #[serde(default)]
    pub allowlist: Vec<AllowlistRule>,
    /// MCP tool overrides, keyed by the name the tool is dispatched under.
    ///
    /// That is the advertised name, `<server>__<tool>` sanitized by
    /// [`crate::mcp_names::advertised_name`], because the gate looks this map
    /// up with whatever name the model called. Any other spelling is a key
    /// that matches nothing.
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

        // Parse [mcp_overrides] section.
        //
        // Two shapes, both keyed in memory by the name the tool is *dispatched*
        // under, because that is the only string the gate ever looks up:
        //
        //   [mcp_overrides.<server>.tools.<tool>]   the nested form, written by
        //                                           hand; the halves are
        //                                           separate TOML keys, so
        //                                           `my.tools` needs no escaping
        //   [mcp_overrides.<server>__<tool>]        the flat form, which is what
        //                                           `lev policy add` serializes
        //
        // The nested form used to build a `<server>.<tool>` key, which matches
        // no dispatched tool, so every override written in it was stored and
        // never read. The flat form was not parsed at all, so `lev policy add`
        // wrote a file this function could not read back.
        if let Some(overrides_table) = parsed.get("mcp_overrides").and_then(|v| v.as_table()) {
            for (entry_name, entry_val) in overrides_table {
                match entry_val.get("tools").and_then(|v| v.as_table()) {
                    Some(tools_table) => {
                        for (tool_name, tool_val) in tools_table {
                            let key = crate::mcp_names::advertised_name(entry_name, tool_name);
                            config
                                .mcp_overrides
                                .insert(key, Self::read_override(tool_val));
                        }
                    }
                    // No `tools` sub-table: either the flat form, whose name is
                    // already a dispatched name, or an entry that classifies
                    // nothing and is left alone.
                    None => {
                        if Self::classifies_something(entry_val) {
                            config
                                .mcp_overrides
                                .insert(entry_name.clone(), Self::read_override(entry_val));
                        }
                    }
                }
            }
        }

        Ok(config)
    }

    /// Whether an `[mcp_overrides]` entry sets any classification field.
    ///
    /// This is what separates the flat form from an entry that carries only a
    /// note or a typo. An entry that classifies nothing would override nothing,
    /// so storing it under a tool's name could only shadow a real rule.
    fn classifies_something(value: &toml::Value) -> bool {
        ["sensitivity", "direction", "clearance"]
            .iter()
            .any(|field| value.get(field).and_then(|v| v.as_str()).is_some())
    }

    /// Read one override's three optional fields.
    ///
    /// An unreadable level is left unset rather than defaulted: a `sensitivity`
    /// nobody can parse must not silently become `public`, which is the most
    /// permissive thing it could have meant.
    fn read_override(value: &toml::Value) -> McpToolOverride {
        McpToolOverride {
            sensitivity: value
                .get("sensitivity")
                .and_then(|v| v.as_str())
                .and_then(TaintLevel::from_str_loose),
            direction: value
                .get("direction")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            clearance: value
                .get("clearance")
                .and_then(|v| v.as_str())
                .and_then(TaintLevel::from_str_loose),
        }
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
            .get("my-server__read_customer_data")
            .unwrap();
        assert_eq!(cust.sensitivity, Some(TaintLevel::Private));
        let docs = config
            .mcp_overrides
            .get("my-server__search_public_docs")
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
        let ov = config.mcp_overrides.get("srv__send_email").unwrap();
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

    #[test]
    fn test_matches_false_when_only_channel_pattern_set_but_no_target() {
        let rule = AllowlistRule {
            tool: "post_message".to_string(),
            to: vec![],
            channel: vec!["#general".to_string()],
            max_sensitivity: TaintLevel::Private,
        };
        // `to` is empty (first operand false), which forces evaluation of the
        // `channel` operand in the "patterns set but no target" guard; with no
        // target the rule must not match.
        assert!(!rule.matches("post_message", None, TaintLevel::Public));
    }

    #[test]
    fn two_policies_compare_by_what_they_say() {
        // The daemon reloads this file and only swaps the gate's copy when the
        // contents differ, so equality has to mean "says the same thing"
        // rather than "came from the same bytes".
        let one = PolicyConfig::from_toml("[[allowlist]]\ntool = \"shell\"\n").unwrap();
        let same = PolicyConfig::from_toml("[[allowlist]]\ntool   =   \"shell\"\n").unwrap();
        let other = PolicyConfig::from_toml("[[allowlist]]\ntool = \"web_fetch\"\n").unwrap();
        assert_eq!(one, same);
        assert_ne!(one, other);
        assert_ne!(one, PolicyConfig::default());
    }

    #[test]
    fn test_from_toml_mcp_override_server_without_tools_table() {
        // A server entry with no `tools` sub-table and no classification field
        // is not the flat form either, so nothing is inserted.
        let toml = r#"
[mcp_overrides.emptyserver]
note = "no tools declared here"
"#;
        let config = PolicyConfig::from_toml(toml).unwrap();
        assert!(config.mcp_overrides.is_empty());
    }

    // ─── the key an override is stored under ──────────────────────────────
    //
    // An `[mcp_overrides]` entry sets a tool's sensitivity, direction and
    // clearance, which is what the taint gate consults before letting a
    // tainted outbound call through. The gate looks the map up by the name
    // the model called the tool, so a key in any other spelling is read,
    // stored, and then never matched. That failure is silent, and it fails
    // open: the tool keeps its default classification and the operator
    // believes they tightened it.
    //
    // The three tests below are the three ways that happened.

    /// The nested form built `<server>.<tool>`, which is not a name any tool
    /// is ever dispatched under.
    #[test]
    fn a_nested_override_is_keyed_by_the_name_the_tool_dispatches_under() {
        let toml = r#"
[mcp_overrides.tracker.tools.create_issue]
sensitivity = "internal"
direction = "outbound"
clearance = "internal"
"#;
        let config = PolicyConfig::from_toml(toml).unwrap();
        assert_eq!(
            config.mcp_overrides.keys().collect::<Vec<_>>(),
            vec!["tracker__create_issue"],
            "the key must be the advertised name, not a dotted one"
        );
        let over = &config.mcp_overrides["tracker__create_issue"];
        assert_eq!(over.sensitivity, Some(TaintLevel::Internal));
        assert_eq!(over.direction.as_deref(), Some("outbound"));
        assert_eq!(over.clearance, Some(TaintLevel::Internal));
    }

    /// The nested form is the one that can carry a server or tool whose own
    /// name has a dot in it, because the halves are separate TOML keys. Both
    /// are sanitized the same way the advertised name is.
    #[test]
    fn a_nested_override_sanitizes_a_dotted_server_and_tool() {
        let toml = r#"
[mcp_overrides."my.tools".tools."find.all"]
sensitivity = "private"
"#;
        let config = PolicyConfig::from_toml(toml).unwrap();
        let keys: Vec<&String> = config.mcp_overrides.keys().collect();
        assert!(
            config.mcp_overrides.contains_key("my_tools__find_all"),
            "keys: {keys:?}"
        );
    }

    /// `lev policy add` serializes the map straight back out, which produces
    /// the flat form. Reading it has to give back what was written, or a
    /// command that edits this file silently drops every override in it.
    #[test]
    fn a_policy_file_round_trips_through_serialization() {
        let mut config = PolicyConfig::default();
        config.mcp_overrides.insert(
            "tracker__create_issue".to_string(),
            McpToolOverride {
                sensitivity: Some(TaintLevel::Internal),
                direction: Some("outbound".to_string()),
                clearance: Some(TaintLevel::Internal),
            },
        );
        let written = toml::to_string_pretty(&config).expect("serializes");
        let read_back = PolicyConfig::from_toml(&written).expect("parses");
        assert_eq!(read_back, config, "written as:\n{written}");
    }

    /// An entry with no `tools` table but a classification field is the flat
    /// form, and its name is already a dispatched name.
    #[test]
    fn a_flat_override_keeps_its_name_verbatim() {
        let toml = r#"
[mcp_overrides.tracker__create_issue]
sensitivity = "private"
"#;
        let config = PolicyConfig::from_toml(toml).unwrap();
        assert_eq!(
            config.mcp_overrides["tracker__create_issue"].sensitivity,
            Some(TaintLevel::Private)
        );
    }

    /// A level nobody can parse stays unset. Defaulting it would pick
    /// `public`, the most permissive reading of a security field.
    #[test]
    fn an_unreadable_level_is_left_unset_rather_than_defaulted() {
        let toml = r#"
[mcp_overrides.tracker__create_issue]
sensitivity = "banana"
direction = "outbound"
"#;
        let config = PolicyConfig::from_toml(toml).unwrap();
        let over = &config.mcp_overrides["tracker__create_issue"];
        assert_eq!(over.sensitivity, None);
        assert_eq!(over.clearance, None);
        assert_eq!(over.direction.as_deref(), Some("outbound"));
    }
}

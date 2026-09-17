//! The one place that decides what an MCP tool is called.
//!
//! An MCP tool reaches a model under a name Leviath builds, not the name the
//! server gave it: `<server>__<tool>`, with every character a provider will not
//! accept rewritten. Three things need that exact string and none of them can
//! afford to guess it. The executor advertises it, the taint gate looks up a
//! `[mcp_overrides]` classification by it, and the dashboard's tool chooser
//! offers it.
//!
//! It lived in `leviath-mcp` while two of those three callers open-coded their
//! own copy, and `[mcp_overrides]` built a `<server>.<tool>` key instead. A key
//! in the wrong spelling matches no tool, so every override written against it
//! was read, stored and never used, which is the quietest way a security
//! control can fail. The rule lives here, below every caller, so there is one
//! answer rather than three that agree until one of them is edited.

/// Provider tool-name limit: the name advertised to the LLM must match
/// `^[A-Za-z0-9_-]{1,64}$` (the Anthropic/OpenAI rule). MCP names are laxer
/// (they allow dots), so any MCP name that violates this would make the
/// provider reject the *entire* request.
const MAX_TOOL_NAME_LEN: usize = 64;

/// Sanitize an MCP tool name into the provider-accepted character set.
///
/// Every character outside `[A-Za-z0-9_-]` (notably `.`, which MCP allows and
/// real servers use) becomes `_`, and the result is truncated to 64 bytes. An
/// empty result (a name of only illegal characters) falls back to `tool`.
pub fn sanitize_tool_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    out.truncate(MAX_TOOL_NAME_LEN);
    if out.is_empty() {
        "tool".to_string()
    } else {
        out
    }
}

/// The name a server's tool is advertised, granted and classified under.
///
/// Joined first and sanitized once, so the 64-byte truncation falls where it
/// really falls. Sanitizing each half and joining afterwards would keep a
/// 60-character server name whole and cut the tool off the end instead.
///
/// One caveat no caller can work around: when this name is already taken the
/// executor appends `_2`, and nothing outside the executor can predict that.
/// An override for a tool in a collision has to name the suffixed spelling.
pub fn advertised_name(server: &str, tool: &str) -> String {
    sanitize_tool_name(&format!("{server}__{tool}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_name_passes_through() {
        assert_eq!(sanitize_tool_name("create_issue"), "create_issue");
        assert_eq!(sanitize_tool_name("find-all"), "find-all");
    }

    #[test]
    fn dots_and_other_illegal_characters_become_underscores() {
        assert_eq!(sanitize_tool_name("my.tools"), "my_tools");
        assert_eq!(sanitize_tool_name("weird name!/#"), "weird_name___");
    }

    #[test]
    fn a_name_of_only_illegal_characters_falls_back() {
        assert_eq!(sanitize_tool_name("!!!"), "___");
        assert_eq!(sanitize_tool_name(""), "tool");
    }

    #[test]
    fn a_long_name_is_truncated_to_the_provider_limit() {
        let long = "a".repeat(100);
        assert_eq!(sanitize_tool_name(&long).len(), MAX_TOOL_NAME_LEN);
    }

    #[test]
    fn an_advertised_name_joins_with_two_underscores() {
        assert_eq!(
            advertised_name("tracker", "create_issue"),
            "tracker__create_issue"
        );
    }

    #[test]
    fn an_advertised_name_sanitizes_both_halves() {
        assert_eq!(
            advertised_name("my.tools", "find.all"),
            "my_tools__find_all"
        );
    }

    /// The join happens before the cut, so a long server name cannot swallow
    /// the whole budget and leave the tool nameless.
    #[test]
    fn an_advertised_name_is_truncated_after_joining() {
        let server = "s".repeat(60);
        let name = advertised_name(&server, "create_issue");
        assert_eq!(name.len(), MAX_TOOL_NAME_LEN);
        assert!(name.starts_with(&server), "the server name is kept whole");
        assert!(name.ends_with("__cr"), "the tool name is what gets cut");
    }
}

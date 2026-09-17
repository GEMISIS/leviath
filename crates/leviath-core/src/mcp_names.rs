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
//!
//! The name is now exactly `<server>__<tool>` and nothing ever renames it.
//! [`validate_server_name`] is what buys that: a server name has to be usable
//! in a tool name verbatim, so a `.` in one is refused rather than quietly
//! rewritten to `_`. Two names that differ only by that character used to
//! sanitize to the same string and the second server's tools were handed a
//! `_2` suffix, which nothing outside the executor could predict and which
//! depended on the order servers happened to connect in.

/// Provider tool-name limit: the name advertised to the LLM must match
/// `^[A-Za-z0-9_-]{1,64}$` (the Anthropic/OpenAI rule). MCP names are laxer
/// (they allow dots), so any MCP name that violates this would make the
/// provider reject the *entire* request.
const MAX_TOOL_NAME_LEN: usize = 64;

/// Whether a character may appear in a name sent to a provider.
fn is_provider_safe(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Check a name from `[[mcp_servers]]`, which a person chose and can change.
///
/// The server name goes into every one of that server's tool names unchanged,
/// so it has to be spelled the way a provider will accept. Rewriting it
/// instead would make two different servers share one prefix: `my.tools` and
/// `my_tools` both become `my_tools`, and `.` and `_` are not the same thing.
///
/// A tool name is not checked this way, because the server chose it and MCP
/// allows a dot. Those are sanitized by [`sanitize_tool_name`]; a collision
/// that survives is refused by name rather than renamed.
pub fn validate_server_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("an MCP server name cannot be empty".to_string());
    }
    let bad: Vec<String> = name
        .chars()
        .filter(|c| !is_provider_safe(*c))
        .map(|c| format!("{c:?}"))
        .collect();
    if !bad.is_empty() {
        return Err(format!(
            "MCP server name {name:?} contains {}, which a model provider will not accept in a \
             tool name. The server's tools are named <server>__<tool>, so the name is used as \
             you write it. Use letters, digits, `_` or `-`",
            bad.join(", ")
        ));
    }
    if name.len() >= MAX_TOOL_NAME_LEN {
        return Err(format!(
            "MCP server name {name:?} is {} characters, which leaves no room for a tool name \
             within the {MAX_TOOL_NAME_LEN}-character limit a model provider allows. Use a \
             shorter name",
            name.len()
        ));
    }
    Ok(())
}

/// Sanitize an MCP tool name into the provider-accepted character set.
///
/// Every character outside `[A-Za-z0-9_-]` (notably `.`, which MCP allows and
/// real servers use) becomes `_`, and the result is truncated to 64 bytes. An
/// empty result (a name of only illegal characters) falls back to `tool`.
pub fn sanitize_tool_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if is_provider_safe(c) { c } else { '_' })
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
/// Always exactly this, for every tool of every server. Nothing appends a
/// suffix and nothing depends on the order servers connected in, so the name
/// can be read straight off `config.toml` and written into a blueprint, an
/// `[mcp_overrides]` key or a grant before the server has ever been reached.
///
/// Joined first and sanitized once, so the 64-byte truncation falls where it
/// really falls. Sanitizing each half and joining afterwards would keep a
/// 60-character server name whole and cut the tool off the end instead.
/// [`validate_server_name`] keeps a server name well short of that limit, so
/// in practice the cut only ever lands in a very long tool name.
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

    // ─── validate_server_name ─────────────────────────────────────────────

    #[test]
    fn an_ordinary_server_name_is_accepted() {
        for name in ["tracker", "local-fs", "my_tools", "srv2"] {
            assert_eq!(validate_server_name(name), Ok(()), "{name} should be legal");
        }
    }

    /// The whole point. `my.tools` and `my_tools` used to sanitize to one
    /// prefix, which is what made a suffix necessary in the first place.
    #[test]
    fn a_dotted_server_name_is_refused_rather_than_rewritten() {
        let err = validate_server_name("my.tools").expect_err("a dot is refused");
        assert!(err.contains("my.tools"), "{err}");
        assert!(
            err.contains('.'),
            "the message names the offending character: {err}"
        );
        assert!(err.contains("letters, digits"), "{err}");
    }

    #[test]
    fn every_character_a_provider_refuses_is_named() {
        let err = validate_server_name("a b/c").expect_err("refused");
        assert!(err.contains(' '), "{err}");
        assert!(err.contains('/'), "{err}");
    }

    #[test]
    fn an_empty_server_name_is_refused() {
        assert!(validate_server_name("").is_err());
    }

    /// A server name at the provider limit leaves nothing for the tool, so the
    /// whole set would collapse onto one truncated string.
    #[test]
    fn a_server_name_that_fills_the_whole_limit_is_refused() {
        let err = validate_server_name(&"s".repeat(MAX_TOOL_NAME_LEN)).expect_err("refused");
        assert!(err.contains("shorter name"), "{err}");
        assert!(
            validate_server_name(&"s".repeat(MAX_TOOL_NAME_LEN - 1)).is_ok(),
            "one under the limit is still legal"
        );
    }
}

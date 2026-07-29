//! Provider script metadata, parsed from leading `// @key value` comments.

/// Metadata declared by a provider script via leading `//`-comment annotations.
///
/// All fields are optional and carry sensible defaults, so a script with no
/// annotations still loads. Recognized directives:
/// - `// @provider <name>` - informational name the script claims (activation is
///   by registry name, i.e. the config key / filename, not this).
/// - `// @description <text>`
/// - `// @default_model <id>`
/// - `// @max_context_tokens <int>` (default 8192)
/// - `// @max_output_tokens <int>` (default 4096)
/// - `// @supports_streaming <bool>` (advisory; real streaming is driven by
///   whether the script defines a `stream` function).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderMeta {
    /// Informational provider name (`@provider`).
    pub provider: Option<String>,
    /// One-line description (`@description`).
    pub description: String,
    /// Default model id (`@default_model`), used to fill an empty stage model.
    pub default_model: Option<String>,
    /// Maximum context (input) tokens (`@max_context_tokens`).
    pub max_context_tokens: usize,
    /// Maximum output tokens (`@max_output_tokens`).
    pub max_output_tokens: usize,
    /// Advisory streaming flag (`@supports_streaming`).
    pub supports_streaming: bool,
}

impl Default for ProviderMeta {
    fn default() -> Self {
        Self {
            provider: None,
            description: String::new(),
            default_model: None,
            max_context_tokens: 8192,
            max_output_tokens: 4096,
            supports_streaming: false,
        }
    }
}

/// Parse a provider script's [`ProviderMeta`] from its source comment
/// annotations. Unknown directives and non-comment lines are ignored; a value
/// that fails to parse (e.g. a non-integer `@max_context_tokens`) is ignored and
/// the default is kept, so bad annotations never fail a load.
pub fn parse_provider_annotations(src: &str) -> ProviderMeta {
    let mut meta = ProviderMeta::default();
    for line in src.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("//") else {
            continue;
        };
        let rest = rest.trim();
        let Some(directive) = rest.strip_prefix('@') else {
            continue;
        };
        let (keyword, arg) = match directive.split_once(char::is_whitespace) {
            Some((k, a)) => (k, a.trim()),
            None => (directive, ""),
        };
        match keyword {
            "provider" if !arg.is_empty() => meta.provider = Some(arg.to_string()),
            "description" => meta.description = arg.to_string(),
            "default_model" if !arg.is_empty() => meta.default_model = Some(arg.to_string()),
            "max_context_tokens" => {
                if let Ok(n) = arg.parse::<usize>() {
                    meta.max_context_tokens = n;
                }
            }
            "max_output_tokens" => {
                if let Ok(n) = arg.parse::<usize>() {
                    meta.max_output_tokens = n;
                }
            }
            "supports_streaming" => {
                if let Ok(b) = arg.parse::<bool>() {
                    meta.supports_streaming = b;
                }
            }
            _ => {}
        }
    }
    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_no_annotations() {
        let meta = parse_provider_annotations("fn inference(s, r) { #{} }");
        assert_eq!(meta, ProviderMeta::default());
        assert_eq!(meta.max_context_tokens, 8192);
        assert_eq!(meta.max_output_tokens, 4096);
        assert!(!meta.supports_streaming);
        assert!(meta.provider.is_none());
        assert!(meta.default_model.is_none());
        assert!(meta.description.is_empty());
    }

    #[test]
    fn parses_all_directives() {
        let src = "\
// @provider groq
// @description Groq inference (fast)
// @default_model llama-3.3-70b-versatile
// @max_context_tokens 131072
// @max_output_tokens 32768
// @supports_streaming true
fn inference(s, r) { #{} }
";
        let meta = parse_provider_annotations(src);
        assert_eq!(meta.provider.as_deref(), Some("groq"));
        assert_eq!(meta.description, "Groq inference (fast)");
        assert_eq!(
            meta.default_model.as_deref(),
            Some("llama-3.3-70b-versatile")
        );
        assert_eq!(meta.max_context_tokens, 131072);
        assert_eq!(meta.max_output_tokens, 32768);
        assert!(meta.supports_streaming);
    }

    #[test]
    fn ignores_bad_values_and_unknown_directives() {
        let src = "\
// @provider
// @max_context_tokens not-a-number
// @max_output_tokens also-bad
// @supports_streaming maybe
// @unknown whatever
// a plain comment
not a comment line
";
        let meta = parse_provider_annotations(src);
        // Empty @provider arg is ignored (stays None); bad numbers/bools keep defaults.
        assert!(meta.provider.is_none());
        assert_eq!(meta.max_context_tokens, 8192);
        assert_eq!(meta.max_output_tokens, 4096);
        assert!(!meta.supports_streaming);
    }

    #[test]
    fn directive_with_no_whitespace_arg() {
        // A directive keyword with no trailing argument at end-of-line.
        let meta = parse_provider_annotations("//@description");
        assert!(meta.description.is_empty());
    }
}

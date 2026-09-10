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
/// - `// @mime_type <type> family=<f> [text=<bool>] [extensions=<a,b>] [magic=<hex>]`
///   is a registry row the provider ships, so a run resolving onto it knows a
///   type its models are built for. Repeatable, one row per line. A provider
///   declares a type's shape, not a byte check; the operator's config and a
///   blueprint still override it.
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
    /// Mime type patterns the script's models accept (`@input_types`), as
    /// a comma-separated list: `// @input_types text/*, image/*`. Empty means
    /// text only.
    pub input_types: Vec<String>,
    /// Mime type patterns the script's models can hand back
    /// (`@output_types`). Empty means text only.
    pub output_types: Vec<String>,
    /// Registry rows the provider ships (`@mime_type`), so a run that resolves
    /// onto it knows a type its models are built for. Empty when it declares
    /// none.
    pub mime_rows: Vec<ProviderMimeRow>,
}

/// One `@mime_type` row: a type and the fields a provider may set on it. Not a
/// byte check - that lives beside the config or blueprint that names it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderMimeRow {
    /// The `type/subtype` (or `type/*`) the row answers for.
    pub mime_type: String,
    /// The family a provider keys its encoder on (`text`, `image`, `model`, ...).
    pub family: Option<String>,
    /// Whether the bytes are UTF-8 text.
    pub text: Option<bool>,
    /// File extensions, lowercase, without the dot.
    pub extensions: Vec<String>,
    /// A hex prefix identifying the bytes.
    pub magic: Option<String>,
}

impl ProviderMimeRow {
    /// The row as a TOML table body, for `MimeRegistry::layer`. Only the
    /// fields the directive set are written.
    pub fn to_toml(&self) -> toml::Table {
        let mut t = toml::Table::new();
        if let Some(f) = &self.family {
            t.insert("family".into(), toml::Value::String(f.clone()));
        }
        if let Some(b) = self.text {
            t.insert("text".into(), toml::Value::Boolean(b));
        }
        if !self.extensions.is_empty() {
            t.insert(
                "extensions".into(),
                toml::Value::Array(
                    self.extensions
                        .iter()
                        .map(|e| toml::Value::String(e.clone()))
                        .collect(),
                ),
            );
        }
        if let Some(m) = &self.magic {
            t.insert("magic".into(), toml::Value::String(m.clone()));
        }
        t
    }
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
            input_types: Vec::new(),
            output_types: Vec::new(),
            mime_rows: Vec::new(),
        }
    }
}

impl ProviderMeta {
    /// The provider's `@mime_type` rows as one registry table, keyed by type;
    /// empty when it declared none. A duplicate type keeps the last row.
    pub fn mime_rows_table(&self) -> toml::Table {
        let mut t = toml::Table::new();
        for row in &self.mime_rows {
            t.insert(row.mime_type.clone(), toml::Value::Table(row.to_toml()));
        }
        t
    }

    /// What the script declared its models take and produce; text only when
    /// it declared nothing.
    pub fn mime(&self) -> crate::capabilities::ModelMime {
        let mut mime = crate::capabilities::ModelMime::text_only();
        if !self.input_types.is_empty() {
            mime.input = self.input_types.clone();
        }
        if !self.output_types.is_empty() {
            mime.output = self.output_types.clone();
        }
        mime
    }
}

/// A comma-separated list of mime type patterns, trimmed and lowercased,
/// keeping only the ones shaped like a type.
fn parse_type_list(arg: &str) -> Vec<String> {
    arg.split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| s.contains('/') && !s.contains(char::is_whitespace))
        .collect()
}

/// Parse one `@mime_type <type> key=value ...` directive. The first token is
/// the type; the rest are `family=`, `text=`, `extensions=` (comma list) and
/// `magic=`. An unknown key is ignored, and a directive with no type is
/// dropped, so a typo never fails a load.
fn parse_mime_row(arg: &str) -> Option<ProviderMimeRow> {
    let mut parts = arg.split_whitespace();
    let mime_type = parts.next()?.to_ascii_lowercase();
    if !mime_type.contains('/') {
        return None;
    }
    let mut row = ProviderMimeRow {
        mime_type,
        ..Default::default()
    };
    for kv in parts {
        let Some((key, value)) = kv.split_once('=') else {
            continue;
        };
        match key {
            "family" if !value.is_empty() => row.family = Some(value.to_ascii_lowercase()),
            "text" => row.text = value.parse::<bool>().ok(),
            "extensions" => {
                row.extensions = value
                    .split(',')
                    .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
                    .filter(|e| !e.is_empty())
                    .collect();
            }
            "magic" if !value.is_empty() => row.magic = Some(value.to_string()),
            _ => {}
        }
    }
    Some(row)
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
            "input_types" => meta.input_types = parse_type_list(arg),
            "output_types" => meta.output_types = parse_type_list(arg),
            "mime_type" => {
                if let Some(row) = parse_mime_row(arg) {
                    meta.mime_rows.push(row);
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
    fn parses_mime_type_rows() {
        let src = "// @mime_type application/x-acme-scene family=model extensions=scene,scn magic=41434D45 text=false bareword unknown=9
// @mime_type model/gltf-binary family=model
// @mime_type text/x-note text=true
// @mime_type notatype ignored
// @mime_type
fn inference(s, r) { #{} }
";
        let meta = parse_provider_annotations(src);
        // The three well-formed rows; the one with no `/` and the empty one drop.
        assert_eq!(meta.mime_rows.len(), 3);
        // A row that sets no family carries none, and only what it did set.
        let note = &meta.mime_rows[2];
        assert_eq!(note.mime_type, "text/x-note");
        assert!(note.family.is_none());
        assert_eq!(note.text, Some(true));
        assert_eq!(note.to_toml().len(), 1);
        let scene = &meta.mime_rows[0];
        assert_eq!(scene.mime_type, "application/x-acme-scene");
        assert_eq!(scene.family.as_deref(), Some("model"));
        assert_eq!(scene.text, Some(false));
        assert_eq!(scene.extensions, ["scene", "scn"]);
        assert_eq!(scene.magic.as_deref(), Some("41434D45"));
        assert_eq!(meta.mime_rows[1].mime_type, "model/gltf-binary");
        assert!(meta.mime_rows[1].extensions.is_empty());

        // Rendered as a registry table keyed by type, only the set fields.
        let table = meta.mime_rows_table();
        assert_eq!(table.len(), 3);
        let row = table["application/x-acme-scene"].as_table().unwrap();
        assert_eq!(row["family"].as_str(), Some("model"));
        assert_eq!(row["text"].as_bool(), Some(false));
        assert_eq!(row["magic"].as_str(), Some("41434D45"));
        assert_eq!(row["extensions"].as_array().unwrap().len(), 2);
        // A row that set nothing but the family carries only the family.
        let gltf = table["model/gltf-binary"].as_table().unwrap();
        assert_eq!(gltf.len(), 1);
        assert!(gltf.contains_key("family"));
        // And the whole thing layers into a real registry.
        let mut reg = leviath_core::mime::MimeRegistry::builtin();
        reg.layer(&table, "provider:acme").unwrap();
        assert_eq!(
            reg.info(&leviath_core::mime::MimeType::parse("application/x-acme-scene").unwrap())
                .family,
            "model"
        );
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

    #[test]
    fn input_and_output_types_are_comma_lists_of_patterns() {
        let src = "\
// @input_types text/*, Image/PNG , not-a-type, bad type/x
// @output_types audio/*
fn inference(s, r) { #{} }
";
        let meta = parse_provider_annotations(src);
        assert_eq!(meta.input_types, vec!["text/*", "image/png"]);
        assert_eq!(meta.output_types, vec!["audio/*"]);
        let mime = meta.mime();
        assert_eq!(mime.input, vec!["text/*", "image/png"]);
        assert_eq!(mime.output, vec!["audio/*"]);
        let none = parse_provider_annotations("fn inference(s, r) { #{} }");
        assert!(none.input_types.is_empty());
        assert_eq!(none.mime(), crate::capabilities::ModelMime::text_only());
        let input_only = parse_provider_annotations("// @input_types text/*, image/*\n");
        assert_eq!(input_only.mime().output, vec!["text/*"]);
    }
}

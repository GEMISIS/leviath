//! Turning another harness's config file *contents* into Leviath
//! [`MCPServerConfig`]s.
//!
//! Everything here is a pure `&str -> Result<Vec<..>>` function. No path
//! resolution, no filesystem, no `#[cfg]` - those live in the parent module -
//! so every harness's format is unit-testable on every platform, including the
//! ones whose config files could never exist there.
//!
//! ## The shapes
//!
//! Nine harnesses, four families:
//!
//! * **`mcpServers` object** - Claude Code (`~/.claude.json`, plus a nested
//!   `mcpServers` per project), `.mcp.json`, Claude Desktop, Cursor, Windsurf,
//!   Gemini CLI. Entries are `{command, args, env}` or `{url, headers}`, with
//!   Gemini adding `httpUrl` and Windsurf `serverUrl`.
//! * **`servers` object** - VS Code, same entry shape with an explicit `type`.
//! * **`mcp` object** - OpenCode, whose entries are tagged `local`/`remote` and
//!   whose `command` is an *array* (argv) rather than a string.
//! * **`context_servers` object** - Zed, whose entry nests the launch under a
//!   `command` object (`{path, args, env}`).
//! * **`[mcp_servers]` table** - Codex, the one TOML source.
//!
//! Rather than nine near-identical structs, one tolerant entry parser accepts
//! the union of field names and every wrapper normalises into it. Unknown
//! fields are dropped rather than rejected: these files are written by other
//! tools that add keys on their own schedule, and refusing to import a server
//! because it carries a setting Leviath does not model would be useless
//! strictness.

use std::collections::HashMap;

use leviath_mcp::{MCPServerConfig, MCPTransport};

/// One server offered for import, with enough provenance to show the user
/// where it came from and what it would drag along.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The Leviath config entry this would become.
    pub config: MCPServerConfig,
    /// Sub-location within the file, when one file holds several scopes
    /// (Claude Code keys servers per project). Empty for a flat file.
    pub scope: String,
    /// `env` / `headers` keys whose values look like a literal credential
    /// rather than a `${VAR}` reference. Surfaced so importing a server does
    /// not silently copy another tool's token into `~/.leviath/config.toml`.
    pub inline_secrets: Vec<String>,
}

/// Env/header key fragments that mark a value as credential-shaped.
const SECRET_HINTS: [&str; 7] = [
    "token", "key", "secret", "password", "passwd", "auth", "bearer",
];

/// Whether `value` under `key` looks like a literal credential.
///
/// `${VAR}` and `$VAR` are references Leviath expands at connect time, so they
/// are not secrets in the file; anything else under a credential-shaped key is.
/// Deliberately conservative in one direction only - a false positive costs the
/// user one glance at a flagged row, a false negative silently copies a live
/// token into a second file on disk.
fn looks_like_inline_secret(key: &str, value: &str) -> bool {
    if value.is_empty() || value.starts_with("${") || value.starts_with('$') {
        return false;
    }
    let lower = key.to_ascii_lowercase();
    SECRET_HINTS.iter().any(|hint| lower.contains(hint))
}

/// Collect the credential-shaped keys of one candidate's `env` and `headers`.
fn inline_secrets(config: &MCPServerConfig) -> Vec<String> {
    let mut found: Vec<String> = config
        .env
        .iter()
        .chain(config.headers.iter())
        .filter(|(k, v)| looks_like_inline_secret(k, v))
        .map(|(k, _)| k.clone())
        .collect();
    found.sort();
    found
}

/// Read a JSON object of `string -> string`, skipping non-string values rather
/// than failing: a harness that allows a number or `null` in `env` should cost
/// that one variable, not the whole server.
fn string_map(value: Option<&serde_json::Value>) -> HashMap<String, String> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Read a JSON array of strings, skipping non-string elements.
fn string_list(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether an entry is switched off in its own harness. An explicitly disabled
/// server should not be offered - the user already said no once.
fn is_disabled(entry: &serde_json::Map<String, serde_json::Value>) -> bool {
    entry.get("enabled").and_then(|v| v.as_bool()) == Some(false)
        || entry.get("disabled").and_then(|v| v.as_bool()) == Some(true)
}

/// Parse one server entry from any of the JSON-shaped harnesses.
///
/// Returns `None` when the entry is disabled, malformed, or describes neither a
/// command nor a URL - an entry Leviath cannot connect to is not a candidate.
///
/// Precedence is URL over command. Several harnesses carry both (a stdio
/// fallback alongside a hosted endpoint), and [`MCPServerConfig::resolve`]
/// rejects an entry that sets both without an explicit transport, so exactly
/// one is kept and the transport is stated outright.
pub fn parse_json_entry(name: &str, value: &serde_json::Value) -> Option<Candidate> {
    let entry = value.as_object()?;
    if is_disabled(entry) {
        return None;
    }

    // Zed nests the launch under a `command` *object*; everyone else uses a
    // string (or, for OpenCode, an argv array).
    let nested = entry.get("command").and_then(|v| v.as_object());
    let command_field = nested
        .and_then(|c| c.get("path"))
        .or_else(|| entry.get("command"));

    let (command, mut args) = match command_field {
        Some(serde_json::Value::String(s)) => (Some(s.clone()), Vec::new()),
        // OpenCode: `command: ["npx", "-y", "pkg"]` - head is the program.
        Some(serde_json::Value::Array(_)) => {
            let argv = string_list(entry.get("command"));
            let mut it = argv.into_iter();
            (it.next(), it.collect())
        }
        _ => (None, Vec::new()),
    };
    let declared_args = string_list(nested.map_or_else(|| entry.get("args"), |c| c.get("args")));
    if !declared_args.is_empty() {
        args = declared_args;
    }

    let url = ["url", "httpUrl", "serverUrl", "endpoint"]
        .iter()
        .find_map(|k| entry.get(*k).and_then(|v| v.as_str()))
        .map(str::to_owned);

    let mut env = string_map(nested.map_or_else(|| entry.get("env"), |c| c.get("env")));
    // OpenCode spells it `environment`.
    env.extend(string_map(entry.get("environment")));
    let headers = string_map(entry.get("headers"));

    let config = match (url, command) {
        (Some(url), _) => MCPServerConfig {
            name: name.to_string(),
            transport: Some(MCPTransport::Http),
            url: Some(url),
            headers,
            ..MCPServerConfig::default()
        },
        (None, Some(command)) => MCPServerConfig {
            name: name.to_string(),
            transport: Some(MCPTransport::Stdio),
            command: Some(command),
            args,
            env,
            ..MCPServerConfig::default()
        },
        // Neither: nothing to connect to.
        (None, None) => return None,
    };

    // A malformed `[[mcp_servers]]` entry is a hard error in `Config::load`, so
    // importing one would brick the whole config file rather than costing one
    // server. Both shapes above are valid by construction - each sets exactly
    // one of `command`/`url` and states its transport outright, which is
    // precisely what `validate` checks - so there is no runtime check here to
    // reject something that cannot be built. `every_candidate_validates` holds
    // the invariant instead, and would fail loudly if a future edit to this
    // function broke it.

    Some(Candidate {
        inline_secrets: inline_secrets(&config),
        config,
        scope: String::new(),
    })
}

/// Parse every entry of a JSON object of servers, sorted by name for a stable
/// display order.
fn parse_json_map(map: Option<&serde_json::Value>) -> Vec<Candidate> {
    let Some(obj) = map.and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut out: Vec<Candidate> = obj
        .iter()
        .filter_map(|(name, value)| parse_json_entry(name, value))
        .collect();
    out.sort_by(|a, b| a.config.name.cmp(&b.config.name));
    out
}

/// A JSON file whose servers live under one top-level key.
///
/// Covers `.mcp.json`, Claude Desktop, Cursor, Windsurf, and Gemini CLI
/// (`mcpServers`), VS Code (`servers`), OpenCode (`mcp`), and Zed
/// (`context_servers`) - the key is the only thing that differs.
pub fn parse_json_object(contents: &str, key: &str) -> anyhow::Result<Vec<Candidate>> {
    let root: serde_json::Value = serde_json::from_str(contents)?;
    Ok(parse_json_map(root.get(key)))
}

/// Claude Code's `~/.claude.json`: a global `mcpServers` object plus a
/// per-project one under `projects.<absolute path>.mcpServers`.
///
/// Both are offered. A server configured for one repo is still a server the
/// user set up and may want globally, and the project path rides along as the
/// candidate's scope so the wizard can say where each came from.
pub fn parse_claude_code(contents: &str) -> anyhow::Result<Vec<Candidate>> {
    let root: serde_json::Value = serde_json::from_str(contents)?;
    let mut out = parse_json_map(root.get("mcpServers"));

    if let Some(projects) = root.get("projects").and_then(|v| v.as_object()) {
        let mut paths: Vec<&String> = projects.keys().collect();
        paths.sort();
        for path in paths {
            let scoped = parse_json_map(projects.get(path).and_then(|p| p.get("mcpServers")));
            out.extend(scoped.into_iter().map(|mut c| {
                c.scope = path.clone();
                c
            }));
        }
    }
    Ok(out)
}

/// Codex's `~/.codex/config.toml`, whose servers live in an `[mcp_servers]`
/// table. Reuses the JSON entry parser by converting the TOML table through
/// `serde_json::Value` - the field names are identical, and one tolerant entry
/// parser beats two that must be kept in step.
pub fn parse_codex(contents: &str) -> anyhow::Result<Vec<Candidate>> {
    // Deserialize the TOML straight into a `serde_json::Value` rather than
    // parsing to `toml::Value` and converting: the conversion step's error arm
    // cannot happen for anything TOML can produce, and an error arm that cannot
    // be reached is worse than not having one.
    let root: serde_json::Value = toml::from_str(contents)?;
    Ok(parse_json_map(root.get("mcp_servers")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by_name<'a>(cands: &'a [Candidate], name: &str) -> &'a Candidate {
        cands
            .iter()
            .find(|c| c.config.name == name)
            .expect("candidate is present")
    }

    // ─── stdio entries ──────────────────────────────────────────────────────

    #[test]
    fn parses_a_stdio_entry_with_args_and_env() {
        let json = r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","@mcp/fs"],
                       "env":{"ROOT":"/tmp"}}}}"#;

        let found = parse_json_object(json, "mcpServers").unwrap();

        assert_eq!(found.len(), 1);
        let c = &found[0].config;
        assert_eq!(c.name, "fs");
        assert_eq!(c.transport, Some(MCPTransport::Stdio));
        assert_eq!(c.command.as_deref(), Some("npx"));
        assert_eq!(c.args, vec!["-y", "@mcp/fs"]);
        assert_eq!(c.env.get("ROOT").map(String::as_str), Some("/tmp"));
        assert!(c.url.is_none());
        assert!(found[0].inline_secrets.is_empty());
    }

    #[test]
    fn parses_an_opencode_argv_command() {
        // OpenCode gives the whole argv as an array under `command`, with the
        // environment under `environment`.
        let json = r#"{"mcp":{"fs":{"type":"local","command":["npx","-y","@mcp/fs"],
                       "environment":{"ROOT":"/tmp"}}}}"#;

        let found = parse_json_object(json, "mcp").unwrap();

        let c = &by_name(&found, "fs").config;
        assert_eq!(c.command.as_deref(), Some("npx"));
        assert_eq!(c.args, vec!["-y", "@mcp/fs"]);
        assert_eq!(c.env.get("ROOT").map(String::as_str), Some("/tmp"));
    }

    #[test]
    fn parses_a_zed_nested_command_object() {
        let json = r#"{"context_servers":{"fs":{"command":{"path":"npx",
                       "args":["-y","@mcp/fs"],"env":{"ROOT":"/tmp"}}}}}"#;

        let found = parse_json_object(json, "context_servers").unwrap();

        let c = &by_name(&found, "fs").config;
        assert_eq!(c.command.as_deref(), Some("npx"));
        assert_eq!(c.args, vec!["-y", "@mcp/fs"]);
        assert_eq!(c.env.get("ROOT").map(String::as_str), Some("/tmp"));
    }

    // ─── http entries ───────────────────────────────────────────────────────

    #[test]
    fn parses_http_entries_under_every_spelling_of_the_url_field() {
        // Gemini CLI uses `httpUrl`, Windsurf `serverUrl`, everyone else `url`.
        for key in ["url", "httpUrl", "serverUrl", "endpoint"] {
            let json = format!(r#"{{"mcpServers":{{"api":{{"{key}":"https://x.test/mcp"}}}}}}"#);

            let found = parse_json_object(&json, "mcpServers").unwrap();

            let c = &by_name(&found, "api").config;
            assert_eq!(c.transport, Some(MCPTransport::Http), "field {key}");
            assert_eq!(c.url.as_deref(), Some("https://x.test/mcp"));
            assert!(c.command.is_none());
        }
    }

    #[test]
    fn a_url_wins_over_a_command_so_the_entry_stays_resolvable() {
        // `MCPServerConfig::resolve` rejects an entry carrying both without an
        // explicit transport, so importing both fields would produce a config
        // that fails to load.
        let json = r#"{"mcpServers":{"both":{"command":"npx","url":"https://x.test/mcp"}}}"#;

        let found = parse_json_object(json, "mcpServers").unwrap();

        let c = &by_name(&found, "both").config;
        assert_eq!(c.url.as_deref(), Some("https://x.test/mcp"));
        assert!(c.command.is_none());
        assert!(c.resolve().is_ok());
    }

    #[test]
    fn every_candidate_validates() {
        // The load-bearing invariant: a malformed `[[mcp_servers]]` entry is a
        // hard error in `Config::load`, so importing one would brick the whole
        // config file. `parse_json_entry` guarantees validity by construction
        // rather than checking at runtime, and this is what holds it to that.
        let shapes = [
            r#"{"s":{"a":{"command":"x"}}}"#,
            r#"{"s":{"a":{"command":"x","args":["1"],"env":{"K":"V"}}}}"#,
            r#"{"s":{"a":{"command":["npx","-y","p"]}}}"#,
            r#"{"s":{"a":{"command":{"path":"npx","args":["-y"]}}}}"#,
            r#"{"s":{"a":{"url":"https://y.test"}}}"#,
            r#"{"s":{"a":{"httpUrl":"https://y.test","headers":{"H":"V"}}}}"#,
            r#"{"s":{"a":{"serverUrl":"https://y.test"}}}"#,
            r#"{"s":{"a":{"endpoint":"https://y.test"}}}"#,
            r#"{"s":{"a":{"command":"x","url":"https://y.test"}}}"#,
        ];

        for shape in shapes {
            let found = parse_json_object(shape, "s").unwrap();
            assert_eq!(found.len(), 1, "shape produced no candidate: {shape}");
            assert!(
                found[0].config.validate().is_ok(),
                "shape produced an invalid entry: {shape}"
            );
            assert!(found[0].config.resolve().is_ok());
        }
    }

    // ─── entries that are not candidates ────────────────────────────────────

    #[test]
    fn skips_entries_with_neither_a_command_nor_a_url() {
        let json = r#"{"mcpServers":{"empty":{"description":"nothing to connect to"}}}"#;

        assert!(parse_json_object(json, "mcpServers").unwrap().is_empty());
    }

    #[test]
    fn skips_entries_the_other_harness_has_switched_off() {
        // The user already said no to these once.
        let json = r#"{"mcpServers":{"off":{"command":"x","enabled":false},
                       "also-off":{"command":"y","disabled":true},
                       "on":{"command":"z","enabled":true}}}"#;

        let found = parse_json_object(json, "mcpServers").unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].config.name, "on");
    }

    #[test]
    fn skips_non_object_entries() {
        let json = r#"{"mcpServers":{"bogus":"just a string","ok":{"command":"x"}}}"#;

        let found = parse_json_object(json, "mcpServers").unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].config.name, "ok");
    }

    #[test]
    fn a_missing_or_non_object_key_yields_nothing() {
        assert!(parse_json_object("{}", "mcpServers").unwrap().is_empty());
        assert!(
            parse_json_object(r#"{"mcpServers":[]}"#, "mcpServers")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn malformed_json_is_an_error_not_a_silent_empty_list() {
        // VS Code and Zed allow comments in these files; serde_json does not.
        // The caller shows the row as unreadable rather than pretending the
        // harness configured nothing.
        let err = parse_json_object("{ // a comment\n }", "servers");

        assert!(err.is_err());
    }

    #[test]
    fn non_string_values_inside_env_and_args_are_dropped_not_fatal() {
        let json = r#"{"mcpServers":{"x":{"command":"c","args":["a",7,"b"],
                       "env":{"GOOD":"1","BAD":2}}}}"#;

        let found = parse_json_object(json, "mcpServers").unwrap();

        let c = &found[0].config;
        assert_eq!(c.args, vec!["a", "b"]);
        assert_eq!(c.env.len(), 1);
        assert_eq!(c.env.get("GOOD").map(String::as_str), Some("1"));
    }

    // ─── inline secrets ─────────────────────────────────────────────────────

    #[test]
    fn flags_credential_shaped_env_and_header_values() {
        let json = r#"{"mcpServers":{
            "a":{"command":"x","env":{"API_TOKEN":"sk-live-123","ROOT":"/tmp"}},
            "b":{"url":"https://y.test","headers":{"Authorization":"Bearer abc"}}}}"#;

        let found = parse_json_object(json, "mcpServers").unwrap();

        assert_eq!(by_name(&found, "a").inline_secrets, vec!["API_TOKEN"]);
        assert_eq!(by_name(&found, "b").inline_secrets, vec!["Authorization"]);
    }

    #[test]
    fn does_not_flag_env_references_or_innocuous_keys() {
        // `${VAR}` and `$VAR` are expanded at connect time, so nothing
        // sensitive is in the file.
        let json = r#"{"mcpServers":{"a":{"command":"x","env":{
            "API_TOKEN":"${GITHUB_TOKEN}","OTHER_KEY":"$SOME_VAR",
            "EMPTY_SECRET":"","ROOT":"/tmp"}}}}"#;

        let found = parse_json_object(json, "mcpServers").unwrap();

        assert!(found[0].inline_secrets.is_empty());
    }

    #[test]
    fn secret_detection_covers_every_hint_and_is_case_insensitive() {
        for hint in SECRET_HINTS {
            assert!(
                looks_like_inline_secret(&format!("MY_{}", hint.to_uppercase()), "literal"),
                "{hint} should be treated as credential-shaped"
            );
        }
        assert!(!looks_like_inline_secret("ROOT_DIR", "literal"));
    }

    // ─── Claude Code ────────────────────────────────────────────────────────

    #[test]
    fn claude_code_yields_global_and_per_project_servers_with_scopes() {
        let json = r#"{
            "mcpServers":{"global":{"url":"https://g.test/mcp"}},
            "projects":{
              "/repo/b":{"mcpServers":{"beta":{"command":"b"}}},
              "/repo/a":{"mcpServers":{"alpha":{"command":"a"}}},
              "/repo/c":{"other":"no servers here"}
            }}"#;

        let found = parse_claude_code(json).unwrap();

        assert_eq!(found.len(), 3);
        assert_eq!(found[0].config.name, "global");
        assert!(found[0].scope.is_empty());
        // Projects are visited in sorted path order, so the listing is stable.
        assert_eq!(found[1].config.name, "alpha");
        assert_eq!(found[1].scope, "/repo/a");
        assert_eq!(found[2].config.name, "beta");
        assert_eq!(found[2].scope, "/repo/b");
    }

    #[test]
    fn claude_code_tolerates_a_file_with_no_servers_at_all() {
        // The overwhelmingly common case: a big `~/.claude.json` full of
        // unrelated state, with `projects` empty or absent entirely.
        assert!(
            parse_claude_code(r#"{"numStartups":12,"projects":{}}"#)
                .unwrap()
                .is_empty()
        );
        assert!(
            parse_claude_code(r#"{"numStartups":12}"#)
                .unwrap()
                .is_empty()
        );
        assert!(
            parse_claude_code(r#"{"projects":"not an object"}"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn claude_code_rejects_malformed_json() {
        assert!(parse_claude_code("not json").is_err());
    }

    // ─── Codex ──────────────────────────────────────────────────────────────

    #[test]
    fn codex_parses_stdio_and_http_tables() {
        let toml_src = r#"
            [mcp_servers.fs]
            command = "npx"
            args = ["-y", "@mcp/fs"]

            [mcp_servers.fs.env]
            ROOT = "/tmp"

            [mcp_servers.api]
            url = "https://x.test/mcp"
        "#;

        let found = parse_codex(toml_src).unwrap();

        assert_eq!(found.len(), 2);
        let fs = &by_name(&found, "fs").config;
        assert_eq!(fs.command.as_deref(), Some("npx"));
        assert_eq!(fs.args, vec!["-y", "@mcp/fs"]);
        assert_eq!(fs.env.get("ROOT").map(String::as_str), Some("/tmp"));
        assert_eq!(
            by_name(&found, "api").config.url.as_deref(),
            Some("https://x.test/mcp")
        );
    }

    #[test]
    fn codex_without_an_mcp_servers_table_yields_nothing() {
        assert!(parse_codex("model = \"gpt-5\"\n").unwrap().is_empty());
    }

    #[test]
    fn codex_rejects_malformed_toml() {
        assert!(parse_codex("[[[not toml").is_err());
    }
}

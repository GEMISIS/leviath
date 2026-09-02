//! What `lev integrate` writes for each host: where its MCP config lives, how
//! the `leviath` server entry is merged into it, and where its skill goes.
//!
//! Every host is the same three layers (an MCP server entry, a `SKILL.md`,
//! and the notes a person still has to act on), so this module is one small
//! plan per host plus the two merge routines they share: a JSON merge through
//! `serde_json` and a TOML merge through `toml_edit`. Both keep every key they
//! did not touch, so a config a person hand-wrote survives the command.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use toml_edit::{Array, DocumentMut, Item, Table};

use super::skill::render_skill;
use super::{IntegrateArgs, IntegrateEnv, Report};

/// The name the server is registered under everywhere. It is part of the
/// model-visible tool name on every host, so it stays one lowercase word.
pub(crate) const SERVER_NAME: &str = "leviath";

/// The wall clock a host gives one tool call, in seconds: a day, because a
/// blocking `run` lasts as long as the agent run behind it.
const TOOL_TIMEOUT_SECS: i64 = 86_400;

/// How long a host waits for the server to answer `initialize`, in seconds.
const STARTUP_TIMEOUT_SECS: i64 = 30;

/// The five hosts `lev integrate` knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostKind {
    /// Claude Code: `~/.claude.json` (or `$CLAUDE_CONFIG_DIR/.claude.json`).
    ClaudeCode,
    /// Grok Build: `~/.grok/config.toml`.
    Grok,
    /// Codex CLI: `~/.codex/config.toml`.
    Codex,
    /// Gemini CLI: `~/.gemini/settings.json`.
    Gemini,
    /// Hermes Agent: `~/.hermes/config.yaml`, which it prints rather than writes.
    Hermes,
}

/// Every host, in the order `all` visits them.
pub(crate) const ALL_HOSTS: [HostKind; 5] = [
    HostKind::ClaudeCode,
    HostKind::Grok,
    HostKind::Codex,
    HostKind::Gemini,
    HostKind::Hermes,
];

impl HostKind {
    /// The name a person types and the reports print.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Grok => "grok",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Hermes => "hermes",
        }
    }

    /// The dot-directory under the home directory whose presence means the
    /// host is installed. Claude Code's moves with `CLAUDE_CONFIG_DIR`.
    pub(crate) fn dot_dir(self, env: &IntegrateEnv) -> PathBuf {
        match self {
            Self::ClaudeCode => env
                .claude_config_dir
                .clone()
                .unwrap_or_else(|| env.home.join(".claude")),
            Self::Grok => env.home.join(".grok"),
            Self::Codex => env.home.join(".codex"),
            Self::Gemini => env.home.join(".gemini"),
            Self::Hermes => env.home.join(".hermes"),
        }
    }

    /// Where this host's copy of the skill goes.
    fn skill_path(self, args: &IntegrateArgs, env: &IntegrateEnv) -> PathBuf {
        let root = match self {
            Self::ClaudeCode if args.project => env.cwd.join(".claude").join("skills"),
            Self::ClaudeCode => self.dot_dir(env).join("skills"),
            Self::Hermes => self
                .dot_dir(env)
                .join("skills")
                .join("autonomous-ai-agents"),
            Self::Grok | Self::Codex | Self::Gemini => self.dot_dir(env).join("skills"),
        };
        root.join(SERVER_NAME).join("SKILL.md")
    }
}

/// The `args` every host passes to the binary.
fn serve_args() -> Vec<String> {
    vec!["mcp".to_string(), "serve".to_string()]
}

/// The path of the binary as a string, for config files that hold strings.
fn exe_string(env: &IntegrateEnv) -> String {
    env.lev_exe.to_string_lossy().into_owned()
}

/// Register the server and install the skill for one host.
pub(crate) fn integrate_host(
    host: HostKind,
    args: &IntegrateArgs,
    env: &IntegrateEnv,
    report: &mut Report,
) -> anyhow::Result<()> {
    report.section(host.label());
    match host {
        HostKind::ClaudeCode => claude_code(args, env, report)?,
        HostKind::Grok => grok(args, env, report)?,
        HostKind::Codex => codex(args, env, report)?,
        HostKind::Gemini => gemini(args, env, report)?,
        HostKind::Hermes => hermes(env, report),
    }
    if args.no_skill {
        report.note("skill skipped (--no-skill)");
    } else {
        let path = host.skill_path(args, env);
        write_text(&path, &render_skill(host), args.print, report)?;
    }
    if args.project && matches!(host, HostKind::Codex | HostKind::Gemini | HostKind::Hermes) {
        report.note(format!(
            "--project has no effect for {}: its MCP config is per user",
            host.label()
        ));
    }
    Ok(())
}

// ─── Claude Code ─────────────────────────────────────────────────────────────

/// The JSON entry Claude Code (and `.mcp.json`) wants.
fn claude_entry(env: &IntegrateEnv) -> Value {
    json!({ "type": "stdio", "command": exe_string(env), "args": serve_args() })
}

fn claude_code(
    args: &IntegrateArgs,
    env: &IntegrateEnv,
    report: &mut Report,
) -> anyhow::Result<()> {
    let entry = claude_entry(env);
    if args.project {
        let path = env.cwd.join(".mcp.json");
        return merge_json_file(
            &path,
            &["mcpServers", SERVER_NAME],
            entry,
            args.print,
            report,
        );
    }
    // `CLAUDE_CONFIG_DIR` replaces `~/.claude`, and the user config sits
    // beside that directory as `.claude.json`, so with the variable set the
    // file is `$CLAUDE_CONFIG_DIR/.claude.json`, not `~/.claude.json`.
    let config = match &env.claude_config_dir {
        Some(dir) => dir.join(".claude.json"),
        None => env.home.join(".claude.json"),
    };
    let Some(claude) = (env.which)("claude") else {
        report.note("`claude` is not on PATH; writing its config directly");
        return merge_json_file(
            &config,
            &["mcpServers", SERVER_NAME],
            entry,
            args.print,
            report,
        );
    };
    let argv: Vec<String> = ["mcp", "add-json", "--scope", "user", SERVER_NAME]
        .into_iter()
        .map(str::to_string)
        .chain(std::iter::once(entry.to_string()))
        .collect();
    if args.print {
        report.note(format!(
            "would run: {} {}",
            claude.display(),
            shell_words(&argv)
        ));
        return Ok(());
    }
    match (env.run)(&claude, &argv) {
        Ok(_) => {
            report.note(format!(
                "registered {SERVER_NAME} with `claude mcp add-json --scope user`"
            ));
            Ok(())
        }
        Err(e) => {
            report.note(format!(
                "`claude mcp add-json` did not succeed ({e}); writing {} directly",
                config.display()
            ));
            merge_json_file(
                &config,
                &["mcpServers", SERVER_NAME],
                entry,
                args.print,
                report,
            )
        }
    }
}

/// Quote argv for display. The JSON argument carries spaces and quotes, so
/// anything that is not a plain word is single-quoted.
fn shell_words(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ─── Grok and Codex (TOML) ───────────────────────────────────────────────────

fn grok(args: &IntegrateArgs, env: &IntegrateEnv, report: &mut Report) -> anyhow::Result<()> {
    let path = if args.project {
        env.cwd.join(".grok").join("config.toml")
    } else {
        HostKind::Grok.dot_dir(env).join("config.toml")
    };
    merge_toml_file(&path, env, args.print, report)?;
    report.note(
        "Grok also imports ~/.claude.json, so `grok mcp list` may show leviath twice; \
         config.toml wins by name",
    );
    Ok(())
}

fn codex(args: &IntegrateArgs, env: &IntegrateEnv, report: &mut Report) -> anyhow::Result<()> {
    let path = HostKind::Codex.dot_dir(env).join("config.toml");
    merge_toml_file(&path, env, args.print, report)
}

// ─── Gemini (JSON) ───────────────────────────────────────────────────────────

fn gemini(args: &IntegrateArgs, env: &IntegrateEnv, report: &mut Report) -> anyhow::Result<()> {
    let path = HostKind::Gemini.dot_dir(env).join("settings.json");
    // Gemini's `timeout` is milliseconds.
    let entry = json!({
        "command": exe_string(env),
        "args": serve_args(),
        "timeout": TOOL_TIMEOUT_SECS * 1000,
    });
    merge_json_file(
        &path,
        &["mcpServers", SERVER_NAME],
        entry,
        args.print,
        report,
    )
}

// ─── Hermes (printed snippet) ────────────────────────────────────────────────

/// The `mcp_servers:` stanza for `~/.hermes/config.yaml`. Printed, not
/// written: there is no YAML dependency in the tree, and a config a gateway
/// agent is running from deserves a person's eyes on the paste.
pub(crate) fn hermes_snippet(env: &IntegrateEnv) -> String {
    format!(
        "mcp_servers:\n  {SERVER_NAME}:\n    command: {}\n    args: [\"mcp\", \"serve\"]\n    \
         timeout: {TOOL_TIMEOUT_SECS}\n    connect_timeout: {STARTUP_TIMEOUT_SECS}\n",
        json!(exe_string(env))
    )
}

fn hermes(env: &IntegrateEnv, report: &mut Report) {
    let config = HostKind::Hermes.dot_dir(env).join("config.yaml");
    report.note(format!(
        "add this to {} (merge into an existing `mcp_servers:` block), then run /reload-mcp in \
         chat:\n\n{}",
        config.display(),
        hermes_snippet(env)
    ));
}

// ─── Shared merge and write helpers ─────────────────────────────────────────

/// Read `path` if it exists. A directory where a file should be is an error,
/// not "nothing there": overwriting it would not be what anyone meant.
fn read_existing(path: &Path) -> anyhow::Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    std::fs::read_to_string(path)
        .map(Some)
        .map_err(|e| anyhow::anyhow!("could not read {}: {e}", path.display()))
}

/// Write `contents` to `path`, creating parents, or with `print` say what
/// would be written and touch nothing.
pub(crate) fn write_text(
    path: &Path,
    contents: &str,
    print: bool,
    report: &mut Report,
) -> anyhow::Result<()> {
    if print {
        report.would_write(path, contents);
        return Ok(());
    }
    // Idempotent, visibly: a file that already holds these bytes is left
    // alone and said to be, so a second run reads as the no-op it is.
    if std::fs::read_to_string(path).ok().as_deref() == Some(contents) {
        report.unchanged(path);
        return Ok(());
    }
    // A parent is always there: every path here ends in a file name under
    // a host directory, so `parent()` is `Some` and a fallback would be dead.
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| anyhow::anyhow!("could not create {}: {e}", parent.display()))?;
    std::fs::write(path, contents)
        .map_err(|e| anyhow::anyhow!("could not write {}: {e}", path.display()))?;
    report.wrote(path);
    Ok(())
}

/// Set `keys` (a path of object keys) to `value` inside the JSON object in
/// `existing`, creating intermediate objects and keeping every other key.
///
/// The workspace's `serde_json` has no `preserve_order` feature, so the keys
/// of a rewritten file come out sorted rather than in their original order.
/// Accepted: the hosts read these files by key, and the alternative is a
/// hand-rolled JSON editor for a file that is rewritten by their own CLIs too.
pub(crate) fn merge_json(
    existing: Option<&str>,
    keys: &[&str],
    value: Value,
) -> anyhow::Result<String> {
    let mut root: Value = match existing.map(str::trim).filter(|s| !s.is_empty()) {
        Some(text) => {
            serde_json::from_str(text).map_err(|e| anyhow::anyhow!("not valid JSON: {e}"))?
        }
        None => json!({}),
    };
    let (leaf, parents) = keys
        .split_last()
        .ok_or_else(|| anyhow::anyhow!("no key to set"))?;
    let mut obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("the top level is not a JSON object"))?;
    for key in parents {
        let child = obj.entry(*key).or_insert_with(|| json!({}));
        obj = child
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("`{key}` is not a JSON object"))?;
    }
    obj.insert((*leaf).to_string(), value);
    // `Value`'s alternate `Display` is the pretty printer, and it cannot
    // fail: there is no error arm to leave untested.
    Ok(format!("{root:#}\n"))
}

fn merge_json_file(
    path: &Path,
    keys: &[&str],
    value: Value,
    print: bool,
    report: &mut Report,
) -> anyhow::Result<()> {
    let existing = read_existing(path)?;
    let merged = merge_json(existing.as_deref(), keys, value)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    write_text(path, &merged, print, report)
}

/// Merge `[mcp_servers.leviath]` into the TOML in `existing`, keeping every
/// other table, key and comment as `toml_edit` found it.
pub(crate) fn merge_toml(existing: Option<&str>, env: &IntegrateEnv) -> anyhow::Result<String> {
    let mut doc: DocumentMut = existing
        .unwrap_or_default()
        .parse()
        .map_err(|e: toml_edit::TomlError| anyhow::anyhow!("not valid TOML: {e}"))?;
    let servers = doc
        .entry("mcp_servers")
        .or_insert_with(|| {
            let mut t = Table::new();
            // Implicit, so the file gains `[mcp_servers.leviath]` and not an
            // empty `[mcp_servers]` header above it.
            t.set_implicit(true);
            Item::Table(t)
        })
        .as_table_like_mut()
        .ok_or_else(|| anyhow::anyhow!("`mcp_servers` is not a table"))?;
    let server = servers
        .entry(SERVER_NAME)
        .or_insert(Item::Table(Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| anyhow::anyhow!("`mcp_servers.{SERVER_NAME}` is not a table"))?;
    server.insert("command", toml_edit::value(exe_string(env)));
    server.insert("args", toml_edit::value(Array::from_iter(serve_args())));
    server.insert(
        "startup_timeout_sec",
        toml_edit::value(STARTUP_TIMEOUT_SECS),
    );
    server.insert("tool_timeout_sec", toml_edit::value(TOOL_TIMEOUT_SECS));
    Ok(doc.to_string())
}

fn merge_toml_file(
    path: &Path,
    env: &IntegrateEnv,
    print: bool,
    report: &mut Report,
) -> anyhow::Result<()> {
    let existing = read_existing(path)?;
    let merged = merge_toml(existing.as_deref(), env)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    write_text(path, &merged, print, report)
}

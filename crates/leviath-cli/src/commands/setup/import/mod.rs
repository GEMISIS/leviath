//! Discovering MCP servers already configured in other agent harnesses.
//!
//! Someone installing Leviath has usually already wired up MCP servers
//! somewhere else — Claude Code, Cursor, Codex, Zed. Making them retype each
//! one is busywork, and the entries are close enough in shape to convert
//! mechanically, so `lev setup` offers to import them.
//!
//! ## Layering
//!
//! Two halves, deliberately separated:
//!
//! * [`formats`] turns file *contents* into candidates. Pure, no filesystem, no
//!   `#[cfg]` — every harness's format is testable on every platform, including
//!   ones whose files could never exist there.
//! * This module knows *where* those files live — the only platform-dependent
//!   part. It takes its roots as an injected [`Roots`] rather than reading the
//!   environment, so the whole table is testable against tempdirs, and the one
//!   genuine per-OS branch is `#[cfg]`-gated in a single place, per the rule
//!   `daemon_service` follows.
//!
//! Nothing here is prescribed to Leviath as a whole. Scanning a user's home
//! directory for other tools' config is a desktop-shaped idea; a future mobile
//! host would simply have no sources and offer no import step, with everything
//! downstream unchanged.
//!
//! ## What is deliberately *not* done
//!
//! Discovery never connects to a server, never runs a command, and never reads
//! anything outside the specific files listed below.

pub mod formats;

use std::path::{Path, PathBuf};

pub use formats::Candidate;

/// How a source file's servers are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// A JSON object of servers under one top-level key.
    JsonObject(&'static str),
    /// Claude Code's `~/.claude.json`: global plus per-project scopes.
    ClaudeCode,
    /// Codex's `[mcp_servers]` TOML table.
    CodexToml,
}

/// One harness Leviath knows how to read.
#[derive(Debug, Clone)]
pub struct Source {
    /// Stable id, used in messages and as a dedup key.
    pub id: &'static str,
    /// Name to show the user.
    pub display: &'static str,
    /// The file to read.
    pub path: PathBuf,
    /// How to parse it.
    pub layout: Layout,
    /// Whether this file's format tolerates comments (JSONC). `serde_json`
    /// does not, so a parse failure here is expected rather than alarming and
    /// is reported as such.
    pub allows_comments: bool,
}

/// The result of reading one source.
#[derive(Debug, Clone)]
pub struct Scan {
    pub source: Source,
    /// Servers found, or the reason the file could not be read.
    pub result: Result<Vec<Candidate>, String>,
}

impl Scan {
    /// Candidates found, or an empty slice when the file could not be read.
    pub fn candidates(&self) -> &[Candidate] {
        self.result.as_deref().unwrap_or(&[])
    }
}

/// Where to look. Everything is derived from injected roots rather than read
/// from the environment, so the whole table is testable against tempdirs.
///
/// Two config roots, not one, because the harnesses genuinely disagree. Claude
/// Desktop and VS Code follow the OS convention (`~/Library/Application Support`
/// on macOS, `%APPDATA%` on Windows, `~/.config` on Linux) — that is
/// [`Self::os_config`]. Zed and OpenCode use an XDG-style `~/.config` on macOS
/// *as well as* Linux — that is [`Self::xdg_config`]. Collapsing them would
/// silently look in the wrong place for half the table on macOS.
#[derive(Debug, Clone)]
pub struct Roots {
    /// The user's home directory.
    pub home: PathBuf,
    /// The OS config directory (`dirs::config_dir`).
    pub os_config: PathBuf,
    /// The XDG-style config directory: `$XDG_CONFIG_HOME`, else `~/.config`.
    pub xdg_config: PathBuf,
    /// The current working directory, for project-scoped files.
    pub cwd: PathBuf,
}

impl Roots {
    /// Resolve both config roots from a home directory and the OS convention.
    ///
    /// `$XDG_CONFIG_HOME` is honoured when set, since a user who sets it means
    /// it. On Windows there is no `~/.config` convention at all, so the XDG
    /// root falls back to the OS one.
    pub fn new(home: PathBuf, os_config: PathBuf, cwd: PathBuf) -> Self {
        Self {
            xdg_config: xdg_config_root(&home, &os_config),
            home,
            os_config,
            cwd,
        }
    }
}

/// The XDG-style config root: `$XDG_CONFIG_HOME` if set, else the
/// platform default.
fn xdg_config_root(home: &Path, os_config: &Path) -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => default_xdg_config_root(home, os_config),
    }
}

/// Platform default for the XDG-style root, with no `$XDG_CONFIG_HOME` set.
///
/// macOS and Linux both use `~/.config` here — that is the whole reason this
/// root is separate from `dirs::config_dir()`, which on macOS points at
/// Application Support. Windows has no such convention, so tools that would use
/// it there fall back to the OS config directory.
#[cfg(not(windows))]
fn default_xdg_config_root(home: &Path, _os_config: &Path) -> PathBuf {
    home.join(".config")
}

#[cfg(windows)]
fn default_xdg_config_root(_home: &Path, os_config: &Path) -> PathBuf {
    os_config.to_path_buf()
}

/// Every harness config file Leviath knows about, whether or not it exists.
///
/// Ordering is the order the wizard shows them in: the harnesses most likely to
/// be present first.
pub fn known_sources(roots: &Roots) -> Vec<Source> {
    let home = &roots.home;
    let os_config = &roots.os_config;
    let xdg = &roots.xdg_config;
    let cwd = &roots.cwd;

    vec![
        Source {
            id: "claude-code",
            display: "Claude Code",
            path: home.join(".claude.json"),
            layout: Layout::ClaudeCode,
            allows_comments: false,
        },
        Source {
            id: "claude-code-project",
            display: "Claude Code (this directory)",
            path: cwd.join(".mcp.json"),
            layout: Layout::JsonObject("mcpServers"),
            allows_comments: false,
        },
        Source {
            id: "claude-desktop",
            display: "Claude Desktop",
            path: claude_desktop_path(os_config),
            layout: Layout::JsonObject("mcpServers"),
            allows_comments: false,
        },
        Source {
            id: "codex",
            display: "Codex",
            path: home.join(".codex").join("config.toml"),
            layout: Layout::CodexToml,
            allows_comments: false,
        },
        Source {
            id: "opencode",
            display: "OpenCode",
            path: xdg.join("opencode").join("opencode.json"),
            layout: Layout::JsonObject("mcp"),
            allows_comments: false,
        },
        Source {
            id: "opencode-home",
            display: "OpenCode (home)",
            path: home.join(".opencode.json"),
            layout: Layout::JsonObject("mcp"),
            allows_comments: false,
        },
        Source {
            id: "gemini-cli",
            display: "Gemini CLI",
            path: home.join(".gemini").join("settings.json"),
            layout: Layout::JsonObject("mcpServers"),
            allows_comments: false,
        },
        Source {
            id: "cursor",
            display: "Cursor",
            path: home.join(".cursor").join("mcp.json"),
            layout: Layout::JsonObject("mcpServers"),
            allows_comments: false,
        },
        Source {
            id: "cursor-project",
            display: "Cursor (this directory)",
            path: cwd.join(".cursor").join("mcp.json"),
            layout: Layout::JsonObject("mcpServers"),
            allows_comments: false,
        },
        Source {
            id: "vscode-project",
            display: "VS Code (this directory)",
            path: cwd.join(".vscode").join("mcp.json"),
            layout: Layout::JsonObject("servers"),
            allows_comments: true,
        },
        Source {
            id: "vscode",
            display: "VS Code",
            path: vscode_user_path(os_config),
            layout: Layout::JsonObject("servers"),
            allows_comments: true,
        },
        Source {
            id: "windsurf",
            display: "Windsurf",
            path: home
                .join(".codeium")
                .join("windsurf")
                .join("mcp_config.json"),
            layout: Layout::JsonObject("mcpServers"),
            allows_comments: false,
        },
        Source {
            id: "zed",
            display: "Zed",
            path: xdg.join("zed").join("settings.json"),
            layout: Layout::JsonObject("context_servers"),
            allows_comments: true,
        },
    ]
}

// ── Platform-specific layouts ────────────────────────────────────────────────
//
// The per-OS differences in *where* these files live turn out to be entirely
// absorbed by `dirs::config_dir()`, which the caller supplies as `Roots.config`
// — `~/Library/Application Support` on macOS, `%APPDATA%` on Windows,
// `~/.config` on Linux. Both vendors below put their file at the same relative
// path under it on all three, so no `#[cfg]` is needed here. Writing out three
// identical arms would only claim a difference that does not exist; if one
// vendor ever diverges, *that* is when this grows a `cfg`.

/// Claude Desktop's config file: `<config>/Claude/claude_desktop_config.json`.
fn claude_desktop_path(config: &Path) -> PathBuf {
    config.join("Claude").join("claude_desktop_config.json")
}

/// VS Code's user-level `mcp.json`, beside `settings.json` in the per-user
/// profile directory: `<config>/Code/User/mcp.json`.
fn vscode_user_path(config: &Path) -> PathBuf {
    config.join("Code").join("User").join("mcp.json")
}

// ── Scanning ─────────────────────────────────────────────────────────────────

/// Parse one source's contents according to its layout.
fn parse(layout: Layout, contents: &str) -> anyhow::Result<Vec<Candidate>> {
    match layout {
        Layout::JsonObject(key) => formats::parse_json_object(contents, key),
        Layout::ClaudeCode => formats::parse_claude_code(contents),
        Layout::CodexToml => formats::parse_codex(contents),
    }
}

/// Read and parse every known source that exists.
///
/// Sources whose path does not exist are omitted entirely — on a clean machine
/// this returns nothing and the wizard's import step is one line of text.
///
/// Anything that *does* exist is kept even when it cannot be read or parsed,
/// carrying its error, so the user is told "Zed: couldn't read this" rather
/// than being quietly shown nothing and concluding Zed had no servers. The
/// filter is `exists`, not `is_file`, for exactly that reason: a directory
/// sitting where a config file belongs is a situation worth reporting, not one
/// to silently pretend is an absent harness.
pub fn scan(roots: &Roots) -> Vec<Scan> {
    known_sources(roots)
        .into_iter()
        .filter(|s| s.path.exists())
        .map(|source| {
            let result = std::fs::read_to_string(&source.path)
                .map_err(|e| e.to_string())
                .and_then(|contents| {
                    parse(source.layout, &contents).map_err(|e| describe_parse_error(&source, &e))
                });
            Scan { source, result }
        })
        .collect()
}

/// Turn a parse failure into something worth showing a user.
///
/// VS Code and Zed both allow comments and trailing commas in these files and
/// `serde_json` rejects both, so that failure is expected rather than a sign
/// anything is wrong. Saying so beats a raw `expected value at line 3 column 1`.
fn describe_parse_error(source: &Source, error: &anyhow::Error) -> String {
    if source.allows_comments {
        format!("{error} — this file may use comments, which aren't supported")
    } else {
        error.to_string()
    }
}

/// Whether `name` is already configured in Leviath.
pub fn already_configured(existing: &[leviath_mcp::MCPServerConfig], name: &str) -> bool {
    existing.iter().any(|s| s.name == name)
}

/// A name that does not collide with anything already configured, by appending
/// `-2`, `-3`, … Used when the user chooses to keep both.
pub fn dedup_name(existing: &[leviath_mcp::MCPServerConfig], name: &str) -> String {
    let mut candidate = name.to_string();
    let mut suffix = 1;
    while already_configured(existing, &candidate) {
        suffix += 1;
        candidate = format!("{name}-{suffix}");
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots_in(dir: &Path) -> Roots {
        Roots {
            home: dir.join("home"),
            os_config: dir.join("os-config"),
            xdg_config: dir.join("home").join(".config"),
            cwd: dir.join("cwd"),
        }
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().expect("test paths have a parent")).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    // ─── known_sources ──────────────────────────────────────────────────────

    #[test]
    fn every_known_source_has_a_distinct_id_and_a_nonempty_path() {
        let dir = tempfile::tempdir().unwrap();
        let sources = known_sources(&roots_in(dir.path()));

        assert!(sources.len() >= 9, "expected the full harness table");
        let mut ids: Vec<&str> = sources.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        let total = ids.len();
        ids.dedup();
        assert_eq!(total, ids.len(), "duplicate source ids");

        for source in &sources {
            assert!(
                !source.display.is_empty(),
                "source {} has no label",
                source.id
            );
            assert!(
                source.path.is_absolute(),
                "source {} has a relative path",
                source.id
            );
        }
    }

    #[test]
    fn known_sources_are_rooted_in_the_injected_directories() {
        // Nothing may reach past the roots it was handed -- that is what keeps
        // the scan testable and keeps it from wandering the real home dir.
        let dir = tempfile::tempdir().unwrap();
        let roots = roots_in(dir.path());

        for source in known_sources(&roots) {
            assert!(
                source.path.starts_with(&roots.home)
                    || source.path.starts_with(&roots.os_config)
                    || source.path.starts_with(&roots.xdg_config)
                    || source.path.starts_with(&roots.cwd),
                "source {} escaped the injected roots",
                source.id
            );
        }
    }

    #[test]
    fn zed_and_opencode_use_the_xdg_root_not_the_os_one() {
        // On macOS these two live under `~/.config`, while `dirs::config_dir()`
        // points at `~/Library/Application Support`. Reading them from the OS
        // root would silently find nothing on the most common desktop.
        let dir = tempfile::tempdir().unwrap();
        let roots = roots_in(dir.path());
        let sources = known_sources(&roots);

        for id in ["zed", "opencode"] {
            let source = sources
                .iter()
                .find(|s| s.id == id)
                .expect("source is in the table");
            assert!(
                source.path.starts_with(&roots.xdg_config),
                "{id} should read from the XDG root"
            );
        }
        for id in ["claude-desktop", "vscode"] {
            let source = sources
                .iter()
                .find(|s| s.id == id)
                .expect("source is in the table");
            assert!(
                source.path.starts_with(&roots.os_config),
                "{id} should read from the OS config root"
            );
        }
    }

    #[test]
    fn roots_new_honours_xdg_config_home_when_set() {
        temp_env::with_var("XDG_CONFIG_HOME", Some("/custom/xdg"), || {
            let roots = Roots::new(
                PathBuf::from("/home/u"),
                PathBuf::from("/home/u/os-config"),
                PathBuf::from("/work"),
            );
            assert_eq!(roots.xdg_config, PathBuf::from("/custom/xdg"));
        });
    }

    #[test]
    fn roots_new_falls_back_to_the_platform_default_without_xdg_config_home() {
        // An empty value counts as unset -- an exported-but-blank variable is
        // not a directory anyone meant to point at.
        for value in [None, Some("")] {
            temp_env::with_var("XDG_CONFIG_HOME", value, || {
                let roots = Roots::new(
                    PathBuf::from("/home/u"),
                    PathBuf::from("/home/u/os-config"),
                    PathBuf::from("/work"),
                );
                assert_eq!(
                    roots.xdg_config,
                    default_xdg_config_root(Path::new("/home/u"), Path::new("/home/u/os-config"))
                );
            });
        }
    }

    #[test]
    fn the_table_covers_every_layout() {
        let dir = tempfile::tempdir().unwrap();
        let sources = known_sources(&roots_in(dir.path()));

        assert!(sources.iter().any(|s| s.layout == Layout::ClaudeCode));
        assert!(sources.iter().any(|s| s.layout == Layout::CodexToml));
        assert!(
            sources
                .iter()
                .any(|s| matches!(s.layout, Layout::JsonObject("mcpServers")))
        );
        assert!(
            sources
                .iter()
                .any(|s| matches!(s.layout, Layout::JsonObject("servers")))
        );
        assert!(
            sources
                .iter()
                .any(|s| matches!(s.layout, Layout::JsonObject("mcp")))
        );
        assert!(
            sources
                .iter()
                .any(|s| matches!(s.layout, Layout::JsonObject("context_servers")))
        );
        assert!(sources.iter().any(|s| s.allows_comments));
    }

    // ─── scan ───────────────────────────────────────────────────────────────

    #[test]
    fn scanning_a_clean_machine_finds_nothing() {
        let dir = tempfile::tempdir().unwrap();

        assert!(scan(&roots_in(dir.path())).is_empty());
    }

    #[test]
    fn scan_reads_each_layout_it_finds() {
        let dir = tempfile::tempdir().unwrap();
        let roots = roots_in(dir.path());
        write(
            &roots.home.join(".claude.json"),
            r#"{"projects":{"/repo":{"mcpServers":{"cc":{"url":"https://cc.test"}}}}}"#,
        );
        write(
            &roots.home.join(".codex").join("config.toml"),
            "[mcp_servers.cx]\ncommand = \"cx\"\n",
        );
        write(
            &roots.cwd.join(".mcp.json"),
            r#"{"mcpServers":{"proj":{"command":"p"}}}"#,
        );

        let scans = scan(&roots);

        assert_eq!(scans.len(), 3);
        let names: Vec<&str> = scans
            .iter()
            .flat_map(|s| s.candidates())
            .map(|c| c.config.name.as_str())
            .collect();
        assert!(names.contains(&"cc"));
        assert!(names.contains(&"cx"));
        assert!(names.contains(&"proj"));
        // The Claude Code project scope rides along.
        let cc = scans
            .iter()
            .flat_map(|s| s.candidates())
            .find(|c| c.config.name == "cc")
            .expect("cc was found");
        assert_eq!(cc.scope, "/repo");
    }

    #[test]
    fn an_unreadable_source_is_reported_rather_than_silently_dropped() {
        // Showing nothing would read as "this harness had no servers", which is
        // a different and wrong claim.
        let dir = tempfile::tempdir().unwrap();
        let roots = roots_in(dir.path());
        write(&roots.home.join(".claude.json"), "not json at all");

        let scans = scan(&roots);

        assert_eq!(scans.len(), 1);
        assert!(scans[0].result.is_err());
        assert!(scans[0].candidates().is_empty());
    }

    #[test]
    fn a_jsonc_parse_failure_explains_the_comment_limitation() {
        let dir = tempfile::tempdir().unwrap();
        let roots = roots_in(dir.path());
        write(
            &roots.cwd.join(".vscode").join("mcp.json"),
            "{\n  // a comment VS Code allows\n  \"servers\": {}\n}",
        );

        let scans = scan(&roots);

        let message = scans[0]
            .result
            .as_ref()
            .expect_err("JSONC does not parse as JSON")
            .clone();
        assert!(message.contains("comments"), "unhelpful message: {message}");
    }

    #[test]
    fn a_directory_where_a_config_file_belongs_is_reported_as_unreadable() {
        // Not silently skipped: something is at that path, and "we could not
        // read your Claude Code config" is true where "you have none" is not.
        let dir = tempfile::tempdir().unwrap();
        let roots = roots_in(dir.path());
        std::fs::create_dir_all(roots.home.join(".claude.json")).unwrap();

        let scans = scan(&roots);

        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].source.id, "claude-code");
        assert!(scans[0].result.is_err());
        assert!(scans[0].candidates().is_empty());
    }

    // ─── name collisions ────────────────────────────────────────────────────

    #[test]
    fn already_configured_matches_by_name() {
        let existing = vec![leviath_mcp::MCPServerConfig::stdio("fs", "npx", vec![])];

        assert!(already_configured(&existing, "fs"));
        assert!(!already_configured(&existing, "other"));
    }

    #[test]
    fn dedup_name_leaves_a_free_name_alone() {
        let existing = vec![leviath_mcp::MCPServerConfig::stdio("fs", "npx", vec![])];

        assert_eq!(dedup_name(&existing, "other"), "other");
    }

    #[test]
    fn dedup_name_walks_past_every_taken_suffix() {
        let existing = vec![
            leviath_mcp::MCPServerConfig::stdio("fs", "npx", vec![]),
            leviath_mcp::MCPServerConfig::stdio("fs-2", "npx", vec![]),
            leviath_mcp::MCPServerConfig::stdio("fs-3", "npx", vec![]),
        ];

        assert_eq!(dedup_name(&existing, "fs"), "fs-4");
    }
}

//! What a profile says about one tool call.
//!
//! The decision sits between the configured policy and the grant check in the
//! dispatcher. It sees the policy the config layers resolved to (with a shell
//! redirect already clamped by `write_file`'s policy) and returns what the
//! profile makes of it, with a reason `lev yolo test` and the API can show.
//!
//! The rule the whole module follows: **a profile may loosen an `Ask` and may
//! tighten anything, but it never touches a configured `Deny`**, which stays
//! terminal exactly as it is under bare `--yolo`.

use std::path::{Path, PathBuf};

use crate::config::ToolPolicy;
use crate::shell_keys::{MatchableSegment, matchable_segments};
use leviath_core::blueprint::ToolGroup;

use super::YoloProfile;
use super::rules::{ShellRule, Verdict};

/// Where a called tool comes from, for `@group` matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolKind {
    Builtin,
    Subagent,
    Script,
    Mcp,
}

impl ToolKind {
    /// The group a tool of this kind belongs to.
    pub(crate) fn group(self) -> ToolGroup {
        match self {
            ToolKind::Builtin => ToolGroup::Builtin,
            ToolKind::Subagent => ToolGroup::Subagent,
            ToolKind::Script => ToolGroup::Scripts,
            ToolKind::Mcp => ToolGroup::Mcp,
        }
    }

    /// Classify a called name from what the dispatcher knows: the built-in
    /// set, and whether the name is a compiled script. Everything else came
    /// from an MCP server.
    pub(crate) fn classify(tool: &str, is_builtin: bool, is_script: bool) -> Self {
        if leviath_tools::is_subagent_tool(tool) {
            ToolKind::Subagent
        } else if is_builtin {
            ToolKind::Builtin
        } else if is_script {
            ToolKind::Script
        } else {
            ToolKind::Mcp
        }
    }
}

/// The platform facts path matching depends on, injected so both readings are
/// testable on either OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Platform {
    /// Windows path semantics: `\` becomes `/` before matching and globs are
    /// case-insensitive.
    pub windows: bool,
    /// Whether the shell reads `\` as an escape (`sh`) or a separator
    /// (`cmd.exe`).
    pub backslash_escapes: bool,
}

impl Platform {
    /// The platform this binary runs on.
    pub(crate) fn host() -> Self {
        Self {
            windows: cfg!(windows),
            backslash_escapes: crate::shell_keys::BACKSLASH_ESCAPES,
        }
    }

    fn match_options(self) -> glob::MatchOptions {
        glob::MatchOptions {
            case_sensitive: !self.windows,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        }
    }
}

/// Everything a decision needs about one call.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DecideInput<'a> {
    /// The name the model called.
    pub tool: &'a str,
    /// The call's arguments; only `command` is read, and only for the shell.
    pub arguments: &'a serde_json::Value,
    /// What the config layers resolved to, after the effect clamp.
    pub configured: ToolPolicy,
    /// Whether `--allow` on the command line named this tool. A launch flag
    /// is the most specific thing the person said, so an `ask` list does not
    /// override it; a `deny` list still does.
    pub launch_allowed: bool,
    pub kind: ToolKind,
    /// The run's working directory, which relative paths resolve against.
    pub workdir: &'a Path,
    /// What `~` expands to.
    pub home: Option<&'a Path>,
    pub platform: Platform,
}

impl<'a> DecideInput<'a> {
    /// The input for a real call in a real run: `~` is the user's home and the
    /// platform is this binary's.
    pub(crate) fn for_run(
        tool: &'a str,
        arguments: &'a serde_json::Value,
        configured: ToolPolicy,
        launch_allowed: bool,
        kind: ToolKind,
        workdir: &'a Path,
        home: Option<&'a Path>,
    ) -> Self {
        Self {
            tool,
            arguments,
            configured,
            launch_allowed,
            kind,
            workdir,
            home,
            platform: Platform::host(),
        }
    }
}

/// What the profile decided, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decision {
    pub policy: ToolPolicy,
    /// One line naming the rule or default that produced the policy.
    pub reason: String,
}

impl YoloProfile {
    /// The profile's verdict on one call.
    pub(crate) fn decide(&self, input: &DecideInput<'_>) -> Decision {
        let command = (leviath_tools::canonical_tool_name(input.tool) == "shell")
            .then(|| input.arguments.get("command").and_then(|v| v.as_str()))
            .flatten();
        let (verdict, reason) = match command {
            Some(command) => self.shell_verdict(command, input),
            None => self.tool_verdict(
                input.tool,
                input.kind.group(),
                input.configured,
                input.launch_allowed,
            ),
        };
        Decision {
            policy: verdict.policy(),
            reason,
        }
    }

    /// The verdict for a tool by name: the lists, then what the config said,
    /// then the profile's default.
    fn tool_verdict(
        &self,
        tool: &str,
        source: ToolGroup,
        configured: ToolPolicy,
        launch_allowed: bool,
    ) -> (Verdict, String) {
        if configured == ToolPolicy::Deny {
            return (
                Verdict::Deny,
                "the config denies it; no profile lifts a deny".to_string(),
            );
        }
        for (verdict, rule) in &self.compiled.tools {
            if !rule.matches(tool, source) {
                continue;
            }
            if *verdict == Verdict::Ask && launch_allowed {
                // `--allow` named it: the person already answered this one.
                continue;
            }
            return (
                *verdict,
                format!("tools {verdict} list entry {:?}", rule.entry),
            );
        }
        if configured == ToolPolicy::Allow {
            return (Verdict::Allow, "the config already allows it".to_string());
        }
        let default = self.spec.default.verdict();
        (default, format!("profile default ({default})"))
    }

    /// The verdict for a shell line: each command takes the first rule that
    /// matches it, or the tool-level verdict for `shell`; the line takes the
    /// strictest of its commands.
    fn shell_verdict(&self, command: &str, input: &DecideInput<'_>) -> (Verdict, String) {
        let (base, base_reason) = self.tool_verdict(
            "shell",
            ToolGroup::Builtin,
            input.configured,
            input.launch_allowed,
        );
        if base == Verdict::Deny {
            return (base, base_reason);
        }
        let has_rules = !self.compiled.shell.is_empty();
        let Some(segments) = matchable_segments(command, input.platform.backslash_escapes) else {
            return match has_rules {
                true => (
                    Verdict::Ask,
                    "the line cannot be read ahead of time, so the shell rules cannot be checked"
                        .to_string(),
                ),
                false => (base, base_reason),
            };
        };
        // A redirect is a file write no program name describes, so a command
        // that carries one also answers to what the profile says about
        // `write_file`.
        let write = segments.iter().any(|s| s.writes).then(|| {
            self.tool_verdict(
                "write_file",
                ToolGroup::Builtin,
                input.configured,
                input.launch_allowed,
            )
        });
        let mut line: Option<(Verdict, String)> = None;
        for segment in &segments {
            let mut found = self.segment_verdict(segment, has_rules, (base, &base_reason), input);
            if let (true, Some((wv, wr))) = (segment.writes, &write)
                && wv.stricter(found.0) == *wv
                && *wv != found.0
            {
                found = (*wv, format!("redirect takes write_file's verdict: {wr}"));
            }
            line = Some(match line {
                Some(current) if current.0.stricter(found.0) == current.0 => current,
                _ => found,
            });
        }
        line.unwrap_or((base, base_reason))
    }

    /// One command's verdict.
    fn segment_verdict(
        &self,
        segment: &MatchableSegment,
        has_rules: bool,
        base: (Verdict, &str),
        input: &DecideInput<'_>,
    ) -> (Verdict, String) {
        if segment.opaque {
            return match has_rules {
                true => (
                    Verdict::Ask,
                    "a command binds a variable, expands a word, or installs code, so the shell \
                     rules cannot vouch for it"
                        .to_string(),
                ),
                false => (base.0, base.1.to_string()),
            };
        }
        for (verdict, rule) in &self.compiled.shell {
            if rule_matches(rule, &segment.words, input) {
                return (*verdict, format!("shell {verdict} rule {:?}", rule.entry));
            }
        }
        (base.0, base.1.to_string())
    }
}

/// Whether `rule` covers a command spelled as `words`.
fn rule_matches(rule: &ShellRule, words: &[String], input: &DecideInput<'_>) -> bool {
    if words.len() < rule.command.len() {
        return false;
    }
    let options = input.platform.match_options();
    let leading = rule
        .command
        .iter()
        .zip(words)
        .all(|(pattern, word)| pattern.matches_with(word, options));
    if !leading {
        return false;
    }
    let Some(args) = &rule.args else {
        return true;
    };
    words[rule.command.len()..]
        .iter()
        .all(|word| args.iter().any(|pattern| arg_matches(pattern, word, input)))
}

/// Whether one remaining word satisfies one `args` pattern.
///
/// A flag is matched as text. Anything else is a path, matched only as the
/// path it resolves to: `~` expanded, joined to the workdir, `..` folded, and
/// canonicalised where it exists, so `~/scratch/../.ssh` and a symlink out of
/// `~/scratch` do not pass `~/scratch/**`. The pattern is expanded the same
/// way, so a relative pattern means "under the workdir".
fn arg_matches(pattern: &str, word: &str, input: &DecideInput<'_>) -> bool {
    let options = input.platform.match_options();
    if word.starts_with('-') {
        return glob::Pattern::new(pattern).is_ok_and(|p| p.matches_with(word, options));
    }
    let Some(resolved) = resolve_word(word, input.workdir, input.home) else {
        return false;
    };
    let Some(expanded) = expand_pattern(pattern, input.workdir, input.home) else {
        return false;
    };
    // The pattern through the same resolution as the word, as far as it
    // exists: its glob tail does not, and is re-appended as written, but the
    // directory it hangs off may be a symlink (`/tmp` on macOS), and a word
    // resolved to `/private/tmp/...` must still meet a pattern written as
    // `/tmp/...`. A pattern nothing along which exists stays as written and
    // matches nothing real.
    let expanded = leviath_core::canonicalize_for_match(&expanded).unwrap_or(expanded);
    let pattern_text = leviath_core::read_paths::normalize_match_str(
        &expanded.to_string_lossy(),
        input.platform.windows,
    );
    let word_text = leviath_core::read_paths::normalize_match_str(
        &resolved.to_string_lossy(),
        input.platform.windows,
    );
    glob::Pattern::new(&pattern_text).is_ok_and(|p| p.matches_with(&word_text, options))
}

/// The path a word names, verified as far as the filesystem allows. `None`
/// when `~` has nowhere to expand to, the path climbs past its root, or
/// nothing along it can be canonicalised.
fn resolve_word(word: &str, workdir: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let expanded = expand_home(word, home)?;
    let joined = match expanded.is_absolute() {
        true => expanded,
        false => workdir.join(expanded),
    };
    let folded = fold_dot_dot(&joined)?;
    leviath_core::canonicalize_for_match(&folded)
}

/// A pattern with `~` and the workdir applied, so it is in the same space as
/// a resolved word.
fn expand_pattern(pattern: &str, workdir: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let expanded = expand_home(pattern, home)?;
    Some(match expanded.is_absolute() {
        true => expanded,
        false => workdir.join(expanded),
    })
}

/// `~` and `~/rest` against `home`; anything else unchanged.
fn expand_home(text: &str, home: Option<&Path>) -> Option<PathBuf> {
    if text == "~" {
        return home.map(Path::to_path_buf);
    }
    match text.strip_prefix("~/") {
        Some(rest) => home.map(|h| h.join(rest)),
        None => Some(PathBuf::from(text)),
    }
}

/// Fold `.` and `..` lexically. `None` when a `..` would climb past the root,
/// which is a path that names nothing a rule should vouch for.
fn fold_dot_dot(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // Nothing to climb out of: the root, or the start of a
                // relative path.
                _ => return None,
            },
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

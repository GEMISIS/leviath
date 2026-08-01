//! The `[read_paths]` allowlist: how an agent is granted read access outside
//! its workdir, and how each access is checked.
//!
//! A blueprint's `[read_paths] allow` array *declares* what the agent wants to
//! read. Declaring is not granting: the user's config must either name the
//! same paths (`[security] read_paths` / `[agent_read_paths.<name>]`) or set
//! `allow_blueprint_read_paths = true`. That keeps the manifest tighten-only -
//! an `agent.leviath` someone downloaded cannot ship one TOML line that reads
//! `~/.ssh`. [`ReadPathPolicy::decide`] is that double check, applied per path
//! at resolve time.
//!
//! Three entry forms:
//! - an exact path: grants the whole subtree under it, checked with the same
//!   canonicalize-then-prefix containment as the workdir sandbox
//! - `glob:` - a glob pattern, `*` stays inside one path component, `**`
//!   crosses them
//! - `regex:` - a regex, auto-anchored as `^(?:pattern)$` so `regex:runs`
//!   cannot quietly match `/etc/runs-anything`
//!
//! Patterns match the **symlink-resolved real path** of the file, never the
//! path the agent asked for. That is what makes them safe: a symlink planted
//! inside an allowlisted directory resolves to its real target, and the real
//! target must itself match an entry. The matched string uses `/` separators
//! on every OS and, on Windows, has the `\\?\` verbatim prefix stripped and is
//! compared case-insensitively (see [`normalize_match_str`]).
//!
//! Portability: `~/` expands to the home directory (honoring `LEVIATH_HOME`),
//! and a bare relative entry resolves against the run's workdir. A relative
//! `regex:` is refused - there is no way to splice a workdir into a regex
//! safely, and `glob:` covers that case.

use std::path::{Path, PathBuf};

/// One compiled allowlist entry. Only [`ReadPathSet`] constructs these; the
/// enum is public so a set's contents are inspectable, not so callers build
/// entries by hand (compilation is where `~`/relative resolution and
/// anchoring happen).
#[derive(Debug, Clone)]
pub enum ReadPathEntry {
    /// An exact root: grants the subtree under it. Stored as resolved at
    /// compile time (tilde/workdir applied) but *uncanonicalized* - the root
    /// is canonicalized at match time so a root created after spawn still
    /// works, and a root that cannot be verified never matches.
    Exact(PathBuf),
    /// A glob over the normalized real path.
    Glob {
        /// The compiled pattern, already `/`-separated and prefixed with the
        /// escaped home or workdir when the source entry was `~/` or relative.
        pattern: glob::Pattern,
        /// Match options: `require_literal_separator` always, case sensitivity
        /// per platform semantics.
        options: glob::MatchOptions,
    },
    /// A regex over the normalized real path, anchored at compile time.
    Regex(regex::Regex),
}

impl ReadPathEntry {
    /// Whether the already-canonicalized `canonical` path (and its
    /// pre-normalized string form) lands inside this entry.
    fn matches(&self, canonical: &Path, normalized: &str) -> bool {
        match self {
            ReadPathEntry::Exact(root) => match std::fs::canonicalize(root) {
                Ok(real_root) => canonical.starts_with(&real_root),
                // The root itself cannot be verified (it does not exist, or a
                // parent is unreadable). `canonical` exists - it was
                // canonicalized by the caller - so it cannot really live under
                // an unverifiable root. Refuse.
                Err(_) => false,
            },
            ReadPathEntry::Glob { pattern, options } => pattern.matches_with(normalized, *options),
            ReadPathEntry::Regex(re) => re.is_match(normalized),
        }
    }
}

/// Normalize a canonicalized path string for glob/regex matching.
///
/// With `windows` set (production: `cfg!(windows)`, injected so both branches
/// are testable everywhere):
/// - `\\?\UNC\server\share\..` becomes `\\server\share\..`
/// - `\\?\C:\..` (a drive-letter verbatim path, which is what
///   `fs::canonicalize` returns on Windows) loses the `\\?\` prefix
/// - any other `\\?\` form (`\\?\Volume{..}`) is left alone; such a path
///   simply never matches a drive-letter pattern, which fails closed
/// - every `\` becomes `/`, so patterns are written with `/` on every OS
///
/// Without it the string is returned unchanged - a Unix filename may legally
/// contain `\`.
pub fn normalize_match_str(s: &str, windows: bool) -> String {
    if !windows {
        return s.to_string();
    }
    let stripped = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            rest.to_string()
        } else {
            s.to_string()
        }
    } else {
        s.to_string()
    };
    stripped.replace('\\', "/")
}

/// A compiled set of allowlist entries, bound to the run they were compiled
/// for (tilde and relative entries were resolved at compile time).
#[derive(Debug, Clone, Default)]
pub struct ReadPathSet {
    entries: Vec<ReadPathEntry>,
    /// Windows path semantics for matching (separator normalization and case
    /// folding). Injected rather than read from `cfg!` inside so every branch
    /// runs under test on every OS.
    windows: bool,
}

impl ReadPathSet {
    /// Compile raw `[read_paths] allow` strings against a run's workdir and
    /// home. `windows` selects Windows path semantics: `/`-normalization of
    /// the matched string and case-insensitive glob/regex matching
    /// (production passes `cfg!(windows)`).
    ///
    /// Any invalid entry is a hard error naming the entry - a skipped entry
    /// would degrade the agent silently mid-run, and refusing loudly at
    /// compile (spawn) time is the same posture the sandbox config takes.
    pub fn compile(
        raw: &[String],
        workdir: &Path,
        home: Option<&Path>,
        windows: bool,
    ) -> Result<Self, String> {
        let entries = raw
            .iter()
            .map(|entry| compile_entry(entry, workdir, home, windows))
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self { entries, windows })
    }

    /// Whether the set has no entries at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The compiled entries, for callers that report or display them.
    pub fn entries(&self) -> &[ReadPathEntry] {
        &self.entries
    }

    /// Whether the already-canonicalized `canonical` path matches any entry.
    pub fn matches(&self, canonical: &Path) -> bool {
        let normalized = normalize_match_str(&canonical.to_string_lossy(), self.windows);
        self.entries
            .iter()
            .any(|e| e.matches(canonical, &normalized))
    }
}

/// The outcome of checking one path against a [`ReadPathPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadPathDecision {
    /// Declared by the blueprint and granted by the user (or the user opted
    /// into honoring blueprints wholesale).
    Allowed,
    /// The blueprint never asked for this path; the ordinary workdir refusal
    /// stands.
    NotDeclared,
    /// The blueprint asked, but nothing in the user's config grants it.
    NotGranted,
}

/// Everything read-path enforcement needs, resolved once at spawn.
///
/// `blueprint` is what the manifest declares; `grants` is what the user's
/// config allows (`[security] read_paths` plus `[agent_read_paths.<name>]`);
/// `allow_blueprint` is the `[security] allow_blueprint_read_paths` override
/// that honors declarations without itemized grants.
#[derive(Debug, Clone, Default)]
pub struct ReadPathPolicy {
    /// The agent's name, for error and warning text.
    pub agent: String,
    /// Entries the blueprint declares.
    pub blueprint: ReadPathSet,
    /// Entries the user's config grants.
    pub grants: ReadPathSet,
    /// Whether declarations are honored without itemized grants.
    pub allow_blueprint: bool,
}

impl ReadPathPolicy {
    /// A policy that allows nothing beyond the workdir - the default for
    /// every agent whose blueprint has no `[read_paths]`.
    pub fn inactive() -> Self {
        Self::default()
    }

    /// Whether the blueprint declares any read paths at all. When false, the
    /// resolver never consults this policy and the workdir sandbox behaves
    /// exactly as it always has.
    pub fn is_active(&self) -> bool {
        !self.blueprint.is_empty()
    }

    /// The double check: the blueprint must declare the path AND the user
    /// must grant it (itemized, or via the blanket override).
    pub fn decide(&self, canonical: &Path) -> ReadPathDecision {
        if !self.blueprint.matches(canonical) {
            return ReadPathDecision::NotDeclared;
        }
        if self.allow_blueprint || self.grants.matches(canonical) {
            ReadPathDecision::Allowed
        } else {
            ReadPathDecision::NotGranted
        }
    }
}

/// Validate one entry's syntax without binding it to a run: bad glob/regex,
/// a relative `regex:`, an empty entry. Called from manifest parsing so a
/// broken entry fails `lev validate`/`lev add`/spawn loudly instead of
/// degrading the agent at its first out-of-workdir read.
///
/// Environment problems (`~` with no resolvable home) are not syntax and are
/// only caught when the real compile runs at spawn.
pub fn validate_entry_syntax(raw: &str) -> Result<(), String> {
    // The dummy workdir is deep enough that a reasonable `../` prefix in a
    // relative glob validates; how far up a real run can climb is bound to the
    // real workdir at spawn.
    let workdir = Path::new("/validate/a/b/c/d/e/f/g/h");
    compile_entry(raw, workdir, Some(Path::new("/validate-home")), false).map(|_| ())
}

/// Compile one raw entry. `windows` is the same injected platform-semantics
/// flag as [`ReadPathSet::compile`].
fn compile_entry(
    raw: &str,
    workdir: &Path,
    home: Option<&Path>,
    windows: bool,
) -> Result<ReadPathEntry, String> {
    if raw.trim().is_empty() {
        return Err("read_paths entry is empty".to_string());
    }
    if let Some(rest) = raw.strip_prefix("regex:") {
        compile_regex(raw, rest, home, windows)
    } else if let Some(rest) = raw.strip_prefix("glob:") {
        compile_glob(raw, rest, workdir, home, windows)
    } else {
        compile_exact(raw, workdir, home)
    }
}

/// Whether pattern text starts like an absolute path: `/..`, `//server/..`,
/// or a drive letter `C:/..`. Deliberately literal - a pattern opening with a
/// character class (`[A-Z]:/..`) is refused rather than guessed at.
fn absolute_shaped(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.starts_with('/')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

fn compile_regex(
    raw: &str,
    rest: &str,
    home: Option<&Path>,
    windows: bool,
) -> Result<ReadPathEntry, String> {
    if rest.is_empty() {
        return Err(format!("read_paths entry '{raw}': regex pattern is empty"));
    }
    let body = if let Some(after_tilde) = rest.strip_prefix('~') {
        if !(after_tilde.is_empty() || after_tilde.starts_with('/')) {
            return Err(format!(
                "read_paths entry '{raw}': only '~/' home expansion is supported"
            ));
        }
        let home = home.ok_or_else(|| {
            format!("read_paths entry '{raw}': no home directory resolved for '~' expansion")
        })?;
        let prefix = regex::escape(&normalize_match_str(&home.to_string_lossy(), windows));
        format!("{prefix}{after_tilde}")
    } else if absolute_shaped(rest) {
        rest.to_string()
    } else {
        return Err(format!(
            "read_paths entry '{raw}': regex entries must start with '/', a drive letter, or '~/'; \
             use 'glob:' for workdir-relative patterns"
        ));
    };
    regex::RegexBuilder::new(&format!("^(?:{body})$"))
        .case_insensitive(windows)
        .build()
        .map(ReadPathEntry::Regex)
        .map_err(|e| format!("read_paths entry '{raw}': invalid regex: {e}"))
}

fn compile_glob(
    raw: &str,
    rest: &str,
    workdir: &Path,
    home: Option<&Path>,
    windows: bool,
) -> Result<ReadPathEntry, String> {
    if rest.is_empty() {
        return Err(format!("read_paths entry '{raw}': glob pattern is empty"));
    }
    // Glob has no backslash-escape syntax, so this is lossless and makes
    // Windows-style patterns portable.
    let text = rest.replace('\\', "/");
    let text = if let Some(after_tilde) = text.strip_prefix('~') {
        if !(after_tilde.is_empty() || after_tilde.starts_with('/')) {
            return Err(format!(
                "read_paths entry '{raw}': only '~/' home expansion is supported"
            ));
        }
        let home = home.ok_or_else(|| {
            format!("read_paths entry '{raw}': no home directory resolved for '~' expansion")
        })?;
        // The home directory is data, not pattern: escape any glob
        // metacharacters it happens to contain.
        let prefix = glob::Pattern::escape(&normalize_match_str(&home.to_string_lossy(), windows));
        format!("{prefix}{after_tilde}")
    } else if absolute_shaped(&text) {
        text
    } else {
        resolve_relative_glob(raw, &text, workdir, windows)?
    };
    // A `.` or `..` component anywhere in the final pattern can never match a
    // canonicalized path, so it is always a mistake - refuse it rather than
    // let the entry silently match nothing.
    if text.split('/').any(|c| c == "." || c == "..") {
        return Err(format!(
            "read_paths entry '{raw}': glob patterns cannot contain '.' or '..' components \
             (relative entries fold them against the workdir at the start only)"
        ));
    }
    let pattern = glob::Pattern::new(&text)
        .map_err(|e| format!("read_paths entry '{raw}': invalid glob: {e}"))?;
    let options = glob::MatchOptions {
        case_sensitive: !windows,
        // `*` must not cross a `/`; `**` is the explicit way to.
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    Ok(ReadPathEntry::Glob { pattern, options })
}

/// Anchor a relative glob at the workdir, folding any *leading* `./` and
/// `../` components into the workdir prefix so `glob:../shared/**` means the
/// workdir's sibling.
fn resolve_relative_glob(
    raw: &str,
    text: &str,
    workdir: &Path,
    windows: bool,
) -> Result<String, String> {
    let base_str = normalize_match_str(&workdir.to_string_lossy(), windows);
    let mut base: Vec<&str> = base_str.split('/').collect();
    // "/" splits to ["", ""]; keep the leading "" (it restores the root `/`
    // on rejoin) and drop trailing empties.
    while base.len() > 1 && base.last().is_some_and(|s| s.is_empty()) {
        base.pop();
    }
    let mut rest = text;
    loop {
        if let Some(r) = rest.strip_prefix("./") {
            rest = r;
        } else if let Some(r) = rest.strip_prefix("../") {
            if base.len() <= 1 {
                return Err(format!(
                    "read_paths entry '{raw}': relative pattern escapes the filesystem root"
                ));
            }
            base.pop();
            rest = r;
        } else {
            break;
        }
    }
    // The workdir is data, not pattern.
    let prefix = glob::Pattern::escape(&base.join("/"));
    Ok(if rest.is_empty() {
        prefix
    } else {
        format!("{prefix}/{rest}")
    })
}

fn compile_exact(raw: &str, workdir: &Path, home: Option<&Path>) -> Result<ReadPathEntry, String> {
    let path = if let Some(after_tilde) = raw.strip_prefix('~') {
        let sub = after_tilde
            .strip_prefix('/')
            .or_else(|| after_tilde.strip_prefix('\\'));
        let sub = match (sub, after_tilde.is_empty()) {
            (_, true) => "",
            (Some(sub), _) => sub,
            (None, false) => {
                return Err(format!(
                    "read_paths entry '{raw}': only '~/' home expansion is supported"
                ));
            }
        };
        let home = home.ok_or_else(|| {
            format!("read_paths entry '{raw}': no home directory resolved for '~' expansion")
        })?;
        if sub.is_empty() {
            home.to_path_buf()
        } else {
            home.join(sub)
        }
    } else if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        workdir.join(raw)
    };
    Ok(ReadPathEntry::Exact(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(entries: &[&str], workdir: &str, home: Option<&str>, windows: bool) -> ReadPathSet {
        let raw: Vec<String> = entries.iter().map(|s| s.to_string()).collect();
        ReadPathSet::compile(&raw, Path::new(workdir), home.map(Path::new), windows)
            .expect("entries compile")
    }

    fn compile_err(entry: &str, workdir: &str, home: Option<&str>) -> String {
        ReadPathSet::compile(
            &[entry.to_string()],
            Path::new(workdir),
            home.map(Path::new),
            false,
        )
        .expect_err("entry must be refused")
    }

    // -- compile errors ----------------------------------------------------

    #[test]
    fn empty_and_whitespace_entries_are_refused() {
        assert!(compile_err("", "/w", None).contains("empty"));
        assert!(compile_err("   ", "/w", None).contains("empty"));
        assert!(compile_err("glob:", "/w", None).contains("glob pattern is empty"));
        assert!(compile_err("regex:", "/w", None).contains("regex pattern is empty"));
    }

    #[test]
    fn invalid_patterns_are_refused() {
        assert!(compile_err("glob:/a/[", "/w", None).contains("invalid glob"));
        assert!(compile_err("regex:/a/(", "/w", None).contains("invalid regex"));
    }

    /// There is no safe way to splice a workdir into a regex, so a relative
    /// regex is a hard error pointing at glob.
    #[test]
    fn a_relative_regex_is_refused() {
        let err = compile_err("regex:etc/passwd", "/w", None);
        assert!(err.contains("must start with"), "got: {err}");
        assert!(err.contains("glob:"), "got: {err}");
    }

    /// `~user` expansion is not supported in any entry kind - only `~/`.
    #[test]
    fn tilde_user_forms_are_refused() {
        for entry in ["~other/x", "glob:~other/**", "regex:~other/.*"] {
            let err = compile_err(entry, "/w", Some("/home/me"));
            assert!(err.contains("only '~/'"), "{entry}: {err}");
        }
    }

    /// `~` without a resolvable home is an environment error at compile time,
    /// for every entry kind.
    #[test]
    fn tilde_without_a_home_is_refused() {
        for entry in ["~/docs", "glob:~/docs/**", "regex:~/docs/.*"] {
            let err = compile_err(entry, "/w", None);
            assert!(err.contains("no home directory"), "{entry}: {err}");
        }
    }

    /// A dot component in the middle of a glob can never match a canonical
    /// path, so it is refused instead of silently matching nothing.
    #[test]
    fn interior_dot_components_in_globs_are_refused() {
        for entry in ["glob:/a/../b/**", "glob:/a/./b", "glob:a/../../b"] {
            let err = compile_err(entry, "/w/x", None);
            assert!(err.contains("cannot contain"), "{entry}: {err}");
        }
    }

    #[test]
    fn a_relative_glob_cannot_climb_past_the_root() {
        let err = compile_err("glob:../../../x/**", "/w", None);
        assert!(err.contains("escapes the filesystem root"), "got: {err}");
    }

    // -- exact entries -----------------------------------------------------

    #[test]
    fn an_exact_root_grants_its_subtree_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/f.txt"), b"x").unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("f.txt");
        std::fs::write(&outside_file, b"x").unwrap();

        let s = set(&[root.to_str().unwrap()], "/w", None, false);
        assert!(s.matches(&root.join("sub/f.txt")));
        assert!(!s.matches(&std::fs::canonicalize(&outside_file).unwrap()));
    }

    /// The entry itself may be uncanonicalized (macOS `/tmp` vs
    /// `/private/tmp`); the root is canonicalized at match time.
    #[test]
    fn an_uncanonicalized_exact_root_still_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
        let s = set(&[dir.path().to_str().unwrap()], "/w", None, false);
        assert!(s.matches(&std::fs::canonicalize(dir.path().join("f.txt")).unwrap()));
    }

    /// A root that cannot be verified never matches - the canonicalized
    /// candidate exists, so it cannot really live under a nonexistent root.
    #[test]
    fn a_nonexistent_exact_root_never_matches() {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        let s = set(&["/definitely/not/a/real/root"], "/w", None, false);
        assert!(!s.matches(&real));
    }

    #[test]
    fn a_relative_exact_entry_resolves_against_the_workdir() {
        let parent = tempfile::tempdir().unwrap();
        let workdir = parent.path().join("work");
        let sibling = parent.path().join("shared");
        std::fs::create_dir_all(&workdir).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("doc.md"), b"x").unwrap();

        let s = set(&["../shared"], workdir.to_str().unwrap(), None, false);
        assert!(s.matches(&std::fs::canonicalize(sibling.join("doc.md")).unwrap()));
    }

    #[test]
    fn tilde_exact_entries_expand_to_the_home_argument() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("docs")).unwrap();
        std::fs::write(home.path().join("docs/a.md"), b"x").unwrap();
        let home_str = home.path().to_str().unwrap();

        let bare = set(&["~"], "/w", Some(home_str), false);
        let scoped = set(&["~/docs"], "/w", Some(home_str), false);
        let canonical = std::fs::canonicalize(home.path().join("docs/a.md")).unwrap();
        assert!(bare.matches(&canonical));
        assert!(scoped.matches(&canonical));
    }

    // -- glob entries (matching is pure string work, no filesystem) --------

    #[test]
    fn star_stays_within_one_component_and_doublestar_crosses() {
        let s = set(&["glob:/data/runs/*"], "/w", None, false);
        assert!(s.matches(Path::new("/data/runs/r1")));
        assert!(!s.matches(Path::new("/data/runs/r1/log.txt")));

        let deep = set(&["glob:/data/runs/**"], "/w", None, false);
        assert!(deep.matches(Path::new("/data/runs/r1/log.txt")));
        assert!(!deep.matches(Path::new("/data/other/x")));
    }

    #[test]
    fn a_relative_glob_is_anchored_at_the_workdir() {
        let s = set(&["glob:../shared/**"], "/w/agent", None, false);
        assert!(s.matches(Path::new("/w/shared/notes/a.md")));
        assert!(!s.matches(Path::new("/w/agent/own.md")));
        assert!(!s.matches(Path::new("/elsewhere/shared/a.md")));
    }

    /// A relative glob anchored at the filesystem root itself: the root's
    /// trailing-empty split segment must not double the separator.
    #[test]
    fn a_relative_glob_works_from_a_root_workdir() {
        let s = set(&["glob:docs/**"], "/", None, false);
        assert!(s.matches(Path::new("/docs/a.md")));
        assert!(!s.matches(Path::new("/other/a.md")));
    }

    /// A pattern that is nothing but dot components (`glob:../`) reduces to
    /// the folded prefix alone and matches exactly that directory.
    #[test]
    fn a_dots_only_glob_matches_the_folded_directory_itself() {
        let s = set(&["glob:../"], "/a/b", None, false);
        assert!(s.matches(Path::new("/a")));
        assert!(!s.matches(Path::new("/a/b")));
    }

    /// The workdir is data: glob metacharacters in it must match literally.
    #[test]
    fn a_metachar_workdir_is_escaped_in_relative_globs() {
        let s = set(&["glob:./docs/**"], "/we[ird]/w", None, false);
        assert!(s.matches(Path::new("/we[ird]/w/docs/a.md")));
        // If the workdir were spliced in unescaped, `[ird]` would be a class
        // and this single-character variant would match.
        assert!(!s.matches(Path::new("/wei/w/docs/a.md")));
    }

    /// The home directory is data too.
    #[test]
    fn a_metachar_home_is_escaped_in_tilde_globs() {
        let s = set(&["glob:~/docs/**"], "/w", Some("/ho[me]"), false);
        assert!(s.matches(Path::new("/ho[me]/docs/a.md")));
        assert!(!s.matches(Path::new("/hom/docs/a.md")));
    }

    /// Windows-style pattern text is normalized to `/` so blueprints written
    /// with backslashes keep working.
    #[test]
    fn backslash_glob_patterns_are_normalized() {
        let s = set(&[r"glob:C:\data\runs\**"], "/w", None, true);
        assert!(s.matches(Path::new(r"C:\data\runs\r1\log.txt")));
    }

    #[test]
    fn glob_case_sensitivity_follows_the_platform_flag() {
        let insensitive = set(&["glob:/Data/**"], "/w", None, true);
        assert!(insensitive.matches(Path::new("/data/x")));
        let sensitive = set(&["glob:/Data/**"], "/w", None, false);
        assert!(!sensitive.matches(Path::new("/data/x")));
    }

    // -- regex entries -----------------------------------------------------

    /// The anchor is the point: an unanchored `regex:/etc/runs` must not
    /// match `/etc/runs-anything` or `/prefix/etc/runs`.
    #[test]
    fn regexes_are_anchored_to_the_whole_path() {
        let s = set(&["regex:/etc/runs"], "/w", None, false);
        assert!(s.matches(Path::new("/etc/runs")));
        assert!(!s.matches(Path::new("/etc/runs-anything")));
        assert!(!s.matches(Path::new("/prefix/etc/runs")));

        let subtree = set(&["regex:/etc/runs/.*"], "/w", None, false);
        assert!(subtree.matches(Path::new("/etc/runs/deep/file")));
    }

    #[test]
    fn regex_case_sensitivity_follows_the_platform_flag() {
        let insensitive = set(&["regex:/Data/.*"], "/w", None, true);
        assert!(insensitive.matches(Path::new("/data/x")));
        let sensitive = set(&["regex:/Data/.*"], "/w", None, false);
        assert!(!sensitive.matches(Path::new("/data/x")));
    }

    /// The home is spliced in escaped, so a metacharacter in the home path
    /// matches itself and nothing else.
    #[test]
    fn a_metachar_home_is_escaped_in_tilde_regexes() {
        let s = set(&["regex:~/docs/.*"], "/w", Some("/ho.me"), false);
        assert!(s.matches(Path::new("/ho.me/docs/a")));
        assert!(!s.matches(Path::new("/hoXme/docs/a")));
    }

    /// A drive-letter regex is accepted as absolute-shaped.
    #[test]
    fn a_drive_letter_regex_is_accepted() {
        let s = set(&["regex:C:/data/.*"], "/w", None, true);
        assert!(s.matches(Path::new(r"C:\data\x")));
    }

    // -- normalize_match_str ----------------------------------------------

    #[test]
    fn unix_strings_pass_through_untouched() {
        assert_eq!(
            normalize_match_str(r"/a/weird\name", false),
            r"/a/weird\name"
        );
    }

    #[test]
    fn windows_verbatim_prefixes_are_stripped_for_matching() {
        assert_eq!(normalize_match_str(r"\\?\C:\Users\x", true), "C:/Users/x");
        assert_eq!(
            normalize_match_str(r"\\?\UNC\srv\share\x", true),
            "//srv/share/x"
        );
        // Unrecognized verbatim forms are left alone (they fail to match
        // drive-letter patterns, which is the safe direction).
        assert_eq!(
            normalize_match_str(r"\\?\Volume{abc}\x", true),
            "//?/Volume{abc}/x"
        );
        assert_eq!(normalize_match_str(r"C:\plain\x", true), "C:/plain/x");
    }

    // -- policy ------------------------------------------------------------

    fn policy(blueprint: &[&str], grants: &[&str], allow_blueprint: bool) -> ReadPathPolicy {
        ReadPathPolicy {
            agent: "tester".into(),
            blueprint: set(blueprint, "/w", None, false),
            grants: set(grants, "/w", None, false),
            allow_blueprint,
        }
    }

    #[test]
    fn an_inactive_policy_declares_nothing() {
        let p = ReadPathPolicy::inactive();
        assert!(!p.is_active());
        assert_eq!(
            p.decide(Path::new("/anything")),
            ReadPathDecision::NotDeclared
        );
    }

    #[test]
    fn a_path_the_blueprint_never_declared_is_not_declared() {
        let p = policy(&["glob:/data/**"], &["glob:/data/**"], false);
        assert!(p.is_active());
        assert_eq!(
            p.decide(Path::new("/etc/passwd")),
            ReadPathDecision::NotDeclared
        );
    }

    /// Declared but ungranted: the blueprint alone grants nothing. This is
    /// the tighten-only invariant.
    #[test]
    fn a_declared_but_ungranted_path_is_not_granted() {
        let p = policy(&["glob:/data/**"], &[], false);
        assert_eq!(p.decide(Path::new("/data/x")), ReadPathDecision::NotGranted);
    }

    #[test]
    fn a_granted_path_is_allowed() {
        let p = policy(&["glob:/data/**"], &["glob:/data/**"], false);
        assert_eq!(p.decide(Path::new("/data/x")), ReadPathDecision::Allowed);
    }

    /// The grant need not be textually identical - it is a second predicate,
    /// so a broad user grant covers a narrow blueprint declaration.
    #[test]
    fn a_broader_grant_covers_a_narrow_declaration() {
        let p = policy(&["glob:/data/runs/**"], &["glob:/data/**"], false);
        assert_eq!(
            p.decide(Path::new("/data/runs/r1")),
            ReadPathDecision::Allowed
        );
    }

    /// A grant that does not cover the declared path does nothing: both
    /// predicates must hold for the same path.
    #[test]
    fn a_nonoverlapping_grant_does_not_help() {
        let p = policy(&["glob:/data/**"], &["glob:/other/**"], false);
        assert_eq!(p.decide(Path::new("/data/x")), ReadPathDecision::NotGranted);
    }

    #[test]
    fn the_blanket_override_honors_declarations_without_grants() {
        let p = policy(&["glob:/data/**"], &[], true);
        assert_eq!(p.decide(Path::new("/data/x")), ReadPathDecision::Allowed);
        // The override does not widen beyond what is declared.
        assert_eq!(p.decide(Path::new("/etc/x")), ReadPathDecision::NotDeclared);
    }

    #[test]
    fn an_empty_set_matches_nothing() {
        let s = set(&[], "/w", None, false);
        assert!(s.is_empty());
        assert!(s.entries().is_empty());
        assert!(!s.matches(Path::new("/anything")));
    }

    // -- validate_entry_syntax --------------------------------------------

    #[test]
    fn syntax_validation_accepts_well_formed_entries() {
        for entry in [
            "/abs/dir",
            "relative/dir",
            "~/docs",
            "glob:~/runs/**",
            "glob:../shared/**",
            "regex:/data/.*",
            r"C:\Users\me\docs",
        ] {
            assert!(validate_entry_syntax(entry).is_ok(), "{entry}");
        }
    }

    #[test]
    fn syntax_validation_refuses_malformed_entries() {
        for entry in ["", "glob:[", "regex:(", "regex:relative/.*", "~oops"] {
            assert!(validate_entry_syntax(entry).is_err(), "{entry}");
        }
    }
}

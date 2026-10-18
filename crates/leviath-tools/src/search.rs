//! The search tools: `search_files` looks inside files, `glob` matches their
//! paths.
//!
//! Both run in-process over a directory walk, so nothing is executed, and both
//! hold every root and every file they meet to the fence `read_file` has: the
//! workdir, then `[read_paths]`, checked on the canonical path. A stage can
//! therefore be handed search without being handed `shell`, whose `grep`
//! reads any file the user can.

use super::*;
use std::fmt::Write as _;

/// Most of one file `search_files` reads. A longer file is searched in its
/// first this-many bytes, and the result says how many files that happened
/// to.
pub(crate) const MAX_SEARCH_FILE_BYTES: usize = 1024 * 1024;

/// Most text one search returns.
///
/// The same idea as the shell's capture cap, sized for a context window rather
/// than for memory: a search answer is read by the model in full, so a page of
/// matches is useful and a megabyte of them is not.
pub(crate) const MAX_SEARCH_OUTPUT_BYTES: usize = 64 * 1024;

/// Bytes looked at to decide a file is binary: a NUL among them means it is.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// Characters of one line shown before it is cut. Minified files are one line.
const MAX_LINE_CHARS: usize = 400;

/// `max_results` when the call leaves it out, and the most it may ask for.
const SEARCH_RESULTS: (u64, u64) = (200, 2000);

/// The same pair for `glob`.
const GLOB_RESULTS: (u64, u64) = (500, 5000);

/// The most `context_lines` a call may ask for.
const MAX_CONTEXT_LINES: u64 = 10;

/// [`BuiltinTools::resolve_read`] over explicit parts, so a walk running on
/// the blocking pool resolves every file it meets exactly as `read_file`
/// resolves one path: the workdir first, then `[read_paths]`.
///
/// `within` is the same seam [`crate::resolve_within`] takes, so a symlink refusal is
/// reachable from a test on every platform.
pub(crate) fn resolve_read_in(
    requested: &str,
    workdir: &Path,
    read_paths: &leviath_core::ReadPathPolicy,
    within: fn(&Path, &Path) -> bool,
) -> anyhow::Result<PathBuf> {
    match crate::resolve_within(requested, workdir, within) {
        Ok(path) => Ok(path),
        Err(workdir_err) => {
            if !read_paths.is_active() {
                return Err(workdir_err);
            }
            BuiltinTools::resolve_outside(
                requested,
                workdir,
                read_paths,
                leviath_core::canonicalize_for_match,
            )
        }
    }
}

/// What `resolve_read` answers from, owned so a walk on the blocking pool can
/// resolve every entry it meets.
pub(crate) struct ReadScope {
    /// The canonical workdir.
    pub(crate) workdir: PathBuf,
    /// The `[read_paths]` policy in force when the call started.
    pub(crate) read_paths: leviath_core::ReadPathPolicy,
    /// The canonical containment check; a seam so the refusal is testable on
    /// every platform, as in [`crate::resolve_within`].
    pub(crate) within: fn(&Path, &Path) -> bool,
}

impl ReadScope {
    /// The path `read_file` would read for `path`, or `None` when it would
    /// refuse: outside the workdir and not granted, including through a
    /// symlink that points out.
    fn confine(&self, path: &Path) -> Option<PathBuf> {
        resolve_read_in(
            &path.to_string_lossy(),
            &self.workdir,
            &self.read_paths,
            self.within,
        )
        .ok()
    }

    /// How a path is shown to the model: relative to the workdir with `/`
    /// separators, or in full for a `[read_paths]` location outside it.
    fn shown(&self, path: &Path) -> String {
        match path.strip_prefix(&self.workdir) {
            Ok(rel) => slash_path(rel),
            Err(_) => path.display().to_string(),
        }
    }
}

/// `path` with `/` between its parts on every platform, so a glob and the
/// model see one spelling.
fn slash_path(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// The caps a search runs under, injectable so a test reaches them with a few
/// bytes instead of a megabyte.
#[derive(Clone, Copy)]
pub(crate) struct Caps {
    /// Bytes of one file searched.
    pub(crate) file_bytes: usize,
    /// Bytes of output returned.
    pub(crate) output_bytes: usize,
}

impl Caps {
    /// The caps a real call runs under.
    pub(crate) const REAL: Self = Self {
        file_bytes: MAX_SEARCH_FILE_BYTES,
        output_bytes: MAX_SEARCH_OUTPUT_BYTES,
    };
}

/// What a `search_files` call asked for, past its pattern and root.
pub(crate) struct SearchOptions {
    /// Lines shown either side of a match.
    pub(crate) context: usize,
    /// Matching lines returned before the search stops.
    pub(crate) max_results: usize,
    /// Search files `.gitignore`, `.ignore` or a leading dot would hide.
    pub(crate) include_ignored: bool,
    /// Which files to search, by path.
    pub(crate) filter: Option<FileFilter>,
}

/// The `glob` argument of `search_files`: which files to look in.
pub(crate) struct FileFilter {
    matcher: globset::GlobMatcher,
    /// A pattern with no `/` is matched against the file name alone, the way
    /// a `.gitignore` line is, so `*.rs` finds every Rust file at any depth.
    by_name: bool,
}

impl FileFilter {
    /// Whether the file at `rel` (relative to the search root), named `name`,
    /// is one to search.
    fn admits(&self, rel: &str, name: &str) -> bool {
        match self.by_name {
            true => self.matcher.is_match(name),
            false => self.matcher.is_match(rel),
        }
    }
}

/// Compile a glob the way both tools read one: `*` stays inside one path
/// segment and `**` crosses them.
fn compile_glob(pattern: &str, argument: &str) -> Result<globset::GlobMatcher, String> {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| format!("[error] '{argument}' is not a valid glob ({e})"))
}

/// A whole-number argument, clamped to `1..=max`, with `default` when absent.
fn count_arg(args: &Value, key: &str, (default, max): (u64, u64)) -> usize {
    let n = args.get(key).and_then(Value::as_u64).unwrap_or(default);
    n.clamp(1, max) as usize
}

/// A boolean argument, with `default` when absent.
fn flag_arg(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

/// The directory walk both tools share: sorted, symlinks never followed,
/// `.git` never entered, and the ignore files honoured unless the call said
/// otherwise.
///
/// `require_git(false)` because a workdir is often not a repository, and a
/// `.gitignore` in it still says what its author considers noise.
fn walker(root: &Path, include_ignored: bool) -> ignore::Walk {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .standard_filters(!include_ignored)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_name(<std::ffi::OsStr as Ord>::cmp)
        .filter_entry(not_git_dir);
    builder.build()
}

/// Whether a walk may enter this entry: anything but a `.git` directory, whose
/// object files are never what a search is after.
fn not_git_dir(entry: &ignore::DirEntry) -> bool {
    entry.file_name() != ".git"
}

/// Read up to `cap` bytes of `path`, plus one so a longer file is noticed. An
/// unreadable file reads as empty, which searches as no matches: there is
/// nothing in it the model could have used.
fn read_capped(path: &Path, cap: usize) -> Vec<u8> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    let _ = std::fs::File::open(path)
        .and_then(|file| file.take(cap as u64 + 1).read_to_end(&mut bytes));
    bytes
}

/// Output under a byte cap: lines go in whole until the next would pass it.
struct Capped {
    text: String,
    cap: usize,
    full: bool,
}

impl Capped {
    fn new(cap: usize) -> Self {
        Self {
            text: String::new(),
            cap,
            full: false,
        }
    }

    /// Add `line`, or mark the output full and return `false` when it does
    /// not fit.
    fn push(&mut self, line: &str) -> bool {
        if self.full || self.text.len() + line.len() + 1 > self.cap {
            self.full = true;
            return false;
        }
        self.text.push_str(line);
        self.text.push('\n');
        true
    }
}

/// One line as shown: cut at [`MAX_LINE_CHARS`] characters, with a marker.
fn clip(line: &str) -> String {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((at, _)) => format!("{}...", line.get(..at).unwrap_or(line)),
        None => line.to_string(),
    }
}

/// The line ranges to print for `hits` with `context` lines either side,
/// overlapping windows merged, each clamped to the file's `len` lines.
fn windows(hits: &[usize], context: usize, len: usize) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for &hit in hits {
        let start = hit.saturating_sub(context);
        let end = (hit + context).min(len - 1);
        match out.last_mut() {
            Some(last) if start <= last.1 + 1 => last.1 = end,
            _ => out.push((start, end)),
        }
    }
    out
}

/// Search every file under `root` for `matcher`, returning the text the
/// model sees.
pub(crate) fn search_at(
    scope: &ReadScope,
    root: &Path,
    matcher: &regex::Regex,
    opts: &SearchOptions,
    caps: Caps,
) -> String {
    let mut out = Capped::new(caps.output_bytes);
    let mut matched = 0usize;
    let mut stopped_early = false;
    let mut partial = 0usize;
    for entry in walker(root, opts.include_ignored).filter_map(Result::ok) {
        let rel = slash_path(entry.path().strip_prefix(root).unwrap_or(entry.path()));
        let name = entry.file_name().to_string_lossy();
        if !opts.filter.as_ref().is_none_or(|f| f.admits(&rel, &name)) {
            continue;
        }
        let Some(real) = scope.confine(entry.path()).filter(|p| p.is_file()) else {
            continue;
        };
        let mut bytes = read_capped(&real, caps.file_bytes);
        if bytes.iter().take(BINARY_SNIFF_BYTES).any(|b| *b == 0) {
            continue;
        }
        if bytes.len() > caps.file_bytes {
            bytes.truncate(caps.file_bytes);
            partial += 1;
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let mut hits: Vec<usize> = (0..lines.len())
            .filter(|&i| matcher.is_match(lines[i]))
            .collect();
        if hits.is_empty() {
            continue;
        }
        let budget = opts.max_results - matched;
        if hits.len() > budget {
            hits.truncate(budget);
            stopped_early = true;
        }
        matched += hits.len();
        if !print_file(&mut out, &scope.shown(&real), &lines, &hits, opts.context) || stopped_early
        {
            break;
        }
    }
    let mut notes = Vec::new();
    if stopped_early {
        notes.push(format!(
            "[truncated: stopped after {} matching lines (max_results). Narrow the pattern, \
             path or glob, or raise max_results, to see the rest.]",
            opts.max_results
        ));
    }
    if out.full {
        notes.push(format!(
            "[truncated: the output reached its {}-byte cap. Narrow the pattern, path or glob \
             to see the rest.]",
            caps.output_bytes
        ));
    }
    if partial > 0 {
        notes.push(format!(
            "[note: {partial} file(s) longer than {} bytes were searched in their first {} \
             bytes only.]",
            caps.file_bytes, caps.file_bytes
        ));
    }
    let body = match matched {
        0 => format!(
            "No matches for {:?} under '{}'.",
            matcher.as_str(),
            scope.shown(root)
        ),
        _ => out.text.trim_end().to_string(),
    };
    std::iter::once(body)
        .chain(notes)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Print one file's matches as `path:line: text`, with context lines as
/// `path-line- text` and `--` between groups that do not touch, a file's
/// first group included when another file came before it. `false` once the
/// output is full.
fn print_file(
    out: &mut Capped,
    shown: &str,
    lines: &[&str],
    hits: &[usize],
    context: usize,
) -> bool {
    for (start, end) in windows(hits, context, lines.len()) {
        if context > 0 && !out.text.is_empty() && !out.push("--") {
            return false;
        }
        for (i, line) in lines.iter().enumerate().take(end + 1).skip(start) {
            let mark = match hits.binary_search(&i) {
                Ok(_) => ':',
                Err(_) => '-',
            };
            let mut row = String::new();
            let _ = write!(row, "{shown}{mark}{}{mark} {}", i + 1, clip(line));
            if !out.push(&row) {
                return false;
            }
        }
    }
    true
}

/// Every path under `root` that `matcher` matches, relative to `root`,
/// returned relative to the workdir and sorted, directories marked with a
/// trailing `/`.
pub(crate) fn glob_at(
    scope: &ReadScope,
    root: &Path,
    matcher: &globset::GlobMatcher,
    max_results: usize,
    include_ignored: bool,
) -> String {
    let mut found: Vec<String> = walker(root, include_ignored)
        .filter_map(Result::ok)
        .filter(|e| e.depth() > 0)
        .filter(|e| matcher.is_match(slash_path(e.path().strip_prefix(root).unwrap_or(e.path()))))
        .filter_map(|e| scope.confine(e.path()))
        .map(|real| match real.is_dir() {
            true => format!("{}/", scope.shown(&real)),
            false => scope.shown(&real),
        })
        .collect();
    found.sort();
    if found.is_empty() {
        return format!(
            "No paths match {:?} under '{}'.",
            matcher.glob().glob(),
            scope.shown(root)
        );
    }
    let total = found.len();
    found.truncate(max_results);
    let mut text = found.join("\n");
    if total > max_results {
        let _ = write!(
            text,
            "\n\n[truncated: {} more paths match (max_results is {max_results}). Narrow the \
             pattern or path to see them.]",
            total - max_results
        );
    }
    text
}

impl BuiltinTools {
    /// The pieces [`resolve_read`](Self::resolve_read) answers from, owned, so
    /// a walk on the blocking pool can resolve each file it meets the same
    /// way.
    pub(crate) fn read_scope(&self) -> crate::search::ReadScope {
        crate::search::ReadScope {
            workdir: self.ctx.workdir.clone(),
            read_paths: self.ctx.read_paths(),
            within: resolves_within,
        }
    }

    /// The root a search walks, resolved as `read_file` resolves a path, and
    /// refused when nothing is there.
    fn search_root(&self, args: &Value) -> Result<PathBuf, String> {
        let requested = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let root = self
            .resolve_read(requested)
            .map_err(|e| format!("[error] {e}"))?;
        match root.exists() {
            true => Ok(root),
            false => Err(format!("[error] path '{requested}' does not exist")),
        }
    }

    pub(crate) async fn search_files(&self, args: &Value) -> String {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return "[error] missing 'pattern' argument".to_string();
        };
        let source = match flag_arg(args, "regex", true) {
            true => pattern.to_string(),
            false => regex::escape(pattern),
        };
        let matcher = match regex::RegexBuilder::new(&source)
            .case_insensitive(flag_arg(args, "case_insensitive", false))
            .build()
        {
            Ok(m) => m,
            Err(e) => {
                return format!(
                    "[error] 'pattern' is not a valid regular expression ({e}). Set \"regex\": \
                     false to search for it as plain text."
                );
            }
        };
        let filter = match args.get("glob").and_then(Value::as_str) {
            Some(glob) => match compile_glob(glob, "glob") {
                Ok(matcher) => Some(FileFilter {
                    matcher,
                    by_name: !glob.contains('/'),
                }),
                Err(e) => return e,
            },
            None => None,
        };
        let root = match self.search_root(args) {
            Ok(root) => root,
            Err(e) => return e,
        };
        let opts = SearchOptions {
            context: args
                .get("context_lines")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(MAX_CONTEXT_LINES) as usize,
            max_results: count_arg(args, "max_results", SEARCH_RESULTS),
            include_ignored: flag_arg(args, "include_ignored", false),
            filter,
        };
        let scope = self.read_scope();
        crate::exec::off_thread(move || search_at(&scope, &root, &matcher, &opts, Caps::REAL)).await
    }

    pub(crate) async fn glob(&self, args: &Value) -> String {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return "[error] missing 'pattern' argument".to_string();
        };
        let matcher = match compile_glob(pattern, "pattern") {
            Ok(m) => m,
            Err(e) => return e,
        };
        let root = match self.search_root(args) {
            Ok(root) => root,
            Err(e) => return e,
        };
        let max = count_arg(args, "max_results", GLOB_RESULTS);
        let include_ignored = flag_arg(args, "include_ignored", false);
        let scope = self.read_scope();
        crate::exec::off_thread(move || glob_at(&scope, &root, &matcher, max, include_ignored))
            .await
    }
}

#[cfg(test)]
mod tests;

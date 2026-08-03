//! `cargo xtask docs` - keep `docs/content/` honest.
//!
//! The docs are the only part of the repo with no compiler behind them, so
//! nothing catches a link to a page that was renamed, an anchor whose heading
//! moved, or a page that forgot its frontmatter. This does.
//!
//! The checks all run against parsed [`Page`] values rather than the
//! filesystem, so every rule is unit-testable without writing a single file.

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// What `cargo xtask docs` was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum DocsMode {
    /// Report every problem and exit non-zero if there are any.
    Check,
}

impl DocsMode {
    /// Parse the arguments after `docs`.
    pub fn parse(args: &[String]) -> Result<Self> {
        match args.first().map(String::as_str) {
            None | Some("--check") => Ok(Self::Check),
            Some(other) => anyhow::bail!("Unknown `docs` argument: '{other}'. Try `--check`."),
        }
    }
}

/// One parsed page from `docs/content/`.
#[derive(Debug, Default)]
pub struct Page {
    /// Filename without `.md`, which is also its URL slug.
    pub slug: String,
    /// Frontmatter `group`, if present.
    pub group: Option<String>,
    /// Frontmatter `order`, if present.
    pub order: Option<String>,
    /// Frontmatter `title`, if present.
    pub title: Option<String>,
    /// Frontmatter `group_order`, if present.
    pub group_order: Option<String>,
    /// Frontmatter `description`, if present. One sentence, and the only thing
    /// an agent reading `llms.txt` sees before it decides whether to fetch the
    /// page, so a missing or padded one costs it a wasted round trip.
    pub description: Option<String>,
    /// Anchors this page offers: heading slugs plus any `<a id="...">`.
    pub anchors: HashSet<String>,
    /// `/docs/<slug>` links found in the body, as `(slug, Option<anchor>)`.
    pub doc_links: Vec<(String, Option<String>)>,
    /// Relative `*.md` link targets found in the body.
    pub relative_links: Vec<String>,
    /// Line numbers holding an em dash.
    pub em_dash_lines: Vec<usize>,
}

/// Turn heading text into the id the site generates for it.
///
/// Mirrors GitHub's slugger, which is what `marked-gfm-heading-id` uses:
/// lowercase, drop anything that is not alphanumeric, space, hyphen, or
/// underscore, then turn spaces into hyphens.
pub fn heading_slug(text: &str) -> String {
    let cleaned: String = text
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .collect();
    cleaned.trim().replace(' ', "-")
}

/// Strip inline markdown decoration from heading text before slugging it.
///
/// A heading like ``## `[limits]` and friends`` slugs off its visible text, so
/// the backticks, asterisks, and link syntax have to come off first.
pub fn heading_text(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '`' | '*' | '_' => {}
            '[' => {}
            ']' => {
                // Skip a trailing `(target)` so link text survives but the URL does not.
                if chars.peek() == Some(&'(') {
                    for inner in chars.by_ref() {
                        if inner == ')' {
                            break;
                        }
                    }
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Read one page's markdown into a [`Page`].
pub fn parse_page(slug: &str, source: &str) -> Page {
    let mut page = Page {
        slug: slug.to_string(),
        ..Page::default()
    };
    let mut in_frontmatter = false;
    let mut seen_frontmatter = false;
    let mut in_code_fence = false;

    for (idx, line) in source.lines().enumerate() {
        let trimmed = line.trim();

        if trimmed == "---" && !seen_frontmatter {
            match in_frontmatter {
                true => {
                    in_frontmatter = false;
                    seen_frontmatter = true;
                }
                false => in_frontmatter = true,
            }
            continue;
        }
        if in_frontmatter {
            record_frontmatter(&mut page, trimmed);
            continue;
        }

        if trimmed.starts_with("```") {
            in_code_fence = !in_code_fence;
            continue;
        }

        if line.contains('\u{2014}') {
            page.em_dash_lines.push(idx + 1);
        }
        if !in_code_fence {
            collect_anchors(&mut page, trimmed);
            collect_links(&mut page, line);
        }
    }
    page
}

/// Store one `key: value` frontmatter line on the page.
fn record_frontmatter(page: &mut Page, line: &str) {
    let Some((key, value)) = line.split_once(':') else {
        return;
    };
    let value = value.trim().to_string();
    match key.trim() {
        "title" => page.title = Some(value),
        "group" => page.group = Some(value),
        "group_order" => page.group_order = Some(value),
        "order" => page.order = Some(value),
        "description" => page.description = Some(value),
        _ => {}
    }
}

/// Record any anchor this line offers, from a heading or an explicit `<a id>`.
fn collect_anchors(page: &mut Page, trimmed: &str) {
    if let Some(rest) = trimmed.strip_prefix("##") {
        let text = heading_text(rest.trim_start_matches('#').trim());
        page.anchors.insert(heading_slug(&text));
    }
    if let Some((_, after)) = trimmed.split_once("<a id=\"")
        && let Some((id, _)) = after.split_once('"')
    {
        page.anchors.insert(id.to_string());
    }
}

/// Record every `/docs/...` and relative `.md` link on this line.
fn collect_links(page: &mut Page, line: &str) {
    for chunk in line.split("](").skip(1) {
        let Some((target, _)) = chunk.split_once(')') else {
            continue;
        };
        let target = target.trim();

        if let Some(rest) = target.strip_prefix("/docs/") {
            let (slug, anchor) = match rest.split_once('#') {
                Some((s, a)) => (s, Some(a.to_string())),
                None => (rest, None),
            };
            page.doc_links.push((slug.to_string(), anchor));
        } else if target.ends_with(".md") && !target.contains("://") {
            page.relative_links.push(target.to_string());
        }
    }
}

/// Run every rule over the whole page set, returning one message per problem.
pub fn check_all(pages: &[Page]) -> Vec<String> {
    let by_slug: HashMap<&str, &Page> = pages.iter().map(|p| (p.slug.as_str(), p)).collect();
    let mut problems = Vec::new();

    for page in pages {
        check_frontmatter(page, &mut problems);
        check_links(page, &by_slug, &mut problems);
        for line in &page.em_dash_lines {
            problems.push(format!(
                "{}.md:{line}: em dash. The docs use plain sentences instead",
                page.slug
            ));
        }
    }
    check_orders(pages, &mut problems);
    problems.sort();
    problems
}

/// The ceiling on a `description`, in characters.
///
/// `llms.txt` renders one per page as a single line. Past roughly this length a
/// description stops being a routing hint and becomes the page, which is the
/// failure this bound exists to prevent.
pub const MAX_DESCRIPTION_CHARS: usize = 160;

/// Every page needs all five frontmatter keys, and `description` has to stay
/// short enough to be a one-line routing hint.
fn check_frontmatter(page: &Page, problems: &mut Vec<String>) {
    let missing = [
        ("title", page.title.is_none()),
        ("group", page.group.is_none()),
        ("group_order", page.group_order.is_none()),
        ("order", page.order.is_none()),
        ("description", page.description.is_none()),
    ];
    for (key, absent) in missing {
        if absent {
            problems.push(format!("{}.md: frontmatter is missing `{key}`", page.slug));
        }
    }
    // Counted in chars, not bytes: the workspace denies `clippy::string_slice`
    // for the same reason, and a description may hold non-ASCII.
    if let Some(description) = &page.description {
        let length = description.chars().count();
        if length > MAX_DESCRIPTION_CHARS {
            problems.push(format!(
                "{}.md: description is {length} chars, over the {MAX_DESCRIPTION_CHARS} limit",
                page.slug
            ));
        }
    }
}

/// Doc links must resolve, anchors must exist, relative links are banned.
fn check_links(page: &Page, by_slug: &HashMap<&str, &Page>, problems: &mut Vec<String>) {
    for (slug, anchor) in &page.doc_links {
        let Some(target) = by_slug.get(slug.as_str()) else {
            problems.push(format!(
                "{}.md: links to /docs/{slug}, which has no page",
                page.slug
            ));
            continue;
        };
        let Some(anchor) = anchor else { continue };
        if !target.anchors.contains(anchor) {
            problems.push(format!(
                "{}.md: links to /docs/{slug}#{anchor}, which has no such heading",
                page.slug
            ));
        }
    }
    for target in &page.relative_links {
        problems.push(format!(
            "{}.md: relative link to `{target}`. Use /docs/<slug> so the renderer can rewrite it",
            page.slug
        ));
    }
}

/// No two pages in a group may claim the same `order`.
fn check_orders(pages: &[Page], problems: &mut Vec<String>) {
    let mut seen: HashMap<(&str, &str), &str> = HashMap::new();
    for page in pages {
        let (Some(group), Some(order)) = (&page.group, &page.order) else {
            continue;
        };
        if let Some(other) = seen.insert((group, order), &page.slug) {
            problems.push(format!(
                "{group}: `{}` and `{other}` both claim order {order}",
                page.slug
            ));
        }
    }
}

/// Read `docs/content/`, run every check, and report.
pub fn run(_mode: DocsMode) -> Result<()> {
    let dir = Path::new("docs/content");
    let mut pages = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::path);

    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        pages.push(parse_page(slug, &std::fs::read_to_string(&path)?));
    }

    let problems = check_all(&pages);
    if problems.is_empty() {
        println!("docs: {} pages, no problems", pages.len());
        return Ok(());
    }
    for problem in &problems {
        println!("  {problem}");
    }
    anyhow::bail!("docs: {} problem(s)", problems.len())
}

#[cfg(test)]
#[path = "docs_tests.rs"]
mod tests;

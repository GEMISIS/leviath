//! Tests for the docs checker.
//!
//! Split into its own file so the test bodies stay out of the coverage
//! measurement, matching the layout the rest of the workspace uses.

use super::*;

/// A well-formed page with an explicit `order`.
fn page_with(slug: &str, order: usize, body: &str) -> Page {
    let source = format!(
        "---\ntitle: T\ndescription: D\ngroup: Concepts\ngroup_order: 2\norder: {order}\n---\n\n{body}\n"
    );
    parse_page(slug, &source)
}

/// A minimal well-formed page, so each test only spells out what it is about.
///
/// The `order` is derived from the slug so that two pages built this way never
/// collide on the duplicate-order rule. Tests that want a collision use
/// [`page_with`] and say so.
fn page(slug: &str, body: &str) -> Page {
    let order = slug.bytes().map(usize::from).sum();
    page_with(slug, order, body)
}

// ── DocsMode ────────────────────────────────────────────────────────────────

#[test]
fn parse_defaults_to_check() {
    assert_eq!(DocsMode::parse(&[]).unwrap(), DocsMode::Check);
}

#[test]
fn parse_accepts_check_flag() {
    let args = vec!["--check".to_string()];
    assert_eq!(DocsMode::parse(&args).unwrap(), DocsMode::Check);
}

#[test]
fn parse_rejects_anything_else() {
    let args = vec!["--fix".to_string()];
    let err = DocsMode::parse(&args).unwrap_err();
    assert!(err.to_string().contains("--fix"), "{err}");
}

// ── Slugging ────────────────────────────────────────────────────────────────

#[test]
fn heading_slug_matches_githubs_rules() {
    assert_eq!(heading_slug("Inference pools"), "inference-pools");
    assert_eq!(heading_slug("The tool lane"), "the-tool-lane");
    // Punctuation is dropped, not replaced, so `policy.toml` loses its dot.
    assert_eq!(heading_slug("policy.toml"), "policytoml");
    assert_eq!(
        heading_slug("model_providers.<name>"),
        "model_providersname"
    );
    assert_eq!(heading_slug("  Graph  "), "graph");
}

#[test]
fn heading_text_strips_inline_markup() {
    assert_eq!(heading_text("`[limits]`"), "limits");
    assert_eq!(heading_text("**Bold** heading"), "Bold heading");
    // Link syntax keeps the text and drops the target.
    assert_eq!(heading_text("See [the docs](/docs/api)"), "See the docs");
    // A bracket with no following paren keeps everything after it.
    assert_eq!(heading_text("array[0] index"), "array0 index");
}

// ── Frontmatter ─────────────────────────────────────────────────────────────

#[test]
fn frontmatter_keys_are_read() {
    let p = page_with("api", 1, "# API");
    assert_eq!(p.title.as_deref(), Some("T"));
    assert_eq!(p.description.as_deref(), Some("D"));
    assert_eq!(p.group.as_deref(), Some("Concepts"));
    assert_eq!(p.group_order.as_deref(), Some("2"));
    assert_eq!(p.order.as_deref(), Some("1"));
}

#[test]
fn a_missing_description_is_reported() {
    // The one frontmatter key an agent reads before deciding whether to fetch
    // the page. Without it `llms.txt` is a bare link list again.
    let src = "---\ntitle: T\ngroup: G\ngroup_order: 1\norder: 1\n---\n\n# Hi\n";
    let problems = check_all(&[parse_page("x", src)]);
    assert!(problems.iter().any(|p| p.contains("missing `description`")));
}

#[test]
fn a_description_at_the_limit_is_accepted() {
    let description = "d".repeat(MAX_DESCRIPTION_CHARS);
    let src = format!(
        "---\ntitle: T\ndescription: {description}\ngroup: G\ngroup_order: 1\norder: 1\n---\n\n# Hi\n"
    );
    let problems = check_all(&[parse_page("x", &src)]);
    assert!(problems.is_empty(), "{problems:?}");
}

#[test]
fn an_over_long_description_is_reported() {
    let description = "d".repeat(MAX_DESCRIPTION_CHARS + 1);
    let src = format!(
        "---\ntitle: T\ndescription: {description}\ngroup: G\ngroup_order: 1\norder: 1\n---\n\n# Hi\n"
    );
    let problems = check_all(&[parse_page("x", &src)]);
    assert!(
        problems.iter().any(|p| p.contains("over the")),
        "{problems:?}"
    );
}

#[test]
fn a_description_is_measured_in_chars_not_bytes() {
    // Every char here is 4 bytes, so a byte-counting check would reject a
    // description well inside the limit.
    let description = "🌊".repeat(MAX_DESCRIPTION_CHARS);
    let src = format!(
        "---\ntitle: T\ndescription: {description}\ngroup: G\ngroup_order: 1\norder: 1\n---\n\n# Hi\n"
    );
    let problems = check_all(&[parse_page("x", &src)]);
    assert!(problems.is_empty(), "{problems:?}");
}

#[test]
fn unknown_and_malformed_frontmatter_lines_are_ignored() {
    let src = "---\ntitle: T\nnonsense\nextra: value\n---\n\n# Hi\n";
    let p = parse_page("x", src);
    assert_eq!(p.title.as_deref(), Some("T"));
    assert!(p.group.is_none());
}

#[test]
fn a_missing_frontmatter_key_is_reported() {
    let src = "---\ntitle: T\ngroup: G\n---\n\n# Hi\n";
    let problems = check_all(&[parse_page("x", src)]);
    assert!(problems.iter().any(|p| p.contains("missing `order`")));
    assert!(problems.iter().any(|p| p.contains("missing `group_order`")));
}

#[test]
fn a_horizontal_rule_after_the_frontmatter_is_not_a_second_block() {
    let src = "---\ntitle: T\ngroup: G\ngroup_order: 1\norder: 1\n---\n\ntext\n\n---\n\nmore\n";
    let p = parse_page("x", src);
    assert_eq!(p.title.as_deref(), Some("T"));
}

// ── Anchors ─────────────────────────────────────────────────────────────────

#[test]
fn headings_and_explicit_ids_both_become_anchors() {
    let p = page(
        "engine",
        "## Inference pools\n\n<a id=\"legacy\"></a>\n\n### Deep one",
    );
    assert!(p.anchors.contains("inference-pools"));
    assert!(p.anchors.contains("legacy"));
    assert!(p.anchors.contains("deep-one"));
}

#[test]
fn an_unterminated_anchor_tag_is_skipped() {
    let p = page("x", "<a id=\"oops");
    assert!(p.anchors.is_empty());
}

#[test]
fn a_single_hash_title_is_not_an_anchor() {
    let p = page("x", "# Page title");
    assert!(p.anchors.is_empty());
}

// ── Links ───────────────────────────────────────────────────────────────────

#[test]
fn doc_links_are_collected_with_and_without_anchors() {
    let p = page(
        "a",
        "See [x](/docs/api) and [y](/docs/engine#inference-pools).",
    );
    assert_eq!(
        p.doc_links,
        vec![
            ("api".to_string(), None),
            ("engine".to_string(), Some("inference-pools".to_string())),
        ]
    );
}

#[test]
fn an_unclosed_link_is_skipped() {
    let p = page("a", "broken [text](/docs/api");
    assert!(p.doc_links.is_empty());
}

#[test]
fn external_and_plain_links_are_not_doc_links() {
    let p = page("a", "[o](https://example.com) [p](/app) [q](#local)");
    assert!(p.doc_links.is_empty());
    assert!(p.relative_links.is_empty());
}

#[test]
fn a_link_to_a_missing_page_is_reported() {
    let problems = check_all(&[page("a", "[gone](/docs/comparison)")]);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("/docs/comparison") && p.contains("no page")),
        "{problems:?}"
    );
}

#[test]
fn a_link_to_a_missing_anchor_is_reported() {
    let pages = vec![page("a", "[x](/docs/b#nope)"), page("b", "## Real heading")];
    let problems = check_all(&pages);
    assert!(
        problems.iter().any(|p| p.contains("no such heading")),
        "{problems:?}"
    );
}

#[test]
fn a_link_to_a_real_anchor_is_accepted() {
    let pages = vec![
        page("a", "[x](/docs/b#real-heading)"),
        page("b", "## Real heading"),
    ];
    assert!(check_all(&pages).is_empty(), "{:?}", check_all(&pages));
}

#[test]
fn a_relative_md_link_is_reported() {
    let problems = check_all(&[page("daemon", "the [API guide](api.md)")]);
    assert!(
        problems.iter().any(|p| p.contains("relative link")),
        "{problems:?}"
    );
}

#[test]
fn an_external_md_url_is_not_a_relative_link() {
    let p = page("a", "[sec](https://github.com/o/r/blob/main/SECURITY.md)");
    assert!(p.relative_links.is_empty());
}

// ── Em dashes and code fences ───────────────────────────────────────────────

#[test]
fn an_em_dash_is_reported_with_its_line() {
    let problems = check_all(&[page("x", "a sentence \u{2014} with a dash")]);
    assert!(
        problems.iter().any(|p| p.contains("em dash")),
        "{problems:?}"
    );
}

#[test]
fn links_inside_a_code_fence_are_ignored() {
    let p = page("a", "```bash\nsee [x](/docs/nope)\n```\n\nreal text");
    assert!(p.doc_links.is_empty());
}

// ── Ordering ────────────────────────────────────────────────────────────────

#[test]
fn two_pages_claiming_one_order_are_reported() {
    let problems = check_all(&[page_with("a", 1, "text"), page_with("b", 1, "text")]);
    assert!(
        problems.iter().any(|p| p.contains("both claim order")),
        "{problems:?}"
    );
}

#[test]
fn pages_in_different_groups_may_share_an_order() {
    let a = page_with("a", 1, "text");
    let src =
        "---\ntitle: T\ndescription: D\ngroup: Guides\ngroup_order: 4\norder: 1\n---\n\ntext\n";
    assert!(check_all(&[a, parse_page("b", src)]).is_empty());
}

#[test]
fn a_page_missing_group_or_order_is_skipped_by_the_order_check() {
    let src = "---\ntitle: T\n---\n\ntext\n";
    let problems = check_all(&[parse_page("a", src), parse_page("b", src)]);
    assert!(!problems.iter().any(|p| p.contains("both claim order")));
}

// ── The real corpus ─────────────────────────────────────────────────────────

#[test]
fn the_shipped_docs_pass_every_check() {
    // Anchored on this file so it does not depend on the working directory.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent")
        .join("docs/content");
    let mut pages = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("docs/content is readable") {
        let path = entry.expect("readable entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let slug = path.file_stem().and_then(|s| s.to_str()).expect("a stem");
        let body = std::fs::read_to_string(&path).expect("readable page");
        pages.push(parse_page(slug, &body));
    }
    assert!(!pages.is_empty(), "found no pages in {}", dir.display());

    let problems = check_all(&pages);
    assert!(
        problems.is_empty(),
        "docs/content has {} problem(s):\n  {}",
        problems.len(),
        problems.join("\n  ")
    );
}

// ── Published schemas ───────────────────────────────────────────────────────

#[test]
fn the_real_schemas_are_present_and_parse() {
    // `run` resolves this relative to the repo root, but a test's working
    // directory is the crate root, so anchor on the manifest directory.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ has a parent");
    assert_eq!(
        check_schemas(&root.join("docs/schema")).unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn a_missing_schema_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let problems = check_schemas(dir.path()).unwrap();
    assert_eq!(problems.len(), SCHEMAS.len());
    assert!(
        problems.iter().all(|p| p.contains("missing")),
        "{problems:?}"
    );
}

#[test]
fn a_truncated_schema_is_reported() {
    // The failure this exists for: a half-written file still gets synced to S3
    // and served under a name llms.txt advertises.
    let dir = tempfile::tempdir().unwrap();
    for name in SCHEMAS {
        std::fs::write(dir.path().join(name), "{\"$schema\": ").unwrap();
    }
    let problems = check_schemas(dir.path()).unwrap();
    assert!(
        problems.iter().all(|p| p.contains("not valid JSON")),
        "{problems:?}"
    );
}

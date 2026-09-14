//! Tests for `cargo xtask bedrock-windows`, driven off fixture HTML so no
//! network is touched.

use super::*;

/// The index page: three cards, one of them linked twice and one an
/// embedding model with no limits.
fn index() -> String {
    r#"<a href="./model-card-anthropic-claude-sonnet-5.html">Claude Sonnet 5</a>
<a href="./model-card-amazon-nova-pro.html">Nova Pro</a>
<a href="./model-card-anthropic-claude-sonnet-5.html">again</a>
<a href="./model-card-amazon-titan-embed.html">Titan</a>
<a href="./model-cards-anthropic.html">vendor page</a>
<a href="./model-card-broken">no suffix"#
        .to_string()
}

/// The pieces of a card the parser reads.
struct Card<'a> {
    title: &'a str,
    context: &'a str,
    output: &'a str,
    reasoning: bool,
    counts: bool,
    runtime_id: &'a str,
    geo: &'a [&'a str],
    global: &'a str,
    mantle_id: &'a str,
}

/// A card as AWS lays it out.
fn card(spec: &Card<'_>) -> String {
    let reasoning_line = if spec.reasoning {
        "<li><p><b>Reasoning:</b> Supported</p></li>"
    } else {
        ""
    };
    let count_icon = if spec.counts {
        "icon-yes.png"
    } else {
        "icon-no.png"
    };
    let geo_cells: String = spec
        .geo
        .iter()
        .map(|g| format!("<code class=\"code\">{g}</code><br />"))
        .collect();
    let Card {
        title,
        context,
        output,
        runtime_id,
        global,
        mantle_id,
        ..
    } = spec;
    format!(
        r#"<html><head><title>{title} - Amazon Bedrock</title></head><body>
<ul><li><p><b>Model lifecycle:</b> Active</p></li>
<li><p><b>Context window:</b> {context}</p></li>
<li><p><b>Max output tokens:</b> {output}</p></li>
{reasoning_line}</ul>
<table><tr><td><p><img src="/images/icons/icon-yes.png" /> <a href="streaming.html">Response streaming</a></p>
<p><img src="/images/icons/{count_icon}" /> <a href="count-tokens.html">Count tokens</a></p></td></tr></table>
<h2 id="programmatic-access">Programmatic Access</h2>
<table><thead><tr><th>Endpoint</th><th>Model ID</th><th>URL</th><th>Geo</th><th>Global</th></tr></thead>
<tr><td tabindex="-1">bedrock-runtime</td><td>{runtime_id}</td><td>N/A</td><td>{geo_cells}</td><td>{global}</td></tr>
<tr><td>bedrock-mantle</td><td>{mantle_id}</td><td>https://bedrock-mantle.x</td><td>N/A</td><td>N/A</td></tr>
</table></body></html>"#
    )
}

fn sonnet() -> String {
    card(&Card {
        title: "Claude Sonnet 5",
        context: "1M tokens",
        output: "128K",
        reasoning: true,
        counts: false,
        runtime_id: "N/A",
        geo: &[
            "us.anthropic.claude-sonnet-5",
            "eu.anthropic.claude-sonnet-5",
        ],
        global: "global.anthropic.claude-sonnet-5",
        mantle_id: "anthropic.claude-sonnet-5",
    })
}

fn nova() -> String {
    card(&Card {
        title: "Nova Pro",
        context: "300K tokens",
        output: "5K",
        reasoning: false,
        counts: true,
        runtime_id: "amazon.nova-pro-v1:0",
        geo: &["us.amazon.nova-pro-v1:0"],
        global: "N/A",
        mantle_id: "N/A",
    })
}

fn fixture_fetch(url: &str) -> Result<String> {
    Ok(match url {
        INDEX_URL => index(),
        u if u.ends_with("model-card-anthropic-claude-sonnet-5.html") => sonnet(),
        u if u.ends_with("model-card-amazon-nova-pro.html") => nova(),
        // The embedding model: a card with no limits.
        _ => "<html><title>Titan - Amazon Bedrock</title></html>".to_string(),
    })
}

fn failing_fetch(_url: &str) -> Result<String> {
    Err(NetworkError("aws down".into()).into())
}

fn empty_fetch(_url: &str) -> Result<String> {
    Ok(String::new())
}

#[test]
fn mode_parses_write_check_and_rejects_junk() {
    assert_eq!(WindowsMode::parse(&[]).unwrap(), WindowsMode::Write);
    assert_eq!(
        WindowsMode::parse(&["--check".into()]).unwrap(),
        WindowsMode::Check
    );
    assert!(WindowsMode::parse(&["--nope".into()]).is_err());
}

#[test]
fn the_index_links_each_card_once() {
    assert_eq!(
        card_links(&index()),
        vec![
            "model-card-anthropic-claude-sonnet-5.html",
            "model-card-amazon-nova-pro.html",
            "model-card-amazon-titan-embed.html",
        ]
    );
    assert!(card_links("").is_empty());
}

#[test]
fn token_figures_are_read_as_printed() {
    assert_eq!(parse_tokens("1M tokens"), Some(1_000_000));
    assert_eq!(parse_tokens("200K tokens"), Some(200_000));
    assert_eq!(parse_tokens("128K"), Some(128_000));
    assert_eq!(parse_tokens("8,192 tokens"), Some(8_192));
    assert_eq!(parse_tokens("1.5m"), Some(1_500_000));
    assert_eq!(parse_tokens(""), None);
    assert_eq!(parse_tokens("lots"), None);
    assert_eq!(parse_tokens("0K"), None);
}

#[test]
fn a_card_is_read_into_a_row() {
    let row = parse_card(&sonnet()).unwrap();
    assert_eq!(row.id, "anthropic.claude-sonnet-5");
    assert_eq!(row.name, "Claude Sonnet 5");
    assert_eq!((row.context, row.output), (1_000_000, 128_000));
    assert!(row.reasoning);
    assert!(!row.count_tokens);
    assert_eq!(
        row.profiles,
        vec![
            "us.anthropic.claude-sonnet-5",
            "eu.anthropic.claude-sonnet-5",
            "global.anthropic.claude-sonnet-5"
        ]
    );
    assert_eq!(row.source, "aws-model-card");
    let row = parse_card(&nova()).unwrap();
    assert_eq!(row.id, "amazon.nova-pro-v1:0");
    assert!(!row.reasoning);
    assert!(row.count_tokens);
    assert_eq!(row.profiles, vec!["us.amazon.nova-pro-v1:0"]);
}

#[test]
fn a_card_without_limits_a_name_or_an_id_is_skipped() {
    assert_eq!(parse_card("<html></html>"), None);
    let no_output = "<p><b>Context window:</b> 1K</p>";
    assert_eq!(parse_card(no_output), None);
    let no_title = "<p><b>Context window:</b> 1K</p><p><b>Max output tokens:</b> 1K</p>";
    assert_eq!(parse_card(no_title), None);
    let no_id = format!("<title>X - Amazon Bedrock</title>{no_title}");
    assert_eq!(parse_card(&no_id), None);
    // A runtime row with a profile only names the bare id through it.
    let profile_only = card(&Card {
        title: "X",
        context: "1K",
        output: "1K",
        reasoning: false,
        counts: false,
        runtime_id: "N/A",
        geo: &["us.v.m-v1:0"],
        global: "N/A",
        mantle_id: "N/A",
    });
    assert_eq!(parse_card(&profile_only).unwrap().id, "v.m-v1:0");
    // A table with too few cells is ignored.
    let odd = format!(
        "{no_id}<table><tr><th>Model ID</th></tr><tr><td>bedrock-runtime</td></tr></table>"
    );
    assert_eq!(parse_card(&odd), None);
    // A count-tokens link with no icon before it is not supported.
    let bare_link = format!(
        "{no_title}<title>X - Amazon Bedrock</title><a href=\"count-tokens.html\">Count</a><table><tr><th>Model ID</th></tr><tr><td>bedrock-runtime</td><td>v.m</td></tr>"
    );
    let row = parse_card(&bare_link).unwrap();
    assert!(!row.count_tokens);
    assert_eq!(row.id, "v.m");
    assert!(row.profiles.is_empty());
    // A yes icon with no earlier no icon.
    let yes_only = bare_link.replace(
        "<a href=\"count-tokens.html\">",
        "<img src=\"icon-yes.png\"><a href=\"count-tokens.html\">",
    );
    assert!(parse_card(&yes_only).unwrap().count_tokens);
    // A title with markup inside it.
    let marked = no_title.to_string()
        + "<title><b>Y</b> - Amazon Bedrock</title><table><tr><th>Model ID</th></tr><tr><td>bedrock-mantle</td><td>v.y</td></tr>";
    let row = parse_card(&marked).unwrap();
    assert_eq!(row.name, "Y");
    assert_eq!(row.id, "v.y");
}

#[test]
fn without_prefix_strips_only_a_profile_prefix() {
    assert_eq!(
        without_prefix("us.amazon.nova-pro-v1:0"),
        "amazon.nova-pro-v1:0"
    );
    assert_eq!(
        without_prefix("amazon.nova-pro-v1:0"),
        "amazon.nova-pro-v1:0"
    );
    assert_eq!(without_prefix("bare"), "bare");
}

#[test]
fn code_spans_and_tags_are_read_plainly() {
    assert_eq!(
        code_spans("<code class=\"c\">a</code> x <code>b<i>!</i></code><code<code>open"),
        vec!["a", "b!"]
    );
    assert_eq!(strip_tags("  <b>a</b>\n b  "), "a b");
}

fn row(id: &str, context: u64, output: u64, source: &str) -> Row {
    Row {
        id: id.to_string(),
        name: id.to_string(),
        context,
        output,
        reasoning: false,
        count_tokens: false,
        profiles: vec![format!("us.{id}")],
        source: source.to_string(),
    }
}

fn table(rows: Vec<Row>) -> Table {
    Table {
        read_on: "2026-01-01".to_string(),
        rows: rows.into_iter().map(|r| (r.id.clone(), r)).collect(),
    }
}

#[test]
fn the_table_round_trips_through_the_file_text() {
    let mut named = row("v.a", 1000, 100, "aws-model-card");
    named.name = "A \"quoted\" name".to_string();
    named.reasoning = true;
    let t = table(vec![named, row("v.b", 2000, 200, "manual")]);
    let text = render_table(&t);
    assert!(text.starts_with("# What each Bedrock model"));
    let back = parse_table(&text).unwrap();
    assert_eq!(back.read_on, "2026-01-01");
    assert_eq!(back.rows.len(), 2);
    assert_eq!(back.rows["v.a"].name, "A 'quoted' name");
    assert!(back.rows["v.a"].reasoning);
    assert_eq!(back.rows["v.b"].source, "manual");
    assert!(parse_table("read_on = 1").is_err());
}

#[test]
fn the_merge_keeps_manual_and_missing_rows_and_dates_a_change() {
    let existing = table(vec![
        row("v.a", 1000, 100, "aws-model-card"),
        row("v.b", 2000, 200, "manual"),
        row("v.gone", 3000, 300, "aws-model-card"),
    ]);
    let fetched: Rows = [
        row("v.a", 1200, 100, "aws-model-card"),
        row("v.b", 9000, 900, "aws-model-card"),
        row("v.new", 500, 50, "aws-model-card"),
    ]
    .into_iter()
    .map(|r| (r.id.clone(), r))
    .collect();
    let merged = merge(&existing, &fetched, "2026-09-14").unwrap();
    assert_eq!(merged.table.read_on, "2026-09-14");
    assert_eq!(merged.table.rows["v.a"].context, 1200);
    assert_eq!(merged.table.rows["v.b"].context, 2000);
    assert!(merged.table.rows.contains_key("v.gone"));
    assert!(merged.table.rows.contains_key("v.new"));
    assert_eq!(merged.changes.len(), 2);
    let text: Vec<String> = merged.changes.iter().map(ToString::to_string).collect();
    assert!(
        text[0].starts_with("~ v.a: 1000 in / 100 out (aws-model-card) -> 1200 in"),
        "{}",
        text[0]
    );
    assert!(
        text[1].starts_with("+ v.new: 500 in / 50 out"),
        "{}",
        text[1]
    );
    // Nothing moved: the date stays.
    let same = merge(&existing, &existing.rows, "2026-09-14").unwrap();
    assert_eq!(same.table.read_on, "2026-01-01");
    assert!(same.changes.is_empty());
}

#[test]
fn the_merge_refuses_an_empty_thin_or_implausible_scrape() {
    let existing = table(vec![
        row("v.a", 1000, 100, "aws-model-card"),
        row("v.b", 1000, 100, "aws-model-card"),
        row("v.c", 1000, 100, "aws-model-card"),
    ]);
    let err = merge(&existing, &Rows::new(), "d").unwrap_err();
    assert!(err.to_string().contains("no model card"), "{err}");
    let thin: Rows = [row("v.a", 1000, 100, "aws-model-card")]
        .into_iter()
        .map(|r| (r.id.clone(), r))
        .collect();
    let err = merge(&existing, &thin, "d").unwrap_err();
    assert!(err.to_string().contains("only 1 cards"), "{err}");
    let mut wild: Rows = existing.rows.clone();
    wild.insert("v.a".to_string(), row("v.a", 5000, 100, "aws-model-card"));
    let err = merge(&existing, &wild, "d").unwrap_err();
    assert!(err.to_string().contains("would move"), "{err}");
    wild.insert("v.a".to_string(), row("v.a", 1000, 10, "aws-model-card"));
    assert!(merge(&existing, &wild, "d").is_err());
    // A row that names a limit on the edge of plausibility is written, and
    // one that only gains a profile or a flag is a change worth writing.
    let mut edge = existing.rows.clone();
    let mut flagged = row("v.a", 2500, 100, "aws-model-card");
    flagged.count_tokens = true;
    edge.insert("v.a".to_string(), flagged);
    let merged = merge(&existing, &edge, "d").unwrap();
    assert_eq!(merged.changes.len(), 1);
    assert!(merged.changes[0].to_string().contains(", counts"));
}

#[test]
fn run_with_writes_then_is_idempotent_then_checks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("windows.toml");
    std::fs::write(&path, "read_on = \"2026-01-01\"\n").unwrap();
    let outcome = run_with(WindowsMode::Write, fixture_fetch, &path, "2026-09-14").unwrap();
    assert_eq!(outcome, Outcome::Changed(2));
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("id = \"anthropic.claude-sonnet-5\""));
    assert!(written.contains("read_on = \"2026-09-14\""));
    let again = run_with(WindowsMode::Write, fixture_fetch, &path, "2026-12-25").unwrap();
    assert_eq!(again, Outcome::Unchanged);
    assert!(run_with(WindowsMode::Check, fixture_fetch, &path, "2026-12-25").is_ok());
    // A file behind the cards fails the check.
    std::fs::write(&path, "read_on = \"2026-01-01\"\n").unwrap();
    let err = run_with(WindowsMode::Check, fixture_fetch, &path, "2026-09-14").unwrap_err();
    assert!(err.to_string().contains("would change"), "{err}");
    assert!(!is_network_error(&err));
}

#[test]
fn run_with_surfaces_the_network_read_and_scrape_failures() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("windows.toml");
    std::fs::write(&path, "read_on = \"2026-01-01\"\n").unwrap();
    let err = run_with(WindowsMode::Write, failing_fetch, &path, "d").unwrap_err();
    assert!(is_network_error(&err));
    let err = run_with(WindowsMode::Write, empty_fetch, &path, "d").unwrap_err();
    assert!(err.to_string().contains("no model card"), "{err}");
    let err = run_with(
        WindowsMode::Write,
        fixture_fetch,
        &dir.path().join("gone.toml"),
        "d",
    )
    .unwrap_err();
    assert!(!is_network_error(&err));
    std::fs::write(&path, "read_on = 5\n").unwrap();
    assert!(run_with(WindowsMode::Write, fixture_fetch, &path, "d").is_err());
}

#[test]
fn the_real_fetch_reports_an_unreachable_host_as_the_network() {
    let err = fetch_http("http://127.0.0.1:1/model-cards.html").unwrap_err();
    assert!(is_network_error(&err), "{err}");
    let err = fetch_http("not a url").unwrap_err();
    assert!(is_network_error(&err), "{err}");
}

#[test]
fn the_workspace_root_holds_the_table_and_today_is_a_date() {
    assert!(workspace_root().join(WINDOWS_FILE).exists());
    let today = today();
    assert_eq!(today.len(), 10);
    assert_eq!(today.matches('-').count(), 2);
}

#[test]
fn the_shipped_table_parses_and_names_the_current_claude() {
    // A Windows checkout with `core.autocrlf` hands us CRLF; the refresh
    // always writes LF, so compare the file as the repository stores it.
    let text = std::fs::read_to_string(workspace_root().join(WINDOWS_FILE))
        .unwrap()
        .replace("\r\n", "\n");
    let table = parse_table(&text).unwrap();
    let sonnet = &table.rows["anthropic.claude-sonnet-5"];
    assert_eq!((sonnet.context, sonnet.output), (1_000_000, 128_000));
    // Rendering what was parsed reproduces the file, so a hand edit that
    // drifts from the writer's shape is caught here.
    assert_eq!(render_table(&table), text);
}

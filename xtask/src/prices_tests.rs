//! Tests for `cargo xtask prices`: the parsers and the merge rules, against
//! fixture JSON. Nothing here touches the network.

use super::*;

/// A per-token string as OpenRouter writes it.
fn or_model(
    id: &str,
    prompt: &str,
    completion: &str,
    read: Option<&str>,
    write: Option<&str>,
) -> String {
    let mut pricing = format!("\"prompt\": \"{prompt}\", \"completion\": \"{completion}\"");
    if let Some(r) = read {
        pricing.push_str(&format!(", \"input_cache_read\": \"{r}\""));
    }
    if let Some(w) = write {
        pricing.push_str(&format!(", \"input_cache_write\": \"{w}\""));
    }
    format!("{{\"id\": \"{id}\", \"pricing\": {{{pricing}}}}}")
}

fn openrouter_body(models: &[String]) -> String {
    format!("{{\"data\": [{}]}}", models.join(","))
}

fn ll_entry(
    key: &str,
    provider: &str,
    mode: &str,
    input: Option<f64>,
    output: Option<f64>,
    read: Option<f64>,
    write: Option<f64>,
) -> String {
    let mut fields = format!("\"litellm_provider\": \"{provider}\", \"mode\": \"{mode}\"");
    for (name, v) in [
        ("input_cost_per_token", input),
        ("output_cost_per_token", output),
        ("cache_read_input_token_cost", read),
        ("cache_creation_input_token_cost", write),
    ] {
        if let Some(v) = v {
            fields.push_str(&format!(", \"{name}\": {v:e}"));
        }
    }
    format!("\"{key}\": {{{fields}}}")
}

fn litellm_body(entries: &[String]) -> String {
    format!("{{{}}}", entries.join(","))
}

fn rate(input: f64, read: Option<f64>, write: Option<f64>, output: f64) -> Rate {
    Rate {
        input,
        cache_read: read,
        cache_write: write,
        output,
        long_context: None,
    }
}

fn key(provider: &str, id: &str) -> (String, String) {
    (provider.to_owned(), id.to_owned())
}

fn row(provider: &str, prefix: &str, rates: [f64; 4], source: &str) -> Row {
    Row {
        provider: provider.to_owned(),
        prefix: prefix.to_owned(),
        input: rates[0],
        cache_read: rates[1],
        cache_write: rates[2],
        output: rates[3],
        source: source.to_owned(),
        long_context: None,
    }
}

fn table(read_on: &str, rows: Vec<Row>) -> Table {
    Table {
        read_on: read_on.to_owned(),
        rows: rows
            .into_iter()
            .map(|r| ((r.provider.clone(), r.prefix.clone()), r))
            .collect(),
        unit_rows: Vec::new(),
    }
}

/// One priced model per provider on both sides, so `merge`'s emptiness
/// refusal is satisfied and a test can add the case it is about.
fn baseline() -> (Prices, Prices) {
    let mut or = Prices::new();
    let mut ll = Prices::new();
    for (p, id) in [
        ("anthropic", "claude-base"),
        ("google", "gemini-base"),
        ("meta", "muse-spark-base"),
        ("openai", "gpt-base"),
        ("xai", "grok-base"),
    ] {
        or.insert(key(p, id), rate(1.0, Some(0.1), None, 4.0));
        ll.insert(key(p, id), rate(1.0, Some(0.1), None, 4.0));
    }
    (or, ll)
}

fn empty_table() -> Table {
    table("2026-01-01", Vec::new())
}

// ── Argument parsing ─────────────────────────────────────────────────────────

#[test]
fn mode_parses_write_check_and_rejects_the_rest() {
    assert_eq!(PricesMode::parse(&[]).unwrap(), PricesMode::Write);
    assert_eq!(
        PricesMode::parse(&["--check".to_owned()]).unwrap(),
        PricesMode::Check
    );
    let err = PricesMode::parse(&["--force".to_owned()]).unwrap_err();
    assert!(err.to_string().contains("--force"));
}

// ── OpenRouter parser ────────────────────────────────────────────────────────

#[test]
fn openrouter_keeps_the_vendors_per_million_and_normalises_anthropic() {
    let body = openrouter_body(&[
        or_model("anthropic/claude-opus-4.8", "0.000005", "0.000025", Some("0.0000005"), Some("0.00000625")),
        or_model("openai/gpt-5.5", "0.000005", "0.00003", Some("0.0000005"), None),
        or_model("google/gemini-3.5-flash", "0.0000015", "0.000009", None, None),
        or_model("x-ai/grok-4.6", "0.000003", "0.000015", None, None),
        or_model("openai/gpt-5.5:batch", "0.0000025", "0.000015", None, None),
        or_model("google/lyria-3", "0", "0", None, None),
        "{\"id\": \"no-slash\", \"pricing\": {\"prompt\": \"0.1\", \"completion\": \"0.2\"}}".to_owned(),
        "{\"id\": \"openai/no-pricing\"}".to_owned(),
        "{\"id\": \"openai/bad-number\", \"pricing\": {\"prompt\": \"abc\", \"completion\": \"0.1\"}}".to_owned(),
        "{\"name\": \"no id\"}".to_owned(),
    ]);
    let prices = parse_openrouter(&body).unwrap();
    let ids: Vec<&(String, String)> = prices.keys().collect();
    assert_eq!(
        ids,
        vec![
            &key("anthropic", "claude-opus-4-8"),
            &key("google", "gemini-3.5-flash"),
            &key("openai", "gpt-5.5"),
            &key("xai", "grok-4.6"),
        ]
    );
    let opus = &prices[&key("anthropic", "claude-opus-4-8")];
    assert_eq!(opus.input, 5.0);
    assert_eq!(opus.output, 25.0);
    assert_eq!(opus.cache_read, Some(0.5));
    assert_eq!(opus.cache_write, Some(6.25));
    let gpt = &prices[&key("openai", "gpt-5.5")];
    assert_eq!(gpt.cache_read, Some(0.5));
    assert_eq!(gpt.cache_write, None);
}

#[test]
fn openrouter_rejects_a_body_that_is_not_its_shape() {
    assert!(parse_openrouter("not json").is_err());
    assert!(parse_openrouter("{\"models\": []}").is_err());
    assert!(parse_openrouter("{\"data\": []}").unwrap().is_empty());
}

#[test]
fn per_million_reads_strings_and_numbers_and_nothing_else() {
    assert_eq!(per_million(None), None);
    assert_eq!(per_million(Some(&serde_json::json!(true))), None);
    assert_eq!(per_million(Some(&serde_json::json!("0.000001"))), Some(1.0));
    assert_eq!(per_million(Some(&serde_json::json!(0.000002))), Some(2.0));
}

// ── LiteLLM parser ───────────────────────────────────────────────────────────

#[test]
fn litellm_maps_providers_filters_chat_and_strips_the_gemini_prefix() {
    let body = litellm_body(&[
        ll_entry(
            "gpt-5.5",
            "openai",
            "chat",
            Some(5e-6),
            Some(3e-5),
            Some(5e-7),
            None,
        ),
        ll_entry(
            "claude-opus-4-8",
            "anthropic",
            "chat",
            Some(5e-6),
            Some(2.5e-5),
            Some(5e-7),
            Some(6.25e-6),
        ),
        ll_entry(
            "gemini/gemini-3.5-flash",
            "gemini",
            "chat",
            Some(1.5e-6),
            Some(9e-6),
            None,
            None,
        ),
        ll_entry(
            "text-embedding-3",
            "openai",
            "embedding",
            Some(1e-7),
            Some(0.0),
            None,
            None,
        ),
        ll_entry(
            "ft:gpt-4o",
            "openai",
            "chat",
            Some(3e-6),
            Some(1.2e-5),
            None,
            None,
        ),
        ll_entry(
            "vertex_ai/gemini/gemini-x",
            "gemini",
            "chat",
            Some(1e-6),
            Some(2e-6),
            None,
            None,
        ),
        ll_entry(
            "mistral-large",
            "mistral",
            "chat",
            Some(1e-6),
            Some(2e-6),
            None,
            None,
        ),
        ll_entry("openai/container", "openai", "chat", None, None, None, None),
        ll_entry(
            "gemini/gemini-exp",
            "gemini",
            "chat",
            Some(0.0),
            Some(0.0),
            None,
            None,
        ),
        "\"sample_spec\": \"not an object\"".to_owned(),
    ]);
    let prices = parse_litellm(&body).unwrap();
    let ids: Vec<&(String, String)> = prices.keys().collect();
    assert_eq!(
        ids,
        vec![
            &key("anthropic", "claude-opus-4-8"),
            &key("google", "gemini-3.5-flash"),
            &key("openai", "gpt-5.5"),
        ]
    );
    assert_eq!(prices[&key("google", "gemini-3.5-flash")].input, 1.5);
    assert_eq!(
        prices[&key("anthropic", "claude-opus-4-8")].cache_write,
        Some(6.25)
    );
}

#[test]
fn litellm_collapses_agreeing_copies_and_drops_disagreeing_ones() {
    let body = litellm_body(&[
        ll_entry(
            "gemini/gemini-pro",
            "gemini",
            "chat",
            Some(1.25e-6),
            Some(1e-5),
            None,
            None,
        ),
        ll_entry(
            "gemini-pro",
            "gemini",
            "chat",
            Some(1.25e-6),
            Some(1e-5),
            None,
            None,
        ),
        ll_entry(
            "gemini/gemini-flash",
            "gemini",
            "chat",
            Some(3e-7),
            Some(2.5e-6),
            None,
            None,
        ),
        ll_entry(
            "gemini-flash",
            "gemini",
            "chat",
            Some(6e-7),
            Some(2.5e-6),
            None,
            None,
        ),
    ]);
    let prices = parse_litellm(&body).unwrap();
    assert_eq!(prices.len(), 1);
    assert!(prices.contains_key(&key("google", "gemini-pro")));
}

#[test]
fn litellm_rejects_a_body_that_is_not_an_object() {
    assert!(parse_litellm("[]").is_err());
    assert!(parse_litellm("nope").is_err());
}

// ── Merge rules ──────────────────────────────────────────────────────────────

#[test]
fn agreement_within_five_percent_writes_both_at_openrouters_figure() {
    let (mut or, mut ll) = baseline();
    or.insert(key("openai", "gpt-9"), rate(2.0, Some(0.2), None, 12.0));
    ll.insert(key("openai", "gpt-9"), rate(2.08, Some(0.2), None, 12.4));
    let merged = merge(&empty_table(), &or, &ll, "2026-08-29").unwrap();
    let row = &merged.table.rows[&key("openai", "gpt-9")];
    assert_eq!(row.input, 2.0);
    assert_eq!(row.output, 12.0);
    assert_eq!(row.cache_read, 0.2);
    assert_eq!(row.cache_write, 2.0, "no write premium published: input");
    assert_eq!(row.source, "both");
    assert_eq!(merged.table.read_on, "2026-08-29");
    assert!(merged.disagreements.is_empty());
    assert_eq!(merged.changes.len(), 6, "five baseline rows plus this one");
}

#[test]
fn a_model_only_one_source_prices_is_written_with_that_source() {
    let (mut or, mut ll) = baseline();
    or.insert(
        key("anthropic", "claude-only-or"),
        rate(5.0, Some(0.5), Some(6.25), 25.0),
    );
    ll.insert(key("google", "gemini-only-ll"), rate(0.5, None, None, 3.0));
    let merged = merge(&empty_table(), &or, &ll, "2026-08-29").unwrap();
    let a = &merged.table.rows[&key("anthropic", "claude-only-or")];
    assert_eq!(a.source, "openrouter");
    assert_eq!(a.cache_write, 6.25, "a premium is taken");
    let g = &merged.table.rows[&key("google", "gemini-only-ll")];
    assert_eq!(g.source, "litellm");
    assert_eq!((g.cache_read, g.cache_write), (0.5, 0.5));
}

#[test]
fn a_disagreement_keeps_the_existing_row_and_is_reported() {
    let (mut or, mut ll) = baseline();
    or.insert(key("openai", "gpt-9"), rate(2.0, None, None, 12.0));
    ll.insert(key("openai", "gpt-9"), rate(2.5, None, None, 12.0));
    or.insert(key("openai", "gpt-new"), rate(1.0, None, None, 2.0));
    ll.insert(key("openai", "gpt-new"), rate(1.0, None, None, 3.0));
    let existing = table(
        "2026-01-01",
        vec![row("openai", "gpt-9", [1.9, 0.19, 1.9, 11.0], "both")],
    );
    let merged = merge(&existing, &or, &ll, "2026-08-29").unwrap();
    assert_eq!(
        merged.table.rows[&key("openai", "gpt-9")],
        row("openai", "gpt-9", [1.9, 0.19, 1.9, 11.0], "both")
    );
    assert!(!merged.table.rows.contains_key(&key("openai", "gpt-new")));
    assert_eq!(merged.disagreements.len(), 2);
    assert!(
        merged.disagreements[0]
            .contains("openai/gpt-9: openrouter 2.0/-/-/12.0 vs litellm 2.5/-/-/12.0")
    );
}

#[test]
fn a_manual_row_is_never_overwritten() {
    let (mut or, mut ll) = baseline();
    or.insert(key("google", "gemini-pinned"), rate(9.0, None, None, 90.0));
    ll.insert(key("google", "gemini-pinned"), rate(9.0, None, None, 90.0));
    let pinned = row("google", "gemini-pinned", [4.0, 0.4, 4.0, 40.0], "manual");
    let existing = table("2026-01-01", vec![pinned.clone()]);
    let merged = merge(&existing, &or, &ll, "2026-08-29").unwrap();
    assert_eq!(merged.table.rows[&key("google", "gemini-pinned")], pinned);
    assert!(
        merged
            .changes
            .iter()
            .all(|c| !c.to_string().contains("pinned"))
    );
}

#[test]
fn a_dated_variant_at_the_same_price_collapses_into_its_family() {
    let (mut or, mut ll) = baseline();
    for id in [
        "gpt-9",
        "gpt-9-2026-04-23",
        "gpt-9-mini",
        "gpt-9-mini-2026-05-01",
    ] {
        let (i, o) = if id.contains("mini") {
            (0.5, 3.0)
        } else {
            (2.0, 12.0)
        };
        or.insert(key("openai", id), rate(i, None, None, o));
        ll.insert(key("openai", id), rate(i, None, None, o));
    }
    let merged = merge(&empty_table(), &or, &ll, "2026-08-29").unwrap();
    let openai: Vec<&str> = merged
        .table
        .rows
        .keys()
        .filter(|(p, _)| p == "openai")
        .map(|(_, id)| id.as_str())
        .collect();
    assert_eq!(openai, vec!["gpt-9", "gpt-9-mini", "gpt-base"]);
}

#[test]
fn an_existing_row_is_refreshed_not_collapsed() {
    let (mut or, mut ll) = baseline();
    or.insert(
        key("anthropic", "claude-sonnet-4"),
        rate(3.0, None, None, 15.0),
    );
    ll.insert(
        key("anthropic", "claude-sonnet-4"),
        rate(3.0, None, None, 15.0),
    );
    or.insert(
        key("anthropic", "claude-sonnet-4-6"),
        rate(3.0, Some(0.3), Some(3.75), 15.0),
    );
    ll.insert(
        key("anthropic", "claude-sonnet-4-6"),
        rate(3.0, Some(0.3), Some(3.75), 15.0),
    );
    let existing = table(
        "2026-01-01",
        vec![row(
            "anthropic",
            "claude-sonnet-4-6",
            [3.0, 0.3, 3.75, 15.0],
            "openrouter",
        )],
    );
    let merged = merge(&existing, &or, &ll, "2026-08-29").unwrap();
    let refreshed = &merged.table.rows[&key("anthropic", "claude-sonnet-4-6")];
    assert_eq!(refreshed.source, "both", "the row is refreshed in place");
    assert!(
        merged
            .table
            .rows
            .contains_key(&key("anthropic", "claude-sonnet-4"))
    );
    let change = merged
        .changes
        .iter()
        .find(|c| matches!(c, Change::Changed(..)))
        .expect("a source change is a change");
    assert!(
        change
            .to_string()
            .contains("(openrouter) -> 3.0/0.3/3.75/15.0 (both)")
    );
}

#[test]
fn nothing_new_leaves_the_table_and_its_date_alone() {
    let (or, ll) = baseline();
    let merged = merge(&empty_table(), &or, &ll, "2026-08-29").unwrap();
    let again = merge(&merged.table, &or, &ll, "2026-12-25").unwrap();
    assert_eq!(again.table, merged.table);
    assert!(again.changes.is_empty());
    assert_eq!(again.table.read_on, "2026-08-29");
}

#[test]
fn a_cache_write_below_input_is_storage_not_a_rate() {
    let (mut or, mut ll) = baseline();
    or.insert(
        key("google", "gemini-x"),
        rate(1.5, Some(0.15), Some(0.0416667), 9.0),
    );
    ll.insert(key("google", "gemini-x"), rate(1.5, Some(0.15), None, 9.0));
    let merged = merge(&empty_table(), &or, &ll, "2026-08-29").unwrap();
    let g = &merged.table.rows[&key("google", "gemini-x")];
    assert_eq!(g.cache_write, 1.5);
    assert_eq!(g.cache_read, 0.15);
}

#[test]
fn a_zero_cache_read_defaults_to_input() {
    let r = rate(2.0, Some(0.0), Some(2.5), 8.0);
    assert_eq!(r.resolve(), (2.0, 2.0, 2.5, 8.0));
}

#[test]
fn a_move_over_three_times_is_refused() {
    let (mut or, mut ll) = baseline();
    or.insert(key("openai", "gpt-9"), rate(2.0, None, None, 12.0));
    ll.insert(key("openai", "gpt-9"), rate(2.0, None, None, 12.0));
    let existing = table(
        "2026-01-01",
        vec![row("openai", "gpt-9", [2.0, 0.2, 2.0, 50.0], "both")],
    );
    let err = merge(&existing, &or, &ll, "2026-08-29").unwrap_err();
    assert!(
        err.to_string().contains("openai/gpt-9 would move by 4.2x"),
        "{err}"
    );

    let existing = table(
        "2026-01-01",
        vec![row("openai", "gpt-9", [7.0, 0.7, 7.0, 12.0], "both")],
    );
    assert!(merge(&existing, &or, &ll, "2026-08-29").is_err());

    let existing = table(
        "2026-01-01",
        vec![row("openai", "gpt-9", [1.0, 0.1, 1.0, 12.0], "both")],
    );
    assert!(
        merge(&existing, &or, &ll, "2026-08-29").is_ok(),
        "2x is a repricing"
    );
}

#[test]
fn a_source_with_nothing_for_a_provider_is_refused() {
    let (or, ll) = baseline();
    let mut no_google = or.clone();
    no_google.retain(|(p, _), _| p != "google");
    let err = merge(&empty_table(), &no_google, &ll, "2026-08-29").unwrap_err();
    assert!(
        err.to_string()
            .contains("OpenRouter lists no priced google"),
        "{err}"
    );
    let mut no_openai = ll.clone();
    no_openai.retain(|(p, _), _| p != "openai");
    let err = merge(&empty_table(), &or, &no_openai, "2026-08-29").unwrap_err();
    assert!(
        err.to_string().contains("LiteLLM lists no priced openai"),
        "{err}"
    );
}

#[test]
fn a_row_without_a_positive_price_is_refused() {
    let (or, ll) = baseline();
    let existing = table(
        "2026-01-01",
        vec![row("openai", "gpt-free", [1.0, 0.1, 1.0, 0.0], "manual")],
    );
    let err = merge(&existing, &or, &ll, "2026-08-29").unwrap_err();
    assert!(
        err.to_string()
            .contains("openai/gpt-free has no positive price"),
        "{err}"
    );
}

#[test]
fn agreement_compares_every_side_both_publish() {
    let a = rate(1.0, Some(0.1), Some(1.25), 4.0);
    assert!(a.agrees(&rate(1.04, Some(0.1), None, 4.0)));
    assert!(!a.agrees(&rate(1.0, Some(0.2), Some(1.25), 4.0)));
    assert!(!a.agrees(&rate(1.0, Some(0.1), Some(2.0), 4.0)));
    assert!(!a.agrees(&rate(1.0, None, None, 4.3)));
}

// ── Rendering ────────────────────────────────────────────────────────────────

#[test]
fn the_file_round_trips_sorted_with_float_literals() {
    let t = table(
        "2026-08-29",
        vec![
            row("openai", "gpt-9", [5.0, 0.5, 5.0, 30.0], "both"),
            row(
                "anthropic",
                "claude-9",
                [0.075, 0.0075, 0.09375, 0.375],
                "litellm",
            ),
        ],
    );
    let text = render_table(&t);
    assert!(text.starts_with("# Published list prices"));
    assert!(text.contains("read_on = \"2026-08-29\""));
    assert!(text.contains("input = 5.0\n"));
    assert!(text.contains("cache_read = 0.0075\n"));
    let anthropic_at = text.find("prefix = \"claude-9\"").unwrap();
    let openai_at = text.find("prefix = \"gpt-9\"").unwrap();
    assert!(anthropic_at < openai_at, "sorted by provider");
    assert_eq!(parse_table(&text).unwrap(), t);
}

#[test]
fn the_shipped_file_parses_and_renders_to_itself() {
    let path = workspace_root().join(RATES_FILE);
    // A Windows checkout with `core.autocrlf` hands us CRLF; the refresh
    // always writes LF, so compare the file as git stores it.
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("\r\n", "\n");
    let t = parse_table(&text).unwrap();
    assert!(!t.rows.is_empty());
    assert_eq!(
        render_table(&t),
        text,
        "the file is what the refresh would write"
    );
}

#[test]
fn a_malformed_file_is_an_error() {
    assert!(parse_table("read_on = 5").is_err());
    assert!(parse_table("[[rate]]\nprovider = \"x\"").is_err());
}

#[test]
fn numbers_print_as_the_shortest_float() {
    assert_eq!(fmt_num(5.0), "5.0");
    assert_eq!(fmt_num(0.3), "0.3");
    assert_eq!(fmt_num(round6(0.1 + 0.2)), "0.3");
    assert_eq!(fmt_num(round6(0.0000000416666 * 1e6)), "0.041667");
}

#[test]
fn a_change_prints_old_and_new() {
    let added = Change::Added(row("openai", "gpt-9", [5.0, 0.5, 5.0, 30.0], "both"));
    assert_eq!(added.to_string(), "+ openai/gpt-9: 5.0/0.5/5.0/30.0 (both)");
    let changed = Change::Changed(
        row("openai", "gpt-9", [5.0, 0.5, 5.0, 30.0], "both"),
        row("openai", "gpt-9", [4.0, 0.4, 4.0, 20.0], "openrouter"),
    );
    assert_eq!(
        changed.to_string(),
        "~ openai/gpt-9: 5.0/0.5/5.0/30.0 (both) -> 4.0/0.4/4.0/20.0 (openrouter)"
    );
}

// ── run_with ─────────────────────────────────────────────────────────────────

fn fixture_openrouter() -> String {
    openrouter_body(&[
        or_model(
            "anthropic/claude-opus-4.8",
            "0.000005",
            "0.000025",
            Some("0.0000005"),
            Some("0.00000625"),
        ),
        or_model(
            "openai/gpt-5.5",
            "0.000005",
            "0.00003",
            Some("0.0000005"),
            None,
        ),
        or_model(
            "google/gemini-3.5-flash",
            "0.0000015",
            "0.000009",
            Some("0.00000015"),
            Some("0.00000004"),
        ),
        or_model(
            "meta/muse-spark-1.3",
            "0.00000125",
            "0.00000425",
            Some("0.00000015"),
            None,
        ),
        or_model("x-ai/grok-4.3", "0.000002", "0.00001", None, None),
    ])
}

fn fixture_litellm() -> String {
    litellm_body(&[
        ll_entry(
            "gpt-5.5",
            "openai",
            "chat",
            Some(5e-6),
            Some(3e-5),
            Some(5e-7),
            None,
        ),
        ll_entry(
            "claude-opus-4-8",
            "anthropic",
            "chat",
            Some(5e-6),
            Some(2.5e-5),
            Some(5e-7),
            Some(6.25e-6),
        ),
        ll_entry(
            "gemini/gemini-3.5-flash",
            "gemini",
            "chat",
            Some(1.5e-6),
            Some(9e-6),
            Some(1.5e-7),
            None,
        ),
        ll_entry(
            "meta/muse-spark-1.3",
            "meta",
            "chat",
            Some(1.25e-6),
            Some(4.25e-6),
            Some(1.5e-7),
            None,
        ),
        ll_entry(
            "xai/grok-4.3",
            "xai",
            "chat",
            Some(2e-6),
            Some(1e-5),
            None,
            None,
        ),
    ])
}

fn fixture_fetch(url: &str) -> Result<String> {
    if url.contains("openrouter") {
        Ok(fixture_openrouter())
    } else {
        Ok(fixture_litellm())
    }
}

fn failing_fetch(url: &str) -> Result<String> {
    Err(NetworkError(format!("{url}: connection refused")).into())
}

fn garbage_fetch(_url: &str) -> Result<String> {
    Ok("<html>".to_owned())
}

fn scratch_file(text: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rates.toml");
    std::fs::write(&path, text).unwrap();
    (dir, path)
}

const EMPTY_FILE: &str = "read_on = \"2026-01-01\"\n";

#[test]
fn write_mode_rewrites_the_file_and_stamps_today() {
    let (_dir, path) = scratch_file(EMPTY_FILE);
    let outcome = run_with(PricesMode::Write, fixture_fetch, &path, "2026-08-29").unwrap();
    assert_eq!(outcome, Outcome::Changed(5));
    let written = parse_table(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(written.read_on, "2026-08-29");
    assert_eq!(written.rows.len(), 5);
    let gemini = &written.rows[&key("google", "gemini-3.5-flash")];
    assert_eq!(gemini.cache_write, 1.5, "storage figure rejected");
    assert_eq!(gemini.source, "both");

    // A second run on the same sources changes nothing and keeps the date.
    let again = run_with(PricesMode::Write, fixture_fetch, &path, "2026-12-25").unwrap();
    assert_eq!(again, Outcome::Unchanged);
    assert_eq!(
        parse_table(&std::fs::read_to_string(&path).unwrap())
            .unwrap()
            .read_on,
        "2026-08-29"
    );
}

#[test]
fn check_mode_fails_when_the_file_would_change_and_touches_nothing() {
    let (_dir, path) = scratch_file(EMPTY_FILE);
    let err = run_with(PricesMode::Check, fixture_fetch, &path, "2026-08-29").unwrap_err();
    assert!(err.to_string().contains("would change (5 rows)"), "{err}");
    assert!(!is_network_error(&err));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), EMPTY_FILE);
}

#[test]
fn check_mode_passes_a_current_file() {
    let (_dir, path) = scratch_file(EMPTY_FILE);
    run_with(PricesMode::Write, fixture_fetch, &path, "2026-08-29").unwrap();
    let outcome = run_with(PricesMode::Check, fixture_fetch, &path, "2026-08-30").unwrap();
    assert_eq!(outcome, Outcome::Unchanged);
}

#[test]
fn a_network_failure_is_distinguished_and_touches_nothing() {
    let (_dir, path) = scratch_file(EMPTY_FILE);
    let err = run_with(PricesMode::Write, failing_fetch, &path, "2026-08-29").unwrap_err();
    assert!(is_network_error(&err));
    assert!(
        err.to_string().contains("network: https://openrouter.ai"),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), EMPTY_FILE);
}

#[test]
fn a_garbage_body_is_a_data_error_not_a_network_one() {
    let (_dir, path) = scratch_file(EMPTY_FILE);
    let err = run_with(PricesMode::Write, garbage_fetch, &path, "2026-08-29").unwrap_err();
    assert!(!is_network_error(&err));
    assert!(err.to_string().contains("OpenRouter: not JSON"), "{err}");
}

#[test]
fn a_missing_or_unparseable_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.toml");
    assert!(run_with(PricesMode::Write, fixture_fetch, &missing, "2026-08-29").is_err());
    let (_dir, bad) = scratch_file("read_on = 5\n");
    assert!(run_with(PricesMode::Write, fixture_fetch, &bad, "2026-08-29").is_err());
}

#[test]
fn a_refusal_leaves_the_file_untouched() {
    let text = render_table(&table(
        "2026-01-01",
        vec![row("openai", "gpt-5.5", [1.0, 0.1, 1.0, 5.0], "both")],
    ));
    let (_dir, path) = scratch_file(&text);
    let err = run_with(PricesMode::Write, fixture_fetch, &path, "2026-08-29").unwrap_err();
    assert!(err.to_string().contains("would move by 6.0x"), "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
}

#[test]
fn today_is_a_civil_date_and_the_root_holds_the_table() {
    assert_eq!(today().len(), "YYYY-MM-DD".len());
    assert!(workspace_root().join(RATES_FILE).is_file());
}

#[test]
fn the_network_error_reads_as_one() {
    let err = NetworkError("x".to_owned());
    assert_eq!(err.to_string(), "network: x");
    assert!(std::error::Error::source(&err).is_none());
}

// ── New vendors, tiers and unit rows ────────────────────────────────────────

#[test]
fn openrouter_ids_map_x_ai_to_xai_and_keep_only_metas_served_models() {
    assert_eq!(
        openrouter_model("x-ai/grok-4.3", &PROVIDERS),
        Some(key("xai", "grok-4.3"))
    );
    assert_eq!(
        openrouter_model("meta/muse-spark-1.3", &PROVIDERS),
        Some(key("meta", "muse-spark-1.3"))
    );
    assert_eq!(openrouter_model("meta/muse-glimmer-30b", &PROVIDERS), None);
    assert_eq!(openrouter_model("meta-llama/llama-4", &PROVIDERS), None);
    assert_eq!(openrouter_model("x-ai/grok-4.3:batch", &PROVIDERS), None);
    assert_eq!(openrouter_model("no-slash", &PROVIDERS), None);
}

#[test]
fn litellm_reads_the_new_vendors_and_the_smallest_long_context_tier() {
    let tiered = "\"xai/grok-4.3\": {\"litellm_provider\": \"xai\", \"mode\": \"chat\", \
        \"input_cost_per_token\": 2e-6, \"output_cost_per_token\": 1e-5, \
        \"input_cost_per_token_above_200k_tokens\": 4e-6, \"output_cost_per_token_above_200k_tokens\": 2e-5, \
        \"cache_read_input_token_cost_above_200k_tokens\": 1e-6, \
        \"input_cost_per_token_above_500k_tokens\": 8e-6, \"output_cost_per_token_above_500k_tokens\": 4e-5, \
        \"input_cost_per_token_above_xk_tokens\": 1e-6}"
        .to_owned();
    let half = "\"meta/muse-spark-1.3\": {\"litellm_provider\": \"meta\", \"mode\": \"chat\", \
        \"input_cost_per_token\": 1.25e-6, \"output_cost_per_token\": 4.25e-6, \
        \"input_cost_per_token_above_128k_tokens\": 2e-6}"
        .to_owned();
    let glimmer = ll_entry(
        "meta/muse-glimmer-30b",
        "meta",
        "chat",
        Some(1e-7),
        Some(2e-7),
        None,
        None,
    );
    let prices = parse_litellm(&litellm_body(&[tiered, half, glimmer])).unwrap();
    assert_eq!(prices.len(), 2, "Glimmer is not served by Meta's API");
    let grok = &prices[&key("xai", "grok-4.3")];
    let tier = grok.long_context.expect("a tier");
    assert_eq!(tier.threshold, 200_000);
    assert_eq!(tier.input, 4.0);
    assert_eq!(tier.cache_read, Some(1.0));
    assert_eq!(tier.cache_write, None);
    assert_eq!(tier.output, 20.0);
    assert_eq!(
        prices[&key("meta", "muse-spark-1.3")].long_context,
        None,
        "a tier with no output price is no tier"
    );
}

#[test]
fn a_tier_rides_on_the_row_whichever_source_vouched_and_renders_back() {
    let (mut or, mut ll) = baseline();
    let tier = TierRate {
        threshold: 200_000,
        input: 4.0,
        cache_read: None,
        cache_write: Some(1.0),
        output: 20.0,
    };
    or.insert(key("xai", "grok-4.3"), rate(2.0, None, None, 10.0));
    ll.insert(
        key("xai", "grok-4.3"),
        Rate {
            long_context: Some(tier),
            ..rate(2.0, None, None, 10.0)
        },
    );
    let merged = merge(&empty_table(), &or, &ll, "2026-09-16").unwrap();
    let row = &merged.table.rows[&key("xai", "grok-4.3")];
    assert_eq!(row.source, "both");
    assert_eq!(
        row.long_context,
        Some(Tier {
            threshold: 200_000,
            input: 4.0,
            cache_read: 4.0,
            cache_write: 4.0,
            output: 20.0,
        }),
        "the cache sides default the way a row's do"
    );
    let change = merged
        .changes
        .iter()
        .find(|c| matches!(c, Change::Added(r) if r.prefix == "grok-4.3"))
        .unwrap();
    assert!(
        change.to_string().contains("+4.0/4.0/4.0/20.0 from 200000"),
        "{change}"
    );

    let mut with_units = merged.table.clone();
    with_units.unit_rows.push(UnitRow {
        provider: "meta".into(),
        prefix: "muse-image-1.0".into(),
        unit: "image".into(),
        usd: 0.01,
        source: "manual".into(),
        checked_on: "2026-09-16".into(),
    });
    let text = render_table(&with_units);
    assert!(
        text.contains("[rate.long_context]\nthreshold = 200000\n"),
        "{text}"
    );
    assert!(
        text.contains("[[unit_rate]]\nprovider = \"meta\""),
        "{text}"
    );
    assert_eq!(parse_table(&text).unwrap(), with_units);
}

#[test]
fn unit_rows_survive_a_refresh_and_an_old_one_is_reported() {
    let mut existing = empty_table();
    existing.unit_rows = vec![
        UnitRow {
            provider: "xai".into(),
            prefix: "grok-tts".into(),
            unit: "million_chars".into(),
            usd: 15.0,
            source: "manual".into(),
            checked_on: "2026-01-01".into(),
        },
        UnitRow {
            provider: "meta".into(),
            prefix: "muse-image-1.0".into(),
            unit: "image".into(),
            usd: 0.01,
            source: "manual".into(),
            checked_on: "2026-09-01".into(),
        },
        UnitRow {
            provider: "meta".into(),
            prefix: "muse-voice".into(),
            unit: "audio_hour".into(),
            usd: 0.18,
            source: "manual".into(),
            checked_on: "someday".into(),
        },
    ];
    let (or, ll) = baseline();
    let merged = merge(&existing, &or, &ll, "2026-09-16").unwrap();
    assert_eq!(merged.table.unit_rows, existing.unit_rows);
    let stale = stale_unit_rows(&existing, "2026-09-16");
    assert_eq!(stale.len(), 2, "{stale:?}");
    assert!(stale[0].contains("xai/grok-tts"));
    assert!(stale[1].contains("'someday'"));
    assert!(stale_unit_rows(&existing, "not a day").is_empty());
}

/// Image, speech and transcription models priced by the token are read like
/// chat models; an image model with no plain output rate is priced by its
/// image tokens.
#[test]
fn litellm_reads_the_media_models_priced_by_the_token() {
    let image =
        "\"gpt-image-1\": {\"litellm_provider\": \"openai\", \"mode\": \"image_generation\", \
                 \"input_cost_per_token\": 5e-06, \"output_cost_per_image_token\": 4e-05}"
            .to_owned();
    let body = litellm_body(&[
        image,
        ll_entry(
            "gpt-4o-mini-tts",
            "openai",
            "audio_speech",
            Some(6e-7),
            Some(1e-5),
            None,
            None,
        ),
        ll_entry(
            "sora-2",
            "openai",
            "video_generation",
            Some(1e-6),
            Some(1e-6),
            None,
            None,
        ),
    ]);
    let prices = parse_litellm(&body).unwrap();
    assert_eq!(prices[&key("openai", "gpt-image-1")].output, 40.0);
    assert_eq!(prices[&key("openai", "gpt-4o-mini-tts")].input, 0.6);
    assert!(
        !prices.contains_key(&key("openai", "sora-2")),
        "a video model is priced by the second, not the token"
    );
}

#[test]
fn litellm_unit_prices_are_read_per_second_character_hour_image_and_clip() {
    let body = r#"{
        "sora-2": {"litellm_provider": "openai", "mode": "video_generation", "output_cost_per_video_per_second": 0.1},
        "openai/sora-2": {"litellm_provider": "openai", "mode": "video_generation", "output_cost_per_video_per_second": 0.1},
        "gemini/veo-3.1-lite-generate-preview": {"litellm_provider": "gemini", "mode": "video_generation", "output_cost_per_second": 0.05},
        "tts-1": {"litellm_provider": "openai", "mode": "audio_speech", "input_cost_per_character": 1.5e-05},
        "gpt-4o-mini-tts": {"litellm_provider": "openai", "mode": "audio_speech", "input_cost_per_token": 6e-07, "output_cost_per_second": 0.00025},
        "xai/grok-imagine-image": {"litellm_provider": "xai", "mode": "image_generation", "input_cost_per_image": 0.02},
        "whisper-1": {"litellm_provider": "openai", "mode": "audio_transcription", "input_cost_per_second": 0.0001},
        "stability.sd3-5-large-v1:0": {"litellm_provider": "bedrock", "mode": "image_generation", "output_cost_per_image": 0.08},
        "gemini/lyria-3-clip-preview": {"litellm_provider": "gemini", "mode": "chat", "output_cost_per_image": 0.04},
        "gemini/gemini-3.1-flash-image": {"litellm_provider": "gemini", "mode": "image_generation", "input_cost_per_token": 5e-07, "output_cost_per_image": 0.045},
        "gemini/veo-disagrees": {"litellm_provider": "gemini", "mode": "video_generation", "output_cost_per_second": 0.05},
        "veo-disagrees": {"litellm_provider": "gemini", "mode": "video_generation", "output_cost_per_second": 0.5},
        "ft:tts-1:x": {"litellm_provider": "openai", "mode": "audio_speech", "input_cost_per_character": 1e-05},
        "1024-x-1024/max-steps/stability.x": {"litellm_provider": "bedrock", "mode": "image_generation", "output_cost_per_image": 0.1},
        "mistral-image": {"litellm_provider": "mistral", "mode": "image_generation", "output_cost_per_image": 0.1},
        "free-video": {"litellm_provider": "openai", "mode": "video_generation", "output_cost_per_second": 0},
        "gpt-5.5": {"litellm_provider": "openai", "mode": "chat", "output_cost_per_image": 0.2},
        "a-note": "not an object"
    }"#;
    let rows = parse_litellm_units(body, "2026-09-17").unwrap();
    let found: Vec<(&str, &str, &str, f64)> = rows
        .iter()
        .map(|r| {
            (
                r.provider.as_str(),
                r.prefix.as_str(),
                r.unit.as_str(),
                r.usd,
            )
        })
        .collect();
    assert_eq!(
        found,
        vec![
            ("bedrock", "stability.sd3-5-large-v1:0", "image", 0.08),
            ("google", "lyria-3-clip-preview", "clip", 0.04),
            (
                "google",
                "veo-3.1-lite-generate-preview",
                "video_second",
                0.05
            ),
            ("openai", "gpt-4o-mini-tts", "audio_hour", 0.9),
            ("openai", "sora-2", "video_second", 0.1),
            ("openai", "tts-1", "million_chars", 15.0),
            ("openai", "whisper-1", "audio_hour", 0.36),
            ("xai", "grok-imagine-image", "image", 0.02),
        ]
    );
    assert!(
        rows.iter()
            .all(|r| r.source == LITELLM_UNIT_SOURCE && r.checked_on == "2026-09-17")
    );
    assert!(parse_litellm_units("[]", "2026-09-17").is_err());
    assert!(parse_litellm_units("nope", "2026-09-17").is_err());
}

#[test]
fn unit_rows_keep_a_persons_rows_and_refresh_litellms() {
    let unit = |provider: &str, prefix: &str, usd: f64, source: &str, checked_on: &str| UnitRow {
        provider: provider.into(),
        prefix: prefix.into(),
        unit: "image".into(),
        usd,
        source: source.into(),
        checked_on: checked_on.into(),
    };
    let existing = vec![
        unit("xai", "grok-imagine-image", 0.02, "manual", "2026-09-16"),
        unit("openai", "same", 0.1, "litellm", "2026-01-01"),
        unit("openai", "moved", 0.1, "litellm", "2026-01-01"),
        unit("openai", "gone", 0.1, "litellm", "2026-01-01"),
    ];
    let fresh = vec![
        unit("xai", "grok-imagine-image", 0.5, "litellm", "2026-09-17"),
        unit("openai", "same", 0.1, "litellm", "2026-09-17"),
        unit("openai", "moved", 0.2, "litellm", "2026-09-17"),
        unit("openai", "new", 0.3, "litellm", "2026-09-17"),
    ];
    let (rows, changes) = merge_units(&existing, fresh);
    assert_eq!(
        rows[0], existing[0],
        "a person's row wins and is kept as written"
    );
    let same = rows.iter().find(|r| r.prefix == "same").unwrap();
    assert_eq!(
        same.checked_on, "2026-01-01",
        "an unmoved row keeps its day"
    );
    assert!(rows.iter().all(|r| r.prefix != "gone"));
    assert_eq!(
        changes,
        vec![
            "~ unit openai/moved: 0.1 per image -> 0.2 per image",
            "+ unit openai/new: 0.3 per image",
            "- unit openai/gone: LiteLLM no longer prices it",
        ]
    );
    let (_, none) = merge_units(&rows, rows.clone());
    assert!(
        none.is_empty(),
        "a refresh with nothing new changes nothing"
    );

    let table = Table {
        read_on: "2026-09-17".into(),
        rows: Rows::new(),
        unit_rows: vec![unit("openai", "old-litellm", 0.1, "litellm", "2020-01-01")],
    };
    assert!(
        stale_unit_rows(&table, "2026-09-17").is_empty(),
        "LiteLLM's rows are read again"
    );
}

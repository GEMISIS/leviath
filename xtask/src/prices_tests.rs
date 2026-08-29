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
    }
}

fn table(read_on: &str, rows: Vec<Row>) -> Table {
    Table {
        read_on: read_on.to_owned(),
        rows: rows
            .into_iter()
            .map(|r| ((r.provider.clone(), r.prefix.clone()), r))
            .collect(),
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
        ("openai", "gpt-base"),
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
fn openrouter_keeps_the_three_vendors_per_million_and_normalises_anthropic() {
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
    assert_eq!(merged.changes.len(), 4, "three baseline rows plus this one");
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
    assert_eq!(outcome, Outcome::Changed(3));
    let written = parse_table(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(written.read_on, "2026-08-29");
    assert_eq!(written.rows.len(), 3);
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
    assert!(err.to_string().contains("would change (3 rows)"), "{err}");
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

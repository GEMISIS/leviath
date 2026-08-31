//! `cargo xtask prices` - refresh the vendor list prices without a person or
//! an AI in the loop.
//!
//! Anthropic, OpenAI and Google quote no price through their APIs, so
//! `crates/leviath-providers/pricing/rates.toml` carries their list prices and
//! a cost computed from it is only as current as the file. Keeping it current
//! by hand means transcription, and transcription is where a wrong digit gets
//! in. This command reads the same list prices from two places that do publish
//! them programmatically and writes the file only where they agree:
//!
//! * OpenRouter's public catalogue (`/api/v1/models`, no key), which carries
//!   each vendor's models under the vendor's own prefix with the vendor's list
//!   price, and is what Leviath already prices OpenRouter runs from;
//! * LiteLLM's `model_prices_and_context_window.json`, a community table of
//!   the same prices, as the cross-check.
//!
//! The rules are fixed so two runs on the same inputs write the same file:
//!
//! * a model both sources price within 5% is written with `source = "both"`,
//!   at OpenRouter's figure;
//! * a model only one source prices is written with that source's name;
//! * a model the two price more than 5% apart is not written; the existing
//!   row, if any, is kept and the disagreement is printed;
//! * a row whose `source` is `manual` is never overwritten;
//! * a model id is kept as the vendor writes it, minus the `openai/`,
//!   `anthropic/` or `google/` prefix. OpenRouter spells Anthropic's versions
//!   with a dot (`claude-opus-4.8`) where the API id has a dash
//!   (`claude-opus-4-8`), so those are normalised. Variants after a colon
//!   (`:batch`, `:free`, `:thinking`) are routing options, not models, and
//!   are dropped;
//! * a new id that a shorter new id covers as a prefix at the same price, such
//!   as `gpt-5.5-2026-04-23` beside `gpt-5.5`, is not written. Lookup is by
//!   longest matching prefix, so the shorter row already prices it;
//! * a cache-read rate is taken as published. A cache-write rate is taken only
//!   when it is at least the input rate: a write is a premium on input, and a
//!   figure below input (OpenRouter lists Google's per-hour storage price in
//!   that field) is not a per-token rate. Either defaults to the input rate;
//! * a source entry with a zero or negative input or output price is not a
//!   price (a free tier, or a model priced by the image) and is dropped.
//!
//! And it refuses, leaving the file untouched, when any existing row would
//! move by more than 3x, when the finished table has a row with no positive
//! output price, or when either source lists nothing for one of the three
//! providers. Any of those is a broken source or a broken parser, and a bad
//! table is worse than a stale one.
//!
//! `--check` prints the diff and exits 1 if the file would change, touching
//! nothing. A network failure exits 2, also touching nothing.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Path of the price table, relative to the workspace root.
pub const RATES_FILE: &str = "crates/leviath-providers/pricing/rates.toml";

/// OpenRouter's public model catalogue, with each model's list price.
const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/models";

/// LiteLLM's price table, the cross-check.
const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// The providers the table prices, in the order the file sorts them.
const PROVIDERS: [&str; 3] = ["anthropic", "google", "openai"];

/// Two figures within this fraction of each other agree.
const AGREE_TOLERANCE: f64 = 0.05;

/// An existing row moving by more than this ratio, either way, is refused.
const REFUSE_RATIO: f64 = 3.0;

/// How long to wait on either source before calling it a network failure.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Header written above the rows, so the file explains itself.
const FILE_HEADER: &str = "\
# Published list prices, USD per million tokens, for the providers whose APIs
# do not quote them. Read by `leviath_providers::pricing::published_rates`
# (longest matching `prefix` wins) and rewritten by `cargo xtask prices`, which
# takes the figures from OpenRouter's catalogue cross-checked against LiteLLM.
#
# `source` records where a row came from: `both` when the two sources agreed,
# `openrouter` or `litellm` when only one listed it, and `manual` for a row a
# person wrote, which the refresh never overwrites.
";

// ── CLI argument parsing ─────────────────────────────────────────────────────

/// What `cargo xtask prices` was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum PricesMode {
    /// Fetch, merge, and rewrite the file when the rows changed.
    Write,
    /// Fetch, merge, print the diff, and fail if the file would change.
    Check,
}

impl PricesMode {
    /// Parse the arguments after `prices`.
    pub fn parse(args: &[String]) -> Result<Self> {
        match args.first().map(String::as_str) {
            None => Ok(Self::Write),
            Some("--check") => Ok(Self::Check),
            Some(other) => anyhow::bail!("Unknown `prices` argument: '{other}'. Try `--check`."),
        }
    }
}

// ── Fetching ─────────────────────────────────────────────────────────────────

/// A fetch of a URL to its body. A plain `fn` pointer so the merge can be
/// tested against fixture JSON without a network.
pub type Fetch = fn(&str) -> Result<String>;

/// A source could not be read. Distinguished from every other failure because
/// it maps to exit 2 and says nothing about the table.
#[derive(Debug)]
pub struct NetworkError(pub String);

impl fmt::Display for NetworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "network: {}", self.0)
    }
}

impl std::error::Error for NetworkError {}

/// Whether an error from [`run_with`] was the network rather than the data.
pub fn is_network_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NetworkError>().is_some()
}

/// The real fetch: a GET with a bounded wait, any failure a [`NetworkError`].
fn fetch_http(url: &str) -> Result<String> {
    let body = reqwest::blocking::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent("leviath-xtask-prices")
        .build()
        .and_then(|client| client.get(url).send())
        .and_then(reqwest::blocking::Response::error_for_status)
        .and_then(reqwest::blocking::Response::text)
        .map_err(|e| NetworkError(format!("{url}: {e}")))?;
    Ok(body)
}

// ── The table ────────────────────────────────────────────────────────────────

/// One row of `rates.toml`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Row {
    /// `anthropic`, `google` or `openai`.
    pub provider: String,
    /// The model-id prefix the row covers.
    pub prefix: String,
    /// Fresh input, USD per million tokens.
    pub input: f64,
    /// Cache read, USD per million tokens.
    pub cache_read: f64,
    /// Cache write, USD per million tokens.
    pub cache_write: f64,
    /// Output, USD per million tokens.
    pub output: f64,
    /// `both`, `openrouter`, `litellm` or `manual`.
    pub source: String,
}

impl Row {
    /// The four rates as a compact `in/read/write/out` string for the diff.
    fn rates(&self) -> String {
        format!(
            "{}/{}/{}/{} ({})",
            fmt_num(self.input),
            fmt_num(self.cache_read),
            fmt_num(self.cache_write),
            fmt_num(self.output),
            self.source
        )
    }
}

/// The file as parsed.
#[derive(Debug, Deserialize)]
struct RateFile {
    /// `YYYY-MM-DD` of the last refresh.
    read_on: String,
    /// The rows, in file order.
    #[serde(default)]
    rate: Vec<Row>,
}

/// The rows keyed by `(provider, prefix)`, which is also the file's order.
pub type Rows = BTreeMap<(String, String), Row>;

/// The table: the day it was read, and its rows.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    /// `YYYY-MM-DD` of the last refresh.
    pub read_on: String,
    /// Every row.
    pub rows: Rows,
}

/// Parse `rates.toml`.
pub fn parse_table(text: &str) -> Result<Table> {
    let file: RateFile = toml::from_str(text).context("rates.toml does not parse")?;
    let rows = file
        .rate
        .into_iter()
        .map(|row| ((row.provider.clone(), row.prefix.clone()), row))
        .collect();
    Ok(Table {
        read_on: file.read_on,
        rows,
    })
}

/// Render a table as the file, sorted by provider then prefix.
pub fn render_table(table: &Table) -> String {
    let mut out = String::from(FILE_HEADER);
    out.push_str(&format!("\nread_on = \"{}\"\n", table.read_on));
    for row in table.rows.values() {
        out.push_str(&format!(
            "\n[[rate]]\nprovider = \"{}\"\nprefix = \"{}\"\ninput = {}\ncache_read = {}\ncache_write = {}\noutput = {}\nsource = \"{}\"\n",
            row.provider,
            row.prefix,
            fmt_num(row.input),
            fmt_num(row.cache_read),
            fmt_num(row.cache_write),
            fmt_num(row.output),
            row.source
        ));
    }
    out
}

/// A per-million figure as TOML: always a float (`5.0`, not `5`), and the
/// shortest digits that round-trip.
fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

/// Round to a millionth of a dollar per million tokens, so a per-token figure
/// multiplied out does not leave float noise in the file.
fn round6(v: f64) -> f64 {
    (v * 1e6).round() / 1e6
}

// ── The sources ──────────────────────────────────────────────────────────────

/// One model's price as a source publishes it, USD per million tokens. The
/// cache sides are optional because most sources omit one or both.
#[derive(Debug, Clone, PartialEq)]
pub struct Rate {
    /// Fresh input.
    pub input: f64,
    /// Cache read, when published.
    pub cache_read: Option<f64>,
    /// Cache write, when published.
    pub cache_write: Option<f64>,
    /// Output.
    pub output: f64,
}

impl Rate {
    /// The four rates a row carries, with the cache sides defaulted per the
    /// module rules: a read as published, a write only when it is a premium.
    fn resolve(&self) -> (f64, f64, f64, f64) {
        let input = round6(self.input);
        let read = self
            .cache_read
            .filter(|r| *r > 0.0)
            .map(round6)
            .unwrap_or(input);
        let write = self
            .cache_write
            .map(round6)
            .filter(|w| *w >= input)
            .unwrap_or(input);
        (input, read, write, round6(self.output))
    }

    /// Whether two sources agree: input and output within tolerance, and each
    /// cache side within tolerance where both publish it.
    fn agrees(&self, other: &Rate) -> bool {
        within(self.input, other.input)
            && within(self.output, other.output)
            && both_within(self.cache_read, other.cache_read)
            && both_within(self.cache_write, other.cache_write)
    }
}

/// Whether two figures are within [`AGREE_TOLERANCE`] of the larger.
fn within(a: f64, b: f64) -> bool {
    (a - b).abs() <= AGREE_TOLERANCE * a.abs().max(b.abs())
}

/// [`within`] when both sides are present; agreement when either is absent.
fn both_within(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => within(a, b),
        _ => true,
    }
}

/// Prices keyed by `(provider, model id)`.
pub type Prices = BTreeMap<(String, String), Rate>;

/// The vendor id as the table spells it. OpenRouter writes Anthropic's
/// versions with a dot where the API id has a dash.
fn vendor_id(provider: &str, id: &str) -> String {
    if provider == "anthropic" {
        id.replace('.', "-")
    } else {
        id.to_owned()
    }
}

/// A per-token figure from a source, as per million, or `None` when absent or
/// not a number.
fn per_million(value: Option<&serde_json::Value>) -> Option<f64> {
    let v = value?;
    let per_token = match v {
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok()?,
        serde_json::Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    Some(per_token * 1_000_000.0)
}

/// Whether a rate is a price at all. A zero or negative side is a free tier
/// or a model billed some other way, and the table must never carry one.
fn is_priced(rate: &Rate) -> bool {
    rate.input > 0.0 && rate.output > 0.0
}

/// Parse OpenRouter's `/api/v1/models` body into the three vendors' prices.
pub fn parse_openrouter(body: &str) -> Result<Prices> {
    let doc: serde_json::Value = serde_json::from_str(body).context("OpenRouter: not JSON")?;
    let models = doc
        .get("data")
        .and_then(serde_json::Value::as_array)
        .context("OpenRouter: no `data` array")?;
    let mut out = Prices::new();
    for model in models {
        let Some(id) = model.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some((vendor, rest)) = id.split_once('/') else {
            continue;
        };
        if !PROVIDERS.contains(&vendor) || rest.contains(':') {
            continue;
        }
        let pricing = model.get("pricing");
        let field = |name: &str| per_million(pricing.and_then(|p| p.get(name)));
        let (Some(input), Some(output)) = (field("prompt"), field("completion")) else {
            continue;
        };
        let rate = Rate {
            input,
            cache_read: field("input_cache_read"),
            cache_write: field("input_cache_write"),
            output,
        };
        if is_priced(&rate) {
            out.insert((vendor.to_owned(), vendor_id(vendor, rest)), rate);
        }
    }
    Ok(out)
}

/// Parse LiteLLM's price table into the three vendors' prices.
///
/// LiteLLM keys a model several ways (`gemini/gemini-2.5-pro` beside
/// `gemini-2.5-pro`); they collapse to one id, and when the copies disagree
/// beyond tolerance the id is dropped rather than picked from.
pub fn parse_litellm(body: &str) -> Result<Prices> {
    let doc: serde_json::Value = serde_json::from_str(body).context("LiteLLM: not JSON")?;
    let entries = doc.as_object().context("LiteLLM: not an object")?;
    let mut seen: BTreeMap<(String, String), Vec<(String, Rate)>> = BTreeMap::new();
    for (key, entry) in entries {
        let provider = match entry
            .get("litellm_provider")
            .and_then(serde_json::Value::as_str)
        {
            Some("openai") => "openai",
            Some("anthropic") => "anthropic",
            Some("gemini") => "google",
            _ => continue,
        };
        if entry.get("mode").and_then(serde_json::Value::as_str) != Some("chat")
            || key.contains(':')
        {
            continue;
        }
        let id = match key.split_once('/') {
            None => key.as_str(),
            Some((_, rest)) if !rest.contains('/') => rest,
            Some(_) => continue,
        };
        let field = |name: &str| per_million(entry.get(name));
        let (Some(input), Some(output)) = (
            field("input_cost_per_token"),
            field("output_cost_per_token"),
        ) else {
            continue;
        };
        let rate = Rate {
            input,
            cache_read: field("cache_read_input_token_cost"),
            cache_write: field("cache_creation_input_token_cost"),
            output,
        };
        if is_priced(&rate) {
            seen.entry((provider.to_owned(), id.to_owned()))
                .or_default()
                .push((key.clone(), rate));
        }
    }
    let mut out = Prices::new();
    for (model, mut copies) in seen {
        copies.sort_by(|a, b| a.0.cmp(&b.0));
        let first = &copies[0].1;
        if copies.iter().all(|(_, r)| r.agrees(first)) {
            out.insert(model, first.clone());
        }
    }
    Ok(out)
}

// ── The merge ────────────────────────────────────────────────────────────────

/// What a merge did to one row, for the diff.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// A row the file did not have.
    Added(Row),
    /// A row that moved, old and new.
    Changed(Row, Row),
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Change::Added(row) => {
                write!(f, "+ {}/{}: {}", row.provider, row.prefix, row.rates())
            }
            Change::Changed(old, new) => write!(
                f,
                "~ {}/{}: {} -> {}",
                old.provider,
                old.prefix,
                old.rates(),
                new.rates()
            ),
        }
    }
}

/// The result of a merge: the table to write, and what to say about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Merged {
    /// The table after the merge. `read_on` is `today` only when a row moved.
    pub table: Table,
    /// Every row added or changed, in file order.
    pub changes: Vec<Change>,
    /// Models the two sources price more than 5% apart, and so were not
    /// written.
    pub disagreements: Vec<String>,
}

/// Merge the two sources into the existing table under the module's rules.
///
/// Errors are refusals: the caller must leave the file alone.
pub fn merge(
    existing: &Table,
    openrouter: &Prices,
    litellm: &Prices,
    today: &str,
) -> Result<Merged> {
    for provider in PROVIDERS {
        let lists = |prices: &Prices| prices.keys().any(|(p, _)| p == provider);
        if !lists(openrouter) {
            anyhow::bail!("refusing: OpenRouter lists no priced {provider} model");
        }
        if !lists(litellm) {
            anyhow::bail!("refusing: LiteLLM lists no priced {provider} model");
        }
    }

    // 1. Candidates: one rate per id, with the source that vouches for it.
    let mut disagreements = Vec::new();
    let mut candidates: BTreeMap<(String, String), (Rate, &str)> = BTreeMap::new();
    let keys: std::collections::BTreeSet<&(String, String)> =
        openrouter.keys().chain(litellm.keys()).collect();
    for key in keys {
        let candidate = match (openrouter.get(key), litellm.get(key)) {
            (Some(a), Some(b)) if a.agrees(b) => (a.clone(), "both"),
            (Some(a), Some(b)) => {
                disagreements.push(format!(
                    "{}/{}: openrouter {} vs litellm {}",
                    key.0,
                    key.1,
                    rate_summary(a),
                    rate_summary(b)
                ));
                continue;
            }
            (Some(a), None) => (a.clone(), "openrouter"),
            (None, Some(b)) => (b.clone(), "litellm"),
            (None, None) => continue,
        };
        candidates.insert(key.clone(), candidate);
    }

    // 2. Collapse new ids a shorter new id already prices. Existing rows are
    //    exempt: they are refreshed, never collapsed away.
    let mut kept: Vec<((String, String), Rate, &str)> = Vec::new();
    for (key, (rate, source)) in candidates {
        let covered = !existing.rows.contains_key(&key)
            && kept.iter().any(|(k, r, _)| {
                k.0 == key.0
                    && key.1.len() > k.1.len()
                    && key.1.starts_with(&k.1)
                    && r.agrees(&rate)
            });
        if !covered {
            kept.push((key, rate, source));
        }
    }

    // 3. Apply to the table.
    let mut rows = existing.rows.clone();
    let mut changes = Vec::new();
    for ((provider, prefix), rate, source) in kept {
        let (input, cache_read, cache_write, output) = rate.resolve();
        let new = Row {
            provider: provider.clone(),
            prefix: prefix.clone(),
            input,
            cache_read,
            cache_write,
            output,
            source: source.to_owned(),
        };
        match rows.get(&(provider.clone(), prefix.clone())) {
            Some(old) if old.source == "manual" => {}
            Some(old) => {
                let ratio = |a: f64, b: f64| (a / b).max(b / a);
                let worst = ratio(old.input, new.input).max(ratio(old.output, new.output));
                if worst > REFUSE_RATIO {
                    anyhow::bail!(
                        "refusing: {provider}/{prefix} would move by {worst:.1}x ({} -> {}); \
                         a change that large is a broken source until a person says otherwise",
                        old.rates(),
                        new.rates()
                    );
                }
                if *old != new {
                    changes.push(Change::Changed(old.clone(), new.clone()));
                    rows.insert((provider, prefix), new);
                }
            }
            None => {
                changes.push(Change::Added(new.clone()));
                rows.insert((provider, prefix), new);
            }
        }
    }

    if let Some(row) = rows.values().find(|r| r.output <= 0.0 || r.input <= 0.0) {
        anyhow::bail!(
            "refusing: {}/{} has no positive price ({})",
            row.provider,
            row.prefix,
            row.rates()
        );
    }

    let read_on = if changes.is_empty() {
        existing.read_on.clone()
    } else {
        today.to_owned()
    };
    Ok(Merged {
        table: Table { read_on, rows },
        changes,
        disagreements,
    })
}

/// A source rate as `in/read/write/out` with `-` for an absent side.
fn rate_summary(rate: &Rate) -> String {
    let opt = |v: Option<f64>| {
        v.map(|v| fmt_num(round6(v)))
            .unwrap_or_else(|| "-".to_owned())
    };
    format!(
        "{}/{}/{}/{}",
        fmt_num(round6(rate.input)),
        opt(rate.cache_read),
        opt(rate.cache_write),
        fmt_num(round6(rate.output))
    )
}

// ── Running ──────────────────────────────────────────────────────────────────

/// What a run did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing moved; the file is untouched.
    Unchanged,
    /// This many rows were written (or, under `--check`, would be).
    Changed(usize),
}

/// Fetch both sources, merge, and write or check, reporting on stdout.
pub fn run_with(mode: PricesMode, fetch: Fetch, path: &Path, today: &str) -> Result<Outcome> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let existing = parse_table(&text)?;
    let openrouter = parse_openrouter(&fetch(OPENROUTER_URL)?)?;
    let litellm = parse_litellm(&fetch(LITELLM_URL)?)?;
    let merged = merge(&existing, &openrouter, &litellm, today)?;

    for change in &merged.changes {
        println!("{change}");
    }
    for d in &merged.disagreements {
        println!("? {d} (not written)");
    }
    // The Codex model catalog rides along on this fetch. It is a compiled
    // table with no endpoint behind it, so nothing here can rewrite it - but
    // the catalogue already in hand is enough to say when it looks stale, and
    // this is the job that runs every week. Reported, never fatal: the second
    // half of the report is a question rather than a finding, and a weekly
    // job that fails on a question is a weekly job somebody turns off.
    for line in codex_catalog::drift(&codex_ids()?, &openrouter) {
        println!("? {line}");
    }
    let added = merged
        .changes
        .iter()
        .filter(|c| matches!(c, Change::Added(_)))
        .count();
    println!(
        "prices: {} rows, {} added, {} changed, {} disagreements; openrouter {} models, litellm {} models",
        merged.table.rows.len(),
        added,
        merged.changes.len() - added,
        merged.disagreements.len(),
        openrouter.len(),
        litellm.len()
    );

    if merged.changes.is_empty() {
        println!(
            "prices: {} is current as of {}",
            path.display(),
            existing.read_on
        );
        return Ok(Outcome::Unchanged);
    }
    let count = merged.changes.len();
    match mode {
        PricesMode::Check => anyhow::bail!(
            "{} would change ({count} rows); run `cargo xtask prices`",
            path.display()
        ),
        PricesMode::Write => {
            std::fs::write(path, render_table(&merged.table))
                .with_context(|| format!("writing {}", path.display()))?;
            println!(
                "prices: wrote {} rows to {} (read_on {today})",
                count,
                path.display()
            );
            Ok(Outcome::Changed(count))
        }
    }
}

/// The Codex catalog's model ids, read from the shipped source.
fn codex_ids() -> Result<Vec<String>> {
    codex_catalog::catalog_ids(include_str!(
        "../../crates/leviath-providers/src/codex/catalog.rs"
    ))
}

/// The workspace root, from this crate's manifest directory.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Today as `YYYY-MM-DD`, UTC, so two machines on one day agree.
fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// Entry point for `cargo xtask prices [--check]`.
///
/// A network failure exits 2 here rather than through `main`'s exit 1, so a
/// workflow can tell "could not check" from "the table is wrong".
pub fn run(mode: PricesMode) -> Result<()> {
    let path = workspace_root().join(RATES_FILE);
    match run_with(mode, fetch_http, &path, &today()) {
        Ok(_) => Ok(()),
        Err(err) if is_network_error(&err) => {
            eprintln!("prices: {err}");
            std::process::exit(2);
        }
        Err(err) => Err(err),
    }
}

/// The Codex model catalog check, fed by the same fetch as the prices above.
/// Declared here rather than in `main.rs`, which is coverage-excluded and
/// guarded in CI for that reason - a module belonging to this task has no
/// business changing the binary's entrypoint.
#[path = "codex_catalog.rs"]
pub mod codex_catalog;

#[cfg(test)]
#[path = "prices_tests.rs"]
mod tests;

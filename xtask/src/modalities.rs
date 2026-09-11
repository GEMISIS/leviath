//! `cargo xtask modalities` - refresh which mime types each model takes and
//! produces, for the vendors whose direct APIs do not report it per model.
//!
//! Anthropic, OpenAI and Google publish no per-model modality list a client
//! can read, so `crates/leviath-providers/mime/modalities.toml` carries the
//! lists and the runtime falls to a name heuristic where the file is silent.
//! Keeping the file current by hand means guessing which of a vendor's models
//! grew a modality, which is where a wrong row gets in. This command reads the
//! same source `cargo xtask prices` reads - OpenRouter's public catalogue,
//! whose `architecture` block names each model's input and output modalities -
//! and rewrites the file from it.
//!
//! The rules are fixed so two runs on the same input write the same file:
//!
//! * a model the catalogue lists under a supported vendor is written with
//!   `source = "openrouter"`, its `input`/`output` mapped from the modality
//!   words (`text` to `text/*`, `image` to `image/*`, `file` to
//!   `application/pdf`, `audio` to `audio/*`, `video` to `video/*`);
//! * a row whose `source` is `manual` is never overwritten;
//! * a row already in the file that the catalogue no longer lists is kept, so
//!   a model dropped from the gateway is not silently forgotten;
//! * a model id is kept as the vendor writes it, minus the `openai/`,
//!   `anthropic/` or `google/` prefix; OpenRouter spells Anthropic's versions
//!   with a dot (`claude-opus-4.8`) where the API id has a dash
//!   (`claude-opus-4-8`), so those are normalised, and variants after a colon
//!   (`:free`, `:thinking`) are routing options, not models, and are dropped;
//! * patterns are written in a fixed order, so a row's list is stable.
//!
//! This is the modality half of the weekly model refresh; `cargo xtask prices`
//! is the price half, and both read the one catalogue.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::prices::{Fetch, NetworkError, is_network_error};

/// The file this command owns, relative to the workspace root.
pub const MODALITIES_FILE: &str = "crates/leviath-providers/mime/modalities.toml";

/// OpenRouter's public catalogue: no key, each model carrying its architecture.
const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/models";

/// The wait a single fetch is allowed before it is called a network failure.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The vendors whose direct APIs do not report modalities, and whose models
/// the catalogue carries under these prefixes.
const PROVIDERS: [&str; 3] = ["anthropic", "google", "openai"];

/// Every pattern a modality word maps to, in the order rows are written.
const ORDER: [&str; 5] = ["text/*", "image/*", "audio/*", "video/*", "application/pdf"];

/// The header written above the rows, so a reader knows what writes the file.
const FILE_HEADER: &str = "\
# Published input and output modalities for the providers whose APIs do not
# report them per model. Read by `leviath_providers::mime_tables` (longest
# matching `prefix` wins) and rewritten by `cargo xtask modalities`, which takes
# the lists from OpenRouter's catalogue architecture block.
#
# `source` records where a row came from: `openrouter` for a row the refresh
# wrote, and `manual` for a row a person wrote, which the refresh never
# overwrites. A model no row matches falls to the name heuristic in
# `mime_tables`, which is the floor these rows lift.
";

// ── CLI argument parsing ─────────────────────────────────────────────────────

/// What `cargo xtask modalities` was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum ModalitiesMode {
    /// Fetch, merge, and rewrite the file when the rows changed.
    Write,
    /// Fetch, merge, print the diff, and fail if the file would change.
    Check,
}

impl ModalitiesMode {
    /// Parse the arguments after `modalities`.
    pub fn parse(args: &[String]) -> Result<Self> {
        match args.first().map(String::as_str) {
            None => Ok(Self::Write),
            Some("--check") => Ok(Self::Check),
            Some(other) => {
                anyhow::bail!("Unknown `modalities` argument: '{other}'. Try `--check`.")
            }
        }
    }
}

// ── The table ────────────────────────────────────────────────────────────────

/// One row of `modalities.toml`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Row {
    /// `anthropic`, `google` or `openai`.
    pub provider: String,
    /// The model-id prefix the row covers.
    pub prefix: String,
    /// Mime patterns the model accepts.
    pub input: Vec<String>,
    /// Mime patterns the model can produce.
    pub output: Vec<String>,
    /// `openrouter` for a row the refresh wrote, `manual` for a hand-written
    /// row the refresh never overwrites.
    pub source: String,
}

impl Row {
    /// The lists as a compact `in -> out (source)` string for the diff.
    fn lists(&self) -> String {
        format!(
            "[{}] -> [{}] ({})",
            self.input.join(" "),
            self.output.join(" "),
            self.source
        )
    }
}

/// The file as parsed.
#[derive(Debug, Deserialize)]
struct ModalityFile {
    /// `YYYY-MM-DD` of the last refresh.
    read_on: String,
    /// The rows, in file order.
    #[serde(default)]
    modality: Vec<Row>,
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

/// Parse `modalities.toml`.
pub fn parse_table(text: &str) -> Result<Table> {
    let file: ModalityFile = toml::from_str(text).context("modalities.toml does not parse")?;
    let rows = file
        .modality
        .into_iter()
        .map(|row| ((row.provider.clone(), row.prefix.clone()), row))
        .collect();
    Ok(Table {
        read_on: file.read_on,
        rows,
    })
}

/// Render a table back to the file's text, header and all.
pub fn render_table(table: &Table) -> String {
    let mut out = String::from(FILE_HEADER);
    out.push_str(&format!("\nread_on = \"{}\"\n", table.read_on));
    for row in table.rows.values() {
        out.push_str(&format!(
            "\n[[modality]]\nprovider = \"{}\"\nprefix = \"{}\"\ninput = {}\noutput = {}\nsource = \"{}\"\n",
            row.provider,
            row.prefix,
            toml_list(&row.input),
            toml_list(&row.output),
            row.source,
        ));
    }
    out
}

/// A list of patterns as a TOML array literal.
fn toml_list(patterns: &[String]) -> String {
    let inner = patterns
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{inner}]")
}

// ── The catalogue ────────────────────────────────────────────────────────────

/// A modality word as the pattern it maps to, or `None` for a word no built-in
/// family covers.
fn modality_pattern(word: &str) -> Option<&'static str> {
    match word.trim().to_ascii_lowercase().as_str() {
        "text" => Some("text/*"),
        "image" => Some("image/*"),
        "audio" => Some("audio/*"),
        "video" => Some("video/*"),
        "file" => Some("application/pdf"),
        _ => None,
    }
}

/// The vendor id as the table spells it. OpenRouter writes Anthropic's versions
/// with a dot where the API id has a dash.
fn vendor_id(provider: &str, id: &str) -> String {
    if provider == "anthropic" {
        id.replace('.', "-")
    } else {
        id.to_string()
    }
}

/// The words under `architecture.<key>`, mapped to patterns in the fixed order.
fn patterns(architecture: Option<&serde_json::Value>, key: &str) -> Vec<String> {
    let found: Vec<&'static str> = architecture
        .and_then(|a| a.get(key))
        .and_then(serde_json::Value::as_array)
        .map(|words| {
            words
                .iter()
                .filter_map(serde_json::Value::as_str)
                .filter_map(modality_pattern)
                .collect()
        })
        .unwrap_or_default();
    // Filtering the fixed order gives a stable list with no duplicates, since
    // each pattern appears in `ORDER` exactly once.
    ORDER
        .iter()
        .filter(|p| found.contains(p))
        .map(|p| (*p).to_string())
        .collect()
}

/// Parse the catalogue into the vendors' modality rows.
pub fn parse_catalogue(body: &str) -> Result<Rows> {
    let doc: serde_json::Value = serde_json::from_str(body).context("OpenRouter: not JSON")?;
    let models = doc
        .get("data")
        .and_then(serde_json::Value::as_array)
        .context("OpenRouter: no `data` array")?;
    let mut out = Rows::new();
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
        let architecture = model.get("architecture");
        let input = patterns(architecture, "input_modalities");
        let output = patterns(architecture, "output_modalities");
        if input.is_empty() || output.is_empty() {
            continue;
        }
        out.insert(
            (vendor.to_owned(), vendor_id(vendor, rest)),
            Row {
                provider: vendor.to_owned(),
                prefix: vendor_id(vendor, rest),
                input,
                output,
                source: "openrouter".to_owned(),
            },
        );
    }
    Ok(out)
}

// ── The merge ────────────────────────────────────────────────────────────────

/// What a merge did to one row, for the diff.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// A row the file did not have.
    Added(Row),
    /// A row whose lists moved, old and new.
    Changed(Row, Row),
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Change::Added(row) => write!(f, "+ {}/{}: {}", row.provider, row.prefix, row.lists()),
            Change::Changed(old, new) => write!(
                f,
                "~ {}/{}: {} -> {}",
                old.provider,
                old.prefix,
                old.lists(),
                new.lists()
            ),
        }
    }
}

/// The result of a merge: the table to write, and what to say about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Merged {
    /// The table after the merge.
    pub table: Table,
    /// What moved, for the diff.
    pub changes: Vec<Change>,
}

/// Fold the catalogue rows into the existing file: the catalogue wins except on
/// a `manual` row, existing rows the catalogue no longer lists are kept, and
/// `read_on` moves to today only when something changed.
pub fn merge(existing: &Table, fetched: &Rows, today: &str) -> Merged {
    let mut rows = existing.rows.clone();
    let mut changes = Vec::new();
    for (key, new) in fetched {
        match rows.get(key) {
            Some(old) if old.source == "manual" => {}
            Some(old) if old.input == new.input && old.output == new.output => {}
            Some(old) => {
                changes.push(Change::Changed(old.clone(), new.clone()));
                rows.insert(key.clone(), new.clone());
            }
            None => {
                changes.push(Change::Added(new.clone()));
                rows.insert(key.clone(), new.clone());
            }
        }
    }
    let read_on = if changes.is_empty() {
        existing.read_on.clone()
    } else {
        today.to_string()
    };
    Merged {
        table: Table { read_on, rows },
        changes,
    }
}

// ── Running ──────────────────────────────────────────────────────────────────

/// The upshot of a run, for the caller and the tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing moved; the file is untouched.
    Unchanged,
    /// This many rows were written (or, under `--check`, would be).
    Changed(usize),
}

/// Fetch, merge, and write or check, reporting on stdout.
pub fn run_with(mode: ModalitiesMode, fetch: Fetch, path: &Path, today: &str) -> Result<Outcome> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let existing = parse_table(&text)?;
    let fetched = parse_catalogue(&fetch(OPENROUTER_URL)?)?;
    let merged = merge(&existing, &fetched, today);

    for change in &merged.changes {
        println!("{change}");
    }
    let added = merged
        .changes
        .iter()
        .filter(|c| matches!(c, Change::Added(_)))
        .count();
    println!(
        "modalities: {} rows, {} added, {} changed; openrouter {} models",
        merged.table.rows.len(),
        added,
        merged.changes.len() - added,
        fetched.len()
    );

    if merged.changes.is_empty() {
        println!(
            "modalities: {} is current as of {}",
            path.display(),
            existing.read_on
        );
        return Ok(Outcome::Unchanged);
    }
    let count = merged.changes.len();
    match mode {
        ModalitiesMode::Check => anyhow::bail!(
            "{} would change ({count} rows); run `cargo xtask modalities`",
            path.display()
        ),
        ModalitiesMode::Write => {
            std::fs::write(path, render_table(&merged.table))
                .with_context(|| format!("writing {}", path.display()))?;
            println!(
                "modalities: wrote {} rows to {} (read_on {today})",
                merged.table.rows.len(),
                path.display()
            );
            Ok(Outcome::Changed(count))
        }
    }
}

/// The real fetch: a GET with a bounded wait, any failure a [`NetworkError`].
fn fetch_http(url: &str) -> Result<String> {
    let body = reqwest::blocking::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent("leviath-xtask-modalities")
        .build()
        .and_then(|client| client.get(url).send())
        .and_then(reqwest::blocking::Response::error_for_status)
        .and_then(reqwest::blocking::Response::text)
        .map_err(|e| NetworkError(format!("{url}: {e}")))?;
    Ok(body)
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

/// Entry point for `cargo xtask modalities [--check]`.
///
/// A network failure exits 2, as `prices` does, so a workflow can tell "could
/// not check" from "the table is wrong".
pub fn run(mode: ModalitiesMode) -> Result<()> {
    let path = workspace_root().join(MODALITIES_FILE);
    match run_with(mode, fetch_http, &path, &today()) {
        Ok(_) => Ok(()),
        Err(err) if is_network_error(&err) => {
            eprintln!("modalities: {err}");
            std::process::exit(2);
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
#[path = "modalities_tests.rs"]
mod tests;

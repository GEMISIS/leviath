//! `cargo xtask bedrock-windows` - refresh what each Bedrock model can hold
//! and say, from AWS's own model cards.
//!
//! Bedrock's APIs list a model's modalities and lifecycle and nothing about
//! its size: `ListFoundationModels` and `GetFoundationModel` carry no token
//! limit at all. The one place AWS states a context window and a maximum
//! output is the model's card in the user guide, a static HTML page per model
//! linked from the "models at a glance" index. This command reads every card
//! and rewrites `crates/leviath-providers/bedrock/windows.toml` from them, so
//! the numbers the runtime budgets against are AWS's and not a guess.
//!
//! The rules are fixed so two runs on the same input write the same file:
//!
//! * a card with a context window and a maximum output is written with
//!   `source = "aws-model-card"`; one without (an embedding, image or speech
//!   model) is skipped;
//! * a row whose `source` is `manual` is never overwritten;
//! * a row already in the file that no card names any more is kept;
//! * the row's `id` is the bare model id, without an inference-profile
//!   prefix; the geo and global profile ids the card names are kept beside
//!   it, in the card's order;
//! * AWS prints the limits rounded (`1M tokens`, `164K`), and they are kept as
//!   printed, with K as a thousand and M as a million.
//!
//! Every card is fetched, over a hundred of them, so a run takes a minute.
//! A network failure exits 2, as `prices` does. A scrape that came back empty
//! or would move a limit more than threefold is refused rather than written:
//! the page layout changed, and a person should look before the runtime
//! budgets against the result.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::prices::{Fetch, NetworkError, is_network_error};

/// The file this command owns, relative to the workspace root.
pub const WINDOWS_FILE: &str = "crates/leviath-providers/bedrock/windows.toml";

/// Where the cards are linked from.
const INDEX_URL: &str = "https://docs.aws.amazon.com/bedrock/latest/userguide/model-cards.html";

/// Where each card lives; the index links them by file name.
const CARD_BASE: &str = "https://docs.aws.amazon.com/bedrock/latest/userguide/";

/// The wait a single fetch is allowed before it is called a network failure.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The inference-profile prefixes AWS defines, so a bare id can be read off
/// a profile id when the card names no bare id.
const PROFILE_PREFIXES: [&str; 8] = ["us", "eu", "apac", "global", "jp", "au", "ca", "us-gov"];

/// How far a limit may move between refreshes before the write is refused.
const PLAUSIBLE_MOVE: f64 = 3.0;

/// The header written above the rows, so a reader knows what writes the file.
const FILE_HEADER: &str = "\
# What each Bedrock model can hold and say, as AWS publishes it.
#
# Bedrock's own APIs list a model's modalities and lifecycle and nothing
# about its size, so the two token limits here are read from the model's
# card in the AWS documentation, which is the one place AWS states them.
# `cargo xtask bedrock-windows` reads every card and rewrites this file; a
# row whose `source` is `manual` was written by a person and is left alone.
#
# `id` is the bare model id with any inference-profile prefix removed;
# `profiles` are the geo and global inference-profile ids the card names,
# which are what a request on `bedrock-runtime` has to use for most current
# models. `context` and `output` are tokens; AWS prints them rounded (164K),
# and they are kept as printed with K as 1000 and M as 1000000.
#
# `reasoning` says the card has a Reasoning line, and `count_tokens` whether
# the card lists CountTokens as supported on `bedrock-runtime`.
";

// ── CLI argument parsing ─────────────────────────────────────────────────────

/// What `cargo xtask bedrock-windows` was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum WindowsMode {
    /// Fetch, merge, and rewrite the file when the rows changed.
    Write,
    /// Fetch, merge, print the diff, and fail if the file would change.
    Check,
}

impl WindowsMode {
    /// Parse the arguments after `bedrock-windows`.
    pub fn parse(args: &[String]) -> Result<Self> {
        match args.first().map(String::as_str) {
            None => Ok(Self::Write),
            Some("--check") => Ok(Self::Check),
            Some(other) => {
                anyhow::bail!("Unknown `bedrock-windows` argument: '{other}'. Try `--check`.")
            }
        }
    }
}

// ── The table ────────────────────────────────────────────────────────────────

/// One row of `windows.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Row {
    /// The bare model id.
    pub id: String,
    /// What AWS calls it.
    pub name: String,
    /// The context window, in tokens.
    pub context: u64,
    /// The maximum output, in tokens.
    pub output: u64,
    /// Whether the card has a Reasoning line.
    #[serde(default)]
    pub reasoning: bool,
    /// Whether the card lists CountTokens as supported on `bedrock-runtime`.
    #[serde(default)]
    pub count_tokens: bool,
    /// The inference-profile ids the card names.
    #[serde(default)]
    pub profiles: Vec<String>,
    /// `aws-model-card` for a row the refresh wrote, `manual` for a
    /// hand-written row the refresh never overwrites.
    pub source: String,
}

impl Row {
    /// The limits as a compact string for the diff.
    fn limits(&self) -> String {
        format!(
            "{} in / {} out{}{} ({})",
            self.context,
            self.output,
            if self.reasoning { ", reasoning" } else { "" },
            if self.count_tokens { ", counts" } else { "" },
            self.source
        )
    }
}

/// The file as parsed.
#[derive(Debug, Deserialize)]
struct WindowFile {
    /// `YYYY-MM-DD` of the last refresh.
    read_on: String,
    /// The rows, in file order.
    #[serde(default)]
    model: Vec<Row>,
}

/// The rows keyed by id, which is also the file's order.
pub type Rows = BTreeMap<String, Row>;

/// The table: the day it was read, and its rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    /// `YYYY-MM-DD` of the last refresh.
    pub read_on: String,
    /// Every row.
    pub rows: Rows,
}

/// Parse `windows.toml`.
pub fn parse_table(text: &str) -> Result<Table> {
    let file: WindowFile = toml::from_str(text).context("windows.toml does not parse")?;
    let rows = file
        .model
        .into_iter()
        .map(|row| (row.id.clone(), row))
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
            "\n[[model]]\nid = \"{}\"\nname = \"{}\"\ncontext = {}\noutput = {}\nreasoning = {}\ncount_tokens = {}\nprofiles = {}\nsource = \"{}\"\n",
            row.id,
            row.name.replace('"', "'"),
            row.context,
            row.output,
            row.reasoning,
            row.count_tokens,
            toml_list(&row.profiles),
            row.source,
        ));
    }
    out
}

/// A list of ids as a TOML array literal.
fn toml_list(items: &[String]) -> String {
    let inner = items
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{inner}]")
}

// ── The cards ────────────────────────────────────────────────────────────────

/// The card file names the index links, each once, in link order.
pub fn card_links(index_html: &str) -> Vec<String> {
    let mut seen = Vec::new();
    for piece in index_html.split("model-card-").skip(1) {
        let Some((stem, _)) = piece.split_once(".html") else {
            continue;
        };
        if stem.is_empty() || !stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            continue;
        }
        let name = format!("model-card-{stem}.html");
        if !seen.contains(&name) {
            seen.push(name);
        }
    }
    seen
}

/// A token figure as the card prints it: `1M tokens`, `200K tokens`, `128K`,
/// `8,192 tokens`.
pub fn parse_tokens(text: &str) -> Option<u64> {
    let word = text.split_whitespace().next()?.replace(',', "");
    let (digits, scale) = if let Some(digits) = word.strip_suffix(['k', 'K']) {
        (digits, 1_000.0)
    } else if let Some(digits) = word.strip_suffix(['m', 'M']) {
        (digits, 1_000_000.0)
    } else {
        (word.as_str(), 1.0)
    };
    let value: f64 = digits.parse().ok()?;
    (value > 0.0).then(|| (value * scale).round() as u64)
}

/// The text between `after` and the next `</p>`, if the page has it.
fn line_after<'a>(html: &'a str, after: &str) -> Option<&'a str> {
    let (_, rest) = html.split_once(after)?;
    let (line, _) = rest.split_once("</p>")?;
    Some(line.trim())
}

/// Text with its tags removed and its whitespace collapsed.
fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The contents of every `<code>` element in `html`.
fn code_spans(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for piece in html.split("<code").skip(1) {
        let Some((_, body)) = piece.split_once('>') else {
            continue;
        };
        let Some((inner, _)) = body.split_once("</code>") else {
            continue;
        };
        out.push(strip_tags(inner));
    }
    out
}

/// Whether the card lists CountTokens as supported on `bedrock-runtime`:
/// the first mention of the feature sits in the runtime table, and the icon
/// just before it says yes or no.
fn counts_tokens(html: &str) -> bool {
    let Some((before, _)) = html.split_once("count-tokens.html") else {
        return false;
    };
    match (before.rfind("icon-yes"), before.rfind("icon-no")) {
        (Some(yes), Some(no)) => yes > no,
        (Some(_), None) => true,
        _ => false,
    }
}

/// The bare id and the profile ids from the card's programmatic-access
/// table, found by its `Model ID` column header (the section's anchor is
/// also linked from the page's contents list, so it is not the marker): the
/// `bedrock-runtime` row carries the model id (or `N/A`), the geo profile
/// ids and the global profile id; the `bedrock-mantle` row carries the bare
/// id for a model the runtime serves through profiles only.
fn ids(html: &str) -> (Option<String>, Vec<String>) {
    let Some((_, section)) = html.split_once("Model ID<") else {
        return (None, Vec::new());
    };
    let section = section
        .split_once("</table>")
        .map_or(section, |(table, _)| table);
    let mut bare = None;
    let mut profiles = Vec::new();
    for row in section.split("<tr>").skip(1) {
        // Each cell, from the end of its opening tag to its closing one.
        let cells: Vec<&str> = row
            .split("<td")
            .skip(1)
            .filter_map(|cell| cell.split_once('>').map(|(_, body)| body))
            .map(|body| body.split_once("</td>").map_or(body, |(cell, _)| cell))
            .collect();
        if cells.len() < 2 {
            continue;
        }
        let endpoint = strip_tags(cells[0]);
        let model_id = strip_tags(cells[1]);
        let is_id = |s: &str| s.contains('.') && !s.contains(' ') && s != "N/A";
        match endpoint.as_str() {
            "bedrock-runtime" => {
                if is_id(&model_id) {
                    bare = Some(model_id);
                }
                if let Some(geo) = cells.get(3) {
                    profiles.extend(code_spans(geo).into_iter().filter(|s| is_id(s)));
                }
                if let Some(global) = cells.get(4) {
                    let global = strip_tags(global);
                    if is_id(&global) {
                        profiles.push(global);
                    }
                }
            }
            "bedrock-mantle" if bare.is_none() && is_id(&model_id) => bare = Some(model_id),
            _ => {}
        }
    }
    (bare, profiles)
}

/// A profile id without its prefix.
fn without_prefix(profile: &str) -> String {
    match profile.split_once('.') {
        Some((prefix, rest)) if PROFILE_PREFIXES.contains(&prefix) => rest.to_string(),
        _ => profile.to_string(),
    }
}

/// One card as a row, or `None` for a card that states no limits (an
/// embedding, image, video or speech model) or names no id.
pub fn parse_card(html: &str) -> Option<Row> {
    let context = parse_tokens(line_after(html, "Context window:</b>")?)?;
    let output = parse_tokens(line_after(html, "Max output tokens:</b>")?)?;
    let name = html
        .split_once("<title>")
        .map(|(_, rest)| rest.split_once("</title>").map_or(rest, |(t, _)| t))
        .map(|title| strip_tags(title.split(" - Amazon Bedrock").next().unwrap_or("")))
        .filter(|t| !t.is_empty())?;
    let (bare, profiles) = ids(html);
    let id = bare.or_else(|| profiles.first().map(|p| without_prefix(p)))?;
    Some(Row {
        id,
        name,
        context,
        output,
        reasoning: html.contains("Reasoning:</b>"),
        count_tokens: counts_tokens(html),
        profiles,
        source: "aws-model-card".to_string(),
    })
}

// ── The merge ────────────────────────────────────────────────────────────────

/// What a merge did to one row, for the diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A row the file did not have.
    Added(Row),
    /// A row that moved, old and new.
    Changed(Row, Row),
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Change::Added(row) => write!(f, "+ {}: {}", row.id, row.limits()),
            Change::Changed(old, new) => {
                write!(f, "~ {}: {} -> {}", old.id, old.limits(), new.limits())
            }
        }
    }
}

/// The result of a merge: the table to write, and what to say about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    /// The table after the merge.
    pub table: Table,
    /// What moved, for the diff.
    pub changes: Vec<Change>,
}

/// Whether a limit moved further than a card correction plausibly would.
fn implausible(old: u64, new: u64) -> bool {
    let (old, new) = (old as f64, new as f64);
    old / new > PLAUSIBLE_MOVE || new / old > PLAUSIBLE_MOVE
}

/// Fold the scraped rows into the existing file: the cards win except on a
/// `manual` row, existing rows no card names are kept, and `read_on` moves
/// to today only when something changed.
///
/// Refuses a scrape that came back empty, one that names fewer than half the
/// rows the file has, or one that would move a limit more than threefold:
/// each says the page layout changed rather than the model.
pub fn merge(existing: &Table, fetched: &Rows, today: &str) -> Result<Merged> {
    if fetched.is_empty() {
        anyhow::bail!("no model card could be read; the index or the card layout has changed");
    }
    if fetched.len() * 2 < existing.rows.len() {
        anyhow::bail!(
            "only {} cards were read where the file has {} rows; refusing to write",
            fetched.len(),
            existing.rows.len()
        );
    }
    let mut rows = existing.rows.clone();
    let mut changes = Vec::new();
    for (id, new) in fetched {
        match rows.get(id) {
            Some(old) if old.source == "manual" => {}
            Some(old) if old == new => {}
            Some(old) => {
                if implausible(old.context, new.context) || implausible(old.output, new.output) {
                    anyhow::bail!(
                        "{id} would move from {} to {}; refusing to write",
                        old.limits(),
                        new.limits()
                    );
                }
                changes.push(Change::Changed(old.clone(), new.clone()));
                rows.insert(id.clone(), new.clone());
            }
            None => {
                changes.push(Change::Added(new.clone()));
                rows.insert(id.clone(), new.clone());
            }
        }
    }
    let read_on = if changes.is_empty() {
        existing.read_on.clone()
    } else {
        today.to_string()
    };
    Ok(Merged {
        table: Table { read_on, rows },
        changes,
    })
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

/// Every card the index links, read and parsed.
pub fn scrape(fetch: Fetch) -> Result<Rows> {
    let index = fetch(INDEX_URL)?;
    let mut rows = Rows::new();
    for link in card_links(&index) {
        let html = fetch(&format!("{CARD_BASE}{link}"))?;
        if let Some(row) = parse_card(&html) {
            rows.insert(row.id.clone(), row);
        }
    }
    Ok(rows)
}

/// Fetch, merge, and write or check, reporting on stdout.
pub fn run_with(mode: WindowsMode, fetch: Fetch, path: &Path, today: &str) -> Result<Outcome> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let existing = parse_table(&text)?;
    let fetched = scrape(fetch)?;
    let merged = merge(&existing, &fetched, today)?;

    for change in &merged.changes {
        println!("{change}");
    }
    let added = merged
        .changes
        .iter()
        .filter(|c| matches!(c, Change::Added(_)))
        .count();
    println!(
        "bedrock-windows: {} rows, {} added, {} changed; {} cards read",
        merged.table.rows.len(),
        added,
        merged.changes.len() - added,
        fetched.len()
    );

    if merged.changes.is_empty() {
        println!(
            "bedrock-windows: {} is current as of {}",
            path.display(),
            existing.read_on
        );
        return Ok(Outcome::Unchanged);
    }
    let count = merged.changes.len();
    match mode {
        WindowsMode::Check => anyhow::bail!(
            "{} would change ({count} rows); run `cargo xtask bedrock-windows`",
            path.display()
        ),
        WindowsMode::Write => {
            std::fs::write(path, render_table(&merged.table))
                .with_context(|| format!("writing {}", path.display()))?;
            println!(
                "bedrock-windows: wrote {} rows to {} (read_on {today})",
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
        .user_agent("leviath-xtask-bedrock-windows")
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

/// Entry point for `cargo xtask bedrock-windows [--check]`.
///
/// A network failure exits 2, as `prices` does, so a workflow can tell "could
/// not check" from "the table is wrong".
pub fn run(mode: WindowsMode) -> Result<()> {
    let path = workspace_root().join(WINDOWS_FILE);
    match run_with(mode, fetch_http, &path, &today()) {
        Ok(_) => Ok(()),
        Err(err) if is_network_error(&err) => {
            eprintln!("bedrock-windows: {err}");
            std::process::exit(2);
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
#[path = "bedrock_windows_tests.rs"]
mod tests;

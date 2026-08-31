//! Whether the compiled context windows still match what the vendors publish.
//!
//! Every provider carries a `MODELS: &[Row]` table of context and output
//! limits. Those numbers are load-bearing in a way a reader would not guess:
//! percentage region budgets resolve once at spawn against
//! `max_context_tokens`, so a row that is too small silently sizes every
//! region for a fraction of the window the run actually has, for the whole
//! run, with nothing failing and nothing said.
//!
//! That is exactly how it went wrong. `gpt-5.5` sat at 400,000 on the Codex
//! route against a published 1,050,000, and `gpt-5.6` had no OpenAI row at all
//! so it fell through to the `gpt-5` family's 400,000 against a published
//! 922,000. Both were found by hand, months after the models shipped.
//!
//! So this rides along on the fetch `prices` already makes. It reports and
//! never writes: these live in Rust source, and a tool that rewrites source
//! is a much larger promise than one that rewrites a TOML table.
//!
//! ## Which number belongs in the field
//!
//! `ModelCapabilities::max_context_tokens` is documented as the maximum
//! *input* tokens, so LiteLLM's `max_input_tokens` is the like-for-like
//! figure. OpenRouter's `context_length` is the whole window and is larger by
//! exactly the output allowance - 922,000 + 128,000 = 1,050,000 for the
//! gpt-5.6 family - which is the arithmetic that says the two sources agree
//! rather than disagreeing.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

/// What a vendor publishes for one model: input window, output cap.
pub type Windows = BTreeMap<String, (u64, u64)>;

/// One `Row` from a provider's compiled table.
#[derive(Debug, PartialEq)]
pub struct Row {
    /// The model-name prefixes this row answers for.
    pub prefixes: Vec<String>,
    /// `max_context_tokens`: the input window.
    pub context: u64,
    /// `max_output_tokens`.
    pub output: u64,
}

/// The windows LiteLLM publishes for OpenAI's chat models.
///
/// Only OpenAI: the other vendors' tables are checked the same way when
/// somebody wires them up, and reporting on a vendor nobody compares against
/// would be noise.
pub fn parse_litellm_windows(body: &str) -> Result<Windows> {
    let doc: serde_json::Value = serde_json::from_str(body).context("LiteLLM: not JSON")?;
    let entries = doc.as_object().context("LiteLLM: not an object")?;
    let mut out = Windows::new();
    for (key, entry) in entries {
        if entry
            .get("litellm_provider")
            .and_then(serde_json::Value::as_str)
            != Some("openai")
            || entry.get("mode").and_then(serde_json::Value::as_str) != Some("chat")
            || key.contains(':')
        {
            continue;
        }
        let id = match key.split_once('/') {
            None => key.as_str(),
            Some((_, rest)) if !rest.contains('/') => rest,
            Some(_) => continue,
        };
        let field = |name: &str| entry.get(name).and_then(serde_json::Value::as_u64);
        if let (Some(input), Some(output)) = (field("max_input_tokens"), field("max_output_tokens"))
        {
            out.insert(id.to_string(), (input, output));
        }
    }
    Ok(out)
}

/// The `Row` literals in a provider's source.
///
/// Read from the text rather than linked, for the reason `codex_catalog`
/// gives: `xtask` is the tool that checks the crates and depends on none of
/// them.
pub fn parse_rows(source: &str) -> Result<Vec<Row>> {
    let (_, rest) = source
        .split_once("MODELS: &[Row] = &[")
        .context("no MODELS table")?;
    // Terminated by a line that is just `];`, found line-wise rather than as
    // `"\n];"`: the real tables sit at column zero and a fixture does not, and
    // a parser that only reads one of those is a parser that passes its tests
    // and misses the file.
    let end = rest
        .lines()
        .scan(0usize, |at, line| {
            let start = *at;
            *at += line.len() + 1;
            Some((start, line))
        })
        .find(|(_, line)| line.trim() == "];")
        .map(|(at, _)| at)
        .context("MODELS is not terminated")?;
    let body = rest.get(..end).unwrap_or_default();

    let mut rows = Vec::new();
    for chunk in body.split("Row {").skip(1) {
        let prefixes: Vec<String> = chunk
            .split("Match::Prefix(\"")
            .skip(1)
            .filter_map(|rest| rest.split_once('"').map(|(name, _)| name.to_string()))
            .collect();
        let (Some(context), Some(output)) = (number(chunk, "context:"), number(chunk, "output:"))
        else {
            continue;
        };
        if prefixes.is_empty() {
            // A row matched some other way - `Contains`, an exact id. Nothing
            // to look up by, so nothing to compare.
            continue;
        }
        rows.push(Row {
            prefixes,
            context,
            output,
        });
    }
    if rows.is_empty() {
        anyhow::bail!("MODELS parsed as empty");
    }
    Ok(rows)
}

/// The number after `field` in `chunk`, with the `1_000` spelling allowed.
fn number(chunk: &str, field: &str) -> Option<u64> {
    let (_, rest) = chunk.split_once(field)?;
    let digits: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '_')
        .filter(|c| *c != '_')
        .collect();
    digits.parse().ok()
}

/// Where `rows` disagrees with what the vendor publishes.
///
/// A row is judged against the *most specific* published model it answers
/// for - the shortest id starting with one of its prefixes - because that is
/// the model a blueprint naming the prefix will actually reach. A prefix
/// nothing published matches is skipped rather than reported: this build's
/// table legitimately carries models the sources have not caught up with, and
/// the model-list check is the one that speaks to absence.
pub fn drift(label: &str, rows: &[Row], published: &Windows) -> Vec<String> {
    let mut report = Vec::new();
    for row in rows {
        for prefix in &row.prefixes {
            let Some((id, (context, output))) = published
                .iter()
                .filter(|(id, _)| id.starts_with(prefix.as_str()))
                .min_by_key(|(id, _)| id.len())
            else {
                continue;
            };
            if row.context != *context {
                report.push(format!(
                    "{label}: '{prefix}' has context {} and {id} publishes {context}",
                    row.context
                ));
            }
            if row.output != *output {
                report.push(format!(
                    "{label}: '{prefix}' has output {} and {id} publishes {output}",
                    row.output
                ));
            }
        }
    }
    report.sort();
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn published(entries: &[(&str, u64, u64)]) -> Windows {
        entries
            .iter()
            .map(|(id, c, o)| ((*id).to_string(), (*c, *o)))
            .collect()
    }

    const SOURCE: &str = r#"
        pub(crate) const MODELS: &[Row] = &[
            Row {
                matches: &[Match::Prefix("gpt-5.6")],
                temperature: false,
                tools: true,
                context: 922_000,
                output: 128_000,
            },
            Row {
                matches: &[Match::Prefix("o3"), Match::Prefix("o4")],
                temperature: false,
                tools: true,
                context: 200_000,
                output: 100_000,
            },
        ];
    "#;

    /// The prefixes and both numbers, with the `1_000` spelling read.
    #[test]
    fn the_rows_parse_with_their_prefixes_and_numbers() {
        let rows = parse_rows(SOURCE).expect("parses");
        assert_eq!(
            rows,
            vec![
                Row {
                    prefixes: vec!["gpt-5.6".to_string()],
                    context: 922_000,
                    output: 128_000,
                },
                Row {
                    prefixes: vec!["o3".to_string(), "o4".to_string()],
                    context: 200_000,
                    output: 100_000,
                },
            ]
        );
    }

    /// A file that does not hold what this expects fails loudly rather than
    /// reporting a clean table it never read.
    #[test]
    fn a_table_it_cannot_read_is_an_error() {
        for source in [
            "nothing here",
            "MODELS: &[Row] = &[",
            "MODELS: &[Row] = &[\n];",
        ] {
            assert!(parse_rows(source).is_err(), "{source}");
        }
    }

    /// A row matched some way other than a prefix has nothing to look up by,
    /// so it is skipped rather than guessed at.
    #[test]
    fn a_row_with_no_prefix_is_skipped() {
        let source = r#"
            pub(crate) const MODELS: &[Row] = &[
                Row {
                    matches: &[Match::Contains("codex-spark")],
                    context: 128_000,
                    output: 32_000,
                },
                Row {
                    matches: &[Match::Prefix("gpt-5.6")],
                    context: 922_000,
                    output: 128_000,
                },
            ];
        "#;
        let rows = parse_rows(source).expect("parses");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prefixes, ["gpt-5.6"]);
    }

    /// A table that matches what is published says nothing.
    #[test]
    fn a_current_table_reports_nothing() {
        let rows = parse_rows(SOURCE).expect("parses");
        let published = published(&[("gpt-5.6", 922_000, 128_000), ("o3", 200_000, 100_000)]);
        assert!(drift("openai", &rows, &published).is_empty());
    }

    /// The bug this exists for: a window smaller than the model's, which
    /// nothing fails on and which sizes every region for a fraction of the
    /// run's real budget.
    #[test]
    fn a_window_smaller_than_the_published_one_is_reported() {
        let rows = vec![Row {
            prefixes: vec!["gpt-5.6".to_string()],
            context: 400_000,
            output: 128_000,
        }];
        let report = drift(
            "codex",
            &rows,
            &published(&[("gpt-5.6-sol", 922_000, 128_000)]),
        );
        assert_eq!(report.len(), 1, "{report:?}");
        assert!(report[0].contains("context 400000"), "{report:?}");
        assert!(report[0].contains("922000"), "{report:?}");
    }

    /// The output cap is checked too, and separately: the two are different
    /// numbers with different consequences.
    #[test]
    fn a_wrong_output_cap_is_reported_on_its_own() {
        let rows = vec![Row {
            prefixes: vec!["gpt-5.6".to_string()],
            context: 922_000,
            output: 32_000,
        }];
        let report = drift("codex", &rows, &published(&[("gpt-5.6", 922_000, 128_000)]));
        assert_eq!(report.len(), 1, "{report:?}");
        assert!(report[0].contains("output 32000"), "{report:?}");
    }

    /// Judged against the *most specific* published id the prefix reaches,
    /// because that is the model a blueprint naming it lands on.
    #[test]
    fn the_shortest_published_match_is_the_one_compared() {
        let rows = vec![Row {
            prefixes: vec!["gpt-5.6".to_string()],
            context: 922_000,
            output: 128_000,
        }];
        // `gpt-5.6` itself is the shortest, so the odd variant beside it does
        // not drag the comparison.
        let published = published(&[
            ("gpt-5.6", 922_000, 128_000),
            ("gpt-5.6-cyber", 400_000, 128_000),
        ]);
        assert!(drift("openai", &rows, &published).is_empty());
    }

    /// A prefix nothing published matches is silence, not a finding. This
    /// build carries models the sources have not caught up with, and absence
    /// is the model-list check's question rather than this one's.
    #[test]
    fn a_prefix_nothing_publishes_is_not_reported() {
        let rows = vec![Row {
            prefixes: vec!["gpt-6".to_string()],
            context: 1,
            output: 1,
        }];
        assert!(
            drift(
                "openai",
                &rows,
                &published(&[("gpt-5.6", 922_000, 128_000)])
            )
            .is_empty()
        );
    }
}

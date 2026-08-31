//! Whether the compiled Codex model catalog still matches what OpenAI ships.
//!
//! The Codex route has no `/models` endpoint, so
//! `crates/leviath-providers/src/codex/catalog.rs` carries the list as a
//! compiled table. A table nobody checks is a table that rots, and the two
//! ways it rots are both quiet: a model withdrawn or renamed stays offered
//! and fails at the first call, and a model newly served stays unreachable -
//! `serves_model` refuses it locally, so somebody pays for a subscription
//! that includes a model Leviath will not route to.
//!
//! There is no authoritative source to fix that from. Which models Codex
//! serves is a product decision published nowhere, and only an authenticated
//! ChatGPT session can enumerate them. So this does not rewrite the table; it
//! reads the same OpenRouter catalogue `prices` already fetched and reports
//! what looks wrong, every Monday, where somebody will see it.
//!
//! Two directions, and they are not equally trustworthy:
//!
//! * a catalog id no price source knows is probably renamed or withdrawn, and
//!   is worth acting on. Codex-only models are exempt: `gpt-5.3-codex-spark`
//!   is real and OpenRouter will never list it;
//! * an OpenAI model in the same family that the catalog lacks *may* be newly
//!   served, or may be a model Codex does not offer at all. OpenRouter lists
//!   every OpenAI model, most of which Codex has never served, so this half is
//!   a prompt to check rather than a finding.

use anyhow::{Context, Result};

use super::Prices;

/// The family the Codex route serves. Narrower than "every OpenAI model",
/// which would report dozens of irrelevant ids every week.
const FAMILY: &str = "gpt-5.";

/// A model whose name says it is Codex's own, and so will never appear in a
/// public price catalogue.
fn is_codex_only(id: &str) -> bool {
    id.contains("codex")
}

/// The `gpt-5.N` generation of an id, as `N`.
///
/// `None` for anything outside the family, which is most of what OpenAI
/// publishes.
fn generation(id: &str) -> Option<u32> {
    let rest = id.strip_prefix(FAMILY)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The model ids in `catalog.rs`, read from the source rather than linked.
///
/// `xtask` deliberately depends on no Leviath crate - it is the tool that
/// checks them, and every `cargo xtask docs` would otherwise rebuild the
/// provider crate first. The shape parsed here is the one the file declares:
/// `("id", "Display Name"),` rows between the `CATALOG` binding and its `];`.
pub fn catalog_ids(source: &str) -> Result<Vec<String>> {
    let (_, rest) = source
        .split_once("CATALOG: &[(&str, &str)] = &[")
        .context("codex catalog: no CATALOG binding")?;
    let (body, _) = rest
        .split_once("];")
        .context("codex catalog: CATALOG is not terminated")?;
    let mut ids = Vec::new();
    for row in body.split("(\"").skip(1) {
        let Some((id, _)) = row.split_once('"') else {
            continue;
        };
        ids.push(id.to_string());
    }
    if ids.is_empty() {
        anyhow::bail!("codex catalog: CATALOG parsed as empty");
    }
    Ok(ids)
}

/// What looks stale about `ids`, given what the price sources list.
///
/// Reported, never written: see the module note on why there is nothing to
/// write from.
pub fn drift(ids: &[String], openrouter: &Prices) -> Vec<String> {
    let openai: Vec<&str> = openrouter
        .keys()
        .filter(|(vendor, _)| vendor == "openai")
        .map(|(_, id)| id.as_str())
        .collect();

    // Only this generation and later are worth asking about. OpenAI still
    // publishes prices for every `gpt-5.1`, `5.2` and `5.4` variant it ever
    // shipped, and Codex serves none of them: without a floor this reported
    // a dozen models a week, which is a report nobody reads. The floor moves
    // on its own as the catalog does.
    let floor = ids.iter().filter_map(|id| generation(id)).max();

    let mut report = Vec::new();
    for id in ids {
        // Covered by a *prefix*, not only by an exact match, because that is
        // how the price table is read: `gpt-5.6` prices `gpt-5.6-sol`, and a
        // vendor that publishes the family without the variant should not
        // have every variant reported as withdrawn, every week, for ever.
        let known = openai
            .iter()
            .any(|listed| id == listed || id.starts_with(listed));
        if is_codex_only(id) || known {
            continue;
        }
        report.push(format!(
            "codex: '{id}' is in the catalog and no price source lists it - renamed or withdrawn?"
        ));
    }
    for id in &openai {
        // Symmetric to the rule above: the family name itself is covered when
        // the catalog serves variants of it. OpenRouter prices `gpt-5.6`, the
        // catalog serves `gpt-5.6-sol` and friends, and asking every Monday
        // whether Codex serves `gpt-5.6` is the same noise in the other
        // direction.
        let covered = ids
            .iter()
            .any(|known| known == id || known.starts_with(*id));
        let current = match (generation(id), floor) {
            (Some(generation), Some(floor)) => generation >= floor,
            // No floor means an empty catalog, which `catalog_ids` refuses,
            // and no generation means the id is outside the family.
            _ => false,
        };
        if !current || covered {
            continue;
        }
        report.push(format!(
            "codex: '{id}' is listed by OpenAI and not in the catalog - does Codex serve it?"
        ));
    }
    report.sort();
    report
}

#[cfg(test)]
mod tests {
    use super::super::Rate;
    use super::*;

    fn priced(ids: &[&str]) -> Prices {
        ids.iter()
            .map(|id| {
                (
                    ("openai".to_string(), (*id).to_string()),
                    Rate {
                        input: 1.0,
                        cache_read: None,
                        cache_write: None,
                        output: 2.0,
                    },
                )
            })
            .collect()
    }

    /// The parser reads the ids and not the display names beside them.
    #[test]
    fn the_catalog_parses_to_its_ids() {
        let source = r#"
            pub(crate) const CATALOG: &[(&str, &str)] = &[
                ("gpt-5.6-sol", "GPT-5.6 Sol"),
                ("gpt-5.5", "GPT-5.5"),
            ];
        "#;
        assert_eq!(catalog_ids(source).unwrap(), ["gpt-5.6-sol", "gpt-5.5"]);
    }

    /// A file that does not hold what this expects fails loudly. A silent
    /// empty answer would report every model as newly served, every week.
    #[test]
    fn a_catalog_it_cannot_read_is_an_error() {
        for source in [
            "nothing like a catalog here",
            "pub(crate) const CATALOG: &[(&str, &str)] = &[",
            "pub(crate) const CATALOG: &[(&str, &str)] = &[];",
        ] {
            assert!(catalog_ids(source).is_err(), "{source}");
        }
    }

    /// The real catalog parses. Compiled in, so this fails the day the shape
    /// of that file changes rather than the Monday after.
    #[test]
    fn the_shipped_catalog_parses() {
        let source = include_str!("../../crates/leviath-providers/src/codex/catalog.rs");
        let ids = catalog_ids(source).expect("the shipped catalog parses");
        assert!(ids.iter().any(|id| id.starts_with("gpt-5")), "{ids:?}");
    }

    /// A variant the sources price under the family name is not missing.
    ///
    /// This is the noise the check would otherwise generate for ever:
    /// OpenRouter prices `gpt-5.6`, Codex serves `gpt-5.6-sol`, and an exact
    /// comparison calls that withdrawn every Monday.
    #[test]
    fn a_variant_covered_by_its_family_is_not_reported() {
        let ids = vec!["gpt-5.6-sol".to_string(), "gpt-5.6-terra".to_string()];
        assert!(drift(&ids, &priced(&["gpt-5.6"])).is_empty());
    }

    /// A catalog id nothing lists is worth saying; a Codex-only one is not.
    #[test]
    fn an_id_no_source_knows_is_reported_unless_it_is_codexs_own() {
        let ids = vec![
            "gpt-5.5".to_string(),
            "gpt-5.9-withdrawn".to_string(),
            "gpt-5.3-codex-spark".to_string(),
        ];
        let report = drift(&ids, &priced(&["gpt-5.5"]));
        assert_eq!(report.len(), 1, "{report:?}");
        assert!(report[0].contains("gpt-5.9-withdrawn"), "{report:?}");
        assert!(report[0].contains("renamed or withdrawn"));
    }

    /// A new model in the family is a question, and one outside it is not
    /// even that: OpenRouter lists every OpenAI model.
    #[test]
    fn a_new_family_member_is_asked_about_and_an_outsider_is_not() {
        let ids = vec!["gpt-5.5".to_string()];
        let report = drift(&ids, &priced(&["gpt-5.5", "gpt-5.6-cyber", "gpt-4o", "o3"]));
        assert_eq!(report.len(), 1, "{report:?}");
        assert!(report[0].contains("gpt-5.6-cyber"), "{report:?}");
        assert!(report[0].contains("does Codex serve it"));
    }

    /// A model from a generation the catalog has moved past is not a
    /// question. OpenAI still prices every variant it ever shipped, and
    /// without this the check reported a dozen of them a week.
    #[test]
    fn an_older_generation_is_not_asked_about() {
        let ids = vec!["gpt-5.6-sol".to_string()];
        // `gpt-5.6` covers the catalog's own id; the rest are generations it
        // has moved past.
        let sources = priced(&["gpt-5.6", "gpt-5.2-pro", "gpt-5.4-nano"]);
        let report = drift(&ids, &sources);
        assert!(report.is_empty(), "{report:?}");

        // The floor is the catalog's own newest, so the same sources against
        // an older catalog do produce questions.
        let older_catalog = vec!["gpt-5.2-pro".to_string()];
        let report = drift(&older_catalog, &sources);
        assert_eq!(report.len(), 2, "{report:?}");
    }

    /// A catalog that matches its sources says nothing at all.
    #[test]
    fn a_current_catalog_reports_nothing() {
        let ids = vec!["gpt-5.5".to_string(), "gpt-5.3-codex-spark".to_string()];
        assert!(drift(&ids, &priced(&["gpt-5.5"])).is_empty());
    }
}

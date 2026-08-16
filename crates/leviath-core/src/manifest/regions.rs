//! Parsing `[context]` and the `[context.regions]` table: each region's kind,
//! budget, seed, and the tool-output routing that targets it.

use super::*;

/// Parse a `[context.regions]` (or `[stages.<name>.context.regions]`) table into
/// region definitions plus the summed absolute-budget total.
///
/// Each region may express its ceiling as a percentage of the model context
/// window (`budget = "35%"`) with optional absolute guard-rails (`max_tokens`
/// caps it, `min_tokens` floors it), or as a plain absolute `max_tokens` (the
/// legacy form, default 5000). Compacting regions may set `compact_at = "80%"`
/// (compact at that fraction of the resolved budget) and/or an absolute
/// `threshold_tokens` cap. Percentage regions carry a provisional `max_tokens`
/// (the cap, or 0) that is finalized when the layout is resolved against a model
/// window at spawn - see [`ContextLayout::resolved`]. The returned total sums
/// only the absolute maxes; percentage regions contribute at resolution time.
///
/// Malformed `budget`/`compact_at` strings are a hard error so `leviath validate`
/// catches them at load.
pub(super) fn parse_region_layout(
    regions_table: &toml::value::Table,
) -> Result<(Vec<RegionDefinition>, usize)> {
    let mut regions = Vec::new();
    let mut total_tokens = 0usize;

    for (region_name, region_value) in regions_table {
        // `budget = "N%"` opts a region into percentage mode; `max_tokens` then
        // becomes the absolute cap and `min_tokens` the absolute floor. Without a
        // `budget`, `max_tokens` is the literal ceiling (legacy behavior).
        let percent = match region_value.get("budget").and_then(|v| v.as_str()) {
            Some(s) => Some(crate::BudgetSpec::parse_budget(s).map_err(Error::Other)?),
            None => None,
        };
        let max_tokens_opt = region_value
            .get("max_tokens")
            .and_then(|v| v.as_integer())
            .map(|v| v as usize);
        let min_tokens = region_value
            .get("min_tokens")
            .and_then(|v| v.as_integer())
            .map(|v| v as usize);

        let budget = match percent {
            Some(percent) => crate::BudgetSpec::Percent {
                percent,
                min: min_tokens,
                max: max_tokens_opt,
            },
            None => crate::BudgetSpec::Absolute(max_tokens_opt.unwrap_or(5000)),
        };
        // Provisional resolved ceiling: the literal value for absolute regions,
        // the cap (or 0) for percentage regions until resolution overwrites it.
        let provisional_max_tokens = match &budget {
            crate::BudgetSpec::Absolute(n) => *n,
            crate::BudgetSpec::Percent { max, .. } => max.unwrap_or(0),
        };

        // Compacting regions carry a compaction trigger. Parse `compact_at` (a
        // fraction of the resolved budget) and the absolute `threshold_tokens`
        // guard, and reconcile them into (RegionDefinition.compact_at, the value
        // stored on RegionKind::Compacting) per the resolution contract in
        // `ContextLayout::resolve_compacting_threshold`.
        let compact_at = match region_value.get("compact_at").and_then(|v| v.as_str()) {
            Some(s) => Some(crate::BudgetSpec::parse_budget(s).map_err(Error::Other)?),
            None => None,
        };
        let explicit_threshold = region_value
            .get("threshold_tokens")
            .and_then(|v| v.as_integer())
            .map(|v| v as usize);

        let kind_str = region_value
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("temporary");

        let kind = match kind_str {
            "pinned" => RegionKind::Pinned,
            "sliding_window" => {
                let max_items = region_value
                    .get("max_items")
                    .and_then(|v| v.as_integer())
                    .unwrap_or(10) as usize;
                let eviction_strategy = match region_value.get("strategy").and_then(|v| v.as_str())
                {
                    Some("bulk") => {
                        let overflow = region_value
                            .get("overflow")
                            .and_then(|v| v.as_integer())
                            .unwrap_or(10) as usize;
                        EvictionStrategy::Bulk { overflow }
                    }
                    Some("compact") => {
                        let compact_count = region_value
                            .get("compact_count")
                            .and_then(|v| v.as_integer())
                            .unwrap_or(10) as usize;
                        EvictionStrategy::Compact { compact_count }
                    }
                    Some("per_item") | None => EvictionStrategy::PerItem,
                    // Unknown used to mean per_item, so `strategy = "per-item"`
                    // or a mistyped `compact` left the region evicting one
                    // entry at a time with no sign the setting was read.
                    Some(other) => {
                        return Err(Error::Other(format!(
                            "region '{region_name}': strategy \"{other}\" is not \
                             valid (valid: per_item, bulk, compact)"
                        )));
                    }
                };
                RegionKind::SlidingWindow {
                    max_items,
                    eviction_strategy,
                }
            }
            "temporary" => RegionKind::Temporary,
            "compacting" => {
                // Reconcile compact_at / threshold_tokens into the value stored on
                // the kind (the absolute cap or the usize::MAX "no cap" sentinel);
                // resolution turns it into the concrete threshold.
                let threshold = match (compact_at, explicit_threshold, percent.is_some()) {
                    (Some(_), Some(cap), _) => cap,
                    (Some(_), None, _) => usize::MAX,
                    (None, Some(t), _) => t,
                    // No compact_at and no threshold: default to 80% of the budget
                    // for percentage regions (resolved later), else the legacy
                    // absolute `max_tokens * 8 / 10`.
                    (None, None, true) => usize::MAX,
                    (None, None, false) => provisional_max_tokens * 8 / 10,
                };
                RegionKind::Compacting {
                    threshold_tokens: threshold,
                }
            }
            "clearable" => RegionKind::Clearable,
            "compact_history" => {
                let source = region_value
                    .get("source_region")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                RegionKind::CompactHistory {
                    source_region: source,
                }
            }
            "checklist" => RegionKind::Checklist,
            "hashmap" | "hash_map" => {
                let max_entries = region_value
                    .get("max_entries")
                    .and_then(|v| v.as_integer())
                    .map(|v| v as usize);
                RegionKind::HashMap { max_entries }
            }
            "custom" => {
                let script = region_value
                    .get("script")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        Error::Other(format!(
                            "region '{region_name}': kind = \"custom\" requires \
                             script = \"<path>.rhai\""
                        ))
                    })?
                    .to_string();
                let persistent = region_value
                    .get("persistent")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                RegionKind::Custom { script, persistent }
            }
            unknown => {
                // A typo'd kind used to silently become Temporary - for a
                // custom region that would mean the script never runs, with
                // no signal anywhere. Fail at load instead; `lev validate`
                // surfaces this immediately.
                return Err(Error::Other(format!(
                    "region '{region_name}': unknown kind \"{unknown}\" (valid kinds: \
                     pinned, sliding_window, temporary, compacting, clearable, \
                     compact_history, checklist, hashmap, custom)"
                )));
            }
        };

        // The effective compact_at fraction to store on the region: an explicit
        // value, or the 80% default for a percentage-budget compacting region
        // with no explicit threshold (so it resolves relative to the budget).
        let compact_at_field = match (kind_str, compact_at, explicit_threshold, percent.is_some()) {
            ("compacting", Some(f), _, _) => Some(f),
            ("compacting", None, None, true) => Some(0.80),
            _ => None,
        };

        let required = region_value
            .get("required")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let required_message = region_value
            .get("required_message")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Default on: a region is summarizable unless its author says the
        // content does not survive a paraphrase.
        let summarizable = region_value
            .get("summarizable")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // Default `evict`: making room is what every region did before this
        // setting existed, and it is right for the transcript regions that are
        // the majority of them.
        let admission = match region_value.get("admission").and_then(|v| v.as_str()) {
            Some("reject") => crate::region::Admission::Reject,
            Some("evict") | None => crate::region::Admission::Evict,
            Some(other) => {
                return Err(crate::error::Error::ValidationFailed(format!(
                    "region '{region_name}' has admission = \"{other}\"; \
                     expected \"evict\" or \"reject\""
                )));
            }
        };

        // One line on what the region is for, shown to the model above its
        // contents. Optional: most regions are named well enough that a
        // sentence would only cost tokens.
        let description = region_value
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let seed = parse_region_seed(region_name, region_value.get("seed"));

        // Percentage regions contribute their (unknown) size at resolution, so
        // only absolute budgets add to the summed total here.
        if percent.is_none() {
            total_tokens += provisional_max_tokens;
        }

        let mut def = RegionDefinition::new(region_name.clone(), kind, provisional_max_tokens)
            .with_budget(budget)
            .with_required(required, required_message);
        def.summarizable = summarizable;
        def.admission = admission;
        def.description = description;
        if let Some(f) = compact_at_field {
            def = def.with_compact_at(f);
        }
        if let Some(seed) = seed {
            def = def.with_seed(seed);
        }
        regions.push(def);
    }

    Ok((regions, total_tokens))
}

/// Parse one `[[transforms.mappings]]` entry. An omitted or unrecognized
/// `transform` yields `None` (a plain region copy at apply time).
pub(super) fn parse_region_mapping(v: &toml::Value) -> RegionMapping {
    let transform = match v.get("transform").and_then(|x| x.as_str()) {
        Some("direct") => Some(ContentTransform::Direct),
        Some("summarize") => Some(ContentTransform::Summarize),
        Some("extract") => Some(ContentTransform::Extract {
            fields: v
                .get("fields")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        }),
        _ => None,
    };
    RegionMapping {
        from_region: str_field(v, "from_region"),
        to_region: str_field(v, "to_region"),
        transform,
    }
}

/// Parse a region's `seed` value from `[context.regions.<name>]`.
///
/// String forms: `"task_input"` → caller input keyed `task` (the `--task`/prompt
/// text); any other string → caller input keyed by that string, with the
/// convenience alias `"input"` meaning "keyed by this region's own name".
/// Table forms: `{ glob = "…" }`, `{ files = [...] }`, `{ literal = "…" }`,
/// `{ rhai = "…" }`, `{ command = "…" }`, or `{ caller = "…" }`.
///
/// Back-compat: a region literally named `task` with no `seed` gets an implicit
/// `CallerInput { name: "task" }`, so unmodified blueprints seed the task text
/// exactly as before.
pub(super) fn parse_region_seed(
    region_name: &str,
    value: Option<&toml::Value>,
) -> Option<RegionSeed> {
    let Some(value) = value else {
        return (region_name == "task").then(|| RegionSeed::CallerInput {
            name: "task".to_string(),
        });
    };
    match value {
        toml::Value::String(s) => Some(match s.as_str() {
            "task_input" => RegionSeed::CallerInput {
                name: "task".to_string(),
            },
            "input" => RegionSeed::CallerInput {
                name: region_name.to_string(),
            },
            other => RegionSeed::CallerInput {
                name: other.to_string(),
            },
        }),
        toml::Value::Table(t) => {
            if let Some(pattern) = t.get("glob").and_then(|v| v.as_str()) {
                Some(RegionSeed::Glob {
                    pattern: pattern.to_string(),
                })
            } else if let Some(files) = t.get("files").and_then(|v| v.as_array()) {
                Some(RegionSeed::Files {
                    paths: files
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect(),
                })
            } else if let Some(text) = t.get("literal").and_then(|v| v.as_str()) {
                Some(RegionSeed::Literal {
                    text: text.to_string(),
                })
            } else if let Some(script) = t.get("rhai").and_then(|v| v.as_str()) {
                Some(RegionSeed::Rhai {
                    script: script.to_string(),
                })
            } else if let Some(command) = t.get("command").and_then(|v| v.as_str()) {
                Some(RegionSeed::Command {
                    command: command.to_string(),
                })
            } else {
                t.get("caller")
                    .and_then(|v| v.as_str())
                    .map(|name| RegionSeed::CallerInput {
                        name: name.to_string(),
                    })
            }
        }
        _ => None,
    }
}

//! Parsing the standalone top-level tables - the ones that configure the agent
//! as a whole rather than any one stage.

use super::*;

/// Parse `[compaction]` over the defaults, leaving any field the manifest does
/// not mention at its default rather than at zero.
pub(super) fn parse_compaction_config(table: &toml::value::Table) -> Result<CompactionConfig> {
    let mut cc = CompactionConfig::default();

    if let Some(provider) = str_of(table, "provider") {
        cc.provider = provider.to_string();
    }
    if let Some(model) = str_of(table, "model") {
        cc.model = model.to_string();
    }
    if let Some(sp) = str_of(table, "system_prompt") {
        cc.system_prompt = Some(sp.to_string());
    }
    if let Some(mst) = count_of(table, "[compaction]", "max_summary_tokens")? {
        cc.max_summary_tokens = mst;
    }
    if let Some(temp) = table.get("temperature").and_then(|v| v.as_float()) {
        cc.temperature = temp as f32;
    }

    Ok(cc)
}

/// Parse `[read_paths]`. Entries are syntax-checked here so a broken one fails
/// `lev validate`/`lev add`/spawn loudly, instead of degrading the agent at its
/// first out-of-workdir read.
pub(super) fn parse_read_paths(
    table: &toml::value::Table,
) -> Result<crate::blueprint::ReadPathsConfig> {
    let mut allow = Vec::new();
    if let Some(entries) = array_of(table, "allow") {
        for entry in entries {
            let Some(raw) = entry.as_str() else {
                return Err(Error::Other(format!(
                    "[read_paths] allow entries must be strings, got: {entry}"
                )));
            };
            crate::read_paths::validate_entry_syntax(raw).map_err(Error::Other)?;
            allow.push(raw.to_string());
        }
    }
    Ok(crate::blueprint::ReadPathsConfig { allow })
}

/// Parse `[safe_commands]`: what this agent would like to run unprompted.
///
/// Inert until the user opts in, so parsing is permissive - but a non-string
/// entry is still a hard error, because a list that silently loses members
/// reads as a grant that was made.
pub(super) fn parse_safe_commands(
    table: &toml::value::Table,
) -> Result<crate::blueprint::SafeCommandsConfig> {
    let strings = |field: &str| -> Result<Vec<String>> {
        let Some(entries) = table.get(field).and_then(|v| v.as_array()) else {
            return Ok(Vec::new());
        };
        entries
            .iter()
            .map(|entry| {
                entry.as_str().map(str::to_string).ok_or_else(|| {
                    Error::Other(format!(
                        "[safe_commands] {field} entries must be strings, got: {entry}"
                    ))
                })
            })
            .collect()
    };
    Ok(crate::blueprint::SafeCommandsConfig {
        tools: strings("tools")?,
        shell: strings("shell")?,
    })
}

/// Flatten `[tool_permissions]` into the `tool_perm:<tool>` metadata keys the
/// permission layer reads.
///
/// A value that cannot be read is refused here rather than left to
/// resolution: resolution maps anything it does not recognise to `ask`, so a
/// misspelled `deny` would become a prompt.
pub(super) fn tool_permission_metadata(
    table: &toml::value::Table,
) -> Result<Vec<(String, serde_json::Value)>> {
    table
        .iter()
        .map(|(tool_name, policy_val)| {
            let policy = policy_val.as_str().ok_or_else(|| {
                Error::Other(format!(
                    "[tool_permissions]: {tool_name} must be one of {}",
                    TOOL_POLICIES.join(", ")
                ))
            })?;
            validate_tool_policy("[tool_permissions]", tool_name, policy)?;
            Ok((
                format!("tool_perm:{}", tool_name),
                serde_json::Value::String(policy.to_string()),
            ))
        })
        .collect()
}

/// Parse `[context.file_tracking]`. Tracking both directions into a `files`
/// region is the default because that is what the shipped layouts assume.
pub(super) fn parse_file_tracking(table: &toml::value::Table) -> Result<crate::FileTrackingConfig> {
    Ok(crate::FileTrackingConfig {
        region: str_of(table, "region").unwrap_or("files").to_string(),
        track_reads: bool_of(table, "track_reads").unwrap_or(true),
        track_writes: bool_of(table, "track_writes").unwrap_or(true),
        max_file_tokens: count_of(table, "[context.file_tracking]", "max_file_tokens")?,
    })
}

/// Parse `[repetition_detection]`. Every field stays `None` when absent so the
/// global config's value survives; there are no local defaults to apply here.
pub(super) fn parse_repetition_detection(
    table: &toml::value::Table,
) -> Result<crate::RepetitionDetectionConfig> {
    let where_ = "[repetition_detection]";
    Ok(crate::RepetitionDetectionConfig {
        max_repeat_calls: count_of(table, where_, "max_repeat_calls")?,
        max_readonly_streak: count_of(table, where_, "max_readonly_streak")?,
        enabled: bool_of(table, "enabled"),
    })
}

/// Parse an `[agent.output]` or `[stages.<name>.output]` block.
///
/// `format` is read as an opaque string and never matched against a known set:
/// a value this parser has never seen is as valid as `"markdown"`, which is what
/// lets a blueprint ask for a2ui, a house schema, or a format invented after
/// this code was written without touching it.
///
/// `schema` is taken as arbitrary TOML and converted to JSON, so an author can
/// write the schema inline as a TOML table rather than embedding a JSON string.
pub(super) fn parse_output_spec(table: &toml::value::Table) -> crate::output::OutputSpec {
    let string_field = |key: &str| {
        table
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
    };
    crate::output::OutputSpec {
        format: string_field("format"),
        instructions: string_field("instructions"),
        example: string_field("example"),
        // A schema that will not convert is dropped rather than fatal: the
        // validator itself already treats an uncompilable schema as "skip the
        // check" rather than "refuse every submission", and disagreeing here
        // would make the same bad schema fatal at load and harmless at dispatch.
        schema: table
            .get("schema")
            .and_then(|v| serde_json::to_value(v).ok()),
        validator: string_field("validator"),
    }
}

pub(super) fn parse_security_config(security_table: &toml::value::Table) -> crate::SecurityConfig {
    let mut sc = crate::SecurityConfig::default();
    if let Some(tt) = bool_of(security_table, "taint_tracking") {
        sc.taint_tracking = tt;
    }
    sc
}

/// Every key read off a `[sandbox]` table. Anything else is refused: a
/// misspelled `netwrok = false` would otherwise be ignored, leaving the
/// sandbox looser than the file said. The schema guard in `tests.rs` holds the
/// published schema to this list.
pub(super) const SANDBOX_KEYS: &[&str] = &[
    "engine",
    "image",
    "kind",
    "mount",
    "mounts",
    "network",
    "on_unavailable",
    "persist",
];

/// Parse a `[sandbox]` / `[stages.X.sandbox]` table into a `ToolSandboxConfig`.
/// A present block with no `kind` means host passthrough; omit the block to
/// inherit the broader (agent/global) sandbox. An unknown `kind` or
/// `on_unavailable` value is a hard error rather than a silently-ignored
/// misconfiguration (mirrors transition-condition/transform validation), and
/// so is a key the table does not have. `where_` is the prefix an error
/// carries: empty for the agent's own block, the stage for a stage's.
pub(super) fn parse_sandbox_config(
    where_: &str,
    table: &toml::value::Table,
) -> Result<crate::sandbox::ToolSandboxConfig> {
    use crate::sandbox::{OnUnavailable, SandboxKind, ToolSandboxConfig};

    reject_unknown_keys(&format!("{where_}sandbox"), table, SANDBOX_KEYS)?;
    let mut sc = ToolSandboxConfig::default();

    if let Some(kind) = str_of(table, "kind") {
        sc.kind = match kind {
            "none" => SandboxKind::None,
            "namespace" => SandboxKind::Namespace,
            "container" => SandboxKind::Container,
            other => {
                return Err(Error::Other(format!(
                    "sandbox has unknown kind '{other}' \
                     (valid: none, namespace, container)"
                )));
            }
        };
    }
    if let Some(image) = str_of(table, "image") {
        sc.image = Some(image.to_string());
    }
    if let Some(engine) = str_of(table, "engine") {
        sc.engine = Some(engine.to_string());
    }
    if let Some(network) = bool_of(table, "network") {
        sc.network = network;
    }
    if let Some(persist) = bool_of(table, "persist") {
        sc.persist = persist;
    }
    // Both spellings: the published schema lists both, and `config.toml`'s own
    // `[sandbox]` table (a different parser) documents `mounts`, so a blueprint
    // author copying from there wrote a key that was silently ignored.
    let listed = match (array_of(table, "mount"), array_of(table, "mounts")) {
        (Some(a), Some(b)) if a != b => {
            return Err(Error::Other(
                "sandbox names both `mount` and `mounts` with different lists; keep one"
                    .to_string(),
            ));
        }
        (Some(a), _) | (None, Some(a)) => Some(a),
        (None, None) => None,
    };
    if let Some(mounts) = listed {
        sc.mounts = mounts
            .iter()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect();
    }
    if let Some(ou) = str_of(table, "on_unavailable") {
        sc.on_unavailable = match ou {
            "error" => OnUnavailable::Error,
            "warn" => OnUnavailable::Warn,
            other => {
                return Err(Error::Other(format!(
                    "sandbox has unknown on_unavailable '{other}' \
                     (valid: error, warn)"
                )));
            }
        };
    }
    Ok(sc)
}

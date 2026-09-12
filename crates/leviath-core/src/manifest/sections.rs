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
///
/// `on_validator_error` is the one enum-ish field, and a value it does not
/// recognise is a hard error rather than a silent fallback: a misspelled
/// policy would otherwise load as the default and change what happens to a
/// run's answer. `where_` names the table an error came from.
pub(super) fn parse_output_spec(
    where_: &str,
    table: &toml::value::Table,
) -> Result<crate::output::OutputSpec> {
    let string_field = |key: &str| {
        table
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
    };
    let on_validator_error = match table.get("on_validator_error") {
        None => None,
        Some(value) => match value.as_str().map(str::trim) {
            Some("reject") => Some(crate::output::OnValidatorError::Reject),
            Some("accept") => Some(crate::output::OnValidatorError::Accept),
            _ => {
                return Err(Error::Other(format!(
                    "{where_}: on_validator_error must be \"reject\" or \"accept\", got: {value}"
                )));
            }
        },
    };
    let mut artifacts = Vec::new();
    if let Some(listed) = table.get("artifacts") {
        let items = listed.as_array().ok_or_else(|| {
            Error::Other(format!(
                "{where_}: artifacts must be a list of tables, e.g. \
                 [[stages.x.output.artifacts]] name = \"final\", type = \"video/mp4\""
            ))
        })?;
        for item in items {
            artifacts.push(parse_artifact_spec(where_, item)?);
        }
    }
    Ok(crate::output::OutputSpec {
        format: string_field("format"),
        instructions: string_field("instructions"),
        example: string_field("example"),
        artifacts,
        // A schema that will not convert is dropped rather than fatal: the
        // validator itself already treats an uncompilable schema as "skip the
        // check" rather than "refuse every submission", and disagreeing here
        // would make the same bad schema fatal at load and harmless at dispatch.
        schema: table
            .get("schema")
            .and_then(|v| serde_json::to_value(v).ok()),
        validator: string_field("validator"),
        on_validator_error,
    })
}

/// One `[[...output.artifacts]]` table: a name, a type or pattern, and
/// whether the submission may leave it out.
fn parse_artifact_spec(where_: &str, item: &toml::Value) -> Result<crate::output::ArtifactSpec> {
    let table = item.as_table().ok_or_else(|| {
        Error::Other(format!(
            "{where_}: each artifact must be a table with name and type, got: {item}"
        ))
    })?;
    let text = |key: &str| {
        table
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let name = text("name")
        .ok_or_else(|| Error::Other(format!("{where_}: an artifact needs a name")))?
        .to_string();
    let mime_type = text("type")
        .ok_or_else(|| {
            Error::Other(format!(
                "{where_}: artifact '{name}' needs a type, such as \"video/mp4\" or \"image/*\""
            ))
        })?
        .to_ascii_lowercase();
    let well_formed = mime_type
        .split_once('/')
        .is_some_and(|(kind, sub)| !kind.is_empty() && !sub.is_empty() && !sub.contains('/'));
    if !well_formed {
        return Err(Error::Other(format!(
            "{where_}: artifact '{name}' has type '{mime_type}', which is not type/subtype"
        )));
    }
    Ok(crate::output::ArtifactSpec {
        name,
        mime_type,
        required: table
            .get("required")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        description: text("description").map(str::to_string),
    })
}

/// Every key [`parse_output_spec`] reads off an output table, for the schema
/// guard in `tests.rs`. Like `REGION_KEYS`, a list and not a check: the parser
/// ignores a key it does not know.
#[cfg(test)]
pub(super) const OUTPUT_KEYS: &[&str] = &[
    "artifacts",
    "example",
    "format",
    "instructions",
    "on_validator_error",
    "schema",
    "validator",
];

/// Parse the `[[dependencies]]` array: what an agent needs in place before it
/// runs. Shape only - whether a dependency is satisfied is a spawn/`lev deps`
/// concern. A missing or unknown `kind`, or a kind missing its field, is a hard
/// error so a broken declaration fails `lev validate` rather than at spawn.
pub(super) fn parse_dependencies(
    items: &[toml::Value],
) -> Result<Vec<crate::blueprint::Dependency>> {
    items.iter().map(parse_dependency).collect()
}

fn parse_dependency(item: &toml::Value) -> Result<crate::blueprint::Dependency> {
    use crate::blueprint::{Dependency, DependencyKind};
    let table = item
        .as_table()
        .ok_or_else(|| Error::Other("each [[dependencies]] entry must be a table".to_string()))?;
    let text = |key: &str| {
        table
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let name = text("name")
        .ok_or_else(|| Error::Other("a dependency needs a name".to_string()))?
        .to_string();
    let kind_tag = text("kind").ok_or_else(|| {
        Error::Other(format!(
            "dependency '{name}' needs a kind (mcp_server, env, binary or script)"
        ))
    })?;
    let need = |key: &str| -> Result<String> {
        text(key).map(str::to_string).ok_or_else(|| {
            Error::Other(format!(
                "dependency '{name}' of kind '{kind_tag}' needs '{key}'"
            ))
        })
    };
    let string_list = |key: &str| -> Result<Vec<String>> {
        let Some(arr) = table.get(key).and_then(|v| v.as_array()) else {
            return Ok(Vec::new());
        };
        arr.iter()
            .map(|e| {
                e.as_str().map(str::to_string).ok_or_else(|| {
                    Error::Other(format!(
                        "dependency '{name}': {key} entries must be strings, got: {e}"
                    ))
                })
            })
            .collect()
    };
    let kind = match kind_tag {
        "mcp_server" => DependencyKind::McpServer {
            server: need("server")?,
            env: string_list("env")?,
        },
        "env" => DependencyKind::Env { var: need("var")? },
        "binary" => DependencyKind::Binary {
            command: need("command")?,
        },
        "script" => DependencyKind::Script {
            check: need("check")?,
        },
        other => {
            return Err(Error::Other(format!(
                "dependency '{name}' has unknown kind '{other}' \
                 (valid: mcp_server, env, binary, script)"
            )));
        }
    };
    let install = match table.get("install") {
        None => None,
        Some(value) => Some(parse_dependency_install(&name, value)?),
    };
    Ok(Dependency {
        name,
        kind,
        required: table
            .get("required")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        remedy: text("remedy").map(str::to_string),
        description: text("description").map(str::to_string),
        install,
    })
}

fn parse_dependency_install(
    dep: &str,
    value: &toml::Value,
) -> Result<crate::blueprint::DependencyInstall> {
    use crate::blueprint::{DependencyInstall, McpServerTemplate};
    let table = value
        .as_table()
        .ok_or_else(|| Error::Other(format!("dependency '{dep}': install must be a table")))?;
    let text = |key: &str| {
        table
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let str_map = |key: &str| -> Result<std::collections::BTreeMap<String, String>> {
        let Some(t) = table.get(key).and_then(|v| v.as_table()) else {
            return Ok(std::collections::BTreeMap::new());
        };
        t.iter()
            .map(|(k, v)| {
                v.as_str()
                    .map(|s| (k.clone(), s.to_string()))
                    .ok_or_else(|| {
                        Error::Other(format!(
                            "dependency '{dep}': install.{key} values must be strings"
                        ))
                    })
            })
            .collect()
    };
    let server = match table.get("server") {
        None => None,
        Some(sv) => {
            let st = sv.as_table().ok_or_else(|| {
                Error::Other(format!(
                    "dependency '{dep}': install.server must be a table"
                ))
            })?;
            let stext = |key: &str| {
                st.get(key)
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            };
            let args = match st.get("args").and_then(|v| v.as_array()) {
                None => Vec::new(),
                Some(a) => a
                    .iter()
                    .map(|e| {
                        e.as_str().map(str::to_string).ok_or_else(|| {
                            Error::Other(format!(
                                "dependency '{dep}': install.server.args entries must be strings"
                            ))
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            };
            let str_table = |key: &str| -> Result<std::collections::BTreeMap<String, String>> {
                match st.get(key).and_then(|v| v.as_table()) {
                    None => Ok(std::collections::BTreeMap::new()),
                    Some(h) => h
                        .iter()
                        .map(|(k, v)| {
                            v.as_str().map(|s| (k.clone(), s.to_string())).ok_or_else(|| {
                                Error::Other(format!(
                                    "dependency '{dep}': install.server.{key} values must be strings"
                                ))
                            })
                        })
                        .collect(),
                }
            };
            Some(McpServerTemplate {
                transport: stext("transport"),
                command: stext("command"),
                url: stext("url"),
                args,
                headers: str_table("headers")?,
                env: str_table("env")?,
            })
        }
    };
    Ok(DependencyInstall {
        command: text("command").map(str::to_string),
        commands: str_map("commands")?,
        script: text("script").map(str::to_string),
        server,
    })
}

/// Every key a `[[dependencies]]` entry may carry, across all kinds, for the
/// schema guard in `tests.rs`. A flat union: the parser reads the keys its kind
/// needs and kind mismatches are caught in `Blueprint::validate`.
#[cfg(test)]
pub(super) const DEPENDENCIES_KEYS: &[&str] = &[
    "check",
    "command",
    "description",
    "env",
    "install",
    "kind",
    "name",
    "remedy",
    "required",
    "server",
    "var",
];

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

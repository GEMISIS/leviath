//! What a script tool may do: its declared permissions resolved against the
//! run's policy. Split out of `script_host.rs` for size.

use super::*;

/// Resolve `[tool_script_permissions]` into concrete allow/deny booleans.
///
/// `Allow`/`Deny` map directly. `Inherit` means:
/// - `read_file` / `write_file` / `shell`: permitted only when the agent's resolved policy for
///   the equivalent built-in (`resolve_builtin`) is [`ToolPolicy::Allow`]. This
///   is evaluated once against the entry stage's permission layers; a later
///   stage's `tool_permissions` do not re-gate a script's host calls.
/// - `http_get` / `http_post` / `env_var`: permitted (no built-in equivalent to
///   inherit from, and the tool itself is still gated by Layers 1/2/4).
///
/// `resolve_builtin` is a `&dyn Fn` (not `impl Fn`) so this function has a single
/// monomorphization; otherwise each distinct caller closure type gets its own
/// copy of the `net`/`filelike` match arms, and coverage is attributed
/// per-instantiation (each only exercises the arms that caller hits).
pub fn resolve_script_permissions(
    perms: &ScriptToolPermissions,
    resolve_builtin: &dyn Fn(&str) -> ToolPolicy,
) -> ScriptAllow {
    let net = |p: ScriptPermission| match p {
        ScriptPermission::Allow | ScriptPermission::Inherit => true,
        ScriptPermission::Deny => false,
    };
    let filelike = |p: ScriptPermission, builtin: &str| match p {
        ScriptPermission::Allow => true,
        ScriptPermission::Deny => false,
        ScriptPermission::Inherit => resolve_builtin(builtin) == ToolPolicy::Allow,
    };
    ScriptAllow {
        http_get: net(perms.http_get),
        http_post: net(perms.http_post),
        env_var: net(perms.env_var),
        read_file: filelike(perms.read_file, "read_file"),
        write_file: filelike(perms.write_file, "write_file"),
        shell: filelike(perms.shell, "shell"),
    }
}

/// Map a `[tool_script_permissions]` string to a [`ScriptPermission`]. An
/// unrecognized value yields `None` (the field is left at the global default) -
/// parsed by hand (not via `Deserialize`) so every arm is deterministically
/// covered, without pulling in serde's unexercised visitor machinery.
fn parse_script_permission_str(s: &str) -> Option<ScriptPermission> {
    match s {
        "allow" => Some(ScriptPermission::Allow),
        "deny" => Some(ScriptPermission::Deny),
        "inherit" => Some(ScriptPermission::Inherit),
        _ => None,
    }
}

/// How restrictive a script permission is, for clamping.
///
/// `Allow` (unconditional) is the loosest; `Inherit` still requires the agent's
/// own policy for the equivalent built-in to permit the call; `Deny` is the
/// tightest.
fn script_restrictiveness(p: ScriptPermission) -> u8 {
    match p {
        ScriptPermission::Allow => 0,
        ScriptPermission::Inherit => 1,
        ScriptPermission::Deny => 2,
    }
}

/// The effective `[tool_script_permissions]` for an agent: the user's global
/// config with the agent's own blueprint `[tool_script_permissions]` overlaid
/// per field - but **only where the manifest is more restrictive**.
///
/// Agents ship their own `.rhai` tool scripts, so it is reasonable for a
/// manifest to say "this agent never needs `shell`". It is not reasonable for it
/// to say the opposite: a manifest that could set `shell = "allow"` over a user's
/// global `deny` meant installing an agent was enough to overrule the machine's
/// configuration. So a manifest may tighten a field and never loosen it, the same
/// rule [`crate::tools::resolve_policy`] applies to `[tool_permissions]`.
///
/// Parsed CLI-side (these types live in the CLI config, not `leviath-core`),
/// mirroring `parse_blueprint_mcp_servers`.
pub fn effective_script_permissions(
    global: &ScriptToolPermissions,
    manifest_toml: &str,
) -> ScriptToolPermissions {
    let mut eff = global.clone();
    // `toml::from_str`, not `manifest_toml.parse::<toml::Value>()`. In toml 1.x
    // `FromStr for Value` parses a single *value*, not a document - so a real
    // manifest starting with `[agent]` reads as an array literal followed by
    // junk and fails. It still compiles, so the change is silent; the tests are
    // what caught it.
    let Ok(value) = toml::from_str::<toml::Value>(manifest_toml) else {
        return eff;
    };
    let Some(table) = value
        .get("tool_script_permissions")
        .and_then(|v| v.as_table())
    else {
        return eff;
    };
    // For each key the agent set to a recognized value, keep whichever of the
    // two is stricter.
    let apply = |key: &str, slot: &mut ScriptPermission| {
        if let Some(p) = table
            .get(key)
            .and_then(|v| v.as_str())
            .and_then(parse_script_permission_str)
            && script_restrictiveness(p) > script_restrictiveness(*slot)
        {
            *slot = p;
        }
    };
    apply("http_get", &mut eff.http_get);
    apply("http_post", &mut eff.http_post);
    apply("shell", &mut eff.shell);
    apply("read_file", &mut eff.read_file);
    apply("write_file", &mut eff.write_file);
    apply("env_var", &mut eff.env_var);
    eff
}

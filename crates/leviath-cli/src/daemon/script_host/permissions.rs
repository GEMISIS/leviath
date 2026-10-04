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
pub(crate) fn resolve_script_permissions(
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

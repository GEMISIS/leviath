//! Holding a call's writes to the write paths in force at its stage.
//!
//! The fence itself (the globs, their layers, and how a path is resolved
//! against them) is [`leviath_core::write_paths`]. This is what a tool call
//! writes, read off its arguments: the `path` of `write_file` and `edit_file`,
//! and every redirect target of a `shell` line.
//!
//! What it does not see is just as much a part of the contract. A shell
//! command that writes without a redirect (`cp`, `mv`, `tee`, `sed -i`), an MCP
//! tool, and the install tools all write where their own permissions let
//! them. `lev validate` warns about a stage that sets write paths and still
//! has shell or MCP tools.

use leviath_core::write_paths::WriteFence;

use crate::shell_keys::WriteTargets;

/// The refusal for a call to `tool_name` with `arguments` that would write
/// outside `fence`, or `None` when it writes only inside it, writes nothing a
/// fence covers, or the fence is open.
pub(crate) fn write_paths_refusal(
    tool_name: &str,
    arguments: &serde_json::Value,
    workdir: &std::path::Path,
    fence: &WriteFence,
) -> Option<String> {
    write_paths_refusal_for(
        tool_name,
        arguments,
        workdir,
        fence,
        crate::shell_keys::BACKSLASH_ESCAPES,
    )
}

/// [`write_paths_refusal`] with the shell's escape rule supplied, so both
/// platforms' readings are testable on either.
pub(crate) fn write_paths_refusal_for(
    tool_name: &str,
    arguments: &serde_json::Value,
    workdir: &std::path::Path,
    fence: &WriteFence,
    backslash_escapes: bool,
) -> Option<String> {
    if fence.is_open() {
        return None;
    }
    let tool = leviath_tools::canonical_tool_name(tool_name);
    let targets: Vec<String> = match tool {
        "write_file" | "edit_file" => arguments
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .into_iter()
            .collect(),
        "shell" => {
            let command = arguments.get("command").and_then(|v| v.as_str())?;
            match crate::shell_keys::every_write_target_for(command, backslash_escapes) {
                WriteTargets::Known(targets) => targets,
                WriteTargets::Unreadable => {
                    return Some(format!(
                        "[blocked] shell refused: this line redirects a write to a place that \
                         cannot be read ahead of time (a variable, a substitution, or a construct \
                         that does not parse), so it cannot be held to this stage's write paths \
                         ({}). Redirect to a literal path inside them, or use write_file.",
                        fence.describe()
                    ));
                }
            }
        }
        _ => return None,
    };
    let shown = targets.iter().find_map(|t| fence.check(t, workdir).err())?;
    Some(format!(
        "[blocked] {tool} refused: {shown} is outside this stage's write paths ({}). Write \
         inside them instead.",
        fence.describe()
    ))
}

#[cfg(test)]
#[path = "write_fence_tests.rs"]
mod tests;

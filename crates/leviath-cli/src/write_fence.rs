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

use leviath_core::write_paths::Refused;

use crate::shell_keys::{Landing, Target};

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
    let targets: Vec<Target> = match tool {
        "write_file" | "edit_file" => arguments
            .get("path")
            .and_then(|v| v.as_str())
            .map(|path| Target {
                written: path.to_string(),
                lands: vec![path.into()],
            })
            .into_iter()
            .collect(),
        "shell" => {
            let command = arguments.get("command").and_then(|v| v.as_str())?;
            let home = crate::yolo::home();
            match crate::shell_keys::landing_for(
                command,
                backslash_escapes,
                workdir,
                home.as_deref(),
            ) {
                Landing::Known(targets) => targets,
                Landing::Lost => {
                    return Some(format!(
                        "[blocked] shell refused: this line may change directory in a way that \
                         is not followed (a `cd` behind `{{`, `!`, `then`, `command` or an \
                         assignment, `popd`, `CDPATH`, `.`, `source` or `eval`), so where its \
                         relative redirects land cannot be held to this stage's write paths \
                         ({}). Redirect to a path from the workdir on a line that does not move, \
                         or use write_file.",
                        fence.describe()
                    ));
                }
                Landing::Unparsed | Landing::Unnamed => {
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
    targets
        .iter()
        .find_map(|target| refusal_for(tool, target, workdir, fence))
}

/// The refusal for one target, or `None` when every place it may land is
/// inside the fence. A target that lands inside from one directory and
/// outside from another (a `cd` that may fail) says which path to use.
fn refusal_for(
    tool: &str,
    target: &Target,
    workdir: &std::path::Path,
    fence: &WriteFence,
) -> Option<String> {
    let judged: Vec<(String, Result<(), Refused>)> = target
        .lands
        .iter()
        .map(|land| {
            let land = land.to_string_lossy().into_owned();
            let verdict = fence.check(&land, workdir);
            (land, verdict)
        })
        .collect();
    let refused = judged.iter().find_map(|(_, v)| v.clone().err())?;
    let inside = judged.iter().find(|(_, v)| v.is_ok());
    Some(match (inside, &refused) {
        (Some((land, _)), Refused::Outside(shown)) => format!(
            "[blocked] {tool} refused: '{written}' may land at {shown}, since the `cd` before it \
             may or may not happen, and {shown} is outside this stage's write paths ({globs}). \
             Use {from_workdir} (a path from the workdir) without the `cd`.",
            written = target.written,
            globs = fence.describe(),
            from_workdir = from_workdir(land, workdir),
        ),
        _ => fence.refusal(tool, &refused),
    })
}

/// `land` relative to `workdir` when it is under it, as it is otherwise.
fn from_workdir(land: &str, workdir: &std::path::Path) -> String {
    let path = std::path::Path::new(land);
    path.strip_prefix(workdir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
#[path = "write_fence_tests.rs"]
mod tests;

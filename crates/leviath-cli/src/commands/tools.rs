//! `lev tools` — list and validate the globally available Rhai script tools
//! (issue #97). These live in `<leviath-home>/tools/` and are auto-discovered by
//! every agent at spawn; this command surfaces what's there (and what failed to
//! compile) without starting the daemon. Agent-specific tools are validated by
//! `lev validate <agent>` instead.

use std::path::{Path, PathBuf};

use clap::Args;
use leviath_scripting::{ScriptToolMeta, ScriptToolSet, SkippedTool};

/// Arguments for `lev tools`.
#[derive(Args)]
pub struct ToolsArgs {
    /// Emit the tool inventory as JSON instead of human-readable text.
    #[arg(long)]
    pub(crate) json: bool,
}

/// The global script-tools directory (`<leviath-home>/tools/`), mirroring the
/// daemon's own global scan in `spawn::script_scan_dirs`. `None` when no home
/// directory resolves.
fn global_tools_dir() -> Option<PathBuf> {
    crate::config::leviath_home_dir().map(|h| h.join("tools"))
}

/// The outcome of scanning a tools directory: the tools that compiled and the
/// files that were skipped (with the reason each failed).
struct ToolsReport {
    valid: Vec<ScriptToolMeta>,
    skipped: Vec<SkippedTool>,
}

/// Discover + compile the script tools in `dir` (if any), returning them sorted
/// by name alongside the skipped files. A `None`/absent dir yields an empty
/// report.
fn scan_tools(dir: Option<&Path>) -> ToolsReport {
    let dirs: Vec<PathBuf> = dir.map(Path::to_path_buf).into_iter().collect();
    let (set, skipped) = ScriptToolSet::discover(&dirs);
    let mut valid = set.metas();
    valid.sort_by(|a, b| a.name.cmp(&b.name));
    ToolsReport { valid, skipped }
}

/// Render one tool's parameters as a compact `name:type[!]` list (`!` marks a
/// required parameter).
fn params_summary(meta: &ScriptToolMeta) -> String {
    meta.params
        .iter()
        .map(|p| {
            let req = if p.required { "!" } else { "" };
            format!("{}:{}{req}", p.name, p.ty)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The JSON view of a report (built by hand — no derive — so the shape is
/// explicit and stable).
fn report_json(dir_label: &str, report: &ToolsReport) -> serde_json::Value {
    let tools: Vec<serde_json::Value> = report
        .valid
        .iter()
        .map(|m| {
            let params: Vec<serde_json::Value> = m
                .params
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "name": p.name,
                        "type": p.ty,
                        "required": p.required,
                        "description": p.description,
                    })
                })
                .collect();
            serde_json::json!({
                "name": m.name,
                "description": m.description,
                "requires": m.required_caps,
                "params": params,
            })
        })
        .collect();
    let skipped: Vec<serde_json::Value> = report
        .skipped
        .iter()
        .map(|s| {
            serde_json::json!({
                "path": s.path.display().to_string(),
                "reason": s.reason,
            })
        })
        .collect();
    serde_json::json!({ "dir": dir_label, "tools": tools, "skipped": skipped })
}

/// Print a report in human-readable form. Valid tools are `✓`, skipped files are
/// `✗` with their reason (non-fatal — invalid scripts are simply not advertised,
/// exactly as the daemon treats them).
fn print_human(dir_label: &str, report: &ToolsReport) {
    println!("Global script tools ({dir_label}):");
    if report.valid.is_empty() && report.skipped.is_empty() {
        println!("  (none)");
        return;
    }
    for meta in &report.valid {
        let desc = if meta.description.is_empty() {
            String::new()
        } else {
            format!(" — {}", meta.description)
        };
        println!("  ✓ {}{desc}", meta.name);
        let params = params_summary(meta);
        if !params.is_empty() {
            println!("      params: {params}");
        }
        if !meta.required_caps.is_empty() {
            println!("      requires: {}", meta.required_caps.join(", "));
        }
    }
    for s in &report.skipped {
        println!("  ✗ {}: {}", s.path.display(), s.reason);
    }
}

/// Testable core: scan `dir`, then print the report as JSON or text.
fn run(dir: Option<&Path>, json: bool) -> anyhow::Result<()> {
    let report = scan_tools(dir);
    let dir_label = dir.map_or_else(
        || "<no home directory>".to_string(),
        |d| d.display().to_string(),
    );
    if json {
        // The report is plain `serde_json::Value`; serialization is infallible.
        let text = serde_json::to_string_pretty(&report_json(&dir_label, &report))
            .expect("tools report serializes");
        println!("{text}");
    } else {
        print_human(&dir_label, &report);
    }
    Ok(())
}

/// `lev tools` entry point.
pub async fn execute(args: ToolsArgs) -> anyhow::Result<()> {
    run(global_tools_dir().as_deref(), args.json)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tools dir with one valid tool (params + a `@requires`) and one broken
    /// script (no `@tool` directive → skipped).
    fn dir_with_mixed_tools() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("upper.rhai"),
            "// @tool upper\n// @description Upper-case text\n// @param text string required \"in\"\n// @requires network\nparams.text",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("broken.rhai"),
            "no tool directive here\nlet",
        )
        .unwrap();
        dir
    }

    #[test]
    fn scan_tools_lists_valid_and_skipped() {
        let dir = dir_with_mixed_tools();
        let report = scan_tools(Some(dir.path()));
        assert_eq!(report.valid.len(), 1);
        assert_eq!(report.valid[0].name, "upper");
        assert_eq!(report.valid[0].required_caps, ["network"]);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].reason.to_lowercase().contains("tool"));
    }

    #[test]
    fn scan_tools_none_dir_is_empty() {
        let report = scan_tools(None);
        assert!(report.valid.is_empty() && report.skipped.is_empty());
    }

    #[test]
    fn params_summary_marks_required() {
        let meta = ScriptToolMeta {
            name: "t".to_string(),
            description: String::new(),
            params: vec![
                leviath_scripting::ParamSpec {
                    name: "a".to_string(),
                    ty: "string".to_string(),
                    required: true,
                    description: String::new(),
                    schema: None,
                },
                leviath_scripting::ParamSpec {
                    name: "b".to_string(),
                    ty: "integer".to_string(),
                    required: false,
                    description: String::new(),
                    schema: None,
                },
            ],
            required_caps: vec![],
        };
        assert_eq!(params_summary(&meta), "a:string!, b:integer");
    }

    #[test]
    fn report_json_shape() {
        let dir = dir_with_mixed_tools();
        let report = scan_tools(Some(dir.path()));
        let v = report_json("d", &report);
        assert_eq!(v["dir"], "d");
        assert_eq!(v["tools"][0]["name"], "upper");
        assert_eq!(v["tools"][0]["requires"][0], "network");
        assert_eq!(v["tools"][0]["params"][0]["required"], true);
        assert!(v["skipped"][0]["reason"].as_str().is_some());
    }

    #[test]
    fn run_text_and_json_and_empty() {
        // Two valid tools — a full one (description + params + requires) and a
        // minimal one (none of those) — so print_human covers both the present
        // and absent branches of each field, and sort_by actually compares.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("zeta.rhai"),
            "// @tool zeta\n// @description Full tool\n// @param x string required\n// @requires network\nparams.x",
        )
        .unwrap();
        std::fs::write(dir.path().join("alpha.rhai"), "// @tool alpha\n1").unwrap();
        std::fs::write(dir.path().join("broken.rhai"), "no directive\nlet").unwrap();
        // Text + JSON over the populated dir.
        run(Some(dir.path()), false).unwrap();
        run(Some(dir.path()), true).unwrap();
        // Empty dir → the "(none)" branch.
        let empty = tempfile::tempdir().unwrap();
        run(Some(empty.path()), false).unwrap();
        // No home directory → the label fallback.
        run(None, true).unwrap();
    }

    #[test]
    fn global_tools_dir_reads_home() {
        let home = tempfile::tempdir().unwrap();
        temp_env::with_var("LEVIATH_HOME", Some(home.path().to_str().unwrap()), || {
            assert_eq!(global_tools_dir(), Some(home.path().join("tools")));
        });
    }
}

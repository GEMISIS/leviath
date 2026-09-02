//! `lev tools` - list and validate the globally available Rhai script tools.
//! These live in `<leviath-home>/tools/` and are auto-discovered by
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

/// The global script-tools directory (`~/.leviath/tools/`), mirroring the
/// daemon's own global scan in `spawn::script_scan_dirs`. `None` when no home
/// directory resolves.
///
/// This resolved to `$HOME/tools/` until the shared resolver landed - the
/// `"tools"` component was joined onto the *user home* rather than the
/// `.leviath` data root, unlike `providers/`, `agents/` and `runs/`. Every
/// `.rhai` file found here is compiled and offered to **every** agent as an
/// executable tool, and `$HOME/tools` is an ordinary directory a developer may
/// already have.
fn global_tools_dir() -> Option<PathBuf> {
    leviath_core::tools_dir()
}

/// The outcome of scanning a tools directory: the tools that compiled (each
/// with the file it came from) and the files that were skipped (with the
/// reason each failed).
struct ToolsReport {
    valid: Vec<(ScriptToolMeta, PathBuf)>,
    skipped: Vec<SkippedTool>,
}

/// Discover + compile the script tools in `dir` (if any), returning them sorted
/// by name alongside the skipped files. A `None`/absent dir yields an empty
/// report.
fn scan_tools(dir: Option<&Path>) -> ToolsReport {
    let dirs: Vec<PathBuf> = dir.map(Path::to_path_buf).into_iter().collect();
    let (set, skipped) = ScriptToolSet::discover(&dirs);
    let mut valid = set.sources();
    valid.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    ToolsReport { valid, skipped }
}

/// The `// installed by leviath: ...` line a script starts with, when it does.
/// Shared with the MCP server's `list_tools`.
///
/// `install_tool` is the audited way into the global directory and the only
/// one that writes this line, so a file without it was hand-written or put
/// there by something else (a `shell` call no sandbox confined, say). The
/// line is the whole record of who installed what: `lev tools` prints it
/// under each tool, and says so when a file has none.
///
/// Combinators rather than `?`: the path was just compiled from, so it is
/// readable and non-empty, and an early return for either would be a branch
/// nothing can reach.
pub(crate) fn provenance_line(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().and_then(|text| {
        text.lines()
            .next()
            .filter(|first| first.starts_with("// installed by leviath:"))
            .map(str::to_string)
    })
}

/// What `lev tools` prints under a tool whose file has no provenance line.
const NO_PROVENANCE: &str = "no provenance line (hand-written, or written outside install_tool)";

/// A parameter's type label: the scalar `type` for a flat param, or the `type`
/// inside a raw `schema` fragment (falling back to `schema` when the fragment has
/// no top-level `type`, e.g. a `oneOf`).
fn param_type_label(p: &leviath_scripting::ParamSpec) -> String {
    match &p.schema {
        Some(frag) => frag
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("schema")
            .to_string(),
        None => p.ty.clone(),
    }
}

/// Render one tool's parameters as a compact `name:type[!]` list (`!` marks a
/// required parameter). Shared with the MCP server's `list_tools`.
pub(crate) fn params_summary(meta: &ScriptToolMeta) -> String {
    meta.params
        .iter()
        .map(|p| {
            let req = if p.required { "!" } else { "" };
            format!("{}:{}{req}", p.name, param_type_label(p))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The JSON view of a report (built by hand - no derive - so the shape is
/// explicit and stable).
fn report_json(dir_label: &str, report: &ToolsReport) -> serde_json::Value {
    let tools: Vec<serde_json::Value> = report
        .valid
        .iter()
        .map(|(m, path)| {
            let params: Vec<serde_json::Value> = m
                .params
                .iter()
                .map(|p| {
                    // A raw `schema` fragment is surfaced verbatim (so `lev tools`
                    // shows the enum/bounds the model actually sees); otherwise the
                    // flat type/description.
                    match &p.schema {
                        Some(frag) => serde_json::json!({
                            "name": p.name,
                            "required": p.required,
                            "schema": frag,
                        }),
                        None => serde_json::json!({
                            "name": p.name,
                            "type": p.ty,
                            "required": p.required,
                            "description": p.description,
                        }),
                    }
                })
                .collect();
            serde_json::json!({
                "name": m.name,
                "description": m.description,
                "requires": m.required_caps,
                "available": crate::daemon::spawn::current_platform_satisfies(&m.required_caps),
                "params": params,
                // `null` for a file `install_tool` did not write.
                "provenance": provenance_line(path),
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

/// The human-readable report, one entry per line. Valid tools are `✓`, skipped
/// files are `✗` with their reason (non-fatal - invalid scripts are simply not
/// advertised, exactly as the daemon treats them). Built as lines rather than
/// printed so the rendering is testable.
fn human_lines(dir_label: &str, report: &ToolsReport) -> Vec<String> {
    let mut lines = vec![format!("Global script tools ({dir_label}):")];
    if report.valid.is_empty() && report.skipped.is_empty() {
        lines.push("  (none)".to_string());
        return lines;
    }
    for (meta, path) in &report.valid {
        let desc = if meta.description.is_empty() {
            String::new()
        } else {
            format!(" - {}", meta.description)
        };
        // A tool compiles but only loads if the platform satisfies its `@requires`
        // (the same gate the daemon applies at spawn); flag the ones that won't,
        // which also catches an unknown/typo'd capability name.
        let available = crate::daemon::spawn::current_platform_satisfies(&meta.required_caps);
        let marker = if available { "✓" } else { "⚠" };
        lines.push(format!("  {marker} {}{desc}", meta.name));
        if !available {
            lines.push("      unavailable on this platform (unsatisfiable @requires)".to_string());
        }
        let params = params_summary(meta);
        if !params.is_empty() {
            lines.push(format!("      params: {params}"));
        }
        if !meta.required_caps.is_empty() {
            lines.push(format!("      requires: {}", meta.required_caps.join(", ")));
        }
        // Where the file came from, as the file itself records it: the audit
        // this command exists for.
        lines.push(format!(
            "      {}",
            provenance_line(path).unwrap_or_else(|| NO_PROVENANCE.to_string())
        ));
    }
    for s in &report.skipped {
        lines.push(format!("  ✗ {}: {}", s.path.display(), s.reason));
    }
    lines
}

/// Print a report in human-readable form.
fn print_human(dir_label: &str, report: &ToolsReport) {
    for line in human_lines(dir_label, report) {
        println!("{line}");
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
pub(crate) async fn execute(args: ToolsArgs) -> anyhow::Result<()> {
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
        assert_eq!(report.valid[0].0.name, "upper");
        assert_eq!(report.valid[0].0.required_caps, ["network"]);
        assert_eq!(report.valid[0].1, dir.path().join("upper.rhai"));
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
    fn param_type_label_reads_flat_and_fragment() {
        let flat = leviath_scripting::ParamSpec {
            name: "a".into(),
            ty: "integer".into(),
            required: false,
            description: String::new(),
            schema: None,
        };
        assert_eq!(param_type_label(&flat), "integer");
        // A fragment with a top-level `type`.
        let typed = leviath_scripting::ParamSpec {
            schema: Some(serde_json::json!({ "type": "string", "enum": ["a", "b"] })),
            ..flat.clone()
        };
        assert_eq!(param_type_label(&typed), "string");
        // A fragment without a top-level `type` (e.g. oneOf) → "schema".
        let typeless = leviath_scripting::ParamSpec {
            schema: Some(serde_json::json!({ "oneOf": [] })),
            ..flat
        };
        assert_eq!(param_type_label(&typeless), "schema");
    }

    #[test]
    fn report_json_surfaces_raw_schema_fragment() {
        // A `.rhai` with a sibling `.toml` carrying a raw enum schema: the JSON
        // output shows the fragment verbatim (not a flat `type`).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pick.rhai"), "params.choice").unwrap();
        std::fs::write(
            dir.path().join("pick.toml"),
            "[tool]\nname = \"pick\"\n[[tool.params]]\nname = \"choice\"\nrequired = true\nschema = { type = \"string\", enum = [\"x\", \"y\"] }\n",
        )
        .unwrap();
        let report = scan_tools(Some(dir.path()));
        // params_summary reads the fragment's type.
        assert_eq!(params_summary(&report.valid[0].0), "choice:string!");
        let v = report_json("d", &report);
        let param = &v["tools"][0]["params"][0];
        assert_eq!(param["schema"]["enum"][1], "y");
        assert!(
            param.get("type").is_none(),
            "no flat type when a fragment is present"
        );
    }

    #[test]
    fn report_json_shape() {
        let dir = dir_with_mixed_tools();
        let report = scan_tools(Some(dir.path()));
        let v = report_json("d", &report);
        assert_eq!(v["dir"], "d");
        assert_eq!(v["tools"][0]["name"], "upper");
        assert_eq!(v["tools"][0]["requires"][0], "network");
        // `network` is satisfiable on this (desktop) platform.
        assert_eq!(v["tools"][0]["available"], true);
        assert_eq!(v["tools"][0]["params"][0]["required"], true);
        // Hand-written: no provenance line, and the JSON says so with `null`.
        assert!(v["tools"][0]["provenance"].is_null());
        assert!(v["skipped"][0]["reason"].as_str().is_some());
    }

    /// One file `install_tool` wrote (its first line is the provenance) and one
    /// written by hand: both renderings show where each came from.
    fn dir_with_installed_and_handwritten() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("fetch.rhai"),
            "// installed by leviath: mcp host, workdir /work/proj at 1700000000\n\
             // @tool fetch\n// @description Fetch a page\nparams.url",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("mine.rhai"),
            "// @tool mine\n// @description Hand-written\n1",
        )
        .unwrap();
        dir
    }

    #[test]
    fn provenance_line_reads_the_first_line_only_when_it_is_one() {
        let dir = dir_with_installed_and_handwritten();
        assert_eq!(
            provenance_line(&dir.path().join("fetch.rhai")).as_deref(),
            Some("// installed by leviath: mcp host, workdir /work/proj at 1700000000")
        );
        assert_eq!(provenance_line(&dir.path().join("mine.rhai")), None);
        // An empty file has no first line; a missing one cannot be read.
        std::fs::write(dir.path().join("empty.rhai"), "").unwrap();
        assert_eq!(provenance_line(&dir.path().join("empty.rhai")), None);
        assert_eq!(provenance_line(&dir.path().join("absent.rhai")), None);
    }

    #[test]
    fn human_lines_print_each_tools_provenance_or_say_it_has_none() {
        let dir = dir_with_installed_and_handwritten();
        let report = scan_tools(Some(dir.path()));
        let lines = human_lines("d", &report);
        let fetch = lines
            .iter()
            .position(|l| l.contains("fetch - Fetch a page"))
            .unwrap();
        assert_eq!(
            lines[fetch + 1],
            "      // installed by leviath: mcp host, workdir /work/proj at 1700000000"
        );
        let mine = lines
            .iter()
            .position(|l| l.contains("mine - Hand-written"))
            .unwrap();
        assert_eq!(lines[mine + 1], format!("      {NO_PROVENANCE}"));
        assert!(lines[mine + 1].contains("written outside install_tool"));
    }

    #[test]
    fn report_json_carries_the_provenance_line_or_null() {
        let dir = dir_with_installed_and_handwritten();
        let report = scan_tools(Some(dir.path()));
        let v = report_json("d", &report);
        assert_eq!(v["tools"][0]["name"], "fetch");
        assert_eq!(
            v["tools"][0]["provenance"],
            "// installed by leviath: mcp host, workdir /work/proj at 1700000000"
        );
        assert_eq!(v["tools"][1]["name"], "mine");
        assert!(v["tools"][1]["provenance"].is_null());
    }

    #[test]
    fn run_text_and_json_and_empty() {
        // Two valid tools - a full one (description + params + requires) and a
        // minimal one (none of those) - so print_human covers both the present
        // and absent branches of each field, and sort_by actually compares.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("zeta.rhai"),
            "// @tool zeta\n// @description Full tool\n// @param x string required\n// @requires network\nparams.x",
        )
        .unwrap();
        std::fs::write(dir.path().join("alpha.rhai"), "// @tool alpha\n1").unwrap();
        // An unsatisfiable capability → the `⚠` / unavailable branch.
        std::fs::write(
            dir.path().join("gpu.rhai"),
            "// @tool gpu\n// @requires gpu\n1",
        )
        .unwrap();
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
    fn global_tools_dir_is_under_the_leviath_data_dir() {
        let home = tempfile::tempdir().unwrap();
        temp_env::with_var("LEVIATH_HOME", Some(home.path().to_str().unwrap()), || {
            // `<home>/.leviath/tools`, alongside `providers/` and `agents/`.
            // This asserted `<home>/tools` before - the `"tools"` component was
            // joined onto the user home rather than the data root. Every `.rhai`
            // file in this directory becomes an executable tool for *every*
            // agent, so it belongs inside Leviath's own directory rather than in
            // a plausible-looking one at the top of the user's home.
            assert_eq!(
                global_tools_dir(),
                Some(home.path().join(".leviath").join("tools"))
            );
        });
    }
}

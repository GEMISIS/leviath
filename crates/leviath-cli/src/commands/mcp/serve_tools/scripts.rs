//! The Rhai tool tools: `install_tool` and `list_tools`.

use super::*;

pub(crate) fn install_tool(shared: &Shared, args: &Args) -> CallOutcome {
    let name = str_arg(args, "name").unwrap_or_default();
    let source = str_arg(args, "source").unwrap_or_default();
    let overwrite = bool_arg(args, "overwrite").unwrap_or(false);
    let provenance = format!("mcp host, workdir {}", shared.env.default_cwd);
    match leviath_tools::install_script_tool(
        shared.env.tools_dir.as_deref(),
        &name,
        &source,
        overwrite,
        &shared.reserved,
        Some(&provenance),
    ) {
        Ok(installed) => ok(
            installed.summary(),
            json!({
                "name": installed.name,
                "path": installed.path.display().to_string(),
                "description": installed.description,
                "parameters_schema": installed.parameters_schema,
                "requires": installed.requires,
                "replaced": installed.replaced,
            }),
            None,
        ),
        Err(e) => fail(
            format!("install_tool '{name}' refused: {e}"),
            json!({ "name": name }),
        ),
    }
}

pub(crate) fn list_tools(shared: &Shared) -> CallOutcome {
    let dirs: Vec<PathBuf> = shared.env.tools_dir.iter().cloned().collect();
    let (set, skipped) = leviath_scripting::ScriptToolSet::discover(&dirs);
    let mut sources = set.sources();
    sources.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    let tools: Vec<Value> = sources
        .iter()
        .map(|(meta, path)| {
            json!({
                "name": meta.name,
                "description": meta.description,
                "params": crate::commands::tools::params_summary(meta),
                "requires": meta.required_caps,
                "source": path.display().to_string(),
                "provenance": crate::commands::tools::provenance_line(path),
            })
        })
        .collect();
    let skipped_rows: Vec<Value> = skipped
        .iter()
        .map(|s| json!({ "path": s.path.display().to_string(), "reason": s.reason }))
        .collect();
    let mut lines: Vec<String> = tools
        .iter()
        .map(|t| {
            format!(
                "{}({}): {}{}",
                t["name"].as_str().unwrap_or(""),
                t["params"].as_str().unwrap_or(""),
                t["description"].as_str().unwrap_or(""),
                t["provenance"]
                    .as_str()
                    .map(|p| format!("  [{p}]"))
                    .unwrap_or_default()
            )
        })
        .collect();
    for s in &skipped_rows {
        lines.push(format!(
            "skipped {}: {}",
            s["path"].as_str().unwrap_or(""),
            s["reason"].as_str().unwrap_or("")
        ));
    }
    if lines.is_empty() {
        lines.push("no global Rhai tools installed".to_string());
    }
    ok(
        lines.join("\n"),
        json!({
            "dir": shared.env.tools_dir.as_ref().map(|d| d.display().to_string()),
            "tools": tools,
            "skipped": skipped_rows,
        }),
        None,
    )
}

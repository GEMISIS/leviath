//! The core of `install_tool`: compile a Rhai tool script and place it in the
//! global tools directory, where every future agent run discovers it.
//!
//! This is the persist path for mechanical learnings. A stage hook cannot touch
//! the filesystem by design, and a script tool's `write_file` is confined to the
//! run's workdir, so before this nothing an agent could do reached
//! `~/.leviath/tools/`. The function is deliberately pure over its inputs: the
//! destination directory, the reserved-name set and the symlink predicate are
//! all parameters, so the built-in tool, the MCP server and the tests call the
//! same code with nothing ambient.
//!
//! Every refusal happens before anything is written. A script that does not
//! compile, a name that collides with a built-in, a name that is not a plain
//! file stem: each is an `Err` naming what to change, and the directory every
//! agent executes from is left exactly as it was.

use std::path::{Path, PathBuf};

use leviath_scripting::ParamSpec;

/// What [`install_script_tool`] wrote, and what the model was told about it.
#[derive(Debug, Clone, PartialEq)]
pub struct InstalledTool {
    /// The tool's name: the `// @tool` directive, which is also the file stem.
    pub name: String,
    /// Where the script now lives.
    pub path: PathBuf,
    /// The script's `// @description`, possibly empty.
    pub description: String,
    /// The declared parameters, in declaration order.
    pub params: Vec<ParamSpec>,
    /// The platform capabilities the script declared with `// @requires`.
    pub requires: Vec<String>,
    /// The JSON Schema a stage will advertise for the tool's arguments.
    pub parameters_schema: serde_json::Value,
    /// Whether a previous script of the same name was overwritten.
    pub replaced: bool,
    /// Whether a `<name>.toml` sits beside the script. Discovery prefers the
    /// TOML's metadata over the script's annotations, which is worth knowing
    /// when the annotations were just written.
    pub toml_sibling: bool,
}

impl InstalledTool {
    /// The tool result a model reads back: what was installed, its interface,
    /// and when it becomes callable.
    pub fn summary(&self) -> String {
        let mut out = format!("Installed tool '{}' at {}.", self.name, self.path.display());
        if self.replaced {
            out.push_str(" It replaced the previous script of that name.");
        }
        if !self.description.is_empty() {
            out.push_str(&format!("\nDescription: {}", self.description));
        }
        out.push_str(&format!("\nParams: {}", params_summary(&self.params)));
        if !self.requires.is_empty() {
            out.push_str(&format!("\nRequires: {}", self.requires.join(", ")));
        }
        if self.toml_sibling {
            out.push_str(&format!(
                "\nNote: {}.toml sits beside the script and overrides its annotations.",
                self.name
            ));
        }
        out.push_str(
            "\nEvery agent on this machine can call it from its next spawn. A stage that sets \
             `available_global_tools = true` advertises it; a `dynamic_tools` agent already running \
             sees it on its next turn.",
        );
        out
    }
}

/// `name:type!` for a required parameter, `name:type` for an optional one, with
/// the description in parentheses when there is one. `none` when the tool takes
/// no parameters, so the line is never blank.
fn params_summary(params: &[ParamSpec]) -> String {
    if params.is_empty() {
        return "none".to_string();
    }
    params
        .iter()
        .map(|p| {
            let bang = if p.required { "!" } else { "" };
            if p.description.is_empty() {
                format!("{}:{}{bang}", p.name, p.ty)
            } else {
                format!("{}:{}{bang} ({})", p.name, p.ty, p.description)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Compile `source` as a Rhai tool named `name` and write it to
/// `<tools_dir>/<name>.rhai`.
///
/// `reserved` holds the names that would never be routed to a script (the
/// built-in and sub-agent tools; the caller adds MCP names when it knows them).
/// `overwrite` allows replacing an existing script of the same name.
///
/// Refuses, writing nothing, when: `tools_dir` is `None`; `name` is not a
/// single safe path component; `name` is reserved; the script fails to parse
/// its annotations or to compile; the script's `// @tool` differs from `name`;
/// a script already exists and `overwrite` is false; the destination resolves
/// outside `tools_dir` through a symlink; or the directory or file cannot be
/// written. The `Err` text is written for the model that has to fix the script.
pub fn install_script_tool(
    tools_dir: Option<&Path>,
    name: &str,
    source: &str,
    overwrite: bool,
    reserved: &[String],
) -> Result<InstalledTool, String> {
    install_script_tool_with(
        tools_dir,
        name,
        source,
        overwrite,
        reserved,
        leviath_core::resolves_within,
    )
}

/// [`install_script_tool`] with the symlink predicate injected.
///
/// A `fn` pointer rather than `impl Fn`, matching [`crate::resolve_within`]:
/// the refusal it guards needs a real symlink to reach, and creating one on
/// Windows takes a privilege CI runners do not have, so the predicate is a
/// parameter and the refusal is tested on every platform.
pub fn install_script_tool_with(
    tools_dir: Option<&Path>,
    name: &str,
    source: &str,
    overwrite: bool,
    reserved: &[String],
    within: fn(&Path, &Path) -> bool,
) -> Result<InstalledTool, String> {
    let Some(tools_dir) = tools_dir else {
        return Err(
            "no home directory resolves, so there is no global tools directory to install \
             into; set LEVIATH_HOME"
                .to_string(),
        );
    };
    if !leviath_core::is_safe_path_component(name) {
        return Err(format!(
            "tool name '{name}' must be a single path component: letters, digits, '.', '_' \
             or '-' only"
        ));
    }
    if reserved.iter().any(|r| r == name) {
        return Err(format!(
            "'{name}' is the name of a built-in tool; a script under that name would never be \
             called, so pick another"
        ));
    }
    let label = format!("{name}.rhai");
    let meta = leviath_scripting::tool::check_source(&label, source).map_err(|e| e.to_string())?;
    if meta.name != name {
        return Err(format!(
            "the script declares `// @tool {}` but the name given is '{name}'; they must agree, \
             because a tool is discovered by its @tool directive",
            meta.name
        ));
    }
    std::fs::create_dir_all(tools_dir)
        .map_err(|e| format!("cannot create {}: {e}", tools_dir.display()))?;
    let path = tools_dir.join(&label);
    if !within(&path, tools_dir) {
        return Err(format!(
            "refusing to write {}: it resolves outside {} through a symlink",
            path.display(),
            tools_dir.display()
        ));
    }
    let replaced = path.exists();
    if replaced && !overwrite {
        return Err(format!(
            "tool '{name}' already exists at {}; pass overwrite = true to replace it",
            path.display()
        ));
    }
    leviath_sys::write_private(&path, source.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(InstalledTool {
        name: meta.name.clone(),
        toml_sibling: path.with_extension("toml").exists(),
        path,
        description: meta.description.clone(),
        parameters_schema: meta.parameters_schema(),
        params: meta.params,
        requires: meta.required_caps,
        replaced,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const UPPER: &str = "// @tool upper\n// @description Upper-case text\n// @param text string required \"input to transform\"\nparams.text.to_upper()\n";

    fn reserved() -> Vec<String> {
        vec!["read_file".to_string(), "spawn_agent".to_string()]
    }

    #[test]
    fn installs_a_compiling_script_under_its_tool_name() {
        let home = tempfile::tempdir().unwrap();
        let tools = home.path().join("tools");
        let installed =
            install_script_tool(Some(&tools), "upper", UPPER, false, &reserved()).unwrap();
        assert_eq!(installed.name, "upper");
        assert_eq!(installed.path, tools.join("upper.rhai"));
        assert_eq!(std::fs::read_to_string(&installed.path).unwrap(), UPPER);
        assert_eq!(installed.description, "Upper-case text");
        assert_eq!(installed.params.len(), 1);
        assert!(installed.requires.is_empty());
        assert!(!installed.replaced);
        assert!(!installed.toml_sibling);
        assert_eq!(installed.parameters_schema["required"][0], "text");
        // Discovery finds it exactly as a spawn would.
        let (set, skipped) = leviath_scripting::ScriptToolSet::discover(&[tools]);
        assert!(skipped.is_empty());
        assert!(set.contains("upper"));
        let text = installed.summary();
        assert!(text.contains("Installed tool 'upper'"), "{text}");
        assert!(text.contains("text:string! (input to transform)"), "{text}");
        assert!(!text.contains("replaced"), "{text}");
        assert!(text.contains("available_global_tools"), "{text}");
    }

    #[test]
    fn refuses_without_a_tools_dir() {
        let err = install_script_tool(None, "upper", UPPER, false, &[]).unwrap_err();
        assert!(err.contains("LEVIATH_HOME"), "{err}");
    }

    #[test]
    fn refuses_a_name_that_is_not_a_plain_file_stem() {
        let home = tempfile::tempdir().unwrap();
        for bad in ["../upper", "a/b", "", "..", "sp ace"] {
            let err = install_script_tool(Some(home.path()), bad, UPPER, false, &[]).unwrap_err();
            assert!(err.contains("single path component"), "{bad}: {err}");
        }
        assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
    }

    #[test]
    fn refuses_a_reserved_name() {
        let home = tempfile::tempdir().unwrap();
        let src = UPPER.replace("@tool upper", "@tool read_file");
        let err = install_script_tool(Some(home.path()), "read_file", &src, false, &reserved())
            .unwrap_err();
        assert!(err.contains("built-in tool"), "{err}");
    }

    #[test]
    fn refuses_a_script_that_does_not_declare_a_tool() {
        let home = tempfile::tempdir().unwrap();
        let err =
            install_script_tool(Some(home.path()), "upper", "params.text", false, &[]).unwrap_err();
        assert!(err.contains("@tool"), "{err}");
        assert!(!home.path().join("upper.rhai").exists());
    }

    #[test]
    fn refuses_a_script_that_does_not_compile() {
        let home = tempfile::tempdir().unwrap();
        let src = "// @tool upper\nlet x = ;\n";
        let err = install_script_tool(Some(home.path()), "upper", src, false, &[]).unwrap_err();
        assert!(err.contains("compilation failed"), "{err}");
        assert!(err.contains("upper.rhai"), "{err}");
    }

    #[test]
    fn refuses_when_the_tool_directive_disagrees_with_the_name() {
        let home = tempfile::tempdir().unwrap();
        let err = install_script_tool(Some(home.path()), "lower", UPPER, false, &[]).unwrap_err();
        assert!(err.contains("`// @tool upper`"), "{err}");
        assert!(err.contains("'lower'"), "{err}");
    }

    #[test]
    fn refuses_to_replace_unless_asked_and_reports_the_replacement() {
        let home = tempfile::tempdir().unwrap();
        let tools = home.path().join("tools");
        install_script_tool(Some(&tools), "upper", UPPER, false, &[]).unwrap();
        let err = install_script_tool(Some(&tools), "upper", UPPER, false, &[]).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert!(err.contains("overwrite = true"), "{err}");

        let newer = UPPER.replace("Upper-case text", "Shout");
        let installed = install_script_tool(Some(&tools), "upper", &newer, true, &[]).unwrap();
        assert!(installed.replaced);
        assert_eq!(std::fs::read_to_string(&installed.path).unwrap(), newer);
        let text = installed.summary();
        assert!(text.contains("replaced the previous script"), "{text}");
        assert!(text.contains("Description: Shout"), "{text}");
    }

    #[test]
    fn reports_a_toml_sibling_that_will_override_the_annotations() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("upper.toml"), "[tool]\nname = \"upper\"\n").unwrap();
        let installed = install_script_tool(Some(home.path()), "upper", UPPER, false, &[]).unwrap();
        assert!(installed.toml_sibling);
        assert!(installed.summary().contains("upper.toml sits beside"));
    }

    #[test]
    fn fails_when_the_tools_dir_cannot_be_created() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("tools");
        std::fs::write(&file, "not a directory").unwrap();
        let err = install_script_tool(Some(&file), "upper", UPPER, false, &[]).unwrap_err();
        assert!(err.contains("cannot create"), "{err}");
    }

    #[test]
    fn fails_when_the_script_cannot_be_written() {
        let home = tempfile::tempdir().unwrap();
        // A directory where the file should go: exists, so overwrite is needed,
        // and then the write itself fails.
        std::fs::create_dir_all(home.path().join("upper.rhai")).unwrap();
        let err = install_script_tool(Some(home.path()), "upper", UPPER, true, &[]).unwrap_err();
        assert!(err.contains("cannot write"), "{err}");
    }

    #[test]
    fn refuses_a_destination_the_predicate_places_outside_the_tools_dir() {
        let home = tempfile::tempdir().unwrap();
        let err =
            install_script_tool_with(Some(home.path()), "upper", UPPER, false, &[], |_, _| false)
                .unwrap_err();
        assert!(err.contains("through a symlink"), "{err}");
        assert!(!home.path().join("upper.rhai").exists());
    }

    /// The real predicate, against a real symlink: the injected-predicate test
    /// above proves the refusal arm, this proves the predicate fires for the
    /// case an agent could arrange for itself.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_script_pointing_outside_is_refused_for_real() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let victim = elsewhere.path().join("victim.rhai");
        std::fs::write(&victim, "// untouched\n").unwrap();
        std::os::unix::fs::symlink(&victim, home.path().join("upper.rhai")).unwrap();
        let err = install_script_tool(Some(home.path()), "upper", UPPER, true, &[]).unwrap_err();
        assert!(err.contains("through a symlink"), "{err}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "// untouched\n");
    }

    #[test]
    fn summary_covers_tools_with_requirements_and_no_parameters() {
        let installed = InstalledTool {
            name: "ls".to_string(),
            path: PathBuf::from("/x/ls.rhai"),
            description: String::new(),
            params: vec![ParamSpec {
                name: "flags".to_string(),
                ty: "string".to_string(),
                required: false,
                description: String::new(),
                schema: None,
            }],
            requires: vec!["shell".to_string()],
            parameters_schema: serde_json::json!({}),
            replaced: false,
            toml_sibling: false,
        };
        let text = installed.summary();
        assert!(text.contains("Params: flags:string\n"), "{text}");
        assert!(text.contains("Requires: shell"), "{text}");
        assert!(!text.contains("Description:"), "{text}");
        assert_eq!(params_summary(&[]), "none");
    }
}

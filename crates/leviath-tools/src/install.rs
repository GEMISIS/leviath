//! The core of `install_tool`: compile a Rhai tool script and place it in the
//! global tools directory, where every future agent run discovers it.
//!
//! This is the persist path for mechanical learnings. A stage hook cannot touch
//! the filesystem by design, and a script tool's `write_file` is confined to the
//! run's workdir, so before this nothing an agent could do reached
//! `~/.leviath/tools/`. The function is deliberately pure over its inputs: the
//! destination directory, the reserved-name set, the provenance and the
//! filesystem predicates are all parameters, so the built-in tool, the MCP
//! server and the tests call the same code with nothing ambient.
//!
//! Every refusal happens before anything is written. A script that does not
//! compile, a name that collides with a built-in, a name that is not a plain
//! file stem: each is an `Err` naming what to change, and the directory every
//! agent executes from is left exactly as it was.
//!
//! What lands on disk carries its origin. When the caller says who is
//! installing, the file starts with one `// installed by leviath: ...` line, so
//! `lev tools` and `cat` show where a model-authored tool came from. A plain
//! `//` comment without `@` is invisible to the annotation parser and to the
//! compiler, and it is the final text, comment included, that is compiled.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use leviath_scripting::ParamSpec;
use serde_json::Value;

use crate::{BuiltinTools, SUBAGENT_TOOLS};

/// The largest script `install_script_tool` accepts, in bytes.
///
/// Every `.rhai` in the tools directory is read and compiled at every spawn of
/// every agent on the machine, and again on each dynamic refresh. A cap here is
/// what keeps one oversized install from taxing every later run.
pub const MAX_TOOL_SOURCE_BYTES: usize = 256 * 1024;

/// What [`install_script_tool`] wrote, and what the model was told about it.
#[derive(Debug, Clone, PartialEq)]
pub struct InstalledTool {
    /// The tool's name: the `// @tool` directive, which is also the file stem.
    pub name: String,
    /// Where the script now lives.
    pub path: PathBuf,
    /// The script's `// @description`.
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

/// The text that is actually written: `source` behind one provenance line when
/// the caller gave one, `source` unchanged otherwise.
///
/// Seconds since the Unix epoch rather than a formatted date: it needs no
/// dependency, sorts as text, and any shell turns it back into a date.
fn stamped_source(source: &str, provenance: Option<&str>) -> String {
    match provenance {
        Some(who) => {
            let at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default();
            format!("// installed by leviath: {who} at {at}\n{source}")
        }
        None => source.to_string(),
    }
}

/// The `[tool] name` a sibling TOML would install the script under, when the
/// sibling exists and declares one. Anything else (no file, unreadable, not
/// TOML, no name) is `None`: discovery will complain about a broken sibling on
/// its own, and only a *disagreeing* name is this function's concern.
fn toml_sibling_name(toml_path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(toml_path).ok()?;
    let doc: toml::Value = toml::from_str(&text).ok()?;
    doc.get("tool")?.get("name")?.as_str().map(str::to_string)
}

/// Whether `path` itself is a symlink, without following it.
///
/// Writing through a link that points inside the tools directory would rewrite
/// whichever script it points at under the wrong name, and the containment
/// predicate cannot see that because the target is within bounds.
fn destination_is_symlink(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
}

/// Compile `source` as a Rhai tool named `name` and write it to
/// `<tools_dir>/<name>.rhai`.
///
/// `reserved` holds the names that would never be routed to a script (the
/// built-in and sub-agent tools; the caller adds MCP names when it knows them).
/// `overwrite` allows replacing an existing script of the same name.
/// `provenance`, when given, is written into the file as a leading
/// `// installed by leviath: <provenance> at <unix seconds>` line, so the
/// origin of a model-authored tool is visible to anyone who lists or reads it.
///
/// Refuses, writing nothing, when: `tools_dir` is `None`; `source` exceeds
/// [`MAX_TOOL_SOURCE_BYTES`]; `name` is not a single safe path component;
/// `name` is reserved; the script fails to parse its annotations or to
/// compile; the script's `// @tool` differs from `name`; the script has no
/// `// @description`; a script already exists and `overwrite` is false; the
/// destination is a symlink or resolves outside `tools_dir`; a `<name>.toml`
/// sibling declares a different `[tool] name`; or the directory or file cannot
/// be written. The `Err` text is written for the model that has to fix the
/// script.
pub fn install_script_tool(
    tools_dir: Option<&Path>,
    name: &str,
    source: &str,
    overwrite: bool,
    reserved: &[String],
    provenance: Option<&str>,
) -> Result<InstalledTool, String> {
    install_script_tool_with(
        tools_dir,
        name,
        source,
        overwrite,
        reserved,
        provenance,
        InstallProbes::real(),
    )
}

/// The filesystem questions the install asks before writing, as `fn`
/// pointers rather than `impl Fn`, matching [`crate::resolve_within`]: the
/// refusals they guard need a real symlink to reach, and creating one on
/// Windows takes a privilege CI runners do not have, so both predicates are
/// injectable and both refusals are tested on every platform.
#[derive(Debug, Clone, Copy)]
pub struct InstallProbes {
    /// Whether the destination resolves inside the tools directory once every
    /// symlink on the way is followed. [`leviath_core::resolves_within`] in
    /// production.
    pub within: fn(&Path, &Path) -> bool,
    /// Whether the destination itself is a symlink, without following it.
    pub is_symlink: fn(&Path) -> bool,
}

impl InstallProbes {
    /// The probes that look at the real filesystem.
    pub fn real() -> Self {
        Self {
            within: leviath_core::resolves_within,
            is_symlink: destination_is_symlink,
        }
    }
}

/// [`install_script_tool`] with the filesystem predicates injected.
pub fn install_script_tool_with(
    tools_dir: Option<&Path>,
    name: &str,
    source: &str,
    overwrite: bool,
    reserved: &[String],
    provenance: Option<&str>,
    probes: InstallProbes,
) -> Result<InstalledTool, String> {
    let Some(tools_dir) = tools_dir else {
        return Err(
            "no home directory resolves, so there is no global tools directory to install \
             into; set LEVIATH_HOME"
                .to_string(),
        );
    };
    if source.len() > MAX_TOOL_SOURCE_BYTES {
        return Err(format!(
            "the script is {} bytes; a tool script may be at most {MAX_TOOL_SOURCE_BYTES} bytes, \
             because every agent on this machine compiles it at every spawn",
            source.len()
        ));
    }
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
    // Compile what will be written, provenance line included, so the file on
    // disk is exactly the text that passed.
    let text = stamped_source(source, provenance);
    let meta = leviath_scripting::tool::check_source(&label, &text).map_err(|e| e.to_string())?;
    if meta.name != name {
        return Err(format!(
            "the script declares `// @tool {}` but the name given is '{name}'; they must agree, \
             because a tool is discovered by its @tool directive",
            meta.name
        ));
    }
    if meta.description.trim().is_empty() {
        return Err(
            "the script has no `// @description`; a tool the model cannot tell apart from the \
             others will never be called, so say what it does"
                .to_string(),
        );
    }
    std::fs::create_dir_all(tools_dir)
        .map_err(|e| format!("cannot create {}: {e}", tools_dir.display()))?;
    let path = tools_dir.join(&label);
    if !(probes.within)(&path, tools_dir) {
        return Err(format!(
            "refusing to write {}: it resolves outside {} through a symlink",
            path.display(),
            tools_dir.display()
        ));
    }
    if (probes.is_symlink)(&path) {
        return Err(format!(
            "refusing to write {}: it is a symlink, and writing through it would change \
             whatever it points at",
            path.display()
        ));
    }
    let replaced = path.exists();
    if replaced && !overwrite {
        return Err(format!(
            "tool '{name}' already exists at {}; pass overwrite = true to replace it",
            path.display()
        ));
    }
    let toml_path = path.with_extension("toml");
    if let Some(other) = toml_sibling_name(&toml_path)
        && other != name
    {
        return Err(format!(
            "{} sits beside the script and declares `[tool] name = \"{other}\"`; discovery \
             prefers the TOML, so the tool would be installed under that name instead of \
             '{name}'; remove or fix the TOML first",
            toml_path.display()
        ));
    }
    leviath_sys::write_private(&path, text.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(InstalledTool {
        name: meta.name.clone(),
        toml_sibling: toml_path.exists(),
        path,
        description: meta.description.clone(),
        parameters_schema: meta.parameters_schema(),
        params: meta.params,
        requires: meta.required_caps,
        replaced,
    })
}

impl BuiltinTools {
    /// The `install_tool` built-in: compile the `source` argument as a Rhai
    /// tool named `name` and install it into the global tools directory.
    ///
    /// Synchronous, like the environment tools, so a seed or a host with no
    /// runtime can call it. It takes no per-path lock: fan-out workers that
    /// install the same name concurrently are last-writer-wins through an
    /// atomic rename, which is acceptable for what should be identical
    /// learnings and never leaves a half-written script.
    ///
    /// Reserved names are everything discovery would drop a script for: this
    /// platform's built-ins, the sub-agent tools, and whatever the spawn put on
    /// the context (the MCP tools this run offers).
    pub(crate) fn install_tool(&self, args: &Value) -> String {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return "[error] install_tool: missing 'name' argument".to_string();
        };
        let Some(source) = args.get("source").and_then(Value::as_str) else {
            return "[error] install_tool: missing 'source' argument".to_string();
        };
        let overwrite = args
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut reserved = self.names();
        reserved.extend(SUBAGENT_TOOLS.iter().map(|s| s.to_string()));
        reserved.extend(self.ctx.reserved_names.iter().cloned());
        let provenance = format!("agent run in {}", self.ctx.workdir.display());
        match install_script_tool(
            self.ctx.tools_dir.as_deref(),
            name,
            source,
            overwrite,
            &reserved,
            Some(&provenance),
        ) {
            Ok(installed) => installed.summary(),
            Err(e) => format!("[error] install_tool: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolContext;
    use serde_json::json;

    const UPPER: &str = "// @tool upper\n// @description Upper-case text\n// @param text string required \"input to transform\"\nparams.text.to_upper()\n";

    fn reserved() -> Vec<String> {
        vec!["read_file".to_string(), "spawn_agent".to_string()]
    }

    /// The plain install: no provenance, the file is the source verbatim.
    fn install(
        tools_dir: Option<&Path>,
        name: &str,
        source: &str,
        overwrite: bool,
        reserved: &[String],
    ) -> Result<InstalledTool, String> {
        install_script_tool(tools_dir, name, source, overwrite, reserved, None)
    }

    #[test]
    fn installs_a_compiling_script_under_its_tool_name() {
        let home = tempfile::tempdir().unwrap();
        let tools = home.path().join("tools");
        let installed = install(Some(&tools), "upper", UPPER, false, &reserved()).unwrap();
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

    /// The provenance line is the first line of the file, the annotation
    /// parser steps over it, and discovery compiles the stamped file exactly
    /// as the install did.
    #[test]
    fn a_provenance_line_leads_the_file_and_survives_discovery() {
        let home = tempfile::tempdir().unwrap();
        let tools = home.path().join("tools");
        let installed = install_script_tool(
            Some(&tools),
            "upper",
            UPPER,
            false,
            &[],
            Some("agent run in /work/repo"),
        )
        .unwrap();
        let text = std::fs::read_to_string(&installed.path).unwrap();
        let first = text.lines().next().unwrap();
        assert!(
            first.starts_with("// installed by leviath: agent run in /work/repo at "),
            "{first}"
        );
        let secs: u64 = first.rsplit(' ').next().unwrap().parse().unwrap();
        assert!(secs > 1_600_000_000, "a real clock reading: {secs}");
        assert!(text.ends_with(UPPER), "the source follows unchanged");
        assert_eq!(installed.description, "Upper-case text");
        let (set, skipped) = leviath_scripting::ScriptToolSet::discover(&[tools]);
        assert!(skipped.is_empty(), "{skipped:?}");
        let meta = &set.get("upper").unwrap().meta;
        assert_eq!(meta.description, "Upper-case text");
        assert_eq!(meta.params.len(), 1);
    }

    #[test]
    fn refuses_without_a_tools_dir() {
        let err = install(None, "upper", UPPER, false, &[]).unwrap_err();
        assert!(err.contains("LEVIATH_HOME"), "{err}");
    }

    /// The cap is checked before anything else looks at the text, so an
    /// oversized script is refused without being compiled.
    #[test]
    fn refuses_a_script_over_the_size_cap_before_compiling_it() {
        let home = tempfile::tempdir().unwrap();
        let big = format!("{UPPER}// {}\n", "x".repeat(MAX_TOOL_SOURCE_BYTES));
        let err = install(Some(home.path()), "upper", &big, false, &[]).unwrap_err();
        assert!(err.contains("at most"), "{err}");
        assert!(err.contains(&MAX_TOOL_SOURCE_BYTES.to_string()), "{err}");
        assert!(!home.path().join("upper.rhai").exists());
        // Exactly at the cap is fine.
        let padding = MAX_TOOL_SOURCE_BYTES - UPPER.len() - "// \n".len();
        let exact = format!("{UPPER}// {}\n", "x".repeat(padding));
        assert_eq!(exact.len(), MAX_TOOL_SOURCE_BYTES);
        install(Some(home.path()), "upper", &exact, false, &[]).unwrap();
    }

    #[test]
    fn refuses_a_name_that_is_not_a_plain_file_stem() {
        let home = tempfile::tempdir().unwrap();
        for bad in ["../upper", "a/b", "", "..", "sp ace"] {
            let err = install(Some(home.path()), bad, UPPER, false, &[]).unwrap_err();
            assert!(err.contains("single path component"), "{bad}: {err}");
        }
        assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
    }

    #[test]
    fn refuses_a_reserved_name() {
        let home = tempfile::tempdir().unwrap();
        let src = UPPER.replace("@tool upper", "@tool read_file");
        let err = install(Some(home.path()), "read_file", &src, false, &reserved()).unwrap_err();
        assert!(err.contains("built-in tool"), "{err}");
    }

    #[test]
    fn refuses_a_script_that_does_not_declare_a_tool() {
        let home = tempfile::tempdir().unwrap();
        let err = install(Some(home.path()), "upper", "params.text", false, &[]).unwrap_err();
        assert!(err.contains("@tool"), "{err}");
        assert!(!home.path().join("upper.rhai").exists());
    }

    #[test]
    fn refuses_a_script_that_does_not_compile() {
        let home = tempfile::tempdir().unwrap();
        let src = "// @tool upper\n// @description d\nlet x = ;\n";
        let err = install(Some(home.path()), "upper", src, false, &[]).unwrap_err();
        assert!(err.contains("compilation failed"), "{err}");
        assert!(err.contains("upper.rhai"), "{err}");
    }

    #[test]
    fn refuses_when_the_tool_directive_disagrees_with_the_name() {
        let home = tempfile::tempdir().unwrap();
        let err = install(Some(home.path()), "lower", UPPER, false, &[]).unwrap_err();
        assert!(err.contains("`// @tool upper`"), "{err}");
        assert!(err.contains("'lower'"), "{err}");
    }

    /// A missing directive and a blank one are the same refusal: neither
    /// gives the model anything to choose the tool by.
    #[test]
    fn refuses_a_script_without_a_description() {
        let home = tempfile::tempdir().unwrap();
        let missing = "// @tool upper\nparams.text.to_upper()\n";
        let blank = "// @tool upper\n// @description   \nparams.text.to_upper()\n";
        for src in [missing, blank] {
            let err = install(Some(home.path()), "upper", src, false, &[]).unwrap_err();
            assert!(err.contains("@description"), "{err}");
            assert!(err.contains("never be called"), "{err}");
        }
        assert!(!home.path().join("upper.rhai").exists());
    }

    #[test]
    fn refuses_to_replace_unless_asked_and_reports_the_replacement() {
        let home = tempfile::tempdir().unwrap();
        let tools = home.path().join("tools");
        install(Some(&tools), "upper", UPPER, false, &[]).unwrap();
        let err = install(Some(&tools), "upper", UPPER, false, &[]).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert!(err.contains("overwrite = true"), "{err}");

        let newer = UPPER.replace("Upper-case text", "Shout");
        let installed = install(Some(&tools), "upper", &newer, true, &[]).unwrap();
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
        let installed = install(Some(home.path()), "upper", UPPER, false, &[]).unwrap();
        assert!(installed.toml_sibling);
        assert!(installed.summary().contains("upper.toml sits beside"));
    }

    /// A sibling that would install the script under another name is refused:
    /// the model was told it installed `upper`, and discovery would offer
    /// something else.
    #[test]
    fn refuses_a_toml_sibling_that_names_a_different_tool() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("upper.toml"), "[tool]\nname = \"shout\"\n").unwrap();
        let err = install(Some(home.path()), "upper", UPPER, false, &[]).unwrap_err();
        assert!(err.contains("upper.toml"), "{err}");
        assert!(err.contains("`[tool] name = \"shout\"`"), "{err}");
        assert!(!home.path().join("upper.rhai").exists());
    }

    /// A sibling that is not TOML, or has no `[tool] name`, is discovery's
    /// problem to report; the install goes ahead and notes the sibling.
    #[test]
    fn a_toml_sibling_without_a_name_is_noted_but_not_a_refusal() {
        for text in [
            "not = [toml",
            "[tool]\ndescription = \"d\"\n",
            "[other]\nname = \"x\"\n",
        ] {
            let home = tempfile::tempdir().unwrap();
            std::fs::write(home.path().join("upper.toml"), text).unwrap();
            let installed = install(Some(home.path()), "upper", UPPER, false, &[]).unwrap();
            assert!(installed.toml_sibling, "{text}");
        }
        // No sibling at all reads as no name.
        assert_eq!(
            toml_sibling_name(Path::new("/nonexistent/upper.toml")),
            None
        );
    }

    #[test]
    fn fails_when_the_tools_dir_cannot_be_created() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("tools");
        std::fs::write(&file, "not a directory").unwrap();
        let err = install(Some(&file), "upper", UPPER, false, &[]).unwrap_err();
        assert!(err.contains("cannot create"), "{err}");
    }

    #[test]
    fn fails_when_the_script_cannot_be_written() {
        let home = tempfile::tempdir().unwrap();
        // A directory where the file should go: exists, so overwrite is needed,
        // and then the write itself fails.
        std::fs::create_dir_all(home.path().join("upper.rhai")).unwrap();
        let err = install(Some(home.path()), "upper", UPPER, true, &[]).unwrap_err();
        assert!(err.contains("cannot write"), "{err}");
    }

    #[test]
    fn refuses_a_destination_the_predicate_places_outside_the_tools_dir() {
        let home = tempfile::tempdir().unwrap();
        let err = install_script_tool_with(
            Some(home.path()),
            "upper",
            UPPER,
            false,
            &[],
            None,
            InstallProbes {
                within: |_, _| false,
                is_symlink: |_| false,
            },
        )
        .unwrap_err();
        assert!(err.contains("through a symlink"), "{err}");
        assert!(!home.path().join("upper.rhai").exists());
    }

    /// The injected predicate reaches the refusal on every platform; the real
    /// predicate is exercised on its own below.
    #[test]
    fn refuses_a_destination_the_predicate_says_is_a_symlink() {
        let home = tempfile::tempdir().unwrap();
        let err = install_script_tool_with(
            Some(home.path()),
            "upper",
            UPPER,
            true,
            &[],
            None,
            InstallProbes {
                within: |_, _| true,
                is_symlink: |_| true,
            },
        )
        .unwrap_err();
        assert!(err.contains("it is a symlink"), "{err}");
        assert!(!home.path().join("upper.rhai").exists());
    }

    #[test]
    fn the_real_symlink_predicate_reads_a_plain_file_and_a_missing_one_as_not_links() {
        let home = tempfile::tempdir().unwrap();
        let plain = home.path().join("upper.rhai");
        std::fs::write(&plain, UPPER).unwrap();
        assert!(!destination_is_symlink(&plain));
        assert!(!destination_is_symlink(&home.path().join("absent.rhai")));
    }

    /// The real predicates, against real symlinks: the injected-predicate
    /// tests above prove the refusal arms, these prove the predicates fire for
    /// the cases an agent could arrange for itself. A link pointing outside
    /// the tools directory and a link pointing at a sibling script inside it
    /// are both refused, and neither target is touched.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_destination_is_refused_for_real() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let victim = elsewhere.path().join("victim.rhai");
        std::fs::write(&victim, "// untouched\n").unwrap();
        std::os::unix::fs::symlink(&victim, home.path().join("upper.rhai")).unwrap();
        let err = install(Some(home.path()), "upper", UPPER, true, &[]).unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "// untouched\n");

        let sibling = home.path().join("other.rhai");
        std::fs::write(&sibling, "// also untouched\n").unwrap();
        std::os::unix::fs::symlink(&sibling, home.path().join("lower.rhai")).unwrap();
        let lower = UPPER.replace("@tool upper", "@tool lower");
        let err = install(Some(home.path()), "lower", &lower, true, &[]).unwrap_err();
        assert!(err.contains("it is a symlink"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&sibling).unwrap(),
            "// also untouched\n"
        );
        assert!(destination_is_symlink(&home.path().join("lower.rhai")));
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

    // ── The built-in over the core ────────────────────────────────────────

    /// Built-ins over `workdir` that install into `tools`, with the names a
    /// spawn would reserve on top of the built-ins.
    fn tools_installing_into(workdir: &Path, tools: &Path) -> BuiltinTools {
        BuiltinTools::new(
            ToolContext::new(workdir.to_path_buf())
                .with_tools_dir(Some(tools.to_path_buf()))
                .with_reserved_names(vec!["acme_search".to_string()]),
        )
    }

    /// Through the public `execute`, so the dispatch arm is what is tested:
    /// the file lands in the context's tools directory, stamped with the run's
    /// workdir as its provenance.
    #[tokio::test]
    async fn the_builtin_installs_into_the_context_tools_dir_with_provenance() {
        let workdir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let tools = home.path().join("tools");
        let builtins = tools_installing_into(workdir.path(), &tools);
        let out = builtins
            .execute("install_tool", json!({"name": "upper", "source": UPPER}))
            .await;
        assert!(out.starts_with("Installed tool 'upper'"), "{out}");
        let text = std::fs::read_to_string(tools.join("upper.rhai")).unwrap();
        let expected = format!(
            "// installed by leviath: agent run in {} at ",
            builtins.workdir().display()
        );
        assert!(text.starts_with(&expected), "{text}");
        assert!(text.ends_with(UPPER), "{text}");

        // Without overwrite the second install is refused; with it, replaced.
        let out = builtins
            .execute("install_tool", json!({"name": "upper", "source": UPPER}))
            .await;
        assert!(
            out.starts_with("[error] install_tool: tool 'upper' already exists"),
            "{out}"
        );
        let out = builtins
            .execute(
                "install_tool",
                json!({"name": "upper", "source": UPPER, "overwrite": true}),
            )
            .await;
        assert!(out.contains("replaced the previous script"), "{out}");
    }

    #[test]
    fn the_builtin_refuses_missing_arguments() {
        let workdir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let builtins = tools_installing_into(workdir.path(), home.path());
        assert_eq!(
            builtins.install_tool(&json!({"source": UPPER})),
            "[error] install_tool: missing 'name' argument"
        );
        assert_eq!(
            builtins.install_tool(&json!({"name": "upper"})),
            "[error] install_tool: missing 'source' argument"
        );
        assert_eq!(
            builtins.install_tool(&json!({"name": 3, "source": UPPER})),
            "[error] install_tool: missing 'name' argument"
        );
        assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
    }

    /// The reserved set is the union a spawn's discovery would drop for: this
    /// platform's built-ins (and their aliases), the sub-agent tools, and the
    /// names the context carries for the run's MCP tools.
    #[test]
    fn the_builtin_reserves_builtin_subagent_and_context_names() {
        let workdir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let builtins = tools_installing_into(workdir.path(), home.path());
        for taken in ["read_file", "bash", "spawn_agent", "acme_search"] {
            let src = UPPER.replace("@tool upper", &format!("@tool {taken}"));
            let out = builtins.install_tool(&json!({"name": taken, "source": src}));
            assert!(out.contains("built-in tool"), "{taken}: {out}");
        }
        assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
    }

    /// The spawn learns the reserved set from the constructed built-ins, so the
    /// names can also arrive after construction; they replace what the context
    /// carried.
    #[test]
    fn the_reserved_names_can_be_set_after_construction() {
        let workdir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let builtins = tools_installing_into(workdir.path(), home.path())
            .with_reserved_names(vec!["late_mcp".to_string()]);
        let src = UPPER.replace("@tool upper", "@tool late_mcp");
        let out = builtins.install_tool(&json!({"name": "late_mcp", "source": src}));
        assert!(
            out.contains("'late_mcp' is the name of a built-in tool"),
            "{out}"
        );
        // The context's own list was replaced, so `acme_search` is free again.
        let src = UPPER.replace("@tool upper", "@tool acme_search");
        let out = builtins.install_tool(&json!({"name": "acme_search", "source": src}));
        assert!(out.starts_with("Installed tool 'acme_search'"), "{out}");
    }

    #[test]
    fn the_builtin_reports_a_missing_tools_dir() {
        let workdir = tempfile::tempdir().unwrap();
        let builtins =
            BuiltinTools::new(ToolContext::new(workdir.path().to_path_buf()).with_tools_dir(None));
        let out = builtins.install_tool(&json!({"name": "upper", "source": UPPER}));
        assert!(
            out.starts_with("[error] install_tool: no home directory"),
            "{out}"
        );
    }
}

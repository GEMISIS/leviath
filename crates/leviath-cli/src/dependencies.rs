//! Checking a graph's declared `dependencies` against this machine.
//!
//! A graph says what it needs (see [`DependencyDef`]);
//! this module answers whether the machine has it. The same evaluator is used
//! two ways: the spawn gate fails a run before the first billed inference if a
//! required dependency is missing, and `lev deps` reports the same findings
//! (and, separately and explicitly, installs).
//!
//! Nothing here installs or changes anything - a check only reads. The
//! env-and-PATH reads go through a [`Probe`] so the evaluator is testable
//! without depending on the host's environment; [`SystemProbe`] is the real one.

use std::ffi::OsString;
use std::path::Path;

use leviath_mcp::MCPServerConfig;
use leviath_runtime::spec::graph::{CodeRef, DependencyDef, Needs};

/// A dependency's kind, as a status line and `lev deps` name it.
pub(crate) fn kind_tag(needs: &Needs) -> &'static str {
    match needs {
        Needs::McpServer { .. } => "mcp_server",
        Needs::Env(_) => "env",
        Needs::Binary(_) => "binary",
        Needs::Check(_) => "script",
    }
}

/// The machine facts a check reads: environment variables and what is on
/// `PATH`. Injected so the evaluator can be tested against a fake machine.
pub trait Probe {
    /// The value of an environment variable, or `None` if it is unset.
    fn env(&self, name: &str) -> Option<String>;
    /// Whether a program of this name resolves on `PATH`.
    fn which(&self, name: &str) -> bool;
}

/// The real machine.
pub struct SystemProbe;

impl Probe for SystemProbe {
    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
    fn which(&self, name: &str) -> bool {
        which_in(std::env::var_os("PATH"), name)
    }
}

/// Whether `name` (or `name.exe`) is a file in any directory of `path`.
///
/// The `.exe` suffix is checked on every OS: it costs a stat that misses on
/// Unix and spares a `#[cfg(windows)]` split. This is presence, not an
/// executable-bit check - enough to tell "installed" from "not".
fn which_in(path: Option<OsString>, name: &str) -> bool {
    let Some(path) = path else {
        return false;
    };
    std::env::split_paths(&path)
        .any(|dir| dir.join(name).is_file() || dir.join(format!("{name}.exe")).is_file())
}

/// Whether a dependency is in place, and why not when it is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyState {
    /// The dependency is present.
    Satisfied,
    /// It is missing; the string is the remedy shown to the user.
    Unmet(String),
    /// The check itself could not run (a broken script, an unreadable file);
    /// the string is the reason.
    Unusable(String),
}

impl DependencyState {
    /// Whether this counts as "in place".
    pub fn is_satisfied(&self) -> bool {
        matches!(self, DependencyState::Satisfied)
    }

    /// The remedy or reason to show, empty when satisfied.
    pub fn detail(&self) -> &str {
        match self {
            DependencyState::Satisfied => "",
            DependencyState::Unmet(remedy) => remedy,
            DependencyState::Unusable(reason) => reason,
        }
    }
}

/// One dependency's evaluated status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyStatus {
    /// The dependency's name.
    pub name: String,
    /// Its kind tag (`mcp_server`, `env`, `binary`, `script`).
    pub kind: &'static str,
    /// Whether an unmet result blocks the run.
    pub required: bool,
    /// What the check found.
    pub state: DependencyState,
}

/// Whether `var` is set to a non-empty value, via `probe`.
///
/// Only presence is examined; the value never travels further than this
/// `bool`, so a secret is never returned or logged by the code that checks
/// for it.
pub(crate) fn env_is_set(probe: &dyn Probe, var: &str) -> bool {
    probe.env(var).filter(|v| !v.trim().is_empty()).is_some()
}

impl DependencyStatus {
    /// A one-line human status, shared by `lev deps check` and `lev validate`:
    /// `[ok  ] name (kind)`, or `[MISS] name (kind) - remedy`.
    pub fn line(&self) -> String {
        let mark = match &self.state {
            DependencyState::Satisfied => "ok  ",
            DependencyState::Unmet(_) => "MISS",
            DependencyState::Unusable(_) => "ERR ",
        };
        let req = if self.required { "" } else { " (optional)" };
        let detail = self.state.detail();
        if detail.is_empty() {
            format!("[{mark}] {} ({}){req}", self.name, self.kind)
        } else {
            format!("[{mark}] {} ({}){req} - {detail}", self.name, self.kind)
        }
    }
}

/// The evaluation of a blueprint's whole dependency list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyReport {
    /// One entry per declared dependency, in declaration order.
    pub statuses: Vec<DependencyStatus>,
}

impl DependencyReport {
    /// The required dependencies that are not satisfied - what blocks a run.
    pub fn blocking(&self) -> Vec<&DependencyStatus> {
        self.statuses
            .iter()
            .filter(|s| s.required && !s.state.is_satisfied())
            .collect()
    }

    /// A spawn-blocking message when a required dependency is missing, or
    /// `None` when the run may proceed.
    pub fn blocking_message(&self) -> Option<String> {
        let blocking = self.blocking();
        if blocking.is_empty() {
            return None;
        }
        let mut msg = String::from("this agent's dependencies are not satisfied:");
        for s in blocking {
            msg.push_str(&format!(
                "\n  - {} ({}): {}",
                s.name,
                s.kind,
                s.state.detail()
            ));
        }
        msg.push_str("\n\nRun `lev deps check <agent>` to see them, or `lev deps install <agent>` to set them up.");
        Some(msg)
    }
}

/// Evaluate every dependency of a blueprint against this machine.
///
/// `servers` is the operator's configured MCP servers, `blueprint_dir` the
/// directory holding the blueprint (a check named by file is read from it,
/// and never from outside it), and `probe` reads env and `PATH`.
pub fn evaluate(
    deps: &[DependencyDef],
    servers: &[MCPServerConfig],
    blueprint_dir: &Path,
    probe: &dyn Probe,
) -> DependencyReport {
    let statuses = deps
        .iter()
        .map(|dep| DependencyStatus {
            name: dep.name.clone(),
            kind: kind_tag(&dep.needs),
            required: dep.required,
            state: evaluate_one(dep, servers, blueprint_dir, probe),
        })
        .collect();
    DependencyReport { statuses }
}

/// A dependency's own remedy, or a sensible default sentence.
fn remedy_or(dep: &DependencyDef, default: String) -> String {
    dep.remedy.clone().unwrap_or(default)
}

fn evaluate_one(
    dep: &DependencyDef,
    servers: &[MCPServerConfig],
    blueprint_dir: &Path,
    probe: &dyn Probe,
) -> DependencyState {
    match &dep.needs {
        Needs::McpServer { server, env } => {
            if !servers.iter().any(|s| s.name == server.as_str()) {
                return DependencyState::Unmet(remedy_or(
                    dep,
                    format!("configure the MCP server '{server}' in your config"),
                ));
            }
            for var in env {
                if !env_is_set(probe, var) {
                    return DependencyState::Unmet(remedy_or(
                        dep,
                        format!("set {var} (the '{server}' server needs it)"),
                    ));
                }
            }
            DependencyState::Satisfied
        }
        Needs::Env(var) => {
            if env_is_set(probe, var) {
                DependencyState::Satisfied
            } else {
                DependencyState::Unmet(remedy_or(
                    dep,
                    format!("set the environment variable {var}"),
                ))
            }
        }
        Needs::Binary(command) => {
            if probe.which(command) {
                DependencyState::Satisfied
            } else {
                DependencyState::Unmet(remedy_or(
                    dep,
                    format!("install '{command}' and put it on your PATH"),
                ))
            }
        }
        Needs::Check(check) => evaluate_script(check, blueprint_dir),
    }
}

/// What a check is called in a message: its file, or `inline check`.
pub(crate) fn code_label(code: &CodeRef) -> &str {
    match code {
        CodeRef::File(path) => path,
        CodeRef::Inline(_) => "inline check",
    }
}

/// Compile and run a dependency's check. A check named by file is read from
/// the blueprint's directory, and never from outside it.
fn evaluate_script(check: &CodeRef, blueprint_dir: &Path) -> DependencyState {
    let label = code_label(check);
    let source = match leviath_runtime::bind::host::read_code(check, Some(blueprint_dir)) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => return DependencyState::Unusable(format!("check script: {e}")),
    };
    let compiled = match leviath_scripting::dependency_check::compile(label, &source) {
        Ok(c) => c,
        Err(e) => return DependencyState::Unusable(e.to_string()),
    };
    match leviath_scripting::dependency_check::run(&compiled) {
        leviath_scripting::dependency_check::Verdict::Satisfied => DependencyState::Satisfied,
        leviath_scripting::dependency_check::Verdict::Unmet(remedy) => {
            DependencyState::Unmet(remedy)
        }
        leviath_scripting::dependency_check::Verdict::Unusable(reason) => {
            DependencyState::Unusable(reason)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_runtime::spec::names::McpServerName;
    use std::collections::HashMap;

    struct FakeProbe {
        env: HashMap<String, String>,
        bins: Vec<String>,
    }
    impl Probe for FakeProbe {
        fn env(&self, name: &str) -> Option<String> {
            self.env.get(name).cloned()
        }
        fn which(&self, name: &str) -> bool {
            self.bins.iter().any(|b| b == name)
        }
    }

    fn dep(name: &str, needs: Needs) -> DependencyDef {
        DependencyDef {
            name: name.to_string(),
            needs,
            required: true,
            remedy: None,
            description: None,
            install: None,
        }
    }

    fn server(name: &str) -> MCPServerConfig {
        MCPServerConfig {
            name: name.to_string(),
            ..Default::default()
        }
    }

    fn eval_one(
        dep: &DependencyDef,
        servers: &[MCPServerConfig],
        probe: &dyn Probe,
    ) -> DependencyState {
        evaluate(std::slice::from_ref(dep), servers, Path::new("."), probe)
            .statuses
            .remove(0)
            .state
    }

    #[test]
    fn mcp_server_satisfied_and_missing() {
        let probe = FakeProbe {
            env: HashMap::from([("MESHY_API_KEY".into(), "sk-123".into())]),
            bins: vec![],
        };
        let d = dep(
            "meshy",
            Needs::McpServer {
                server: McpServerName::new("meshy").unwrap(),
                env: vec!["MESHY_API_KEY".into()],
            },
        );
        assert_eq!(
            eval_one(&d, &[server("meshy")], &probe),
            DependencyState::Satisfied
        );
        // Server not configured.
        assert!(
            matches!(eval_one(&d, &[], &probe), DependencyState::Unmet(m) if m.contains("meshy"))
        );
        // Server present but the key is missing.
        let no_key = FakeProbe {
            env: HashMap::new(),
            bins: vec![],
        };
        assert!(matches!(
            eval_one(&d, &[server("meshy")], &no_key),
            DependencyState::Unmet(m) if m.contains("MESHY_API_KEY")
        ));
        // A blank key counts as unset.
        let blank = FakeProbe {
            env: HashMap::from([("MESHY_API_KEY".into(), "  ".into())]),
            bins: vec![],
        };
        assert!(!eval_one(&d, &[server("meshy")], &blank).is_satisfied());
    }

    #[test]
    fn env_and_binary_kinds() {
        let probe = FakeProbe {
            env: HashMap::from([("TOKEN".into(), "x".into())]),
            bins: vec!["blender".into()],
        };
        assert_eq!(
            eval_one(&dep("t", Needs::Env("TOKEN".into())), &[], &probe),
            DependencyState::Satisfied
        );
        assert!(matches!(
            eval_one(&dep("t", Needs::Env("NOPE".into())), &[], &probe),
            DependencyState::Unmet(m) if m.contains("NOPE")
        ));
        assert_eq!(
            eval_one(&dep("b", Needs::Binary("blender".into())), &[], &probe),
            DependencyState::Satisfied
        );
        assert!(matches!(
            eval_one(&dep("b", Needs::Binary("nope".into())), &[], &probe),
            DependencyState::Unmet(m) if m.contains("nope")
        ));
    }

    #[test]
    fn a_custom_remedy_overrides_the_default() {
        let probe = FakeProbe {
            env: HashMap::new(),
            bins: vec![],
        };
        let mut d = dep("t", Needs::Env("NOPE".into()));
        d.remedy = Some("do the thing".into());
        assert_eq!(
            eval_one(&d, &[], &probe),
            DependencyState::Unmet("do the thing".into())
        );
    }

    #[test]
    fn script_check_states() {
        let probe = FakeProbe {
            env: HashMap::new(),
            bins: vec![],
        };
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, body: &str| {
            std::fs::write(dir.path().join(name), body).unwrap();
        };
        write("ok.rhai", "fn check() { () }");
        write("bad.rhai", r#"fn check() { "install it" }"#);
        write("broken.rhai", r#"fn check() { throw "boom" }"#);
        let run = |name: &str| {
            evaluate(
                &[dep("s", Needs::Check(CodeRef::File(name.into())))],
                &[],
                dir.path(),
                &probe,
            )
            .statuses
            .remove(0)
            .state
        };
        assert_eq!(run("ok.rhai"), DependencyState::Satisfied);
        assert_eq!(run("bad.rhai"), DependencyState::Unmet("install it".into()));
        assert!(matches!(run("broken.rhai"), DependencyState::Unusable(r) if r.contains("boom")));
        assert!(
            matches!(run("missing.rhai"), DependencyState::Unusable(r) if r.contains("cannot read"))
        );
        // A path escaping the blueprint dir is refused.
        assert!(matches!(
            run("../escape.rhai"),
            DependencyState::Unusable(r) if r.contains("outside the blueprint")
        ));
        // A script that will not compile is unusable.
        write("nofn.rhai", "fn other() { () }");
        assert!(
            matches!(run("nofn.rhai"), DependencyState::Unusable(r) if r.contains("fn check()"))
        );
        // A check written inline needs no file.
        let inline = |source: &str| {
            evaluate(
                &[dep("s", Needs::Check(CodeRef::Inline(source.into())))],
                &[],
                dir.path(),
                &probe,
            )
            .statuses
            .remove(0)
        };
        let ok = inline("fn check() { () }");
        assert_eq!((ok.kind, ok.state), ("script", DependencyState::Satisfied));
        assert_eq!(code_label(&CodeRef::Inline(String::new())), "inline check");
    }

    #[test]
    fn report_blocking_and_message() {
        let probe = FakeProbe {
            env: HashMap::new(),
            bins: vec![],
        };
        let mut optional = dep("opt", Needs::Env("NOPE".into()));
        optional.required = false;
        let report = evaluate(
            &[
                dep("need", Needs::Env("NOPE".into())),
                optional,
                dep("have", Needs::Env("NOPE".into())),
            ],
            &[],
            Path::new("."),
            &probe,
        );
        // Only the required, unmet one blocks.
        let blocking = report.blocking();
        assert_eq!(blocking.len(), 2); // "need" and "have" are both required+unmet
        let msg = report.blocking_message().unwrap();
        assert!(msg.contains("need"), "{msg}");
        assert!(msg.contains("lev deps install"), "{msg}");
        assert!(
            !msg.contains("opt"),
            "optional deps are not blocking: {msg}"
        );
    }

    #[test]
    fn dependency_state_detail_covers_every_variant() {
        assert_eq!(DependencyState::Satisfied.detail(), "");
        assert_eq!(DependencyState::Unmet("fix it".into()).detail(), "fix it");
        assert_eq!(DependencyState::Unusable("broke".into()).detail(), "broke");
    }

    #[test]
    fn status_line_covers_marks_and_optional() {
        let mk = |state, required| DependencyStatus {
            name: "d".into(),
            kind: "env",
            required,
            state,
        };
        assert_eq!(
            mk(DependencyState::Satisfied, true).line(),
            "[ok  ] d (env)"
        );
        assert_eq!(
            mk(DependencyState::Unmet("set it".into()), true).line(),
            "[MISS] d (env) - set it"
        );
        assert_eq!(
            mk(DependencyState::Unusable("broke".into()), false).line(),
            "[ERR ] d (env) (optional) - broke"
        );
    }

    #[test]
    fn a_satisfied_report_has_no_blocking_message() {
        let probe = FakeProbe {
            env: HashMap::from([("TOKEN".into(), "x".into())]),
            bins: vec![],
        };
        let report = evaluate(
            &[dep("t", Needs::Env("TOKEN".into()))],
            &[],
            Path::new("."),
            &probe,
        );
        assert!(report.blocking().is_empty());
        assert!(report.blocking_message().is_none());
        assert!(report.statuses[0].state.is_satisfied());
    }

    #[test]
    fn system_probe_reads_the_real_machine() {
        let probe = SystemProbe;
        // PATH is always set; a nonsense var is not.
        assert!(probe.env("PATH").is_some());
        assert!(probe.env("LEVIATH_DEPS_DEFINITELY_UNSET_XYZ").is_none());
        // A binary that cannot exist is not found.
        assert!(!probe.which("leviath-not-a-real-binary-xyz"));
    }

    #[test]
    fn which_in_finds_files_and_handles_no_path() {
        assert!(!which_in(None, "anything"));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("foo"), b"#!/bin/sh\n").unwrap();
        std::fs::write(dir.path().join("bar.exe"), b"MZ").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert!(which_in(Some(path.clone()), "foo"));
        assert!(which_in(Some(path.clone()), "bar"));
        assert!(!which_in(Some(path), "missing"));
    }
}

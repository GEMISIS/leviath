//! Scripted taint-gate rules: load `~/.config/leviath/rules/*.rhai` and build the
//! [`ScriptRuleChecker`] the runtime's taint gate consults after the static
//! allowlist. The daemon owns the Rhai engine (leviath-runtime has no scripting
//! dependency), so the checker is installed as a world resource.

use std::path::Path;
use std::sync::Arc;

use leviath_runtime::taint::ScriptRuleChecker;

/// Build a [`ScriptRuleChecker`] from every `*.rhai` file in `rules_dir`. When the
/// directory is absent/unreadable or holds no rule scripts, a no-op checker (that
/// never allows anything) is returned, so the daemon can install it
/// unconditionally. Each script receives a `context` map
/// (`tool` / `target` / `taint_level`) and should evaluate to `true` to allow the
/// call; the first script that allows wins and its file stem is the rule name.
///
/// The sources are read once, here, and captured by the returned closure, so a
/// checker built from a directory is a snapshot of it. That is why the daemon
/// holds the sources too (`policy_reload`): re-reading them is how an edited
/// rule reaches a run without a restart.
pub(crate) fn build_gate_script_checker(rules_dir: &Path) -> Arc<ScriptRuleChecker> {
    checker_from_scripts(read_rule_scripts(rules_dir))
}

/// The `(rule name, source)` pairs in `rules_dir`, in file-name order so two
/// reads of an unchanged directory compare equal and the "first rule that
/// allows wins" verdict does not depend on the order the filesystem hands
/// entries back. A missing or unreadable directory reads as no rules.
pub(crate) fn read_rule_scripts(rules_dir: &Path) -> Vec<(String, String)> {
    let mut scripts: Vec<(String, String)> = std::fs::read_dir(rules_dir)
        .ok()
        .into_iter()
        .flatten() // ReadDir → Result<DirEntry>
        .flatten() // drop per-entry errors
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rhai") {
                return None;
            }
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("rule")
                .to_string();
            std::fs::read_to_string(&path)
                .ok()
                .map(|source| (name, source))
        })
        .collect();
    scripts.sort();
    scripts
}

/// Compile `scripts` into the checker the gate consults. Empty ⇒ a checker that
/// never allows anything, so the daemon can install one unconditionally.
pub(crate) fn checker_from_scripts(scripts: Vec<(String, String)>) -> Arc<ScriptRuleChecker> {
    if scripts.is_empty() {
        return Arc::new(|_tool, _target, _taint| None);
    }

    let engine = leviath_scripting::ScriptEngine::new();
    Arc::new(
        move |tool: &str,
              target: Option<&str>,
              taint: leviath_core::TaintLevel|
              -> Option<String> {
            scripts.iter().find_map(|(name, source)| {
                engine
                    .check_gate_rule(source, tool, target, taint.as_str())
                    .unwrap_or(false)
                    .then(|| name.clone())
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::TaintLevel;

    fn write_rule(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn missing_or_empty_dir_yields_a_noop_checker() {
        // Nonexistent directory ⇒ a checker that never allows anything.
        let dir = tempfile::tempdir().unwrap();
        let noop = build_gate_script_checker(&dir.path().join("nope"));
        assert_eq!(noop("shell", None, TaintLevel::Public), None);
        // Present but no `.rhai` files (a stray non-rule file is ignored).
        write_rule(dir.path(), "notes.txt", "ignored");
        let noop2 = build_gate_script_checker(dir.path());
        assert_eq!(noop2("shell", None, TaintLevel::Public), None);
    }

    #[test]
    fn a_matching_rule_allows_and_names_itself() {
        let dir = tempfile::tempdir().unwrap();
        // Allow `shell` regardless of taint; deny everything else.
        write_rule(dir.path(), "company.rhai", r#"context.tool == "shell""#);
        let checker = build_gate_script_checker(dir.path());

        // Matching tool ⇒ Some(rule name).
        assert_eq!(
            checker("shell", None, TaintLevel::Internal),
            Some("company".to_string())
        );
        // Non-matching tool ⇒ None (rule evaluated false).
        assert_eq!(checker("read_file", None, TaintLevel::Internal), None);
    }

    #[test]
    fn a_rule_can_key_on_target_and_taint() {
        let dir = tempfile::tempdir().unwrap();
        write_rule(
            dir.path(),
            "internal_only.rhai",
            r#"context.taint_level == "internal" && context.target == "ops@corp""#,
        );
        let checker = build_gate_script_checker(dir.path());
        assert!(checker("send_email", Some("ops@corp"), TaintLevel::Internal).is_some());
        assert!(checker("send_email", Some("ops@corp"), TaintLevel::Private).is_none());
    }

    #[test]
    fn a_built_checker_is_a_snapshot_of_the_directory() {
        // Why `policy_reload` exists: the sources live in the closure, so a
        // checker built at boot answers from the file as it was at boot no
        // matter what is written over it afterwards.
        let dir = tempfile::tempdir().unwrap();
        write_rule(dir.path(), "company.rhai", r#"context.tool == "shell""#);
        let checker = build_gate_script_checker(dir.path());
        write_rule(
            dir.path(),
            "company.rhai",
            r#"context.tool == "send_email""#,
        );
        assert_eq!(
            checker("send_email", None, TaintLevel::Public),
            None,
            "an already-built checker cannot see the edit; something has to rebuild it"
        );
    }

    #[test]
    fn rule_sources_come_back_in_file_name_order() {
        let dir = tempfile::tempdir().unwrap();
        write_rule(dir.path(), "zulu.rhai", "false");
        write_rule(dir.path(), "alpha.rhai", "true");
        let names: Vec<String> = read_rule_scripts(dir.path())
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, vec!["alpha".to_string(), "zulu".to_string()]);
    }

    #[test]
    fn a_script_that_errors_is_treated_as_no_match() {
        let dir = tempfile::tempdir().unwrap();
        // Not a bool expression ⇒ eval error ⇒ unwrap_or(false) ⇒ no match.
        write_rule(dir.path(), "broken.rhai", "this is not valid rhai @@@");
        let checker = build_gate_script_checker(dir.path());
        assert_eq!(checker("shell", None, TaintLevel::Public), None);
    }
}

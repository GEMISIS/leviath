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
pub fn build_gate_script_checker(rules_dir: &Path) -> Arc<ScriptRuleChecker> {
    let scripts: Vec<(String, String)> = std::fs::read_dir(rules_dir)
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
    fn a_script_that_errors_is_treated_as_no_match() {
        let dir = tempfile::tempdir().unwrap();
        // Not a bool expression ⇒ eval error ⇒ unwrap_or(false) ⇒ no match.
        write_rule(dir.path(), "broken.rhai", "this is not valid rhai @@@");
        let checker = build_gate_script_checker(dir.path());
        assert_eq!(checker("shell", None, TaintLevel::Public), None);
    }
}

//! Keeping the taint gate's policy in step with the files it is written in.
//!
//! Two files decide whether a tainted outbound call is allowed: `policy.toml`
//! (the static allowlist and the `[mcp_overrides]`) and `rules/*.rhai` (the
//! scripted rules consulted after it). Both are installed as world resources,
//! and both are re-read here so `lev policy add` lands on the next run: it
//! writes `policy.toml` and prints the rule it wrote, and a gate that goes on
//! blocking the call that rule permits has nothing to say why.
//!
//! The scripted half needs this more than the static one, and in a way no
//! "restart the daemon" advice covers: a rule's sources are read into a
//! closure, so an installed checker keeps answering with the text it was built
//! from no matter how often the file is edited. Only a rebuild picks up a
//! change.
//!
//! The shape is the config reloader's, beside this module: stat the files,
//! reload when they moved, compare, and swap the world resource before the next
//! run resolves anything. They cannot ride that reloader itself - they are not
//! `config.toml`, they live under the platform config directory
//! (`~/.config/leviath` on Linux), so they get their own mtime check here.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use leviath_runtime::taint::ScriptRuleChecker;

/// One `*.rhai` rule file as of a stat: which file, and when it last changed.
/// Comparing the whole sorted list catches an edit, a new rule, and a deleted
/// one, which a directory mtime alone does not (on most filesystems a write
/// through an existing file does not touch its directory).
type RulesStamp = Vec<(PathBuf, Option<SystemTime>)>;

/// What a refresh built and `install` has yet to hand to the world.
#[derive(Default)]
struct Pending {
    policy: Option<leviath_core::PolicyConfig>,
    rules: Option<Arc<ScriptRuleChecker>>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.policy.is_none() && self.rules.is_none()
    }
}

struct State {
    /// The mtime `policy` was read at. `None` means the file was absent, and
    /// the empty default is in force until it appears.
    policy_mtime: Option<SystemTime>,
    policy: leviath_core::PolicyConfig,
    /// The stat of the rules directory the current `scripts` were read at.
    rules_stamp: RulesStamp,
    /// The rule sources behind the installed checker, kept so a directory
    /// whose mtimes moved without its contents changing (a `touch`, a rewrite
    /// of the same bytes) does not recompile the engine.
    scripts: Vec<(String, String)>,
    pending: Pending,
}

/// Serves the freshest taint-gate policy, reloading `policy.toml` and
/// `rules/*.rhai` when they change on disk.
///
/// Split into [`refresh`](Self::refresh) (stat, reload, compare) and
/// [`install`](Self::install) (hand the result to the world) for the same
/// reason the provider reload is: the world is only reachable from the host's
/// synchronous hooks, and refreshing is the part a caller can do anywhere.
pub struct PolicyReload {
    policy_path: PathBuf,
    rules_dir: PathBuf,
    state: Mutex<State>,
}

/// What a [`refresh`](PolicyReload::refresh) found had changed. Both false is
/// the usual answer and means `install` has nothing to do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PolicyChange {
    /// `policy.toml` now says something different.
    pub policy: bool,
    /// The `rules/*.rhai` set now says something different.
    pub rules: bool,
}

impl PolicyChange {
    /// Whether anything at all changed.
    pub fn any(&self) -> bool {
        self.policy || self.rules
    }
}

impl PolicyReload {
    /// Start from what the two paths hold right now, with both halves pending
    /// so the first [`install`](Self::install) seeds the world. That is the
    /// daemon's boot install, expressed as the first refresh rather than as a
    /// separate one-off load.
    pub fn new(policy_path: PathBuf, rules_dir: PathBuf) -> Self {
        let policy_mtime = file_mtime(&policy_path);
        let policy = load_policy(&policy_path).unwrap_or_else(|e| {
            warn_unloadable(&policy_path, &e);
            leviath_core::PolicyConfig::default()
        });
        let rules_stamp = stamp_rules(&rules_dir);
        let scripts = crate::daemon::gate_rules::read_rule_scripts(&rules_dir);
        let checker = crate::daemon::gate_rules::checker_from_scripts(scripts.clone());
        Self {
            policy_path,
            rules_dir,
            state: Mutex::new(State {
                policy_mtime,
                policy: policy.clone(),
                rules_stamp,
                scripts,
                pending: Pending {
                    policy: Some(policy),
                    rules: Some(checker),
                },
            }),
        }
    }

    /// One over the paths the daemon really uses: `<config>/leviath/policy.toml`
    /// and `<config>/leviath/rules`.
    pub fn for_daemon() -> Arc<Self> {
        Arc::new(Self::new(
            crate::commands::policy::policy_path(),
            crate::commands::policy::rules_dir(),
        ))
    }

    /// Re-stat both paths and rebuild whatever moved, holding the result for
    /// [`install`](Self::install).
    ///
    /// A `policy.toml` that will not parse keeps the policy already in force,
    /// and records the broken file's mtime so the warning is logged once
    /// rather than on every spawn until it is fixed. Losing an allowlist to a
    /// half-saved edit would silently *tighten* the gate, which is the wrong
    /// direction to fail in for a file people edit by hand.
    pub fn refresh(&self) -> PolicyChange {
        let policy_mtime = file_mtime(&self.policy_path);
        let rules_stamp = stamp_rules(&self.rules_dir);
        let mut state = self.lock();
        let mut change = PolicyChange::default();

        if policy_mtime != state.policy_mtime {
            state.policy_mtime = policy_mtime;
            match load_policy(&self.policy_path) {
                Ok(policy) => {
                    if policy != state.policy {
                        // Bound in a plain statement rather than left as a lazy
                        // `%path.display()` field: the method call inside a
                        // structured field only runs when the callsite is
                        // enabled, and tracing caches that interest globally,
                        // so under a coverage run it can be unreachable. The
                        // config reloader beside this does the same.
                        let displayed = self.policy_path.display();
                        tracing::info!(
                            path = %displayed,
                            "reloaded the taint policy after an on-disk change"
                        );
                        state.policy = policy.clone();
                        state.pending.policy = Some(policy);
                        change.policy = true;
                    }
                }
                Err(e) => warn_unloadable(&self.policy_path, &e),
            }
        }

        if rules_stamp != state.rules_stamp {
            state.rules_stamp = rules_stamp;
            let scripts = crate::daemon::gate_rules::read_rule_scripts(&self.rules_dir);
            if scripts != state.scripts {
                let displayed = self.rules_dir.display();
                let count = scripts.len();
                tracing::info!(
                    path = %displayed,
                    rules = count,
                    "reloaded the scripted gate rules after an on-disk change"
                );
                state.scripts = scripts.clone();
                state.pending.rules =
                    Some(crate::daemon::gate_rules::checker_from_scripts(scripts));
                change.rules = true;
            }
        }

        change
    }

    /// [`refresh`](Self::refresh), then [`install`](Self::install) whatever it
    /// built. What the daemon's spawn and reload hooks call: one statement, and
    /// two stats when nothing moved.
    pub fn refresh_into(&self, world: &mut leviath_runtime::PipelineWorld) {
        if self.refresh().any() {
            self.install(world);
        }
    }

    /// Put whatever [`refresh`](Self::refresh) built into `world`. Does nothing
    /// when nothing is pending, so it is safe to call before every spawn.
    pub fn install(&self, world: &mut leviath_runtime::PipelineWorld) {
        let pending = std::mem::take(&mut self.lock().pending);
        if pending.is_empty() {
            return;
        }
        let world = world.world_mut();
        if let Some(policy) = pending.policy {
            world.insert_resource(leviath_runtime::pipeline::PolicyGate(policy));
        }
        if let Some(rules) = pending.rules {
            world.insert_resource(leviath_runtime::pipeline::GateScriptRules(rules));
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The policy file's contents, or the reason it could not be read. A missing
/// file is not an error: no `policy.toml` means no allowlist.
fn load_policy(path: &Path) -> Result<leviath_core::PolicyConfig, String> {
    if !path.exists() {
        return Ok(leviath_core::PolicyConfig::default());
    }
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    leviath_core::PolicyConfig::from_toml(&content)
}

/// One warning per broken save, not one per spawn: the caller records the
/// mtime either way, so the next stat of an unfixed file is a no-op.
fn warn_unloadable(path: &Path, error: &str) {
    let displayed = path.display();
    tracing::warn!(
        path = %displayed,
        error = %error,
        "policy.toml could not be read; keeping the policy already in force"
    );
}

/// Every `*.rhai` file in `dir` with its mtime, sorted by path. A missing or
/// unreadable directory stamps as empty, and starts being watched the moment
/// it appears.
fn stamp_rules(dir: &Path) -> RulesStamp {
    let mut stamp: RulesStamp = std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().and_then(|e| e.to_str()) == Some("rhai"))
                .then(|| (path.clone(), file_mtime(&path)))
        })
        .collect();
    stamp.sort();
    stamp
}

/// The file's mtime, or `None` when it does not exist or cannot be stat'd.
fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::TaintLevel;
    use std::time::Duration;

    /// Force a file's mtime strictly newer, so an edit inside one clock tick is
    /// still a change (the config reloader's tests do the same).
    fn bump_mtime(path: &Path) {
        let later = SystemTime::now() + Duration::from_secs(5);
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(later).unwrap();
    }

    fn write(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        bump_mtime(path);
    }

    /// A world with nothing in it but the resources under test.
    fn world() -> (tokio::runtime::Runtime, leviath_runtime::PipelineWorld) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let world = leviath_runtime::PipelineWorld::new(
            leviath_runtime::ProviderRegistry::new(),
            Arc::new(crate::daemon::tool_service::CliToolService::new()),
            leviath_runtime::inference_pool::InferencePoolConfig::new(),
            1,
            None,
            runtime.handle().clone(),
        );
        (runtime, world)
    }

    fn installed_rule_names(world: &mut leviath_runtime::PipelineWorld) -> Vec<String> {
        world
            .world_mut()
            .get_resource::<leviath_runtime::pipeline::PolicyGate>()
            .map(|p| p.0.allowlist.iter().map(|r| r.tool.clone()).collect())
            .unwrap_or_default()
    }

    fn installed_checker(world: &mut leviath_runtime::PipelineWorld) -> Arc<ScriptRuleChecker> {
        world
            .world_mut()
            .get_resource::<leviath_runtime::pipeline::GateScriptRules>()
            .expect("the daemon always installs a checker")
            .0
            .clone()
    }

    fn paths(dir: &Path) -> (PathBuf, PathBuf) {
        let rules = dir.join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        (dir.join("policy.toml"), rules)
    }

    #[test]
    fn the_first_install_seeds_both_resources() {
        let dir = tempfile::tempdir().unwrap();
        let (policy_path, rules_dir) = paths(dir.path());
        write(
            &policy_path,
            "[[allowlist]]\ntool = \"send_email\"\nmax_sensitivity = \"internal\"\n",
        );
        write(
            &rules_dir.join("company.rhai"),
            r#"context.tool == "shell""#,
        );

        let reload = PolicyReload::new(policy_path, rules_dir);
        let (_rt, mut world) = world();
        reload.install(&mut world);

        assert_eq!(installed_rule_names(&mut world), vec!["send_email"]);
        assert_eq!(
            installed_checker(&mut world)("shell", None, TaintLevel::Internal),
            Some("company".to_string())
        );
    }

    #[test]
    fn an_unchanged_pair_refreshes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (policy_path, rules_dir) = paths(dir.path());
        write(&policy_path, "[[allowlist]]\ntool = \"shell\"\n");
        write(&rules_dir.join("company.rhai"), "true");

        let reload = PolicyReload::new(policy_path, rules_dir);
        assert_eq!(reload.refresh(), PolicyChange::default());
        assert!(!reload.refresh().any());
    }

    /// A rule `lev policy add` wrote after boot is in force on the next run,
    /// with no daemon restart.
    #[test]
    fn a_rule_added_after_boot_reaches_the_world() {
        let dir = tempfile::tempdir().unwrap();
        let (policy_path, rules_dir) = paths(dir.path());
        let reload = PolicyReload::new(policy_path.clone(), rules_dir);
        let (_rt, mut world) = world();
        reload.install(&mut world);
        assert!(installed_rule_names(&mut world).is_empty());

        write(
            &policy_path,
            "[[allowlist]]\ntool = \"send_email\"\nmax_sensitivity = \"internal\"\n",
        );
        assert_eq!(
            reload.refresh(),
            PolicyChange {
                policy: true,
                rules: false
            }
        );
        reload.install(&mut world);
        assert_eq!(
            installed_rule_names(&mut world),
            vec!["send_email"],
            "the rule the user just added has to be in force without a restart"
        );
    }

    /// The scripted half, where an edit only lands if the engine is rebuilt:
    /// the sources live inside the compiled closure.
    #[test]
    fn an_edited_rule_script_reaches_the_world() {
        let dir = tempfile::tempdir().unwrap();
        let (policy_path, rules_dir) = paths(dir.path());
        let rule = rules_dir.join("company.rhai");
        write(&rule, r#"context.tool == "shell""#);
        let reload = PolicyReload::new(policy_path, rules_dir);
        let (_rt, mut world) = world();
        reload.install(&mut world);
        assert!(installed_checker(&mut world)("send_email", None, TaintLevel::Internal).is_none());

        write(&rule, r#"context.tool == "send_email""#);
        assert_eq!(
            reload.refresh(),
            PolicyChange {
                policy: false,
                rules: true
            }
        );
        reload.install(&mut world);
        assert_eq!(
            installed_checker(&mut world)("send_email", None, TaintLevel::Internal),
            Some("company".to_string()),
            "the edited rule has to be the one the gate consults"
        );
    }

    #[test]
    fn a_deleted_rule_file_stops_allowing() {
        let dir = tempfile::tempdir().unwrap();
        let (policy_path, rules_dir) = paths(dir.path());
        let rule = rules_dir.join("company.rhai");
        write(&rule, r#"context.tool == "shell""#);
        let reload = PolicyReload::new(policy_path, rules_dir);
        let (_rt, mut world) = world();
        reload.install(&mut world);

        std::fs::remove_file(&rule).unwrap();
        assert!(reload.refresh().rules);
        reload.install(&mut world);
        assert_eq!(
            installed_checker(&mut world)("shell", None, TaintLevel::Internal),
            None,
            "a rule the user deleted must stop permitting the call"
        );
    }

    #[test]
    fn touching_a_rule_without_changing_it_is_not_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let (policy_path, rules_dir) = paths(dir.path());
        let rule = rules_dir.join("company.rhai");
        write(&rule, "true");
        let reload = PolicyReload::new(policy_path, rules_dir);

        // Same bytes, newer mtime: the stat moves, the sources do not, and
        // recompiling the engine over it would be work for nothing.
        write(&rule, "true");
        assert!(
            !reload.refresh().any(),
            "an identical rewrite must not rebuild the checker"
        );
    }

    #[test]
    fn rewriting_the_policy_with_the_same_content_is_not_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let (policy_path, rules_dir) = paths(dir.path());
        write(&policy_path, "[[allowlist]]\ntool = \"shell\"\n");
        let reload = PolicyReload::new(policy_path.clone(), rules_dir);
        write(&policy_path, "[[allowlist]]\ntool = \"shell\"\n");
        assert!(!reload.refresh().any());
    }

    #[test]
    fn a_broken_policy_save_keeps_the_one_in_force_and_warns_once() {
        let dir = tempfile::tempdir().unwrap();
        let (policy_path, rules_dir) = paths(dir.path());
        write(
            &policy_path,
            "[[allowlist]]\ntool = \"send_email\"\nmax_sensitivity = \"internal\"\n",
        );
        let reload = PolicyReload::new(policy_path.clone(), rules_dir);
        let (_rt, mut world) = world();
        reload.install(&mut world);

        write(&policy_path, "this is not : : toml");
        assert!(!reload.refresh().any());
        // The mtime was recorded even though the load failed, so a file left
        // broken is stat'd once more and never re-read.
        assert!(!reload.refresh().any());
        reload.install(&mut world);
        assert_eq!(
            installed_rule_names(&mut world),
            vec!["send_email"],
            "a half-saved edit must not quietly drop the user's allowlist"
        );
    }

    #[test]
    fn a_good_save_after_a_broken_one_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let (policy_path, rules_dir) = paths(dir.path());
        write(&policy_path, "[[allowlist]]\ntool = \"a\"\n");
        let reload = PolicyReload::new(policy_path.clone(), rules_dir);
        write(&policy_path, "broken : :");
        let _ = reload.refresh();
        write(&policy_path, "[[allowlist]]\ntool = \"b\"\n");
        assert!(reload.refresh().policy);
        let (_rt, mut world) = world();
        reload.install(&mut world);
        assert_eq!(installed_rule_names(&mut world), vec!["b"]);
    }

    #[test]
    fn a_policy_file_that_appears_after_boot_is_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        let (policy_path, rules_dir) = paths(dir.path());
        let reload = PolicyReload::new(policy_path.clone(), rules_dir);
        std::fs::write(&policy_path, "[[allowlist]]\ntool = \"shell\"\n").unwrap();
        assert!(reload.refresh().policy);
    }

    #[test]
    fn a_policy_file_that_cannot_be_read_falls_back_to_an_empty_one() {
        // A directory where the file should be: `exists()` is true and the read
        // fails, which is the io-error arm rather than the parse-error one.
        let dir = tempfile::tempdir().unwrap();
        let (policy_path, rules_dir) = paths(dir.path());
        std::fs::create_dir(&policy_path).unwrap();
        let reload = PolicyReload::new(policy_path, rules_dir);
        let (_rt, mut world) = world();
        reload.install(&mut world);
        assert!(installed_rule_names(&mut world).is_empty());
    }

    #[test]
    fn install_hands_each_rebuild_over_once() {
        let dir = tempfile::tempdir().unwrap();
        let (policy_path, rules_dir) = paths(dir.path());
        let reload = PolicyReload::new(policy_path, rules_dir);
        let (_rt, mut world) = world();
        reload.install(&mut world);
        assert!(reload.lock().pending.is_empty());
        // Nothing pending: a second install before any refresh is a no-op.
        reload.install(&mut world);
        assert!(reload.lock().pending.is_empty());
    }

    #[test]
    fn refresh_into_installs_a_change_and_leaves_an_unchanged_world_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (policy_path, rules_dir) = paths(dir.path());
        let reload = PolicyReload::new(policy_path.clone(), rules_dir);
        let (_rt, mut world) = world();
        reload.install(&mut world);

        // Nothing moved: the world keeps what it has and no rebuild happens.
        reload.refresh_into(&mut world);
        assert!(installed_rule_names(&mut world).is_empty());

        write(&policy_path, "[[allowlist]]\ntool = \"shell\"\n");
        reload.refresh_into(&mut world);
        assert_eq!(installed_rule_names(&mut world), vec!["shell"]);
    }

    #[test]
    fn for_daemon_watches_the_real_policy_paths() {
        let reload = PolicyReload::for_daemon();
        assert_eq!(
            reload.policy_path,
            crate::commands::policy::policy_path(),
            "the daemon has to watch the file `lev policy add` writes"
        );
        assert_eq!(reload.rules_dir, crate::commands::policy::rules_dir());
    }
}

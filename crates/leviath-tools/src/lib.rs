//! Native built-in tools for Leviath agents.
//!
//! Provides file system and shell tools sandboxed to a working directory.

use leviath_core::resolves_within;
use leviath_providers::Tool;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::process::Command;
use tokio::time::{Duration, timeout};

// The tool families, one module per concern; lib.rs keeps the struct and
// its constructors.
mod context;
mod defs;
mod exec;
mod platform;
pub use context::*;
pub use platform::*;

/// Built-in tools: read_file, write_file, edit_file, list_dir, shell.
///
/// Carries the [`PlatformCapabilities`] of the current platform; tools whose
/// [`tool_required_capabilities`] aren't satisfied are dropped from
/// [`tool_defs`](Self::tool_defs), [`names`](Self::names), and rejected by
/// [`execute`](Self::execute).
pub struct BuiltinTools {
    ctx: ToolContext,
    platform: PlatformCapabilities,
    /// When set, shell commands run through this sandbox instead of the host.
    shell_executor: Option<Arc<dyn ShellExecutor>>,
}

impl BuiltinTools {
    /// Create a new BuiltinTools instance with the given sandbox context,
    /// filtering tools against the current platform's capabilities.
    pub fn new(ctx: ToolContext) -> Self {
        Self {
            ctx,
            platform: PlatformCapabilities::current(),
            shell_executor: None,
        }
    }

    /// Route this agent's shell execution through `executor` (a container /
    /// namespace sandbox) instead of the host.
    pub fn with_shell_executor(mut self, executor: Arc<dyn ShellExecutor>) -> Self {
        self.shell_executor = Some(executor);
        self
    }

    /// Create a BuiltinTools instance with an explicit platform capability set,
    /// for tests or hosts that need to override the compile-time default.
    pub fn with_capabilities(ctx: ToolContext, platform: PlatformCapabilities) -> Self {
        Self {
            ctx,
            platform,
            shell_executor: None,
        }
    }

    /// Whether a built-in named `canonical_name` is available on this platform.
    fn available(&self, canonical_name: &str) -> bool {
        self.platform
            .satisfies(tool_required_capabilities(canonical_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_tools(dir: &std::path::Path) -> BuiltinTools {
        BuiltinTools::new(ToolContext::new(dir.to_path_buf()))
    }

    /// Built-ins over a mobile capability set (no `ProcessSpawn`), so the
    /// `shell` tool and its `bash` alias are filtered out.
    fn make_mobile_tools(dir: &std::path::Path) -> BuiltinTools {
        BuiltinTools::with_capabilities(
            ToolContext::new(dir.to_path_buf()),
            PlatformCapabilities::mobile(),
        )
    }

    // ── Tool definitions ──────────────────────────────────────────────────

    #[test]
    fn tool_defs_returns_sixteen_tools() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let defs = tools.tool_defs();
        assert_eq!(defs.len(), 16);
    }

    #[test]
    fn tool_defs_names_are_correct() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let names: Vec<String> = tools.tool_defs().iter().map(|t| t.name.clone()).collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"read_files".to_string()));
        assert!(names.contains(&"write_file".to_string()));
        assert!(names.contains(&"edit_file".to_string()));
        assert!(names.contains(&"list_dir".to_string()));
        assert!(names.contains(&"shell".to_string()));
        assert!(names.contains(&"present_for_review".to_string()));
        assert!(names.contains(&"ask_user_text".to_string()));
        assert!(names.contains(&"ask_user_choice".to_string()));
        assert!(names.contains(&"ask_user_confirm".to_string()));
        assert!(names.contains(&"edit_document".to_string()));
        assert!(names.contains(&"context_write".to_string()));
        assert!(names.contains(&"context_append".to_string()));
        assert!(names.contains(&"context_read".to_string()));
        assert!(names.contains(&"context_delete".to_string()));
        assert!(names.contains(&"context_list".to_string()));
    }

    #[test]
    fn tool_defs_edit_document_requires_content() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let def = tools
            .tool_defs()
            .into_iter()
            .find(|t| t.name == "edit_document")
            .expect("edit_document tool def must exist");
        let required = def.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "content"));
        assert_eq!(def.parameters["properties"]["content"]["type"], "string");
        // Also present in the builtin name list.
        assert!(tools.names().contains(&"edit_document".to_string()));
    }

    #[test]
    fn tool_defs_ask_user_choice_has_options_array() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let def = tools
            .tool_defs()
            .into_iter()
            .find(|t| t.name == "ask_user_choice")
            .unwrap();
        let required = def.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "prompt"));
        assert!(required.iter().any(|v| v == "options"));
        assert_eq!(def.parameters["properties"]["options"]["type"], "array");
    }

    #[tokio::test]
    async fn context_tools_return_runtime_error() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        for name in [
            "context_write",
            "context_append",
            "context_read",
            "context_delete",
            "context_list",
        ] {
            let result = tools.execute(name, serde_json::json!({})).await;
            assert!(result.contains("context tools must be handled by the runtime"));
        }
    }

    #[tokio::test]
    async fn ask_user_tools_not_handled_by_builtin_execute() {
        // ask_user_* tools are intercepted upstream (worker.rs/foreground.rs),
        // exactly like present_for_review - execute() must never run them.
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        for name in [
            "ask_user_text",
            "ask_user_choice",
            "ask_user_confirm",
            "edit_document",
        ] {
            let result = tools.execute(name, serde_json::json!({})).await;
            assert!(result.contains("Unknown built-in tool"));
        }
    }

    #[test]
    fn context_tool_descriptions_mention_key_concepts() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let defs = tools.tool_defs();

        let write_def = defs.iter().find(|t| t.name == "context_write").unwrap();
        assert!(
            write_def.description.contains("system prompt"),
            "context_write should mention system prompt: {}",
            write_def.description
        );
        assert!(
            write_def.description.contains("replaced"),
            "context_write should mention replacement: {}",
            write_def.description
        );

        let read_def = defs.iter().find(|t| t.name == "context_read").unwrap();
        assert!(
            read_def.description.contains("summary"),
            "context_read should mention summary: {}",
            read_def.description
        );

        let list_def = defs.iter().find(|t| t.name == "context_list").unwrap();
        assert!(
            list_def.description.contains("token"),
            "context_list should mention tokens: {}",
            list_def.description
        );

        let append_def = defs.iter().find(|t| t.name == "context_append").unwrap();
        assert!(
            append_def.description.contains("without replacing"),
            "context_append should mention 'without replacing': {}",
            append_def.description
        );
    }

    fn assert_has_description(name: &str, description: &str) {
        assert!(
            !description.is_empty(),
            "tool {} has empty description",
            name
        );
    }

    fn assert_has_object_params(name: &str, params: &serde_json::Value) {
        assert!(params.is_object(), "tool {} has non-object params", name);
    }

    #[test]
    fn tool_defs_have_descriptions() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        for def in tools.tool_defs() {
            assert_has_description(&def.name, &def.description);
        }
    }

    #[test]
    #[should_panic(expected = "tool bogus has empty description")]
    fn tool_defs_have_descriptions_panics_on_empty_description() {
        assert_has_description("bogus", "");
    }

    #[test]
    fn tool_defs_have_parameters() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        for def in tools.tool_defs() {
            assert_has_object_params(&def.name, &def.parameters);
        }
    }

    #[test]
    #[should_panic(expected = "tool bogus has non-object params")]
    fn tool_defs_have_parameters_panics_on_non_object_params() {
        assert_has_object_params("bogus", &serde_json::Value::Null);
    }

    // ── names() ───────────────────────────────────────────────────────────

    #[test]
    fn names_includes_bash_alias() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let names = tools.names();
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&"shell".to_string()));
    }

    #[test]
    fn canonical_tool_name_resolves_aliases_and_passes_others_through() {
        // An alias resolves to its canonical name.
        assert_eq!(canonical_tool_name("bash"), "shell");
        // A canonical built-in is unchanged.
        assert_eq!(canonical_tool_name("shell"), "shell");
        assert_eq!(canonical_tool_name("read_file"), "read_file");
        // An unknown name (e.g. an MCP tool whose server may not be installed)
        // passes through untouched, so it is matched/omitted as-is.
        assert_eq!(canonical_tool_name("acme__do_thing"), "acme__do_thing");
        // Every alias in the table round-trips to a real canonical name.
        for (alias, canonical) in TOOL_ALIASES {
            assert_eq!(canonical_tool_name(alias), *canonical);
        }
    }

    #[test]
    fn names_returns_seventeen_entries() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        assert_eq!(tools.names().len(), 17);
    }

    // ── Sub-agent tool definitions ────────────────────────────────────────

    #[test]
    fn subagent_tool_defs_returns_five_tools() {
        let defs = BuiltinTools::subagent_tool_defs();
        assert_eq!(defs.len(), 5);
    }

    #[test]
    fn subagent_tool_names_returns_five_names() {
        let names = BuiltinTools::subagent_tool_names();
        assert_eq!(names.len(), 5);
        assert!(names.contains(&"spawn_agent".to_string()));
        assert!(names.contains(&"check_agent".to_string()));
        assert!(names.contains(&"wait_for_agent".to_string()));
        assert!(names.contains(&"send_to_agent".to_string()));
        assert!(names.contains(&"kill_agent".to_string()));
    }

    #[test]
    fn subagent_tool_defs_names_match_subagent_tool_names() {
        let defs = BuiltinTools::subagent_tool_defs();
        let names = BuiltinTools::subagent_tool_names();
        let def_names: Vec<String> = defs.iter().map(|d| d.name.clone()).collect();
        assert_eq!(def_names, names);
    }

    // ── resolve() ─────────────────────────────────────────────────────────

    #[test]
    fn resolve_relative_path() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let result = tools.resolve("hello.txt").unwrap();
        assert!(result.starts_with(&tools.ctx.workdir));
        assert!(result.ends_with("hello.txt"));
    }

    #[test]
    fn resolve_rejects_path_escape() {
        let dir = std::env::temp_dir().join("leviath_test_sandbox");
        fs::create_dir_all(&dir).ok();
        let tools = make_tools(&dir);
        let result = tools.resolve("../../etc/passwd");
        assert!(result.is_err());
    }

    /// The escape a lexical check cannot see. `<workdir>/link -> /` makes
    /// `link/etc/passwd` textually contained the whole way, and the old
    /// `starts_with` containment let `fs::read_to_string` follow it straight out.
    ///
    /// This matters most where the containment is load-bearing: Leviath's file
    /// tools run on the *host* over the bind-mounted workdir even when the
    /// The containment refusal itself, driven through the injected predicate so
    /// it is exercised on every platform. The `#[cfg(unix)]` tests below prove
    /// the same refusal against a real symlink; this one proves the arm exists
    /// and fires on Windows too, where a test cannot create one.
    #[test]
    fn resolve_refuses_a_path_that_does_not_resolve_within_the_workdir() {
        fn escapes(_: &Path, _: &Path) -> bool {
            false
        }
        let dir = tempfile::tempdir().unwrap();
        let err = BuiltinTools::resolve_within("notes.txt", dir.path(), escapes)
            .expect_err("a path that resolves outside must be refused");
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    /// The converse, so the test above is not passing merely because everything
    /// is refused: with the real predicate an ordinary path resolves.
    #[test]
    fn resolve_admits_an_ordinary_path_within_the_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let resolved =
            BuiltinTools::resolve_within("notes.txt", dir.path(), leviath_core::resolves_within)
                .expect("an ordinary path resolves");
        assert!(resolved.ends_with("notes.txt"));
    }

    /// stage's `shell` is confined to a container, so a symlink the agent made
    /// inside the container escaped the container through these tools. It is also
    /// reachable from a checked-in symlink in a freshly cloned repository, which
    /// is exactly what a coding agent is pointed at.
    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("workspace");
        fs::create_dir(&workdir).unwrap();
        std::os::unix::fs::symlink("/", workdir.join("link")).unwrap();
        let tools = make_tools(&workdir);

        // Precondition: this is textually inside the workdir, so a lexical
        // `starts_with` containment check alone would pass it.
        // Built from `ctx.workdir` rather than `workdir` because the context
        // canonicalizes (on macOS `/var` becomes `/private/var`).
        let normalized = tools.ctx.workdir.join("link/etc/hosts");
        assert!(normalized.starts_with(&tools.ctx.workdir));

        let err = tools.resolve("link/etc/hosts").unwrap_err().to_string();
        assert!(err.contains("symlink"), "got: {err}");

        // And the tool itself refuses rather than returning the file.
        let out = tools.read_file(&json!({ "path": "link/etc/hosts" })).await;
        assert!(out.contains("[error]"), "got: {out}");
    }

    /// A write through an escaping symlink is refused too - this was the path
    /// that could overwrite `~/.ssh/authorized_keys`.
    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("workspace");
        fs::create_dir(&workdir).unwrap();
        std::os::unix::fs::symlink(outside.path(), workdir.join("link")).unwrap();
        let tools = make_tools(&workdir);

        let out = tools
            .write_file(&json!({ "path": "link/pwned.txt", "content": "x" }))
            .await;
        assert!(out.contains("[error]"), "got: {out}");
        assert!(
            !outside.path().join("pwned.txt").exists(),
            "nothing may be written outside the workdir"
        );
    }

    // ── [read_paths]: reads may be granted outside the workdir ────────────

    /// Tools whose context carries a `[read_paths]` policy compiled for
    /// `workdir` (no home, unix path semantics - the platform seams have
    /// their own tests in `leviath_core::read_paths`).
    fn make_tools_with_read_paths(
        workdir: &std::path::Path,
        blueprint: &[&str],
        grants: &[&str],
        allow_blueprint: bool,
    ) -> BuiltinTools {
        let compile = |entries: &[&str]| {
            let raw: Vec<String> = entries.iter().map(|s| s.to_string()).collect();
            leviath_core::ReadPathSet::compile(&raw, workdir, None, false)
                .expect("test entries compile")
        };
        let policy = leviath_core::ReadPathPolicy {
            agent: "tester".into(),
            blueprint: compile(blueprint),
            grants: compile(grants),
            allow_blueprint,
        };
        BuiltinTools::new(ToolContext::new(workdir.to_path_buf()).with_read_paths(policy))
    }

    /// The whole point of the feature: a declared-and-granted directory is
    /// readable, through every read-only tool.
    #[tokio::test]
    async fn read_tools_reach_a_declared_and_granted_outside_path() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("doc.md"), "outside contents").unwrap();
        let entry = outside.path().to_str().unwrap();
        let tools = make_tools_with_read_paths(dir.path(), &[entry], &[entry], false);

        let target = outside.path().join("doc.md");
        let target = target.to_str().unwrap();
        let out = tools.read_file(&json!({ "path": target })).await;
        assert_eq!(out, "outside contents");

        let listed = tools
            .list_dir(&json!({ "path": outside.path().to_str().unwrap() }))
            .await;
        assert!(listed.contains("doc.md"), "got: {listed}");

        // `read_files` mixes inside and outside paths per element.
        fs::write(dir.path().join("inside.txt"), "inside contents").unwrap();
        let out = tools
            .read_files(&json!({ "paths": ["inside.txt", target] }))
            .await;
        assert!(out.contains("inside contents"), "got: {out}");
        assert!(out.contains("outside contents"), "got: {out}");
    }

    /// `[read_paths]` grants reads and nothing else: the same fully granted
    /// path is still refused for `write_file` and `edit_file`, which never
    /// consult the policy.
    #[tokio::test]
    async fn write_and_edit_stay_confined_despite_read_grants() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("doc.md"), "original").unwrap();
        let entry = outside.path().to_str().unwrap();
        let tools = make_tools_with_read_paths(dir.path(), &[entry], &[entry], false);

        let target = outside.path().join("doc.md");
        let target = target.to_str().unwrap();
        let out = tools
            .write_file(&json!({ "path": target, "content": "clobbered" }))
            .await;
        assert!(out.contains("[error]"), "got: {out}");
        let out = tools
            .edit_file(&json!({ "path": target, "old_str": "original", "new_str": "x" }))
            .await;
        assert!(out.contains("[error]"), "got: {out}");
        assert_eq!(
            fs::read_to_string(outside.path().join("doc.md")).unwrap(),
            "original",
            "a read grant must never permit a write"
        );
    }

    /// Declared by the blueprint but granted by nothing: refused, and the
    /// error says exactly which config stanza would grant it.
    #[tokio::test]
    async fn an_ungranted_declaration_is_refused_with_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("doc.md"), "secret").unwrap();
        let entry = outside.path().to_str().unwrap();
        let tools = make_tools_with_read_paths(dir.path(), &[entry], &[], false);

        let target = outside.path().join("doc.md");
        let out = tools
            .read_file(&json!({ "path": target.to_str().unwrap() }))
            .await;
        assert!(out.contains("[error]"), "got: {out}");
        assert!(out.contains("does not grant"), "got: {out}");
        assert!(out.contains("[agent_read_paths.tester]"), "got: {out}");
        assert!(!out.contains("secret"), "content must not leak");
    }

    /// The `allow_blueprint_read_paths` override honors declarations without
    /// itemized grants - and still nothing beyond what is declared.
    #[tokio::test]
    async fn the_blanket_override_honors_declarations() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("doc.md"), "outside contents").unwrap();
        let entry = outside.path().to_str().unwrap();
        let tools = make_tools_with_read_paths(dir.path(), &[entry], &[], true);

        let target = outside.path().join("doc.md");
        let out = tools
            .read_file(&json!({ "path": target.to_str().unwrap() }))
            .await;
        assert_eq!(out, "outside contents");

        // Undeclared stays undeclared: the override widens nothing.
        let undeclared = tempfile::tempdir().unwrap();
        fs::write(undeclared.path().join("x.txt"), "x").unwrap();
        let out = tools
            .read_file(&json!({ "path": undeclared.path().join("x.txt").to_str().unwrap() }))
            .await;
        assert!(
            out.contains("not in this agent's [read_paths]"),
            "got: {out}"
        );
    }

    /// With no `[read_paths]` at all, an outside read gets the original
    /// workdir refusal, word for word - the policy is never consulted.
    #[tokio::test]
    async fn an_inactive_policy_keeps_the_workdir_error() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let out = tools.read_file(&json!({ "path": "/etc/hosts" })).await;
        assert!(
            out.contains("would escape the working directory"),
            "got: {out}"
        );
    }

    /// A relative request resolves against the workdir in the fallback too,
    /// so a workdir-relative entry like `../shared` is reachable by the
    /// matching relative request.
    #[tokio::test]
    async fn a_relative_request_reaches_a_relative_grant() {
        let parent = tempfile::tempdir().unwrap();
        let workdir = parent.path().join("work");
        let shared = parent.path().join("shared");
        fs::create_dir_all(&workdir).unwrap();
        fs::create_dir_all(&shared).unwrap();
        fs::write(shared.join("doc.md"), "shared contents").unwrap();
        let tools = make_tools_with_read_paths(&workdir, &["../shared"], &["../shared"], false);

        let out = tools
            .read_file(&json!({ "path": "../shared/doc.md" }))
            .await;
        assert_eq!(out, "shared contents");
    }

    /// An interior `.` in a fallback request is folded away (`Path::components`
    /// drops it), so `<granted>/./doc.md` resolves the same as
    /// `<granted>/doc.md`.
    #[tokio::test]
    async fn a_dot_component_is_folded_in_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("doc.md"), "outside contents").unwrap();
        let entry = outside.path().to_str().unwrap();
        let tools = make_tools_with_read_paths(dir.path(), &[entry], &[entry], false);

        let target = format!("{}/./doc.md", outside.path().to_str().unwrap());
        let out = tools.read_file(&json!({ "path": target })).await;
        assert_eq!(out, "outside contents");
    }

    /// Folding `..` past the top is unresolvable no matter what any allowlist
    /// says. Mirrors `resolve_rejects_excessive_parent_dir_traversal`: a
    /// *relative* base (`wd`) gives the accumulator exactly one leading
    /// `Normal` component and no platform-specific root/drive/prefix, so the
    /// first `..` pops `wd` and the second calls `pop()` on an empty
    /// accumulator - firing the bail on every OS. `/..` or an empty base does
    /// not: neither is absolute on Windows, and the join reshapes them so the
    /// `pop()` never fails there.
    #[test]
    fn folding_past_the_root_is_unresolvable() {
        let policy = leviath_core::ReadPathPolicy {
            agent: "tester".into(),
            allow_blueprint: true,
            ..Default::default()
        };
        let err = BuiltinTools::resolve_outside(
            "../../x",
            Path::new("wd"),
            &policy,
            leviath_core::canonicalize_for_match,
        )
        .expect_err("popping past the top must be refused");
        assert!(err.to_string().contains("cannot be resolved"), "{err}");
    }

    /// The fail-closed arm, driven through the injected canonicalizer so it
    /// runs on every platform: a path nothing can verify is refused, never
    /// matched.
    #[test]
    fn an_unverifiable_path_is_refused() {
        fn unverifiable(_: &Path) -> Option<PathBuf> {
            None
        }
        let policy = leviath_core::ReadPathPolicy {
            agent: "tester".into(),
            allow_blueprint: true,
            ..Default::default()
        };
        let err =
            BuiltinTools::resolve_outside("/outside/x", Path::new("/w"), &policy, unverifiable)
                .expect_err("an unverifiable path must be refused");
        assert!(err.to_string().contains("cannot be verified"), "{err}");
    }

    /// The attack the policy exists to stop: a symlink planted *inside* a
    /// granted directory, pointing outside it. The policy sees the real
    /// target, which no entry declares.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_inside_a_granted_directory_cannot_escape_it() {
        let dir = tempfile::tempdir().unwrap();
        let granted = tempfile::tempdir().unwrap();
        let secret_home = tempfile::tempdir().unwrap();
        fs::write(secret_home.path().join("id_rsa"), "PRIVATE KEY").unwrap();
        std::os::unix::fs::symlink(
            secret_home.path().join("id_rsa"),
            granted.path().join("innocent.md"),
        )
        .unwrap();
        let entry = granted.path().to_str().unwrap();
        let tools = make_tools_with_read_paths(dir.path(), &[entry], &[entry], false);

        let out = tools
            .read_file(&json!({ "path": granted.path().join("innocent.md").to_str().unwrap() }))
            .await;
        assert!(out.contains("[error]"), "got: {out}");
        assert!(!out.contains("PRIVATE KEY"), "content must not leak");
    }

    /// The same attack against a glob entry - the variant the original PR
    /// missed entirely. The pattern is matched against the symlink-resolved
    /// real path, and the real target does not match it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_glob_grant_is_symlink_safe() {
        let dir = tempfile::tempdir().unwrap();
        let granted = tempfile::tempdir().unwrap();
        let secret_home = tempfile::tempdir().unwrap();
        fs::write(secret_home.path().join("id_rsa"), "PRIVATE KEY").unwrap();
        std::os::unix::fs::symlink(
            secret_home.path().join("id_rsa"),
            granted.path().join("innocent.md"),
        )
        .unwrap();
        // Patterns match the canonical real path, so build the entry from it.
        let canonical = fs::canonicalize(granted.path()).unwrap();
        let entry = format!("glob:{}/**", canonical.display());
        let tools = make_tools_with_read_paths(dir.path(), &[&entry], &[&entry], false);

        let out = tools
            .read_file(&json!({ "path": granted.path().join("innocent.md").to_str().unwrap() }))
            .await;
        assert!(out.contains("[error]"), "got: {out}");
        assert!(!out.contains("PRIVATE KEY"), "content must not leak");

        // The positive pair: a real file under the same glob is readable, so
        // the refusal above is the symlink and not the pattern.
        fs::write(granted.path().join("real.md"), "real contents").unwrap();
        let out = tools
            .read_file(&json!({ "path": granted.path().join("real.md").to_str().unwrap() }))
            .await;
        assert_eq!(out, "real contents");
    }

    /// A symlink whose target stays inside the granted subtree is fine - the
    /// rule is about where the path lands, exactly as in the workdir.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_within_a_granted_directory_is_readable() {
        let dir = tempfile::tempdir().unwrap();
        let granted = tempfile::tempdir().unwrap();
        fs::create_dir(granted.path().join("real")).unwrap();
        fs::write(granted.path().join("real/doc.md"), "granted contents").unwrap();
        std::os::unix::fs::symlink(granted.path().join("real"), granted.path().join("link"))
            .unwrap();
        let entry = granted.path().to_str().unwrap();
        let tools = make_tools_with_read_paths(dir.path(), &[entry], &[entry], false);

        let out = tools
            .read_file(&json!({ "path": granted.path().join("link/doc.md").to_str().unwrap() }))
            .await;
        assert_eq!(out, "granted contents");
    }

    /// A symlink that stays *inside* the workdir keeps working - the rule is
    /// about where the path lands, not whether a symlink was involved. Agents
    /// operate on real repositories, which contain plenty of internal symlinks.
    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_allows_symlink_within_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("workspace");
        fs::create_dir(&workdir).unwrap();
        fs::create_dir(workdir.join("real")).unwrap();
        fs::write(workdir.join("real/file.txt"), "contents").unwrap();
        std::os::unix::fs::symlink(workdir.join("real"), workdir.join("link")).unwrap();
        let tools = make_tools(&workdir);

        assert!(tools.resolve("link/file.txt").is_ok());
        let out = tools.read_file(&json!({ "path": "link/file.txt" })).await;
        assert_eq!(out, "contents");
    }

    #[test]
    fn resolve_dot_stays_in_workdir() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let result = tools.resolve("./foo/./bar.txt").unwrap();
        assert!(result.starts_with(&tools.ctx.workdir));
        assert!(result.ends_with("foo/bar.txt"));
    }

    // ── execute() with file I/O (async) ───────────────────────────────────

    #[tokio::test]
    async fn execute_unknown_tool_returns_error() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let result = tools.execute("nonexistent", json!({})).await;
        assert!(result.contains("[error]"));
        assert!(result.contains("Unknown built-in tool"));
    }

    #[tokio::test]
    async fn read_file_missing_path_arg() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let result = tools.execute("read_file", json!({})).await;
        assert!(result.contains("[error]"));
        assert!(result.contains("missing 'path'"));
    }

    #[tokio::test]
    async fn write_and_read_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        let write_result = tools
            .execute(
                "write_file",
                json!({"path": "test.txt", "content": "hello world"}),
            )
            .await;
        assert!(write_result.contains("Successfully wrote"));
        assert!(write_result.contains("11 bytes"));

        let read_result = tools
            .execute("read_file", json!({"path": "test.txt"}))
            .await;
        assert_eq!(read_result, "hello world");
    }

    #[tokio::test]
    async fn write_file_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        let result = tools
            .execute(
                "write_file",
                json!({"path": "sub/dir/file.txt", "content": "nested"}),
            )
            .await;
        assert!(result.contains("Successfully wrote"));
        assert!(dir.path().join("sub/dir/file.txt").exists());
    }

    #[tokio::test]
    async fn write_tools_refuse_to_resurrect_a_deleted_workspace() {
        // Issue #107: an external harness deletes the workspace mid-run.
        // `create_dir_all` would happily recreate it and let the agent write
        // into an empty tree that no longer resembles the checkout it reasoned
        // about - and the runtime's health check, which just stats the workdir,
        // would never see it was gone.
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("workspace");
        fs::create_dir(&workdir).unwrap();
        fs::write(workdir.join("a.txt"), "before").unwrap();
        let tools = make_tools(&workdir);
        fs::remove_dir_all(&workdir).unwrap();

        for (tool, args) in [
            ("write_file", json!({"path": "a.txt", "content": "after"})),
            (
                "edit_file",
                json!({"path": "a.txt", "old_str": "before", "new_str": "after"}),
            ),
        ] {
            let result = tools.execute(tool, args).await;
            assert!(
                result.contains("workspace") && result.contains("no longer accessible"),
                "{tool} got: {result}"
            );
        }
        assert!(!workdir.exists(), "the workspace must stay gone");
    }

    #[tokio::test]
    async fn write_file_missing_content_arg() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("write_file", json!({"path": "f.txt"})).await;
        assert!(result.contains("missing 'content'"));
    }

    #[tokio::test]
    async fn write_file_missing_path_arg() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("write_file", json!({"content": "x"})).await;
        assert!(result.contains("missing 'path'"));
    }

    #[test]
    fn resolve_rejects_excessive_parent_dir_traversal() {
        // A *relative, nonexistent* workdir keeps `resolve`'s accumulator free
        // of any platform-specific leading root/drive/prefix components:
        // `canonicalize` fails for a path that doesn't exist (on every OS), so
        // `ToolContext::new` keeps the raw relative `PathBuf` verbatim. The
        // request then decomposes into exactly `[Normal(workdir), ParentDir,
        // ParentDir, ...]`; the first `..` pops the single workdir component and
        // the second `..` calls `normalized.pop()` on an *empty* accumulator,
        // which returns `false` - firing the "escapes the working directory"
        // bail deterministically on every OS.
        //
        // (An empty "" workdir is not portable here: on Windows `canonicalize("")`
        // can succeed and yield an absolute cwd whose Prefix/RootDir components
        // absorb the `..`, so `pop()` never fails and this bail is never hit --
        // which is exactly why this branch was Windows-uncovered before.)
        let tools = BuiltinTools::new(ToolContext::new(PathBuf::from(
            "leviath-nonexistent-relative-workdir",
        )));
        let result = tools.resolve("../../etc/passwd");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("escapes the working directory")
        );
    }

    #[tokio::test]
    async fn edit_file_successful_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        tools
            .execute(
                "write_file",
                json!({"path": "e.txt", "content": "foo bar baz"}),
            )
            .await;

        let result = tools
            .execute(
                "edit_file",
                json!({"path": "e.txt", "old_str": "bar", "new_str": "qux"}),
            )
            .await;
        assert!(result.contains("Successfully edited"));

        let content = tools.execute("read_file", json!({"path": "e.txt"})).await;
        assert_eq!(content, "foo qux baz");
    }

    #[tokio::test]
    async fn edit_file_string_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        tools
            .execute("write_file", json!({"path": "e.txt", "content": "abc"}))
            .await;

        let result = tools
            .execute(
                "edit_file",
                json!({"path": "e.txt", "old_str": "xyz", "new_str": "123"}),
            )
            .await;
        assert!(result.contains("String not found"));
    }

    #[tokio::test]
    async fn edit_file_missing_file_returns_read_error() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        let result = tools
            .execute(
                "edit_file",
                json!({"path": "does-not-exist.txt", "old_str": "a", "new_str": "b"}),
            )
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("Failed to read"));
    }

    #[tokio::test]
    async fn edit_file_multiple_occurrences() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        tools
            .execute("write_file", json!({"path": "e.txt", "content": "aaa aaa"}))
            .await;

        let result = tools
            .execute(
                "edit_file",
                json!({"path": "e.txt", "old_str": "aaa", "new_str": "bbb"}),
            )
            .await;
        assert!(result.contains("2 occurrences"));
        assert!(result.contains("must be unique"));
    }

    #[tokio::test]
    async fn edit_file_missing_args() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        let r1 = tools.execute("edit_file", json!({})).await;
        assert!(r1.contains("missing 'path'"));

        let r2 = tools.execute("edit_file", json!({"path": "f.txt"})).await;
        assert!(r2.contains("missing 'old_str'"));

        let r3 = tools
            .execute("edit_file", json!({"path": "f.txt", "old_str": "x"}))
            .await;
        assert!(r3.contains("missing 'new_str'"));
    }

    #[tokio::test]
    async fn list_dir_contents() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        fs::create_dir(dir.path().join("subdir")).unwrap();

        let result = tools.execute("list_dir", json!({})).await;
        assert!(result.contains("a.txt"));
        assert!(result.contains("subdir/"));
    }

    #[tokio::test]
    async fn list_dir_empty() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("list_dir", json!({})).await;
        assert!(result.contains("empty directory"));
    }

    #[tokio::test]
    async fn list_dir_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/inner.txt"), "data").unwrap();

        let result = tools.execute("list_dir", json!({"path": "sub"})).await;
        assert!(result.contains("inner.txt"));
    }

    #[tokio::test]
    async fn read_file_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools
            .execute("read_file", json!({"path": "nope.txt"}))
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("Failed to read"));
    }

    // ── read_files (batch reads) ────────────────────────────────────────────

    #[tokio::test]
    async fn read_files_multiple_valid_files() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        fs::write(dir.path().join("b.txt"), "beta").unwrap();

        let result = tools
            .execute("read_files", json!({"paths": ["a.txt", "b.txt"]}))
            .await;
        assert!(result.contains("### [a.txt]"));
        assert!(result.contains("alpha"));
        assert!(result.contains("### [b.txt]"));
        assert!(result.contains("beta"));
        // Results are joined with a blank line between entries.
        assert!(result.contains("\n\n"));
    }

    #[tokio::test]
    async fn read_files_missing_paths_arg() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("read_files", json!({})).await;
        assert!(result.contains("[error]"));
        assert!(result.contains("missing 'paths'"));
    }

    #[tokio::test]
    async fn read_files_non_array_paths_arg() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        // A string (not an array) → as_array() returns None → same error path.
        let result = tools.execute("read_files", json!({"paths": "a.txt"})).await;
        assert!(result.contains("[error]"));
        assert!(result.contains("missing 'paths'"));
    }

    #[tokio::test]
    async fn read_files_empty_paths_array() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("read_files", json!({"paths": []})).await;
        assert!(result.contains("[error]"));
        assert!(result.contains("empty"));
    }

    #[tokio::test]
    async fn read_files_missing_file_reports_per_file_error() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        fs::write(dir.path().join("present.txt"), "here").unwrap();

        let result = tools
            .execute(
                "read_files",
                json!({"paths": ["present.txt", "absent.txt"]}),
            )
            .await;
        // Valid file still returned…
        assert!(result.contains("### [present.txt]"));
        assert!(result.contains("here"));
        // …while the missing one produces a per-file error under its header.
        assert!(result.contains("### [absent.txt]"));
        assert!(result.contains("Failed to read"));
    }

    #[tokio::test]
    async fn read_files_non_string_element_reports_error() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        fs::write(dir.path().join("ok.txt"), "content").unwrap();

        let result = tools
            .execute("read_files", json!({"paths": ["ok.txt", 42]}))
            .await;
        assert!(result.contains("content"));
        assert!(result.contains("non-string path in array"));
    }

    #[tokio::test]
    async fn read_files_path_escape_reported_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools
            .execute("read_files", json!({"paths": ["../../etc/passwd"]}))
            .await;
        assert!(result.contains("### [../../etc/passwd]"));
        assert!(result.contains("escape"));
    }

    // ── resolve() absolute paths ────────────────────────────────────────────

    #[test]
    fn resolve_absolute_path_inside_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        // Build the absolute path from the tool's own (canonicalized) workdir
        // rather than `dir.path()` directly - on macOS `/tmp`/`/var` are
        // symlinks, so the two can differ even though they're the same place.
        let abs = tools.ctx.workdir.join("inside.txt");
        let result = tools.resolve(abs.to_str().unwrap()).unwrap();
        assert_eq!(result, abs);
    }

    #[test]
    fn resolve_rejects_absolute_path_outside_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.resolve("/etc/passwd");
        assert!(result.is_err());
    }

    // ── path-escape rejection propagates through each tool ─────────────────

    #[tokio::test]
    async fn read_file_path_escape_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools
            .execute("read_file", json!({"path": "../../etc/passwd"}))
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("escape"));
    }

    #[tokio::test]
    async fn write_file_path_escape_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools
            .execute(
                "write_file",
                json!({"path": "../../evil.txt", "content": "x"}),
            )
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("escape"));
    }

    #[tokio::test]
    async fn edit_file_path_escape_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools
            .execute(
                "edit_file",
                json!({"path": "../../evil.txt", "old_str": "a", "new_str": "b"}),
            )
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("escape"));
    }

    #[tokio::test]
    async fn list_dir_path_escape_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("list_dir", json!({"path": "../../"})).await;
        assert!(result.contains("[error]"));
        assert!(result.contains("escape"));
    }

    // ── filesystem failure branches ─────────────────────────────────────────

    #[tokio::test]
    async fn write_file_fails_when_path_is_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        fs::create_dir(dir.path().join("adir")).unwrap();

        let result = tools
            .execute("write_file", json!({"path": "adir", "content": "x"}))
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("Failed to write"));
    }

    #[tokio::test]
    async fn write_file_parent_dir_creation_fails_when_blocked_by_file() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        // "blocker" exists as a plain file, so create_dir_all("blocker") must fail.
        fs::write(dir.path().join("blocker"), "im a file").unwrap();

        let result = tools
            .execute(
                "write_file",
                json!({"path": "blocker/nested.txt", "content": "x"}),
            )
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("Failed to create directories"));
    }

    #[tokio::test]
    async fn read_file_fails_when_path_is_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        fs::create_dir(dir.path().join("adir")).unwrap();

        let result = tools.execute("read_file", json!({"path": "adir"})).await;
        assert!(result.contains("[error]"));
        assert!(result.contains("Failed to read"));
    }

    #[tokio::test]
    async fn list_dir_fails_when_path_is_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        fs::write(dir.path().join("afile.txt"), "content").unwrap();

        let result = tools
            .execute("list_dir", json!({"path": "afile.txt"}))
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("Failed to read directory"));
    }

    // `set_readonly(false)` widens Unix perms beyond the original, but here it
    // only re-enables cleanup of a throwaway tempdir file, which is exactly
    // what we want.
    #[allow(clippy::permissions_set_readonly_false)]
    #[tokio::test]
    async fn edit_file_write_failure_after_successful_match() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let file_path = dir.path().join("ro.txt");
        fs::write(&file_path, "hello world").unwrap();

        // Make the file read-only so the read succeeds but the write-back
        // fails. `set_readonly(true)` is cross-platform (clears the write bits
        // on Unix; sets the read-only attribute on Windows), so the write
        // error arm is exercised on every OS.
        let mut perms = fs::metadata(&file_path).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&file_path, perms).unwrap();

        let result = tools
            .execute(
                "edit_file",
                json!({"path": "ro.txt", "old_str": "hello", "new_str": "goodbye"}),
            )
            .await;

        // Restore permissions so tempdir cleanup can remove the file.
        let mut perms = fs::metadata(&file_path).unwrap().permissions();
        perms.set_readonly(false);
        fs::set_permissions(&file_path, perms).unwrap();

        assert!(result.contains("[error]"));
        assert!(result.contains("Failed to write"));
    }

    #[tokio::test]
    async fn shell_echo_command() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools
            .execute("shell", json!({"command": "echo hello"}))
            .await;
        assert!(result.trim().contains("hello"));
    }

    #[tokio::test]
    async fn bash_alias_works() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools
            .execute("bash", json!({"command": "echo alias_test"}))
            .await;
        assert!(result.contains("alias_test"));
    }

    #[tokio::test]
    async fn shell_missing_command_arg() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("shell", json!({})).await;
        assert!(result.contains("missing 'command'"));
    }

    /// A `ShellExecutor` that ignores the requested command and instead runs a
    /// fixed marker command - proof that shell execution is routed through it.
    struct RedirectExecutor;
    impl ShellExecutor for RedirectExecutor {
        fn build_command(
            &self,
            shell: &str,
            flag: &str,
            _command: &str,
            workdir: &Path,
        ) -> Command {
            let mut c = Command::new(shell);
            c.arg(flag).arg("echo SANDBOXED").current_dir(workdir);
            c
        }
    }

    #[tokio::test]
    async fn shell_routes_through_executor_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let tools = BuiltinTools::new(ToolContext::new(dir.path().to_path_buf()))
            .with_shell_executor(Arc::new(RedirectExecutor));
        // The agent asked for `echo host`, but the executor redirects it.
        let result = tools
            .execute("shell", json!({"command": "echo host"}))
            .await;
        assert!(result.contains("SANDBOXED"), "got: {result}");
        assert!(!result.contains("host"));
    }

    #[tokio::test]
    async fn shell_failing_command() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("shell", json!({"command": "false"})).await;
        assert!(result.contains("[exit code"));
    }

    #[tokio::test]
    async fn shell_successful_command_with_no_output() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("shell", json!({"command": "true"})).await;
        assert_eq!(result, "(command succeeded with no output)");
    }

    // The stdout+stderr non-zero-exit formatting is asserted directly against
    // `format_command_output` (below) rather than via a real shell command:
    // producing stdout, stderr, and a non-zero exit in a single command needs
    // shell-specific syntax (`;`/`1>&2` on `sh`, `&`/redirection on `cmd.exe`)
    // that isn't portable, and this session already hit real Windows CI
    // failures from insufficiently-verified platform-specific test commands.
    #[test]
    fn format_command_output_non_zero_exit_reports_stdout_and_stderr() {
        let result = BuiltinTools::format_command_output(b"out-line\n", b"err-line\n", false, 1);
        assert!(result.contains("[exit code 1]"));
        assert!(result.contains("stdout:"));
        assert!(result.contains("out-line"));
        assert!(result.contains("stderr:"));
        assert!(result.contains("err-line"));
    }

    #[test]
    fn format_command_output_non_zero_exit_omits_empty_streams() {
        // Whitespace-only streams are treated as empty and neither the
        // stdout: nor stderr: block is emitted.
        let result = BuiltinTools::format_command_output(b"   \n", b"", false, 2);
        assert_eq!(result, "[exit code 2]\n");
    }

    #[test]
    fn format_command_output_success_with_output_returns_stdout() {
        let result = BuiltinTools::format_command_output(b"hello\n", b"", true, 0);
        assert_eq!(result, "hello\n");
    }

    #[test]
    fn format_command_output_success_no_output() {
        let result = BuiltinTools::format_command_output(b"   ", b"noise", true, 0);
        assert_eq!(result, "(command succeeded with no output)");
    }

    #[tokio::test]
    async fn shell_with_timeout_fires_on_slow_command() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools
            .shell_with_timeout(&json!({"command": "sleep 5"}), Duration::from_millis(100))
            .await;
        assert!(result.contains("[timed out]"));
    }

    /// A timed-out (or cancelled) command takes its *grandchildren* with it.
    ///
    /// `kill_on_drop` only reaps the shell. Anything the shell started is
    /// reparented to init and keeps running - a cancelled agent's `sleep`
    /// outliving the run that spawned it. Verified by writing a marker file
    /// after a delay: if the grandchild survived, the marker appears.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_timed_out_command_kills_its_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let tools = make_tools(dir.path());

        // A *backgrounded subshell* is the grandchild, and it is what writes the
        // marker. Chaining (`sleep 2 && touch`) would not test anything: the
        // `touch` is run by the shell itself, so killing the shell suppresses it
        // whether or not the group was signalled.
        let cmd = format!("( sleep 2; touch {} ) & sleep 30", marker.display());
        let result = tools
            .shell_with_timeout(&json!({ "command": cmd }), Duration::from_millis(100))
            .await;
        assert!(result.contains("[timed out]"), "got: {result}");

        // Well past when the grandchild would have written it.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !marker.exists(),
            "the grandchild outlived the command that started it"
        );
    }

    #[tokio::test]
    async fn shell_spawn_failure_when_workdir_missing() {
        // A workdir that doesn't exist on disk makes Command::output() fail
        // before the shell ever runs (current_dir() can't chdir into it).
        // canonicalize() fails for a nonexistent path, so ToolContext::new()
        // falls back to keeping the raw (nonexistent) path as-is.
        let tools = make_tools(std::path::Path::new(
            "/definitely/does/not/exist/leviath-test",
        ));
        let result = tools.execute("shell", json!({"command": "echo hi"})).await;
        assert!(result.contains("[error]"));
        assert!(result.contains("Failed to spawn shell"));
    }

    // ── ToolContext ────────────────────────────────────────────────────────

    #[test]
    fn tool_context_new_canonicalizes() {
        let dir = std::env::temp_dir();
        let ctx = ToolContext::new(dir.clone());
        // Canonicalized path should be absolute
        assert!(ctx.workdir.is_absolute());
    }

    #[test]
    fn tool_context_new_with_nonexistent_dir() {
        let ctx = ToolContext::new(PathBuf::from("/nonexistent/path/unlikely"));
        // Falls back to the original path when canonicalization fails
        assert_eq!(ctx.workdir, PathBuf::from("/nonexistent/path/unlikely"));
    }

    // ── detect_shell ──────────────────────────────────────────────────────

    /// Windows' `detect_shell()` branch is a plain, unconditional constant
    /// return (no env/filesystem dependence to inject) - the
    /// platform-agnostic `detect_shell_returns_valid_shell` test below
    /// already exercises it on Windows CI, but this asserts the exact
    /// documented return value directly.
    #[cfg(windows)]
    #[test]
    fn detect_shell_returns_cmd_exe() {
        let (shell, flag) = BuiltinTools::detect_shell();
        assert_eq!(shell, "cmd.exe");
        assert_eq!(flag, "/C");
    }

    #[test]
    fn detect_shell_returns_valid_shell() {
        // Pure reader: `detect_shell()` always returns a non-empty shell (and the
        // "-c" flag on non-Windows) regardless of $SHELL, so it is robust to a
        // concurrent temp-env writer and needs no serialization of its own.
        let (shell, flag) = BuiltinTools::detect_shell();
        assert!(!shell.is_empty());
        assert!(!flag.is_empty());
        #[cfg(not(windows))]
        assert_eq!(flag, "-c");
    }

    /// Forces `detect_shell()` to exercise the real `shell_exists` closure by
    /// temporarily setting $SHELL to an unrecognized path, causing the candidate
    /// loop (and the closure) to be reached. `temp_env::with_var` sets the var,
    /// runs the closure, and restores it - serialized against every other
    /// temp-env test process-wide, so no hand-rolled lock is needed.
    #[cfg(not(windows))]
    #[test]
    fn detect_shell_queries_real_filesystem_for_unrecognized_shell() {
        let (shell, flag) =
            temp_env::with_var("SHELL", Some("/opt/not-a-recognized-shell"), || {
                BuiltinTools::detect_shell()
            });
        assert_eq!(flag, "-c");
        assert!(
            [
                "/bin/bash",
                "/usr/bin/bash",
                "/bin/zsh",
                "/usr/bin/zsh",
                "/bin/sh"
            ]
            .contains(&shell)
        );
    }

    // ── detect_shell_impl() - inject env and filesystem for full branch coverage ──

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_returns_zsh_from_env() {
        // `$SHELL` is trusted only when it exists on disk.
        let (shell, flag) =
            BuiltinTools::detect_shell_impl(Some("/usr/local/bin/zsh".to_string()), &|s| {
                s == "/usr/local/bin/zsh"
            });
        assert_eq!(shell, "/usr/local/bin/zsh");
        assert_eq!(flag, "-c");
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_returns_bash_from_env() {
        let (shell, flag) =
            BuiltinTools::detect_shell_impl(Some("/usr/local/bin/bash".to_string()), &|s| {
                s == "/usr/local/bin/bash"
            });
        assert_eq!(shell, "/usr/local/bin/bash");
        assert_eq!(flag, "-c");
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_returns_sh_from_env() {
        let (shell, flag) =
            BuiltinTools::detect_shell_impl(Some("/usr/bin/sh".to_string()), &|s| {
                s == "/usr/bin/sh"
            });
        assert_eq!(shell, "/usr/bin/sh");
        assert_eq!(flag, "-c");
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_falls_back_when_env_shell_is_missing() {
        // Regression for #79: `$SHELL` is a recognized shell name but does not
        // exist on disk (a stale or sandbox-missing `/bin/zsh`). It must NOT be
        // returned - fall through to an available fallback instead of failing
        // every shell call with "No such file or directory".
        let (shell, flag) =
            BuiltinTools::detect_shell_impl(Some("/bin/zsh".to_string()), &|s| s == "/bin/sh");
        assert_eq!(shell, "/bin/sh");
        assert_eq!(flag, "-c");
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_falls_through_when_env_unrecognized() {
        // /opt/fish doesn't end with /zsh, /bash, or /sh → falls to candidate loop
        let (shell, flag) =
            BuiltinTools::detect_shell_impl(Some("/opt/fish".to_string()), &|s| s == "/bin/bash");
        assert_eq!(shell, "/bin/bash");
        assert_eq!(flag, "-c");
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_skips_missing_candidates_and_finds_zsh() {
        // bash paths return false; /bin/zsh exists - covers shell_exists false branch
        let (shell, flag) = BuiltinTools::detect_shell_impl(None, &|s| s == "/bin/zsh");
        assert_eq!(shell, "/bin/zsh");
        assert_eq!(flag, "-c");
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_returns_last_resort_when_nothing_exists() {
        let (shell, flag) = BuiltinTools::detect_shell_impl(None, &|_| false);
        assert_eq!(shell, "sh");
        assert_eq!(flag, "-c");
    }

    #[tokio::test]
    async fn concurrent_edits_same_file_serialize_no_lost_update() {
        // Two workers edit different unique strings in the SAME file at once.
        // The per-path lock serializes the read-modify-write, so both edits
        // land; without it, the second write would clobber the first.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "A\nB\n").unwrap();
        let tools = std::sync::Arc::new(make_tools(dir.path()));

        let t1 = {
            let t = tools.clone();
            tokio::spawn(async move {
                t.execute(
                    "edit_file",
                    json!({"path": "f.txt", "old_str": "A", "new_str": "A1"}),
                )
                .await
            })
        };
        let t2 = {
            let t = tools.clone();
            tokio::spawn(async move {
                t.execute(
                    "edit_file",
                    json!({"path": "f.txt", "old_str": "B", "new_str": "B2"}),
                )
                .await
            })
        };
        let (r1, r2) = tokio::join!(t1, t2);
        assert!(!r1.unwrap().starts_with("[error]"));
        assert!(!r2.unwrap().starts_with("[error]"));

        let final_content = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert_eq!(
            final_content, "A1\nB2\n",
            "both concurrent edits must apply (no lost update)"
        );
    }

    #[tokio::test]
    async fn concurrent_writes_different_files_both_succeed() {
        // Different files never contend on the per-path lock.
        let dir = tempfile::tempdir().unwrap();
        let tools = std::sync::Arc::new(make_tools(dir.path()));

        let a = {
            let t = tools.clone();
            tokio::spawn(async move {
                t.execute("write_file", json!({"path": "a.txt", "content": "AAA"}))
                    .await
            })
        };
        let b = {
            let t = tools.clone();
            tokio::spawn(async move {
                t.execute("write_file", json!({"path": "b.txt", "content": "BBB"}))
                    .await
            })
        };
        let (ra, rb) = tokio::join!(a, b);
        assert!(!ra.unwrap().starts_with("[error]"));
        assert!(!rb.unwrap().starts_with("[error]"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "AAA"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "BBB"
        );
    }

    // ── Platform capabilities ─────────────────────────────────────────────

    #[test]
    fn desktop_supports_all_capabilities() {
        let caps = PlatformCapabilities::desktop();
        assert!(caps.supports(ToolCapability::ProcessSpawn));
        assert!(caps.supports(ToolCapability::FileSystem));
        assert!(caps.supports(ToolCapability::Network));
    }

    #[test]
    fn mobile_lacks_process_spawn() {
        let caps = PlatformCapabilities::mobile();
        assert!(!caps.supports(ToolCapability::ProcessSpawn));
        assert!(caps.supports(ToolCapability::FileSystem));
        assert!(caps.supports(ToolCapability::Network));
    }

    #[test]
    fn current_matches_desktop_and_is_the_default() {
        // Only desktop targets are built today.
        assert_eq!(
            PlatformCapabilities::current(),
            PlatformCapabilities::desktop()
        );
        assert_eq!(
            PlatformCapabilities::default(),
            PlatformCapabilities::desktop()
        );
    }

    #[test]
    fn satisfies_requires_all_and_empty_is_always_met() {
        let caps = PlatformCapabilities::mobile();
        assert!(caps.satisfies(&[]));
        assert!(caps.satisfies(&[ToolCapability::FileSystem]));
        assert!(!caps.satisfies(&[ToolCapability::ProcessSpawn]));
        // All-or-nothing: one unmet requirement fails the whole set.
        assert!(!caps.satisfies(&[ToolCapability::FileSystem, ToolCapability::ProcessSpawn]));
    }

    #[test]
    fn from_capabilities_builds_explicit_set() {
        let caps = PlatformCapabilities::from_capabilities([ToolCapability::Network]);
        assert!(caps.supports(ToolCapability::Network));
        assert!(!caps.supports(ToolCapability::FileSystem));
    }

    #[test]
    fn tool_required_capabilities_by_name() {
        assert_eq!(
            tool_required_capabilities("shell"),
            &[ToolCapability::ProcessSpawn]
        );
        assert_eq!(
            tool_required_capabilities("read_file"),
            &[ToolCapability::FileSystem]
        );
        // Runtime-handled / platform-agnostic tools require nothing.
        assert!(tool_required_capabilities("context_write").is_empty());
        assert!(tool_required_capabilities("present_for_review").is_empty());
        assert!(tool_required_capabilities("unknown_tool").is_empty());
    }

    #[test]
    fn mobile_tool_defs_omit_shell_but_keep_the_rest() {
        let dir = std::env::temp_dir();
        let tools = make_mobile_tools(&dir);
        let names: Vec<String> = tools.tool_defs().iter().map(|t| t.name.clone()).collect();
        assert!(!names.contains(&"shell".to_string()));
        // The other 15 built-ins remain.
        assert_eq!(tools.tool_defs().len(), 15);
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"context_write".to_string()));
        assert!(names.contains(&"present_for_review".to_string()));
    }

    #[test]
    fn desktop_tool_defs_include_shell() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let names: Vec<String> = tools.tool_defs().iter().map(|t| t.name.clone()).collect();
        assert!(names.contains(&"shell".to_string()));
    }

    #[test]
    fn mobile_names_omit_shell_and_bash_alias() {
        let dir = std::env::temp_dir();
        let tools = make_mobile_tools(&dir);
        let names = tools.names();
        assert!(!names.contains(&"shell".to_string()));
        assert!(!names.contains(&"bash".to_string()));
        // File + context tools still recognized.
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"context_write".to_string()));
    }

    #[test]
    fn desktop_names_include_shell_and_bash_alias() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let names = tools.names();
        assert!(names.contains(&"shell".to_string()));
        assert!(names.contains(&"bash".to_string()));
    }

    #[tokio::test]
    async fn mobile_execute_shell_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_mobile_tools(dir.path());
        let out = tools.execute("shell", json!({"command": "echo hi"})).await;
        assert!(out.contains("not available on this platform"), "got: {out}");
        // The `bash` alias resolves to `shell` and is rejected the same way.
        let out = tools.execute("bash", json!({"command": "echo hi"})).await;
        assert!(out.contains("not available on this platform"), "got: {out}");
    }

    #[tokio::test]
    async fn mobile_execute_file_tool_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_mobile_tools(dir.path());
        let out = tools
            .execute("write_file", json!({"path": "x.txt", "content": "hi"}))
            .await;
        assert!(!out.starts_with("[error]"), "got: {out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("x.txt")).unwrap(),
            "hi"
        );
    }
}

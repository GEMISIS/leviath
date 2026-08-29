//! Execution of `seed = { tools = [...] }` region seeds.
//!
//! A tool seed calls the run's own tools at spawn and writes their combined
//! output into a context region, so an agent starts its first inference already
//! knowing what the tools would have told it. The motivating case is the clock:
//! a research agent that cannot ask the date reasons from its training cutoff,
//! and a tool it is never prompted to call does not help.
//!
//! # Why any tool is allowed here
//!
//! Unlike a `command` seed, which runs a shell line and therefore needs the
//! `[safe_commands]` list and the `allow_seed_commands` switch to hem it in, a
//! tool seed reaches nothing new: every call goes through
//! [`crate::tools::resolve_policy`], the same resolution the tool lane applies
//! mid-run, so a user's `[tool_permissions]` decides exactly as it would there.
//! A seed cannot call what the agent could not call.
//!
//! That is what lets the list be open. A built-in, an MCP server's tool, a Rhai
//! script tool - all are callable, spelled as the agent would spell them.
//!
//! # Why `ask` cannot run
//!
//! A seed runs before the first inference and therefore before any approval
//! prompt exists; there is nobody to ask. Rather than silently escalating an
//! `ask` to an allow (which would make seeding a way around the permission
//! layer) or blocking forever, an `ask` tool is refused with a message naming
//! the setting that would let it run. `allow` runs, `deny` is refused, and both
//! read the same as they would mid-run.

use std::collections::HashMap;
use std::sync::Arc;

use leviath_core::layout::SeedToolCall;

use crate::config::ToolPolicy;

/// Runs one seeded tool call: `(name, args) -> result text`.
///
/// Injected rather than called directly so every failure arm is testable
/// without a live MCP connection or a compiled Rhai engine. Mirrors
/// [`crate::daemon::seed_command::SeedCommandRunner`].
pub(crate) type SeedToolRunner =
    Arc<dyn Fn(&str, &serde_json::Value) -> Result<String, String> + Send + Sync>;

/// How tool seeds are executed for one spawn.
#[derive(Clone)]
pub(crate) struct SeedToolPolicy {
    /// The executor.
    pub runner: SeedToolRunner,
}

impl SeedToolPolicy {
    /// A policy over an explicit runner.
    pub(crate) fn new(runner: SeedToolRunner) -> Self {
        Self { runner }
    }

    /// A policy that runs nothing, for tests that spawn without seed tools.
    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self {
            runner: Arc::new(|name, _| Err(format!("tool seeds are not resolved here ('{name}')"))),
        }
    }

    /// Run `call`, or report why it did not.
    pub(crate) fn run(&self, call: &SeedToolCall) -> Result<String, String> {
        (self.runner)(&call.name, &call.args)
    }
}

/// Whether a seed may run `policy`, and what to say when it may not.
///
/// Separated from the running so the decision is testable on its own, and so
/// the refusal text is written once rather than at each call site.
pub(crate) fn seed_policy_refusal(name: &str, policy: ToolPolicy) -> Option<String> {
    match policy {
        ToolPolicy::Allow => None,
        ToolPolicy::Deny => Some(format!(
            "tool '{name}' is denied by `[tool_permissions]`, so it cannot seed a region"
        )),
        ToolPolicy::Ask => Some(format!(
            "tool '{name}' is set to `ask`, and a seed runs before the first inference - \
             there is nobody to prompt. Set `[tool_permissions] {name} = \"allow\"` if this \
             agent is meant to call it at spawn."
        )),
    }
}

/// Render one call's result as the block that goes into the region.
///
/// Headed with the tool's name, following the `--- <path> ---` headings the
/// `files` and `glob` seeds already use, so a region seeded from several
/// sources reads the same however it was filled.
pub(crate) fn seed_block(name: &str, result: &str) -> String {
    format!("--- {name} ---\n{}", result.trim_end())
}

/// Join the blocks of one region's seed.
pub(crate) fn join_blocks(blocks: Vec<String>) -> Option<String> {
    (!blocks.is_empty()).then(|| blocks.join("\n\n"))
}

/// The policy layers a seed resolves each call against.
///
/// Borrowed from the spawn path rather than rebuilt, so a seed and a mid-run
/// call cannot disagree about what the user configured.
pub(crate) struct SeedToolPermissions<'a> {
    /// `--allow` / `--ask` / `--deny` / `--yolo` for this run.
    pub launch: &'a HashMap<String, ToolPolicy>,
    /// The entry stage's `[stages.<name>.tool_permissions]`.
    pub stage: &'a HashMap<String, String>,
    /// The agent's `[tool_permissions]`.
    pub agent: &'a HashMap<String, String>,
    /// The user's `config.toml` `[tool_permissions]`.
    pub global: &'a HashMap<String, ToolPolicy>,
    /// Whether this blueprint may loosen a tool below its built-in default.
    pub may_loosen: bool,
}

impl SeedToolPermissions<'_> {
    /// The resolved policy for `name`.
    pub(crate) fn resolve(&self, name: &str, is_builtin: bool) -> ToolPolicy {
        crate::tools::resolve_policy(
            name,
            is_builtin,
            self.launch,
            self.stage,
            self.agent,
            self.global,
            self.may_loosen,
        )
    }
}

/// Everything the production runner needs to answer one seeded call.
///
/// Cloned into the closure rather than borrowed, because the runner outlives
/// the borrow of the spawn locals it is built from.
pub(crate) struct SeedToolContext {
    /// The agent's built-in tools, over its workdir.
    pub builtins: Arc<leviath_tools::BuiltinTools>,
    /// Which names dispatch to `builtins` rather than to MCP.
    pub builtin_names: std::collections::HashSet<String>,
    /// The agent's compiled Rhai tools.
    pub script_tools: leviath_scripting::ScriptToolSet,
    /// The host those scripts run against.
    pub script_host: Arc<dyn leviath_scripting::ScriptHost>,
    /// The shared MCP executor.
    pub mcp: Arc<tokio::sync::Mutex<leviath_mcp::ToolExecutor>>,
    /// The run's write budget, shared with the tool lane so what a seed
    /// writes at spawn counts against the same ceiling as turn one.
    pub writes: Arc<crate::daemon::tool_service::WriteBudget>,
}

/// Decides one seeded call's policy: `(tool name, is_builtin) -> policy`.
///
/// A closure rather than [`SeedToolPermissions`] itself, so the spawn path can
/// hand over the layered resolution it already built without this module
/// learning its shape or borrowing its four maps for the runner's lifetime.
pub(crate) type SeedPolicyResolver = Arc<dyn Fn(&str, bool) -> ToolPolicy + Send + Sync>;

/// Build the runner a real spawn uses.
///
pub(crate) fn production_runner(
    ctx: SeedToolContext,
    resolve: SeedPolicyResolver,
) -> SeedToolRunner {
    Arc::new(move |name: &str, args: &serde_json::Value| {
        let is_builtin = ctx.builtin_names.contains(name);
        // The same three fences the tool lane applies to a mid-run call, in
        // the same order: a seed used to skip all of them, and a seed is the
        // one call that runs before anyone could have been asked.
        let policy = resolve(name, is_builtin);
        let policy =
            crate::tools::clamp_by_effect(name, args, policy, &|| resolve("write_file", true));
        if let Some(refusal) = seed_policy_refusal(name, policy) {
            return Err(refusal);
        }
        let workdir = ctx.builtins.workdir();
        if let Some(refusal) = crate::tools::escaping_write_refusal(name, args, workdir) {
            return Err(refusal);
        }
        if let Some(refusal) = crate::tools::write_budget_refusal(name, args, workdir, &ctx.writes)
        {
            return Err(refusal);
        }
        if let Some(declared) = crate::tools::declared_write_bytes(name, args) {
            ctx.writes.record(declared);
        }
        // A script tool is checked first, exactly as the tool lane checks it
        // first, so a discovered `.rhai` dispatches to the engine. The Rhai
        // engine is synchronous, so this branch needs no runtime at all.
        if let Some(tool) = ctx.script_tools.get(name) {
            return Ok(leviath_scripting::execute_script_tool(
                tool,
                args.clone(),
                ctx.script_host.clone(),
            ));
        }
        match is_builtin {
            true => {
                let out = block_on_daemon(ctx.builtins.execute(name, args.clone()));
                // A redirect is only measurable after the fact, as in the lane.
                ctx.writes
                    .record(crate::tools::measured_write_bytes(name, args, workdir));
                out
            }
            false => {
                let mcp = ctx.mcp.clone();
                let name = name.to_string();
                let args = args.clone();
                block_on_daemon(async move {
                    let routed = mcp.lock().await.route(&name);
                    mcp_text(match routed {
                        Ok((client, original)) => {
                            leviath_mcp::ToolExecutor::call_routed(&client, &original, args).await
                        }
                        Err(e) => Err(e),
                    })
                })
            }
        }
    })
}

/// What an MCP call reports back, as the seed records it.
///
/// A named function rather than arms inside the call, because covering those
/// arms in place would mean spawning a real MCP server: here the same three
/// outcomes are ordinary values. A tool that ran and said it failed is an
/// `[error]` just as much as one that could not be reached - the seed treats
/// both as "no block", and the model never sees a failure dressed as data.
pub(super) fn mcp_text(result: anyhow::Result<leviath_mcp::execution::ExecutionResult>) -> String {
    match result {
        Ok(r) if r.success => r.text,
        Ok(r) => format!("[error] {}", r.text),
        Err(e) => format!("[error] tool error: {e}"),
    }
}

/// Await `fut` from the synchronous spawn path.
///
/// The spawn path is sync and is called from inside the daemon's runtime (the
/// fan-out spawner runs it from an ECS system), so neither `block_on` on the
/// current handle nor a second runtime on this thread is allowed. `block_in_place`
/// moves this thread out of the scheduler for the duration and lets the handle
/// drive the future *on the daemon's own runtime* - which matters for MCP,
/// whose transports have tasks living there. A fresh runtime would leave those
/// tasks unpolled and the call would simply hang.
///
/// Without an ambient runtime - a unit test calling the runner directly - a
/// current-thread runtime is correct and sufficient, because a built-in tool
/// awaits nothing that belongs to another reactor.
fn block_on_daemon<F>(fut: F) -> Result<String, String>
where
    F: std::future::Future<Output = String> + Send,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => Ok(tokio::task::block_in_place(|| handle.block_on(fut))),
        // Building a current-thread runtime with none already present fails
        // only on OS resource exhaustion, at which point the spawn this seed
        // belongs to is doomed anyway - the same stance `run_seed_command`
        // takes, and for the same reason.
        Err(_) => Ok(tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime for a seed call always builds")
            .block_on(fut)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_allowed_tool_has_nothing_to_refuse() {
        assert_eq!(seed_policy_refusal("current_time", ToolPolicy::Allow), None);
    }

    /// A denied tool is refused in the seed exactly as it would be mid-run: the
    /// point of routing seeds through policy resolution is that a configured
    /// `deny` is terminal everywhere, not only where a model asked.
    #[test]
    fn a_denied_tool_says_so() {
        let refusal = seed_policy_refusal("shell", ToolPolicy::Deny).expect("refused");
        assert!(refusal.contains("shell"));
        assert!(refusal.contains("denied"));
    }

    /// The interesting one. There is nobody to prompt at spawn, so `ask` cannot
    /// quietly become an allow - that would make a tool seed a way around the
    /// permission layer for any tool a blueprint chose to name.
    #[test]
    fn an_ask_tool_is_refused_and_names_the_setting_that_would_let_it_run() {
        let refusal = seed_policy_refusal("write_file", ToolPolicy::Ask).expect("refused");
        assert!(refusal.contains("nobody to prompt"), "{refusal}");
        assert!(
            refusal.contains("write_file = \"allow\""),
            "the fix is spelled out: {refusal}"
        );
    }

    #[test]
    fn a_block_is_headed_with_the_tool_that_produced_it() {
        assert_eq!(
            seed_block("current_time", "{\"utc\": \"2026-08-18T19:32:07Z\"}"),
            "--- current_time ---\n{\"utc\": \"2026-08-18T19:32:07Z\"}"
        );
        // Trailing whitespace is dropped so the join spaces blocks evenly
        // however a tool chose to end its output.
        assert_eq!(seed_block("t", "x\n\n\n"), "--- t ---\nx");
    }

    #[test]
    fn blocks_join_with_a_blank_line_and_nothing_joins_to_nothing() {
        assert_eq!(
            join_blocks(vec!["--- a ---\n1".into(), "--- b ---\n2".into()]),
            Some("--- a ---\n1\n\n--- b ---\n2".to_string())
        );
        assert_eq!(
            join_blocks(vec!["--- a ---\n1".into()]),
            Some("--- a ---\n1".to_string())
        );
        // Every call having failed leaves the region unseeded rather than
        // holding an empty string, which reads downstream as content.
        assert_eq!(join_blocks(Vec::new()), None);
    }

    #[test]
    fn a_disabled_policy_runs_nothing_and_names_the_tool_it_skipped() {
        let policy = SeedToolPolicy::disabled();
        let err = policy
            .run(&SeedToolCall::new("current_time"))
            .expect_err("disabled");
        assert!(err.contains("current_time"), "{err}");
    }

    /// The runner really is the seam: what it answers is what the seed records,
    /// and the arguments reach it unchanged.
    #[test]
    fn the_injected_runner_receives_the_call_as_written() {
        let policy = SeedToolPolicy::new(Arc::new(|name, args| Ok(format!("{name}:{args}"))));
        let call =
            SeedToolCall::with_args("which_command", serde_json::json!({ "command": "git" }));
        assert_eq!(
            policy.run(&call).expect("ran"),
            "which_command:{\"command\":\"git\"}"
        );
        // A call with no arguments carries an empty object, not a null.
        assert_eq!(
            policy.run(&SeedToolCall::new("current_time")).expect("ran"),
            "current_time:{}"
        );
    }

    /// A server that ran the tool and reported failure is not data. Reporting
    /// its text plainly would put a diagnostic into the region under a heading
    /// that says the tool answered.
    #[test]
    fn an_mcp_failure_is_marked_as_one_however_it_failed() {
        let ok = leviath_mcp::execution::ExecutionResult {
            success: true,
            data: serde_json::Value::Null,
            text: "the answer".to_string(),
        };
        assert_eq!(mcp_text(Ok(ok)), "the answer");

        let failed = leviath_mcp::execution::ExecutionResult {
            success: false,
            data: serde_json::Value::Null,
            text: "no such record".to_string(),
        };
        assert_eq!(mcp_text(Ok(failed)), "[error] no such record");

        let unreachable = mcp_text(Err(anyhow::anyhow!("connection refused")));
        assert_eq!(unreachable, "[error] tool error: connection refused");
    }

    // ── the production runner ────────────────────────────────────────────

    /// A context whose script set and MCP executor are empty, so a call falls
    /// through to the built-ins. `dir` is the agent's workdir.
    fn ctx_over(
        dir: &std::path::Path,
        scripts: leviath_scripting::ScriptToolSet,
    ) -> SeedToolContext {
        ctx_with_budget(dir, scripts, Arc::new(unlimited_writes()))
    }

    /// A budget that stops nothing, over a filesystem reporting plenty of room.
    fn unlimited_writes() -> crate::daemon::tool_service::WriteBudget {
        crate::daemon::tool_service::WriteBudget::with_probe(Default::default(), |_| {
            Some(leviath_core::write_limits::MIN_FREE_BYTES * 100)
        })
    }

    /// [`ctx_over`] with the run's write budget chosen.
    fn ctx_with_budget(
        dir: &std::path::Path,
        scripts: leviath_scripting::ScriptToolSet,
        writes: Arc<crate::daemon::tool_service::WriteBudget>,
    ) -> SeedToolContext {
        let builtins = Arc::new(leviath_tools::BuiltinTools::new(
            leviath_tools::ToolContext::new(dir.to_path_buf()),
        ));
        let builtin_names = builtins.names().into_iter().collect();
        SeedToolContext {
            writes,
            builtins,
            builtin_names,
            script_tools: scripts,
            script_host: Arc::new(crate::daemon::script_host::DaemonScriptHost::new(
                // Nothing the seeded script needs here: the tool under test
                // computes its answer without reaching the network, the shell
                // or the filesystem.
                crate::daemon::script_host::ScriptAllow {
                    http_get: false,
                    http_post: false,
                    shell: false,
                    read_file: false,
                    write_file: false,
                    env_var: false,
                },
                dir.to_path_buf(),
            )),
            mcp: Arc::new(tokio::sync::Mutex::new(leviath_mcp::ToolExecutor::new())),
        }
    }

    /// Every call allowed, which is what an unconfigured environment tool
    /// already resolves to.
    fn allow_all() -> SeedPolicyResolver {
        Arc::new(|_, _| ToolPolicy::Allow)
    }

    /// No ambient runtime: `block_on_daemon` builds one of its own, which is
    /// the branch a plain `#[test]` takes and a `lev run` without a daemon
    /// would take too.
    #[test]
    fn a_builtin_seeds_without_an_ambient_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let runner = production_runner(ctx_over(dir.path(), Default::default()), allow_all());
        let out = runner("current_time", &serde_json::json!({})).expect("ran");
        let v: serde_json::Value = serde_json::from_str(&out).expect("the tool's JSON");
        assert!(v["utc"].as_str().is_some(), "{out}");
    }

    /// Inside the daemon's runtime the call must be driven on *that* runtime -
    /// a second one would leave an MCP transport's tasks unpolled and the call
    /// would hang. Multi-thread because `block_in_place` requires it, which is
    /// what the daemon actually runs.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_builtin_seeds_on_the_daemons_own_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let runner = production_runner(ctx_over(dir.path(), Default::default()), allow_all());
        let out = runner("system_info", &serde_json::json!({})).expect("ran");
        let v: serde_json::Value = serde_json::from_str(&out).expect("the tool's JSON");
        assert_eq!(v["os"], std::env::consts::OS);
    }

    /// The policy gate runs before anything else, so a denied tool is never
    /// executed - not executed and then discarded.
    #[test]
    fn a_denied_tool_is_refused_before_it_runs() {
        let dir = tempfile::tempdir().unwrap();
        let refuse: SeedPolicyResolver = Arc::new(|_, _| ToolPolicy::Deny);
        let runner = production_runner(ctx_over(dir.path(), Default::default()), refuse);
        let err = runner("current_time", &serde_json::json!({})).expect_err("refused");
        assert!(err.contains("denied"), "{err}");
        // An `ask` is refused too, for want of anyone to ask.
        let ask: SeedPolicyResolver = Arc::new(|_, _| ToolPolicy::Ask);
        let runner = production_runner(ctx_over(dir.path(), Default::default()), ask);
        let err = runner("current_time", &serde_json::json!({})).expect_err("refused");
        assert!(err.contains("nobody to prompt"), "{err}");
    }

    /// A seed runs before the first inference, so it used to run before every
    /// fence the tool lane applies: a `shell` seed could redirect outside the
    /// workdir, which the same line as a tool call is refused for.
    #[test]
    fn a_seed_cannot_redirect_outside_the_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("pwned.txt");
        let command = format!("echo pwn > {}", target.display());
        let runner = production_runner(ctx_over(dir.path(), Default::default()), allow_all());
        let err = runner("shell", &serde_json::json!({ "command": command })).expect_err("refused");
        assert!(err.contains("outside the working directory"), "{err}");
        assert!(!target.exists(), "the seed wrote outside the workdir");
    }

    /// And the write clamp: a `shell` seed that redirects is a write, so a
    /// `write_file = "deny"` refuses it even though `shell` itself is allowed.
    #[test]
    fn a_seed_redirect_answers_to_the_write_policy() {
        let dir = tempfile::tempdir().unwrap();
        let deny_writes: SeedPolicyResolver = Arc::new(|name, _| match name {
            "write_file" => ToolPolicy::Deny,
            _ => ToolPolicy::Allow,
        });
        let runner = production_runner(ctx_over(dir.path(), Default::default()), deny_writes);
        let err = runner(
            "shell",
            &serde_json::json!({ "command": "echo x > inside.txt" }),
        )
        .expect_err("refused");
        assert!(err.contains("denied"), "{err}");
        assert!(
            !dir.path().join("inside.txt").exists(),
            "the seed wrote anyway"
        );
    }

    /// A seed's write spends the run's budget, the same budget turn one then
    /// checks against, and a seed over the ceiling is refused before it lands.
    #[test]
    fn a_seed_write_spends_the_run_budget() {
        let dir = tempfile::tempdir().unwrap();
        let writes = Arc::new(crate::daemon::tool_service::WriteBudget::with_probe(
            leviath_core::write_limits::WriteLimits {
                per_call: Some(8),
                per_run: Some(10),
            },
            |_| Some(leviath_core::write_limits::MIN_FREE_BYTES * 100),
        ));
        let runner = production_runner(
            ctx_with_budget(dir.path(), Default::default(), writes.clone()),
            allow_all(),
        );
        runner(
            "write_file",
            &serde_json::json!({ "path": "seeded.txt", "content": "12345678" }),
        )
        .expect("fits");
        assert_eq!(writes.written(), 8);
        let err = runner(
            "write_file",
            &serde_json::json!({ "path": "more.txt", "content": "123" }),
        )
        .expect_err("over the run ceiling");
        assert!(err.contains("budget"), "{err}");
        assert!(!dir.path().join("more.txt").exists());
        // A shell redirect is measured after the fact and charged too.
        let runner = production_runner(
            ctx_with_budget(dir.path(), Default::default(), Arc::new(unlimited_writes())),
            allow_all(),
        );
        runner(
            "shell",
            &serde_json::json!({ "command": "echo hi > out.txt" }),
        )
        .expect("ran");
    }

    /// Whether a name is a built-in is what the resolver is told, and it is the
    /// question `default_tool_policy` turns on for an unknown tool.
    #[test]
    fn the_resolver_is_told_whether_the_name_is_a_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<(String, bool)>::new()));
        let captured = seen.clone();
        let resolver: SeedPolicyResolver = Arc::new(move |name: &str, is_builtin: bool| {
            captured
                .lock()
                .unwrap()
                .push((name.to_string(), is_builtin));
            ToolPolicy::Deny
        });
        let runner = production_runner(ctx_over(dir.path(), Default::default()), resolver);
        let _ = runner("current_time", &serde_json::json!({}));
        let _ = runner("acme__thing", &serde_json::json!({}));
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [
                ("current_time".to_string(), true),
                ("acme__thing".to_string(), false),
            ]
        );
    }

    /// A Rhai script tool is checked before the built-ins, exactly as the tool
    /// lane checks it first, and runs synchronously - no runtime involved.
    #[test]
    fn a_script_tool_seeds_through_the_rhai_engine() {
        let dir = tempfile::tempdir().unwrap();
        let tools_dir = dir.path().join("tools");
        std::fs::create_dir(&tools_dir).unwrap();
        std::fs::write(
            tools_dir.join("greet.rhai"),
            "// @tool greet
// @param who string required Who to greet
`hello ` + params.who",
        )
        .unwrap();
        let (set, skipped) = leviath_scripting::ScriptToolSet::discover(&[tools_dir]);
        assert!(skipped.is_empty(), "{skipped:?}");
        let runner = production_runner(ctx_over(dir.path(), set), allow_all());
        let out = runner("greet", &serde_json::json!({ "who": "world" })).expect("ran");
        assert!(out.contains("hello world"), "{out}");
    }

    /// A name that is neither a built-in nor a script is an MCP tool. With no
    /// server connected there is nothing to answer it, and that comes back as a
    /// tool error rather than a panic or a hang.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unconnected_mcp_tool_reports_an_error_rather_than_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let runner = production_runner(ctx_over(dir.path(), Default::default()), allow_all());
        let out = runner("acme__do_thing", &serde_json::json!({})).expect("answered");
        assert!(out.starts_with("[error]"), "{out}");
    }

    /// The same name with a server behind it: the seed routes to the server
    /// and carries its text back, going through the executor's per-server
    /// lock the way the tool lane does.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_connected_mcp_tool_answers_a_seed() {
        const STUB: &str = r#"
import sys, json
def respond(id_, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": id_, "result": result}) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line); method = req.get("method", ""); id_ = req.get("id")
    if method == "initialize":
        respond(id_, {"capabilities": {"tools": {"listChanged": False}}, "protocolVersion": "2024-11-05"})
    elif method == "tools/list":
        respond(id_, {"tools": [{"name": "do_thing", "description": "s", "inputSchema": {"type": "object", "properties": {}}}]})
    elif method == "tools/call":
        respond(id_, {"content": [{"type": "text", "text": "seeded by acme"}], "isError": False})
    elif method != "notifications/initialized" and method != "notifications/cancelled":
        respond(id_, {})
"#;
        let mut client = leviath_mcp::MCPClient::spawn("python3", &["-c", STUB], &HashMap::new())
            .await
            .expect("spawn stub");
        client.connect().await.expect("connect");
        client.list_tools().await.expect("list_tools");
        let mut executor = leviath_mcp::ToolExecutor::new();
        let _ = executor.add_client_advertised(
            "acme".to_string(),
            client,
            &std::collections::HashSet::new(),
        );
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_over(dir.path(), Default::default());
        ctx.mcp = Arc::new(tokio::sync::Mutex::new(executor));
        let runner = production_runner(ctx, allow_all());
        let out = runner("acme__do_thing", &serde_json::json!({})).expect("answered");
        assert_eq!(out, "seeded by acme");
    }

    #[test]
    fn permissions_resolve_through_the_same_path_the_tool_lane_uses() {
        let launch = HashMap::new();
        let stage = HashMap::new();
        let agent = HashMap::new();
        let mut global = HashMap::new();
        global.insert("current_time".to_string(), ToolPolicy::Deny);
        let perms = SeedToolPermissions {
            launch: &launch,
            stage: &stage,
            agent: &agent,
            global: &global,
            may_loosen: false,
        };
        // The user's config is honoured at seed time, not only mid-run.
        assert_eq!(perms.resolve("current_time", true), ToolPolicy::Deny);
        // And an unconfigured environment tool keeps its built-in `allow`.
        assert_eq!(perms.resolve("system_info", true), ToolPolicy::Allow);
        // While a mutating one still defaults to `ask`, which a seed refuses.
        assert_eq!(perms.resolve("write_file", true), ToolPolicy::Ask);
    }
}

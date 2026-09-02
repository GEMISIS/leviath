//! Tool execution: dispatch, filesystem operations, and the shell.

use super::*;

/// What a directory holds, one entry per line, for the `read_file` correction.
///
/// Capped: a directory of thousands would bury the answer it is meant to give,
/// and the model only needs enough to pick its next path. Directories are marked
/// so a second wrong `read_file` is avoidable rather than merely explained.
fn directory_listing(path: &std::path::Path) -> String {
    const MAX_ENTRIES: usize = 50;
    let mut names: Vec<String> = std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            match e.file_type().map(|ft| ft.is_dir()) {
                Ok(true) => format!("{name}/"),
                _ => name,
            }
        })
        .collect();
    names.sort();
    if names.is_empty() {
        // Empty, or unreadable. The model does the same thing next either way.
        return "  (no entries to show)".to_string();
    }
    let total = names.len();
    names.truncate(MAX_ENTRIES);
    let mut out = names
        .iter()
        .map(|n| format!("  {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    if total > MAX_ENTRIES {
        out.push_str(&format!(
            "\n  ... and {} more (call list_dir for the rest)",
            total - MAX_ENTRIES
        ));
    }
    out
}

/// Fold `..` components of `raw` lexically, without touching the filesystem.
///
/// `None` when a `..` would climb past the root: that request is unresolvable
/// whatever any allowlist says. `Path::components` already drops interior
/// `.`, and every caller passes an absolute path, so no `CurDir` survives.
/// Shared by [`resolve_within`] and [`BuiltinTools::resolve_outside`].
fn fold_parents(raw: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            c => normalized.push(c),
        }
    }
    Some(normalized)
}

/// Resolve `requested` against `workdir`, refusing anything that leaves it.
///
/// The shared definition of "inside the workspace" for every path a tool or
/// a script hands the daemon: the built-in file tools and the Rhai script
/// host both go through here, so `write_file` and a script's `write_file`
/// cannot disagree about what a path is allowed to be.
///
/// The `within` predicate is a `fn` pointer (not `impl Fn`) so there is one
/// monomorphization, matching the seam idiom used elsewhere in the workspace.
/// The seam exists because the refusal cannot be reached otherwise on every
/// platform: producing the escape needs a real symlink, and creating one on
/// Windows requires a privilege CI runners do not have. Injecting the
/// predicate lets the refusal itself be tested everywhere, while the
/// `#[cfg(unix)]` tests still prove the real filesystem behaviour end to end.
pub fn resolve_within(
    requested: &str,
    workdir: &Path,
    within: fn(&Path, &Path) -> bool,
) -> anyhow::Result<PathBuf> {
    if is_null_device(requested) {
        return Ok(PathBuf::from(requested));
    }
    let raw = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        workdir.join(requested)
    };

    // Normalize by resolving .. and . without requiring the path to exist.
    let Some(normalized) = fold_parents(&raw) else {
        anyhow::bail!("path '{}' escapes the working directory", requested);
    };

    if !normalized.starts_with(workdir) {
        // Names the workspace and what to do instead. "Denied" on its own
        // sends an agent looking for a different way out, and it spends
        // iterations - which the stage's budget is charged for - finding
        // that there isn't one.
        anyhow::bail!(
            "path '{}' would escape the working directory ({}). Use a path \
             inside the workspace instead - a relative path resolves \
             against it.",
            requested,
            workdir.display()
        );
    }

    if !within(&normalized, workdir) {
        anyhow::bail!(
            "path '{requested}' resolves outside the working directory through a symlink"
        );
    }

    Ok(normalized)
}

impl BuiltinTools {
    /// Execute a built-in tool by name (resolving aliases), returning the result
    /// as a string.
    pub async fn execute(&self, name: &str, args: Value) -> String {
        let canonical = canonical_tool_name(name);
        // A tool whose platform capabilities aren't met never advertises, but a
        // caller could still dispatch to it directly - reject it here too.
        if !self.available(canonical) {
            return format!("[error] tool '{}' is not available on this platform", name);
        }
        // The environment tools do no awaiting of their own (see `env.rs`), so
        // they answer here rather than through five arms that each `.await`
        // something which never yields. Taken before the match so the
        // "is this one of mine" test and the dispatch are a single step -
        // a guard arm would need a fallback branch nothing can reach.
        if let Some(result) = self.execute_env_tool(canonical, &args) {
            return result;
        }
        match canonical {
            "read_file" => self.read_file(&args).await,
            "read_files" => self.read_files(&args).await,
            "write_file" => self.write_file(&args).await,
            "edit_file" => self.edit_file(&args).await,
            "list_dir" => self.list_dir(&args).await,
            "shell" => self.shell(&args).await,
            // Synchronous like the environment tools: compile, then one
            // atomic write into the global tools directory (see `install.rs`).
            "install_tool" => self.install_tool(&args),
            // Like the context tools, this one needs the live world: the stage,
            // iteration and token counts it reports exist only there.
            "runtime_info" => "[error] runtime_info must be handled by the runtime".to_string(),
            n if n.starts_with("context_") => {
                "[error] context tools must be handled by the runtime".to_string()
            }
            // Like the context tools, this one needs the live world: it writes
            // an ECS component and a context region. Refused here so the
            // runtime stays the only path that can record an output.
            SUBMIT_OUTPUT_TOOL => {
                "[error] submit_output must be handled by the runtime".to_string()
            }
            // Applied inline by the dispatcher, which parks the calling agent
            // on its workers - neither of which this executor can do. Refused
            // here so the world stays the only thing that can start a fan-out.
            FAN_OUT_TOOL => "[error] fan_out must be handled by the runtime".to_string(),
            _ => format!("[error] Unknown built-in tool: {}", name),
        }
    }

    /// Refuse to create anything when the working directory itself is gone.
    ///
    /// `write_file` calls `create_dir_all`, which would otherwise silently
    /// resurrect a workspace an external harness deleted mid-run - leaving the
    /// agent writing into an empty tree that no longer resembles the checkout it
    /// reasoned about, and masking the loss from the runtime's health check.
    /// Creating *sub*directories inside a live workspace is untouched; only a
    /// missing workspace root is refused.
    pub(crate) fn ensure_workspace(&self) -> Result<(), String> {
        if std::fs::metadata(&self.ctx.workdir).is_ok_and(|m| m.is_dir()) {
            return Ok(());
        }
        Err(format!(
            "[error] workspace '{}' is no longer accessible",
            self.ctx.workdir.display()
        ))
    }

    /// Resolve a requested path to an absolute path inside the workdir.
    ///
    /// Two checks, because either alone is insufficient:
    ///
    /// 1. **Lexical.** `..` and `.` are folded out and the result must sit under
    ///    the workdir. Cheap, and it catches the obvious `../../etc/passwd`.
    /// 2. **Symbolic.** The deepest *existing* ancestor is canonicalized and the
    ///    result re-checked. Without it the containment is purely textual: a
    ///    symlink at `<workdir>/link` pointing at `/` makes
    ///    `read_file("link/etc/passwd")` normalize to a path that starts with the
    ///    workdir, pass, and then be followed by `fs::read_to_string`, and the
    ///    same hole lets `write_file` overwrite `~/.ssh/authorized_keys`.
    ///
    /// That matters most where the containment is load-bearing. Leviath's file
    /// tools run **on the host over the bind-mounted workdir** even when the
    /// stage's `shell` is confined to a container, so a symlink the agent
    /// creates inside the container escapes the container through the file
    /// tools. It also matters for a freshly cloned repository, which is exactly
    /// what a coding agent operates on and which can carry a checked-in symlink
    /// pointing anywhere.
    ///
    /// The check is not TOCTOU-proof: a symlink planted between this call and the
    /// subsequent `open` still wins. Closing that needs `openat`/`O_NOFOLLOW`
    /// throughout, which is a larger change; this stops the planted-symlink case,
    /// which is the one an agent can actually arrange.
    pub(crate) fn resolve(&self, requested: &str) -> anyhow::Result<PathBuf> {
        resolve_within(requested, &self.ctx.workdir, resolves_within)
    }

    /// Resolve a requested path for a *read-only* tool.
    ///
    /// Identical to [`resolve`](Self::resolve) - same two checks, same
    /// errors - until the workdir refuses. Only then, and only when the
    /// agent's `[read_paths]` policy is active, the path is checked against
    /// that policy: canonicalized first (fail closed), then both predicates
    /// of [`leviath_core::ReadPathPolicy::decide`] must hold - the blueprint
    /// declared it AND the user's config grants it.
    ///
    /// This function is deliberately not called by `write_file`/`edit_file`.
    /// `[read_paths]` grants reads; the write tools stay on
    /// [`resolve`](Self::resolve) so an allowlisted directory can be read but
    /// never written.
    pub(crate) fn resolve_read(&self, requested: &str) -> anyhow::Result<PathBuf> {
        match resolve_within(requested, &self.ctx.workdir, resolves_within) {
            Ok(path) => Ok(path),
            Err(workdir_err) => {
                let read_paths = self.ctx.read_paths();
                if !read_paths.is_active() {
                    return Err(workdir_err);
                }
                Self::resolve_outside(
                    requested,
                    &self.ctx.workdir,
                    &read_paths,
                    leviath_core::canonicalize_for_match,
                )
            }
        }
    }

    /// The out-of-workdir arm of [`resolve_read`](Self::resolve_read), with
    /// the canonicalizer injected (`fn` pointer, same seam idiom as
    /// [`resolve_within`]) so the fail-closed refusal is
    /// testable on every platform.
    ///
    /// The returned path is the *canonicalized* one - the path that was
    /// actually vetted - so the subsequent `open` operates on what the policy
    /// approved rather than re-walking any symlinks.
    pub(crate) fn resolve_outside(
        requested: &str,
        workdir: &Path,
        policy: &leviath_core::ReadPathPolicy,
        canon: fn(&Path) -> Option<PathBuf>,
    ) -> anyhow::Result<PathBuf> {
        // Relative requests resolve against the workdir here too, so a
        // relative `[read_paths]` entry like "../shared" is reachable by the
        // matching relative request. Canonicalization below is what decides
        // containment; the workdir join is just the base.
        let raw = if Path::new(requested).is_absolute() {
            PathBuf::from(requested)
        } else {
            workdir.join(requested)
        };

        // Fold `..` lexically; popping past the filesystem root is
        // unresolvable no matter what any allowlist says.
        let Some(normalized) = fold_parents(&raw) else {
            anyhow::bail!("path '{requested}' cannot be resolved");
        };

        // The policy only ever sees the real, symlink-resolved path. A path
        // that cannot be verified is refused, never matched.
        let Some(canonical) = canon(&normalized) else {
            anyhow::bail!("path '{requested}' cannot be verified against [read_paths]");
        };

        match policy.decide(&canonical) {
            leviath_core::ReadPathDecision::Allowed => Ok(canonical),
            leviath_core::ReadPathDecision::NotDeclared => anyhow::bail!(
                "path '{requested}' is outside the working directory and not in this \
                 agent's [read_paths]"
            ),
            leviath_core::ReadPathDecision::NotGranted => anyhow::bail!(
                "path '{requested}' matches this agent's [read_paths], but your config \
                 does not grant it; add it under [agent_read_paths.{agent}] (or set \
                 allow_blueprint_read_paths = true under [security]) in your config.toml",
                agent = policy.agent
            ),
        }
    }

    pub(crate) async fn read_file(&self, args: &Value) -> String {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "[error] missing 'path' argument".to_string(),
        };

        let path = match self.resolve_read(path_str) {
            Ok(p) => p,
            Err(e) => return format!("[error] {}", e),
        };

        match std::fs::read_to_string(&path) {
            Ok(content) => cap_file_content(&content, MAX_READ_FILE_BYTES),
            // A directory is not a malformed path, it is the wrong tool: the
            // model wanted to see what is in there. The raw OS message ("Is a
            // directory (os error 21)") names the problem without naming the
            // fix, so the next call is another guess.
            //
            // The listing comes back with the error rather than a pointer to
            // `list_dir`, because 21 of the bundled agents' stages grant
            // `read_file` and not `list_dir`: there, naming that tool asks for
            // something the stage cannot do. Answering here settles it in one
            // call and reads the same in every stage.
            Err(_) if path.is_dir() => {
                let listing = directory_listing(&path);
                format!(
                    "[error] '{path_str}' is a directory, not a file. It contains:\n{listing}\n\
                     Call read_file again on one of these entries."
                )
            }
            Err(e) => format!("[error] Failed to read '{}': {}", path_str, e),
        }
    }

    pub(crate) async fn read_files(&self, args: &Value) -> String {
        let paths = match args.get("paths").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => return "[error] missing 'paths' argument (expected array)".to_string(),
        };

        if paths.is_empty() {
            return "[error] 'paths' array is empty".to_string();
        }

        let mut results = Vec::with_capacity(paths.len());
        for path_val in paths {
            let path_str = match path_val.as_str() {
                Some(p) => p,
                None => {
                    results.push("[error] non-string path in array".to_string());
                    continue;
                }
            };

            let path = match self.resolve_read(path_str) {
                Ok(p) => p,
                Err(e) => {
                    results.push(format!("### [{}]\n[error] {}", path_str, e));
                    continue;
                }
            };

            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    results.push(format!("### [{}]\n{}", path_str, content));
                }
                Err(e) => {
                    results.push(format!("### [{}]\n[error] Failed to read: {}", path_str, e));
                }
            }
        }

        results.join("\n\n")
    }

    pub(crate) async fn write_file(&self, args: &Value) -> String {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "[error] missing 'path' argument".to_string(),
        };
        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "[error] missing 'content' argument".to_string(),
        };
        if let Err(e) = self.ensure_workspace() {
            return e;
        }

        let path = match self.resolve(path_str) {
            Ok(p) => p,
            Err(e) => return format!("[error] {}", e),
        };

        // Serialize concurrent writes to the same file (fan-out workers).
        let lock = self.ctx.lock_for(&path);
        let _guard = lock.lock().await;

        let parent = {
            let mut p = path.clone();
            p.pop();
            p
        };
        if let Err(e) = std::fs::create_dir_all(&parent) {
            return format!(
                "[error] Failed to create directories for '{}': {}",
                path_str, e
            );
        }

        match std::fs::write(&path, content) {
            Ok(()) => format!(
                "Successfully wrote {} bytes to '{}'",
                content.len(),
                path_str
            ),
            Err(e) => format!("[error] Failed to write '{}': {}", path_str, e),
        }
    }

    pub(crate) async fn edit_file(&self, args: &Value) -> String {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "[error] missing 'path' argument".to_string(),
        };
        let old_str = match args.get("old_str").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return "[error] missing 'old_str' argument".to_string(),
        };
        let new_str = match args.get("new_str").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return "[error] missing 'new_str' argument".to_string(),
        };
        if let Err(e) = self.ensure_workspace() {
            return e;
        }

        let path = match self.resolve(path_str) {
            Ok(p) => p,
            Err(e) => return format!("[error] {}", e),
        };

        // Serialize the read-modify-write against concurrent edits/writes to the
        // same file (fan-out workers), preventing lost updates.
        let lock = self.ctx.lock_for(&path);
        let _guard = lock.lock().await;

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return format!("[error] Failed to read '{}': {}", path_str, e),
        };

        let count = content.matches(old_str).count();
        match count {
            0 => format!(
                "[error] String not found in '{}'. Ensure old_str matches the file exactly.",
                path_str
            ),
            1 => {
                let new_content = content.replacen(old_str, new_str, 1);
                match std::fs::write(&path, &new_content) {
                    Ok(()) => format!("Successfully edited '{}'", path_str),
                    Err(e) => format!("[error] Failed to write '{}': {}", path_str, e),
                }
            }
            n => format!(
                "[error] Found {} occurrences of the string in '{}'. old_str must be unique.",
                n, path_str
            ),
        }
    }

    pub(crate) async fn list_dir(&self, args: &Value) -> String {
        let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let path = match self.resolve_read(path_str) {
            Ok(p) => p,
            Err(e) => return format!("[error] {}", e),
        };

        let entries = match std::fs::read_dir(&path) {
            Ok(e) => e,
            Err(e) => return format!("[error] Failed to read directory '{}': {}", path_str, e),
        };

        let mut items: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        items.sort_by_key(|e| e.file_name());

        let mut lines = Vec::new();
        for entry in items {
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            if is_dir {
                lines.push(format!("{}/", name));
            } else {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                lines.push(format!("{} ({}B)", name, size));
            }
        }

        if lines.is_empty() {
            format!("(empty directory: {})", path_str)
        } else {
            lines.join("\n")
        }
    }

    /// Detect the best available shell on the system.
    ///
    /// Priority:
    /// - Windows: cmd.exe (always available)
    /// - Unix: $SHELL env var (user's preferred shell) → bash → zsh → sh
    pub(crate) fn detect_shell() -> (&'static str, &'static str) {
        Self::detect_shell_for(
            std::env::consts::OS,
            std::env::var("SHELL").ok(),
            &Self::shell_path_exists,
        )
    }

    /// Whether `path` names something on disk.
    ///
    /// A named `fn` rather than a closure at the call site. On Windows
    /// [`Self::detect_shell_for`] returns before probing anything, so a closure
    /// written at the call site would be a region the Windows coverage leg
    /// never executes; a `fn` can be handed to the seam directly by a test that
    /// runs on every platform.
    pub(crate) fn shell_path_exists(path: &str) -> bool {
        std::path::Path::new(path).exists()
    }

    /// Core shell-detection logic with injectable OS, env, and filesystem
    /// checks for testing.
    ///
    /// `os` is a parameter rather than a `#[cfg(windows)]` branch, following
    /// `leviath_sys::browser::open_command_for` and
    /// `leviath_runtime::pipeline::shell_guidance_for`: the Windows answer is
    /// then reachable under test from any platform instead of only on the
    /// Windows CI leg. Callers pass `std::env::consts::OS`.
    ///
    /// `$SHELL` is deliberately ignored on Windows even though Git for Windows
    /// sets it - the value names an MSYS path (`/usr/bin/bash`) that
    /// `CreateProcess` cannot run.
    ///
    /// `shell_exists` is a trait object (`&dyn Fn(&str) -> bool`) rather
    /// than `impl Fn(&str) -> bool` so every caller - production's real
    /// `Path::exists` probe and each test's distinct closure - shares
    /// exactly ONE monomorphization of this function instead of one per
    /// closure type (this function was a confirmed generic-monomorphization
    /// coverage-attribution artifact: every source position had a covered
    /// instantiation, but the summary table still reported some as missed).
    pub(crate) fn detect_shell_for(
        os: &str,
        env_shell: Option<String>,
        shell_exists: &dyn Fn(&str) -> bool,
    ) -> (&'static str, &'static str) {
        if os == "windows" {
            return ("cmd.exe", "/C");
        }
        if let Some(shell) = env_shell
            && (shell.ends_with("/zsh") || shell.ends_with("/bash") || shell.ends_with("/sh"))
            && shell_exists(&shell)
        {
            // Only trust `$SHELL` when it actually exists - a stale or
            // sandbox-missing `$SHELL` (e.g. `/bin/zsh` in an environment that
            // doesn't ship it) would otherwise make every shell call fail to
            // spawn.
            // When it's missing, fall through to the known-path fallback list.
            let shell: &'static str = Box::leak(shell.into_boxed_str());
            return (shell, "-c");
        }
        for &shell in &[
            "/bin/bash",
            "/usr/bin/bash",
            "/bin/zsh",
            "/usr/bin/zsh",
            "/bin/sh",
        ] {
            if shell_exists(shell) {
                return (shell, "-c");
            }
        }
        ("sh", "-c")
    }

    pub(crate) async fn shell(&self, args: &Value) -> String {
        self.shell_with_timeout(args, Duration::from_secs(60)).await
    }

    /// Same as [`Self::shell`], with an injectable timeout so tests can
    /// exercise the timeout branch without a real 60-second wait.
    pub(crate) async fn shell_with_timeout(
        &self,
        args: &Value,
        timeout_duration: Duration,
    ) -> String {
        self.shell_with_limits(args, timeout_duration, MAX_CAPTURE_BYTES)
            .await
    }

    /// Same again, with the capture cap injectable too.
    ///
    /// The truncation wiring is otherwise only reachable by producing a real
    /// megabyte of output, which needs a shell one-liner that floods stdout -
    /// and `cmd.exe` and `sh` have no such line in common. A tiny cap and a
    /// plain `echo` exercise the same arms on every platform.
    pub(crate) async fn shell_with_limits(
        &self,
        args: &Value,
        timeout_duration: Duration,
        cap: usize,
    ) -> String {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "[error] missing 'command' argument".to_string(),
        };

        let workdir = self.ctx.workdir.clone();
        let (shell, flag) = Self::detect_shell();

        // When a sandbox executor is attached, it builds a command that runs
        // inside a container / namespace (still targeting `workdir`); otherwise
        // run the shell directly on the host.
        let mut cmd = match &self.shell_executor {
            Some(executor) => executor.build_command(shell, flag, command, &workdir),
            None => {
                let mut c = crate::platform::child_command(shell);
                c.arg(flag).arg(command).current_dir(&workdir);
                c
            }
        };
        // Reap the whole command on drop, not just the shell.
        //
        // Dropping a `Command` future detaches its process by default, so a
        // cancelled agent (or an elapsed timeout, which drops the future the
        // same way) would leave its shell running: the run gone from every
        // listing while its command carries on writing to the workspace.
        // `kill_on_drop` covers the shell and only the shell. Anything it
        // started (`sleep 400 && …`) is a *grandchild*, gets reparented to init,
        // and keeps running. Putting the shell in its own process group and
        // signalling the group on drop takes the whole tree down with it.
        cmd.kill_on_drop(true);
        own_process_group(&mut cmd);
        // Strip the credentials the daemon holds but this command has no use
        // for.
        //
        // After the branch above rather than inside its host arm, so it also
        // covers the namespace sandbox (which `unshare`s but still inherits the
        // environment) and the warn-fallback that quietly runs on the host when
        // namespaces turn out to be unusable - the arm most likely to be
        // forgotten. A container exec is built with no `-e` flags and so never
        // inherited the daemon's environment to begin with, which makes this a
        // no-op there rather than a special case.
        self.ctx.shell_env.apply(&mut cmd);
        // `spawn` inherits stdio where `output` pipes it; pipe explicitly so the
        // command's output is still captured.
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // Spawn *inside* the timed future so the reaper guard lives exactly as
        // long as the command does: dropping this future (timeout, or the whole
        // batch dropped because the agent was cancelled) drops the guard, which
        // signals the group. Keeping spawn and wait in one fallible block also
        // keeps a single error arm, as `Command::output()` had.
        let run = async {
            let mut child = cmd.spawn()?;
            // The child leads its own group, so its pid is the group id.
            let _reaper = child.id().map(ProcessGroupReaper);
            // Taken before the join so both pipes are drained concurrently with
            // each other and with the wait. `piped()` above guarantees both.
            let mut out = child.stdout.take().expect("stdout was piped");
            let mut err = child.stderr.take().expect("stderr was piped");
            let (stdout, stderr, status) = tokio::join!(
                capture_capped(&mut out, cap),
                capture_capped(&mut err, cap),
                child.wait(),
            );
            // The exit status is the only fallible part worth failing on, so it
            // stays the single error edge this block has - the same shape
            // `wait_with_output()` presented. A pipe that errors mid-read is
            // handled inside `capture_capped` as an early end of output.
            status.map(|status| (stdout, stderr, status))
        };

        match timeout(timeout_duration, run).await {
            Err(_) => format!("[timed out] Command exceeded 60s: {}", command),
            Ok(Err(e)) => format!("[error] Failed to spawn shell '{}': {}", shell, e),
            Ok(Ok((stdout, stderr, status))) => {
                let body = Self::format_command_output(
                    &stdout.kept,
                    &stderr.kept,
                    status.success(),
                    status.code().unwrap_or(-1),
                );
                match capture_note(&stdout, &stderr, cap) {
                    Some(note) => format!("{body}\n\n{note}"),
                    None => body,
                }
            }
        }
    }

    /// Format captured command output. Split out (behavior-preserving) from
    /// [`Self::shell_with_timeout`] so the success / non-zero-exit
    /// stdout+stderr formatting arms can be exercised deterministically on
    /// every platform, independent of the host shell's command-chaining and
    /// redirection syntax (`cmd.exe` and `sh` differ, so an integration test
    /// that produces stdout+stderr+non-zero-exit in one command is not
    /// portable).
    pub(crate) fn format_command_output(
        stdout: &[u8],
        stderr: &[u8],
        success: bool,
        exit_code: i32,
    ) -> String {
        let stdout = String::from_utf8_lossy(stdout);
        let stderr = String::from_utf8_lossy(stderr);

        if success {
            if stdout.trim().is_empty() {
                "(command succeeded with no output)".to_string()
            } else {
                stdout.to_string()
            }
        } else {
            let mut result = format!("[exit code {}]\n", exit_code);
            if !stdout.trim().is_empty() {
                result.push_str(&format!("stdout:\n{}\n", stdout));
            }
            if !stderr.trim().is_empty() {
                result.push_str(&format!("stderr:\n{}", stderr));
            }
            result
        }
    }
}

/// Largest slice of one stream (stdout or stderr) a single shell call keeps.
///
/// Unbounded capture is a memory-exhaustion hole: `wait_with_output()` holds a
/// child's entire output in the daemon's memory with nothing to stop it, and a
/// command that prints for its full 60-second budget runs to gigabytes on a
/// fast local pipe.
///
/// Sized just above `MAX_SCRIPT_IO_BYTES` (900 KB in `daemon::script_host`),
/// which caps the same text when it reaches a Rhai tool script, so this one is
/// the outer bound and that one stays the tighter of the two. A megabyte of
/// shell output already overruns any region budget an agent has; keeping more
/// of it helps nobody downstream.
pub(crate) const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

/// Is `path` the platform's discard device?
///
/// The null device is not a location in the filesystem, so a workspace check has
/// nothing to say about it: writing there writes nowhere, and reading there
/// reads nothing. Refusing it answers `path '/dev/null' would escape the
/// working directory`, which is both wrong and, worse, unactionable - an agent
/// told that spends turns guessing at a path it cannot fix.
///
/// Deliberately *only* the null device, not `/dev/stdout` or `/dev/stderr`.
/// Those are the daemon's own streams once a tool opens them by name, and a
/// tool writing into them would land in the middle of whatever the CLI is
/// drawing. A shell *redirect* to them stays allowed, because that redirects
/// the child's streams rather than opening the daemon's - a different thing
/// that happens to be spelled the same way.
pub fn is_null_device(path: &str) -> bool {
    // `NUL` is the Windows spelling, matched on every platform for the same
    // reason the redirect classifier does: a command should not depend on who
    // ran it. On Unix the name resolves to an ordinary file in the workdir, and
    // treating it as a sink writes nothing rather than creating litter.
    path == "/dev/null" || path.eq_ignore_ascii_case("nul")
}

/// Most of a file `read_file` returns.
///
/// Sized below [`MAX_CAPTURE_BYTES`] on purpose: shell output is usually a
/// filtered answer, while a file read is raw material and a 256 KiB file is
/// already far past any region budget an agent has. A stage that genuinely
/// wants more sets `max_result_tokens` for the tool.
pub(crate) const MAX_READ_FILE_BYTES: usize = 256 * 1024;

/// `content` truncated to `cap` bytes, with a line saying so when it was.
///
/// Said rather than silently dropped, for the reason [`capture_note`] gives: an
/// agent reading a truncated file as the whole file draws a wrong conclusion
/// from it, and the conclusion is worse than the gap.
pub(crate) fn cap_file_content(content: &str, cap: usize) -> String {
    if content.len() <= cap {
        return content.to_string();
    }
    // On a char boundary, or the result is not a `String` at all.
    let kept = leviath_core::text::substring(content, 0, cap);
    format!(
        "{kept}\n[truncated] The file is {} bytes; the first {} are shown. Read a range, or \
         narrow with a search, rather than re-reading the whole file.",
        content.len(),
        kept.len(),
    )
}

/// What one stream produced, and how much of it was kept.
#[derive(Debug)]
pub(crate) struct Captured {
    pub(crate) kept: Vec<u8>,
    /// Everything the child wrote, including what was discarded.
    pub(crate) total: u64,
}

/// Read `stream` to EOF, keeping at most `cap` bytes.
///
/// **Keeps reading after the cap is reached** rather than stopping, and that is
/// the whole design. A reader that walks away leaves the child blocked on a
/// full pipe, so a command producing more than the cap would stop making
/// progress and die at the timeout instead of finishing - turning a truncated
/// result into a failed one. Past the cap the bytes are counted and dropped.
///
/// A read error ends the capture rather than failing the call. It means no more
/// output is coming, which is what EOF means too, and the exit status still
/// describes what the command did - so reporting "failed to spawn shell" for a
/// command that ran to completion would be a lie.
///
/// `&mut dyn` rather than a generic: a generic here gets one instrumented
/// monomorphization per call site, and `cargo llvm-cov` reports the ones the
/// tests do not reach as uncovered.
pub(crate) async fn capture_capped(
    stream: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
    cap: usize,
) -> Captured {
    use tokio::io::AsyncReadExt;

    let mut kept: Vec<u8> = Vec::new();
    let mut total: u64 = 0;
    let mut buf = [0u8; 8192];
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return Captured { kept, total },
            Ok(n) => n,
        };
        total += n as u64;
        if kept.len() < cap {
            let room = cap - kept.len();
            kept.extend_from_slice(&buf[..n.min(room)]);
        }
    }
}

/// A line telling the agent its command outproduced the capture cap, or `None`
/// when everything it wrote is present.
///
/// Said rather than silently dropped: an agent that reads a truncated listing
/// as the whole listing draws a wrong conclusion from it, which is worse than
/// knowing the answer is incomplete.
pub(crate) fn capture_note(stdout: &Captured, stderr: &Captured, cap: usize) -> Option<String> {
    let lost = |c: &Captured| c.total > c.kept.len() as u64;
    let which = match (lost(stdout), lost(stderr)) {
        (false, false) => return None,
        (true, false) => "stdout",
        (false, true) => "stderr",
        (true, true) => "stdout and stderr",
    };
    let total = stdout.total + stderr.total;
    Some(format!(
        "[truncated] The command wrote {total} bytes; {which} exceeded the {cap}-byte capture \
         limit and only the beginning is shown. Narrow the command (a filter, a line count, a \
         smaller range) rather than re-running it."
    ))
}

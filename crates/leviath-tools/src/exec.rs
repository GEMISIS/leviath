//! Tool execution: dispatch, filesystem operations, and the shell.

use super::*;

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
        match canonical {
            "read_file" => self.read_file(&args).await,
            "read_files" => self.read_files(&args).await,
            "write_file" => self.write_file(&args).await,
            "edit_file" => self.edit_file(&args).await,
            "list_dir" => self.list_dir(&args).await,
            "shell" => self.shell(&args).await,
            n if n.starts_with("context_") => {
                "[error] context tools must be handled by the runtime".to_string()
            }
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
    ///    result re-checked. Without this the containment was purely textual: a
    ///    symlink at `<workdir>/link` pointing at `/` made
    ///    `read_file("link/etc/passwd")` normalize to a path that starts with the
    ///    workdir, pass, and then be followed by `fs::read_to_string`. The same
    ///    hole let `write_file` overwrite `~/.ssh/authorized_keys`.
    ///
    /// That mattered most where the containment is load-bearing. Leviath's file
    /// tools run **on the host over the bind-mounted workdir** even when the
    /// stage's `shell` is confined to a container - so a symlink the agent
    /// created inside the container escaped the container through the file
    /// tools. It also matters for a freshly cloned repository, which is exactly
    /// what a coding agent operates on and which can carry a checked-in symlink
    /// pointing anywhere.
    ///
    /// The check is not TOCTOU-proof: a symlink planted between this call and the
    /// subsequent `open` still wins. Closing that needs `openat`/`O_NOFOLLOW`
    /// throughout, which is a larger change; this stops the planted-symlink case,
    /// which is the one an agent can actually arrange.
    pub(crate) fn resolve(&self, requested: &str) -> anyhow::Result<PathBuf> {
        Self::resolve_within(requested, &self.ctx.workdir, resolves_within)
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
        match Self::resolve_within(requested, &self.ctx.workdir, resolves_within) {
            Ok(path) => Ok(path),
            Err(workdir_err) => {
                if !self.ctx.read_paths.is_active() {
                    return Err(workdir_err);
                }
                Self::resolve_outside(
                    requested,
                    &self.ctx.workdir,
                    &self.ctx.read_paths,
                    leviath_core::canonicalize_for_match,
                )
            }
        }
    }

    /// The out-of-workdir arm of [`resolve_read`](Self::resolve_read), with
    /// the canonicalizer injected (`fn` pointer, same seam idiom as
    /// [`resolve_within`](Self::resolve_within)) so the fail-closed refusal is
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
        // unresolvable no matter what any allowlist says. `Path::components`
        // already drops interior `.`, and `raw` is absolute here (an absolute
        // request, or a relative one joined onto the canonicalized workdir),
        // so no `CurDir` survives to this loop - the catch-all mirrors
        // `resolve_within`.
        let mut normalized = PathBuf::new();
        for component in raw.components() {
            match component {
                Component::ParentDir => {
                    if !normalized.pop() {
                        anyhow::bail!("path '{requested}' cannot be resolved");
                    }
                }
                other => normalized.push(other),
            }
        }

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

    /// Core of [`resolve`](Self::resolve) with the containment check injected.
    ///
    /// A `fn` pointer (not `impl Fn`) so there is one monomorphization, matching
    /// the seam idiom used elsewhere in the workspace. The seam exists because
    /// the refusal cannot be reached otherwise on every platform: producing the
    /// escape needs a real symlink, and creating one on Windows requires a
    /// privilege CI runners do not have. Injecting the predicate lets the
    /// refusal itself be tested everywhere, while the `#[cfg(unix)]` tests below
    /// still prove the real filesystem behaviour end to end.
    pub(crate) fn resolve_within(
        requested: &str,
        workdir: &Path,
        within: fn(&Path, &Path) -> bool,
    ) -> anyhow::Result<PathBuf> {
        let raw = if Path::new(requested).is_absolute() {
            PathBuf::from(requested)
        } else {
            workdir.join(requested)
        };

        // Normalize by resolving .. and . without requiring the path to exist.
        let mut normalized = PathBuf::new();
        for component in raw.components() {
            match component {
                Component::ParentDir => {
                    if !normalized.pop() {
                        anyhow::bail!("path '{}' escapes the working directory", requested);
                    }
                }
                c => normalized.push(c),
            }
        }

        if !normalized.starts_with(workdir) {
            anyhow::bail!("path '{}' would escape the working directory", requested);
        }

        if !within(&normalized, workdir) {
            anyhow::bail!(
                "path '{requested}' resolves outside the working directory through a symlink"
            );
        }

        Ok(normalized)
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
            Ok(content) => content,
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
        #[cfg(windows)]
        {
            ("cmd.exe", "/C")
        }

        #[cfg(not(windows))]
        {
            Self::detect_shell_impl(std::env::var("SHELL").ok(), &|s| {
                std::path::Path::new(s).exists()
            })
        }
    }

    /// Core shell-detection logic with injectable env and filesystem checks
    /// for testing.
    ///
    /// `shell_exists` is a trait object (`&dyn Fn(&str) -> bool`) rather
    /// than `impl Fn(&str) -> bool` so every caller - production's real
    /// `Path::exists` closure and each test's distinct closure - shares
    /// exactly ONE monomorphization of this function instead of one per
    /// closure type (this function was a confirmed generic-monomorphization
    /// coverage-attribution artifact: every source position had a covered
    /// instantiation, but the summary table still reported some as missed).
    #[cfg(not(windows))]
    pub(crate) fn detect_shell_impl(
        env_shell: Option<String>,
        shell_exists: &dyn Fn(&str) -> bool,
    ) -> (&'static str, &'static str) {
        if let Some(shell) = env_shell
            && (shell.ends_with("/zsh") || shell.ends_with("/bash") || shell.ends_with("/sh"))
            && shell_exists(&shell)
        {
            // Only trust `$SHELL` when it actually exists - a stale or
            // sandbox-missing `$SHELL` (e.g. `/bin/zsh` in an environment that
            // doesn't ship it) otherwise made every shell call fail to spawn.
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
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "[error] missing 'command' argument".to_string(),
        };

        let workdir = self.ctx.workdir.clone();
        let (shell, flag) = Self::detect_shell();

        // When a sandbox executor is attached, it builds a command that runs
        // inside a container / namespace (still targeting `workdir`); otherwise
        // run the shell directly on the host - the exact prior behavior.
        let mut cmd = match &self.shell_executor {
            Some(executor) => executor.build_command(shell, flag, command, &workdir),
            None => {
                let mut c = Command::new(shell);
                c.arg(flag).arg(command).current_dir(&workdir);
                c
            }
        };
        // Reap the whole command on drop, not just the shell.
        //
        // Dropping a `Command` future detaches its process by default, so a
        // cancelled agent (or an elapsed timeout, which drops the future the
        // same way) left its shell running: the run vanished from every listing
        // while its command carried on writing to the workspace. `kill_on_drop`
        // fixes the shell - but only the shell. Anything the shell itself
        // started (`sleep 400 && …`) is a *grandchild*, gets reparented to init,
        // and keeps running. Putting the shell in its own process group and
        // signalling the group on drop takes the whole tree down with it.
        cmd.kill_on_drop(true);
        own_process_group(&mut cmd);
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
            let child = cmd.spawn()?;
            // The child leads its own group, so its pid is the group id.
            let _reaper = child.id().map(ProcessGroupReaper);
            child.wait_with_output().await
        };

        match timeout(timeout_duration, run).await {
            Err(_) => format!("[timed out] Command exceeded 60s: {}", command),
            Ok(Err(e)) => format!("[error] Failed to spawn shell '{}': {}", shell, e),
            Ok(Ok(output)) => Self::format_command_output(
                &output.stdout,
                &output.stderr,
                output.status.success(),
                output.status.code().unwrap_or(-1),
            ),
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

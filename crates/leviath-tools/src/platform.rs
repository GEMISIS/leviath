//! Platform capabilities and process-group management.

use super::*;

/// Put `cmd` in its own process group, so the whole command tree can be
/// signalled as one. `leviath_sys::configure_detached` does this for a
/// `std::process::Command`; the tool lane uses tokio's, which has its own
/// (Unix-only) setter. A no-op where the platform has no process groups -
/// there, killing the direct child is all that exists.
pub fn own_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(not(unix))]
    let _ = cmd;
}

/// Build a child-process command with no console window.
///
/// Re-exported from [`leviath_sys::child_command_async`] so the tool lane names
/// it without reaching across crates at every call site. The decision lives
/// there, once, for both command flavours - see that function for why it is
/// made at construction rather than by a second call the caller can forget.
pub use leviath_sys::child_command_async as child_command;

/// SIGKILLs a shell's whole process group when dropped.
///
/// `kill_on_drop` reaps the shell itself; this reaps what the shell started.
/// Held for the duration of one shell tool call, so it fires on every exit path -
/// normal completion (where the group is already gone and the signal is a
/// harmless no-op), timeout, and the future being dropped because the agent was
/// cancelled.
pub struct ProcessGroupReaper(pub u32);

impl Drop for ProcessGroupReaper {
    fn drop(&mut self) {
        // The group is usually already gone; failing to signal it is expected.
        let _ = leviath_sys::kill_process_group(self.0);
    }
}

/// A platform feature a built-in tool depends on.
///
/// Each built-in declares the capabilities it requires (see
/// [`tool_required_capabilities`]); the current platform declares what it
/// provides (see [`PlatformCapabilities`]). A tool whose requirements aren't
/// met by the platform simply doesn't register - it's dropped from
/// advertisement, name-recognition, and dispatch. This lets platform-specific
/// tools (like `shell`, which spawns OS processes) coexist without per-tool
/// `#[cfg]` gates or one-off boolean flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolCapability {
    /// Can launch child processes (e.g. the `shell` tool, stdio MCP servers).
    ProcessSpawn,
    /// Can read and write files.
    FileSystem,
    /// Can make outbound network/HTTP requests.
    Network,
}

/// The set of [`ToolCapability`]s the current platform provides.
///
/// Detection is compile-time: [`current`](Self::current) resolves to the
/// capability set for the build target. Desktop targets provide everything; a
/// future mobile or wasm host would provide a reduced set (no
/// [`ProcessSpawn`](ToolCapability::ProcessSpawn)). This mirrors the
/// `leviath-sys` crate's approach of centralizing platform `#[cfg]` branching
/// in one place rather than scattering it across call sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformCapabilities {
    capabilities: HashSet<ToolCapability>,
}

impl PlatformCapabilities {
    /// Capabilities for a desktop host: process spawning, filesystem, network.
    pub fn desktop() -> Self {
        Self {
            capabilities: HashSet::from([
                ToolCapability::ProcessSpawn,
                ToolCapability::FileSystem,
                ToolCapability::Network,
            ]),
        }
    }

    /// Capabilities for a mobile host: filesystem and network, but no process
    /// spawning (so the `shell` tool doesn't register).
    pub fn mobile() -> Self {
        Self {
            capabilities: HashSet::from([ToolCapability::FileSystem, ToolCapability::Network]),
        }
    }

    /// The capability set for the platform this binary was built for.
    ///
    /// Only desktop targets are built today, so this resolves to
    /// [`desktop`](Self::desktop). A future mobile/wasm target adds a `cfg` arm
    /// here returning [`mobile`](Self::mobile) - every built-in then auto-filters
    /// against it with no other change.
    pub fn current() -> Self {
        Self::desktop()
    }

    /// Build a capability set from an explicit collection (tests, future hosts).
    pub fn from_capabilities(caps: impl IntoIterator<Item = ToolCapability>) -> Self {
        Self {
            capabilities: caps.into_iter().collect(),
        }
    }

    /// Whether the platform provides `cap`.
    pub fn supports(&self, cap: ToolCapability) -> bool {
        self.capabilities.contains(&cap)
    }

    /// Whether the platform provides *every* capability in `required`. An empty
    /// requirement slice is always satisfied.
    pub fn satisfies(&self, required: &[ToolCapability]) -> bool {
        required.iter().all(|c| self.supports(*c))
    }
}

impl Default for PlatformCapabilities {
    fn default() -> Self {
        Self::current()
    }
}

/// The capabilities a built-in tool requires to function.
///
/// Keyed by *canonical* tool name (resolve aliases via [`canonical_tool_name`]
/// first). An empty slice means the tool is platform-agnostic and always
/// available - this covers the runtime-handled tools (`present_for_review`,
/// `ask_user_*`, `edit_document`, `context_*`) which touch neither the OS
/// process table nor the filesystem directly.
pub fn tool_required_capabilities(canonical_name: &str) -> &'static [ToolCapability] {
    match canonical_name {
        "shell" => &[ToolCapability::ProcessSpawn],
        // `install_tool` writes the global tools directory: a filesystem write
        // like `write_file`, outside the workdir but on the same disk.
        "read_file" | "read_files" | "write_file" | "edit_file" | "list_dir" | "install_tool" => {
            &[ToolCapability::FileSystem]
        }
        _ => &[],
    }
}

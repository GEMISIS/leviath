//! Sandboxed tool-execution configuration.
//!
//! Describes *where* an agent's shell/command tools run — directly on the host
//! ([`SandboxKind::None`], the default), inside a fresh set of Linux namespaces
//! ([`SandboxKind::Namespace`], via `unshare(1)`), or inside a container
//! ([`SandboxKind::Container`], via Docker/Podman). Only shell command execution
//! is affected; file tools are already path-confined to the agent's workdir,
//! which the sandbox bind-mounts.
//!
//! The type follows the same "optional block at agent + stage level, cascade
//! through a global default" shape as [`crate::SecurityConfig`]; see
//! [`resolve_sandbox`]. It is named `ToolSandboxConfig` to avoid colliding with
//! the unrelated Rhai `SandboxConfig` in `leviath-scripting`.

use serde::{Deserialize, Serialize};

/// The isolation mechanism used for an agent's shell tool execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SandboxKind {
    /// Run directly on the host — current behavior, explicit opt-out.
    #[default]
    None,
    /// Run under fresh Linux namespaces via `unshare(1)` (Linux only).
    Namespace,
    /// Run inside a container via Docker or Podman.
    Container,
}

/// What to do when the configured sandbox runtime can't be established (e.g. no
/// container engine on `PATH`, or `namespace` requested on a non-Linux host).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnUnavailable {
    /// Fail agent spawn with a clear error. The safe default for untrusted code.
    #[default]
    Error,
    /// Log a warning and fall back to host execution.
    Warn,
}

/// Sandbox configuration for tool execution, at either the agent or the stage
/// level. A present `[sandbox]` block (agent or stage) overrides broader levels;
/// see [`resolve_sandbox`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSandboxConfig {
    /// The isolation mechanism. Defaults to [`SandboxKind::None`] (host).
    #[serde(default)]
    pub kind: SandboxKind,
    /// Container image (e.g. `"ubuntu:24.04"`). Required for
    /// [`SandboxKind::Container`]; ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Container engine binary to use (e.g. `"docker"`, `"podman"`, `"nerdctl"`,
    /// `"finch"`). `None` auto-detects (Docker, then Podman). Leviath isn't
    /// prescriptive — any Docker-CLI-compatible binary works. Container kind only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// Whether the sandbox has network access. `false` isolates the network.
    #[serde(default = "default_true")]
    pub network: bool,
    /// Extra host paths to bind-mount into the sandbox. The agent's workdir is
    /// always mounted regardless of this list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<String>,
    /// Keep a container warm across the agent's stages rather than tearing it
    /// down between them. Ignored for non-container kinds.
    #[serde(default)]
    pub persist: bool,
    /// What to do when the runtime is unavailable.
    #[serde(default)]
    pub on_unavailable: OnUnavailable,
}

fn default_true() -> bool {
    true
}

impl Default for ToolSandboxConfig {
    fn default() -> Self {
        // A present `[sandbox]` block with no `kind` means "host" (no-op) — unlike
        // `SecurityConfig`, an empty block is not "turn everything on". Callers
        // still cascade through `resolve_sandbox` so "no block" inherits the global
        // default (also host).
        Self {
            kind: SandboxKind::None,
            image: None,
            engine: None,
            network: true,
            mounts: Vec::new(),
            persist: false,
            on_unavailable: OnUnavailable::Error,
        }
    }
}

impl ToolSandboxConfig {
    /// Whether this config actually isolates execution (i.e. is not host-passthrough).
    pub fn is_active(&self) -> bool {
        self.kind != SandboxKind::None
    }
}

/// Resolve the effective [`ToolSandboxConfig`] for a stage: the most specific
/// present config (stage over agent), or the global default, or host when nothing
/// is set. Mirrors [`crate::taint::resolve_security`].
pub fn resolve_sandbox(
    global: Option<&ToolSandboxConfig>,
    agent: Option<&ToolSandboxConfig>,
    stage: Option<&ToolSandboxConfig>,
) -> ToolSandboxConfig {
    if let Some(s) = stage {
        return s.clone();
    }
    if let Some(a) = agent {
        return a.clone();
    }
    if let Some(g) = global {
        return g.clone();
    }
    ToolSandboxConfig::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_host_passthrough() {
        let c = ToolSandboxConfig::default();
        assert_eq!(c.kind, SandboxKind::None);
        assert!(!c.is_active());
        assert!(c.network);
        assert_eq!(c.on_unavailable, OnUnavailable::Error);
    }

    #[test]
    fn stage_overrides_agent_and_global() {
        let global = ToolSandboxConfig {
            kind: SandboxKind::Namespace,
            ..Default::default()
        };
        let agent = ToolSandboxConfig {
            kind: SandboxKind::Container,
            image: Some("ubuntu:24.04".into()),
            ..Default::default()
        };
        let stage = ToolSandboxConfig {
            kind: SandboxKind::Container,
            image: Some("node:22-slim".into()),
            network: false,
            ..Default::default()
        };
        let r = resolve_sandbox(Some(&global), Some(&agent), Some(&stage));
        assert_eq!(r.image.as_deref(), Some("node:22-slim"));
        assert!(!r.network);
    }

    #[test]
    fn agent_overrides_global_when_no_stage() {
        let global = ToolSandboxConfig {
            kind: SandboxKind::Namespace,
            ..Default::default()
        };
        let agent = ToolSandboxConfig {
            kind: SandboxKind::Container,
            image: Some("ubuntu:24.04".into()),
            ..Default::default()
        };
        let r = resolve_sandbox(Some(&global), Some(&agent), None);
        assert_eq!(r.kind, SandboxKind::Container);
        assert_eq!(r.image.as_deref(), Some("ubuntu:24.04"));
    }

    #[test]
    fn global_used_when_no_agent_or_stage() {
        let global = ToolSandboxConfig {
            kind: SandboxKind::Namespace,
            ..Default::default()
        };
        let r = resolve_sandbox(Some(&global), None, None);
        assert_eq!(r.kind, SandboxKind::Namespace);
    }

    #[test]
    fn nothing_set_is_host() {
        let r = resolve_sandbox(None, None, None);
        assert_eq!(r.kind, SandboxKind::None);
    }

    #[test]
    fn is_active_true_for_isolating_kinds() {
        for kind in [SandboxKind::Namespace, SandboxKind::Container] {
            let c = ToolSandboxConfig {
                kind,
                ..Default::default()
            };
            assert!(c.is_active());
        }
    }

    #[test]
    fn deserializes_and_applies_field_defaults() {
        // `network` omitted → `default_true`; other omitted fields fall back too.
        let c: ToolSandboxConfig =
            toml::from_str("kind = \"container\"\nimage = \"alpine\"").unwrap();
        assert!(c.network);
        assert!(c.is_active());
        assert_eq!(c.on_unavailable, OnUnavailable::Error);
        assert!(c.mounts.is_empty());
        assert!(!c.persist);
    }
}

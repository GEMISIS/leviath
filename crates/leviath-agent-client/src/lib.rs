//! # Leviath Agent Client Protocol
//!
//! Wire types and Leviath mappings for the [Agent **Client** Protocol][acp] - the
//! JSON-RPC 2.0 protocol agent hosts (Zed, Gas City, …) use to drive a headless
//! agent over **stdio**.
//!
//! ## Naming
//!
//! The bare acronym "ACP" is deliberately never used unqualified anywhere in this
//! workspace: it is claimed by two unrelated protocols. This crate implements the
//! Agent **Client** Protocol (JSON-RPC over stdio, the one implemented here). The
//! Agent **Communication** Protocol (a REST + SSE API from the BeeAI project,
//! since folded into A2A) is a different thing entirely and is not implemented
//! anywhere in this workspace.
//!
//! ## Scope
//!
//! This crate is deliberately **pure**: wire types plus total functions over
//! them. It has no transport, no I/O, no async runtime, and no knowledge of the
//! Leviath daemon. The stdio server that drives it lives in `leviath-cli`
//! (`commands::agent_client`).
//!
//! Framing is **newline-delimited JSON**: exactly one compact JSON message per
//! line. See [`protocol::MAX_FRAME_BYTES`] for the size ceiling this implies.
//!
//! [acp]: https://agentclientprotocol.com

pub mod mapping;
pub mod protocol;

pub use mapping::{
    flatten_prompt, is_permission_request, parse_region_markers, permission_request,
    stop_reason_for, stop_reason_for_label,
};
pub use protocol::{
    AgentCapabilities, AgentInfo, ClientCapabilities, ContentBlock, EmbeddedResource,
    InitializeParams, InitializeResult, JsonRpcError, JsonRpcMessage, MAX_FRAME_BYTES,
    PROTOCOL_VERSION, PermissionOption, PermissionOptionKind, PermissionOutcome,
    PromptCapabilities, RequestPermissionParams, RequestPermissionResult, SessionCancelParams,
    SessionNewParams, SessionNewResult, SessionPromptParams, SessionPromptResult, SessionUpdate,
    SessionUpdateParams, StopReason, ToolCallRef, ToolCallStatus, ToolKind, error_codes,
};

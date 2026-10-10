//! A tool as a model is offered it.
//!
//! Here rather than beside the providers that send it, because the built-in
//! tools describe themselves with it and have no other reason to depend on a
//! provider.

use serde::{Deserialize, Serialize};

/// A tool that can be called by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    /// Tool name
    pub name: String,

    /// Tool description
    pub description: String,

    /// JSON schema for tool parameters
    pub parameters: serde_json::Value,
}

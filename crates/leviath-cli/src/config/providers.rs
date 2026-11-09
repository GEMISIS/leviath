//! `[providers]` and `[model_providers]`: which endpoint a stage's model
//! resolves to, and the credentials for it.
//!
//! The types are the runtime's, so that `lev` and a host embedding the runtime
//! read these tables through one definition and build providers from them with
//! one piece of code.

pub use leviath_runtime::user_creds::{ModelProviderConfig, ModelProviderKind, ProviderConfig};

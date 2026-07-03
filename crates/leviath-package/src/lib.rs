//! # Leviath Package
//!
//! Agent packaging, sharing, and installation.
//!
//! Provides tools for bundling agents with their blueprints and dependencies,
//! sharing them via registries, and installing them locally.

pub mod bundler;
pub mod installer;
pub mod manifest;
pub mod registry;
#[cfg(test)]
mod test_support;

pub use bundler::AgentBundler;
pub use installer::{AgentInstaller, InstalledAgent};
pub use manifest::PackageManifest;
pub use registry::{PackageInfo, PackageRegistry};

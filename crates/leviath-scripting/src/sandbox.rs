//! Sandbox configuration for Rhai scripts.

/// Configuration for the Rhai sandbox.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Maximum number of operations before script is terminated
    pub max_operations: usize,

    /// Maximum string size in characters
    pub max_string_size: usize,

    /// Maximum array size
    pub max_array_size: usize,

    /// Maximum map size
    pub max_map_size: usize,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            max_operations: 100_000,
            max_string_size: 1_000_000,
            max_array_size: 10_000,
            max_map_size: 10_000,
        }
    }
}

impl SandboxConfig {
    /// Create a new sandbox configuration with custom limits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the maximum number of operations.
    pub fn with_max_operations(mut self, max: usize) -> Self {
        self.max_operations = max;
        self
    }

    /// Set the maximum string size.
    pub fn with_max_string_size(mut self, max: usize) -> Self {
        self.max_string_size = max;
        self
    }

    /// Set the maximum array size.
    pub fn with_max_array_size(mut self, max: usize) -> Self {
        self.max_array_size = max;
        self
    }

    /// Set the maximum map size.
    pub fn with_max_map_size(mut self, max: usize) -> Self {
        self.max_map_size = max;
        self
    }
}

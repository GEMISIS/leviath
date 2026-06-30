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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_expected_values() {
        let cfg = SandboxConfig::default();
        assert_eq!(cfg.max_operations, 100_000);
        assert_eq!(cfg.max_string_size, 1_000_000);
        assert_eq!(cfg.max_array_size, 10_000);
        assert_eq!(cfg.max_map_size, 10_000);
    }

    #[test]
    fn new_same_as_default() {
        let new = SandboxConfig::new();
        let def = SandboxConfig::default();
        assert_eq!(new.max_operations, def.max_operations);
        assert_eq!(new.max_string_size, def.max_string_size);
        assert_eq!(new.max_array_size, def.max_array_size);
        assert_eq!(new.max_map_size, def.max_map_size);
    }

    #[test]
    fn with_max_operations() {
        let cfg = SandboxConfig::new().with_max_operations(500);
        assert_eq!(cfg.max_operations, 500);
        // other fields remain default
        assert_eq!(cfg.max_string_size, 1_000_000);
    }

    #[test]
    fn with_max_string_size() {
        let cfg = SandboxConfig::new().with_max_string_size(2048);
        assert_eq!(cfg.max_string_size, 2048);
        assert_eq!(cfg.max_operations, 100_000);
    }

    #[test]
    fn with_max_array_size() {
        let cfg = SandboxConfig::new().with_max_array_size(50);
        assert_eq!(cfg.max_array_size, 50);
        assert_eq!(cfg.max_map_size, 10_000);
    }

    #[test]
    fn with_max_map_size() {
        let cfg = SandboxConfig::new().with_max_map_size(99);
        assert_eq!(cfg.max_map_size, 99);
        assert_eq!(cfg.max_array_size, 10_000);
    }

    #[test]
    fn chaining_multiple_builders() {
        let cfg = SandboxConfig::new()
            .with_max_operations(1)
            .with_max_string_size(2)
            .with_max_array_size(3)
            .with_max_map_size(4);
        assert_eq!(cfg.max_operations, 1);
        assert_eq!(cfg.max_string_size, 2);
        assert_eq!(cfg.max_array_size, 3);
        assert_eq!(cfg.max_map_size, 4);
    }

    #[test]
    fn clone_preserves_values() {
        let cfg = SandboxConfig::new().with_max_operations(42);
        let cloned = cfg.clone();
        assert_eq!(cloned.max_operations, 42);
    }

    #[test]
    fn debug_impl_exists() {
        let cfg = SandboxConfig::new();
        let debug = format!("{cfg:?}");
        assert!(debug.contains("SandboxConfig"));
        assert!(debug.contains("100000"));
    }
}

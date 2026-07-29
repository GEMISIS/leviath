//! Repetition detection for agent inference loops.
//!
//! Detects degenerate read loops where agents call the same tool with identical
//! arguments many times without taking productive action (write, edit, bash).
//! When a threshold is hit, returns a nudge message to inject into the
//! conversation so the model breaks out of the loop.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use bevy_ecs::prelude::Component;

/// Configuration for repetition detection thresholds.
#[derive(Debug, Clone)]
pub struct RepetitionConfig {
    /// Maximum times the same tool+args combo may repeat before a nudge.
    pub max_repeat_calls: usize,
    /// Maximum consecutive read-only calls (no productive calls in between).
    pub max_readonly_streak: usize,
    /// Whether detection is enabled.
    pub enabled: bool,
}

impl Default for RepetitionConfig {
    fn default() -> Self {
        Self {
            max_repeat_calls: 3,
            max_readonly_streak: 10,
            enabled: true,
        }
    }
}

/// Tracks tool call patterns and detects repetitive loops. A per-agent ECS
/// component (seeded from the blueprint's repetition config at spawn).
#[derive(Debug, Component)]
pub struct RepetitionDetector {
    config: RepetitionConfig,
    /// Counts keyed by (tool_name, args_hash).
    call_counts: HashMap<(String, u64), usize>,
    /// Consecutive read-only calls with no productive call in between.
    readonly_streak: usize,
}

/// Tools considered "read-only" (no side effects).
const READONLY_TOOLS: &[&str] = &[
    "read_file",
    "list_dir",
    "list_files",
    "glob",
    "grep",
    "search",
    "context_read",
    "context_list",
];

/// Tools considered "productive" (produce side effects / forward progress).
const PRODUCTIVE_TOOLS: &[&str] = &[
    "write_file",
    "edit_file",
    "bash",
    "run_command",
    "execute",
    "shell",
    "context_write",
    "context_append",
    "context_delete",
];

fn hash_arguments(args: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    args.hash(&mut hasher);
    hasher.finish()
}

fn is_readonly(tool_name: &str) -> bool {
    READONLY_TOOLS.contains(&tool_name)
}

fn is_productive(tool_name: &str) -> bool {
    PRODUCTIVE_TOOLS.contains(&tool_name)
}

impl RepetitionDetector {
    /// Create a new detector with the given configuration.
    pub fn new(config: RepetitionConfig) -> Self {
        Self {
            config,
            call_counts: HashMap::new(),
            readonly_streak: 0,
        }
    }

    /// Create a detector with default thresholds.
    pub fn with_defaults() -> Self {
        Self::new(RepetitionConfig::default())
    }

    /// Build a detector from a blueprint's
    /// [`RepetitionDetectionConfig`](leviath_core::blueprint::RepetitionDetectionConfig),
    /// filling any unset field from the [`RepetitionConfig`] defaults.
    pub fn from_detection_config(cfg: &leviath_core::blueprint::RepetitionDetectionConfig) -> Self {
        let d = RepetitionConfig::default();
        Self::new(RepetitionConfig {
            max_repeat_calls: cfg.max_repeat_calls.unwrap_or(d.max_repeat_calls),
            max_readonly_streak: cfg.max_readonly_streak.unwrap_or(d.max_readonly_streak),
            enabled: cfg.enabled.unwrap_or(d.enabled),
        })
    }

    /// Record a tool call and return a nudge message if a threshold is hit.
    ///
    /// Returns `Some(nudge_message)` when the agent should be nudged to break
    /// out of a loop, `None` otherwise.
    pub fn record_call(&mut self, tool_name: &str, tool_arguments: &str) -> Option<String> {
        if !self.config.enabled {
            return None;
        }

        // Reset counters on productive calls
        if is_productive(tool_name) {
            self.call_counts.clear();
            self.readonly_streak = 0;
            return None;
        }

        // Track readonly streak
        if is_readonly(tool_name) {
            self.readonly_streak += 1;
        }

        // Track same-call repetition
        let args_hash = hash_arguments(tool_arguments);
        let key = (tool_name.to_string(), args_hash);
        let count = self.call_counts.entry(key).or_insert(0);
        *count += 1;

        // Check repeat threshold
        if *count >= self.config.max_repeat_calls {
            let count_val = *count;
            return Some(format!(
                "You have called {tool_name} with the same arguments {count_val} times. \
                 This appears to be a loop. Take a different action - try writing code \
                 with write_file, editing with edit_file, running commands with bash, \
                 or storing analysis with context_write."
            ));
        }

        // Check readonly streak threshold
        if self.readonly_streak >= self.config.max_readonly_streak {
            let streak = self.readonly_streak;
            return Some(format!(
                "You have made {streak} consecutive read-only tool calls \
                 (read_file/list_dir) without any writes or command execution. \
                 Break this pattern - write a file, edit code, run a command, \
                 or use context_write to store your analysis."
            ));
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_detection_config_maps_set_fields_and_defaults_unset() {
        use leviath_core::blueprint::RepetitionDetectionConfig;
        let all = RepetitionDetectionConfig {
            max_repeat_calls: Some(1),
            max_readonly_streak: Some(2),
            enabled: Some(false),
        };
        let d = RepetitionDetector::from_detection_config(&all);
        assert_eq!(d.config.max_repeat_calls, 1);
        assert_eq!(d.config.max_readonly_streak, 2);
        assert!(!d.config.enabled);

        let unset = RepetitionDetectionConfig {
            max_repeat_calls: None,
            max_readonly_streak: None,
            enabled: None,
        };
        let d2 = RepetitionDetector::from_detection_config(&unset);
        assert_eq!(d2.config.max_repeat_calls, 3);
        assert_eq!(d2.config.max_readonly_streak, 10);
        assert!(d2.config.enabled);
    }

    #[test]
    fn test_repeat_triggers_nudge_at_threshold() {
        let config = RepetitionConfig {
            max_repeat_calls: 3,
            max_readonly_streak: 10,
            enabled: true,
        };
        let mut detector = RepetitionDetector::new(config);

        assert!(
            detector
                .record_call("read_file", r#"{"path":"src/main.rs"}"#)
                .is_none()
        );
        assert!(
            detector
                .record_call("read_file", r#"{"path":"src/main.rs"}"#)
                .is_none()
        );

        let nudge = detector.record_call("read_file", r#"{"path":"src/main.rs"}"#);
        assert!(nudge.is_some());
        let msg = nudge.unwrap();
        assert!(msg.contains("read_file"));
        assert!(msg.contains("3 times"));
    }

    #[test]
    fn test_productive_call_resets_counter() {
        let mut detector = RepetitionDetector::with_defaults();

        // Read twice
        detector.record_call("read_file", r#"{"path":"foo.py"}"#);
        detector.record_call("read_file", r#"{"path":"foo.py"}"#);

        // Productive call resets
        assert!(
            detector
                .record_call("write_file", r#"{"path":"foo.py","content":"x"}"#)
                .is_none()
        );

        // Read again - counter should be back to 1
        assert!(
            detector
                .record_call("read_file", r#"{"path":"foo.py"}"#)
                .is_none()
        );
        assert!(
            detector
                .record_call("read_file", r#"{"path":"foo.py"}"#)
                .is_none()
        );

        // Third read triggers nudge again
        let nudge = detector.record_call("read_file", r#"{"path":"foo.py"}"#);
        assert!(nudge.is_some());
    }

    #[test]
    fn test_readonly_streak_detection() {
        let config = RepetitionConfig {
            max_repeat_calls: 100, // high so repeat doesn't trigger first
            max_readonly_streak: 5,
            enabled: true,
        };
        let mut detector = RepetitionDetector::new(config);

        // 5 different read-only calls
        for i in 0..4 {
            assert!(
                detector
                    .record_call("read_file", &format!(r#"{{"path":"file{i}.rs"}}"#))
                    .is_none()
            );
        }
        let nudge = detector.record_call("list_dir", r#"{"path":"src/"}"#);
        assert!(nudge.is_some());
        assert!(nudge.unwrap().contains("5 consecutive read-only"));
    }

    #[test]
    fn test_different_arguments_dont_trigger_repeat() {
        let mut detector = RepetitionDetector::with_defaults();

        assert!(
            detector
                .record_call("read_file", r#"{"path":"a.rs"}"#)
                .is_none()
        );
        assert!(
            detector
                .record_call("read_file", r#"{"path":"b.rs"}"#)
                .is_none()
        );
        assert!(
            detector
                .record_call("read_file", r#"{"path":"c.rs"}"#)
                .is_none()
        );
        // No nudge - all different args
    }

    #[test]
    fn test_configurable_thresholds() {
        let config = RepetitionConfig {
            max_repeat_calls: 2,
            max_readonly_streak: 100,
            enabled: true,
        };
        let mut detector = RepetitionDetector::new(config);

        assert!(
            detector
                .record_call("read_file", r#"{"path":"x.rs"}"#)
                .is_none()
        );
        let nudge = detector.record_call("read_file", r#"{"path":"x.rs"}"#);
        assert!(nudge.is_some());
    }

    #[test]
    fn test_disabled_detector_never_nudges() {
        let config = RepetitionConfig {
            max_repeat_calls: 1,
            max_readonly_streak: 1,
            enabled: false,
        };
        let mut detector = RepetitionDetector::new(config);

        assert!(
            detector
                .record_call("read_file", r#"{"path":"x.rs"}"#)
                .is_none()
        );
        assert!(
            detector
                .record_call("read_file", r#"{"path":"x.rs"}"#)
                .is_none()
        );
    }

    #[test]
    fn test_productive_call_resets_readonly_streak() {
        let config = RepetitionConfig {
            max_repeat_calls: 100,
            max_readonly_streak: 3,
            enabled: true,
        };
        let mut detector = RepetitionDetector::new(config);

        detector.record_call("read_file", r#"{"path":"a.rs"}"#);
        detector.record_call("read_file", r#"{"path":"b.rs"}"#);
        // Productive call resets streak
        detector.record_call("bash", r#"{"command":"cargo test"}"#);

        detector.record_call("read_file", r#"{"path":"c.rs"}"#);
        detector.record_call("read_file", r#"{"path":"d.rs"}"#);
        // Only 2 in streak after reset, so no nudge
        assert!(
            detector
                .record_call("list_dir", r#"{"path":"src/"}"#)
                .is_some()
        ); // 3 = threshold
    }

    #[test]
    fn test_non_readonly_non_productive_doesnt_affect_streak() {
        let config = RepetitionConfig {
            max_repeat_calls: 100,
            max_readonly_streak: 3,
            enabled: true,
        };
        let mut detector = RepetitionDetector::new(config);

        detector.record_call("read_file", r#"{"path":"a.rs"}"#);
        detector.record_call("read_file", r#"{"path":"b.rs"}"#);
        // Unknown tool: not readonly, not productive - doesn't increment or reset streak
        detector.record_call("some_other_tool", r#"{}"#);
        // Streak is still 2
        assert!(
            detector
                .record_call("read_file", r#"{"path":"c.rs"}"#)
                .is_some()
        ); // 3 = threshold
    }
}

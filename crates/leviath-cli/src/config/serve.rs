//! `[serve]` in `~/.leviath/config.toml`: what `lev serve` will take on at
//! once, and how long it gives any one request.

use serde::{Deserialize, Serialize};

/// In-flight requests `lev serve` admits before answering 503.
pub(crate) const DEFAULT_MAX_CONCURRENT_REQUESTS: u64 = 64;

/// Seconds a request may take before `lev serve` answers 408.
pub(crate) const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

/// `[serve]` in `~/.leviath/config.toml`.
///
/// Both keys are ceilings on the HTTP API, not on the runs behind it: a spawn
/// that takes the daemon a minute still spawns, because the route answers as
/// soon as the daemon has accepted it. The websocket routes are outside both,
/// since a subscription is meant to stay open. `0` disables a limit. The
/// `lev serve` flags of the same name win over these.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServeConfig {
    /// Requests in flight at once before the next one is answered 503.
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: u64,

    /// Seconds one request may take before it is answered 408.
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
}

fn default_max_concurrent_requests() -> u64 {
    DEFAULT_MAX_CONCURRENT_REQUESTS
}

fn default_request_timeout_secs() -> u64 {
    DEFAULT_REQUEST_TIMEOUT_SECS
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            max_concurrent_requests: DEFAULT_MAX_CONCURRENT_REQUESTS,
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_table_and_an_empty_one_both_mean_the_defaults() {
        let empty: ServeConfig = toml::from_str("").unwrap();
        assert_eq!(empty, ServeConfig::default());
        assert_eq!(empty.max_concurrent_requests, 64);
        assert_eq!(empty.request_timeout_secs, 30);
    }

    #[test]
    fn each_key_is_read_on_its_own() {
        let only_cap: ServeConfig = toml::from_str("max_concurrent_requests = 8").unwrap();
        assert_eq!(only_cap.max_concurrent_requests, 8);
        assert_eq!(only_cap.request_timeout_secs, 30);

        let only_timeout: ServeConfig = toml::from_str("request_timeout_secs = 0").unwrap();
        assert_eq!(only_timeout.max_concurrent_requests, 64);
        assert_eq!(only_timeout.request_timeout_secs, 0);
    }
}

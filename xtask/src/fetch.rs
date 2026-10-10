//! What the three table refreshers (`prices`, `modalities` and
//! `bedrock-windows`) share: the fetch they read their sources through, the
//! error that tells a failed fetch apart from bad data, the result a run
//! reports, and where the tables live.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Result;

/// How long one fetch may wait before it is called a network failure.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A fetch of a URL to its body. A plain `fn` pointer so each refresh can be
/// tested against fixture bodies without a network.
pub type Fetch = fn(&str) -> Result<String>;

/// A source could not be read. Distinguished from every other failure because
/// it maps to exit 2 and says nothing about the table.
#[derive(Debug)]
pub struct NetworkError(pub String);

impl fmt::Display for NetworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "network: {}", self.0)
    }
}

impl std::error::Error for NetworkError {}

/// Whether an error from a refresh was the network rather than the data.
pub fn is_network_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NetworkError>().is_some()
}

/// The real fetch: a GET with a bounded wait, any failure a [`NetworkError`].
///
/// `user_agent` names the command, so a source's logs say which one asked.
pub fn http(url: &str, user_agent: &str) -> Result<String> {
    let body = reqwest::blocking::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent(user_agent)
        .build()
        .and_then(|client| client.get(url).send())
        .and_then(reqwest::blocking::Response::error_for_status)
        .and_then(reqwest::blocking::Response::text)
        .map_err(|e| NetworkError(format!("{url}: {e}")))?;
    Ok(body)
}

/// What a run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing moved; the file is untouched.
    Unchanged,
    /// This many rows were written (or, under `--check`, would be).
    Changed(usize),
}

/// The workspace root, from this crate's manifest directory.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Today as `YYYY-MM-DD`, UTC, so two machines on one day agree.
pub fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreachable_host_or_a_bad_url_is_the_network() {
        let err = http("http://127.0.0.1:1/models", "leviath-xtask-test").unwrap_err();
        assert!(is_network_error(&err), "{err}");
        assert!(
            err.to_string()
                .starts_with("network: http://127.0.0.1:1/models: ")
        );
        let err = http("not a url", "leviath-xtask-test").unwrap_err();
        assert!(is_network_error(&err), "{err}");
    }

    #[test]
    fn the_network_error_reads_as_one_and_nothing_else_is_one() {
        let err = NetworkError("x".to_owned());
        assert_eq!(err.to_string(), "network: x");
        assert!(std::error::Error::source(&err).is_none());
        assert!(!is_network_error(&anyhow::anyhow!("network: x")));
    }

    #[test]
    fn today_is_a_civil_date_and_the_root_is_the_workspace() {
        let today = today();
        assert_eq!(today.len(), "YYYY-MM-DD".len());
        assert_eq!(today.matches('-').count(), 2);
        assert!(workspace_root().join("xtask/Cargo.toml").is_file());
    }
}

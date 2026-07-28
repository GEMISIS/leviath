//! Debug HTTP logging helpers, gated behind the `debug-http` feature.
//!
//! When enabled, logs full request/response details for every provider HTTP call.

/// Redact credential-bearing headers in a header map for safe logging.
///
/// Both halves come from `leviath_core::secrets` now. The local copies named
/// exactly `authorization`, `x-api-key` and `api-key`, which meant Gemini's
/// `x-goog-api-key` was logged **in full** whenever this feature was on — the
/// one provider header that did not happen to be on the list.
fn redact_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let raw = value.to_str().unwrap_or("<non-utf8>");
            let val = match leviath_core::is_secret_header(name.as_str()) {
                true => leviath_core::redact(raw),
                false => raw.to_string(),
            };
            (name.to_string(), val)
        })
        .collect()
}

/// Log details about an HTTP request before it is sent.
pub fn log_request(
    provider: &str,
    method: &str,
    url: &str,
    headers: &reqwest::header::HeaderMap,
    body_size: usize,
) {
    let redacted = redact_headers(headers);
    tracing::debug!(
        provider = provider,
        method = method,
        url = url,
        body_bytes = body_size,
        headers = ?redacted,
        "debug-http: sending request"
    );
}

/// Log details about an HTTP response after it is received.
pub fn log_response(
    provider: &str,
    url: &str,
    status: u16,
    headers: &reqwest::header::HeaderMap,
    body_size: Option<u64>,
    elapsed: std::time::Duration,
) {
    let redacted = redact_headers(headers);
    tracing::debug!(
        provider = provider,
        url = url,
        status = status,
        body_bytes = ?body_size,
        elapsed_ms = elapsed.as_millis() as u64,
        headers = ?redacted,
        "debug-http: received response"
    );
}

/// Log an HTTP error.
pub fn log_error(provider: &str, url: &str, error: &str) {
    tracing::debug!(
        provider = provider,
        url = url,
        error = error,
        "debug-http: request error"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // The `redact` / `is_secret_header` unit tests live with their definitions
    // in `leviath_core::secrets`. What matters here is that this module's
    // header pass actually uses them.

    #[test]
    fn redact_headers_hides_auth() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "authorization",
            "Bearer sk-secret-key-1234".parse().unwrap(),
        );
        headers.insert("x-api-key", "sk-ant-api03-realkey".parse().unwrap());
        // Gemini's header. The old exact-name list did not include it, so this
        // key was logged in full whenever `debug-http` was enabled.
        headers.insert("x-goog-api-key", "AIzaSyRealGoogleKey".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());

        let redacted = redact_headers(&headers);
        for (name, value) in &redacted {
            if name == "content-type" {
                assert_eq!(value, "application/json");
                continue;
            }
            assert!(value.starts_with("****"), "{name} not redacted: {value}");
        }
        let joined = redacted
            .iter()
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        for leak in ["sk-secret", "realkey", "AIzaSyRealGoogleKey"] {
            assert!(!joined.contains(leak), "{leak} leaked: {joined}");
        }
    }
}

//! Debug HTTP logging helpers, gated behind the `debug-http` feature.
//!
//! When enabled, logs full request/response details for every provider HTTP call.

/// Redact an API key for logging, showing only the last 4 characters.
///
/// Counts *characters*, not bytes, for the same two reasons as `lev setup`'s
/// redactor: `key.len() - 4` can land inside a multi-byte character and panic
/// (the shape of issue #115), and a 5-byte 2-character key is longer than 4
/// *bytes*, so the byte branch would print the whole thing behind four stars.
fn redact_api_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 4 {
        "****".to_string()
    } else {
        format!(
            "****{}",
            chars[chars.len() - 4..].iter().collect::<String>()
        )
    }
}

/// Redact authorization headers in a header map for safe logging.
fn redact_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let name_lower = name.as_str().to_lowercase();
            let val = if name_lower == "authorization"
                || name_lower == "x-api-key"
                || name_lower == "api-key"
            {
                redact_api_key(value.to_str().unwrap_or("<non-utf8>"))
            } else {
                value.to_str().unwrap_or("<non-utf8>").to_string()
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

    #[test]
    fn redact_api_key_short() {
        assert_eq!(redact_api_key("abc"), "****");
        assert_eq!(redact_api_key("abcd"), "****");
    }

    #[test]
    fn redact_api_key_long() {
        assert_eq!(redact_api_key("sk-ant-api03-abc123xyz"), "****3xyz");
    }

    #[test]
    fn redact_api_key_multibyte() {
        // Issue #115: `key.len() - 4` used to land inside the last '日' and panic.
        assert_eq!(redact_api_key("sk-日本語日本語"), "****語日本語");
        // 2 characters but 6 bytes — the byte-length guard called this "long" and
        // printed the whole key. Character counting redacts it.
        assert_eq!(redact_api_key("日本"), "****");
    }

    #[test]
    fn redact_headers_hides_auth() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "authorization",
            "Bearer sk-secret-key-1234".parse().unwrap(),
        );
        headers.insert("x-api-key", "sk-ant-api03-realkey".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());

        let redacted = redact_headers(&headers);
        for (name, value) in &redacted {
            if name == "authorization" || name == "x-api-key" {
                assert!(value.starts_with("****"), "expected redacted: {value}");
                assert!(!value.contains("sk-secret"), "key leaked: {value}");
                assert!(!value.contains("realkey"), "key leaked: {value}");
            }
            if name == "content-type" {
                assert_eq!(value, "application/json");
            }
        }
    }
}

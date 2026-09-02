//! JSON-RPC 2.0 envelope types and framing shared by every MCP transport.
//!
//! Both the stdio and HTTP transports speak the same JSON-RPC dialect; only the
//! bytes underneath differ. Keeping the envelope here means the "did the server
//! answer with an error / with nothing at all" rules have exactly one
//! implementation, and so does the handling of frames the server sends *to* us.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC error code for an unimplemented method. `i32`, the width every
/// JSON-RPC envelope in the workspace gives an error code, so the server side
/// (`crate::server`) can hand it straight to one.
pub const METHOD_NOT_FOUND: i32 = -32601;

/// A JSON-RPC request or notification.
///
/// A notification is simply a request with no `id`; the absent field is skipped
/// during serialization rather than sent as `null`, which some servers reject.
#[derive(Serialize)]
pub(crate) struct JsonRpcRequest {
    pub(crate) jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<u64>,
    pub(crate) method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) params: Option<Value>,
}

impl JsonRpcRequest {
    /// A request that expects a response.
    pub(crate) fn request(id: u64, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id: Some(id),
            method: method.to_string(),
            params: Some(params),
        }
    }

    /// A fire-and-forget notification.
    pub(crate) fn notification(method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id: None,
            method: method.to_string(),
            params: Some(params),
        }
    }

    /// Serialize to a single line, newline included.
    ///
    /// Infallible: every field is a plain serializable type.
    pub(crate) fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("JsonRpcRequest is always serializable");
        s.push('\n');
        s
    }
}

/// A JSON-RPC response frame.
#[derive(Deserialize)]
pub(crate) struct JsonRpcResponse {
    pub(crate) id: Option<Value>,
    pub(crate) result: Option<Value>,
    pub(crate) error: Option<JsonRpcError>,
}

/// The `error` member of a JSON-RPC response.
#[derive(Deserialize)]
pub(crate) struct JsonRpcError {
    /// The JSON-RPC error code, surfaced in the message because it is the half
    /// of the error a reader can look up: `-32601` says "method not found"
    /// whatever prose the server chose to pair with it.
    pub(crate) code: i64,
    pub(crate) message: String,
}

impl JsonRpcResponse {
    /// Reduce a response to its `result`, turning both failure shapes into
    /// errors: an explicit `error` member, and a frame carrying neither
    /// `result` nor `error` (which is malformed, but servers do emit it).
    pub(crate) fn into_result(self) -> anyhow::Result<Value> {
        if let Some(error) = self.error {
            return Err(anyhow::anyhow!(
                "MCP server error {}: {}",
                error.code,
                error.message
            ));
        }
        self.result
            .ok_or_else(|| anyhow::anyhow!("MCP server returned no result"))
    }
}

/// What an inbound frame is, from the client's point of view.
///
/// The transports read a single stream that carries three different things.
/// Treating every frame as "the response to my last request" means a server
/// that pings us, or asks us for its roots, corrupts the request/response
/// pairing or blocks waiting for a reply that never comes.
pub(crate) enum Inbound {
    /// A response to a request we sent.
    Response(Box<JsonRpcResponse>),
    /// A request *from* the server, which expects a reply.
    ServerRequest { id: Value, method: String },
    /// A one-way notification from the server.
    Notification { method: String },
}

/// Classify one parsed inbound frame.
///
/// The discriminator is JSON-RPC's own: a frame with a `method` is something
/// the server is asking of us (a request when it carries an `id`, a
/// notification when it doesn't); anything else is a response to us.
pub(crate) fn classify(frame: Value) -> anyhow::Result<Inbound> {
    let method = frame
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string);

    match method {
        Some(method) => match frame.get("id") {
            Some(id) if !id.is_null() => Ok(Inbound::ServerRequest {
                id: id.clone(),
                method,
            }),
            _ => Ok(Inbound::Notification { method }),
        },
        None => {
            let response: JsonRpcResponse = serde_json::from_value(frame)
                .map_err(|e| anyhow::anyhow!("Failed to parse JSON-RPC response: {}", e))?;
            Ok(Inbound::Response(Box::new(response)))
        }
    }
}

/// Build the reply to a server-initiated request.
///
/// `ping` is answered with an empty result (it exists purely as a liveness
/// check). Everything else - `roots/list`, `sampling/createMessage`,
/// `elicitation/create` - gets a well-formed "method not found" rather than
/// silence, so a server that waits on the reply makes progress instead of
/// stalling the connection.
pub(crate) fn reply_to_server_request(id: &Value, method: &str) -> Value {
    if method == "ping" {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {} })
    } else {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": METHOD_NOT_FOUND,
                "message": format!("Leviath does not implement '{method}'"),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serializes_with_id() {
        let line = JsonRpcRequest::request(42, "tools/list", serde_json::json!({})).to_line();
        assert!(line.ends_with('\n'));
        assert!(line.contains("\"jsonrpc\":\"2.0\""));
        assert!(line.contains("\"id\":42"));
        assert!(line.contains("\"method\":\"tools/list\""));
    }

    #[test]
    fn notification_omits_id_entirely() {
        // Sending `"id": null` marks the frame as a *request* with a null id in
        // some server implementations, which then try to respond to it.
        let line = JsonRpcRequest::notification("notifications/initialized", serde_json::json!({}))
            .to_line();
        assert!(!line.contains("\"id\""), "got: {line}");
    }

    #[test]
    fn request_omits_absent_params() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id: Some(1),
            method: "x".to_string(),
            params: None,
        };
        assert!(!req.to_line().contains("params"));
    }

    fn response(json: &str) -> JsonRpcResponse {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn into_result_returns_the_result_member() {
        let value = response(r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#)
            .into_result()
            .unwrap();
        assert_eq!(value, serde_json::json!({"tools": []}));
    }

    #[test]
    fn into_result_surfaces_the_error_member() {
        let err = response(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"nope"}}"#)
            .into_result()
            .expect_err("error member must fail");
        assert!(err.to_string().contains("nope"), "got: {err}");
    }

    #[test]
    fn into_result_rejects_a_frame_with_neither_member() {
        let err = response(r#"{"jsonrpc":"2.0","id":1}"#)
            .into_result()
            .expect_err("empty frame must fail");
        assert!(err.to_string().contains("no result"), "got: {err}");
    }

    /// Classify `frame` and render the outcome, payload included, as a string.
    ///
    /// One exhaustive `match` covering every arm, rather than a `matches!` per
    /// case: this way the assertion also pins the extracted `id`/`method`, and
    /// no arm goes unexercised.
    fn classified(frame: Value) -> String {
        match classify(frame).unwrap() {
            Inbound::Response(_) => "response".to_string(),
            Inbound::ServerRequest { id, method } => format!("server_request:{id}:{method}"),
            Inbound::Notification { method } => format!("notification:{method}"),
        }
    }

    #[test]
    fn classify_distinguishes_the_three_frame_kinds() {
        assert_eq!(
            classified(serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {}})),
            "response"
        );
        assert_eq!(
            classified(serde_json::json!({"jsonrpc": "2.0", "id": 7, "method": "ping"})),
            "server_request:7:ping"
        );
        assert_eq!(
            classified(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/progress"})),
            "notification:notifications/progress"
        );
    }

    #[test]
    fn classify_treats_a_null_id_method_frame_as_a_notification() {
        // A notification with an explicit `"id": null` must not be answered:
        // replying to a null id is meaningless and some servers error on it.
        assert_eq!(
            classified(
                serde_json::json!({"jsonrpc": "2.0", "id": null, "method": "notifications/x"})
            ),
            "notification:notifications/x"
        );
    }

    #[test]
    fn classify_rejects_a_malformed_response() {
        // No `method`, so it must be a response - but it isn't shaped like one.
        let err = classify(serde_json::json!([1, 2, 3]))
            .err()
            .expect("array is not a response");
        assert!(err.to_string().contains("parse"), "got: {err}");
    }

    #[test]
    fn ping_is_answered_with_an_empty_result() {
        let reply = reply_to_server_request(&serde_json::json!(3), "ping");
        assert_eq!(reply["id"], serde_json::json!(3));
        assert_eq!(reply["result"], serde_json::json!({}));
    }

    #[test]
    fn unsupported_server_request_gets_method_not_found() {
        // Silence would leave a server blocked waiting for a reply.
        let reply = reply_to_server_request(&serde_json::json!("abc"), "sampling/createMessage");
        assert_eq!(reply["id"], serde_json::json!("abc"));
        assert_eq!(reply["error"]["code"], serde_json::json!(METHOD_NOT_FOUND));
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap()
                .contains("sampling/createMessage")
        );
    }
}

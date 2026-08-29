//! One parameterised Python stdio MCP server, in place of the near-identical
//! JSON-RPC stub scripts that were pasted into each test module that needed a
//! server to talk to.
//!
//! The server reads one JSON-RPC request per line from stdin and answers
//! `initialize`, `tools/list` and `tools/call`; notifications (requests with
//! no `id`) are ignored and any other method gets a `-32601` error. Every
//! knob a copy differed on is a builder method here, so a test that needs a
//! server which fails `tools/list`, or one whose tool echoes the name it was
//! called with, says so in Rust rather than in a second Python template.

/// What the stub answers to `tools/call`.
#[derive(Clone, Debug)]
enum CallReply {
    /// A single text block, flagged with `isError` as given.
    Text { text: String, is_error: bool },
    /// A single text block reading `called <tool name>`, so a test can see
    /// which name reached the server.
    EchoName,
    /// A JSON-RPC error (`-32603`) with this message.
    Error(String),
}

/// A builder for the stub's Python source. `McpStub::new()` is a server with
/// no tools that answers every call with `hello from tool`; chain the
/// setters below and finish with [`McpStub::source`].
#[derive(Clone, Debug)]
pub struct McpStub {
    tools: Vec<(String, Option<String>)>,
    input_schema: String,
    capabilities: String,
    call: CallReply,
    list_error: Option<String>,
    init_error: Option<String>,
}

impl Default for McpStub {
    fn default() -> Self {
        Self::new()
    }
}

impl McpStub {
    /// A server with no tools and empty capabilities whose `tools/call`
    /// replies `hello from tool`.
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            input_schema: "{}".to_string(),
            capabilities: "{}".to_string(),
            call: CallReply::Text {
                text: "hello from tool".to_string(),
                is_error: false,
            },
            list_error: None,
            init_error: None,
        }
    }

    /// Advertise a tool; `description` is omitted from the wire when `None`.
    pub fn tool(mut self, name: &str, description: Option<&str>) -> Self {
        self.tools
            .push((name.to_string(), description.map(str::to_string)));
        self
    }

    /// The `inputSchema` every advertised tool carries, as JSON text
    /// (default `{}`).
    pub fn input_schema(mut self, json: &str) -> Self {
        self.input_schema = json.to_string();
        self
    }

    /// Declare a `tools` capability with the given `listChanged` flag.
    pub fn list_changed(self, list_changed: bool) -> Self {
        self.capabilities_json(&format!(
            r#"{{"tools": {{"listChanged": {list_changed}}}}}"#
        ))
    }

    /// The whole `capabilities` object of the `initialize` result, as JSON
    /// text (default `{}`).
    pub fn capabilities_json(mut self, json: &str) -> Self {
        self.capabilities = json.to_string();
        self
    }

    /// `tools/call` succeeds with this text.
    pub fn replying(mut self, text: &str) -> Self {
        self.call = CallReply::Text {
            text: text.to_string(),
            is_error: false,
        };
        self
    }

    /// `tools/call` returns this text flagged `isError: true` (a tool
    /// execution error, as distinct from a JSON-RPC error).
    pub fn replying_error(mut self, text: &str) -> Self {
        self.call = CallReply::Text {
            text: text.to_string(),
            is_error: true,
        };
        self
    }

    /// `tools/call` replies `called <name>` for whatever tool name it was
    /// sent.
    pub fn echoing_tool_name(mut self) -> Self {
        self.call = CallReply::EchoName;
        self
    }

    /// `tools/call` answers with a JSON-RPC error carrying this message.
    pub fn call_fails(mut self, message: &str) -> Self {
        self.call = CallReply::Error(message.to_string());
        self
    }

    /// `tools/list` answers with a JSON-RPC error carrying this message.
    pub fn list_fails(mut self, message: &str) -> Self {
        self.list_error = Some(message.to_string());
        self
    }

    /// `initialize` answers with a JSON-RPC error carrying this message, so
    /// the handshake fails.
    pub fn init_fails(mut self, message: &str) -> Self {
        self.init_error = Some(message.to_string());
        self
    }

    /// The Python source, ready for `python3 -c`.
    pub fn source(&self) -> String {
        let tools = self
            .tools
            .iter()
            .map(|(name, description)| {
                let description = description
                    .as_deref()
                    .map(|d| format!(", \"description\": {}", py_str(d)))
                    .unwrap_or_default();
                format!(
                    "{{\"name\": {}{description}, \"inputSchema\": json.loads({})}}",
                    py_str(name),
                    py_str(&self.input_schema)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let (call_text, call_echo, call_is_error, call_error) = match &self.call {
            CallReply::Text { text, is_error } => (
                py_str(text),
                "False",
                py_bool(*is_error),
                "None".to_string(),
            ),
            CallReply::EchoName => ("None".to_string(), "True", "False", "None".to_string()),
            CallReply::Error(message) => ("None".to_string(), "False", "False", py_str(message)),
        };
        let list_error = py_opt(self.list_error.as_deref());
        let init_error = py_opt(self.init_error.as_deref());
        let capabilities = py_str(&self.capabilities);
        format!(
            r#"
import sys, json

TOOLS = [{tools}]
CAPABILITIES = json.loads({capabilities})
CALL_TEXT = {call_text}
CALL_ECHO = {call_echo}
CALL_IS_ERROR = {call_is_error}
CALL_ERROR = {call_error}
LIST_ERROR = {list_error}
INIT_ERROR = {init_error}

def respond(id_, result):
    sys.stdout.write(json.dumps({{"jsonrpc": "2.0", "id": id_, "result": result}}) + "\n")
    sys.stdout.flush()

def error(id_, code, message):
    sys.stdout.write(json.dumps({{"jsonrpc": "2.0", "id": id_, "error": {{"code": code, "message": message}}}}) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method", "")
    id_ = req.get("id")
    if id_ is None:
        continue  # a notification: nothing to answer
    if method == "initialize":
        if INIT_ERROR is not None:
            error(id_, -32000, INIT_ERROR)
        else:
            respond(id_, {{"capabilities": CAPABILITIES, "protocolVersion": "2024-11-05"}})
    elif method == "tools/list":
        if LIST_ERROR is not None:
            error(id_, -32000, LIST_ERROR)
        else:
            respond(id_, {{"tools": TOOLS}})
    elif method == "tools/call":
        if CALL_ERROR is not None:
            error(id_, -32603, CALL_ERROR)
        else:
            text = "called " + req["params"]["name"] if CALL_ECHO else CALL_TEXT
            respond(id_, {{"content": [{{"type": "text", "text": text}}], "isError": CALL_IS_ERROR}})
    else:
        error(id_, -32601, "method not found")
"#
        )
    }
}

/// `s` as a double-quoted Python string literal.
fn py_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn py_bool(b: bool) -> &'static str {
    if b { "True" } else { "False" }
}

fn py_opt(s: Option<&str>) -> String {
    s.map(py_str).unwrap_or_else(|| "None".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_knob_reaches_the_source() {
        let src = McpStub::new()
            .tool("echo", Some("say \"hi\""))
            .tool("bare", None)
            .input_schema(r#"{"type": "object"}"#)
            .list_changed(true)
            .replying_error("boom")
            .list_fails("no list")
            .init_fails("no init")
            .source();
        assert!(
            src.contains(r#""name": "echo", "description": "say \"hi\""#),
            "{src}"
        );
        assert!(src.contains(r#"{"name": "bare", "inputSchema""#), "{src}");
        assert!(
            src.contains(r#"CAPABILITIES = json.loads("{\"tools\": {\"listChanged\": true}}")"#)
        );
        assert!(src.contains("CALL_IS_ERROR = True"));
        assert!(src.contains(r#"LIST_ERROR = "no list""#));
        assert!(src.contains(r#"INIT_ERROR = "no init""#));

        let echo = McpStub::default().echoing_tool_name().source();
        assert!(echo.contains("CALL_ECHO = True"));
        let failing = McpStub::new()
            .capabilities_json(r#"{"tools": {}}"#)
            .call_fails("nope")
            .source();
        assert!(failing.contains(r#"CALL_ERROR = "nope""#));
        assert!(failing.contains(r#"json.loads("{\"tools\": {}}")"#));
        let plain = McpStub::new().replying("ok\nthen").source();
        assert!(plain.contains(r#"CALL_TEXT = "ok\nthen""#));
    }
}

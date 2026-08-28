#!/usr/bin/env python3
"""A stateless OpenAI-compatible mock provider for driving a real daemon.

Usage:
    mock.py PORT                         # every turn answers "done"
    mock.py PORT TOOL '{"json":"args"}'  # first turn asks for TOOL, then "done"

The decision is made from the request body, never from a turn counter: the
daemon spends a turn during startup, so a counter-based script never reaches
the agent. The tool call is returned until `messages` carries a `role: "tool"`
entry, then the reply is plain text and the run completes.

Routes:
    GET  /v1/models          one model, "mock-1"
    POST /v1/chat/completions  JSON or SSE, depending on `stream`
    GET  /count              how many completions were served (probe counter)
    POST /reset              zero the counter
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1])
TOOL = sys.argv[2] if len(sys.argv) > 2 else None
ARGS = sys.argv[3] if len(sys.argv) > 3 else "{}"
CALLS = [0]
USAGE = {"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def _json(self, obj, status=200):
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.startswith("/count"):
            return self._json({"count": CALLS[0]})
        return self._json({"object": "list", "data": [{"id": "mock-1", "object": "model"}]})

    def do_POST(self):
        if self.path.startswith("/reset"):
            CALLS[0] = 0
            return self._json({"ok": True})
        n = int(self.headers.get("content-length", "0"))
        req = json.loads(self.rfile.read(n) or b"{}")
        CALLS[0] += 1
        seen_tool = any(m.get("role") == "tool" for m in req.get("messages", []))
        want_tool = TOOL is not None and not seen_tool
        if req.get("stream"):
            return self._sse(want_tool)
        if want_tool:
            message = {
                "role": "assistant",
                "content": None,
                "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": TOOL, "arguments": ARGS}}],
            }
            finish = "tool_calls"
        else:
            message = {"role": "assistant", "content": "done"}
            finish = "stop"
        self._json({"id": "c1", "object": "chat.completion", "model": "mock-1", "usage": USAGE,
                    "choices": [{"index": 0, "finish_reason": finish, "message": message}]})

    def _sse(self, want_tool):
        if want_tool:
            delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                                                          "function": {"name": TOOL, "arguments": ARGS}}]}
            finish = "tool_calls"
        else:
            delta = {"role": "assistant", "content": "done"}
            finish = "stop"
        chunks = [
            {"id": "c1", "object": "chat.completion.chunk", "model": "mock-1",
             "choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
            {"id": "c1", "object": "chat.completion.chunk", "model": "mock-1",
             "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]},
            {"id": "c1", "object": "chat.completion.chunk", "model": "mock-1", "choices": [], "usage": USAGE},
        ]
        body = "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"
        body = body.encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()

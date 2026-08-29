#!/usr/bin/env python3
"""A stateless OpenAI- and Anthropic-compatible mock provider for driving a real daemon.

Usage:
    mock.py PORT                         # every turn answers "done"
    mock.py PORT TOOL '{"json":"args"}'  # first turn asks for TOOL, then "done"
    mock.py PORT TOOL '[{"a":1},{"a":2}]'  # a JSON list asks for TOOL once per element, in one batch

The decision is made from the request body, never from a turn counter: the
daemon spends a turn during startup, so a counter-based script never reaches
the agent. The tool call is returned until `messages` carries a `role: "tool"`
entry, then the reply is plain text and the run completes.

Routes:
    GET  /v1/models          two models, "gpt-mock" and "claude-mock"
    POST /v1/chat/completions  JSON or SSE, depending on `stream`
    POST /v1/messages        the Anthropic shape of the same answer (JSON only;
                             point the harness at it with `stream_inference = false`)
    POST /v1/messages/count_tokens  Anthropic's count endpoint: `input_tokens`
                             is the request text's bytes over four
    GET  /count              how many completions and how many token counts
                             were served (probe counters)
    POST /reset              zero the counters

Set `LV_MOCK_OVERSIZE_MIB=N` to answer every completion with N MiB of one
frame that never closes, for probing the daemon's read caps.

The count tally is what makes the context-window guard measurable from the
outside: a run whose request is under half the model's window must show zero
count calls, and one above it exactly one per inference.
"""
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1])
TOOL = sys.argv[2] if len(sys.argv) > 2 else None
ARGS = sys.argv[3] if len(sys.argv) > 3 else "{}"
# `LV_MOCK_OVERSIZE_MIB=N` makes every completion answer with N MiB of a single
# never-ending frame (one `data:` line with no terminator when streaming, one
# JSON string otherwise), which is what a peer that never stops looks like to
# the daemon's read caps.
OVERSIZE_MIB = int(os.environ.get("LV_MOCK_OVERSIZE_MIB", "0"))


def tool_calls(streaming):
    """The tool calls the first turn asks for: one per element when ARGS is a
    JSON list, so a probe can put several calls in one batch."""
    try:
        parsed = json.loads(ARGS)
    except ValueError:
        parsed = None
    args_list = parsed if isinstance(parsed, list) else [ARGS]
    calls = []
    for i, args in enumerate(args_list):
        arguments = args if isinstance(args, str) else json.dumps(args)
        call = {"id": f"call_{i + 1}", "type": "function", "function": {"name": TOOL, "arguments": arguments}}
        if streaming:
            call = {"index": i, **call}
        calls.append(call)
    return calls
CALLS = [0]
COUNTS = [0]
USAGE = {"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15}


def anthropic_text(req):
    """Every piece of text an Anthropic-shaped request carries, for the count."""
    parts = []
    system = req.get("system", "")
    if isinstance(system, list):
        parts.extend(b.get("text", "") for b in system)
    else:
        parts.append(system or "")
    for m in req.get("messages", []):
        content = m.get("content", "")
        if isinstance(content, list):
            parts.extend(b.get("text", "") or b.get("content", "") or "" for b in content if isinstance(b, dict))
        else:
            parts.append(content or "")
    return "\n".join(str(p) for p in parts)


def anthropic_seen_tool(req):
    """Whether any message carries a `tool_result` block, the Anthropic way of
    saying a tool has already answered."""
    for m in req.get("messages", []):
        content = m.get("content", "")
        if isinstance(content, list) and any(
            isinstance(b, dict) and b.get("type") == "tool_result" for b in content
        ):
            return True
    return False


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
            return self._json({"count": CALLS[0], "token_counts": COUNTS[0]})
        # Both providers read `data[].id`; the Anthropic one only routes to a
        # model its listing carried, so the Claude-shaped id has to be here.
        return self._json({"object": "list", "data": [
            {"id": "gpt-mock", "object": "model"},
            {"id": "claude-mock", "object": "model", "display_name": "Claude Mock"},
        ], "has_more": False})

    def do_POST(self):
        if self.path.startswith("/reset"):
            CALLS[0] = 0
            COUNTS[0] = 0
            return self._json({"ok": True})
        n = int(self.headers.get("content-length", "0"))
        req = json.loads(self.rfile.read(n) or b"{}")
        if self.path.startswith("/v1/messages/count_tokens"):
            COUNTS[0] += 1
            return self._json({"input_tokens": len(anthropic_text(req)) // 4})
        if self.path.startswith("/v1/messages"):
            return self._anthropic(req)
        CALLS[0] += 1
        if OVERSIZE_MIB:
            return self._oversize(bool(req.get("stream")))
        seen_tool = any(m.get("role") == "tool" for m in req.get("messages", []))
        want_tool = TOOL is not None and not seen_tool
        if req.get("stream"):
            return self._sse(want_tool)
        if want_tool:
            message = {
                "role": "assistant",
                "content": None,
                "tool_calls": tool_calls(streaming=False),
            }
            finish = "tool_calls"
        else:
            message = {"role": "assistant", "content": "done"}
            finish = "stop"
        self._json({"id": "c1", "object": "chat.completion", "model": "gpt-mock", "usage": USAGE,
                    "choices": [{"index": 0, "finish_reason": finish, "message": message}]})

    def _anthropic(self, req):
        """The Anthropic Messages shape of the same decision, always buffered."""
        CALLS[0] += 1
        want_tool = TOOL is not None and not anthropic_seen_tool(req)
        if want_tool:
            content = [
                {"type": "tool_use", "id": f"toolu_{i + 1}", "name": TOOL,
                 "input": json.loads(c["function"]["arguments"])}
                for i, c in enumerate(tool_calls(streaming=False))
            ]
            stop = "tool_use"
        else:
            content = [{"type": "text", "text": "done"}]
            stop = "end_turn"
        self._json({"id": "msg_1", "type": "message", "role": "assistant",
                    "model": req.get("model", "gpt-mock"), "content": content,
                    "stop_reason": stop,
                    "usage": {"input_tokens": 12, "output_tokens": 3}})

    def _oversize(self, streaming):
        """One frame of OVERSIZE_MIB MiB that never closes."""
        pad = b"x" * (OVERSIZE_MIB * 1024 * 1024)
        if streaming:
            body, content_type = b"data: " + pad, "text/event-stream"
        else:
            body, content_type = b'{"pad":"' + pad + b'"}', "application/json"
        self.send_response(200)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _sse(self, want_tool):
        if want_tool:
            delta = {"role": "assistant", "tool_calls": tool_calls(streaming=True)}
            finish = "tool_calls"
        else:
            delta = {"role": "assistant", "content": "done"}
            finish = "stop"
        chunks = [
            {"id": "c1", "object": "chat.completion.chunk", "model": "gpt-mock",
             "choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
            {"id": "c1", "object": "chat.completion.chunk", "model": "gpt-mock",
             "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]},
            {"id": "c1", "object": "chat.completion.chunk", "model": "gpt-mock", "choices": [], "usage": USAGE},
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

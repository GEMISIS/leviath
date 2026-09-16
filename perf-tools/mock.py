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

Set `LV_MOCK_CUT_OFF=1` to answer the Anthropic route's first turn with a
`tool_use` whose input stopped mid-argument (`stop_reason: "max_tokens"`), the
shape a reply cut off by the output cap takes. `LV_MOCK_CUT_OFF=always` answers
every turn that way. Only a request that offers the tool is cut off, so a stage
without it (an error edge's recovery stage) gets "done". Like the real API, the
route refuses any request whose
history carries a `tool_use` input that is not an object.

Set `LV_MOCK_SPLIT_UTF8=1` to answer a streamed completion with CJK and an
emoji in the text, written to the socket in two flushes that cut the emoji
in half, the way a transport boundary lands inside a character.

The count tally is what makes the context-window guard measurable from the
outside: a run whose request is under half the model's window must show zero
count calls, and one above it exactly one per inference.
"""
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1])
TOOL = sys.argv[2] if len(sys.argv) > 2 else None
ARGS = sys.argv[3] if len(sys.argv) > 3 else "{}"
# `LV_MOCK_OVERSIZE_MIB=N` makes every completion answer with N MiB of a single
# never-ending frame (one `data:` line with no terminator when streaming, one
# JSON string otherwise), which is what a peer that never stops looks like to
# the daemon's read caps.
OVERSIZE_MIB = int(os.environ.get("LV_MOCK_OVERSIZE_MIB", "0"))
# `LV_MOCK_SPLIT_UTF8=1` makes the streamed answer "done 完成 🎉" and flushes
# the body in two pieces, the cut two bytes into the four-byte emoji, so the
# daemon's stream reader sees a chunk that is not UTF-8 on its own.
SPLIT_UTF8 = os.environ.get("LV_MOCK_SPLIT_UTF8") == "1"
# `LV_MOCK_IMAGE=1` makes every text reply carry one PNG the way OpenRouter
# returns a drawn image: an `images` list of data URIs on the message.
IMAGE = os.environ.get("LV_MOCK_IMAGE") == "1"
# `LV_MOCK_REQUIRE_HEADER=Name=value` makes the mock a gateway that refuses
# (401) any request without that header, the way a corporate proxy does, so a
# probe can prove `[providers] <provider>_headers` reached the wire.
REQUIRE_HEADER = os.environ.get("LV_MOCK_REQUIRE_HEADER", "").partition("=")
# `LV_MOCK_DUMP=path` appends one JSON line per completion request (path,
# headers, body), so a probe can assert what was sent.
DUMP = os.environ.get("LV_MOCK_DUMP")
# `LV_MOCK_CUT_OFF=1` cuts the first Anthropic tool call off mid-argument;
# `always` cuts every turn off.
CUT_OFF = os.environ.get("LV_MOCK_CUT_OFF", "")
IMAGE_URI = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=="


def with_image(message):
    """The reply's message with the mock's picture on it, when asked for."""
    if IMAGE:
        message["images"] = [{"type": "image_url", "image_url": {"url": IMAGE_URI}}]
    return message
SPLIT_TEXT = "done 完成 🎉"


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


def anthropic_bad_tool_input(req):
    """The first history `tool_use` whose input is not an object, named by the
    path the real API puts in its refusal, or None."""
    for i, m in enumerate(req.get("messages", [])):
        content = m.get("content", "")
        if not isinstance(content, list):
            continue
        for j, b in enumerate(content):
            if isinstance(b, dict) and b.get("type") == "tool_use" and not isinstance(b.get("input"), dict):
                return f"messages.{i}.content.{j}.tool_use.input"
    return None


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

    def _gateway_refuses(self):
        """Whether the required header is missing; answers the 401 itself."""
        name, _, value = REQUIRE_HEADER
        if not name or self.headers.get(name) == value:
            return False
        self._json({"error": {"message": f"missing gateway header {name}"}}, 401)
        return True

    def _dump(self, req):
        if DUMP:
            with open(DUMP, "a") as out:
                out.write(json.dumps({
                    "path": self.path,
                    "headers": {k.lower(): v for k, v in self.headers.items()},
                    "body": req,
                }) + "\n")

    def do_GET(self):
        if self._gateway_refuses():
            return
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
        self._dump(req)
        if self._gateway_refuses():
            return
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
            message = with_image({"role": "assistant", "content": "done"})
            finish = "stop"
        return self._json({"id": "c1", "object": "chat.completion", "model": "gpt-mock", "usage": USAGE,
                           "choices": [{"index": 0, "finish_reason": finish, "message": message}]})

    def _anthropic(self, req):
        """The Anthropic Messages shape of the same decision, always buffered."""
        CALLS[0] += 1
        bad = anthropic_bad_tool_input(req)
        if bad:
            return self._json({"type": "error", "error": {
                "type": "invalid_request_error",
                "message": f"{bad}: Input should be an object"}}, 400)
        seen_tool = anthropic_seen_tool(req)
        want_tool = TOOL is not None and not seen_tool
        # Only a request that offers the tool can be answered with a call to
        # it, so a recovery stage without it gets a plain answer.
        cut_tool = TOOL or "write_file"
        offered = any(t.get("name") == cut_tool for t in req.get("tools") or [])
        if offered and (CUT_OFF == "always" or (CUT_OFF and not seen_tool)):
            content = [{"type": "tool_use", "id": f"toolu_cut_{CALLS[0]}", "name": cut_tool,
                        "input": '{"path": "report.md", "content": "# A long rep'}]
            stop = "max_tokens"
        elif want_tool:
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
            delta = with_image({"role": "assistant", "content": SPLIT_TEXT if SPLIT_UTF8 else "done"})
            finish = "stop"
        chunks = [
            {"id": "c1", "object": "chat.completion.chunk", "model": "gpt-mock",
             "choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
            {"id": "c1", "object": "chat.completion.chunk", "model": "gpt-mock",
             "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]},
            {"id": "c1", "object": "chat.completion.chunk", "model": "gpt-mock", "choices": [], "usage": USAGE},
        ]
        body = "".join(f"data: {json.dumps(c, ensure_ascii=False)}\n\n" for c in chunks)
        body = (body + "data: [DONE]\n\n").encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        if SPLIT_UTF8 and not want_tool:
            # Two bytes into the emoji: the first flush ends mid-character.
            cut = body.index("🎉".encode()) + 2
            self.wfile.write(body[:cut])
            self.wfile.flush()
            time.sleep(0.2)
            self.wfile.write(body[cut:])
            return
        self.wfile.write(body)


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()

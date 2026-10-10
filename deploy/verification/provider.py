#!/usr/bin/env python3
"""Zero-cost OpenAI-style fixture, reachable only on the verification network."""

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOCK = threading.Lock()
STATS = {"calls": 0, "finished": 0}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def json_response(self, status, value):
        data = json.dumps(value, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/stats":
            with LOCK:
                value = dict(STATS)
            self.json_response(200, value)
        else:
            self.json_response(404, {})

    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self.json_response(404, {})
            return
        if self.headers.get("Authorization") != "Bearer local-zero-cost-fixture":
            self.json_response(401, {})
            return
        length = int(self.headers.get("Content-Length", "0"))
        if not 0 < length <= 65536:
            self.json_response(400, {})
            return
        body = json.loads(self.rfile.read(length))
        scenario = json.loads(body["messages"][-1]["content"])
        size = scenario.get("bytes", 64)
        delay = scenario.get("delay_seconds", 0)
        if not 0 <= size <= 3 * 1024 * 1024 or not 0 <= delay <= 5:
            self.json_response(400, {})
            return
        with LOCK:
            STATS["calls"] += 1
        usage = {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
        base = {"id": "local-fixture", "object": "chat.completion.chunk", "created": 1, "model": "local-upstream"}
        if not body.get("stream"):
            self.json_response(200, {
                **base, "object": "chat.completion", "usage": usage,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "x" * size}, "finish_reason": "stop"}],
            })
        else:
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            self.close_connection = True
            try:
                self.event({**base, "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": None}], "usage": None})
                time.sleep(delay)
                # One large SSE event tests parser, aggregation, frame splitting,
                # real TCP backpressure and downstream buffer release together.
                self.event({**base, "choices": [{"index": 0, "delta": {"content": "x" * size}, "finish_reason": None}], "usage": None})
                self.event({**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": None})
                if not scenario.get("incomplete"):
                    self.event({**base, "choices": [], "usage": usage})
                    self.wfile.write(b"data: [DONE]\n\n")
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass
        with LOCK:
            STATS["finished"] += 1

    def event(self, value):
        self.wfile.write(b"data: " + json.dumps(value, separators=(",", ":")).encode() + b"\n\n")
        self.wfile.flush()


if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", 8099), Handler).serve_forever()

"""Mock OpenAI-compatible endpoint, stand-in for a `hermes gateway`.

Asserts the exact request shape riddle sends, then replies with an SSE stream
in the same frame format Hermes/OpenAI use.
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

SEEN = {}


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        n = int(self.headers["Content-Length"])
        body = json.loads(self.rfile.read(n))
        SEEN["path"] = self.path
        SEEN["auth"] = self.headers.get("Authorization")
        SEEN["ctype"] = self.headers.get("Content-Type")
        SEEN["model"] = body.get("model")
        SEEN["stream"] = body.get("stream")
        SEEN["cap"] = "max_tokens" in body
        user = [m for m in body["messages"] if m["role"] == "user"][-1]
        parts = {p["type"]: p for p in user["content"]}
        SEEN["has_image"] = "image_url" in parts
        SEEN["image_prefix"] = parts["image_url"]["image_url"]["url"][:22]
        SEEN["system_len"] = len(body["messages"][0]["content"])
        print("REQUEST " + json.dumps(SEEN), file=sys.stderr, flush=True)

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        chunks = [
            "Who writes to me",
            " in the dark?",
            " Tell me your name.",
            "\n⁂ hello is anyone there",
        ]
        self.wfile.write(
            b'data: {"choices":[{"delta":{"role":"assistant","content":null}}]}\n\n'
        )
        for c in chunks:
            frame = {"choices": [{"delta": {"content": c}, "index": 0}]}
            # Alternate compact and pretty-printed frames: both are valid JSON
            # and riddle must cope with either.
            sep = (",", ":") if chunks.index(c) % 2 == 0 else (", ", ": ")
            self.wfile.write(
                f"data: {json.dumps(frame, separators=sep)}\n\n".encode()
            )
            self.wfile.flush()
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


HTTPServer(("127.0.0.1", 8642), H).serve_forever()

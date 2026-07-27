"""Mock endpoint that replies with an ANNOTATION instead of prose.

Same SSE frame shape as mock-oracle.py — this one exists to drive the drawing
modality end to end with no API key and no tablet.

The reply is a line of prose, a ⟦draw …⟧ block, then the ⁂ transcription
postscript. The block is deliberately TORN ACROSS FRAMES: mid-op, mid-
coordinate, mid-quoted-string, and finally with a body that already looks
complete while the closing ⟧ is still on the wire. StreamParser is fed the
running accumulated text, so that last boundary is the one that matters — a
half-arrived block must render nothing, and there is no un-inking once it has.

  python3 scripts/mock-oracle-draw.py [--malformed]

Env: RIDDLE_MOCK_PORT (default 8642, same as mock-oracle.py — override it to
run both at once), RIDDLE_MOCK_MALFORMED=1 same as --malformed.
"""
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

SEEN = {}

CHUNKS = [
    "I see it. ",
    "That word there —\n",
    # The opening delimiter itself lands across two frames: a parser that
    # matches on the literal "⟦draw" in one frame never fires here.
    "⟦dr",
    "aw\ncircle 412 2",
    # …and every coordinate after it is split too, so a per-frame line parse
    # would see "circle 412 2" as a complete three-token op in the wrong place.
    "68 90\narrow 300 5",
    "20 405 300\nunderline 120 640 690 6",
    "40\ntick 880 150\nnote 60 820 \"your ink wa",
    "vered here\"\n",
    # Every op is now on the wire and the body reads as finished. It is not:
    # the block is still open, and nothing may render from this frame.
    "⟧",
    "\n⁂ the rain kept on all",
    " night",
]

# Spliced in just before the closing ⟧ — after the last good op, so the ops
# around a drop are untouched and the drop count is the only difference.
# Wrong arity, a coordinate past 1000, and a name outside the vocabulary: all
# three must be DROPPED, never clamped.
BAD = [
    "circle 500 5",
    "00\nbox 100 100 14",
    "00 200\nswirl 300 300\n",
]
BAD_AT = 8


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        n = int(self.headers["Content-Length"])
        body = json.loads(self.rfile.read(n))
        SEEN["path"] = self.path
        SEEN["auth"] = self.headers.get("Authorization")
        SEEN["model"] = body.get("model")
        SEEN["stream"] = body.get("stream")
        user = [m for m in body["messages"] if m["role"] == "user"][-1]
        parts = {p["type"]: p for p in user["content"]}
        SEEN["has_image"] = "image_url" in parts
        SEEN["image_prefix"] = parts["image_url"]["image_url"]["url"][:22]
        system = body["messages"][0]["content"]
        SEEN["system_len"] = len(system)
        # Informational, not an assertion: shows at a glance whether the draw
        # protocol actually reached the prompt.
        SEEN["draw_protocol"] = "⟦draw" in system
        print("REQUEST " + json.dumps(SEEN), file=sys.stderr, flush=True)

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        chunks = list(CHUNKS)
        if MALFORMED:
            chunks[BAD_AT:BAD_AT] = BAD
        self.wfile.write(
            b'data: {"choices":[{"delta":{"role":"assistant","content":null}}]}\n\n'
        )
        for i, c in enumerate(chunks):
            frame = {"choices": [{"delta": {"content": c}, "index": 0}]}
            # Alternate compact and pretty-printed frames, as mock-oracle.py
            # does: both are valid JSON and riddle must cope with either.
            sep = (",", ":") if i % 2 == 0 else (", ", ": ")
            self.wfile.write(f"data: {json.dumps(frame, separators=sep)}\n\n".encode())
            self.wfile.flush()
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


MALFORMED = "--malformed" in sys.argv or os.environ.get("RIDDLE_MOCK_MALFORMED") == "1"
PORT = int(os.environ.get("RIDDLE_MOCK_PORT", "8642"))
print(f"mock-oracle-draw on :{PORT} malformed={MALFORMED}", file=sys.stderr, flush=True)
HTTPServer(("127.0.0.1", PORT), H).serve_forever()

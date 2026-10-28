#!/usr/bin/env python3
"""Local range-capable test server for the NDM prototype.

Usage: serve.py <file> <port> <throttle_bytes_per_sec_per_conn> <logfile>
- Supports HTTP Range (206) and HEAD.
- Throttles EACH connection separately (this is what makes multi-connection
  downloads measurably faster, like real per-connection-limited servers).
- Appends every served range to the logfile as "start-end".
"""
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

FILE, PORT, THROTTLE, LOG = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
with open(FILE, "rb") as f:
    DATA = f.read()
TOTAL = len(DATA)
CHUNK = 64 * 1024


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _send_headers(self, code, start, end):
        self.send_response(code)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("ETag", '"ndm-test-v1"')
        if code == 206:
            self.send_header("Content-Range", f"bytes {start}-{end}/{TOTAL}")
        self.send_header("Content-Length", str(end - start + 1 if code != 200 or start else TOTAL))
        self.end_headers()

    def do_HEAD(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("ETag", '"ndm-test-v1"')
        self.send_header("Content-Length", str(TOTAL))
        self.end_headers()

    def do_GET(self):
        rng = self.headers.get("Range")
        m = re.match(r"bytes=(\d+)-(\d*)", rng) if rng else None
        if m:
            start = int(m.group(1))
            end = int(m.group(2)) if m.group(2) else TOTAL - 1
            end = min(end, TOTAL - 1)
            if start >= TOTAL or start > end:
                # a real server answers 416 here; without it the code
                # below would emit a negative Content-Length — a shape
                # no real server produces, so engine paths that request
                # at/past EOF (resume vs a shortened file) went untested
                self.send_response(416)
                self.send_header("Content-Range", f"bytes */{TOTAL}")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            code = 206
        else:
            start, end, code = 0, TOTAL - 1, 200
        with open(LOG, "a") as lf:
            lf.write(f"{start}-{end}\n")
        self._send_headers(code, start, end)
        pos = start
        last = time.time()
        sent_window = 0
        while pos <= end:
            n = min(CHUNK, end - pos + 1)
            try:
                self.connection.sendall(DATA[pos : pos + n])
            except (BrokenPipeError, ConnectionResetError):
                return
            pos += n
            if THROTTLE > 0:
                sent_window += n
                now = time.time()
                elapsed = now - last
                allowed = THROTTLE * elapsed
                if sent_window > allowed:
                    time.sleep((sent_window - allowed) / THROTTLE)
                    last = time.time()
                    sent_window = 0


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()

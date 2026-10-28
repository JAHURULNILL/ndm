#!/usr/bin/env python3
"""Adversarial test servers for the NDM audit-2 regression.
Usage: adv_server.py <mode> <file> <port>
Modes:
  offsetlie  - 206 with Content-Range always starting at 0 (except the
               0-0 verify request, which is answered truthfully), body
               always from offset 0. D-1: engine must FAIL, not corrupt.
  gzip       - gzips the body iff the client advertises gzip. D-2: the
               fixed engine must receive identity bytes, SHA-exact.
  chunked    - HEAD 405; GET 200 with Transfer-Encoding: chunked, no
               Content-Length. D-5: unknown-size download must finish.
  empty416   - HEAD 405; GET bytes=0-0 -> 416 (empty file). D-4.
  slowhead   - HEAD sleeps 4s, then a normal ranged server. E-F5.
  gzipalways - gzips every response, advertised or not. D-2b.
  huge       - HEAD claims Content-Length = u64::MAX and the range
               verify agrees. Final audit G-1: start() must refuse
               the size, not abort allocating a block map from it.
  headzero   - HEAD claims Content-Length: 0 while GET serves the
               real file (ranged). Final audit G-2: the probe must
               not trust the zero — the download must be SHA-exact.
"""
import gzip
import socket
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODE = sys.argv[1]
DATA = open(sys.argv[2], 'rb').read()
PORT = int(sys.argv[3])
TOTAL = len(DATA)


class H(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *a):
        pass

    def _send(self, code, body=b'', headers=()):
        self.send_response(code)
        for k, v in headers:
            self.send_header(k, v)
        self.end_headers()
        if body:
            self.wfile.write(body)

    def do_HEAD(self):
        if MODE in ('chunked', 'empty416'):
            self._send(405)
            return
        if MODE == 'slowhead':
            time.sleep(4)
        if MODE == 'gzip':
            # a spec-compliant server mirrors the GET decision: gzip is
            # only used when the client advertises it
            if 'gzip' in (self.headers.get('Accept-Encoding') or ''):
                gz = gzip.compress(DATA)
                self._send(200, b'', [('Content-Length', str(len(gz))),
                                      ('Content-Encoding', 'gzip')])
            else:
                self._send(200, b'', [('Content-Length', str(TOTAL))])
            return
        if MODE == 'gzipalways':
            gz = gzip.compress(DATA)
            self._send(200, b'', [('Content-Length', str(len(gz))),
                                  ('Content-Encoding', 'gzip')])
            return
        if MODE == 'huge':
            self._send(200, b'', [('Content-Length', '18446744073709551615'),
                                  ('Accept-Ranges', 'bytes')])
            return
        if MODE == 'headzero':
            self._send(200, b'', [('Content-Length', '0'),
                                  ('Accept-Ranges', 'bytes'),
                                  ('ETag', '"ndm-test-v1"')])
            return
        self._send(200, b'', [('Content-Length', str(TOTAL)),
                              ('Accept-Ranges', 'bytes')])

    def do_GET(self):
        rng = self.headers.get('Range')
        if MODE == 'gzipalways':
            body = gzip.compress(DATA)
            self._send(200, body, [('Content-Length', str(len(body))),
                                    ('Content-Encoding', 'gzip')])
            return
        if MODE == 'gzip':
            if 'gzip' in (self.headers.get('Accept-Encoding') or ''):
                body = gzip.compress(DATA)
                self._send(200, body, [('Content-Length', str(len(body))),
                                        ('Content-Encoding', 'gzip')])
            else:
                self._send(200, DATA, [('Content-Length', str(TOTAL))])
            return
        if MODE == 'offsetlie':
            if rng == 'bytes=0-0':
                self._send(206, DATA[0:1], [('Content-Range', f'bytes 0-0/{TOTAL}'),
                                            ('Content-Length', '1')])
                return
            start = 0
            end = TOTAL - 1
            if rng:
                try:
                    r = rng.split('=')[1]
                    a, b = r.split('-')
                    start, end = int(a), int(b) if b else TOTAL - 1
                except Exception:
                    pass
            length = end - start + 1
            # LIE: claim the requested range in size but stream from 0,
            # and say so in Content-Range (broken-framework behaviour)
            self._send(206, DATA[0:length],
                       [('Content-Range', f'bytes 0-{length - 1}/{TOTAL}'),
                        ('Content-Length', str(length))])
            return
        if MODE == 'huge':
            if rng:
                try:
                    r = rng.split('=')[1]
                    a, b = r.split('-')
                    start = int(a)
                    end = int(b) if b else start
                except Exception:
                    start, end = 0, 0
                self._send(206, DATA[0:1],
                           [('Content-Range', f'bytes {start}-{end}/18446744073709551615'),
                            ('Content-Length', str(end - start + 1))])
                return
            self._send(200, DATA, [('Content-Length', str(TOTAL))])
            return
        if MODE == 'headzero':
            if rng:
                try:
                    r = rng.split('=')[1]
                    a, b = r.split('-')
                    start = int(a)
                    end = int(b) if b else TOTAL - 1
                except Exception:
                    start, end = 0, TOTAL - 1
                end = min(end, TOTAL - 1)
                body = DATA[start:end + 1]
                self._send(206, body, [('Content-Range', f'bytes {start}-{end}/{TOTAL}'),
                                       ('Content-Length', str(len(body)))])
                return
            self._send(200, DATA, [('Content-Length', str(TOTAL))])
            return
        if MODE == 'empty416':
            self._send(416, b'', [('Content-Range', f'bytes */0')])
            return
        if MODE == 'slowhead':
            if rng:
                try:
                    r = rng.split('=')[1]
                    a, b = r.split('-')
                    start = int(a)
                    end = int(b) if b else TOTAL - 1
                except Exception:
                    start, end = 0, TOTAL - 1
                body = DATA[start:end + 1]
                self._send(206, body, [('Content-Range', f'bytes {start}-{end}/{TOTAL}'),
                                       ('Content-Length', str(len(body)))])
                return
            self._send(200, DATA, [('Content-Length', str(TOTAL))])
            return
        self._send(200, DATA, [('Content-Length', str(TOTAL))])


def chunked_server():
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(('127.0.0.1', PORT))
    srv.listen(5)

    def handle(conn):
        try:
            req = b''
            while b'\r\n\r\n' not in req:
                chunk = conn.recv(4096)
                if not chunk:
                    break
                req += chunk
            if req.startswith(b'HEAD'):
                conn.sendall(b'HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n')
                return
            conn.sendall(b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n')
            step = 65536
            for i in range(0, TOTAL, step):
                part = DATA[i:i + step]
                conn.sendall(b'%x\r\n' % len(part) + part + b'\r\n')
            conn.sendall(b'0\r\n\r\n')
        except Exception:
            pass
        finally:
            conn.close()

    while True:
        c, _ = srv.accept()
        threading.Thread(target=handle, args=(c,), daemon=True).start()


if MODE == 'chunked':
    chunked_server()
else:
    ThreadingHTTPServer(('127.0.0.1', PORT), H).serve_forever()

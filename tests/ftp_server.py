#!/usr/bin/env python3
"""Minimal threaded FTP server fixture for the NDM engine tests.

Stdlib only. Serves a fixed root directory with USER/PASS, PASV,
TYPE, SIZE, MDTM, REST and RETR, and appends evidence lines to a
log file (CONNECT / USER / PASS / REST / RETR with offsets and
per-connection ids) that the Rust tests assert against.

Flags:
  --root DIR        directory to serve
  --port N          control port (tests use 8921)
  --log PATH        evidence log (appended, line-buffered)
  --ready PATH      file created once the listener is up
  --user U --pass P require this exact login (default: accept any)
  --no-rest         refuse REST with 502 (single-stream fallback)
  --throttle N      cap each data connection at N bytes/second
"""

import argparse
import os
import socket
import sys
import threading
import time

LOG_LOCK = threading.Lock()
LOG_PATH = None
CONN_COUNTER = 0
COUNTER_LOCK = threading.Lock()


def log(line):
    with LOG_LOCK:
        with open(LOG_PATH, "a") as f:
            f.write(line + "\n")


def next_conn_id():
    global CONN_COUNTER
    with COUNTER_LOCK:
        CONN_COUNTER += 1
        return CONN_COUNTER


class Handler(threading.Thread):
    def __init__(self, conn, addr, cfg):
        super().__init__(daemon=True)
        self.conn = conn
        self.cfg = cfg
        self.cid = next_conn_id()
        self.user = ""
        self.authed = False
        self.rest = 0
        self.data_listener = None

    def send(self, text):
        self.conn.sendall((text + "\r\n").encode())

    def resolve(self, arg):
        rel = arg.lstrip("/")
        path = os.path.normpath(os.path.join(self.cfg.root, rel))
        root = os.path.normpath(self.cfg.root)
        if path != root and not path.startswith(root + os.sep):
            return None
        return path

    def run(self):
        log(f"CONNECT conn={self.cid}")
        try:
            self.send("220 ndm-fixture ready")
            rfile = self.conn.makefile("rb")
            while True:
                raw = rfile.readline()
                if not raw:
                    break
                line = raw.decode("utf-8", "replace").strip()
                if not line:
                    continue
                parts = line.split(" ", 1)
                cmd = parts[0].upper()
                arg = parts[1] if len(parts) > 1 else ""
                if cmd == "USER":
                    self.user = arg
                    log(f"USER conn={self.cid} name={arg}")
                    self.send("331 password required")
                elif cmd == "PASS":
                    if self.cfg.user is not None:
                        self.authed = self.user == self.cfg.user and arg == self.cfg.password
                    else:
                        self.authed = True
                    log(f"PASS conn={self.cid} result={'ok' if self.authed else 'fail'}")
                    self.send("230 logged in" if self.authed else "530 login incorrect")
                elif cmd == "QUIT":
                    self.send("221 bye")
                    break
                elif not self.authed:
                    self.send("530 not logged in")
                elif cmd == "SYST":
                    self.send("215 UNIX Type: L8")
                elif cmd == "FEAT":
                    self.send("211-features")
                    if not self.cfg.no_rest:
                        self.send(" REST STREAM")
                    self.send(" SIZE")
                    self.send(" MDTM")
                    self.send("211 end")
                elif cmd == "TYPE":
                    self.send("200 type set")
                elif cmd == "NOOP":
                    self.send("200 ok")
                elif cmd == "OPTS":
                    self.send("200 ok")
                elif cmd == "PWD":
                    self.send('257 "/"')
                elif cmd in ("CWD", "CDUP"):
                    self.send("250 ok")
                elif cmd == "ABOR":
                    self.send("226 ok")
                elif cmd == "PASV":
                    if self.data_listener:
                        self.data_listener.close()
                    ls = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                    ls.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                    ls.bind(("127.0.0.1", 0))
                    ls.listen(1)
                    self.data_listener = ls
                    port = ls.getsockname()[1]
                    self.send(
                        "227 Entering Passive Mode (127,0,0,1,%d,%d)"
                        % (port // 256, port % 256)
                    )
                elif cmd == "EPSV":
                    self.send("502 EPSV not supported")
                elif cmd == "SIZE":
                    path = self.resolve(arg)
                    if path and os.path.isfile(path):
                        self.send(f"213 {os.path.getsize(path)}")
                    else:
                        self.send("550 not found")
                elif cmd == "MDTM":
                    path = self.resolve(arg)
                    if path and os.path.isfile(path):
                        stamp = time.strftime(
                            "%Y%m%d%H%M%S", time.gmtime(os.path.getmtime(path))
                        )
                        self.send(f"213 {stamp}")
                    else:
                        self.send("550 not found")
                elif cmd == "REST":
                    log(f"REST conn={self.cid} offset={arg}")
                    if self.cfg.no_rest:
                        self.send("502 REST not supported")
                    else:
                        try:
                            self.rest = int(arg)
                            self.send("350 restart position accepted")
                        except ValueError:
                            self.send("501 bad offset")
                elif cmd == "RETR":
                    if not self.handle_retr(arg):
                        break
                else:
                    self.send("502 not implemented")
        except (ConnectionResetError, BrokenPipeError, OSError):
            pass
        finally:
            try:
                if self.data_listener:
                    self.data_listener.close()
                self.conn.close()
            except OSError:
                pass

    def handle_retr(self, arg):
        path = self.resolve(arg)
        if not path or not os.path.isfile(path):
            self.send("550 not found")
            return True
        if not self.data_listener:
            self.send("425 no data connection")
            return True
        offset = self.rest
        self.rest = 0
        log(f"RETR conn={self.cid} path={arg} offset={offset}")
        try:
            self.data_listener.settimeout(15)
            data, _ = self.data_listener.accept()
        except OSError:
            self.send("425 data connection failed")
            return True
        self.data_listener.close()
        self.data_listener = None
        self.send("150 opening data connection")
        try:
            with open(path, "rb") as f:
                f.seek(offset)
                while True:
                    chunk = f.read(64 * 1024)
                    if not chunk:
                        break
                    data.sendall(chunk)
                    if self.cfg.throttle:
                        time.sleep(len(chunk) / self.cfg.throttle)
        except (ConnectionResetError, BrokenPipeError, OSError):
            try:
                data.close()
            except OSError:
                pass
            return False  # client aborted mid-transfer; control is gone or going
        try:
            data.close()
        except OSError:
            pass
        try:
            self.send("226 transfer complete")
        except OSError:
            return False
        return True


def main():
    global LOG_PATH
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", required=True)
    ap.add_argument("--port", type=int, default=8921)
    ap.add_argument("--log", required=True)
    ap.add_argument("--ready")
    ap.add_argument("--user")
    ap.add_argument("--pass", dest="password")
    ap.add_argument("--no-rest", action="store_true")
    ap.add_argument("--throttle", type=int, default=0)
    cfg = ap.parse_args()
    LOG_PATH = cfg.log
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", cfg.port))
    srv.listen(64)
    if cfg.ready:
        with open(cfg.ready, "w") as f:
            f.write("ready\n")
    sys.stderr.write(f"ftp fixture listening on {cfg.port}\n")
    while True:
        conn, addr = srv.accept()
        Handler(conn, addr, cfg).start()


if __name__ == "__main__":
    main()

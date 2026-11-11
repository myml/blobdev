#!/usr/bin/env python3
"""Minimal WebDAV server for local end-to-end tests.

Implements exactly what blobdev needs: GET (with Range), HEAD, PUT (whole and
Content-Range partial), DELETE. Standard library only.

Usage: webdav_server.py ROOT ADDRESS [PORT]
"""
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    root = "."
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        if os.environ.get("WEBDAV_VERBOSE"):
            sys.stderr.write("%s - %s\n" % (self.address_string(), fmt % args))

    def _path(self):
        p = self.path.split("?")[0].lstrip("/")
        full = os.path.normpath(os.path.join(self.root, p))
        if not full.startswith(os.path.abspath(self.root)):
            return None
        return full

    def _parse_range(self, size):
        h = self.headers.get("Range")
        if not h or not h.startswith("bytes="):
            return None
        spec = h[6:].split(",")[0].strip()
        if "-" not in spec:
            return None
        a, b = spec.split("-", 1)
        if a == "":
            # suffix range
            n = int(b)
            start = max(0, size - n)
            return start, size - 1
        start = int(a)
        end = int(b) if b else size - 1
        return start, min(end, size - 1)

    def do_HEAD(self):
        p = self._path()
        if p is None or not os.path.isfile(p):
            self.send_error(404)
            return
        size = os.path.getsize(p)
        self.send_response(200)
        self.send_header("Content-Length", str(size))
        self.send_header("Accept-Ranges", "bytes")
        self.end_headers()

    def do_GET(self):
        p = self._path()
        if p is None or not os.path.isfile(p):
            self.send_error(404)
            return
        size = os.path.getsize(p)
        rng = self._parse_range(size)
        with open(p, "rb") as f:
            if rng:
                start, end = rng
                f.seek(start)
                data = f.read(end - start + 1)
                self.send_response(206)
                self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
            else:
                data = f.read()
                self.send_response(200)
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Accept-Ranges", "bytes")
            self.end_headers()
            self.wfile.write(data)

    def do_PUT(self):
        p = self._path()
        if p is None:
            self.send_error(400)
            return
        os.makedirs(os.path.dirname(p) or ".", exist_ok=True)
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length) if length else b""
        cr = self.headers.get("Content-Range")
        if cr:
            # Content-Range: bytes START-END/TOTAL
            try:
                spec = cr.split(" ", 1)[1]
                rng, total = spec.split("/")
                start = int(rng.split("-")[0])
                total = int(total)
            except Exception:
                self.send_error(400, "bad Content-Range")
                return
            if not os.path.exists(p):
                # Creating with a partial write is not supported.
                self.send_error(409, "partial write to missing object")
                return
            with open(p, "r+b") as f:
                f.seek(start)
                f.write(body)
            self.send_response(204)
            self.end_headers()
            return
        with open(p, "wb") as f:
            f.write(body)
        self.send_response(201)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_MKCOL(self):
        p = self._path()
        if p is None:
            self.send_error(400)
            return
        if os.path.exists(p):
            self.send_response(405)  # already exists: the normal steady state
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        try:
            os.makedirs(p)
        except FileExistsError:
            self.send_response(405)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        except OSError:
            self.send_error(409)  # parent collection missing
            return
        self.send_response(201)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_DELETE(self):
        p = self._path()
        if p is None or not os.path.exists(p):
            self.send_error(404)
            return
        os.remove(p)
        self.send_response(204)
        self.end_headers()


def main():
    root = sys.argv[1]
    addr = sys.argv[2] if len(sys.argv) > 2 else "127.0.0.1"
    port = int(sys.argv[3]) if len(sys.argv) > 3 else 8080
    os.makedirs(root, exist_ok=True)
    Handler.root = os.path.abspath(root)
    srv = ThreadingHTTPServer((addr, port), Handler)
    print(f"webdav on http://{addr}:{port} serving {Handler.root}", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""An HTTP proxy that fails requests on purpose.

Sits in front of a real object store and injects the failures a network or a
busy server produces: dropped connections, 500s, 503s, 429s, slow responses,
and replies that arrive half way through the body. Nothing here is exotic --
it is the ordinary weather between a client and a remote store -- but without
it the retry path is only ever exercised by the unit tests' mocks.

Every failure it injects is counted and printed on request (GET /stats), so a
test can assert what actually happened rather than assume it.

usage: fault_proxy.py <listen-port> <upstream-base-url> [profile]

profiles:
  none        pass everything through (control)
  flaky       every Nth request fails with a 503 (transient)
  reset       every Nth request's connection is dropped before replying
  slow        every Nth request is delayed by a second
  partial     every Nth request gets headers, then a truncated body
  hard        every Nth request gets a 400 (permanent, must not be retried)
"""
import http.client
import http.server
import socketserver
import sys
import threading
import time
import urllib.parse

STATE = {
    "requests": 0,
    "injected": 0,
    "pass": 0,
    "by_kind": {},
}
LOCK = threading.Lock()


class Proxy(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        self.handle_any()

    def do_HEAD(self):
        self.handle_any()

    def do_PUT(self):
        self.handle_any()

    def do_DELETE(self):
        self.handle_any()

    def do_POST(self):
        self.handle_any()

    def do_PROPFIND(self):
        self.handle_any()

    def do_MKCOL(self):
        self.handle_any()

    def stats_page(self):
        with LOCK:
            body = "".join(f"{k}={v}\n" for k, v in STATE.items() if k != "by_kind")
            for k, v in sorted(STATE["by_kind"].items()):
                body += f"kind_{k}={v}\n"
        data = body.encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def handle_any(self):
        if self.path == "/stats":
            return self.stats_page()

        with LOCK:
            STATE["requests"] += 1
            n = STATE["requests"]
        profile = self.server.profile
        every = self.server.every

        # Only act on object traffic; the bucket probe and the stats page must
        # work so a test can tell "the store is down" from "the proxy is".
        inject = profile != "none" and every > 0 and n % every == 0
        if inject:
            kind = profile
            with LOCK:
                STATE["injected"] += 1
                STATE["by_kind"][kind] = STATE["by_kind"].get(kind, 0) + 1
            if kind == "flaky":
                return self.simple(503, b"try again")
            if kind == "reset":
                # Drop the connection without any reply.
                self.close_connection = True
                try:
                    self.connection.close()
                except OSError:
                    pass
                return
            if kind == "slow":
                time.sleep(1.0)
            if kind == "partial":
                self.send_response(200)
                self.send_header("Content-Length", "1048576")
                self.end_headers()
                self.wfile.write(b"x" * 1024)
                self.close_connection = True
                return
            if kind == "hard":
                return self.simple(400, b"bad request")

        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""

        parsed = urllib.parse.urlparse(self.server.upstream)
        conn = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=120)
        path = parsed.path.rstrip("/") + self.path
        headers = {
            k: v
            for k, v in self.headers.items()
            if k.lower() not in ("host", "connection", "content-length")
        }
        if length:
            headers["Content-Length"] = str(length)
        try:
            conn.request(self.command, path, body=body, headers=headers)
            resp = conn.getresponse()
            data = resp.read()
        except Exception as exc:  # noqa: BLE001 - any upstream problem is a 502
            with LOCK:
                STATE["by_kind"]["upstream_error"] = (
                    STATE["by_kind"].get("upstream_error", 0) + 1
                )
            return self.simple(502, str(exc).encode())
        finally:
            conn.close()

        with LOCK:
            STATE["pass"] += 1
        self.send_response(resp.status)
        for k, v in resp.getheaders():
            if k.lower() in ("content-length", "connection", "transfer-encoding"):
                continue
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)

    def simple(self, status, body):
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        # A HEAD reply carries headers only; sending a body after them makes
        # the client parse the body as the next response's status line.
        if self.command != "HEAD":
            self.wfile.write(body)


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True


def main():
    port = int(sys.argv[1])
    upstream = sys.argv[2]
    profile = sys.argv[3] if len(sys.argv) > 3 else "none"
    every = int(sys.argv[4]) if len(sys.argv) > 4 else 5
    srv = Server(("127.0.0.1", port), Proxy)
    srv.upstream = upstream
    srv.profile = profile
    srv.every = every
    srv.serve_forever()


if __name__ == "__main__":
    main()

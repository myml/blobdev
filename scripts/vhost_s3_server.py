#!/usr/bin/env python3
"""A minimal S3 server that resolves the bucket from the Host header.

Written to test virtual-host addressing, which self-hosted servers often do
not support: with `Host: <bucket>.<endpoint>` this server puts objects in
`<root>/<bucket>/<key>`, exactly what AWS does with wildcard DNS. Without the
bucket in the host it falls back to path-style, so the same process also
answers blobdev's --path-style runs.

Supported: PUT, GET (with Range), HEAD, DELETE, GET ?list-type=2. Signature is
not verified -- this is about addressing, not authentication.
"""
import http.server
import os
import sys
import urllib.parse
import xml.sax.saxutils as sax


class Server(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, addr, handler, root, domains):
        super().__init__(addr, handler)
        self.root = root
        # Host suffixes that mark a virtual-host request, e.g. ".127.0.0.1".
        self.domains = domains


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def log_request_line(self, bucket, key):
        """Record how the bucket was resolved, for diagnosing addressing."""
        if os.environ.get("VHOST_S3_LOG"):
            host = (self.headers.get("Host") or "").split(":")[0]
            style = "vhost" if any(host.endswith(d) and host[: -len(d)] for d in self.server.domains) else "path"
            sys.stderr.write(
                f"{self.command} host={host} style={style} bucket={bucket} key={key}\n"
            )
            sys.stderr.flush()

    def resolve(self):
        """Return (bucket, key) for this request, or (None, None)."""
        host = (self.headers.get("Host") or "").split(":")[0]
        parsed = urllib.parse.urlparse(self.path)
        path = urllib.parse.unquote(parsed.path).lstrip("/")

        self.style = "path"
        for domain in self.server.domains:
            if host.endswith(domain):
                bucket = host[: -len(domain)]
                if bucket:
                    self.style = "vhost"
                    return bucket, path
        # Path style: /<bucket>/<key>
        if not path:
            return None, None
        bucket, _, key = path.partition("/")
        return bucket, key

    def object_path(self, bucket, key):
        safe = key.replace("..", "__")
        return os.path.join(self.server.root, bucket, safe)

    def do_PUT(self):
        bucket, key = self.resolve()
        if not bucket or not key:
            return self.send_error(400, "no bucket in request")
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        path = self.object_path(bucket, key)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        start = self.headers.get("Content-Range")
        if start:
            # Honour a ranged write: patch in place, do not truncate.
            spec = start.replace("bytes", "").strip()
            span, _, _total = spec.partition("/")
            lo, _, hi = span.partition("-")
            lo = int(lo)
            if not os.path.exists(path):
                return self.send_error(416, "ranged write to a missing object")
            with open(path, "r+b") as f:
                f.seek(lo)
                f.write(body)
        else:
            with open(path, "wb") as f:
                f.write(body)
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):
        bucket, key = self.resolve()
        parsed = urllib.parse.urlparse(self.path)
        if not bucket:
            return self.send_error(400, "no bucket in request")
        if not key:
            return self.list_bucket(bucket, parsed.query)
        path = self.object_path(bucket, key)
        if not os.path.isfile(path):
            return self.send_error(404, "no such key")
        data = open(path, "rb").read()
        status = 200
        rng = self.headers.get("Range")
        if rng and rng.startswith("bytes="):
            spec = rng[len("bytes="):]
            lo, _, hi = spec.partition("-")
            lo = int(lo) if lo else 0
            hi = int(hi) if hi else len(data) - 1
            hi = min(hi, len(data) - 1)
            total = len(data)
            data = data[lo:hi + 1]
            status = 206
            self.send_response(status)
            self.send_header("Content-Range", f"bytes {lo}-{hi}/{total}")
        else:
            self.send_response(status)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("ETag", '"%d"' % len(data))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)

    def do_HEAD(self):
        self.do_GET()

    def do_DELETE(self):
        bucket, key = self.resolve()
        if not bucket or not key:
            return self.send_error(400, "no bucket in request")
        path = self.object_path(bucket, key)
        try:
            os.remove(path)
        except FileNotFoundError:
            pass
        self.send_response(204)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def list_bucket(self, bucket, query):
        params = urllib.parse.parse_qs(query)
        if params.get("list-type") != ["2"]:
            return self.send_error(400, "only ListObjectsV2 is supported")
        prefix = params.get("prefix", [""])[0]
        base = os.path.join(self.server.root, bucket)
        keys = []
        for root, _dirs, files in os.walk(base):
            for name in files:
                full = os.path.join(root, name)
                rel = os.path.relpath(full, base).replace(os.sep, "/")
                if rel.startswith(prefix):
                    keys.append((rel, os.path.getsize(full)))
        keys.sort()
        parts = [
            '<?xml version="1.0" encoding="UTF-8"?>',
            '<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">',
            f"<Name>{sax.escape(bucket)}</Name>",
            f"<Prefix>{sax.escape(prefix)}</Prefix>",
            f"<KeyCount>{len(keys)}</KeyCount>",
            f"<MaxKeys>1000</MaxKeys>",
            "<IsTruncated>false</IsTruncated>",
        ]
        for key, size in keys:
            parts.append(
                "<Contents>"
                f"<Key>{sax.escape(urllib.parse.quote(key))}</Key>"
                f"<Size>{size}</Size>"
                "</Contents>"
            )
        parts.append("</ListBucketResult>")
        body = "".join(parts).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/xml")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main():
    port = int(sys.argv[1])
    root = sys.argv[2]
    domains = sys.argv[3:] or [".127.0.0.1"]
    os.makedirs(root, exist_ok=True)
    server = Server(("127.0.0.1", port), Handler, root, domains)
    server.serve_forever()


if __name__ == "__main__":
    main()

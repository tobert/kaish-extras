#!/usr/bin/env python3
"""Survey fixture server for docs/design/curl-survey-2026-11.md.

    server.py <port>

Loopback only. Endpoints (all deterministic; /reset zeroes counters):
  GET  /json            {"id":1,"name":"widget","tags":["a","b"]}
  GET  /text            plain text "hello from the survey server\n"
  GET  /redirect        302 -> /json ; /redirect2 302 -> /redirect
  GET  /status/<n>      status n with JSON body {"error":"status n"}
  ANY  /echo[?query]    JSON: method, path, query, content_type, body, and the
                        request headers the survey cares about
  GET  /slow            sleeps 3s, then "slow done\n"
  GET  /file/report.txt 4096 bytes of "0123456789abcdef\n" lines; honors Range
  GET  /flaky           503 twice, then 200 "ok after retries" (counter; /reset)
  GET  /basic-auth      200 for user:pass, else 401 + WWW-Authenticate
  GET  /bearer          200 for "Bearer abc123", else 401
  POST|PUT /upload      JSON: method, content_type, length, body (utf-8 text)
"""
import base64, json, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

FILE = ("0123456789abcdef\n" * 256).encode()  # 4352 bytes
state = {"flaky": 0}
SHOW = ("authorization", "content-type", "user-agent", "x-api-key", "x-custom",
        "referer", "cookie", "content-length", "accept")


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _send(self, code, body=b"", ctype="text/plain", extra=()):
        if isinstance(body, str):
            body = body.encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        for k, v in extra:
            self.send_header(k, v)
        self.end_headers()
        if self.command != "HEAD":
            try:
                self.wfile.write(body)
            except BrokenPipeError:
                pass  # client gave up (--max-time); not a server fault

    def _handle(self):
        u = urlsplit(self.path)
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else b""
        p = u.path
        if p == "/reset":
            state["flaky"] = 0
            return self._send(200, "reset\n")
        if p == "/json":
            return self._send(200, '{"id":1,"name":"widget","tags":["a","b"]}\n', "application/json")
        if p == "/text":
            return self._send(200, "hello from the survey server\n")
        if p == "/redirect":
            return self._send(302, "", extra=[("Location", "/json")])
        if p == "/redirect2":
            return self._send(302, "", extra=[("Location", "/redirect")])
        if p.startswith("/status/"):
            c = int(p.rsplit("/", 1)[1])
            return self._send(c, json.dumps({"error": f"status {c}"}) + "\n", "application/json")
        if p == "/echo":
            h = {k: self.headers[k] for k in SHOW if self.headers.get(k) is not None}
            out = {"method": self.command, "path": p, "query": u.query,
                   "content_type": self.headers.get("Content-Type"),
                   "body": body.decode("utf-8", "replace"), "headers": h}
            return self._send(200, json.dumps(out, sort_keys=True) + "\n", "application/json")
        if p == "/slow":
            time.sleep(3)
            return self._send(200, "slow done\n")
        if p == "/file/report.txt":
            rng = self.headers.get("Range")
            if rng and rng.startswith("bytes="):
                start = int(rng[6:].split("-")[0])
                part = FILE[start:]
                return self._send(206, part, "text/plain",
                                  [("Content-Range", f"bytes {start}-{len(FILE)-1}/{len(FILE)}"),
                                   ("Content-Disposition", 'attachment; filename="report.txt"')])
            return self._send(200, FILE, "text/plain",
                              [("Accept-Ranges", "bytes"),
                               ("Content-Disposition", 'attachment; filename="report.txt"')])
        if p == "/flaky":
            state["flaky"] += 1
            if state["flaky"] <= 2:
                return self._send(503, "unavailable\n")
            return self._send(200, "ok after retries\n")
        if p == "/basic-auth":
            want = "Basic " + base64.b64encode(b"alice:s3cret").decode()
            if self.headers.get("Authorization") == want:
                return self._send(200, '{"auth":"basic","user":"alice"}\n', "application/json")
            return self._send(401, "denied\n", extra=[("WWW-Authenticate", 'Basic realm="survey"')])
        if p == "/bearer":
            if self.headers.get("Authorization") == "Bearer abc123":
                return self._send(200, '{"auth":"bearer"}\n', "application/json")
            return self._send(401, "denied\n")
        if p == "/upload":
            ct = self.headers.get("Content-Type") or ""
            txt = body.decode("utf-8", "replace")
            if ct.startswith("multipart/"):
                # boundary varies per client; keep only the stable parts
                txt = "\n".join(l for l in txt.replace("\r", "").split("\n") if not l.startswith("--"))
                ct = "multipart/form-data"
            out = {"method": self.command, "content_type": ct, "length": len(body), "body": txt}
            return self._send(200, json.dumps(out, sort_keys=True) + "\n", "application/json")
        return self._send(404, "no such endpoint\n")

    do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = do_HEAD = _handle


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()

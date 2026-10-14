#!/usr/bin/env python3
"""Local stand-in for an outbound notification API.

Records each POST as one JSON line: {"path": "...", "body": "..."}.
Bind address is 127.0.0.1. Pass port 0 to let the OS choose a port; the bound
port is printed on stdout before the server accepts requests.

This fixture never forwards traffic. Smoke tests point the service at it with
placeholder credentials such as REPLACE_ME.
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def main() -> None:
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} PORT LOG_PATH", file=sys.stderr)
        sys.exit(2)
    requested = int(sys.argv[1])
    log_path = sys.argv[2]

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self) -> None:  # noqa: N802 - stdlib handler name
            length = int(self.headers.get("Content-Length", "0") or "0")
            body = self.rfile.read(length)
            record = {
                "path": self.path,
                "body": body.decode("utf-8", "replace"),
            }
            with open(log_path, "a", encoding="utf-8") as handle:
                handle.write(json.dumps(record) + "\n")
                handle.flush()
            payload = b'{"status":1}'
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def log_message(self, fmt: str, *args) -> None:
            return

    server = ThreadingHTTPServer(("127.0.0.1", requested), Handler)
    print(server.server_address[1], flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()

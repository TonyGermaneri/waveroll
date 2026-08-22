#!/usr/bin/env python3
"""Development server with the two headers SharedArrayBuffer needs.

A plain static server will not do: `SharedArrayBuffer` is only exposed to cross-origin-isolated
documents, and isolation requires COOP and COEP on every response. On GitHub Pages, where response
headers cannot be set at all, a service worker supplies them instead — see the README.
"""

import http.server
import os
import sys

ROOT = os.path.dirname(os.path.abspath(__file__))


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=ROOT, **kwargs)

    def end_headers(self):
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        self.send_header("Cross-Origin-Resource-Policy", "same-origin")
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8788
    print(f"waveroll dev server on http://localhost:{port}/  (COOP/COEP on)")
    http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()

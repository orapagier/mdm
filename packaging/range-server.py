#!/usr/bin/env python3
"""A minimal HTTP server that honours Range requests.

Python's stdlib http.server answers every request with 200 and the whole body,
which makes it useless for testing a segmented downloader: aria2 would silently
fall back to a single connection. This serves 206 Partial Content properly, and
logs how many ranged requests it saw so a test can assert that segmentation
actually happened.

    python3 range-server.py <directory> <port>
"""

import os
import re
import signal
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

RANGE_RE = re.compile(r"^bytes=(\d*)-(\d*)$")

stats_lock = threading.Lock()
stats = {"total": 0, "ranged": 0}


class RangeHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "RangeTest/1.0"

    def log_message(self, *args):
        pass  # the access log would drown the test output

    def _resolve(self):
        rel = self.path.split("?", 1)[0].lstrip("/")
        root = os.path.realpath(self.directory)
        path = os.path.realpath(os.path.join(root, rel))
        # Refuse anything that escapes the served directory.
        if not path.startswith(root + os.sep) and path != root:
            return None
        return path if os.path.isfile(path) else None

    def do_HEAD(self):
        self._serve(body=False)

    def do_GET(self):
        self._serve(body=True)

    def _serve(self, body):
        path = self._resolve()
        if path is None:
            self.send_error(404)
            return

        size = os.path.getsize(path)
        header = self.headers.get("Range")
        start, end = 0, size - 1
        partial = False

        if header:
            m = RANGE_RE.match(header.strip())
            if m:
                first, last = m.group(1), m.group(2)
                if first:
                    start = int(first)
                    end = int(last) if last else size - 1
                else:
                    # Suffix form: the final N bytes.
                    start = max(0, size - int(last))
                if start >= size or start > end:
                    self.send_response(416)
                    self.send_header("Content-Range", f"bytes */{size}")
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                end = min(end, size - 1)
                partial = True

        with stats_lock:
            stats["total"] += 1
            if partial:
                stats["ranged"] += 1

        length = end - start + 1
        self.send_response(206 if partial else 200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(length))
        self.send_header("Accept-Ranges", "bytes")
        if partial:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.end_headers()

        if not body:
            return
        with open(path, "rb") as fh:
            fh.seek(start)
            remaining = length
            while remaining > 0:
                chunk = fh.read(min(65536, remaining))
                if not chunk:
                    break
                try:
                    self.wfile.write(chunk)
                except (BrokenPipeError, ConnectionResetError):
                    return
                remaining -= len(chunk)


def main():
    directory = sys.argv[1] if len(sys.argv) > 1 else "."
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 8732

    handler = type("H", (RangeHandler,), {"directory": directory})
    server = ThreadingHTTPServer(("127.0.0.1", port), handler)
    server.daemon_threads = True
    print(f"serving {directory} on 127.0.0.1:{port}", flush=True)

    def report(*_):
        print(
            f"\nrequests: {stats['total']} total, {stats['ranged']} ranged",
            flush=True,
        )
        os._exit(0)

    # SIGTERM terminates the interpreter outright, so neither `finally` nor an
    # atexit hook would run; the stats have to be printed from the handler.
    signal.signal(signal.SIGTERM, report)
    signal.signal(signal.SIGINT, report)
    server.serve_forever()


if __name__ == "__main__":
    main()

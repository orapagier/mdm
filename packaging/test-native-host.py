#!/usr/bin/env python3
"""Exercise the native messaging host exactly as Firefox does.

Firefox speaks a 4-byte native-endian length prefix followed by UTF-8 JSON, on
stdin/stdout. Anything else on stdout desynchronises the stream permanently, so
this also asserts that the host emits nothing but well-formed frames.

    python3 test-native-host.py <path-to-mdm-host> [--expect-app]
"""

import json
import os
import struct
import subprocess
import sys
import time


def frame(obj):
    payload = json.dumps(obj).encode("utf-8")
    return struct.pack("@I", len(payload)) + payload


def read_frame(stream, timeout=20.0):
    """Read one length-prefixed message, or None on clean EOF."""
    header = stream.read(4)
    if not header or len(header) < 4:
        return None
    (length,) = struct.unpack("@I", header)
    if length > 64 * 1024 * 1024:
        raise AssertionError(f"absurd frame length {length}")
    body = b""
    while len(body) < length:
        chunk = stream.read(length - len(body))
        if not chunk:
            raise AssertionError("stream ended mid-frame")
        body += chunk
    return json.loads(body.decode("utf-8"))


def main():
    host = sys.argv[1]
    if not os.path.isfile(host):
        sys.exit(f"no such binary: {host}")

    messages = [
        {"id": "1", "type": "ping"},
        {"id": "2", "type": "hello", "version": "1.0.0"},
        {
            "id": "3",
            "type": "download",
            "job": {
                "url": "http://127.0.0.1:8732/test-24m.bin",
                "filename": "via-native-host.bin",
                "size": 25165824,
                "mime": "application/octet-stream",
                "headers": [
                    {"name": "User-Agent", "value": "Mozilla/5.0"},
                    {"name": "Referer", "value": "http://127.0.0.1:8732/"},
                ],
                "referrer": "http://127.0.0.1:8732/",
                "reason": "content-disposition",
                "source": "webRequest",
            },
        },
    ]

    proc = subprocess.Popen(
        [host],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env={**os.environ},
    )

    failures = []
    replies = []
    try:
        for msg in messages:
            proc.stdin.write(frame(msg))
            proc.stdin.flush()
            reply = read_frame(proc.stdout)
            if reply is None:
                failures.append(f"{msg['type']}: host closed the stream")
                break
            replies.append((msg, reply))
            print(f"  -> {msg['type']:9} <- {json.dumps(reply)}")

            if reply.get("id") != msg["id"]:
                failures.append(
                    f"{msg['type']}: id not echoed "
                    f"(sent {msg['id']!r}, got {reply.get('id')!r})"
                )
    finally:
        try:
            proc.stdin.close()
        except BrokenPipeError:
            pass
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()

    stderr = proc.stderr.read().decode("utf-8", "replace")

    # Assertions on the protocol contract.
    if len(replies) != len(messages):
        failures.append(f"expected {len(messages)} replies, got {len(replies)}")

    for msg, reply in replies:
        if msg["type"] in ("ping", "hello") and not reply.get("ok"):
            failures.append(f"{msg['type']} was not acknowledged: {reply}")
        if msg["type"] == "download" and not reply.get("accepted"):
            failures.append(f"download was rejected: {reply}")

    print("\n--- host stderr ---")
    print(stderr.strip() or "(none)")

    if failures:
        print("\nFAILED:")
        for f in failures:
            print("  - " + f)
        sys.exit(1)

    print("\nAll native-host round trips OK")


if __name__ == "__main__":
    main()

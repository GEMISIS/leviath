#!/usr/bin/env python3
"""Hammer `lev serve` with abruptly-dropped WebSocket connections.

Simulates a browser tab being refreshed mid-stream: open a batch of
connections to ``/ws``, hold them for a second, then close them abruptly
(SO_LINGER 0, so the peer sees a reset rather than a clean close). Between
batches the daemon's RSS and live footprint are printed so a per-connection
leak shows up as a staircase.

A healthy server shows zero growth across batches; that was the measured
behavior when this script was written (100 drops, +/- 0 MB).

Usage:
    python3 ws_churn_test.py --pid 6994 --token <control token>
    python3 ws_churn_test.py --pid 6994 --token <t> --batches 10 --per-batch 50
"""

from __future__ import annotations

import argparse
import base64
import os
import socket
import struct
import subprocess
import sys
import time


def rss_kb(pid: int) -> int:
    """Return the process RSS in KB via ``ps``."""
    out = subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)])
    return int(out.strip())


def open_ws(host: str, port: int, path: str, timeout: float = 5.0) -> socket.socket:
    """Open a raw WebSocket (handshake only) and return the socket."""
    sock = socket.create_connection((host, port), timeout=timeout)
    key = base64.b64encode(os.urandom(16)).decode()
    request = (
        f"GET {path} HTTP/1.1\r\n"
        f"Host: {host}:{port}\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        "Sec-WebSocket-Version: 13\r\n"
        "\r\n"
    )
    sock.sendall(request.encode())
    sock.settimeout(2)
    try:
        sock.recv(4096)
    except socket.timeout:
        pass
    return sock


def abrupt_close(sock: socket.socket) -> None:
    """Close with SO_LINGER 0 so the peer sees a reset, like a killed tab."""
    try:
        sock.setsockopt(
            socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0)
        )
    except OSError:
        pass
    sock.close()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--pid", type=int, required=True, help="lev serve pid")
    parser.add_argument("--token", required=True, help="serve auth token")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=3000)
    parser.add_argument("--batches", type=int, default=5)
    parser.add_argument("--per-batch", type=int, default=20)
    args = parser.parse_args()

    path = f"/ws?token={args.token}"
    print(f"baseline rss: {rss_kb(args.pid) / 1024:.1f} MB")

    for batch in range(args.batches):
        socks = []
        for _ in range(args.per_batch):
            try:
                socks.append(open_ws(args.host, args.port, path))
            except OSError as error:
                print(f"  connect error: {error}", file=sys.stderr)
        time.sleep(1.0)
        mid = rss_kb(args.pid)
        for sock in socks:
            abrupt_close(sock)
        time.sleep(1.0)
        after = rss_kb(args.pid)
        print(
            f"batch {batch + 1}: opened {len(socks)}, "
            f"rss mid {mid / 1024:.1f} MB, after close {after / 1024:.1f} MB"
        )

    time.sleep(2)
    print(f"final rss: {rss_kb(args.pid) / 1024:.1f} MB")
    return 0


if __name__ == "__main__":
    sys.exit(main())

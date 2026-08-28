#!/usr/bin/env python3
"""Drive `lev dash` over a pty and measure it.

`lev dash` only enters the real render path with a terminal on stdout, so
this forks a pty, sets a window size, feeds keys, and accumulates the WHOLE
output stream. Accumulating everything matters: a full pty buffer blocks the
child on write, throttles the render loop, and turns the number being measured
into a measurement of this script.

Reports (JSON):
    cpu_seconds     ru_utime + ru_stime of the child, exact and portable
    max_rss_bytes   ru_maxrss, normalised (macOS reports bytes, Linux KiB)
    bytes_written   total escape-stream bytes (a rendering change that emits
                    far more is a regression even if CPU falls)
    repaints        full repaints seen (cursor-home + clear sequences)
    frame1_sha256   sha256 of the first full frame with whitespace stripped and
                    clock-shaped tokens masked; must match before/after a change

    --bin PATH --seconds N --cols C --rows R --keys 'jjj' --json OUT --stream OUT.bin
"""
import argparse
import faulthandler
import fcntl
import hashlib
import json
import os
import pty
import re
import resource
import select
import signal
import struct
import sys
import termios
import time


def normalise(frame: bytes) -> bytes:
    frame = re.sub(rb"\d\d:\d\d(:\d\d)?", b"T", frame)
    frame = re.sub(rb"\b\d+(\.\d+)?\s?(ms|s|m|h|d)\b", b"D", frame)
    frame = re.sub(rb"\b\d+ ago\b", b"D ago", frame)
    return re.sub(rb"\s+", b"", frame)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.environ.get("LV_BIN", "lev"))
    ap.add_argument("--seconds", type=float, default=30)
    ap.add_argument("--cols", type=int, default=200)
    ap.add_argument("--rows", type=int, default=50)
    ap.add_argument("--keys", default="", help="keys to send after 2s, one per 300ms; \\e for Escape")
    ap.add_argument("--json")
    ap.add_argument("--stream", help="write the raw escape stream here")
    a = ap.parse_args()
    # A watchdog: if the run overshoots by 30s, dump every thread's stack and
    # exit non-zero rather than hang a CI job.
    faulthandler.dump_traceback_later(a.seconds + 30, exit=True)

    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(a.bin, [a.bin, "dash"])
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", a.rows, a.cols, 0, 0))

    keys = a.keys.encode().decode("unicode_escape").encode("latin-1")
    out = bytearray()
    deadline = time.monotonic() + a.seconds
    next_key = time.monotonic() + 2.0
    key_i = 0
    while time.monotonic() < deadline:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                chunk = os.read(fd, 1 << 16)
            except OSError:
                break
            if not chunk:
                break
            out.extend(chunk)
        if key_i < len(keys) and time.monotonic() >= next_key:
            os.write(fd, keys[key_i:key_i + 1])
            key_i += 1
            next_key = time.monotonic() + 0.3
    # Teardown escalates: `q` (then `y` for a confirm dialog), SIGTERM, SIGKILL.
    # Keep draining the pty meanwhile so the child never blocks on a full buffer.
    def reap(grace):
        end = time.monotonic() + grace
        while time.monotonic() < end:
            wpid, status, ru = os.wait4(pid, os.WNOHANG)
            if wpid == pid:
                return status, ru
            r, _, _ = select.select([fd], [], [], 0.05)
            if r:
                try:
                    out.extend(os.read(fd, 1 << 16))
                except OSError:
                    pass
        return None
    done = None
    for step in (lambda: os.write(fd, b"q"), lambda: os.write(fd, b"y"),
                 lambda: os.kill(pid, signal.SIGTERM), lambda: os.kill(pid, signal.SIGKILL)):
        try:
            step()
        except (OSError, ProcessLookupError):
            pass
        done = reap(2.0)
        if done:
            break
    if done is None:
        _, status, ru = os.wait4(pid, 0)
    else:
        status, ru = done
    maxrss = ru.ru_maxrss if sys.platform == "darwin" else ru.ru_maxrss * 1024

    repaints = out.count(b"\x1b[1;1H") + out.count(b"\x1b[H")
    first = out.find(b"\x1b[2J")
    second = out.find(b"\x1b[1;1H", first + 1) if first >= 0 else -1
    frame1 = out[first:second] if first >= 0 and second > first else bytes(out[:4096])
    result = {
        "cpu_seconds": round(ru.ru_utime + ru.ru_stime, 4),
        "max_rss_bytes": maxrss,
        "bytes_written": len(out),
        "repaints": repaints,
        "seconds": a.seconds,
        "frame1_sha256": hashlib.sha256(normalise(bytes(frame1))).hexdigest(),
        "exit_status": status,
    }
    print(json.dumps(result, indent=2))
    if a.json:
        with open(a.json, "w") as f:
            json.dump(result, f, indent=2)
    if a.stream:
        with open(a.stream, "wb") as f:
            f.write(out)


if __name__ == "__main__":
    main()

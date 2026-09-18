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
    first_frame_ms  fork to the first byte of output (the first frame drawn)
    ready_ms        fork to the moment --ready-text first appears in the stream
                    (a run id prefix: the list has rows)
    key_latency_ms  p50/p99/max from each key write to the next repaint that
                    carries real content (over 64 bytes, not just cursor toggles)
    burst_settle_ms after --burst N wheel-down and N mouse-motion events are
                    queued at once, how long until the last repaint the burst
                    caused (activity ends at the first 400ms quiet gap)

    --bin PATH --seconds N --cols C --rows R --keys 'jjj' --burst 300
    --ready-text genesis --json OUT --stream OUT.bin
"""
import argparse
import faulthandler
import fcntl
import hashlib
import json
import os
import pty
import re
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


def ms(t, since):
    return None if t is None else round((t - since) * 1000, 1)


def latency(key_sent):
    lat = sorted((done - sent) * 1000 for sent, done in key_sent if done is not None)
    if not lat:
        return None
    pick = lambda q: round(lat[min(len(lat) - 1, int(q * len(lat)))], 1)
    return {"p50": pick(0.5), "p99": pick(0.99), "max": round(lat[-1], 1), "n": len(lat)}


def settle(chunks, burst_at):
    """The last content repaint before the first 400ms quiet gap after the burst."""
    if burst_at is None:
        return None
    last = burst_at
    for t in chunks:
        if t - last > 0.4:
            break
        last = t
    return round((last - burst_at) * 1000, 1)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.environ.get("LV_BIN", "lev"))
    ap.add_argument("--seconds", type=float, default=30)
    ap.add_argument("--cols", type=int, default=200)
    ap.add_argument("--rows", type=int, default=50)
    ap.add_argument("--keys", default="", help="keys to send after 2s, one per 300ms; \\e for Escape")
    ap.add_argument("--burst", type=int, default=0,
                    help="after the keys, queue N wheel-down + N mouse-motion events at once")
    ap.add_argument("--ready-text", default="",
                    help="text that appears once the run list has rows")
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
    ready = a.ready_text.encode()
    out = bytearray()
    start = time.monotonic()
    deadline = start + a.seconds
    next_key = start + 2.0
    key_i = 0
    first_output = None
    ready_at = None
    key_sent = []  # (time sent, time of the first content repaint after it)
    burst_at = None
    burst_chunks = []
    pending = bytearray()  # input not yet accepted by the pty
    os.set_blocking(fd, False)
    while time.monotonic() < deadline:
        r, w, _ = select.select([fd], [fd] if pending else [], [], 0.005)
        now = time.monotonic()
        if r:
            try:
                chunk = os.read(fd, 1 << 16)
            except BlockingIOError:
                # select() said readable but the data went elsewhere first;
                # the next pass reads it.
                chunk = None
            except OSError:
                break
            if chunk == b"":
                break
            if chunk:
                out.extend(chunk)
                # Answer the terminal queries a real emulator answers. crossterm
                # asks for keyboard-enhancement flags and primary device
                # attributes on startup and waits up to 2s for a reply; a
                # silent pty would add that wait to every number below.
                if b"\x1b[c" in chunk:
                    pending.extend(b"\x1b[?62;22c")
                if first_output is None:
                    first_output = now
                if ready and ready_at is None and ready in out[-(len(chunk) + len(ready)):]:
                    ready_at = now
                if len(chunk) > 64:
                    if key_sent and key_sent[-1][1] is None:
                        key_sent[-1] = (key_sent[-1][0], now)
                    if burst_at is not None:
                        burst_chunks.append(now)
        if w and pending:
            try:
                n = os.write(fd, pending)
            except BlockingIOError:
                # The pty's input buffer is full; the rest goes on a later pass.
                n = 0
            del pending[:n]
        if key_i < len(keys) and now >= next_key:
            pending.extend(keys[key_i:key_i + 1])
            key_sent.append((now, None))
            key_i += 1
            next_key = now + 0.3
        if a.burst and burst_at is None and key_i >= len(keys) and now >= next_key + 0.5:
            seq = bytearray()
            for i in range(a.burst):
                seq += b"\x1b[<65;20;10M"
                seq += b"\x1b[<35;%d;%dM" % (10 + i % 150, 5 + i % 30)
            pending.extend(seq)
            burst_at = now
    os.set_blocking(fd, True)
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
                    # The pty's slave side is gone once the child exits; the
                    # next wait4 collects it, so there is nothing to do here.
                    pass
        return None
    done = None
    for step in (lambda: os.write(fd, b"q"), lambda: os.write(fd, b"y"),
                 lambda: os.kill(pid, signal.SIGTERM), lambda: os.kill(pid, signal.SIGKILL)):
        try:
            step()
        except (OSError, ProcessLookupError):
            # The child already left (closed pty, or no such pid): the reap
            # below collects it, and a failed step is not an error.
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
        "first_frame_ms": ms(first_output, start),
        "ready_ms": ms(ready_at, start),
        "key_latency_ms": latency(key_sent),
        "burst_settle_ms": settle(burst_chunks, burst_at),
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

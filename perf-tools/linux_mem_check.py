#!/usr/bin/env python3
"""Linux ground-truth check for the monitor's memory model.

Runs inside a Linux container. A child process allocates ~300MB, then frees
it with MADV_FREE (via a ctypes madvise on an anonymous mmap), and we read
its smaps_rollup at each phase to verify:
  1. Rss/Pss include the lazily-freed pages after MADV_FREE (the ratchet).
  2. LazyFree reports them, so pss - LazyFree tracks the truth.
This is the experiment behind the monitor's live_mb definition on Linux.
"""
import ctypes
import os
import sys
import time

MB = 1024 * 1024
SIZE = 300 * MB
MADV_FREE = 8


def rollup(pid):
    fields = {}
    with open(f"/proc/{pid}/smaps_rollup") as f:
        for line in f:
            parts = line.split()
            if len(parts) >= 2 and parts[0].endswith(":") and parts[1].isdigit():
                fields[parts[0].rstrip(":")] = int(parts[1])
    return fields


def report(pid, label):
    f = rollup(pid)
    rss, pss, lazy = f.get("Rss", 0), f.get("Pss", 0), f.get("LazyFree", 0)
    print(
        f"{label:22} rss {rss/1024:8.1f} MB  pss {pss/1024:8.1f} MB  "
        f"lazyfree {lazy/1024:8.1f} MB  live(pss-lazy) {(pss-lazy)/1024:8.1f} MB",
        flush=True,
    )
    return f


def main():
    pid = os.getpid()
    libc = ctypes.CDLL(None, use_errno=True)
    report(pid, "baseline")

    # Raw anonymous private mapping via libc, so the address and flags are
    # exactly what MADV_FREE requires.
    PROT_READ, PROT_WRITE = 1, 2
    MAP_PRIVATE, MAP_ANONYMOUS = 2, 0x20
    libc.mmap.restype = ctypes.c_void_p
    libc.mmap.argtypes = [
        ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int,
        ctypes.c_int, ctypes.c_int, ctypes.c_long,
    ]
    addr = libc.mmap(None, SIZE, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)
    if addr in (None, ctypes.c_void_p(-1).value):
        print("mmap failed", flush=True)
        sys.exit(2)
    buf = (ctypes.c_char * SIZE).from_address(addr)
    for off in range(0, SIZE, 4096):
        buf[off] = b"\x01"
    report(pid, "after allocate+touch")
    libc.madvise.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
    ret = libc.madvise(ctypes.c_void_p(addr), ctypes.c_size_t(SIZE), MADV_FREE)
    if ret != 0:
        err = ctypes.get_errno()
        print(f"madvise(MADV_FREE) failed: errno {err}", flush=True)
        sys.exit(2)
    time.sleep(0.5)
    after = report(pid, "after MADV_FREE")

    rss_still_counts = after.get("Rss", 0) > 200 * 1024
    lazy_reports = after.get("LazyFree", 0) > 200 * 1024
    live = (after.get("Pss", 0) - after.get("LazyFree", 0)) / 1024
    print(f"\nrss still counts freed pages: {rss_still_counts}")
    print(f"LazyFree reports them:        {lazy_reports}")
    print(f"live estimate after free:     {live:.1f} MB")
    ok = rss_still_counts and lazy_reports and live < 100
    print("VERDICT:", "monitor model CONFIRMED" if ok else "monitor model WRONG")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()

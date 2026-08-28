#!/usr/bin/env python3
"""Latency percentiles for `lev serve` read routes over a fixed runs corpus.

    serve_latency.py --port 8299 --n 200 [--json OUT]

Hits each route N times sequentially (one connection per request, like a
console tab would) and prints p50/p99 in milliseconds. Run against a server
started with the same corpus every time (`LV_RUNS_DIR=... harness.sh lev serve
--port 8299`), or the numbers are not comparable.
"""
import argparse
import json
import os
import statistics
import time
import urllib.request

ROUTES = ["/api/runs?limit=50", "/api/agents/tree", "/api/agents"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=int(os.environ.get("LV_SERVE_PORT", "8299")))
    ap.add_argument("--n", type=int, default=200)
    ap.add_argument("--json")
    a = ap.parse_args()
    token = os.environ["LEVIATH_API_TOKEN"]
    out = {}
    for route in ROUTES:
        samples = []
        size = 0
        for _ in range(a.n):
            req = urllib.request.Request(f"http://127.0.0.1:{a.port}{route}",
                                         headers={"authorization": f"Bearer {token}"})
            t = time.perf_counter()
            with urllib.request.urlopen(req, timeout=60) as r:
                size = len(r.read())
            samples.append((time.perf_counter() - t) * 1000)
        samples.sort()
        out[route] = {
            "p50_ms": round(statistics.median(samples), 2),
            "p99_ms": round(samples[int(len(samples) * 0.99) - 1], 2),
            "max_ms": round(samples[-1], 2),
            "body_bytes": size,
        }
    print(json.dumps(out, indent=2))
    if a.json:
        with open(a.json, "w") as f:
            json.dump(out, f, indent=2)


if __name__ == "__main__":
    main()

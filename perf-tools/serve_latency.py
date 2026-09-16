#!/usr/bin/env python3
"""Latency percentiles for `lev serve` read routes over a fixed runs corpus.

    serve_latency.py --port 8299 --n 200 [--json OUT] [--burst] [--accept-encoding gzip]

Two measurements:

- Per route (the default): hits each route N times sequentially, one connection
  per request, like a console tab would, and prints p50/p99 in milliseconds
  plus the body size as the client received it.
- `--burst`: fires the console's connect-time set (config, first runs page,
  blueprints, models, folder listing, update plan) all at once, N times, and
  reports each route's time inside the burst plus the wall clock to the last
  byte of the slowest one. That number is what a user waits for when The Lair
  opens.

Run against a server started with the same corpus every time
(`LV_RUNS_DIR=... harness.sh lev serve --port 8299`), or the numbers are not
comparable. `--accept-encoding` records what the server sends back when the
client says it can take compressed bodies; leave it off to measure raw bytes.
"""
import argparse
import concurrent.futures
import json
import os
import statistics
import time
import urllib.error
import urllib.request

ROUTES = [
    "/api/runs?limit=50",
    "/api/agents/tree",
    "/api/agents",
    "/api/config",
    "/api/blueprints",
    "/api/models",
    "/api/agents/{id}/children",
]

# The requests the console makes the moment it connects. The runs row matches
# the console's `RUN_ROW_FIELDS`; the blueprint page size is the server's cap.
ROW_FIELDS = "run_id,agent_name,task,title,status,started_at,parent_run_id,waiting_on,children"
CONNECT_BURST = [
    "/api/config",
    "/api/runs?sort=started_at&limit=50&parent=none&fields=" + ROW_FIELDS,
    "/api/blueprints?limit=200",
    "/api/models",
    "/api/fs/dirs",
    "/api/update",
]

RECORDED_HEADERS = ["content-encoding", "x-leviath-catalog-age", "x-leviath-catalog-complete"]


def fetch(base, token, route, accept_encoding):
    """One request; returns (elapsed ms, body bytes, headers of interest)."""
    headers = {"authorization": f"Bearer {token}"}
    if accept_encoding:
        headers["accept-encoding"] = accept_encoding
    req = urllib.request.Request(base + route, headers=headers)
    t = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            size = len(r.read())
            seen = {h: r.headers.get(h) for h in RECORDED_HEADERS if r.headers.get(h)}
    except urllib.error.HTTPError as e:
        size = len(e.read())
        seen = {"status": e.code}
    return (time.perf_counter() - t) * 1000, size, seen


def percentiles(samples):
    samples = sorted(samples)
    return {
        "p50_ms": round(statistics.median(samples), 2),
        "p99_ms": round(samples[max(int(len(samples) * 0.99) - 1, 0)], 2),
        "max_ms": round(samples[-1], 2),
    }


def first_run_id(base, token):
    req = urllib.request.Request(base + "/api/runs?limit=1",
                                 headers={"authorization": f"Bearer {token}"})
    with urllib.request.urlopen(req, timeout=60) as r:
        page = json.load(r)
    items = page["items"] if isinstance(page, dict) else page
    return items[0]["meta"]["run_id"] if items else None


def per_route(base, token, n, accept_encoding):
    run_id = first_run_id(base, token)
    out = {}
    for route in ROUTES:
        if "{id}" in route:
            if run_id is None:
                continue
            route = route.replace("{id}", run_id)
        samples = []
        size = 0
        seen = {}
        for _ in range(n):
            ms, size, seen = fetch(base, token, route, accept_encoding)
            samples.append(ms)
        out[route] = {**percentiles(samples), "body_bytes": size, **seen}
    return out


def burst(base, token, n, accept_encoding):
    per = {route: [] for route in CONNECT_BURST}
    walls = []
    sizes = {}
    seen = {}
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(CONNECT_BURST)) as pool:
        for _ in range(n):
            t = time.perf_counter()
            futures = {route: pool.submit(fetch, base, token, route, accept_encoding)
                       for route in CONNECT_BURST}
            for route, fut in futures.items():
                ms, size, headers = fut.result()
                per[route].append(ms)
                sizes[route] = size
                if headers:
                    seen[route] = headers
            walls.append((time.perf_counter() - t) * 1000)
    out = {"wall_to_last_byte": percentiles(walls), "routes": {}}
    for route, samples in per.items():
        out["routes"][route] = {**percentiles(samples), "body_bytes": sizes[route], **seen.get(route, {})}
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=int(os.environ.get("LV_SERVE_PORT", "8299")))
    ap.add_argument("--n", type=int, default=200)
    ap.add_argument("--json")
    ap.add_argument("--burst", action="store_true", help="fire the console's connect set at once")
    ap.add_argument("--accept-encoding", default=None,
                    help="send this Accept-Encoding (gzip, br, identity); off by default")
    a = ap.parse_args()
    token = os.environ["LEVIATH_API_TOKEN"]
    base = f"http://127.0.0.1:{a.port}"
    if a.burst:
        out = burst(base, token, a.n, a.accept_encoding)
    else:
        out = per_route(base, token, a.n, a.accept_encoding)
    print(json.dumps(out, indent=2))
    if a.json:
        with open(a.json, "w") as f:
            json.dump(out, f, indent=2)


if __name__ == "__main__":
    main()

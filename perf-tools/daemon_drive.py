#!/usr/bin/env python3
"""Drive a real daemon through K runs against mock.py and report the cost.

Runs inside the environment `harness.sh` sets up (call it through the wrapper:
`harness.sh python3 perf-tools/daemon_drive.py ...`). Starts the mock and the
daemon itself, spawns the runs over `lev serve`, waits for every run to reach a
terminal status, then prints one JSON object with wall clock and the daemon's
rusage. Nothing here inherits the caller's environment beyond what the wrapper
passed.

    --runs K          how many runs to spawn (default 8)
    --tool NAME       ask the mock to request this tool on the first turn
    --args JSON       the tool's arguments
    --yolo            spawn with yolo (tool calls need no approval)
    --json PATH       write the measurement here as well as printing it
"""
import argparse
import json
import os
import subprocess
import sys
import time
import urllib.request

LV_BIN = os.environ.get("LV_BIN", "lev")
MOCK_PORT = int(os.environ.get("LV_MOCK_PORT", "8099"))
SERVE_PORT = int(os.environ.get("LV_SERVE_PORT", "8199"))
TOKEN = os.environ["LEVIATH_API_TOKEN"]
HERE = os.path.dirname(os.path.abspath(__file__))


def api(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(f"http://127.0.0.1:{SERVE_PORT}{path}", data=data, method=method,
                                 headers={"authorization": f"Bearer {TOKEN}",
                                          "content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.loads(r.read() or b"null")


def wait_http(url, seconds=20):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            urllib.request.urlopen(url, timeout=1).read()
            return
        except Exception:
            time.sleep(0.1)
    raise SystemExit(f"nothing answered at {url}")


def daemon_cpu_seconds():
    """The daemon's accumulated CPU time, from `ps`. It is a detached process,
    so `getrusage(RUSAGE_CHILDREN)` never sees it."""
    pid_file = os.path.join(os.environ["LEVIATH_HOME"], ".leviath", "daemon.pid")
    try:
        pid = open(pid_file).read().strip().split()[0]
        txt = subprocess.run(["ps", "-o", "cputime=", "-p", pid], capture_output=True, text=True).stdout.strip()
    except (OSError, IndexError):
        return None
    # mm:ss.cc or hh:mm:ss
    parts = txt.split(":")
    if not txt:
        return None
    secs = 0.0
    for part in parts:
        secs = secs * 60 + float(part)
    return secs


def spawn(cmd):
    # A new session so the shell that started us cannot SIGKILL the group.
    return subprocess.Popen(cmd, start_new_session=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", type=int, default=8)
    ap.add_argument("--tool")
    ap.add_argument("--args", default="{}")
    ap.add_argument("--yolo", action="store_true")
    ap.add_argument("--json")
    ap.add_argument("--keep", action="store_true", help="leave the daemon and mock running")
    a = ap.parse_args()

    mock_cmd = [sys.executable, os.path.join(HERE, "mock.py"), str(MOCK_PORT)]
    if a.tool:
        mock_cmd += [a.tool, a.args]
    mock = spawn(mock_cmd)
    wait_http(f"http://127.0.0.1:{MOCK_PORT}/v1/models")

    subprocess.run([LV_BIN, "daemon", "stop"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.run([LV_BIN, "daemon", "start"], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    serve = spawn([LV_BIN, "serve", "--port", str(SERVE_PORT), "--host", "127.0.0.1"])
    wait_http(f"http://127.0.0.1:{SERVE_PORT}/")

    workdir = os.path.join(os.environ["LEVIATH_HOME"], "work")
    cpu_before = daemon_cpu_seconds()
    started = time.monotonic()
    ids = []
    for i in range(a.runs):
        body = {"blueprint": "probe", "task": f"probe run {i}", "workdir": workdir, "yolo": a.yolo}
        ids.append(api("POST", "/api/agents", body)["run_id"])
    terminal = {"complete", "complete_interactive", "error", "cancelled"}
    pending = set(ids)
    deadline = time.monotonic() + 120
    while pending and time.monotonic() < deadline:
        for rid in list(pending):
            st = api("GET", f"/api/agents/{rid}").get("status", "")
            if str(st).lower() in terminal:
                pending.discard(rid)
        time.sleep(0.2)
    wall = time.monotonic() - started
    cpu_after = daemon_cpu_seconds()

    calls = json.loads(urllib.request.urlopen(f"http://127.0.0.1:{MOCK_PORT}/count").read())["count"]
    out = {
        "runs": a.runs,
        "finished": a.runs - len(pending),
        "wall_seconds": round(wall, 3),
        "mock_completions": calls,
        "daemon_cpu_seconds": None if cpu_before is None or cpu_after is None else round(cpu_after - cpu_before, 2),
        "run_ids": ids,
    }
    print(json.dumps(out, indent=2))
    if a.json:
        with open(a.json, "w") as f:
            json.dump(out, f, indent=2)
    if not a.keep:
        serve.terminate()
        subprocess.run([LV_BIN, "daemon", "stop"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        mock.terminate()
    if pending:
        sys.exit(1)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Materialise N run directories by copying one real run.

    fake_runs.py --from RUNS_DIR/<run_id> --count 750 --out /tmp/lv-bench/runs

The template is a run the daemon actually wrote (produce one with
`daemon_drive.py --keep`), so every file has the real on-disk schema and a
realistic size. A generator that wrote 200-byte `context.json` stubs would hide
exactly the per-frame parsing cost the dashboard measurements exist to catch.

Only `run_id` (in `meta.json` and the directory name) and the timestamps are
rewritten; everything else is byte-identical to the template.
"""
import argparse
import json
import os
import shutil
import time


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--from", dest="src", required=True, help="a real run directory")
    ap.add_argument("--count", type=int, default=750)
    ap.add_argument("--out", required=True, help="the runs directory to fill")
    a = ap.parse_args()

    meta_path = os.path.join(a.src, "meta.json")
    with open(meta_path) as f:
        meta = json.load(f)
    base = os.path.basename(a.src.rstrip("/"))
    prefix = base.rsplit("-", 1)[0] if "-" in base else base
    now = int(time.time())
    os.makedirs(a.out, exist_ok=True)
    for i in range(a.count):
        rid = f"{prefix}-{i:06x}"
        dst = os.path.join(a.out, rid)
        if os.path.exists(dst):
            shutil.rmtree(dst)
        shutil.copytree(a.src, dst)
        m = dict(meta)
        m["run_id"] = rid
        for key in ("started_at", "updated_at", "finished_at", "completed_at"):
            if isinstance(m.get(key), int):
                m[key] = now - (a.count - i) * 60
        with open(os.path.join(dst, "meta.json"), "w") as f:
            json.dump(m, f)
    print(f"wrote {a.count} runs under {a.out} from {a.src}")


if __name__ == "__main__":
    main()

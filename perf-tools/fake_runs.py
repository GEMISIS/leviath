#!/usr/bin/env python3
"""Materialise N run directories by copying one real run.

    fake_runs.py --from RUNS_DIR/<run_id> --count 750 --out /tmp/lv-bench/runs

The template is a run the daemon actually wrote (produce one with
`daemon_drive.py --keep`), so every file has the real on-disk schema and a
realistic size. A generator that wrote 200-byte `context.json` stubs would hide
exactly the per-frame parsing cost the dashboard measurements exist to catch.

Only `run_id` (in `meta.json` and the directory name) and the timestamps are
rewritten; everything else is byte-identical to the template.

`--fanout K` shapes the corpus like fan-out work: every run whose index is a
multiple of K+1 becomes a parent and the K runs after it its children
(`parent_run_id`, `children` and `depth` rewritten to match).
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import time


def copy_run(src, dst):
    """A copy-on-write clone where the filesystem has one (APFS), else a copy.

    The bytes are identical either way; a clone just keeps a 5,000-run corpus
    of a 7 MB template from costing 35 GB of disk."""
    if sys.platform == "darwin" and subprocess.run(["cp", "-c", "-R", src, dst]).returncode == 0:
        return
    shutil.copytree(src, dst)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--from", dest="src", required=True, help="a real run directory")
    ap.add_argument("--count", type=int, default=750)
    ap.add_argument("--out", required=True, help="the runs directory to fill")
    ap.add_argument("--fanout", type=int, default=0,
                    help="every (K+1)th run is a parent of the K runs after it")
    a = ap.parse_args()

    meta_path = os.path.join(a.src, "meta.json")
    with open(meta_path) as f:
        meta = json.load(f)
    base = os.path.basename(a.src.rstrip("/"))
    prefix = base.rsplit("-", 1)[0] if "-" in base else base
    now = int(time.time())
    ids = [f"{prefix}-{i:06x}" for i in range(a.count)]
    group = a.fanout + 1
    os.makedirs(a.out, exist_ok=True)
    for i in range(a.count):
        rid = ids[i]
        dst = os.path.join(a.out, rid)
        if os.path.exists(dst):
            shutil.rmtree(dst)
        copy_run(a.src, dst)
        m = dict(meta)
        m["run_id"] = rid
        if a.fanout:
            if i % group == 0:
                m["parent_run_id"] = None
                m["depth"] = 0
                m["children"] = ids[i + 1:min(i + group, a.count)]
            else:
                m["parent_run_id"] = ids[i - i % group]
                m["depth"] = 1
                m["children"] = []
        for key in ("started_at", "updated_at", "finished_at", "completed_at"):
            if isinstance(m.get(key), int):
                m[key] = now - (a.count - i) * 60
        with open(os.path.join(dst, "meta.json"), "w") as f:
            json.dump(m, f)
    print(f"wrote {a.count} runs under {a.out} from {a.src}")


if __name__ == "__main__":
    main()

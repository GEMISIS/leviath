#!/usr/bin/env python3
"""Watch the ``leviath`` daemon and graph sessions, CPU, and memory over time.

Start this script, go do whatever you want with leviath, then press Ctrl-C to
stop it. On stop it writes a timestamped CSV of every sample it took plus a
timestamped PNG graph with three aligned panels:

1. Active sessions - runs whose ``meta.json`` status is non-terminal
   (``starting``, ``running``, ``waiting_input``, ``paused``), sampled from the
   runs directory the daemon persists to.
2. CPU percent of the WHOLE machine: psutil's per-core figure (the
   ``top``/``htop`` convention, where each core is worth 100%) divided by
   the logical core count, so the axis runs 0-100 and reads as "share of
   this machine's total compute". Cross-machine comparisons must note the
   core count, since 10% of a 16-core machine is 1.6 cores of work.
3. Memory - ``rss`` and ``live`` lines, plus ``pss`` where the OS provides it.

Memory metrics, precisely (see also perf-tools/README.md):

- ``rss_mb``: resident set size, what ``ps``/``top`` show. On BOTH macOS and
  Linux this includes pages the allocator has already given back lazily
  (``MADV_FREE``/``MADV_FREE_REUSABLE``): the kernel only reclaims them under
  memory pressure, so RSS ratchets up and stays there even when the process
  holds nothing. RSS alone overstates a busy-then-idle process on every Unix.
- ``pss_mb`` (Linux only): proportional set size from ``smaps_rollup`` -
  shared pages divided by their sharer count. Still includes lazily-freed
  pages, so it has the same ratchet as RSS.
- ``uss_mb``: unique (private) pages. Linux: ``Private_Clean+Private_Dirty``
  from ``smaps_rollup``; Windows: psutil's USS. Same lazy-free caveat on Linux.
- ``lazy_free_mb``: pages the process freed lazily that the kernel has not
  reclaimed yet. Linux: the ``LazyFree`` field of ``smaps_rollup``
  (``MADV_FREE``). macOS: the ``Reclaimable`` column of ``/usr/bin/footprint``
  (``MADV_FREE_REUSABLE``) - measured on a settled post-burst daemon, the
  physical footprint still counts these pages (50 MB dirty of which 48 MB
  reclaimable) until the kernel repossesses them under pressure. This is the
  correction term that turns the ratcheting counters into a live figure.
- ``live_mb``: the headline series - the memory the process actually holds:
  macOS: physical footprint minus its reclaimable portion, floored at 0.
  Linux: ``pss - LazyFree``, floored at 0. Windows: USS (Windows has no
  lazy-free equivalent; ``VirtualFree`` decommits immediately, so no
  correction is needed).

Session counts come from two sources. While running, each sample counts runs
whose ``meta.json`` status is active - accurate for long runs but blind to
runs shorter than the sample interval. On stop, the monitor also reconstructs
the exact concurrency curve from each run directory's creation time and its
``meta.json``'s last-write time, which have sub-second precision even for
runs the sampler never saw; the graph shows both, and the reconstruction is
written next to the CSV as ``*_runs.csv``.

Every panel title carries the metric's average and peak, and a dashed line
marks the average. Because both output names carry a ``YYYYmmdd_HHMMSS``
stamp, repeated runs never clobber each other.

Requires ``psutil`` and ``matplotlib``.

Usage:
    python3 leviath_monitor.py
    python3 leviath_monitor.py -i 0.5
    python3 leviath_monitor.py -n leviath-server -o ./monitor-runs
    python3 leviath_monitor.py --max-samples 60
    python3 leviath_monitor.py --runs-dir /tmp/lev-home/.leviath/runs

Exit codes:
    0 - clean stop (Ctrl-C, sample cap reached, or watched process exited);
        whatever samples were collected have been written out
    1 - bad arguments, or no matching process was found
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import re
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path
from typing import (
    Callable,
    Dict,
    List,
    NamedTuple,
    Optional,
    Sequence,
    Tuple,
)

import psutil

import matplotlib

# Select the non-interactive backend before pyplot is imported: this script is
# routinely run over SSH / in CI, where trying to open a window would fail.
matplotlib.use("Agg")

import matplotlib.pyplot as plt  # noqa: E402  (must follow matplotlib.use)

__all__ = [
    "Sample",
    "MemorySample",
    "DEFAULT_PROCESS_NAME",
    "DEFAULT_INTERVAL",
    "CSV_HEADER",
    "ACTIVE_STATUSES",
    "find_leviath_process",
    "sample_memory",
    "default_runs_dir",
    "ActiveRunCounter",
    "RunInterval",
    "collect_run_intervals",
    "concurrency_steps",
    "write_runs_csv",
    "sample_process",
    "build_output_paths",
    "init_csv",
    "append_csv_row",
    "generate_graph",
    "watch_loop",
    "build_arg_parser",
    "main",
]

#: Substring matched (case-insensitively) against process names and cmdlines.
DEFAULT_PROCESS_NAME = "leviath"

#: Seconds between samples.
DEFAULT_INTERVAL = 1.0

#: Column order of the emitted CSV. Cells whose metric the OS cannot provide
#: are left empty rather than approximated.
CSV_HEADER = (
    "elapsed_seconds",
    "timestamp",
    "cpu_percent",
    "rss_mb",
    "pss_mb",
    "uss_mb",
    "lazy_free_mb",
    "live_mb",
    "active_runs",
)

#: ``meta.json`` statuses that count as a live session. Mirrors the
#: non-terminal variants of ``RunStatus`` in ``crates/leviath-core``
#: (``snake_case`` on disk).
ACTIVE_STATUSES = frozenset({"starting", "running", "waiting_input", "paused"})

#: Bytes per megabyte, used to convert psutil's RSS figure.
_BYTES_PER_MB = 1024 * 1024

#: Matches the value ``top -stats mem`` prints on macOS, e.g. ``22M``,
#: ``1536K+``, ``1.2G-``. The trailing +/- is top's delta marker.
_TOP_MEM_RE = re.compile(r"^(\d+(?:\.\d+)?)([BKMG])[+-]?$")

#: Matches the summary line of ``/usr/bin/footprint``, e.g.
#: ``lev [6994]: 64-bit    Footprint: 22 MB (16384 bytes per page)``.
_FOOTPRINT_RE = re.compile(r"Footprint:\s+(\d+(?:\.\d+)?)\s*([BKMG])B?\b")

#: Matches the TOTAL row of ``/usr/bin/footprint``'s category table, whose
#: first three sized columns are Dirty, Clean, and Reclaimable, e.g.
#: ``50 MB    9360 KB        48 MB       2789    TOTAL``.
_FOOTPRINT_TOTAL_RE = re.compile(
    r"^\s*(\d+(?:\.\d+)?)\s*([BKMG])B?\s+"
    r"(\d+(?:\.\d+)?)\s*([BKMG])B?\s+"
    r"(\d+(?:\.\d+)?)\s*([BKMG])B?\s+\d+\s+TOTAL\b"
)


class MemorySample(NamedTuple):
    """Every memory figure one poll of the watched process yields.

    ``None`` means the running OS cannot provide that metric (the CSV cell is
    left empty); it is never approximated from another column.
    """

    rss_mb: float
    pss_mb: Optional[float]
    uss_mb: Optional[float]
    lazy_free_mb: Optional[float]
    live_mb: Optional[float]


class Sample(NamedTuple):
    """One point-in-time measurement of the watched process."""

    elapsed: float
    timestamp: str
    cpu_percent: float
    memory: MemorySample
    active_runs: Optional[int]


def find_leviath_process(
    pattern: str = DEFAULT_PROCESS_NAME,
) -> Optional[psutil.Process]:
    """Return the best running process matching *pattern*, or ``None``.

    The match is a case-insensitive substring test against both the process
    name and its full command line. Among the matches, the daemon and serve
    processes are preferred over incidental hits: an editor whose command line
    merely contains a leviath path would otherwise win by being first in the
    process table.

    Ranking, best first:

    1. name matches and the cmdline mentions ``daemon`` or ``serve``
    2. name matches
    3. only the cmdline matches

    This monitor's own process is always skipped, as is any process running
    this script (its command line contains "leviath_monitor", which would
    otherwise match the default pattern).

    Args:
        pattern: Substring to look for.

    Returns:
        A :class:`psutil.Process` for the best match, or ``None`` if nothing
        matched.
    """
    needle = pattern.lower()
    own_pid = os.getpid()
    best: Optional[Tuple[int, int, psutil.Process]] = None
    for proc in psutil.process_iter(["pid", "name", "cmdline"]):
        try:
            info = proc.info
            if info.get("pid") == own_pid:
                continue
            name = (info.get("name") or "").lower()
            cmdline = " ".join(info.get("cmdline") or []).lower()
        except psutil.Error:
            # Process died or is not readable while we were iterating.
            continue
        if "leviath_monitor" in cmdline:
            continue
        name_hit = needle in name or name in ("lev", "lev.exe")
        cmdline_hit = needle in cmdline
        if not name_hit and not cmdline_hit:
            continue
        if name_hit and ("daemon" in cmdline or "serve" in cmdline):
            rank = 0
        elif name_hit:
            rank = 1
        else:
            rank = 2
        key = (rank, info.get("pid") or 0)
        if best is None or key < (best[0], best[1]):
            best = (rank, info.get("pid") or 0, proc)
    return best[2] if best else None


_UNIT_TO_MB = {"B": 1 / _BYTES_PER_MB, "K": 1 / 1024, "M": 1.0, "G": 1024.0}


def _footprint_darwin(pid: int) -> Tuple[Optional[float], Optional[float]]:
    """macOS: ``(physical footprint, reclaimable portion)`` in MB.

    Physical footprint is the figure ``vmmap``, Activity Monitor, and the
    kernel's own memory accounting report, and unlike psutil's
    ``memory_full_info`` it needs no elevated privileges. It excludes the
    empty allocator regions RSS keeps counting - measured on an idle daemon:
    292.8 MB RSS over a 21.7 MB footprint - but it still counts pages the
    allocator returned via ``MADV_FREE_REUSABLE``: they stay in the footprint,
    flagged in the ``Reclaimable`` column of the category table, until the
    kernel repossesses them under pressure. The reclaimable figure is the
    macOS twin of Linux's ``LazyFree``, so callers subtract it for ``live``.

    Beware attribution when reading that table yourself: mimalloc tags its
    arenas with VM tag 100, which Apple's tools label ``IOAccelerator`` as if
    it were GPU memory. Retagging via ``MIMALLOC_OS_TAG=240`` relabels the
    same regions ``app-specific tag 1``, proving they are heap pages.

    ``/usr/bin/footprint`` answers in ~30ms; ``top`` is the fallback because
    it is always present but costs over a second of system time per call (it
    scans the whole process table even when pinned to one pid). ``top`` knows
    nothing about reclaimable pages, so the fallback reports ``None`` for it.
    """
    try:
        out = subprocess.run(
            ["/usr/bin/footprint", "-p", str(pid)],
            capture_output=True,
            text=True,
            timeout=10,
        ).stdout
        match = _FOOTPRINT_RE.search(out)
        if match:
            footprint = float(match.group(1)) * _UNIT_TO_MB[match.group(2)]
            reclaimable = None
            for line in out.splitlines():
                total = _FOOTPRINT_TOTAL_RE.match(line)
                if total:
                    reclaimable = (
                        float(total.group(5)) * _UNIT_TO_MB[total.group(6)]
                    )
                    break
            return footprint, reclaimable
    except (OSError, subprocess.SubprocessError):
        pass
    try:
        out = subprocess.run(
            ["top", "-l", "1", "-s", "0", "-pid", str(pid), "-stats", "mem"],
            capture_output=True,
            text=True,
            timeout=10,
        ).stdout
    except (OSError, subprocess.SubprocessError):
        return None, None
    for line in reversed(out.splitlines()):
        token = line.strip().split()[0] if line.strip() else ""
        match = _TOP_MEM_RE.match(token)
        if match:
            return float(match.group(1)) * _UNIT_TO_MB[match.group(2)], None
    return None, None


def _smaps_rollup_kb(pid: int) -> Dict[str, int]:
    """Linux: the ``smaps_rollup`` fields in kB, or empty on any failure.

    Unprivileged for the user's own processes.
    """
    try:
        text = Path(f"/proc/{pid}/smaps_rollup").read_text()
    except OSError:
        return {}
    fields: Dict[str, int] = {}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[0].endswith(":") and parts[1].isdigit():
            fields[parts[0].rstrip(":")] = int(parts[1])
    return fields


def _memory_linux(proc: psutil.Process) -> MemorySample:
    """Linux memory sample from ``smaps_rollup``.

    ``live = pss - LazyFree``: on Linux, RSS **and** PSS keep counting pages
    the process already freed via ``MADV_FREE`` until the kernel reclaims them
    under pressure - the same ratchet macOS RSS has. ``LazyFree`` is the
    kernel's own count of those pages, so subtracting it is the accurate
    correction, not an estimate. (Lazily-freed pages are private to the freeing
    process, so the subtraction is not distorted by PSS's sharing division.)
    """
    rss = proc.memory_info().rss / _BYTES_PER_MB
    fields = _smaps_rollup_kb(proc.pid)
    if not fields:
        return MemorySample(rss, None, None, None, None)
    pss = fields.get("Pss")
    lazy = fields.get("LazyFree", 0)
    uss = None
    if "Private_Clean" in fields or "Private_Dirty" in fields:
        uss = (fields.get("Private_Clean", 0) + fields.get("Private_Dirty", 0)) / 1024
    live = None
    if pss is not None:
        live = max(pss - lazy, 0) / 1024
        pss = pss / 1024
    return MemorySample(rss, pss, uss, lazy / 1024, live)


def _memory_windows_or_fallback(proc: psutil.Process) -> MemorySample:
    """USS via psutil - unprivileged on Windows for the user's own processes.

    Windows needs no lazy-free correction: freed heap is decommitted (it
    leaves the working set and the commit charge immediately), so USS is
    already the live figure.
    """
    rss = proc.memory_info().rss / _BYTES_PER_MB
    try:
        uss = proc.memory_full_info().uss / _BYTES_PER_MB
    except (psutil.Error, AttributeError):
        return MemorySample(rss, None, None, None, None)
    return MemorySample(rss, None, uss, None, uss)


def sample_memory(proc: psutil.Process) -> MemorySample:
    """Every memory metric the running OS can provide for *proc*.

    Raises ``psutil.Error`` if the process vanished (matching ``cpu_percent``),
    so the watch loop's exit path stays in one place.
    """
    if sys.platform == "darwin":
        rss = proc.memory_info().rss / _BYTES_PER_MB
        footprint, reclaimable = _footprint_darwin(proc.pid)
        live = footprint
        if footprint is not None and reclaimable is not None:
            live = max(footprint - reclaimable, 0.0)
        return MemorySample(rss, None, None, reclaimable, live)
    if sys.platform.startswith("linux"):
        return _memory_linux(proc)
    return _memory_windows_or_fallback(proc)


def default_runs_dir() -> Path:
    """Resolve the runs directory the same way ``lev`` itself does.

    ``LEVIATH_RUNS_DIR`` wins outright; otherwise ``LEVIATH_HOME`` (or the OS
    home) anchors ``.leviath/runs``. Keeping this in lockstep with
    ``runstate::runs_dir`` in ``crates/leviath-cli`` means the session counts
    follow an isolated test daemon automatically.
    """
    override = os.environ.get("LEVIATH_RUNS_DIR")
    if override:
        return Path(override)
    home = os.environ.get("LEVIATH_HOME")
    base = Path(home) if home else Path.home()
    return base / ".leviath" / "runs"


class ActiveRunCounter:
    """Count non-terminal runs by scanning ``meta.json`` files.

    Reads go to the same on-disk run state the daemon persists (its read
    model), so no auth token or API round trip is needed and the count works
    identically on every OS. Each ``meta.json`` is re-read only when its
    mtime changes, so steady-state sampling is a directory listing plus a
    handful of ``stat`` calls even with hundreds of historical runs on disk.
    """

    def __init__(self, runs_dir: Path):
        self.runs_dir = runs_dir
        self._cache: Dict[Path, Tuple[float, Optional[str]]] = {}

    def count(self) -> Optional[int]:
        """Return the number of active runs, or ``None`` if unreadable."""
        try:
            entries = list(self.runs_dir.iterdir())
        except OSError:
            return None
        active = 0
        seen = set()
        for entry in entries:
            meta_path = entry / "meta.json"
            seen.add(meta_path)
            try:
                mtime = meta_path.stat().st_mtime
            except OSError:
                self._cache.pop(meta_path, None)
                continue
            cached = self._cache.get(meta_path)
            if cached is not None and cached[0] == mtime:
                status = cached[1]
            else:
                status = self._read_status(meta_path)
                self._cache[meta_path] = (mtime, status)
            if status in ACTIVE_STATUSES:
                active += 1
        # Forget runs whose directories were deleted.
        for stale in [p for p in self._cache if p not in seen]:
            del self._cache[stale]
        return active

    @staticmethod
    def _read_status(meta_path: Path) -> Optional[str]:
        """Read ``status`` out of one ``meta.json``, or ``None`` on any error."""
        try:
            with open(meta_path, encoding="utf-8") as handle:
                meta = json.load(handle)
        except (OSError, ValueError):
            return None
        status = meta.get("status")
        return status if isinstance(status, str) else None


class RunInterval(NamedTuple):
    """One run's lifetime in wall-clock epoch seconds."""

    run_id: str
    start: float
    end: float


def collect_run_intervals(
    runs_dir: Path, wall_start: float, wall_end: float
) -> List[RunInterval]:
    """Reconstruct each run's exact lifetime from its on-disk artifacts.

    Sampling ``meta.json`` statuses undercounts whenever a run starts and
    finishes inside one sample interval - and ``meta.json``'s own
    ``started_at``/``updated_at`` are whole seconds, so a sub-second run
    collapses to a zero-length interval there too. The filesystem does better:
    the run *directory* is created once at spawn and never renamed over, so
    its birth time is the start, and ``meta.json``'s last write lands with the
    terminal status, so its mtime is the end. Both carry sub-second precision
    on APFS/NTFS/ext4.

    Runs still active (or unreadable) at *wall_end* are treated as ending
    there. On filesystems without birth times (some Linux setups) the
    directory mtime would move with every write, so the whole-second
    ``started_at`` from ``meta.json`` is the fallback start.

    Args:
        runs_dir: The runs directory the daemon persists to.
        wall_start: Epoch seconds when monitoring began; intervals that ended
            before this are dropped.
        wall_end: Epoch seconds when monitoring stopped; open intervals are
            clamped here.

    Returns:
        Overlapping-window intervals, clipped to the monitoring window and
        sorted by start time.
    """
    intervals: List[RunInterval] = []
    try:
        entries = list(runs_dir.iterdir())
    except OSError:
        return intervals
    for entry in entries:
        meta_path = entry / "meta.json"
        try:
            dir_stat = entry.stat()
            meta_stat = meta_path.stat()
            with open(meta_path, encoding="utf-8") as handle:
                meta = json.load(handle)
        except (OSError, ValueError):
            continue
        start = getattr(dir_stat, "st_birthtime", None)
        if start is None:
            started_at = meta.get("started_at")
            if not isinstance(started_at, (int, float)):
                continue
            start = float(started_at)
        status = meta.get("status")
        if isinstance(status, str) and status not in ACTIVE_STATUSES:
            end = meta_stat.st_mtime
        else:
            end = wall_end
        end = max(end, start)
        if end < wall_start or start > wall_end:
            continue
        intervals.append(
            RunInterval(entry.name, max(start, wall_start), min(end, wall_end))
        )
    intervals.sort(key=lambda item: item.start)
    return intervals


def concurrency_steps(
    intervals: Sequence[RunInterval], wall_start: float, wall_end: float
) -> Tuple[List[float], List[float]]:
    """Turn run intervals into an exact concurrency step function.

    Args:
        intervals: Output of :func:`collect_run_intervals`.
        wall_start: Epoch seconds of monitoring start; x values are elapsed
            seconds relative to this, matching the sampled series.
        wall_end: Epoch seconds of monitoring stop; the curve is closed here.

    Returns:
        ``(xs, ys)`` suitable for a ``step(where="post")`` plot; empty lists
        when there were no intervals.
    """
    if not intervals:
        return [], []
    events: List[Tuple[float, int]] = []
    for interval in intervals:
        events.append((interval.start, 1))
        events.append((interval.end, -1))
    # At identical timestamps let ends land before starts so a back-to-back
    # handoff does not read as a moment of double concurrency.
    events.sort(key=lambda event: (event[0], event[1]))
    xs: List[float] = [0.0]
    ys: List[float] = [0]
    count = 0
    for when, delta in events:
        count += delta
        elapsed = when - wall_start
        if xs[-1] == elapsed:
            ys[-1] = count
        else:
            xs.append(elapsed)
            ys.append(count)
    xs.append(wall_end - wall_start)
    ys.append(0)
    return xs, ys


def write_runs_csv(
    path: Path | str, intervals: Sequence[RunInterval], wall_start: float
) -> None:
    """Write the reconstructed intervals next to the sample CSV.

    Columns are elapsed seconds relative to monitoring start, so they line up
    with the main CSV's x-axis directly.
    """
    with open(path, "w", newline="", encoding="utf-8") as handle:
        writer = csv.writer(handle)
        writer.writerow(("run_id", "start_seconds", "end_seconds"))
        for interval in intervals:
            writer.writerow(
                (
                    interval.run_id,
                    f"{interval.start - wall_start:.3f}",
                    f"{interval.end - wall_start:.3f}",
                )
            )


def sample_process(
    proc: psutil.Process,
    start_time: float,
    run_counter: Optional[ActiveRunCounter] = None,
) -> Sample:
    """Take one measurement of *proc*.

    Args:
        proc: The process to measure.
        start_time: ``time.time()`` value from when monitoring began, used to
            compute the elapsed-seconds x-axis value.
        run_counter: Optional session counter; ``None`` records an empty
            ``active_runs`` cell.

    Returns:
        A :class:`Sample`.

    Raises:
        psutil.Error: If the process vanished or is no longer readable.
    """
    # psutil reports percent-of-one-core (the top/htop convention); divide by
    # the logical core count so the recorded figure is percent of the whole
    # machine and the axis reads 0-100.
    cores = psutil.cpu_count(logical=True) or 1
    cpu_percent = float(proc.cpu_percent(interval=None)) / cores
    memory = sample_memory(proc)
    active_runs = run_counter.count() if run_counter is not None else None
    return Sample(
        elapsed=time.time() - start_time,
        timestamp=datetime.now().isoformat(timespec="seconds"),
        cpu_percent=cpu_percent,
        memory=memory,
        active_runs=active_runs,
    )


def build_output_paths(
    base_dir: Path | str, timestamp: datetime
) -> Tuple[Path, Path]:
    """Create *base_dir* and return timestamped ``(csv_path, png_path)``.

    Args:
        base_dir: Directory the outputs should land in; created if missing.
        timestamp: Stamp used to name the files.

    Returns:
        A ``(csv_path, png_path)`` tuple sharing one ``YYYYmmdd_HHMMSS`` stem.
    """
    directory = Path(base_dir)
    directory.mkdir(parents=True, exist_ok=True)
    stem = f"leviath_monitor_{timestamp.strftime('%Y%m%d_%H%M%S')}"
    return directory / f"{stem}.csv", directory / f"{stem}.png"


def init_csv(csv_path: Path | str) -> None:
    """Write the CSV header row to *csv_path*, truncating any existing file."""
    with open(csv_path, "w", newline="", encoding="utf-8") as handle:
        csv.writer(handle).writerow(CSV_HEADER)


def append_csv_row(csv_path: Path | str, sample: Sample) -> None:
    """Append one *sample* to *csv_path*.

    Rows are flushed as they are taken rather than dumped at the end, so the
    data survives even if the monitor is killed outright instead of Ctrl-C'd.
    Unmeasurable values are left as empty cells.
    """
    def cell(value: Optional[float]) -> str:
        return "" if value is None else f"{value:.3f}"

    memory = sample.memory
    with open(csv_path, "a", newline="", encoding="utf-8") as handle:
        csv.writer(handle).writerow(
            [
                f"{sample.elapsed:.3f}",
                sample.timestamp,
                f"{sample.cpu_percent:.2f}",
                f"{memory.rss_mb:.3f}",
                cell(memory.pss_mb),
                cell(memory.uss_mb),
                cell(memory.lazy_free_mb),
                cell(memory.live_mb),
                "" if sample.active_runs is None else str(sample.active_runs),
            ]
        )


def _stats_label(name: str, values: Sequence[float], unit: str) -> str:
    """Format ``name (avg X, peak Y unit)`` for a panel title."""
    if not values:
        return name
    avg = sum(values) / len(values)
    return f"{name} (avg {avg:.1f}, peak {max(values):.1f} {unit})"


def _plot_series(
    axes,
    samples: Sequence[Sample],
    pick: Callable[[Sample], Optional[float]],
    color: str,
    label: Optional[str] = None,
    step: bool = False,
) -> List[float]:
    """Plot one metric, skipping ``None`` gaps, with a dashed average line.

    Returns:
        The plotted (non-``None``) values, for the caller's title stats.
    """
    points = [
        (sample.elapsed, value)
        for sample in samples
        if (value := pick(sample)) is not None
    ]
    if not points:
        return []
    xs = [p[0] for p in points]
    ys = [p[1] for p in points]
    if step:
        axes.step(xs, ys, where="post", color=color, linewidth=1.5, label=label)
    else:
        axes.plot(xs, ys, color=color, linewidth=1.5, label=label)
    axes.fill_between(xs, ys, color=color, alpha=0.15, step="post" if step else None)
    axes.axhline(
        sum(ys) / len(ys), color=color, linewidth=1.0, linestyle="--", alpha=0.6
    )
    return ys


def generate_graph(
    samples: Sequence[Sample],
    png_path: Path | str,
    label: str = DEFAULT_PROCESS_NAME,
    concurrency: Optional[Tuple[Sequence[float], Sequence[float]]] = None,
) -> Path:
    """Render *samples* to a three-panel PNG at *png_path*.

    Panels share one x-axis: active sessions on top, CPU in the middle, and
    memory (RSS vs live footprint) on the bottom, each titled with its average
    and peak.

    Args:
        samples: Collected measurements; an empty sequence produces a
            placeholder image rather than an error.
        png_path: Where to write the PNG.
        label: Process name used in the figure title.
        concurrency: Optional exact concurrency step data from
            :func:`concurrency_steps`, drawn on the sessions panel alongside
            the sampled counts; the sampled series alone misses runs shorter
            than the sample interval.

    Returns:
        The path that was written.
    """
    destination = Path(png_path)
    figure, (runs_axes, cpu_axes, memory_axes) = plt.subplots(
        3, 1, sharex=True, figsize=(10, 9)
    )

    if samples:
        runs = _plot_series(
            runs_axes,
            samples,
            lambda s: None if s.active_runs is None else float(s.active_runs),
            color="tab:green",
            label="sampled",
            step=True,
        )
        exact_peak: Optional[float] = None
        if concurrency is not None and concurrency[0]:
            xs, ys = concurrency
            runs_axes.step(
                xs,
                ys,
                where="post",
                color="tab:orange",
                linewidth=1.2,
                label="exact (run files)",
            )
            exact_peak = max(ys)
        if runs or exact_peak is not None:
            title = _stats_label("Active sessions", runs, "runs")
            if exact_peak is not None:
                title += f" - exact peak {exact_peak:.0f}"
            runs_axes.set_title(title, fontsize=10)
            runs_axes.legend(loc="upper right", fontsize=8)
        else:
            runs_axes.set_title("Active sessions (no data)", fontsize=10)

        cpu = _plot_series(cpu_axes, samples, lambda s: s.cpu_percent, "tab:red")
        cpu_axes.set_title(
            _stats_label("CPU, % of whole machine", cpu, "%"), fontsize=10
        )

        rss = _plot_series(
            memory_axes, samples, lambda s: s.memory.rss_mb, "tab:blue", label="rss"
        )
        pss = _plot_series(
            memory_axes,
            samples,
            lambda s: s.memory.pss_mb,
            "tab:cyan",
            label="pss",
        )
        live = _plot_series(
            memory_axes,
            samples,
            lambda s: s.memory.live_mb,
            "tab:purple",
            label="live",
        )
        parts = [_stats_label("rss", rss, "MB")]
        if pss:
            parts.append(_stats_label("pss", pss, "MB"))
        if live:
            parts.append(_stats_label("live", live, "MB"))
        memory_axes.set_title("Memory - " + ", ".join(parts), fontsize=10)
        memory_axes.legend(loc="upper left", fontsize=8)
    else:
        for axes in (runs_axes, cpu_axes, memory_axes):
            axes.text(
                0.5,
                0.5,
                "No data collected",
                ha="center",
                va="center",
                transform=axes.transAxes,
                fontsize=12,
                color="gray",
            )

    runs_axes.set_ylabel("Sessions")
    cpu_axes.set_ylabel("CPU % (whole machine)")
    memory_axes.set_ylabel("Memory (MB)")
    memory_axes.set_xlabel("Elapsed time (seconds)")
    for axes in (runs_axes, cpu_axes, memory_axes):
        axes.grid(True, alpha=0.3)

    figure.suptitle(
        f"{label} resource usage - {len(samples)} sample(s)", fontsize=13
    )
    figure.tight_layout()
    figure.savefig(destination, dpi=120)
    plt.close(figure)
    return destination


def watch_loop(
    proc: psutil.Process,
    interval: float,
    csv_path: Path | str,
    max_samples: Optional[int] = None,
    sleep_fn: Callable[[float], None] = time.sleep,
    on_sample: Optional[Callable[[Sample], None]] = None,
    run_counter: Optional[ActiveRunCounter] = None,
    start_time: Optional[float] = None,
) -> List[Sample]:
    """Sample *proc* every *interval* seconds until told to stop.

    The loop ends on Ctrl-C (``KeyboardInterrupt``), when the watched process
    exits or becomes unreadable, or when *max_samples* measurements have been
    taken. In every case the samples gathered so far are returned rather than
    discarded.

    ``sleep_fn``, ``max_samples`` and ``on_sample`` are injection points that
    let tests drive this loop deterministically and without real waiting.

    Args:
        proc: Process to watch.
        interval: Seconds to sleep between samples.
        csv_path: CSV file (already carrying its header) to append to.
        max_samples: Optional cap on the number of samples; ``None`` means run
            until interrupted.
        sleep_fn: Callable used to wait between samples.
        on_sample: Called with each sample; defaults to a one-line status print.
        run_counter: Optional session counter shared across samples.
        start_time: ``time.time()`` origin for the elapsed x-axis; defaults to
            now. Callers that also reconstruct run intervals pass the same
            origin to both so the series line up.

    Returns:
        Every sample collected, in order.
    """
    samples: List[Sample] = []
    if start_time is None:
        start_time = time.time()
    report = on_sample if on_sample is not None else _print_sample

    while True:
        try:
            sample = sample_process(proc, start_time, run_counter)
            append_csv_row(csv_path, sample)
            samples.append(sample)
            report(sample)

            if max_samples is not None and len(samples) >= max_samples:
                break
            sleep_fn(interval)
        except KeyboardInterrupt:
            print()  # move off the live status line before the summary
            break
        except (psutil.NoSuchProcess, psutil.AccessDenied):
            print("\nWatched process is gone; wrapping up.")
            break

    return samples


def _print_sample(sample: Sample) -> None:
    """Print a single live status line for *sample*."""
    live = (
        "      n/a"
        if sample.memory.live_mb is None
        else f"{sample.memory.live_mb:9.1f}"
    )
    runs = "  ?" if sample.active_runs is None else f"{sample.active_runs:3d}"
    print(
        f"  [{sample.elapsed:7.1f}s] "
        f"runs {runs}   cpu {sample.cpu_percent:6.1f}%   "
        f"rss {sample.memory.rss_mb:9.1f} MB   live {live} MB"
    )


def build_arg_parser() -> argparse.ArgumentParser:
    """Build and return the command-line argument parser."""
    parser = argparse.ArgumentParser(
        prog="leviath_monitor.py",
        description=(
            "Watch the leviath process and graph its sessions, CPU, and "
            "memory use. Press Ctrl-C to stop and write the outputs."
        ),
    )
    parser.add_argument(
        "-n",
        "--name",
        default=DEFAULT_PROCESS_NAME,
        help=(
            "substring to match against process names/cmdlines "
            f"(default: {DEFAULT_PROCESS_NAME})"
        ),
    )
    parser.add_argument(
        "-i",
        "--interval",
        type=float,
        default=DEFAULT_INTERVAL,
        help=f"seconds between samples (default: {DEFAULT_INTERVAL:g})",
    )
    parser.add_argument(
        "-o",
        "--output-dir",
        default=".",
        help="directory for the CSV and PNG outputs (default: current dir)",
    )
    parser.add_argument(
        "--max-samples",
        type=int,
        default=None,
        help="stop automatically after this many samples (default: unlimited)",
    )
    parser.add_argument(
        "--pid",
        type=int,
        default=None,
        help="watch this exact pid instead of searching by name",
    )
    parser.add_argument(
        "--runs-dir",
        default=None,
        help=(
            "runs directory to count active sessions from (default: "
            "LEVIATH_RUNS_DIR, else LEVIATH_HOME/.leviath/runs, else "
            "~/.leviath/runs)"
        ),
    )
    parser.add_argument(
        "--no-runs",
        action="store_true",
        help="skip session counting entirely",
    )
    return parser


def main(argv: Optional[Sequence[str]] = None) -> int:
    """Run the command-line interface.

    Args:
        argv: Argument list to parse. Defaults to ``sys.argv[1:]``.

    Returns:
        A process exit code (see the module docstring).
    """
    parser = build_arg_parser()
    args = parser.parse_args(argv)

    if args.interval <= 0:
        print("error: --interval must be greater than 0", file=sys.stderr)
        return 1
    if args.max_samples is not None and args.max_samples <= 0:
        print("error: --max-samples must be greater than 0", file=sys.stderr)
        return 1

    if args.pid is not None:
        try:
            proc = psutil.Process(args.pid)
        except psutil.Error:
            print(f"error: pid {args.pid} is not running", file=sys.stderr)
            return 1
    else:
        proc = find_leviath_process(args.name)
    if proc is None:
        print(
            f"error: no running process matching {args.name!r} was found",
            file=sys.stderr,
        )
        return 1

    try:
        pid = proc.pid
        name = proc.name()
    except psutil.Error:
        print(
            f"error: process matching {args.name!r} exited before monitoring "
            "could start",
            file=sys.stderr,
        )
        return 1

    # psutil reports CPU percent as a delta between calls, so the first call
    # always returns 0.0. Prime it here so the first recorded sample is real.
    try:
        proc.cpu_percent(interval=None)
    except psutil.Error:
        pass

    run_counter: Optional[ActiveRunCounter] = None
    if not args.no_runs:
        runs_dir = Path(args.runs_dir) if args.runs_dir else default_runs_dir()
        run_counter = ActiveRunCounter(runs_dir)
        if not runs_dir.is_dir():
            print(
                f"note: runs dir {runs_dir} does not exist yet; session "
                "counts will be empty until it does"
            )

    csv_path, png_path = build_output_paths(args.output_dir, datetime.now())
    init_csv(csv_path)

    print(f"Watching {name!r} (pid {pid}) every {args.interval:g}s")
    print(f"  csv: {csv_path}")
    print(f"  png: {png_path}")
    print("Press Ctrl-C to stop.\n")

    wall_start = time.time()
    samples = watch_loop(
        proc,
        args.interval,
        csv_path,
        max_samples=args.max_samples,
        run_counter=run_counter,
        start_time=wall_start,
    )
    wall_end = time.time()

    concurrency: Optional[Tuple[List[float], List[float]]] = None
    if run_counter is not None:
        intervals = collect_run_intervals(
            run_counter.runs_dir, wall_start, wall_end
        )
        if intervals:
            runs_csv = csv_path.with_name(csv_path.stem + "_runs.csv")
            write_runs_csv(runs_csv, intervals, wall_start)
            concurrency = concurrency_steps(intervals, wall_start, wall_end)
            print(f"\nRun intervals ({len(intervals)}): {runs_csv}")

    generate_graph(samples, png_path, label=name, concurrency=concurrency)

    print(f"\nCollected {len(samples)} sample(s).")
    print(f"Data:  {csv_path}")
    print(f"Graph: {png_path}")
    return 0


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())

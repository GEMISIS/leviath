#!/usr/bin/env python3
"""Watch the ``leviath`` daemon and graph sessions, CPU, and memory over time.

Start this script, go do whatever you want with leviath, then press Ctrl-C to
stop it. On stop it writes a timestamped CSV of every sample it took plus a
timestamped PNG graph with three aligned panels:

1. Active sessions - runs whose ``meta.json`` status is non-terminal
   (``starting``, ``running``, ``waiting_input``, ``paused``), sampled from the
   runs directory the daemon persists to.
2. CPU percent.
3. Memory - two lines. ``rss`` is what ``ps`` shows and includes freed pages
   the allocator has not handed back to the OS, so it only ever ratchets up.
   ``footprint`` is the process's live memory (macOS physical footprint, Linux
   PSS, Windows USS) and is the number that actually tells you whether leviath
   is holding onto memory. When the two diverge, the gap is allocator-retained
   pages, not a leak.

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
    "DEFAULT_PROCESS_NAME",
    "DEFAULT_INTERVAL",
    "CSV_HEADER",
    "ACTIVE_STATUSES",
    "find_leviath_process",
    "sample_live_memory_mb",
    "default_runs_dir",
    "ActiveRunCounter",
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

#: Column order of the emitted CSV.
CSV_HEADER = (
    "elapsed_seconds",
    "timestamp",
    "cpu_percent",
    "rss_mb",
    "footprint_mb",
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


class Sample(NamedTuple):
    """One point-in-time measurement of the watched process."""

    elapsed: float
    timestamp: str
    cpu_percent: float
    rss_mb: float
    footprint_mb: Optional[float]
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


def _live_memory_darwin(pid: int) -> Optional[float]:
    """macOS: physical footprint, in MB.

    Physical footprint is the figure ``vmmap`` and Activity Monitor report,
    and unlike psutil's ``memory_full_info`` it needs no elevated privileges.
    RSS on macOS keeps counting MADV_FREE'd pages, so it can sit hundreds of
    MB above the memory the process actually holds.

    ``/usr/bin/footprint`` answers in ~30ms; ``top`` is the fallback because
    it is always present but costs over a second of system time per call (it
    scans the whole process table even when pinned to one pid).
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
            return float(match.group(1)) * _UNIT_TO_MB[match.group(2)]
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
        return None
    for line in reversed(out.splitlines()):
        token = line.strip().split()[0] if line.strip() else ""
        match = _TOP_MEM_RE.match(token)
        if match:
            return float(match.group(1)) * _UNIT_TO_MB[match.group(2)]
    return None


def _live_memory_linux(pid: int) -> Optional[float]:
    """Linux: PSS from ``smaps_rollup``, in MB. Unprivileged for own processes."""
    try:
        text = Path(f"/proc/{pid}/smaps_rollup").read_text()
    except OSError:
        return None
    for line in text.splitlines():
        if line.startswith("Pss:"):
            parts = line.split()
            if len(parts) >= 2 and parts[1].isdigit():
                return int(parts[1]) / 1024
    return None


def _live_memory_psutil(proc: psutil.Process) -> Optional[float]:
    """Fallback: USS via psutil, in MB. Works unprivileged on Windows."""
    try:
        return proc.memory_full_info().uss / _BYTES_PER_MB
    except (psutil.Error, AttributeError):
        return None


def sample_live_memory_mb(proc: psutil.Process) -> Optional[float]:
    """Return the process's live memory in MB, or ``None`` if unmeasurable.

    "Live" means the memory the process is actually holding right now: macOS
    physical footprint, Linux PSS, Windows USS. Where the platform refuses to
    say (e.g. psutil's USS needs root on macOS), the sample is ``None`` and the
    CSV cell is left empty rather than repeating the misleading RSS number.
    """
    if sys.platform == "darwin":
        return _live_memory_darwin(proc.pid)
    if sys.platform.startswith("linux"):
        value = _live_memory_linux(proc.pid)
        if value is not None:
            return value
    return _live_memory_psutil(proc)


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
    cpu_percent = float(proc.cpu_percent(interval=None))
    rss_mb = proc.memory_info().rss / _BYTES_PER_MB
    footprint_mb = sample_live_memory_mb(proc)
    active_runs = run_counter.count() if run_counter is not None else None
    return Sample(
        elapsed=time.time() - start_time,
        timestamp=datetime.now().isoformat(timespec="seconds"),
        cpu_percent=cpu_percent,
        rss_mb=rss_mb,
        footprint_mb=footprint_mb,
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
    with open(csv_path, "a", newline="", encoding="utf-8") as handle:
        csv.writer(handle).writerow(
            [
                f"{sample.elapsed:.3f}",
                sample.timestamp,
                f"{sample.cpu_percent:.2f}",
                f"{sample.rss_mb:.3f}",
                "" if sample.footprint_mb is None else f"{sample.footprint_mb:.3f}",
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
            step=True,
        )
        if runs:
            runs_axes.set_title(
                _stats_label("Active sessions", runs, "runs"), fontsize=10
            )
        else:
            runs_axes.set_title("Active sessions (no data)", fontsize=10)

        cpu = _plot_series(cpu_axes, samples, lambda s: s.cpu_percent, "tab:red")
        cpu_axes.set_title(_stats_label("CPU", cpu, "%"), fontsize=10)

        rss = _plot_series(
            memory_axes, samples, lambda s: s.rss_mb, "tab:blue", label="rss"
        )
        footprint = _plot_series(
            memory_axes,
            samples,
            lambda s: s.footprint_mb,
            "tab:purple",
            label="footprint (live)",
        )
        parts = [_stats_label("rss", rss, "MB")]
        if footprint:
            parts.append(_stats_label("footprint", footprint, "MB"))
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
    cpu_axes.set_ylabel("CPU %")
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

    Returns:
        Every sample collected, in order.
    """
    samples: List[Sample] = []
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
    footprint = (
        "      n/a"
        if sample.footprint_mb is None
        else f"{sample.footprint_mb:9.1f}"
    )
    runs = "  ?" if sample.active_runs is None else f"{sample.active_runs:3d}"
    print(
        f"  [{sample.elapsed:7.1f}s] "
        f"runs {runs}   cpu {sample.cpu_percent:6.1f}%   "
        f"rss {sample.rss_mb:9.1f} MB   live {footprint} MB"
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

    samples = watch_loop(
        proc,
        args.interval,
        csv_path,
        max_samples=args.max_samples,
        run_counter=run_counter,
    )

    generate_graph(samples, png_path, label=name)

    print(f"\nCollected {len(samples)} sample(s).")
    print(f"Data:  {csv_path}")
    print(f"Graph: {png_path}")
    return 0


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())

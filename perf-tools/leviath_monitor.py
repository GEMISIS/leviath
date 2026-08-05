#!/usr/bin/env python3
"""Watch the ``leviath`` process and graph its CPU / memory use over time.

Start this script, go do whatever you want with leviath, then press Ctrl-C to
stop it. On stop it writes a timestamped CSV of every sample it took plus a
timestamped PNG graph (CPU percent on top, resident memory on the bottom).
Because both output names carry a ``YYYYmmdd_HHMMSS`` stamp, repeated runs never
clobber each other.

Requires ``psutil`` and ``matplotlib``.

Usage:
    python3 leviath_monitor.py
    python3 leviath_monitor.py -i 0.5
    python3 leviath_monitor.py -n leviath-server -o ./monitor-runs
    python3 leviath_monitor.py --max-samples 60

Exit codes:
    0 - clean stop (Ctrl-C, sample cap reached, or watched process exited);
        whatever samples were collected have been written out
    1 - bad arguments, or no matching process was found
"""

from __future__ import annotations

import argparse
import csv
import os
import sys
import time
from datetime import datetime
from pathlib import Path
from typing import Callable, List, NamedTuple, Optional, Sequence, Tuple

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
    "find_leviath_process",
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
CSV_HEADER = ("elapsed_seconds", "timestamp", "cpu_percent", "memory_mb")

#: Bytes per megabyte, used to convert psutil's RSS figure.
_BYTES_PER_MB = 1024 * 1024


class Sample(NamedTuple):
    """One point-in-time measurement of the watched process."""

    elapsed: float
    timestamp: str
    cpu_percent: float
    memory_mb: float


def find_leviath_process(
    pattern: str = DEFAULT_PROCESS_NAME,
) -> Optional[psutil.Process]:
    """Return the first running process matching *pattern*, or ``None``.

    The match is a case-insensitive substring test against both the process
    name and its full command line, so ``leviath`` finds ``leviath``,
    ``leviath-server`` and ``python3 /opt/leviath/run.py`` alike.

    This monitor's own process is always skipped: its command line contains
    "leviath_monitor.py", which would otherwise match the default pattern and
    make the script graph itself instead of the process you care about.

    Args:
        pattern: Substring to look for.

    Returns:
        A :class:`psutil.Process` for the first match, or ``None`` if nothing
        matched.
    """
    needle = pattern.lower()
    own_pid = os.getpid()
    for proc in psutil.process_iter(["pid", "name", "cmdline"]):
        try:
            info = proc.info
            if info.get("pid") == own_pid:
                continue
            name = (info.get("name") or "").lower()
            cmdline = " ".join(info.get("cmdline") or []).lower()
        except psutil.Error:
            # Process died or is not readable while we were iterating; skip it.
            continue
        if needle in name or needle in cmdline:
            return proc
    return None


def sample_process(proc: psutil.Process, start_time: float) -> Sample:
    """Take one CPU/memory measurement of *proc*.

    Args:
        proc: The process to measure.
        start_time: ``time.time()`` value from when monitoring began, used to
            compute the elapsed-seconds x-axis value.

    Returns:
        A :class:`Sample`.

    Raises:
        psutil.Error: If the process vanished or is no longer readable.
    """
    cpu_percent = float(proc.cpu_percent(interval=None))
    memory_mb = proc.memory_info().rss / _BYTES_PER_MB
    return Sample(
        elapsed=time.time() - start_time,
        timestamp=datetime.now().isoformat(timespec="seconds"),
        cpu_percent=cpu_percent,
        memory_mb=memory_mb,
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
    """
    with open(csv_path, "a", newline="", encoding="utf-8") as handle:
        csv.writer(handle).writerow(
            [
                f"{sample.elapsed:.3f}",
                sample.timestamp,
                f"{sample.cpu_percent:.2f}",
                f"{sample.memory_mb:.3f}",
            ]
        )


def generate_graph(
    samples: Sequence[Sample],
    png_path: Path | str,
    label: str = DEFAULT_PROCESS_NAME,
) -> Path:
    """Render *samples* to a two-panel PNG at *png_path*.

    Args:
        samples: Collected measurements; an empty sequence produces a
            placeholder image rather than an error.
        png_path: Where to write the PNG.
        label: Process name used in the figure title.

    Returns:
        The path that was written.
    """
    destination = Path(png_path)
    figure, (cpu_axes, memory_axes) = plt.subplots(
        2, 1, sharex=True, figsize=(10, 7)
    )

    if samples:
        elapsed = [sample.elapsed for sample in samples]
        cpu = [sample.cpu_percent for sample in samples]
        memory = [sample.memory_mb for sample in samples]

        cpu_axes.plot(elapsed, cpu, color="tab:red", linewidth=1.5)
        cpu_axes.fill_between(elapsed, cpu, color="tab:red", alpha=0.15)
        memory_axes.plot(elapsed, memory, color="tab:blue", linewidth=1.5)
        memory_axes.fill_between(elapsed, memory, color="tab:blue", alpha=0.15)

        peak_cpu = max(cpu)
        peak_memory = max(memory)
        cpu_axes.set_title(f"CPU (peak {peak_cpu:.1f}%)", fontsize=10)
        memory_axes.set_title(f"Memory (peak {peak_memory:.1f} MB)", fontsize=10)
    else:
        for axes in (cpu_axes, memory_axes):
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

    cpu_axes.set_ylabel("CPU %")
    memory_axes.set_ylabel("Memory (MB)")
    memory_axes.set_xlabel("Elapsed time (seconds)")
    for axes in (cpu_axes, memory_axes):
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

    Returns:
        Every sample collected, in order.
    """
    samples: List[Sample] = []
    start_time = time.time()
    report = on_sample if on_sample is not None else _print_sample

    while True:
        try:
            sample = sample_process(proc, start_time)
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
    print(
        f"  [{sample.elapsed:7.1f}s] "
        f"cpu {sample.cpu_percent:6.1f}%   mem {sample.memory_mb:9.1f} MB"
    )


def build_arg_parser() -> argparse.ArgumentParser:
    """Build and return the command-line argument parser."""
    parser = argparse.ArgumentParser(
        prog="leviath_monitor.py",
        description=(
            "Watch the leviath process and graph its CPU and memory use. "
            "Press Ctrl-C to stop and write the outputs."
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
    )

    generate_graph(samples, png_path, label=name)

    print(f"\nCollected {len(samples)} sample(s).")
    print(f"Data:  {csv_path}")
    print(f"Graph: {png_path}")
    return 0


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())

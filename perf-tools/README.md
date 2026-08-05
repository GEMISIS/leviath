# perf-tools

Measurement tooling for the leviath daemon. The point of this directory is
that any number we publish about leviath's resource usage is reproducible,
uses the right metric for the OS it was taken on, and says exactly what it
means. If you are comparing leviath against anything else, use the same
metrics on both sides or the comparison is meaningless.

## Tools

- `leviath_monitor.py` - samples one process (CPU, every memory metric the OS
  provides, active session count) on an interval, writes a CSV per sample and
  a three-panel PNG on exit. `--pid` pins an exact process; the default finds
  the daemon by name. On exit it also reconstructs the *exact* session
  concurrency from each run directory's creation time and its `meta.json`'s
  last write (sub-second precision), because interval sampling misses any run
  shorter than the interval; the graph shows both curves and the intervals
  land in a `*_runs.csv`.
- `ws_churn_test.py` - opens batches of WebSocket connections against
  `lev serve` and drops them abruptly (SO_LINGER 0, like a killed browser
  tab), printing the server's RSS between batches. A per-connection leak shows
  up as a staircase.

## Memory metrics: what they mean, and the trap

The single most common benchmarking mistake for long-lived processes is
graphing RSS and calling it "memory usage". It is wrong in the same way on
both macOS and Linux:

Modern allocators return freed memory to the kernel *lazily*, via
`MADV_FREE` (Linux) / `MADV_FREE_REUSABLE` (macOS). The kernel takes those
pages back only under memory pressure - until then they still count in RSS
(and on Linux, in PSS and USS too). So a process that spiked once and freed
everything keeps reporting its high-water mark forever. That is allocator
bookkeeping, not held memory, and it is invisible to `ps`, `top`'s RES
column, and most dashboards.

Both halves of that claim are verified by experiment in this repo, not
folklore:

- **Linux** (`linux_mem_check` experiment, runnable in any container):
  allocate 300 MB anonymous memory, touch every page, `madvise(MADV_FREE)`
  the lot. Measured result: `Rss` and `Pss` in `smaps_rollup` still report
  ~310 MB, the `LazyFree` field reports the 300 MB, and `Pss - LazyFree`
  lands back at the ~10 MB baseline.
- **macOS**: an idle leviath daemon measured 292.8 MB RSS while
  `vmmap --summary` reported a 21.7 MB physical footprint - the difference
  was exactly the `MALLOC_SMALL (empty)` reclaimable regions.
- **macOS, second layer**: physical footprint itself still counts pages the
  allocator returned via `MADV_FREE_REUSABLE`. A settled post-burst daemon
  measured 50 MB footprint of which the `/usr/bin/footprint` category table
  flagged 48 MB as `Reclaimable` - pages the kernel repossesses under
  pressure, holding no data (the in-process reachable heap at that moment was
  ~2 MB). So on macOS the reclaimable column is the twin of Linux's
  `LazyFree` and gets subtracted the same way.

The monitor therefore records every raw metric and one corrected series:

| column | macOS | Linux | Windows |
|---|---|---|---|
| `rss_mb` | psutil RSS | psutil RSS | psutil RSS (working set) |
| `pss_mb` | - | `smaps_rollup` `Pss` | - |
| `uss_mb` | - | `Private_Clean + Private_Dirty` | psutil USS |
| `lazy_free_mb` | `footprint` `Reclaimable` | `smaps_rollup` `LazyFree` | - |
| `live_mb` | footprint minus reclaimable | `Pss - LazyFree` | USS |

`live_mb` is the headline series: the memory the process actually holds.

- macOS physical footprint is the kernel's own accounting (what Activity
  Monitor's Memory column shows); subtracting its reclaimable portion removes
  the lazily-freed pages it still counts. psutil cannot provide USS/PSS on
  macOS without root, and `top` costs over a second of system time per query;
  `/usr/bin/footprint` answers in ~30 ms unprivileged.
- On Linux, `LazyFree` pages are private to the freeing process, so
  subtracting the field from PSS is exact, not an estimate.
- Windows decommits freed heap immediately (no lazy-free mechanism), so USS
  needs no correction.

One attribution trap worth knowing when you point Apple's tools at `lev`
yourself: mimalloc (the default allocator) tags its arena mappings with VM
tag 100, which `vmmap`, `footprint`, and Instruments all label
`IOAccelerator` as if the process held GPU driver memory. It does not - a
headless daemon never loads the AGX Metal driver. Launching with
`MIMALLOC_OS_TAG=240` relabels the very same regions `app-specific tag 1`,
which is how we proved the post-burst "IOAccelerator" residue is just empty,
reclaimable heap pages.

An empty CSV cell means the OS cannot provide that metric. Nothing is ever
approximated from another column.

### Reading a leviath graph honestly

- `rss` far above `live`, both flat: allocator-retained lazily-freed pages.
  Not a leak. The gap disappears under system memory pressure.
- `live` climbing while runs are active, dropping when they finish: normal.
- `live` staying at its peak after every run has finished: that would be
  retention - file an issue with the CSV.

## The deterministic burst benchmark

Numbers comparing leviath builds come from a fixed workload, not from live
model traffic (model latency and token counts vary run to run; a benchmark
that includes them measures the provider, not the runtime):

1. An isolated home: `LEVIATH_HOME=/tmp/levperf`, `LEVIATH_SKIP_DOTENV=1`.
2. A Rhai mock provider (`providers/mock.rhai`) that derives its turn number
   from the conversation and returns ~25 KB `context_append` tool calls for
   12 turns, then finishes - no network, no cost, byte-identical every run.
3. A one-stage `stress` agent whose context window grows to ~77K tokens.
4. N concurrent `lev run stress --yolo` spawns, monitored with
   `leviath_monitor.py --pid <daemon pid> -i 0.5`, with a 45-second settle
   window after the last run so post-burst release is visible.

Run the same protocol on both binaries under comparison, on the same machine,
same power state. Report `live_mb` avg/peak, the settled `live_mb` after the
burst, and CPU avg/peak - never RSS alone.

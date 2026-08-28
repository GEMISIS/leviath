# perf-tools

Measurement tooling for the leviath daemon. The point of this directory is
that any number we publish about leviath's resource usage is reproducible,
uses the right metric for the OS it was taken on, and says exactly what it
means. If you are comparing leviath against anything else, use the same
metrics on both sides or the comparison is meaningless.

## Tools

- The process monitor lives in the
  [leviath-benchmarks](https://github.com/GEMISIS/leviath-benchmarks) repo
  (`bench/monitor.py`) - one canonical home instead of a drifting copy
  here. It samples one process (CPU as whole-machine share, every memory
  metric the OS provides, active session count), writes a CSV per sample,
  and reconstructs exact per-run concurrency from filesystem timestamps.
  This directory keeps the methodology notes below plus the experiments
  that verified them.
- `ws_churn_test.py` - opens batches of WebSocket connections against
  `lev serve` and drops them abruptly (SO_LINGER 0, like a killed browser
  tab), printing the server's RSS between batches. A per-connection leak shows
  up as a staircase.

## The measuring sticks the cleanup work is gated on

Every performance PR has to move one of the numbers below, measured the same
way before and after, or it does not merge. The scripts are Python and shell
on purpose: nothing here is a workspace member, so the 100% coverage gate
does not apply and no benchmark target pollutes a crate's profile.

- `harness.sh` - the isolated environment every live probe runs in. Wraps a
  command in `env -i` with a short `LEVIATH_HOME` (`/tmp/lv`), the
  repo-root `.env` skipped, and the native OpenAI provider pointed at
  `mock.py`. Use it as a prefix: `perf-tools/harness.sh lev ps`. It also
  installs `agents/probe/`, a one-stage blueprint that calls whatever tool
  the mock asks for.
- `mock.py PORT [TOOL ARGS-JSON]` - a stateless OpenAI-compatible provider.
  It decides from the request body, never a turn counter (the daemon spends
  a turn at startup): the tool call is returned until `messages` carries a
  `role: "tool"` entry, then the reply is `done`. `GET /count` says how many
  completions it served, which is what makes a "no provider call happened"
  probe provable rather than silent.
- `daemon_drive.py --runs K [--tool NAME --args JSON --yolo]` - starts the
  mock, a daemon and `lev serve`, spawns K runs, waits for them to finish,
  and prints wall clock plus rusage as JSON. Run it through `harness.sh`.
- `fake_runs.py --from RUN_DIR --count 750 --out DIR` - copies one real run
  directory N times with fresh ids. Copies, not stubs: a 200-byte
  `context.json` would hide exactly the per-frame parsing cost the dashboard
  numbers exist to catch.
- `dash_pty.py --bin lev --seconds 30 --keys 'jjj'` - drives `lev dash`
  over a real pty, accumulating the whole escape stream (a full pty buffer
  blocks the child and corrupts the measurement), and reports the child's
  CPU seconds, max RSS, bytes written, repaint count and a normalised hash of
  the first frame. The hash must match before and after a change; the CPU
  and the syscall count (`measure.sh`, Linux) must fall.
- `measure.sh CMD...` - `perf stat` + `strace -c` on Linux, `/usr/bin/time
  -l` on macOS. Linux is the gate; macOS is corroboration.
- `binsize.sh [BIN]` - release binary size, package count, `__const` /
  `.rodata` size, and whether the unreachable tiktoken vocabularies are still
  linked in.
- `baselines/` - one JSON per commit the numbers were taken at. Compare
  against the newest one.

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
- `live` settling at a flat 20-45 MB floor after a burst, well below its
  peak: freed pages whose purge mimalloc deferred to each worker thread and
  a fully idle daemon never executes (measured: flat for 10 minutes idle,
  then partially flushed by a single follow-up run; the reachable heap at
  that moment is ~2 MB). The pages are reused by the next burst, so the
  floor does not stack across bursts. Launching the daemon with
  `MIMALLOC_PURGE_DELAY=0` purges at free time instead and roughly halves
  the floor at no measured throughput cost.
- `live` staying at its full peak after every run has finished, or a floor
  that grows burst over burst: that would be retention - file an issue with
  the CSV.

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

# Memory budget

A filesystem daemon has no business using hundreds of megabytes. This document
is the budget, why each number is what it is, and how it is enforced.

The numbers here are **asserted in CI from M00**, before the daemon does anything
interesting. That ordering is deliberate: a budget retrofitted at M03 gets
discovered to be unreachable after three milestones of design have already been
committed to it.

## The budget

Measured as **RSS on Linux**, from outside the process, on the **release** binary.
Debug builds allocate differently and are not a valid proxy.

| Component | Ceiling | Asserted from |
|---|---|---|
| Daemon idle (watcher + event store + API) | **32 MiB** | M00 |
| Daemon steady-state, serving search | **96 MiB** | M03 |
| Peak during initial 1M-file scan | **128 MiB** | M01 |
| AI worker, resident while enriching | **4 GiB** | M02b |
| AI worker, not enriching | **0** | M02b |

The last row is the largest single win and it is a process-architecture decision,
not an optimisation: the model does **not** live in the daemon. It runs as a
separate process, spawned on demand, which exits when the enrichment queue
drains. So the steady-state cost of having AI features is zero.

"Idle" means: after a full scan has completed and the process has been idle long
enough for allocator decay to run. See *Settling* below — that qualifier is what
makes the number testable rather than a race.

## Why 32 MiB and not 500 MB

The original proposal set `<500MB RAM idle` for the daemon. That is rejected. Half
a gigabyte idle is not deployable on the target hardware (an older i7 NAS with no
GPU, running other things) and is far above what the workload needs. The daemon's
resident state at idle is a SQLite connection, an HTTP listener, and a file
watcher. None of that is tens of megabytes, let alone hundreds.

## Enforcement

Three layers, because each catches what the others miss:

1. **`crates/dafs-memtest`** spawns the release binary, waits for readiness, lets
   decay settle, and asserts RSS from `/proc/<pid>/statm`. Measuring from outside
   is the only way to get a number that includes the binary's own text and data,
   thread stacks, and mmapped regions.
2. **`/metrics` exports `dafs_resident_bytes`** from M00, so the budget is
   observable in a running deployment. A ceiling only ever checked in CI is a
   ceiling that gets discovered to be wrong in production.
3. **`crates/dafs-alloc` unit tests** cross-check the RSS reader itself — the
   page-size constant is load-bearing for every assertion above, so it is
   verified against a second source (`/proc/self/status`) rather than trusted.

If a ceiling assertion fails, **do not raise the constant to make it pass.**
Either the regression is real, or the budget needs an explicit revision in this
document with the reasoning recorded.

## Allocator

jemalloc, not glibc, and this is a correctness dependency rather than a
preference.

The budget requires RSS to **return** to the idle ceiling after a scan, not
merely to be low before one. glibc's malloc keeps per-thread arenas and, on a
many-thread scan-then-idle workload, fragments them badly enough that freed
memory is never handed back to the OS — RSS ratchets up and stays. The scan is
precisely that shape: many threads, millions of short-lived small allocations,
then near-total quiescence.

Tuning (in `crates/dafs-alloc`, compiled into the binary via the `malloc_conf`
symbol rather than an environment variable, so a deployment cannot lose it):

| Setting | Value | Why |
|---|---|---|
| `background_thread` | `true` | Decay runs without an allocation call to drive it. Without this an *idle* daemon never purges — exactly the state the idle ceiling measures. |
| `dirty_decay_ms` | `1000` | Dirty pages return to the OS a second after going unused. |
| `muzzy_decay_ms` | `0` | Muzzy pages return immediately instead of being held for reuse. |

This trades allocation throughput for lower steady-state RSS. Correct for this
workload: the scan is I/O-bound, and the daemon is idle most of its life.

### Settling

Because decay is time-based, a measurement taken immediately after freeing reads
pre-purge RSS. The test harness therefore waits ~2.5s (decay is 1s) before
measuring.

Forcing an immediate purge would be more deterministic, but the `arenas.purge`
mallctl is only reachable through an `unsafe` call, and an allocator wrapper is
exactly the place where an unsafe escape hatch gets reached for casually later.
Waiting is the cheaper trade.

## Techniques, bound to milestones

These are commitments, not a reading list. Each is due at a specific milestone
because retrofitting it later is expensive.

### M01 — scan and event path

- **Stream, never accumulate.** Bounded channels walker → hasher → writer, with
  backpressure. Peak memory is channel depth, *independent of corpus size* —
  which is what makes the 128 MiB scan ceiling hold at 10M files too.
- **mmap the read path** for hashing and extraction; let the kernel own the page
  cache. Those pages are file-backed and evictable, counted as cache rather than
  anonymous RSS. This is the difference between the kernel reclaiming memory
  under pressure and the daemon being OOM-killed.
- **One arena per worker thread, merged at the end** — not one shared concurrent
  map. (The 1BRC result: per-thread maps beat a shared map by 29s on the contest
  server.)
- **Do not store a `String` per path.** 1M paths at ~80 bytes averages 80 MB in
  `String`s before any structure at all. Intern path *components* into one arena
  and represent a path as parent-id + component-id; the tree shape means
  components repeat heavily. This alone is most of the gap between 32 MiB and a
  naive 200 MB+, and it must land in M01 because M07's graph will depend on path
  IDs — retrofitting it afterwards is far more invasive.
- **SWAR/SIMD parsing** where extraction is bytewise. Lower priority: buys time,
  not memory.

### M00 — allocator and harness

Allocator choice, decay tuning, and the RSS assertion harness. Cheapest possible
point, and it makes every later milestone's memory cost visible on its own PR.
**Assert that post-scan RSS returns to the idle ceiling** — that assertion, not
the idle one, is what catches fragmentation.

### M03 — index

**Binary quantisation is a functional requirement, not an optimisation.** One bit
per dimension instead of 32 is a 32× reduction: for 1M documents × 384 dims,
~1.5 GB of full-precision vectors becomes ~48 MiB. Full-float resident vectors
*cannot* meet the 96 MiB ceiling at 1M documents, so quantise-and-rescore is the
design rather than later tuning. Keep the 1-bit vectors and the graph resident,
page full-precision floats from disk, and rescore an oversampled candidate set
(2–4× the requested `k`). Published results put the recall cost around 2% — and
recall is measurable against the golden corpus, so the trade is verifiable rather
than assumed.

### M01/M03 — SQLite

Small page cache, large mmap window. `cache_size` in single-digit MiB,
`mmap_size` large. Same data either way, but mapped pages are file-backed and
evictable while page-cache pages are anonymous and are not. On an SSD the latency
difference is small; the RSS difference is not. Implemented in
`crates/dafs-store`'s `tune()`.

### M09 — compression

zstd with a trained dictionary for small objects, plain zstd for large. This
corpus is dominated by small text and metadata records, which is exactly where a
shared dictionary matters most. Train offline from a corpus sample, version it,
and store the dictionary ID with each object — **a dictionary is not
reconstructible after the fact**, so treat it as immutable once objects reference
it. Prefer LZ4 only where a measured latency budget rules zstd out; zstd's ~15–25%
better ratio is worth 2–4× encode cost on a write-once, read-many object store.

The **event log** is the single best dictionary-compression target in the system:
append-only, highly repetitive, and it grows without bound.

### M15 — soak

Sustained-soak memory behaviour, i.e. fragmentation over months. The ceilings
above are all short-horizon measurements; this is where they are proven to hold.

## Not a milestone

Memory work is deliberately **not** its own milestone. A milestone with no user
value whose purpose is to make earlier milestones smaller is the trap the roadmap
avoids — see `docs/roadmap-and-design-review.md` §4.

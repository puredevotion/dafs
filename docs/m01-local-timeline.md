# M01 — local timeline

**Delivered.** The first milestone with user value: point the daemon at a
directory and it answers *"what did I work on today?"*

## What it is

A read-only observer. It scans the directories you name, then keeps watching
them, and records what it saw as an append-only event log. A web page shows that
log — newest first, grouped by day, filterable by what happened.

Nothing here opens a file for writing, moves anything, or changes a byte of your
data. That is the property that makes this safe to point at a real home
directory on day one, and it has its own test rather than being a promise:
`scanning_never_modifies_the_observed_tree` snapshots every file's contents and
mtime either side of a scan and compares them.

```sh
dafs --watch ~/Documents --watch ~/src     # observe these
open http://127.0.0.1:7878                 # see what changed
```

## What shipped

| | |
|---|---|
| `dafs-store` | `path_components` / `files` / `events` schema, interning, the timeline query |
| `dafs-scan` | initial scan, debounced live watch, overflow recovery |
| `dafs-api` | `GET /events` with paging and kind filters, `GET`/`PUT /log-level` |
| `ui/` | Vite build; timeline grouped by day, kind filters, paging |
| daemon | `--watch`, `--no-initial-scan`, observer thread, second connection |
| tests | 91 total; crash consistency under `kill -9`, path properties, scan-memory growth |
| fuzz | `paths` target alongside M00's `migrations` |
| CI | `ui-bundle` staleness gate, `dast` (ZAP + nuclei) against a live daemon |

## Decisions worth knowing

**Paths are never stored as strings.** A path is a chain of interned components
with `files.parent_id` pointing at the containing directory. At a million files,
a `String` per path is ~80 MB before any structure at all, against a 32 MiB idle
ceiling. This had to land now rather than later: M07's graph will hang relations
off path ids, and retrofitting interning underneath it is far more invasive.

**Only directory names are cached.** The interner splits into `intern_dir`
(cached) and `intern_leaf` (not). Directory names repeat enormously; filenames
are nearly all distinct, so caching them is one resident entry per file in the
corpus for a hit rate near zero. The first version cached both and grew at ~430
bytes per file — see *Bugs found* below.

**The event log is append-only.** There is no update path, because from M06
events are the unit of synchronisation and a log that can be rewritten cannot be
replicated by replaying it. A correction is a new event.

**Pagination cursors on event id, not timestamp.** A scan puts thousands of
events inside one millisecond; a timestamp cursor skips or repeats them at page
boundaries.

**One connection behind a mutex, not a pool.** This answers the question M00
deliberately left open. The queries are all short reads against a local,
memory-mapped, warm database serving a single user — a pool would add N page
caches, which is exactly what the memory budget spends effort keeping to one.
Every access goes through `spawn_blocking`, so the lock is never held across an
await point. Revisit at M08 if the assistant brings long-running queries, with a
measurement rather than an assumption.

**The observer opens its own connection.** Two connections in WAL mode is the
case WAL exists for: the writer does not block readers. Sharing one would put
every timeline request behind the scan's lock for the length of the scan.

**Readiness does not wait for the initial scan.** On a large tree that runs for
minutes, and a deployment watching `/readyz` would conclude the daemon failed to
start. The API works throughout; it just has less to show at first.

**Directories generate no events.** A directory's mtime changes whenever a child
is added or removed, so emitting on it would double every create and delete with
a meaningless "the folder changed" row.

**Renames are correlated by the kernel, not guessed.** inotify assigns both
halves of a rename a cookie, surfaced by `notify` as a tracker id, so the
pairing is authoritative rather than a heuristic about timing or size. That
matters because pairing wrongly merges two files' histories, which is worse than
not pairing at all. The file keeps its row and its id across the move, so
everything already recorded about it survives. An unmatched half is not a
rename: a file moved out of the tree is a deletion, one moved in is a creation,
and a one-second TTL bounds the wait before it is treated that way.

**A watch-queue overflow triggers a rescan.** The kernel's event queue is finite
and a large burst — extracting an archive, checking out a branch — overflows it.
There is no way to know what was missed, so the only correct response is to
rescan. Treating it as "nothing happened" would leave the timeline quietly wrong
until the next restart.

**Log level is changeable at runtime.** `PUT /log-level` with an `EnvFilter`
directive. Reproducing a problem is usually the expensive part of diagnosing
one, and restarting to raise verbosity destroys the state that was about to be
explained. The filter is parsed before it is installed, so a bad directive is a
400 and the previous filter stays in force — a daemon that silently stopped
logging would be the worst outcome for a debugging feature.

**The UI bundle is committed.** `ui/dist/index.html` is a single self-contained
file, built by Vite and embedded with `include_str!`. The Rust build must work
with no network, so an `npm ci` cannot sit in front of `cargo build`; committing
the output keeps the Rust side hermetic and the Nix flake free of node entirely.
CI rebuilds and fails if the committed copy differs, so source and artifact
cannot drift. Verified both directions: the build is byte-reproducible, and a
one-character source edit does change the output.

## Memory

The budget's M01 requirement is that scan peak is *independent of corpus size*,
not merely under a number on one corpus. A scan accumulating per-file state
passes a single-size ceiling check on a small tree and fails on a real one.

Measured, in a fresh process per size:

| Corpus | Anonymous | Total (incl. mapped DB) |
|---|---|---|
| 64k files | 9.35 MiB | 22.9 MiB |
| 128k files | 9.36 MiB | 32.3 MiB |

**1.00x growth for a 2x corpus.** Against a 128 MiB ceiling.

Two measurement decisions matter here:

- **Anonymous RSS, not total.** The store maps up to 256 MiB of the database, and
  those pages grow with it while being file-backed and evictable — the kernel
  reclaims them under pressure and they cannot cause an OOM. Counting them
  against a memory ceiling measures the page cache rather than the daemon.
- **Corpora above ~50k files.** SQLite's page cache is anonymous and fills
  gradually as the database grows, so below that point anonymous memory rises
  with corpus size for a reason that is not accumulation. Comparing 2k against
  16k reports ~6x growth for a perfectly well-behaved scan.

## Bugs found while building this

Four, all found by tests written to the bar rather than by use.

1. **The interner cached leaf filenames.** One resident entry per file, ~430
   bytes each — about 430 MB extrapolated to a million files, against a 128 MiB
   ceiling. Found by the growth test, which is exactly the check a single-size
   ceiling assertion would have passed.

2. **The growth test itself was wrong first.** It compared two scans in one
   process, so the second read the first's allocator high-water mark. It
   reported 40x growth that was pure artifact — while the real linear growth
   above sat underneath it, unnoticed. Each size now runs in a fresh subprocess.
   A measurement that produces a dramatic number is not thereby a correct one.

3. **A killed scan lost files permanently.** `files` rows were upserted
   immediately but their events only committed at the next batch flush, so a
   `kill -9` between the two left file rows with no event. A rescan then sees
   those files as unchanged — size and mtime already match — and emits nothing.
   The file sits in the store, absent from history, forever, with no error and
   no way for a user to notice. Measured: 305 orphaned rows on an 8k-file
   corpus, converging to 7698 of 8000 files after a rescan. Each batch now
   commits its upserts and events in one transaction; after the fix the same
   kill leaves zero orphans and a rescan reaches exactly 8000.

4. **`/log-level` accepted an unbounded body.** Found by the DAST pass rather
   than by a scanner rule: a 2 MB filter string returned 200. `EnvFilter` takes
   a bare word as a target name, so an arbitrarily long string parses and is
   then held resident for the life of the process — an unauthenticated caller
   growing the daemon's footprint against a 32 MiB ceiling, one request at a
   time. Now bounded twice: a 64 KiB body limit on the whole router, so no
   future route can forget it, and a 1 KiB cap on the filter itself.

The third is the one worth remembering. It was invisible in every functional
test — scans worked, rescans worked, the timeline rendered — and only a real
`kill -9` against a real process exposed it. A dropped connection would not
have: unwinding runs SQLite's cleanup, which is the difference between testing
our error handling and testing the database's durability guarantees.

## Security

The timeline API is the first HTTP surface in the roadmap, so it is the first
milestone where DAST applies — the testing bar scopes it to the three
milestones that have a server rather than listing it against every one.

ZAP baseline and nuclei run against a live daemon holding real events, because a
scanner pointed at an empty timeline passes trivially. What the pass is looking
for is not "no auth" — the API binds loopback and is unauthenticated by design,
a documented pairing — but the accidental kind of finding. Probed directly as
well as by the scanners:

| Probe | Result |
|---|---|
| Malformed and hostile query params (SQL-shaped, null bytes, traversal, huge integers) | 400, no 5xx, no hangs |
| Reflected input in error responses | None — errors are fixed `&'static str`, so nothing is echoed |
| Path traversal on the fallback route | 404 with a fixed body |
| Response headers | No `Server` banner, no version, no paths |
| Oversized request body | **Was 200 — now 413.** See *Bugs found* |

`.zap/rules.tsv` records which ZAP alerts are accepted and why, one line each.
Every ignore is a decision with a reason rather than a way to get a green run —
and each is annotated with what makes it stop holding, which for most of them is
the bind address widening beyond loopback.

## Deliberately not done

- **No content hashing.** `files.content_hash` exists in the schema and is always
  NULL. Hashing every file on every scan is expensive and buys nothing until CAS
  at M09; the column is there so adding it later is not a migration of the
  events table.
- **No metadata beyond size and mtime.** Document properties are M02a, which
  extends this timeline rather than replacing it.
- **The UI polls.** Five seconds, no websocket. A push channel is worth it when
  there is something to push that a poll cannot cover.
- **No auth, still.** The API binds loopback, which is the honest pairing for an
  unauthenticated surface — and `PUT /log-level` is only acceptable because of
  it. Widening the bind address needs auth first.

## Next: M02a

Deterministic metadata extraction — PDF text, Office documents, EXIF, git — with
no LLM. It extends this timeline rather than adding a surface: each event row
expands to show what was extracted, and the timeline gains faceted filtering by
those fields. That is also where the extractor fuzz targets and the golden
corpus start, because document parsers are the highest-severity attack surface
in the project so far.

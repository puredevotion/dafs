# M00 — walking skeleton

**Delivered.** No user value, by design — this is the one milestone in the
roadmap that exists purely to make later milestones measurable.

## What it is

An end-to-end path through every layer the project will use, with one trivial
feature flowing through it, plus the full CI gate. The daemon starts, migrates
its metadata store, serves an HTTP API, shows a status page, and shuts down
cleanly on a signal. Nothing watches files, nothing indexes, nothing syncs.

## Why it exists

The testing bar and the memory budget are merge requirements for every later
milestone. Both have to exist *before* the first real code, not after:

- A memory budget retrofitted at M03 gets discovered to be unreachable once
  three milestones of design already depend on it. Asserting 32 MiB now, when
  the daemon does almost nothing, means every subsequent PR shows its own
  memory cost in isolation.
- A CI gate added later doesn't cover the code written before it. Fuzzing,
  crash-consistency, and the offline-build check all have to be habits from the
  first commit.

This is the only place in the roadmap where "infrastructure without feedback" is
the right call, and it is bounded to roughly a week for that reason.

## What shipped

| | |
|---|---|
| `dafs-daemon` | binary; startup ordering, SIGTERM/SIGINT handling, CLI |
| `dafs-api` | `/`, `/healthz`, `/readyz`, `/version`, `/metrics`, JSON 404 |
| `dafs-store` | SQLite migration runner, connection tuning, crash-consistency test |
| `dafs-alloc` | jemalloc + decay tuning, RSS measurement |
| `dafs-memtest` | spawns the release binary, asserts RSS ceilings from procfs |
| `ui/index.html` | status shell, embedded in the binary; no build step |
| `fuzz/` | `cargo-fuzz` target against the metadata store |
| `flake.nix` | package + OCI image + dev shell, for downstream consumers |
| CI | fmt, clippy, test, offline build, RSS ceiling, fuzz smoke, audit/deny, private-reference scan, `nix build` |

**Measured idle RSS: ~6 MiB against a 32 MiB ceiling** — 19% used, which matters
because M01 adds the file watcher and M03 the vector index.

## Decisions worth knowing

**jemalloc is not optional.** The budget requires RSS to *return* to the idle
ceiling after a scan, which glibc's malloc does not do on a many-thread
scan-then-idle workload — it fragments its per-thread arenas and never hands the
memory back. Tuning is compiled in via the `malloc_conf` symbol rather than
`MALLOC_CONF`, so a deployment cannot lose it by forgetting an env var. See
[`docs/memory-budget.md`](memory-budget.md).

**Bind before migrate, ready after.** The listener binds first so `/healthz`
answers during a long migration; `/readyz` stays 503 until the schema is usable.
Collapsing the two probes would make a deployment either kill a daemon
mid-migration or route traffic at an unmigrated database.

**Single-threaded runtime.** A multi-thread tokio runtime pre-spawns a worker per
core, each with a stack, which is measurable against 32 MiB for no benefit while
the daemon only serves occasional API requests. Revisit when there is concurrent
work to justify it.

**No SQLite connection in `AppState` yet.** rusqlite's `Connection` is not
`Sync`, and the right shape — a blocking thread that owns it, or a small pool —
depends on the query mix M01 introduces. Committing to the wrong shape now would
be harder to undo than adding it later.

**Migrations carry their bookkeeping row in the same transaction.** If the DDL
and the `schema_migrations` insert were separate, a crash between them would
leave a schema change with no record of it, and the next start would try to
apply it again.

**Refuse a newer schema.** A database written by a future build is an error, not
something to work around: an older binary cannot know what invariants the newer
schema relies on, and guessing risks corrupting a user's metadata.

**No frontend build step.** One static HTML file, embedded via `include_str!`.
There is nothing to render yet, and a toolchain added now would need maintaining
through five milestones before it earned anything. M01 picks a real frontend
approach against actual requirements.

**The fuzz crate is outside the workspace.** `cargo-fuzz` builds with its own
sanitizer flags and profile, which should not be inherited from — or impose
themselves on — the workspace. CI checks it still compiles, since `cargo test`
does not cover it and it would otherwise rot silently.

**No `panic = "abort"` and no `strip` in the release profile.** Cargo applies
profile settings to build-script and proc-macro binaries as well as the shipped
one. Aborting breaks libfuzzer's ability to attribute a crash to an input, and a
panic serving one API request should not take the daemon — and a user's metadata
database — down with it. Stripping belongs in the packaging step, where it
affects only the artifact actually shipped. Neither win is worth the cost here.

## Deliberately not done

- **`forbid(unsafe_code)` is `deny` in `dafs-alloc`.** The `malloc_conf` export
  needs a scoped allow and `forbid` cannot be downgraded per-item. Every other
  crate uses `forbid`.
- **No forced allocator purge in tests.** `arenas.purge` is only reachable
  through an `unsafe` call; the harness waits ~2.5s for decay instead. An
  allocator wrapper is exactly where an unsafe escape hatch gets reached for
  casually later.
- **Scan-peak and search ceilings are recorded but not asserted.** There is no
  scan before M01 and no index before M03. The constants live in
  `dafs-memtest::ceilings` so the numbers have one home.
- **No auth.** The API binds loopback, which is the honest pairing for an
  unauthenticated surface. Widening the bind address is a decision for whoever
  adds auth.

## Open: `nix build` fails in one specific environment

`nix build .#dafs` fails with:

```
could not execute process .../build-script-build (never executed)
Caused by: Permission denied (os error 13)
```

**This is not a defect in this repository.** It reproduces with a nine-line
`Cargo.toml` whose only dependency is `quote`, no custom profile, and a
three-line `default.nix` calling `buildRustPackage` — on two different machines,
with the sandbox both on and off. Any `buildRustPackage` derivation with a
build-script dependency fails the same way in that environment.

Ruled out along the way, each by testing rather than reasoning: `panic = "abort"`
in the release profile, `strip`, read-only permissions on the flake's store
source, and a `noexec` build directory. The `panic`/`strip` removals were kept
regardless, because they are right for other reasons (above), but neither was the
cause.

CI's `nix` job runs on a standard runner and is the authority on whether the
flake itself is sound. Until that reports, the flake is unverified rather than
known-good, and this note is here so nobody re-derives the same four dead ends.

## Bugs found while building this

Both were in test infrastructure, which is worth recording because it is the
part that silently passes when wrong:

1. **The readiness probe compared against a hardcoded `HTTP/1.1 200` prefix**
   while probing with HTTP/1.0. The server echoes the request version, so the
   harness timed out for 30s against a daemon that had been answering correctly
   the whole time. Now matches on the status code alone.
2. **Two sanitisation-induced test failures** in the CoreDNS move earlier in this
   session had the same shape — fixtures whose expected values no longer matched
   their inputs. The lesson taken here: assertions that encode a *format* rather
   than a *behaviour* fail for reasons unrelated to the thing under test.

## Next: M01

**Delivered** — see [`docs/m01-local-timeline.md`](m01-local-timeline.md).

Local timeline: the first milestone with user value (*"what did I work on
today?"*). Its memory requirements were the binding constraint, and
path-component interning landed there as planned, because M07's graph will
depend on path IDs and retrofitting it afterwards is far more invasive. See
[`docs/memory-budget.md`](memory-budget.md#m01--scan-and-event-path).

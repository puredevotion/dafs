# dafs

A distributed, AI-native filesystem. Local-first, content-addressed,
peer-to-peer, with files treated as part of a personal knowledge graph rather
than isolated blobs in folders.

> **Status: pre-alpha.** M00 (walking skeleton) is in: the daemon starts, migrates
> its metadata store, serves a small HTTP API, and shuts down cleanly. It does not
> yet watch, index, search, or sync anything — that starts at M01. Do not depend
> on this.

## What it is meant to be

An alternative to Dropbox / OneDrive Files On Demand / Syncthing / Seafile,
where a file is not just `name, path, size, mtime` but also its content,
metadata, relationships, timeline of events, and semantic meaning.

Design commitments, in rough order of how load-bearing they are:

- **Local-first.** The local device always owns the working state. Network
  availability is never required.
- **Your files are never modified by the AI.** Enrichment is asynchronous,
  optional, and strictly additive. Original bytes are untouched.
- **Content-addressed.** Files are chunked and identified by cryptographic
  hash (Merkle DAG), in the tradition of Git, IPFS, and BitTorrent v2.
- **Runs on modest hardware.** CPU-only inference, no GPU. The daemon targets
  **32 MB RSS idle** and ~96 MB serving search over a million files. A
  filesystem has no business using hundreds of megabytes.
- **Independent planes.** Filesystem, synchronisation, and AI are separate and
  stay separate. The AI worker is a separate process that exits when idle, so
  the steady-state cost of having AI features is zero.
- **Every peer link is mutually authenticated and encrypted** from the first
  networked milestone, not bolted on later.

## Non-goals

Cloud hosting. Enterprise multi-tenancy. Collaborative document editing.
Replacing Git. Replacing object storage.

## Platforms

| Platform | Status | Approach |
|---|---|---|
| Linux | primary target | FUSE, Rust, SQLite |
| Windows 11 | planned | Cloud Files API (native Explorer integration) |
| Android / macOS / iOS | later | — |

Development targets modest hardware deliberately: an older i7 NAS with no GPU.

## Roadmap

Built as user-value slices — every milestone should give you something you can
actually use, not infrastructure awaiting a payoff.

**Phase 1 — personal AI memory** (no sync yet; operates on your existing files,
read-only)

| | Milestone | What you get |
|---|---|---|
| M00 | Walking skeleton + CI | *(nothing — scaffolding and the test gate)* ✅ |
| M01 | Local timeline | "What did I work on today?" |
| M02a | Deterministic metadata + browse | Filter and skim by real document properties |
| M02b | Local LLM enrichment | Summaries, keywords, entities |
| M03 | Semantic search | Find things without remembering filenames |
| M04 | Version history | Never lose previous work |

**Phase 2 — distribution**

| | Milestone | What you get |
|---|---|---|
| M05 | Device identity + pairing | *(gate: no networking before this exists)* |
| M06a | One-way replication | Your files are backed up |
| M06b | Bidirectional sync | Edit anywhere |
| M07 | Knowledge graph | Navigate how things connect |
| M08 | AI assistant | Ask questions about your own files |
| M09 | Content-addressed storage | Deduplication and integrity |
| M10 | Selective sync | Control where data physically lives |

**Phase 3 — platform**

| | Milestone | What you get |
|---|---|---|
| M11a | Read-only FUSE mount | Any app can open any file, including online-only |
| M11b | FUSE write path | Full read/write filesystem |
| M12 | Windows integration | Explorer placeholders, sync state, context menu |
| M13 | Mobile | Browse, search, upload, offline pin |
| M14a | Multi-peer distribution | Faster sync from several peers |
| M14b | Erasure coding | Survives losing a device |
| M15 | Production hardening | Trustworthy enough for data you care about |

## Testing

Because this holds people's files, the bar is set higher than usual and applies
to every milestone, not just the ones where it's convenient:

- property-based tests for every invariant — example-based tests alone will not
  find the bugs that matter here
- **crash consistency**: fault injection across every write path, asserting no
  data loss and no unreadable state
- `cargo fuzz` targets for everything parsing bytes the user didn't type —
  document extractors, on-disk formats, and network-supplied peer metadata
- a **cumulative data-loss regression suite**: every bug that ever loses a byte
  earns a permanent test
- deterministic simulation over randomised event interleavings, partitions, and
  clock skew for sync convergence, with replayable seeds
- adversarial tests for prompt injection via document content
- RSS ceilings asserted in CI, so a memory regression fails a PR rather than
  being discovered in production

AI output quality (extraction accuracy, hallucination rate, retrieval
relevance) is measured against a golden corpus of **real** documents. That
corpus is not in this repository and cannot be — it's referenced by content
hash from a manifest, so published numbers name exactly which revision produced
them.

## Building

```sh
cargo build --release -p dafs-daemon
./target/release/dafs             # serves http://127.0.0.1:7878
```

Or with Nix:

```sh
nix build .#dafs                  # the daemon
nix run .#docker                  # streams an OCI image to stdout
nix develop                       # dev shell: clippy, audit, deny, llvm-cov
```

The API binds loopback by default and is unauthenticated. That pairing is
deliberate — widening the bind address is a decision for whoever adds auth.

### Layout

| Crate | |
|---|---|
| `dafs-daemon` | the binary: startup ordering, signal handling, CLI |
| `dafs-api` | HTTP surface and the embedded UI shell |
| `dafs-store` | SQLite schema, migrations, connection tuning |
| `dafs-alloc` | allocator selection and RSS measurement |
| `dafs-memtest` | RSS ceiling assertions against the release binary |

### CI runs in two places, deliberately

The same suite runs as GitHub Actions (`.github/workflows/ci.yml`) and as a
Dagger module (`ci/`). GitHub is free and unmetered for a public repo and gives
every fork and external PR CI with no setup, so it stays as the gate. The Dagger
module means none of that is load-bearing — the identical checks run on any
machine with Dagger, so the project does not depend on one forge remaining free,
available, or willing to host it.

```sh
cd ci
dagger call check        --source=..              # everything, in parallel
dagger call test         --source=..
dagger call hermetic     --source=..              # builds with the network off
dagger call rss-ceiling  --source=..              # release binary, real RSS
dagger call fuzz         --source=.. --seconds=60
dagger call image        --source=..              # OCI container
```

A disagreement between the two is a real environment-dependency bug worth
knowing about. Keep them in step: a check added to one belongs in the other.

Release builds, artifacts, SBOM, and provenance are the one genuinely
GitHub-specific part, in `.github/workflows/release.yml`, on version tags only.

### Hermetic builds are a requirement, not an aspiration

The tree must build and test with no secrets, no network, and no private DNS.
CI enforces it by vendoring dependencies and then building with `--offline`, so
anything still reaching out fails the build. Code that can't build in a clean
container can't be deployed by anyone but its author, which would defeat the
point of this being public.

### Memory

The daemon's idle RSS ceiling is **32 MiB**, asserted in CI against the release
binary from M00 onward, and exported as `dafs_resident_bytes` on `/metrics` so
it is observable in a running deployment rather than only in tests. Current
measured idle: **~6 MiB**. See [`docs/memory-budget.md`](docs/memory-budget.md)
for the full budget, the allocator tuning it depends on, and which technique is
due at which milestone.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). This repository is the source of truth —
issues and pull requests here are the real thing, not a mirror of something
private.

## Licence

[MIT](LICENSE).

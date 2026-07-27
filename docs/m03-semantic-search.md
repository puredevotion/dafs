# M03 — Semantic search

**In progress.** This document covers the foundation that shipped —
vector storage and an embeddings client — not the whole milestone.
`docs/roadmap-and-design-review.md` calls M03 "the first 'wow' milestone";
that still requires the daemon-side worker, the search API, and a UI surface,
none of which are in this slice. See *Next*.

## What it is

Semantic search over the same corpus M01/M02a/M02b already observe,
extract, and (optionally) enrich: embed each file's extracted text into a
vector, store it, and answer "find files like this query" by nearest-neighbour
search instead of keyword matching. Same opt-in posture as M02b's LLM
enrichment — nothing embeds anything until a `--llm-embedding-model` is
configured (not wired to a CLI flag yet — see *Next*) — and the same
`AI output must never modify original files automatically` rule holds
structurally here for the same reason it does in `dafs-enrich`: nothing in
this slice has a code path from a vector, or from search results, back to a
write against anything the daemon observes.

## Vector engine: sqlite-vec, not Qdrant or Turso/libSQL

`docs/roadmap-and-design-review.md` §7 item 2 left this open explicitly:
"Benchmark at M03 against the memory ceiling, decide then." Decided for
[`sqlite-vec`](https://github.com/asg017/sqlite-vec) over the two other
real candidates:

- **Qdrant** is a full vector database designed to run as its own service —
  not something meant to be embedded as a library the way this crate embeds
  pdfium. Running it would mean a second always-on process with its own
  resident memory, directly contradicting §2 item 9's single-binary,
  no-daemon-managed-subprocess vendoring rule and the §8 memory budget (its
  own footprint sits *on top of* dafs's, not instead of it).
- **Turso/libSQL** embeds fine, but it's a fork of SQLite's own engine —
  adopting it would mean replacing the `rusqlite`/bundled-SQLite foundation
  M00–M02b's shipped schema already sits on, not adding alongside it.
- **sqlite-vec** is a C extension that loads into the *same* SQLite
  connection `dafs-store` already opens. It vendors as cleanly as pdfium did
  (§2 item 9) — better, actually: the upstream crate compiles
  `sqlite-vec.c` (a single ~10k-line amalgamation) with `cc` at build time
  and links it straight into the binary, with no shared library to extract
  to a cache file on first run the way pdfium's does. `cargo build --offline`
  already covers it.

## What shipped

| | |
|---|---|
| `dafs-vecstore` | New crate. One function, `register()`, holding the one `unsafe` FFI call in this workspace (`sqlite3_auto_extension`, registering `sqlite-vec`'s `vec0` module against every connection this process opens from then on). `deny(unsafe_code)` with one scoped `#[allow]`, mirroring `dafs-alloc`'s own precedent for jemalloc's `malloc_conf` registration — same reasoning: `forbid` can't be downgraded per-item, `deny` can. |
| `dafs-store::embeddings` | `embedding_queue` (migration 5, same durable-queue shape as `enrichment_queue`) and `embedding_config` (records which model/dimensionality `file_embedding` was created for). `ensure_table`, `store`, `search`, `enqueue`, `pending`, `record_attempt` — mirrors `enrichment.rs`'s shape throughout. |
| `dafs_store::open`/`open_in_memory` | Now call `dafs_vecstore::register()` before opening a connection, so every connection this workspace ever opens — including its own tests — can see `vec0`, without any caller needing to know the crate exists. |
| `dafs-enrich::embed` | A second entry point alongside `enrich`, against the same OpenAI-compatible endpoint's `/embeddings` route. `Config::embedding: Option<EmbeddingConfig>` — a distinct opt-in from chat enrichment's `model`, because an embedding model is typically a different model with a different, fixed output width. |

## Decisions worth knowing

**`file_embedding` is not in the static `MIGRATIONS` list.** Every other
table in `dafs-store` is identical across every deployment. `file_embedding`
can't be: a `vec0` table's vector column is declared with a fixed width
(`float[N]`) at `CREATE VIRTUAL TABLE` time, and `N` is whatever dimensionality
the deployment's configured embedding model happens to produce — a
per-installation choice a shared, forward-only, "never edit a shipped
migration" list has no way to express. `embeddings::ensure_table` creates it
on demand instead, the first time embeddings are configured, and records the
chosen model/width in `embedding_config` (which *is* a static migration —
its shape is universal even though its contents are deployment-specific) so
a later run configured differently fails loudly rather than silently
writing mismatched-width blobs into an existing column.

**Vectors are raw little-endian bytes, not JSON.** `sqlite-vec`'s blob path
(`sqlite-vec.c`'s `fvec_from_value`) accepts a BLOB whose length is a
multiple of 4, memcpy'd directly into an `f32` buffer — no header, no
endianness conversion. Sound on every platform this workspace targets
(x86_64/aarch64, both little-endian), and it avoids a text round-trip on
both the write path and the query path.

**`vec0` doesn't support `ON CONFLICT`/UPSERT.** Found by the first run of
`embeddings::store`'s test suite, not by reading SQLite's virtual-table docs
in advance: SQLite returns "UPSERT not implemented for virtual table" for
one. `store` does delete-then-insert inside the same transaction instead,
same atomicity guarantee, and there's a test
(`re_storing_overwrites_rather_than_duplicates`) asserting the row count
stays at one across a second `store` call for the same file.

**Embeddings are a distinct opt-in from chat enrichment, not a reused
`model` field.** `Config::embedding: Option<EmbeddingConfig>` bundles a
model name with its dimensionality — deliberately one `Option`, not two
independent ones, because a model name with no known width and a width with
no named model are both useless alone, and two separately-optional fields
that must agree or neither work is a footgun `EmbeddingConfig` closes.
`dimensions` is asked for up front, not discovered from a live response,
because `file_embedding`'s column width has to be known *before* the first
embedding is ever requested — it's what `ensure_table` needs, not something
learned from calling `embed` once and measuring.

## Memory

Not yet measured. `dafs-vecstore::register()` itself holds no state beyond
a `Once` flag. `file_embedding`'s resident cost — at 1M documents,
`docs/memory-budget.md` §8.3 is explicit that this is a *functional*
requirement, not a later optimisation: "full-float resident vectors cannot
meet the ceiling ... the quantize-and-rescore path is the design." This
slice stores full-precision `f32` vectors with no quantization yet — see
*Deliberately not done*. The `dafs-memtest` RSS-ceiling scenario for M03
(§8.4: "M03 must hit 96 MB steady-state") is not added in this slice either.

## Bugs found while building this

- The `ON CONFLICT`/UPSERT-on-a-virtual-table failure above, caught by
  `embeddings::store_then_search_finds_the_nearest_neighbour` and two
  sibling tests on the very first run — never shipped, since it's the kind
  of thing a test written to the bar catches before a review does.

## Security

**The FFI registration is tested for correctness, not just soundness by
inspection.** `dafs-vecstore`'s own test suite proves `register()` actually
makes `vec0` usable (`vec0_is_usable_after_register`), is safe to call more
than once (`register_is_idempotent` — relevant because a daemon test
harness or a future in-process restart could call it twice), and that a
created table gives correct nearest-neighbour results
(`a_vec0_table_can_be_created_and_queried` asserts the identical vector is
its own nearest neighbour, not just that the query doesn't error).

**A wrong-width vector is a hard error, at two layers.** `dafs_enrich::embed`
checks the returned vector's length against `EmbeddingConfig::dimensions`
and returns `EnrichError::UnexpectedDimensions` rather than passing a
mismatched vector further down the pipe. `dafs_store::embeddings::store`
doesn't duplicate that check — `vec0` itself rejects a wrong-length blob at
the C layer — and `storing_a_wrong_width_vector_is_an_error_not_a_silent_
truncation` proves that empirically rather than asserting it from reading
`sqlite-vec.c`.

**Prompt injection is not a new surface here.** `embed`'s input is the same
already-extracted `body_text` M02b's `enrich` already sends to the same
class of endpoint; `dafs-enrich`'s existing hostile-server test suite
(`crates/dafs-enrich/tests/hostile_server.rs`) already covers injected
instructions in input text reaching the wire only as an escaped JSON string
value, which applies unchanged to `embed`'s request body.

## Deliberately not done

- **No CLI flags, no daemon worker, no `/search` API route, no UI.**
  `Config::embedding` is always `None` in `dafs-daemon::main` today — see
  *Next*.
- **No binary quantization.** `docs/memory-budget.md` §8.3's oversample-and-
  rescore design (1-bit-per-dimension resident, full floats paged from disk)
  is what makes the 96 MB steady-state ceiling reachable at 1M documents;
  this slice stores full-precision floats only. Flagged, not silently
  skipped: the memory work in *Next* is required before this milestone is
  memory-budget-complete, not a nice-to-have.
- **No `cargo fuzz` target for the embeddings response parser.**
  `dafs_enrich::embed` parses network-supplied JSON the same way `enrich`
  does, and `enrich`'s parser has no fuzz target either — this is an
  existing gap in M02b, not a new one, but it now applies to a second
  parser.
- **No golden-corpus recall measurement.** §6 item 3's external, NAS-hosted
  corpus is what a real recall number needs; nothing in this slice
  references it.

## Next

In roughly the order §5.2's "ship something a user can use" bar wants:

1. `--llm-embedding-model` / `--llm-embedding-dimensions` CLI flags (mirroring
   `--llm-model`'s `requires` chain), and wiring `dafs_enrich::embed` into an
   `embed_worker.rs` that drains `embedding_queue` — mirrors `enrich_worker.rs`
   almost exactly.
2. A `/search` route on `dafs-api`'s router: embed the query text through the
   same client, call `dafs_store::embeddings::search`, join back to the
   timeline for display. Following `AppState`'s existing pattern
   (`TimelineReader` as a trait object the daemon supplies), this should be a
   new trait rather than a direct `dafs-api` → `dafs-enrich`/`dafs-store`
   dependency.
3. A search box in the timeline UI.
4. The `dafs-memtest` RSS scenario for M03, and the binary-quantization work
   that's a prerequisite for it to pass at any real corpus size.
5. A `cargo fuzz` target covering both `enrich`'s and `embed`'s response
   parsers.

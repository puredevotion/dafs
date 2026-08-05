# M03 — Semantic search

Semantic search over the same corpus M01/M02a/M02b already observe,
extract, and (optionally) enrich: embed each file's extracted text into a
vector, store it, and answer "find files like this query" by nearest-neighbour
search instead of keyword matching, end to end — CLI flags, a daemon
worker, an API route, and a search box in the timeline UI. Same opt-in
posture as M02b's LLM enrichment — nothing embeds anything until
`--llm-embedding-model`/`--llm-embedding-dimensions` are configured — and the
same `AI output must never modify original files automatically` rule holds
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
| `dafs-store::embeddings` | `embedding_queue` (migration 5, same durable-queue shape as `enrichment_queue`) and `embedding_config` (records which model/dimensionality `file_embedding` was created for). `ensure_table`, `store`, `search`, `enqueue`, `pending`, `record_attempt` — mirrors `enrichment.rs`'s shape throughout. `search` is two-stage — see *Binary quantization* below. |
| `dafs_store::open`/`open_in_memory` | Now call `dafs_vecstore::register()` before opening a connection, so every connection this workspace ever opens — including its own tests — can see `vec0`, without any caller needing to know the crate exists. |
| `dafs-enrich::embed` | A second entry point alongside `enrich`, against the same OpenAI-compatible endpoint's `/embeddings` route. `Config::embedding: Option<EmbeddingConfig>` — a distinct opt-in from chat enrichment's `model`, because an embedding model is typically a different model with a different, fixed output width. Response parsing lives in a pure `parse_embedding_response` (and `enrich`'s own in `parse_chat_response`), split out from the network call specifically so a fuzz target can drive them directly — see *Fuzzing* below. |
| `--llm-embedding-model`/`--llm-embedding-dimensions` | CLI flags on `dafs-daemon`, mirroring `--llm-model`'s `requires` chain: both required together, and (transitively, through `--llm-base-url`'s own chain) only meaningful alongside chat enrichment — there is no embeddings-only configuration today. |
| `dafs-daemon::embed_worker` | Drains `embedding_queue` via `dafs_enrich::embed`, storing results through `dafs_store::embeddings::store`. Mirrors `enrich_worker` closely; calls `ensure_table` once at startup rather than per-file, and has no `requeue_stale` equivalent — a model/width change is a `DimensionMismatch`, not a version bump to transparently migrate through. `extract_worker::maybe_enqueue_embedding` enqueues a successfully extracted file the same way `maybe_enqueue_enrichment` already did, reusing the same length floor. |
| `dafs-api::search` | A `SearchStore` trait and `/search` route, deliberately **not** a method on `TimelineStore` — answering a query means embedding it (a network call) before anything reaches `dafs_store::embeddings::search`, a different shape of work from every other timeline query, and search is independently optional from the timeline store itself. |
| `dafs-daemon::store_adapter::SqliteSearch` | Bridges `dafs_enrich::embed` and `dafs_store::embeddings::search`/`dafs_store::events::latest_for_file_ids` to `SearchStore`. Its own connection (not a share of `SqliteTimeline`'s) — the embed call is a blocking network round trip that must not sit behind, or hold up, an unrelated timeline request. |
| `dafs_store::events::latest_for_file_ids` | New query: the most recent event for a specific set of file ids, in the caller's order — what a ranked search result needs (each hit's current display state) that the existing `timeline` query (paginated, newest-first) doesn't provide. |
| UI search box | `ui/src/api.js`'s `fetchSearch`, a `<form id="search-form">` in `index.html`, and `main.js` wiring: submitting replaces the timeline view with ranked hits (a distance badge in place of the created/modified/deleted/renamed kind badge); clearing goes back to the timeline. A 503 (embeddings not configured) gets its own message rather than the generic failure text. |

## Binary quantization

`docs/memory-budget.md` §8.3 is explicit that this is a *functional*
requirement, not later tuning: "full-float resident vectors cannot meet the
96 MiB ceiling at 1M documents" — not because of the float table's on-disk
size (`dafs_store::tune`'s small-page-cache/large-mmap-window setup already
makes those pages evictable), but because `vec0` with no ANN index answers a
query with a brute-force scan against *every* row's full vector.

`file_embedding_bin` — a second `vec0` table, `bit[N]`, one bit per
dimension (that dimension's sign) — exists to make that scan cheap enough to
stay resident: a 32× reduction over the full-float table. `search` queries
it first by Hamming distance for an oversampled candidate set (4× the
requested `k`, `docs/memory-budget.md`'s cited "2-4x" range), then reads
*only* those candidates' real vectors out of `file_embedding` to rescore by
true Euclidean distance for the final ranking — the full float table is
never scanned end to end. `search_rescores_candidates_by_true_distance_not_hamming_distance`
(`dafs-store/src/embeddings.rs`) proves the two-stage design actually
changes the ranking, not just that it doesn't error.

Bit-packing matches `sqlite-vec.c`'s own unpacking exactly (bit `i` of
dimension `i` lives in byte `i / 8` at bit position `i % 8`, LSB first), and
`file_embedding_bin`'s declared width is `dimensions` rounded up to the next
multiple of 8 (`vec0` requires a `bit[N]` column's `N` to clear that) — the
extra padding bits are always zero on every vector this module writes, so
they contribute nothing to any Hamming distance regardless of whether the
configured dimensionality happens to already be byte-aligned.

## Decisions worth knowing

**`file_embedding`/`file_embedding_bin` are not in the static `MIGRATIONS`
list.** Every other table in `dafs-store` is identical across every
deployment. Both of these can't be: a `vec0` table's vector column is
declared with a fixed width at `CREATE VIRTUAL TABLE` time, and the width is
whatever dimensionality the deployment's configured embedding model happens
to produce — a per-installation choice a shared, forward-only, "never edit a
shipped migration" list has no way to express. `embeddings::ensure_table`
creates both on demand instead, the first time embeddings are configured,
and records the chosen model/width in `embedding_config` (which *is* a
static migration — its shape is universal even though its contents are
deployment-specific) so a later run configured differently fails loudly
rather than silently writing mismatched-width blobs into an existing column.

**Vectors are raw little-endian bytes, not JSON.** `sqlite-vec`'s blob path
(`sqlite-vec.c`'s `fvec_from_value`) accepts a BLOB whose length is a
multiple of 4, memcpy'd directly into an `f32` buffer — no header, no
endianness conversion. Sound on every platform this workspace targets
(x86_64/aarch64, both little-endian), and it avoids a text round-trip on
both the write path and the query path.

**`vec_bit(...)` has to wrap a bit-column parameter explicitly.** Found by
the first run of `embeddings::store`'s test suite against the new bit
table, not by reading `sqlite-vec.c`'s docs in advance: a bare blob
parameter carries no type subtype and is parsed as float32 by default
(`vector_from_value`'s own fallback) regardless of the destination column's
declared type — inserting a quantized blob without `vec_bit(...)` failed
with a float32-length error even though the *destination* column is `bit[N]`.
`store`/`search` wrap every bit-column parameter in `vec_bit(...)`
accordingly.

**`vec0` doesn't support `ON CONFLICT`/UPSERT.** Found by the first run of
`embeddings::store`'s test suite, not by reading SQLite's virtual-table docs
in advance: SQLite returns "UPSERT not implemented for virtual table" for
one. `store` does delete-then-insert inside the same transaction instead,
same atomicity guarantee, for both `file_embedding` and `file_embedding_bin`
in lockstep — a rowid present in one but not the other would make `search`'s
rescore step either miss a real candidate or rescore one with no float
vector left to rescore against.

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

**`/search` is a separate trait from `/events`, not a method on
`TimelineStore`.** Answering a search query means embedding the query text
(a network call) before `dafs_store::embeddings::search` is even reachable —
a different shape of work from every other timeline-store method, none of
which touch the network — and search is independently optional (a daemon
can watch and extract with no LLM endpoint at all). Folding it into
`TimelineStore` would mean every implementor, including the HTTP layer's own
test fake, grows a method about embeddings whether or not it has any.

## Memory

`crates/dafs-memtest/tests/m03_search_rss.rs` asserts
`docs/memory-budget.md` §8.4's 96 MiB M03 ceiling against a real release
binary with embeddings configured and `/search` serving — measured at
12.54 MiB on a 20-file corpus, comfortably inside budget. That scale proves
the embedding worker's/search route's own shape doesn't regress the
baseline, the same thing `extraction_queue_rss.rs` proves for M02a's
extraction queue — it is **not** a measurement at the 1M-document scale the
96 MiB ceiling is actually about, which needs the golden corpus §6 item 3
describes (see *Next*). Binary quantization (above) is what makes that scale
reachable in principle; nothing in this slice runs it at that scale to
confirm.

## Bugs found while building this

- The `ON CONFLICT`/UPSERT-on-a-virtual-table failure above, caught by
  `embeddings::store_then_search_finds_the_nearest_neighbour` and two
  sibling tests on the very first run — never shipped, since it's the kind
  of thing a test written to the bar catches before a review does.
- The `vec_bit(...)` subtype requirement above, caught by the first run of
  the binary-quantization test suite against `file_embedding_bin` — also
  never shipped.

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

**`/search`'s query string is bounded before it reaches an embedding call.**
`MAX_SEARCH_QUERY_LEN` (4,096 bytes) caps `q` the same way `MAX_FACET_FILTER_LEN`
already caps `/events`'s facet filters — a query string has no body-size
limit the way a JSON body does, so an unbounded `q` would let an
unauthenticated caller force an arbitrarily large embedding request.

**Prompt injection is not a new surface here.** `embed`'s input is the same
already-extracted `body_text` M02b's `enrich` already sends to the same
class of endpoint; `dafs-enrich`'s existing hostile-server test suite
(`crates/dafs-enrich/tests/hostile_server.rs`) already covers injected
instructions in input text reaching the wire only as an escaped JSON string
value, which applies unchanged to `embed`'s request body.

**`enrich`'s and `embed`'s response parsers are fuzzed.**
`fuzz/fuzz_targets/enrich_response.rs` and `embed_response.rs` drive
`parse_chat_response`/`parse_embedding_response` directly on arbitrary
(lossily-UTF-8-converted) bytes — the network-supplied-JSON gap the
milestone's original slice flagged as still open (see the previous
*Deliberately not done* note this closes).

## Faceted search

`SearchStore::search` takes a [`SearchFilters`](../crates/dafs-api/src/search.rs)
alongside the query and limit — the same five columns `/events`' own facet
filters narrow (`doc_type`/`author`/`language`/`git_branch`/`classification`),
exact-match, absent means no filter. The UI's existing facet dropdowns (built
for the timeline) now narrow a search the same way: changing one while
searching re-runs the current query rather than dropping back to the
timeline, since a facet is just as meaningful against ranked results as
against a raw event page.

**Filtering happens after ranking, not before.** A `vec0` query only ever
knows about the embedding column — there is no way to ask it for "nearest
neighbours where `doc_type = pdf`" directly. `SqliteSearch::search` instead
pulls a larger candidate pool from the vector search (`FACET_FILTER_OVERSAMPLE`
× `limit`, capped at `MAX_FACET_CANDIDATE_LIMIT`) when any filter is set, then
filters and truncates in Rust. This is a **second**, independent oversampling
layer on top of `dafs_store::embeddings::search`'s own internal Hamming/
rescore oversampling (see *Binary quantization* above) — the two solve
different problems and neither substitutes for the other.

**A filtered search can return fewer than `limit` hits.** A restrictive
filter can exclude most of the oversampled candidate pool, and there is no
retry-with-a-bigger-pool loop to guarantee an exact count — the alternative
is scanning arbitrarily deep into the ranked list, which trades an
unbounded query cost for a count guarantee nothing here actually needs.
`search_applies_facet_filters_to_vector_search_candidates`
(`dafs-daemon/src/store_adapter.rs`) proves the filter actually overrides
ranking (a nearer-but-wrong-`doc_type` hit loses to a farther-but-matching
one), not just that it doesn't error.

## Deliberately not done

- **No golden-corpus recall measurement.** §6 item 3's external, NAS-hosted
  corpus is what a real recall number — and a real test of binary
  quantization's ~2% recall cost at scale — needs; nothing in this
  milestone references it. See *Next*.
- **No result highlighting or re-ranking control.** The search box takes a
  query, optionally narrowed by facet, and shows ranked results with a
  distance badge; there is no "why did this match" explanation and no way to
  adjust the ranking itself. Neither blocks "a user can search their files
  by meaning, narrowed the same way the timeline already narrows," which is
  the bar this pass was set against.

## Next

- The golden-corpus recall measurement (§6 item 3) and, once it exists, an
  `dafs-memtest`-style scenario at real (1M-document) scale to confirm
  binary quantization actually holds the 96 MiB ceiling there, not only at
  the 20-file scale this milestone's own test runs at.

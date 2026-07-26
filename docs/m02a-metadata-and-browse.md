# M02a — deterministic metadata and browse

**Delivered.** Every event in the timeline can now say *what* the file
actually is — title, author, language, page/word counts, EXIF, git facts —
without an LLM anywhere in the path.

## What it is

The same `dafs --watch ...` as M01. No new flag turns this on and none turns
it off: every file the observer already knows about is queued for extraction,
automatically, by a background worker the daemon owns. What changes is what
comes back — `GET /events` rows gain metadata fields when extraction has
finished for that file, and a new `GET /facets` endpoint answers "what
distinct authors/languages/branches exist" for building filter dropdowns
without pulling full history.

```sh
dafs --watch ~/Documents --watch ~/src     # unchanged
open http://127.0.0.1:7878                 # events now carry doc_type, title, ...
```

Nothing here is a summary, a keyword, or a classification. Those need a model
and arrive at M02b; everything shipped here is mechanically derivable from a
file's own bytes.

## What shipped

| | |
|---|---|
| `dafs-extract` | Sniffing plus docx/xlsx/pptx (zip + XML), EXIF (JPEG/TIFF), and repo-level git facts. No PDF, no LLM. |
| `dafs-pdf-worker` | Standalone PDF text extraction via `pdfium-render`, run as a one-shot child process, its Pdfium `.so` vendored and embedded — no Nix, no network. |
| `dafs-store::metadata` | `file_metadata` (replaceable, keyed by `file_id`) and the durable `extraction_queue` that feeds it. |
| `dafs-api` | `GET /events` extended with metadata fields (omitted when absent, not `null`); new `GET /facets`. |
| `ui/` | Facet filter dropdowns (type, author, language, branch) that degrade gracefully against an older daemon's 404. |
| `dafs-tui` | A bracketed `[doc_type]` tag appended to each event line when metadata is present. |
| fuzz | Four new targets — `docx_extract`, `xlsx_extract`, `pptx_extract`, `exif_extract` — alongside M00/M01's `migrations`/`paths`. |
| tests | 250 total, workspace-wide, including a real `kill -9` crash-consistency test for the extraction queue and an end-to-end pdfium-child-process test. |

## Decisions worth knowing

**No summary, keywords, entities, or classification — on purpose.** The
roadmap's own milestone table (`docs/roadmap-and-design-review.md`, the
M02a/M02b row) splits metadata into two milestones specifically so the
riskiest unknown in the whole project — a small LLM on CPU, no GPU — doesn't
gate shipping the deterministic half. `dafs-extract`'s module docs state the
boundary directly: "No LLM anywhere in this crate — summary, keywords,
entities, and classification are M02b's job and arrive as a separate crate
later." Every field in `Extraction` is something a parser can compute from
bytes alone.

**Git facts are repo-level, not per-file blame.** `git.rs`'s module docs spell
out the cost that ruled per-file blame out: "last commit that touched this
exact path" is an O(commits) walk per file, which at a million-file corpus is
exactly the kind of cost the memory and scan-time budgets exist to rule out.
What ships instead — which repo a file lives in, and that repo's HEAD
(branch, commit, author, time) — is cheap and applied uniformly to every file
under the root. Per-file history is left for a later milestone if it turns
out to matter enough to pay for.

**The extraction queue is durable and poison-file-capped.** `extraction_queue`
survives restarts (a crash mid-processing just leaves the row for the next
pass), and `record_attempt` is written *before* extraction is attempted, not
after — so a crash during the extractor call itself still counts as a used
attempt. `MAX_ATTEMPTS` (5) stops a file that reliably wedges the extractor
from being retried forever; it stays visible in the table for diagnosis, just
no longer offered to the dispatcher.

**PDF runs in a separate process, not in-process.** `pdfium-render` wraps
Pdfium, a C++ library parsing the same untrusted bytes every other extractor
here parses — except C++ is not memory-safe, so a malformed PDF can segfault
the whole process, and `catch_unwind` (what protects every other extractor)
cannot catch that. `dafs-pdf-worker` is the isolation boundary: one process
per PDF, killed on timeout rather than trusted to exit on its own. The
daemon's extraction worker treats the whole child process — clean exit,
crash, or silent vanishing — as the unit of failure.

**pdfium is vendored and embedded, not fetched via Nix.** This crate's own
history is the reason `docs/roadmap-and-design-review.md` §2 gained a new
locked decision (item 9) mid-milestone: pdfium was first wired up via a Nix
`buildInputs` addition, then corrected, because a Nix requirement to produce
a *working* `dafs` binary contradicts "standalone by default." The fix —
vendor the `.so`, embed it with `include_bytes!`, extract it to a cache file
on first run — is now the documented project-wide principle for every future
native/binary dependency, with two named exceptions (LLM weights: too large
to vendor at build time; FUSE/CFAPI: cannot be vendored at all). As that
section puts it: "The shared library is vendored into the repo and embedded
into the binary... the same 'commit the artifact, keep the Rust build
hermetic' trade already made for `ui/dist/index.html` and for SQLite via
rusqlite's `bundled` feature."

**`gix`'s MPL-2.0 dependency is an accepted, documented exception.**
`deny.toml` allows `MPL-2.0` with its own reasoning recorded rather than
silently widened: "Weak, file-level copyleft rather than GPL-style: it only
obliges sharing modifications to files actually licensed under it, and this
project never modifies or vendors uluru's source, only links against it
unmodified. Added deliberately for `uluru`, pulled in unconditionally by
gix-pack (M02a's git metadata extractor, `crates/dafs-extract`) for its
packfile object cache — there is no gix feature flag that avoids gix-pack,
since reading packed objects is not optional for a real repository."

## Memory

Measured with `crates/dafs-memtest`, release binary, RSS from
`/proc/<pid>/statm`, decay settled (2.5s past `dirty_decay_ms`):

| Scenario | RSS | Ceiling | Used |
|---|---|---|---|
| Idle, nothing watched (M00's `idle_rss_is_within_budget`) | ~8.6 MiB | 32 MiB | ~27% |
| Idle, after a 20-file text corpus fully extracts and the queue drains (new `idle_rss_after_the_extraction_queue_drains_is_within_the_same_budget`) | ~9.5–9.8 MiB | 32 MiB | ~30% |

Deterministic extraction costs roughly **1 MiB** over the plain observer at
this corpus size and comfortably holds the *existing* 32 MiB idle ceiling —
no new, higher ceiling was needed, matching the expectation that a queue of
CPU-bound-but-bounded parsers (not an LLM) shouldn't carry LLM-shaped memory
cost. Consistent across eight consecutive runs.

The queue-drain scenario polls a new `/metrics` gauge,
`dafs_extraction_queue_depth` (`dafs_store::metadata::queue_depth`, wired
through `TimelineStats`/`SqliteTimeline` the same way `dafs_events_total` and
`dafs_files_known` already were), rather than a fixed sleep. It combines that
with `dafs_files_known` reaching the corpus size before trusting a `0`
reading — see *Bugs found* below for why a bare `queue_depth == 0` is
ambiguous on its own.

## Bugs found while building this

1. **`.gitignore`'s blanket `*.so` rule silently excluded the vendored pdfium
   library.** Found and fixed during the Nix-to-embedded pdfium correction
   (see *Decisions worth knowing*): a build-hermeticity fix that itself
   almost broke hermeticity, because the file it depended on committing
   would never have been in the repository at all. `.gitignore` now carries
   an explicit exception, `!crates/dafs-pdf-worker/vendor/*.so`, with a
   comment pointing at why.

2. **`dafs-memtest` measured the wrong process's RSS, silently, since M01a.**
   `--detach` defaulted on when M01a shipped it (a spawned daemon forks,
   redirects its own stdio, and its *parent* — the pid `Command::spawn`
   returns — exits immediately). `dafs-memtest`'s `Daemon::spawn` was never
   updated to pass `--detach false`, so every RSS assertion since had been
   reading `/proc/<already-exited-parent-pid>/statm`: a real read that
   succeeds and reports near-zero RSS, so the ceiling assertion always
   "passed" without ever measuring the actual daemon. Found while building
   this milestone's own extraction-queue memtest scenario, which reported an
   implausible 0.00 MiB — the pre-existing idle test gave no such sign,
   because near-zero silently satisfies a ceiling check. Fixed by passing
   `--detach false` from every `Daemon::spawn*` constructor, matching the
   convention `extraction_crash_consistency.rs` and `pdf_extraction.rs`
   already used for their own daemon-spawning tests. Idle RSS now reads a
   real ~8.6 MiB instead of 0.00 MiB.

3. **The extraction-queue-drained signal is ambiguous by itself.**
   `dafs_extraction_queue_depth == 0` means both "everything finished" and
   "nothing has been enqueued yet" — the scan populating `files`/`events`
   and `requeue_stale` populating the queue are two sequential steps on the
   observer thread, so a poll landing in that gap reads a falsely-drained
   queue. Not a shipped defect (nothing in production depends on this
   signal), but a real trap for the new memtest scenario, closed by
   requiring `dafs_files_known` to have caught up to the corpus size first,
   and by placing the one assertion that actually depends on completion
   (a `/facets` check) after the existing decay-settling sleep rather than
   immediately off the back of the poll.

## Security

Deterministic extraction is the first code in this project parsing
attacker-controlled document *content*, not just attacker-controlled paths
(M01's surface). Two different answers for two different kinds of native
risk:

- **Fuzzed directly: docx, xlsx, pptx, EXIF.** Four `cargo-fuzz` targets
  (`docx_extract`, `xlsx_extract`, `pptx_extract`, `exif_extract`) drive fuzzed
  bytes through `dafs_extract::extract`'s real public entry point — the same
  sniff-then-parse-then-`catch_unwind` path a hostile file walks in
  production — asserting only "no panic, no hang"; any `Err` is a fine
  outcome for garbage input. This is the testing bar's own rule
  (`docs/roadmap-and-design-review.md` §5.2 item 3: "a fuzz target for every
  parser that touches bytes the user did not type") applied to Rust code
  cargo-fuzz can actually instrument.
- **Isolated instead: PDF.** `pdfium-render` wraps native C++ that
  cargo-fuzz cannot reach into — there is no Rust-level instrumentation
  boundary inside a prebuilt shared library. Process isolation is the correct
  substitute, not a gap: `dafs-pdf-worker` runs one process per PDF, and a
  crash there costs that one process, never the daemon (see *Decisions worth
  knowing*). Fuzzing would find bugs in Pdfium; isolation makes those bugs
  cost nothing regardless of whether they're ever found.

**No new DAST scope.** M02a extends M01's `/events` response shape and adds
`/facets` to the same HTTP surface — it does not stand up a new server.
`docs/roadmap-and-design-review.md` §5.2 scopes DAST to exactly three
milestones with an HTTP surface (M01, M03, M08); M02a is not a fourth, it
inherits M01's DAST coverage rather than needing its own entry.

## Deliberately not done

- **Per-file git blame.** Repo-level HEAD facts only — see *Decisions worth
  knowing* for the O(commits)-per-file cost that rules it out at this
  milestone's corpus sizes.
- **Summary, keywords, entities, classification.** M02b's job, and the reason
  this milestone exists as a separate one rather than a bigger M02.
- **A worker thread pool.** One dedicated thread drains the queue.
  `extract_worker.rs`'s module docs are explicit: "nothing yet has measured a
  queue depth that a lone thread cannot keep up with, and a pool is the kind
  of complexity that wants a measurement behind it rather than being assumed
  up front."
- **xlsx formula-cached-string cells, docx rich text and real page counts.**
  `office.rs` documents each cut at the point it's made: a docx's laid-out
  page count "depends on page size, margins, and font metrics, none of which
  appear in `document.xml` — only an actual layout engine can compute it,"
  so it's left unset rather than estimated; numeric spreadsheet cells are
  skipped because "a spreadsheet's numbers aren't 'body text' in the sense
  `word_count`/`language` care about."

## Next: M02b

Local LLM enrichment — summary, keywords, entities, classification — on
whatever this milestone's `file_metadata` rows already describe. The
model-distribution question (`docs/roadmap-and-design-review.md` §7 item 5)
is still open and needs its own decision before that milestone can build.

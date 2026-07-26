# M02b — LLM enrichment

**In progress.** Every event whose file has already been through M02a's
deterministic extraction can now also get a summary, keywords, entities, and
a classification — but only if the person running dafs configured somewhere
for those to come from.

## What it is

The same `dafs --watch ...` as M01/M02a. Nothing turns enrichment on by
default: `dafs-enrich` is a thin HTTP client against a user-configured
**OpenAI-compatible chat-completions endpoint** — a local llama.cpp/Ollama/vLLM
server, or a hosted API — and dafs itself never runs, fetches, or vendors a
model. `docs/roadmap-and-design-review.md` §7 item 5 has the resolved
decision: a Gemma-class model's weights are gigabytes, not the tens of
megabytes pdfium's shared library cost, so `include_bytes!`-ing a model the
way §2 item 9 vendors every other native dependency was never on the table.
Rather than solve model distribution, M02b sidesteps it — the model, wherever
it runs, is entirely outside dafs's build and deployment story, and §2 item 9
gains no new exception for it because there is nothing here to vendor. M08
(the AI assistant) reuses this same client and inherits the same answer.

Once a `base_url` is configured, the daemon's enrichment worker reads each
file's already-extracted text straight back out of the store (no re-parsing
the original file) and queues it for enrichment. `Enrichment` never touches
the original file — `docs/roadmap-and-design-review.md` §2 item 1's locked
decision, `AI output must never modify original files automatically`, holds
structurally here: there is no code path from a model's reply to a write
against anything the daemon observes.

## What shipped

| | |
|---|---|
| `dafs-enrich` | `Config`/`Enrichment`/`EnrichError`, `enrich()` — a single blocking POST per file, plain-prompt JSON parsing, no `response_format`. |
| `dafs-store::enrichment` | `file_enrichment` (replaceable, keyed by `file_id`) and the durable `enrichment_queue`, mirroring `metadata`'s two-table shape. |
| migration 4 (`enrichment`) | Adds `file_metadata.body_text`; creates `file_enrichment`/`enrichment_queue`. M02a's own migration is untouched. |
| `deny.toml` | `CDLA-Permissive-2.0` allowed for `webpki-roots`, pulled in by `ureq`'s `tls` feature. |

This document is being written while the daemon-side wiring (the worker that
drains `enrichment_queue` and calls `dafs_enrich::enrich`) and the `GET
/events` API extension are still in progress elsewhere in the tree — the
table above and everything below it describes only what's confirmed and
already merged: the crate itself and its data layer.

## Decisions worth knowing

**Plain-prompt JSON, not `response_format`.** OpenAI's `response_format:
{"type": "json_object"}` (or JSON-schema mode) isn't supported by every
OpenAI-compatible server — local llama.cpp/Ollama builds vary in which
provider-specific extensions they honour. `dafs-enrich` instead asks for JSON
in the prompt text itself and parses defensively: `parse_model_reply` slices
from the first `{` to the last `}` in the reply and parses that span,
because models reliably wrap requested JSON in prose or code-fence markers
despite being asked not to. That works against any chat-completions endpoint
regardless of provider-specific modes, which matters more here than the
marginal reliability gain of a mode most local servers won't even honour.

**`body_text` reuses M02a's extraction instead of re-parsing.** Migration 4
adds `file_metadata.body_text` — text M02a's extractors already computed
internally (to derive `word_count`/`language`) but never persisted. The
migration comment is explicit about why this landed as its own migration
rather than a change to M02a's: "Added here, not in M02a's migration — that
one is never edited once shipped — so M02b's enrichment worker can read a
file's text straight back out of the store instead of re-parsing the
original file a second time." M02a's migration is append-only history now,
not a live document.

**`file_enrichment`/`enrichment_queue` are separate tables from M02a's, on
purpose.** The schema comment in migration 4 states the boundary directly:
"LLM-derived fields live in their own table, never as columns on
`file_metadata`: M02a's crate and schema stay LLM-free in substance, not just
in comments, and enrichment can be independently enabled, disabled, or
re-run without ever touching M02a's shipped schema." Unlike
`extraction_queue`, nothing enqueues `enrichment_queue` unconditionally —
`dafs_store::enrichment`'s own module docs note that decision (is enrichment
configured, is there enough text) belongs to the daemon, not the store.

**The `CDLA-Permissive-2.0` exception in `deny.toml`.** Its comment, quoted
in full: "A data licence, not a code licence — CDLA-Permissive-2.0's 2.0
revision dropped even the attribution requirement 1.0 had, so it imposes
nothing a filesystem daemon needs to act on: no share-alike, no notice file,
no patent restriction. Added deliberately for `webpki-roots` (Mozilla's root
certificate bundle), pulled in by `ureq`'s `tls` feature — `dafs-enrich`'s
client needs TLS to reach a hosted (as opposed to local) OpenAI-compatible
endpoint, and `dafs-tui` already depends on `ureq` without it, so this is new
only because M02b turned TLS on."

**Every input is capped, independent of the caller's own cap.**
`MAX_INPUT_CHARS` (8,000) truncates at a `char` boundary before a request is
ever sent — `body_text` is already capped upstream by
`dafs_extract::MAX_BODY_TEXT_CHARS`, but `enrich()` has no way to enforce
that promise, so it enforces its own, the same "never trust a caller's cap"
instinct `dafs_extract::extract`'s own byte cap follows.

**Every failure is `Err`, categorised only enough to log usefully.**
`EnrichError` distinguishes connection failure, HTTP status, an unparseable
envelope, no choices, no JSON in the reply, and a shape mismatch — but the
caller's retry behaviour is the same for every variant: leave it queued,
bounded by `dafs_store::enrichment::MAX_ATTEMPTS` (5), the same poison-file
cap `metadata::MAX_ATTEMPTS` uses for extraction.

## Memory

Not yet measured for this milestone. `dafs-enrich` itself holds no state
between calls — no connection pool, no cache — so its own footprint is
whatever one in-flight request costs. The daemon-side worker that will
actually drive `enrichment_queue` under `dafs-memtest` is part of the
in-flight daemon wiring this document does not cover; the memory section
here will be filled in once that worker and its queue-drain scenario land,
the same way M02a added its own scenario on top of M01's idle baseline.

## Bugs found while building this

None yet in the shipped part (the crate and the data layer) — the mock-server
tests below were written to the same bar M01/M02a's own bug list came from
("found by tests written to the bar rather than by use") and found nothing
to fix. This section will be revisited once the daemon-side worker and API
extension are in and have had a chance to be exercised end to end.

## Security

**The mock-server test suite (`crates/dafs-enrich/tests/hostile_server.rs`),
against a hand-rolled `TcpListener`, no real network or model required:**

- `model_output_shaped_like_a_complied_instruction_is_only_ever_a_plain_string_field`
  and `injected_instructions_in_the_input_text_reach_the_wire_only_as_an_escaped_json_string`
  are the prompt-injection tests. One feeds `enrich()` a mocked reply engineered
  to look like the model complied with an injected instruction — claiming to
  run a shell command, fetch a URL, overwrite a file — and asserts the
  dangerous-looking text comes back out only as `Enrichment::summary`,
  `keywords`, and `entities`, compared byte for byte. The other sends
  injected-instruction text as the *input* document and asserts, by
  inspecting the raw bytes the mock server received, that it reached the
  wire only as a properly JSON-escaped string value inside the request
  body — never unescaped, never anywhere but the `content` field.
- `a_response_with_no_choices_is_an_error_not_a_panic`,
  `a_truncated_envelope_is_an_error_not_a_panic`,
  `a_deeply_nested_reply_that_does_not_match_the_expected_shape_is_an_error_not_a_panic`,
  `a_reply_wrapped_in_a_few_hundred_kb_of_filler_text_still_parses_without_a_panic`,
  `wrong_content_type_with_a_non_json_body_is_an_error_not_a_panic`, and
  `non_utf8_bytes_in_the_body_are_an_error_not_a_panic` drive a real (mocked)
  HTTP round trip through truncated JSON, an empty `choices` array, a few
  hundred KB of filler text around a valid reply, a deeply nested shape
  mismatch, a mismatched `Content-Type`, and raw non-UTF-8 bytes — each
  asserted to produce a clean `Err`, never a panic.
- `a_server_that_never_responds_is_bounded_by_the_configured_timeout` accepts
  a connection and then writes nothing for 30 seconds; with a 200ms
  `Config::timeout`, `enrich()` is asserted to return (with an `Err`) in
  well under 5 seconds, never hanging the caller.

**The containment argument for prompt injection is structural, not just
tested.** `docs/roadmap-and-design-review.md` §5.3's "Prompt injection
(M02b, M08)" bullet requires the rule in §2 item 1 — `AI output must never
modify original files automatically` — to be tested, not merely asserted.
Inside `dafs-enrich`, the argument is: `enrich()`'s only outputs are
`Enrichment`'s four plain fields (`Option<String>`, `Vec<String>`) or an
`EnrichError`. There is no filesystem write, no process spawn, no second
network call, anywhere in this crate's code that depends on what the
model's reply text says — parsing a reply and acting on its content are
different operations, and this crate only ever does the first. The mock
tests above make that concrete rather than asserted by reading the source:
a reply that looks like a complied-with instruction is demonstrably just
data by the time it leaves `enrich()`.

**Hallucination-rate / golden-corpus evaluation is out of scope for this
repo.** §6 item 3 is explicit that the corpus is real documents, grown
incrementally, and — because the code is public — lives outside the public
tree: NAS-hosted, referenced by content hash from a manifest that is itself
public. Nothing in `crates/dafs-enrich` builds or references that corpus.
The tests here prove the *client* is robust against a hostile or broken
endpoint; they say nothing about whether a given model's summaries are
*good*, which is a different question with a different, external answer.

## Deliberately not done

- **No `response_format`/JSON-schema mode support.** See *Decisions worth
  knowing* — plain-prompt JSON was chosen specifically for compatibility with
  servers that don't support it.
- **No retry-with-backoff inside `enrich()`.** A single call either succeeds
  or returns `Err`; retrying a queued file is the daemon worker's job via
  `enrichment_queue` and `MAX_ATTEMPTS`, the same division of responsibility
  `dafs_extract::extract` and its extraction queue already use.
- **No streaming responses.** One request, one blocking response read — a
  summary/keyword/classification reply is short enough that streaming would
  add complexity (partial-JSON handling mid-stream) for no benefit here.
- **No multi-turn conversation.** Each call is a fresh two-message exchange
  (system prompt, document text); there is no history, no follow-up turn,
  and nothing in `Config` to carry one.
- **No hallucination-rate measurement.** See *Security* — that needs the
  external golden corpus §6 item 3 describes, not something this crate's own
  test suite can provide.

## Next

The in-flight daemon wiring (`enrich_worker.rs`) and `GET /events` API
extension complete this milestone: draining `enrichment_queue`, calling
`dafs_enrich::enrich` with a configured `Config`, and surfacing
`summary`/`keywords`/`entities`/`classification` on the timeline the same
way M02a's metadata fields already appear, omitted rather than `null` when
absent. After that, M08 (the AI assistant) reuses `dafs-enrich`'s client
unchanged — `docs/roadmap-and-design-review.md` §2 item 9 already notes this:
"M08 (AI assistant) reuses the same client and inherits the same answer."

//! Fuzz `dafs_enrich::parse_embedding_response`.
//!
//! See `enrich_response.rs`'s module docs for why this is a pure parser
//! rather than a mock-server-driven `embed` call, and why fuzzing it at all
//! closes a gap `docs/m03-semantic-search.md`'s "Deliberately not done"
//! section flagged explicitly ("this now applies to a second parser").
//!
//! `model`/`expected_dimensions` are fixed rather than fuzzed: the
//! interesting untrusted input here is the JSON response body — the same
//! kind of network-supplied bytes `enrich_response.rs` fuzzes for the chat
//! endpoint — not the caller-supplied expected width, which is an
//! admin-configured constant in every real caller
//! (`dafs_store::embeddings::EmbeddingConfig::dimensions`), never derived
//! from the response itself.
//!
//! # The property asserted
//!
//! No panic, no hang. A malformed envelope, no data, or a width mismatch are
//! all `Err`, the ordinary outcome for fuzzed input. Only a panic or an
//! infinite loop is a bug.
#![no_main]

use libfuzzer_sys::fuzz_target;

/// Arbitrary and small — see the module docs on why the exact value doesn't
/// matter to what this target is testing.
const EXPECTED_DIMENSIONS: usize = 3;

fuzz_target!(|data: &[u8]| {
    // Lossy, same reasoning as `enrich_response.rs`: every input drives a
    // real call rather than a no-op whenever it happens not to be valid
    // UTF-8.
    let body = String::from_utf8_lossy(data);
    let _ = dafs_enrich::parse_embedding_response(&body, "fuzz-model", EXPECTED_DIMENSIONS);
});

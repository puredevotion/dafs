//! Vector storage for M03 semantic search: a durable work queue (same shape
//! as `enrichment::enrichment_queue`) plus `file_embedding`, a `sqlite-vec`
//! `vec0` virtual table, created on demand rather than as a static
//! `MIGRATIONS` entry.
//!
//! # Why `file_embedding` isn't in `MIGRATIONS`
//!
//! Every table in `MIGRATIONS` has an identical shape across every
//! deployment of dafs. `file_embedding` can't: a `vec0` table's vector column
//! is declared with a fixed width (`float[N]`) at `CREATE VIRTUAL TABLE`
//! time, and `N` is the output dimensionality of whichever embedding model a
//! deployment's admin points `--llm-embedding-model` at — 384, 768, 1536,
//! whatever that model happens to produce. A migration list that is shared,
//! forward-only, and "never edit or reorder a shipped entry" (this crate's
//! own module docs) has no way to express a column width that legitimately
//! differs per installation.
//!
//! [`ensure_table`] does instead what a migration would: create the table
//! the first time embeddings are configured, sized to the dimensionality
//! it's told, and record that choice in `embedding_config` (added by
//! `MIGRATIONS` version 5 — that row's *shape* is universal even though its
//! *contents* are deployment-specific) so a later run with a different model
//! or width fails loudly rather than silently writing mismatched-width blobs
//! into an existing column.
//!
//! # Storage format
//!
//! Vectors are bound as raw little-endian `f32` bytes, not JSON text.
//! `sqlite-vec`'s blob path (see `sqlite-vec.c`'s `fvec_from_value`) accepts
//! exactly that: a BLOB whose length is a multiple of 4, memcpy'd directly
//! into an `f32` buffer with no header and no endianness conversion — sound
//! on every platform this workspace targets (x86_64/aarch64, both
//! little-endian). This avoids a text round-trip on both the write path
//! (`store`) and the read path (`search`'s query vector).
//!
//! # Binary quantization: `file_embedding_bin`, not just `file_embedding`
//!
//! `docs/memory-budget.md` §8.3 is explicit that this is a *functional*
//! requirement, not later tuning: "full-float resident vectors cannot meet
//! the 96 MiB ceiling at 1M documents". The reason isn't the float table's
//! on-disk size — `dafs_store::tune`'s small-page-cache/large-mmap-window
//! setup already makes those pages evictable — it's that `vec0` with no ANN
//! index answers `MATCH` with a brute-force scan, computing a real distance
//! against *every* row's full vector for *every* query. At 1M documents that
//! pages the entire float table through the process for one search.
//!
//! `file_embedding_bin` exists to make that scan cheap enough to be resident:
//! a `bit[N]` `vec0` table, one bit per dimension (the sign of that
//! dimension — `docs/memory-budget.md`'s cited 32× reduction), searched by
//! Hamming distance. [`search`] queries this table first for an oversampled
//! candidate set (a small multiple of the caller's `k`), then reads only
//! those candidates' full-precision vectors out of `file_embedding` to
//! rescore and rank — never the whole table. The published recall cost of
//! this two-stage shape is small (~2%) precisely because the expensive part
//! (touching every row) only ever happens on the 32×-smaller table.
//!
//! Bit-packing format: bit `i` of dimension `i` lives in byte `i / 8`, at bit
//! position `i % 8` (LSB first) — matching `sqlite-vec.c`'s own unpacking
//! (`vector[i/8] >> (i % CHAR_BIT) & 1`), so there is no reformatting between
//! what this module writes and what the C extension reads. `vec0` requires a
//! `bit[N]` column's `N` to be a multiple of 8 (`sqlite-vec.c` checks
//! `dimensions % CHAR_BIT`), so `file_embedding_bin`'s declared width is
//! `dimensions` rounded up to the next multiple of 8, not `dimensions`
//! itself. The extra padding bits are always zero on every vector this
//! module ever writes (`quantize_bits` only ever sets bits `0..vector.len()`)
//! and so contribute identically zero to every Hamming distance computed —
//! correct for any configured width, not just ones that already happen to be
//! byte-aligned (384, 768, 1536, ... are, but nothing requires it).

use rusqlite::{Connection, OptionalExtension, params};

use crate::StoreError;
use crate::paths::FileId;

/// Same cap `enrichment::MAX_ATTEMPTS` uses, for the same reason: a file that
/// reliably fails embedding stops being retried after a handful of attempts
/// rather than spinning a worker on it forever across restarts.
pub const MAX_ATTEMPTS: i64 = 5;

/// The embedding model and dimensionality `file_embedding` was created with,
/// if it has been created at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingConfig {
    pub model: String,
    pub dimensions: usize,
}

/// The configured dimensionality doesn't match what `file_embedding` was
/// already created with. Refusing is the only safe action: `vec0`'s column
/// width is fixed at creation time, so silently proceeding would mean either
/// truncating every new vector or rejecting it at the C layer with a far
/// less clear error.
#[derive(Debug, thiserror::Error)]
#[error(
    "file_embedding was created for model {existing_model:?} ({existing_dimensions} dimensions); \
     refusing to reuse it for {requested_model:?} ({requested_dimensions} dimensions) — \
     point at a different data directory or re-embed from scratch"
)]
pub struct DimensionMismatch {
    pub existing_model: String,
    pub existing_dimensions: usize,
    pub requested_model: String,
    pub requested_dimensions: usize,
}

/// Read back which model/dimensionality `file_embedding` was created with,
/// if it has been.
pub fn config(conn: &Connection) -> Result<Option<EmbeddingConfig>, StoreError> {
    conn.query_row("SELECT model, dimensions FROM embedding_config WHERE id = 1", [], |r| {
        Ok(EmbeddingConfig { model: r.get(0)?, dimensions: r.get::<_, i64>(1)? as usize })
    })
    .optional()
    .map_err(StoreError::from)
}

/// Creates `file_embedding` the first time embeddings are configured for
/// this database, sized to `dimensions`. Idempotent: calling this again with
/// the same `model`/`dimensions` (the normal case — every daemon start does
/// this once) is a no-op. Called with a different `model` or `dimensions`
/// than what's already recorded, it refuses rather than reusing the table.
pub fn ensure_table(conn: &Connection, model: &str, dimensions: usize) -> Result<(), StoreError> {
    if let Some(existing) = config(conn)? {
        if existing.model != model || existing.dimensions != dimensions {
            return Err(StoreError::EmbeddingDimensionMismatch(Box::new(DimensionMismatch {
                existing_model: existing.model,
                existing_dimensions: existing.dimensions,
                requested_model: model.to_string(),
                requested_dimensions: dimensions,
            })));
        }
        return Ok(());
    }

    let tx = conn.unchecked_transaction()?;
    // `dimensions` is an admin-configured integer (`--llm-embedding-dimensions`),
    // never caller/request input, so interpolating it into DDL text — the only
    // way to give a `vec0` table a variable-width column — does not reopen the
    // injection risk parameter binding exists to close elsewhere in this crate.
    tx.execute_batch(&format!(
        "CREATE VIRTUAL TABLE file_embedding USING vec0(embedding float[{dimensions}]);"
    ))?;
    // The quantized companion table — see the module docs' *Binary
    // quantization* section on why `search` needs this to be the table it
    // scans, not `file_embedding` itself. Rounded up to a multiple of 8 (see
    // the module docs' *Bit-packing format* paragraph on why that's always
    // correct, not just for widths that already clear it).
    let bit_dimensions = dimensions.div_ceil(8) * 8;
    tx.execute_batch(&format!(
        "CREATE VIRTUAL TABLE file_embedding_bin USING vec0(embedding bit[{bit_dimensions}]);"
    ))?;
    tx.execute(
        "INSERT INTO embedding_config (id, model, dimensions) VALUES (1, ?1, ?2)",
        params![model, dimensions as i64],
    )?;
    tx.commit()?;
    Ok(())
}

/// Serialize a vector to the raw little-endian bytes `file_embedding`'s
/// `embedding` column expects — see the module docs' *Storage format*.
fn to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// One bit per dimension — the sign of that dimension, 1 for non-negative —
/// packed LSB-first into bytes to match `sqlite-vec.c`'s own unpacking. See
/// the module docs' *Binary quantization* section. `vector.len()` need not be
/// a multiple of 8: the final partial byte is simply zero-padded in its
/// unused high bits, which never affects a Hamming distance since both sides
/// of any real comparison share the same dimensionality and thus the same
/// padding.
fn quantize_bits(vector: &[f32]) -> Vec<u8> {
    let mut bytes = vec![0u8; vector.len().div_ceil(8)];
    for (i, &f) in vector.iter().enumerate() {
        if f >= 0.0 {
            bytes[i / 8] |= 1 << (i % 8);
        }
    }
    bytes
}

/// Record `file_id`'s embedding and clear its queue entry, in a single
/// transaction — same reasoning as `enrichment::record_enrichment`: a crash
/// between the two writes must not either lose a successful result or clear
/// the queue entry for a result that never landed.
///
/// `vector.len()` must equal the dimensionality `file_embedding` was created
/// with (see `ensure_table`) — a mismatch is a caller bug (the embedding
/// client returned a different width than configured), not a runtime
/// condition worth a dedicated variant here: `vec0` itself rejects a
/// wrong-length blob at the C layer, and that rejection surfaces as an
/// ordinary `StoreError::Sqlite` rather than a silently truncated or
/// zero-padded vector.
pub fn store(conn: &Connection, file_id: FileId, vector: &[f32]) -> Result<(), StoreError> {
    let tx = conn.unchecked_transaction()?;
    // vec0 virtual tables don't implement `ON CONFLICT`/UPSERT (SQLite returns
    // "UPSERT not implemented for virtual table") — delete-then-insert is the
    // supported way to replace a row, and staying inside this transaction
    // keeps it atomic with the queue-clearing delete below.
    tx.execute("DELETE FROM file_embedding WHERE rowid = ?1", [file_id])?;
    tx.execute(
        "INSERT INTO file_embedding(rowid, embedding) VALUES (?1, ?2)",
        params![file_id, to_blob(vector)],
    )?;
    // Kept in lockstep with the full-precision row, same transaction: a
    // rowid present in one table but not the other would make `search`'s
    // rescore step either miss a real candidate or rescore one that no
    // longer has a float vector to rescore against.
    tx.execute("DELETE FROM file_embedding_bin WHERE rowid = ?1", [file_id])?;
    // `vec_bit(...)` wraps the blob with the BIT element-type subtype `vec0`
    // dispatches on (`sqlite-vec.c`'s `vector_from_value`): a bare blob
    // parameter carries no subtype and is parsed as float32 by default (fine
    // for `file_embedding` above, since that is the type it actually is —
    // but wrong here without this wrapper).
    tx.execute(
        "INSERT INTO file_embedding_bin(rowid, embedding) VALUES (?1, vec_bit(?2))",
        params![file_id, quantize_bits(vector)],
    )?;
    tx.execute("DELETE FROM embedding_queue WHERE file_id = ?1", [file_id])?;
    tx.commit()?;
    Ok(())
}

/// How many candidates [`search`] pulls from the quantized ANN pass per
/// requested result — `docs/memory-budget.md` §8.3's cited "2-4x the
/// requested k". 4, the top of that range: the rescore pass only ever reads
/// as many full-precision vectors as this produces, so erring toward more
/// candidates buys recall at a cost that stays tiny relative to a full-table
/// scan (the thing this two-stage design exists to avoid) at any realistic
/// corpus size.
const OVERSAMPLE_FACTOR: u32 = 4;

/// Nearest neighbours of `query` by Euclidean distance, closest first.
///
/// Two-stage, not a single `file_embedding` scan — see the module docs'
/// *Binary quantization* section for why. First, a Hamming-distance pass over
/// `file_embedding_bin` (the resident, 32×-smaller quantized table) pulls an
/// oversampled candidate set ([`OVERSAMPLE_FACTOR`] × `limit`). Then only
/// those candidates' real vectors are read back out of `file_embedding` and
/// rescored by true Euclidean distance for the final ranking — the full
/// float table is never scanned end to end.
pub fn search(
    conn: &Connection,
    query: &[f32],
    limit: u32,
) -> Result<Vec<(FileId, f64)>, StoreError> {
    let candidate_limit = limit.saturating_mul(OVERSAMPLE_FACTOR);

    let mut candidate_stmt = conn.prepare_cached(
        "SELECT rowid FROM file_embedding_bin
          WHERE embedding MATCH vec_bit(?1)
          ORDER BY distance LIMIT ?2",
    )?;
    let candidates: Vec<FileId> = candidate_stmt
        .query_map(params![quantize_bits(query), candidate_limit], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;

    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    // Rescored one rowid at a time rather than a single `WHERE rowid IN
    // (...)`: `vec0` virtual tables don't support arbitrary secondary
    // predicates the way an ordinary table would, but a per-rowid point
    // lookup is exactly what `rowid = ?` against a virtual table backed by
    // shadow tables resolves to — still bounded work (`candidate_limit`
    // lookups, never the full table).
    let mut rescore_stmt =
        conn.prepare_cached("SELECT embedding FROM file_embedding WHERE rowid = ?1")?;
    let mut rescored: Vec<(FileId, f64)> = Vec::with_capacity(candidates.len());
    for file_id in candidates {
        let blob: Option<Vec<u8>> = rescore_stmt.query_row([file_id], |r| r.get(0)).optional()?;
        // A candidate present in the bin table but missing from the float
        // table would mean the two fell out of lockstep — `store` keeps them
        // in the same transaction, so this should not happen in practice, but
        // skipping rather than erroring keeps one inconsistent row from
        // failing an entire search.
        let Some(blob) = blob else { continue };
        rescored.push((file_id, euclidean_distance(&blob, query)));
    }

    rescored.sort_by(|a, b| a.1.total_cmp(&b.1));
    rescored.truncate(limit as usize);
    Ok(rescored)
}

/// True Euclidean distance between a stored little-endian `f32` blob and a
/// query vector, matching `vec0`'s own float-distance metric — see the
/// module docs' *Storage format* on why the blob is raw bytes rather than a
/// `Vec<f32>` already, and [`search`]'s doc comment for why this is computed
/// here in Rust rather than via a second `vec0 MATCH` (only the oversampled
/// candidates from the bin-table pass ever reach this, never the full table).
fn euclidean_distance(blob: &[u8], query: &[f32]) -> f64 {
    blob.chunks_exact(4)
        .zip(query)
        .map(|(bytes, &q)| {
            let stored = f32::from_le_bytes(bytes.try_into().expect("chunks_exact(4)"));
            let diff = f64::from(stored) - f64::from(q);
            diff * diff
        })
        .sum::<f64>()
        .sqrt()
}

/// Enqueue a file for (re-)embedding, or bump an existing entry's timestamp.
/// Called by the daemon only when embeddings are configured — mirrors
/// `enrichment::enqueue` exactly, including that this module has no opinion
/// on whether a given file should be queued.
pub fn enqueue(conn: &Connection, file_id: FileId, queued_at_unix: i64) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO embedding_queue (file_id, queued_at_unix, attempt_count)
         VALUES (?1, ?2, 0)
         ON CONFLICT(file_id) DO UPDATE SET queued_at_unix = excluded.queued_at_unix",
        params![file_id, queued_at_unix],
    )?;
    Ok(())
}

/// Files still waiting to be embedded, oldest first, capped at
/// [`MAX_ATTEMPTS`]. Mirrors `enrichment::pending` exactly.
pub fn pending(conn: &Connection, limit: u32) -> Result<Vec<FileId>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT file_id FROM embedding_queue
          WHERE attempt_count < ?2
          ORDER BY queued_at_unix ASC LIMIT ?1",
    )?;
    let ids = stmt
        .query_map(params![limit, MAX_ATTEMPTS], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// Record that an embedding attempt was made for `file_id` — called when a
/// worker picks the file up, before the network call, not when it finishes.
/// Mirrors `enrichment::record_attempt` exactly, including why that ordering
/// is what makes a crash mid-request still count against [`MAX_ATTEMPTS`].
pub fn record_attempt(conn: &Connection, file_id: FileId) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE embedding_queue SET attempt_count = attempt_count + 1 WHERE file_id = ?1",
        [file_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::paths::{Interner, ensure_dir_chain};

    fn db_with_file(name: &str) -> (Connection, FileId) {
        let conn = crate::open_in_memory().expect("open");
        let mut i = Interner::new();
        let path = Path::new(name);
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty() && *p != Path::new("/"));
        let file_name = path.file_name().expect("filename").to_str().expect("utf8");
        let parent_id = parent.map(|p| ensure_dir_chain(&conn, &mut i, p).expect("parent"));
        let component_id = i.intern_leaf(&conn, file_name).expect("intern");
        let id =
            crate::paths::upsert_entry(&conn, parent_id, component_id, false, Some(0), Some(0))
                .expect("upsert leaf");
        (conn, id)
    }

    #[test]
    fn ensure_table_is_idempotent_for_the_same_model() {
        let (conn, _) = db_with_file("/a");
        ensure_table(&conn, "test-model", 3).expect("first");
        ensure_table(&conn, "test-model", 3).expect("second, same config");
    }

    #[test]
    fn ensure_table_refuses_a_different_model_or_width() {
        let (conn, _) = db_with_file("/a");
        ensure_table(&conn, "test-model", 3).expect("first");

        let err = ensure_table(&conn, "other-model", 3).unwrap_err();
        assert!(matches!(err, StoreError::EmbeddingDimensionMismatch(_)));

        let err = ensure_table(&conn, "test-model", 4).unwrap_err();
        assert!(matches!(err, StoreError::EmbeddingDimensionMismatch(_)));
    }

    #[test]
    fn store_then_search_finds_the_nearest_neighbour() {
        let (conn, a) = db_with_file("/a");
        let mut i = Interner::new();
        let b_component = i.intern_leaf(&conn, "b").expect("intern b");
        let b = crate::paths::upsert_entry(&conn, None, b_component, false, Some(0), Some(0))
            .expect("upsert b");

        ensure_table(&conn, "test-model", 3).expect("ensure_table");
        store(&conn, a, &[1.0, 0.0, 0.0]).expect("store a");
        store(&conn, b, &[0.0, 1.0, 0.0]).expect("store b");

        let hits = search(&conn, &[0.9, 0.1, 0.0], 1).expect("search");
        assert_eq!(hits.first().map(|(id, _)| *id), Some(a));
    }

    #[test]
    fn re_storing_overwrites_rather_than_duplicates() {
        let (conn, a) = db_with_file("/a");
        ensure_table(&conn, "test-model", 3).expect("ensure_table");
        store(&conn, a, &[1.0, 0.0, 0.0]).expect("first store");
        store(&conn, a, &[0.0, 0.0, 1.0]).expect("second store");

        let rows: i64 =
            conn.query_row("SELECT COUNT(*) FROM file_embedding", [], |r| r.get(0)).expect("count");
        assert_eq!(rows, 1, "re-storing an embedding duplicated the row instead of overwriting it");

        // The nearest neighbour to the second vector should now be `a`, proving
        // the row actually holds the new value and not the first one.
        let hits = search(&conn, &[0.0, 0.0, 0.9], 1).expect("search");
        assert_eq!(hits.first().map(|(id, _)| *id), Some(a));
    }

    #[test]
    fn store_clears_the_queue_entry() {
        let (conn, a) = db_with_file("/a");
        ensure_table(&conn, "test-model", 3).expect("ensure_table");
        enqueue(&conn, a, 500).expect("enqueue");
        assert_eq!(pending(&conn, 10).expect("pending"), vec![a]);

        store(&conn, a, &[1.0, 0.0, 0.0]).expect("store");
        assert!(pending(&conn, 10).expect("pending").is_empty());
    }

    #[test]
    fn storing_a_wrong_width_vector_is_an_error_not_a_silent_truncation() {
        // Proves the module docs' claim on `store` rather than asserting it by
        // inspection: `vec0` itself, not application code, is what rejects a
        // caller bug here.
        let (conn, a) = db_with_file("/a");
        ensure_table(&conn, "test-model", 3).expect("ensure_table");
        let err = store(&conn, a, &[1.0, 0.0]).unwrap_err();
        assert!(matches!(err, StoreError::Sqlite(_)));
    }

    #[test]
    fn a_poison_file_stops_being_retried_past_max_attempts() {
        let (conn, a) = db_with_file("/a");
        enqueue(&conn, a, 100).expect("enqueue");

        for _ in 0..MAX_ATTEMPTS {
            assert_eq!(pending(&conn, 10).expect("pending"), vec![a]);
            record_attempt(&conn, a).expect("attempt");
        }

        assert!(
            pending(&conn, 10).expect("pending").is_empty(),
            "a file past MAX_ATTEMPTS was still offered for dispatch"
        );
    }

    #[test]
    fn config_is_none_before_ensure_table() {
        let (conn, _) = db_with_file("/a");
        assert!(config(&conn).expect("config").is_none());
    }

    #[test]
    fn config_reports_what_ensure_table_recorded() {
        let (conn, _) = db_with_file("/a");
        ensure_table(&conn, "test-model", 3).expect("ensure_table");
        assert_eq!(
            config(&conn).expect("config"),
            Some(EmbeddingConfig { model: "test-model".to_string(), dimensions: 3 })
        );
    }

    #[test]
    fn quantize_bits_sets_one_bit_per_non_negative_dimension() {
        // [1.0, -1.0, 0.0, -0.5] -> bits [1, 0, 1, 0] (0.0 counts as
        // non-negative — `>= 0.0`, not `> 0.0`) packed LSB-first into byte 0.
        let bits = quantize_bits(&[1.0, -1.0, 0.0, -0.5]);
        assert_eq!(bits, vec![0b0000_0101]);
    }

    #[test]
    fn quantize_bits_pads_a_non_byte_aligned_length_with_zero_bits() {
        // 3 dimensions still produces exactly one byte, with the top 5 bits
        // unset — proves the padding [`ensure_table`]'s bit-width rounding
        // relies on is actually zero, not garbage.
        let bits = quantize_bits(&[1.0, 1.0, 1.0]);
        assert_eq!(bits, vec![0b0000_0111]);
    }

    /// `file_embedding_bin` must still work end-to-end for a dimensionality
    /// that isn't already a multiple of 8 (most real embedding models are,
    /// but nothing requires it) — proves the rounding in `ensure_table`
    /// actually produces a usable table rather than one whose declared width
    /// silently disagrees with what `quantize_bits` produces.
    #[test]
    fn store_and_search_work_for_a_non_byte_aligned_dimensionality() {
        let (conn, a) = db_with_file("/a");
        ensure_table(&conn, "test-model", 10).expect("ensure_table");
        store(&conn, a, &[1.0; 10]).expect("store");

        let hits = search(&conn, &[1.0; 10], 1).expect("search");
        assert_eq!(hits.first().map(|(id, _)| *id), Some(a));
    }

    #[test]
    fn search_against_an_empty_store_returns_no_hits() {
        let (conn, _) = db_with_file("/a");
        ensure_table(&conn, "test-model", 3).expect("ensure_table");
        assert!(search(&conn, &[1.0, 0.0, 0.0], 5).expect("search").is_empty());
    }

    /// The property the whole two-stage design exists for: the *final*
    /// ranking must be true Euclidean distance, not just whichever candidates
    /// the Hamming-distance pass happened to surface. A vector whose bit
    /// pattern is closer by Hamming distance but farther by true distance
    /// must lose to one that's the reverse, once both are within the
    /// oversampled candidate set.
    #[test]
    fn search_rescores_candidates_by_true_distance_not_hamming_distance() {
        let (conn, close) = db_with_file("/close");
        let mut i = Interner::new();
        let far_component = i.intern_leaf(&conn, "far").expect("intern far");
        let far = crate::paths::upsert_entry(&conn, None, far_component, false, Some(0), Some(0))
            .expect("upsert far");

        ensure_table(&conn, "test-model", 4).expect("ensure_table");

        let query = [1.0, 1.0, 1.0, 1.0];
        // Same sign pattern as `query` on every dimension (Hamming distance
        // 0 against it) but numerically far away.
        store(&conn, far, &[100.0, 100.0, 100.0, 100.0]).expect("store far");
        // Different sign on one dimension (Hamming distance 1, strictly
        // worse than `far`'s 0) but numerically almost identical to `query`.
        store(&conn, close, &[1.0, 1.0, 1.0, -0.01]).expect("store close");

        // A pure Hamming-distance ranking would put `far` first (distance 0
        // vs. `close`'s distance 1). Requesting all rows guarantees both are
        // in the oversampled candidate set, so this isolates the rescore
        // step's own correctness rather than the oversample width.
        let hits = search(&conn, &query, 2).expect("search");
        assert_eq!(
            hits.first().map(|(id, _)| *id),
            Some(close),
            "the nearer-by-true-distance vector must rank first after rescoring: {hits:?}"
        );
    }
}

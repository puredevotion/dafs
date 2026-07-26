//! LLM-derived enrichment (M02b): a replaceable cache keyed by `file_id`,
//! plus the durable work queue that fills it — the same two-part shape
//! `metadata` already uses for M02a's deterministic extraction, deliberately
//! mirrored rather than reusing that module's tables.
//!
//! Kept as its own tables rather than new columns on `file_metadata`: M02a's
//! crate and schema stay LLM-free in substance, not just in comments, and
//! enrichment can be independently enabled, disabled, or re-run without ever
//! touching M02a's already-shipped migration.
//!
//! Unlike `metadata::enqueue`, nothing in this module enqueues a file
//! unconditionally — a file only ever lands in `enrichment_queue` when
//! enrichment is configured, driven by the daemon's extraction worker after
//! a successful extraction with enough text to be worth sending. That
//! decision (is enrichment configured, is there enough text) is the
//! daemon's, not this module's — this module only knows how to record one.

use rusqlite::{Connection, OptionalExtension, params};

use crate::StoreError;
use crate::paths::FileId;

/// One file's LLM-derived fields, as read back from the store.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileEnrichment {
    pub summary: Option<String>,
    /// JSON array, stored as text — see the migration's own comment for why.
    pub keywords: Option<String>,
    /// JSON array, stored as text.
    pub entities: Option<String>,
    pub classification: Option<String>,
    pub enriched_at_unix: i64,
    pub enrichment_version: u32,
}

/// Same cap `metadata::MAX_ATTEMPTS` uses, for the same reason: a file that
/// reliably fails enrichment (a malformed response, an endpoint that's gone
/// away) stops being retried after a handful of attempts rather than
/// spinning a worker on it forever across restarts.
pub const MAX_ATTEMPTS: i64 = 5;

/// Record one file's enrichment result and clear its queue entry, in a
/// single transaction — same reasoning as `metadata::record_extraction`: a
/// crash between the two writes must not either lose a successful result or
/// clear the queue entry for a result that never landed.
pub fn record_enrichment(
    conn: &Connection,
    file_id: FileId,
    e: &FileEnrichment,
) -> Result<(), StoreError> {
    let tx = conn.unchecked_transaction()?;

    tx.execute(
        "INSERT INTO file_enrichment
             (file_id, summary, keywords, entities, classification,
              enriched_at_unix, enrichment_version)
         VALUES (?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(file_id) DO UPDATE SET
             summary            = excluded.summary,
             keywords           = excluded.keywords,
             entities           = excluded.entities,
             classification     = excluded.classification,
             enriched_at_unix   = excluded.enriched_at_unix,
             enrichment_version = excluded.enrichment_version",
        params![
            file_id,
            e.summary,
            e.keywords,
            e.entities,
            e.classification,
            e.enriched_at_unix,
            e.enrichment_version,
        ],
    )?;

    tx.execute("DELETE FROM enrichment_queue WHERE file_id = ?1", [file_id])?;
    tx.commit()?;
    Ok(())
}

/// Enqueue a file for (re-)enrichment, or bump an existing entry's
/// timestamp. Called by the daemon only when enrichment is configured —
/// this function itself has no opinion on that, it just records the queue
/// entry it's asked to.
pub fn enqueue(conn: &Connection, file_id: FileId, queued_at_unix: i64) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO enrichment_queue (file_id, queued_at_unix, attempt_count)
         VALUES (?1, ?2, 0)
         ON CONFLICT(file_id) DO UPDATE SET queued_at_unix = excluded.queued_at_unix",
        params![file_id, queued_at_unix],
    )?;
    Ok(())
}

/// Files still waiting for enrichment, oldest first, capped at
/// [`MAX_ATTEMPTS`].
pub fn pending(conn: &Connection, limit: u32) -> Result<Vec<FileId>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT file_id FROM enrichment_queue
          WHERE attempt_count < ?2
          ORDER BY queued_at_unix ASC LIMIT ?1",
    )?;
    let ids = stmt
        .query_map(params![limit, MAX_ATTEMPTS], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// Record that an enrichment attempt was made for `file_id` — called when a
/// worker picks the file up, before it makes the network call, not when the
/// call finishes. See `metadata::record_attempt` for why this ordering is
/// what makes a crash mid-request still count against [`MAX_ATTEMPTS`].
pub fn record_attempt(conn: &Connection, file_id: FileId) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE enrichment_queue SET attempt_count = attempt_count + 1 WHERE file_id = ?1",
        [file_id],
    )?;
    Ok(())
}

/// Re-queue every file whose recorded enrichment predates `current_version`,
/// resetting its attempt count — a prompt/schema upgrade deserves fresh
/// retries. Unlike `metadata::requeue_stale`, this does **not** also
/// re-queue every never-enriched file: whether a file should be enriched at
/// all depends on configuration and a text-length threshold the daemon
/// decides, not something this module can determine from the schema alone.
pub fn requeue_stale(
    conn: &Connection,
    current_version: u32,
    at_unix: i64,
) -> Result<usize, StoreError> {
    let changed = conn.execute(
        "INSERT INTO enrichment_queue (file_id, queued_at_unix, attempt_count)
         SELECT fe.file_id, ?2, 0
           FROM file_enrichment fe
          WHERE fe.enrichment_version < ?1
         ON CONFLICT(file_id) DO UPDATE SET queued_at_unix = excluded.queued_at_unix,
                                             attempt_count = 0",
        params![current_version, at_unix],
    )?;
    Ok(changed)
}

/// Which facet column an enrichment-based `/facets`-style query aggregates
/// over. Only `classification` for now: `keywords` and `entities` are JSON
/// arrays stored as text, not single-valued facets, so grouping on them
/// directly would return serialized array literals as if they were opaque
/// values — not what a filter dropdown wants. Mirrors
/// `metadata::FacetColumn` for the same reason that one exists: it is the
/// only way to keep every possible query enumerable by reading this file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FacetColumn {
    Classification,
}

impl FacetColumn {
    fn column(self) -> &'static str {
        match self {
            Self::Classification => "classification",
        }
    }
}

/// Distinct values of one enrichment facet column with their counts, most
/// common first. Mirrors `metadata::distinct_facets` exactly, against
/// `file_enrichment` instead of `file_metadata`.
pub fn distinct_facets(
    conn: &Connection,
    column: FacetColumn,
    limit: u32,
) -> Result<Vec<(String, i64)>, StoreError> {
    let col = column.column();
    // `col` is the one fixed string from `FacetColumn::column`, never caller
    // input, so interpolating it into the SQL does not reopen the injection
    // risk parameter binding exists to close.
    let sql = format!(
        "SELECT {col}, COUNT(*) FROM file_enrichment
          WHERE {col} IS NOT NULL
          GROUP BY {col} ORDER BY COUNT(*) DESC LIMIT ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([limit], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Read one file's enrichment, if any has been recorded.
pub fn get(conn: &Connection, file_id: FileId) -> Result<Option<FileEnrichment>, StoreError> {
    conn.query_row(
        "SELECT summary, keywords, entities, classification, enriched_at_unix, enrichment_version
           FROM file_enrichment WHERE file_id = ?1",
        [file_id],
        |r| {
            Ok(FileEnrichment {
                summary: r.get(0)?,
                keywords: r.get(1)?,
                entities: r.get(2)?,
                classification: r.get(3)?,
                enriched_at_unix: r.get(4)?,
                enrichment_version: r.get(5)?,
            })
        },
    )
    .optional()
    .map_err(StoreError::from)
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

    fn sample() -> FileEnrichment {
        FileEnrichment {
            summary: Some("A report about widgets.".into()),
            keywords: Some(r#"["widgets","report"]"#.into()),
            entities: Some(r#"["Acme Corp"]"#.into()),
            classification: Some("business".into()),
            enriched_at_unix: 1_000,
            enrichment_version: 1,
        }
    }

    #[test]
    fn record_then_get_round_trips() {
        let (conn, file) = db_with_file("/a/report.pdf");
        record_enrichment(&conn, file, &sample()).expect("record");

        let got = get(&conn, file).expect("get").expect("present");
        assert_eq!(got, sample());
    }

    #[test]
    fn record_enrichment_clears_the_queue_entry() {
        let (conn, file) = db_with_file("/a/report.pdf");
        enqueue(&conn, file, 500).expect("enqueue");
        assert_eq!(pending(&conn, 10).expect("pending"), vec![file]);

        record_enrichment(&conn, file, &sample()).expect("record");
        assert!(pending(&conn, 10).expect("pending").is_empty());
    }

    #[test]
    fn re_enrichment_overwrites_rather_than_duplicates() {
        let (conn, file) = db_with_file("/a/report.pdf");
        record_enrichment(&conn, file, &sample()).expect("first");

        let mut second = sample();
        second.summary = Some("Revised summary.".into());
        record_enrichment(&conn, file, &second).expect("second");

        let got = get(&conn, file).expect("get").expect("present");
        assert_eq!(got.summary.as_deref(), Some("Revised summary."));

        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM file_enrichment", [], |r| r.get(0))
            .expect("count");
        assert_eq!(rows, 1, "re-enrichment duplicated the row instead of overwriting it");
    }

    #[test]
    fn a_poison_file_stops_being_retried_past_max_attempts() {
        let (conn, file) = db_with_file("/a/poison.pdf");
        enqueue(&conn, file, 100).expect("enqueue");

        for _ in 0..MAX_ATTEMPTS {
            assert_eq!(pending(&conn, 10).expect("pending"), vec![file]);
            record_attempt(&conn, file).expect("attempt");
        }

        assert!(
            pending(&conn, 10).expect("pending").is_empty(),
            "a file past MAX_ATTEMPTS was still offered for dispatch"
        );
    }

    #[test]
    fn requeue_stale_picks_up_old_enrichment_versions_only() {
        let (conn, old) = db_with_file("/a/old.pdf");
        let mut i = Interner::new();
        let never_id = ensure_dir_chain(&conn, &mut i, Path::new("/a")).expect("dir");
        let never_component = i.intern_leaf(&conn, "never.pdf").expect("intern");
        let never = crate::paths::upsert_entry(
            &conn,
            Some(never_id),
            never_component,
            false,
            Some(0),
            Some(0),
        )
        .expect("never leaf");

        let mut old_enrich = sample();
        old_enrich.enrichment_version = 1;
        record_enrichment(&conn, old, &old_enrich).expect("record old");

        let touched = requeue_stale(&conn, 2, 900).expect("requeue");
        assert_eq!(touched, 1, "only the stale, already-enriched file should be touched");

        assert_eq!(
            pending(&conn, 10).expect("pending"),
            vec![old],
            "requeue_stale must not enqueue a never-enriched file — that decision is the daemon's"
        );
        let _ = never; // never-enriched: confirmed absent above, not queued
    }

    #[test]
    fn distinct_facets_counts_and_orders_by_frequency() {
        let (conn, a) = db_with_file("/a");
        let mut i = Interner::new();
        let b_component = i.intern_leaf(&conn, "b").expect("intern b");
        let b = crate::paths::upsert_entry(&conn, None, b_component, false, Some(0), Some(0))
            .expect("upsert b");
        let c_component = i.intern_leaf(&conn, "c").expect("intern c");
        let c = crate::paths::upsert_entry(&conn, None, c_component, false, Some(0), Some(0))
            .expect("upsert c");

        for (id, classification) in [(a, "business"), (b, "business"), (c, "personal")] {
            let mut e = sample();
            e.classification = Some(classification.into());
            record_enrichment(&conn, id, &e).expect("record");
        }

        let facets = distinct_facets(&conn, FacetColumn::Classification, 10).expect("facets");
        assert_eq!(facets, vec![("business".to_string(), 2), ("personal".to_string(), 1)]);
    }

    #[test]
    fn get_on_an_unenriched_file_returns_none() {
        let (conn, file) = db_with_file("/a/unseen.pdf");
        assert!(get(&conn, file).expect("get").is_none());
    }
}

//! Deterministic extraction output (M02a): a replaceable cache keyed by
//! `file_id`, plus the durable extraction work queue that fills it.
//!
//! `events` is a fact log that is never rewritten. This is the opposite kind
//! of table: extraction output is derived from a file's bytes and can always
//! be regenerated, so a re-extraction overwrites the row rather than adding a
//! new one. There is no `NewMetadata`/`update_metadata` split the way
//! `events` has append-only semantics — [`record_extraction`] is the only
//! write path and it always upserts.

use rusqlite::{Connection, OptionalExtension, params};

use crate::StoreError;
use crate::paths::FileId;

/// A file's extracted metadata, as read back from the store.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileMetadata {
    pub doc_type: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub language: Option<String>,
    pub page_count: Option<i64>,
    pub word_count: Option<i64>,
    pub image_taken_at_unix: Option<i64>,
    pub image_camera_model: Option<String>,
    pub git_branch: Option<String>,
    pub git_head_commit: Option<String>,
    pub git_head_author: Option<String>,
    pub git_head_at_unix: Option<i64>,
    pub extracted_at_unix: i64,
    pub extractor_version: u32,
}

/// A file's extraction attempt is retried at most this many times across
/// restarts before the dispatcher stops re-queuing it. The row is left in
/// `extraction_queue` rather than deleted, so a permanently-failing file is
/// still visible (queryable) rather than silently forgotten.
pub const MAX_ATTEMPTS: i64 = 5;

/// Which facet column a `/facets`-style query aggregates over. An enum
/// rather than a raw column name: it is the only way to keep every possible
/// query enumerable by reading this file, the same reasoning `events::timeline`
/// uses for its branched SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FacetColumn {
    DocType,
    Author,
    Language,
    GitBranch,
}

impl FacetColumn {
    fn column(self) -> &'static str {
        match self {
            Self::DocType => "doc_type",
            Self::Author => "author",
            Self::Language => "language",
            Self::GitBranch => "git_branch",
        }
    }
}

/// Record one file's extraction result and clear its queue entry, in a
/// single transaction.
///
/// Splitting these into two writes would repeat M01's orphaned-event bug in
/// a new shape: a crash between "metadata written" and "queue entry cleared"
/// would either lose the fact that extraction succeeded (if the queue entry
/// survives, it gets redone — wasteful but not wrong) or, the other order,
/// clear the queue entry for a file whose metadata never landed (silently
/// losing it forever). One transaction makes both impossible.
pub fn record_extraction(
    conn: &Connection,
    file_id: FileId,
    m: &FileMetadata,
) -> Result<(), StoreError> {
    let tx = conn.unchecked_transaction()?;

    tx.execute(
        "INSERT INTO file_metadata
             (file_id, doc_type, title, author, language, page_count, word_count,
              image_taken_at_unix, image_camera_model, git_branch, git_head_commit,
              git_head_author, git_head_at_unix, extracted_at_unix, extractor_version)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
         ON CONFLICT(file_id) DO UPDATE SET
             doc_type            = excluded.doc_type,
             title               = excluded.title,
             author              = excluded.author,
             language            = excluded.language,
             page_count          = excluded.page_count,
             word_count          = excluded.word_count,
             image_taken_at_unix = excluded.image_taken_at_unix,
             image_camera_model  = excluded.image_camera_model,
             git_branch          = excluded.git_branch,
             git_head_commit     = excluded.git_head_commit,
             git_head_author     = excluded.git_head_author,
             git_head_at_unix    = excluded.git_head_at_unix,
             extracted_at_unix   = excluded.extracted_at_unix,
             extractor_version   = excluded.extractor_version",
        params![
            file_id,
            m.doc_type,
            m.title,
            m.author,
            m.language,
            m.page_count,
            m.word_count,
            m.image_taken_at_unix,
            m.image_camera_model,
            m.git_branch,
            m.git_head_commit,
            m.git_head_author,
            m.git_head_at_unix,
            m.extracted_at_unix,
            m.extractor_version,
        ],
    )?;

    tx.execute("DELETE FROM extraction_queue WHERE file_id = ?1", [file_id])?;
    tx.commit()?;
    Ok(())
}

/// Enqueue a file for (re-)extraction, or bump an existing entry's timestamp.
/// `attempt_count` is left untouched on an existing row — queuing is not the
/// same event as attempting, see [`record_attempt`].
pub fn enqueue(conn: &Connection, file_id: FileId, queued_at_unix: i64) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO extraction_queue (file_id, queued_at_unix, attempt_count)
         VALUES (?1, ?2, 0)
         ON CONFLICT(file_id) DO UPDATE SET queued_at_unix = excluded.queued_at_unix",
        params![file_id, queued_at_unix],
    )?;
    Ok(())
}

/// Files still waiting for extraction, oldest first, capped at
/// [`MAX_ATTEMPTS`] — a dispatcher can hand these straight to worker threads
/// without checking attempt counts itself.
pub fn pending(conn: &Connection, limit: u32) -> Result<Vec<FileId>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT file_id FROM extraction_queue
          WHERE attempt_count < ?2
          ORDER BY queued_at_unix ASC LIMIT ?1",
    )?;
    let ids = stmt
        .query_map(params![limit, MAX_ATTEMPTS], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// Record that a dispatch attempt was made for `file_id`, whether or not it
/// eventually succeeds — called when a worker picks the file up, not when it
/// finishes. A file that keeps crashing its extractor still gets its
/// attempt_count incremented each time, which is what lets [`pending`]'s cap
/// eventually stop retrying it.
pub fn record_attempt(conn: &Connection, file_id: FileId) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE extraction_queue SET attempt_count = attempt_count + 1 WHERE file_id = ?1",
        [file_id],
    )?;
    Ok(())
}

/// Re-queue every file whose recorded metadata predates `current_version`,
/// resetting its attempt count — an extractor upgrade deserves fresh
/// retries, not to inherit a poison-file cap from the previous version's
/// bugs. Also re-queues anything left in `extraction_queue` from a previous
/// run (a crash mid-processing), since that row was never deleted.
pub fn requeue_stale(
    conn: &Connection,
    current_version: u32,
    at_unix: i64,
) -> Result<usize, StoreError> {
    let tx = conn.unchecked_transaction()?;

    let changed = tx.execute(
        "INSERT INTO extraction_queue (file_id, queued_at_unix, attempt_count)
         SELECT fm.file_id, ?2, 0
           FROM file_metadata fm
          WHERE fm.extractor_version < ?1
         ON CONFLICT(file_id) DO UPDATE SET queued_at_unix = excluded.queued_at_unix,
                                             attempt_count = 0",
        params![current_version, at_unix],
    )?;

    // Every file that has never been extracted at all also needs to enter
    // the queue — a fresh scan creates `files` rows and events, but nothing
    // upstream of this module enqueues them, so a startup pass is what
    // guarantees the queue is complete for the extractor_version currently
    // running.
    let changed_new = tx.execute(
        "INSERT INTO extraction_queue (file_id, queued_at_unix, attempt_count)
         SELECT f.id, ?1, 0
           FROM files f
          WHERE f.is_dir = 0 AND f.deleted_at IS NULL
            AND NOT EXISTS (SELECT 1 FROM file_metadata fm WHERE fm.file_id = f.id)
            AND NOT EXISTS (SELECT 1 FROM extraction_queue q WHERE q.file_id = f.id)",
        params![at_unix],
    )?;

    tx.commit()?;
    Ok(changed + changed_new)
}

/// Files still outstanding in the extraction queue, including any parked
/// past [`MAX_ATTEMPTS`] — unlike [`pending`], this is a raw row count, not
/// what a dispatcher should offer next. It exists for `/metrics`'
/// `dafs_extraction_queue_depth` gauge and for tests polling for the queue to
/// drain, where "how many rows are left at all" is the right question, not
/// "how many are still eligible for a retry".
pub fn queue_depth(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row("SELECT COUNT(*) FROM extraction_queue", [], |r| r.get(0))?)
}

/// Read one file's metadata, if any has been recorded.
pub fn get(conn: &Connection, file_id: FileId) -> Result<Option<FileMetadata>, StoreError> {
    conn.query_row(
        "SELECT doc_type, title, author, language, page_count, word_count,
                image_taken_at_unix, image_camera_model, git_branch, git_head_commit,
                git_head_author, git_head_at_unix, extracted_at_unix, extractor_version
           FROM file_metadata WHERE file_id = ?1",
        [file_id],
        |r| {
            Ok(FileMetadata {
                doc_type: r.get(0)?,
                title: r.get(1)?,
                author: r.get(2)?,
                language: r.get(3)?,
                page_count: r.get(4)?,
                word_count: r.get(5)?,
                image_taken_at_unix: r.get(6)?,
                image_camera_model: r.get(7)?,
                git_branch: r.get(8)?,
                git_head_commit: r.get(9)?,
                git_head_author: r.get(10)?,
                git_head_at_unix: r.get(11)?,
                extracted_at_unix: r.get(12)?,
                extractor_version: r.get(13)?,
            })
        },
    )
    .optional()
    .map_err(StoreError::from)
}

/// Distinct values of one facet column with their counts, most common first
/// — what the UI's filter dropdowns are populated from, so the client never
/// has to pull full history to build them.
///
/// `limit` bounds the result the same way `events::MAX_LIMIT` bounds a
/// timeline page: a column with pathologically many distinct values (an
/// `author` field poisoned by a hostile document, say) must not turn one
/// request into an unbounded response.
pub fn distinct_facets(
    conn: &Connection,
    column: FacetColumn,
    limit: u32,
) -> Result<Vec<(String, i64)>, StoreError> {
    let col = column.column();
    // `col` is one of four fixed strings from `FacetColumn::column`, never
    // caller input, so interpolating it into the SQL does not reopen the
    // injection risk parameter binding exists to close.
    let sql = format!(
        "SELECT {col}, COUNT(*) FROM file_metadata
          WHERE {col} IS NOT NULL
          GROUP BY {col} ORDER BY COUNT(*) DESC LIMIT ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([limit], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::paths::{Interner, ensure_dir_chain};

    /// A real leaf file (`is_dir = 0`), not a directory — several tests below
    /// exercise `requeue_stale`'s directory exclusion, which `ensure_dir_chain`
    /// alone can't produce since every component it creates is a directory.
    fn db_with_file(name: &str) -> (Connection, FileId) {
        let conn = crate::open_in_memory().expect("open");
        let mut i = Interner::new();
        let id = leaf_file(&conn, &mut i, name);
        (conn, id)
    }

    fn leaf_file(conn: &Connection, i: &mut Interner, path: &str) -> FileId {
        let path = Path::new(path);
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty() && *p != Path::new("/"));
        let file_name = path.file_name().expect("path has a filename").to_str().expect("utf8");

        let parent_id = parent.map(|p| ensure_dir_chain(conn, i, p).expect("parent chain"));
        let component_id = i.intern_leaf(conn, file_name).expect("intern leaf");
        crate::paths::upsert_entry(conn, parent_id, component_id, false, Some(0), Some(0))
            .expect("upsert leaf")
    }

    fn sample() -> FileMetadata {
        FileMetadata {
            doc_type: Some("pdf".into()),
            title: Some("Report".into()),
            author: Some("Ada".into()),
            language: Some("en".into()),
            page_count: Some(3),
            word_count: Some(500),
            extracted_at_unix: 1_000,
            extractor_version: 1,
            ..Default::default()
        }
    }

    #[test]
    fn record_then_get_round_trips() {
        let (conn, file) = db_with_file("/a/report.pdf");
        record_extraction(&conn, file, &sample()).expect("record");

        let got = get(&conn, file).expect("get").expect("present");
        assert_eq!(got, sample());
    }

    #[test]
    fn record_extraction_clears_the_queue_entry() {
        let (conn, file) = db_with_file("/a/report.pdf");
        enqueue(&conn, file, 500).expect("enqueue");
        assert_eq!(pending(&conn, 10).expect("pending"), vec![file]);

        record_extraction(&conn, file, &sample()).expect("record");
        assert!(pending(&conn, 10).expect("pending").is_empty());
    }

    #[test]
    fn re_extraction_overwrites_rather_than_duplicates() {
        let (conn, file) = db_with_file("/a/report.pdf");
        record_extraction(&conn, file, &sample()).expect("first");

        let mut second = sample();
        second.title = Some("Revised Report".into());
        second.extracted_at_unix = 2_000;
        record_extraction(&conn, file, &second).expect("second");

        let got = get(&conn, file).expect("get").expect("present");
        assert_eq!(got.title.as_deref(), Some("Revised Report"));

        let rows: i64 =
            conn.query_row("SELECT COUNT(*) FROM file_metadata", [], |r| r.get(0)).expect("count");
        assert_eq!(rows, 1, "re-extraction duplicated the row instead of overwriting it");
    }

    #[test]
    fn pending_is_ordered_oldest_first() {
        let (conn, a) = db_with_file("/a");
        let mut i = Interner::new();
        let b = ensure_dir_chain(&conn, &mut i, Path::new("/b")).expect("b");

        enqueue(&conn, b, 200).expect("enqueue b");
        enqueue(&conn, a, 100).expect("enqueue a");

        assert_eq!(pending(&conn, 10).expect("pending"), vec![a, b]);
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

        // Still present, just not offered — visible for diagnosis rather
        // than silently vanishing.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM extraction_queue WHERE file_id = ?1", [file], |r| {
                r.get(0)
            })
            .expect("count");
        assert_eq!(count, 1);
    }

    #[test]
    fn requeue_stale_picks_up_old_extractor_versions_and_unextracted_files() {
        let (conn, old) = db_with_file("/a/old.pdf");
        let mut i = Interner::new();
        let fresh = leaf_file(&conn, &mut i, "/a/fresh.pdf");
        let never = leaf_file(&conn, &mut i, "/a/never.pdf");

        let mut old_meta = sample();
        old_meta.extractor_version = 1;
        record_extraction(&conn, old, &old_meta).expect("record old");

        let mut fresh_meta = sample();
        fresh_meta.extractor_version = 2;
        record_extraction(&conn, fresh, &fresh_meta).expect("record fresh");

        let touched = requeue_stale(&conn, 2, 900).expect("requeue");
        assert_eq!(touched, 2, "expected the stale file and the never-extracted file");

        let mut queued = pending(&conn, 10).expect("pending");
        queued.sort_unstable();
        let mut expected = vec![old, never];
        expected.sort_unstable();
        assert_eq!(queued, expected);
    }

    #[test]
    fn requeue_stale_resets_attempt_count() {
        let (conn, file) = db_with_file("/a/flaky.pdf");
        let mut meta = sample();
        meta.extractor_version = 1;
        record_extraction(&conn, file, &meta).expect("record");
        enqueue(&conn, file, 100).expect("re-enqueue");
        for _ in 0..MAX_ATTEMPTS {
            record_attempt(&conn, file).expect("attempt");
        }
        assert!(pending(&conn, 10).expect("pending").is_empty(), "should be capped out first");

        requeue_stale(&conn, 2, 200).expect("requeue for the version bump");
        assert_eq!(
            pending(&conn, 10).expect("pending"),
            vec![file],
            "an extractor upgrade should reset the attempt cap"
        );
    }

    #[test]
    fn distinct_facets_counts_and_orders_by_frequency() {
        let (conn, a) = db_with_file("/a");
        let mut i = Interner::new();
        let b = ensure_dir_chain(&conn, &mut i, Path::new("/b")).expect("b");
        let c = ensure_dir_chain(&conn, &mut i, Path::new("/c")).expect("c");

        for (id, doc_type) in [(a, "pdf"), (b, "pdf"), (c, "docx")] {
            let mut m = sample();
            m.doc_type = Some(doc_type.into());
            record_extraction(&conn, id, &m).expect("record");
        }

        let facets = distinct_facets(&conn, FacetColumn::DocType, 10).expect("facets");
        assert_eq!(facets, vec![("pdf".to_string(), 2), ("docx".to_string(), 1)]);
    }

    #[test]
    fn get_on_an_unextracted_file_returns_none() {
        let (conn, file) = db_with_file("/a/unseen.pdf");
        assert!(get(&conn, file).expect("get").is_none());
    }

    #[test]
    fn queue_depth_counts_every_row_including_past_max_attempts() {
        let (conn, file) = db_with_file("/a/poison.pdf");
        assert_eq!(queue_depth(&conn).expect("queue_depth"), 0);

        enqueue(&conn, file, 100).expect("enqueue");
        assert_eq!(queue_depth(&conn).expect("queue_depth"), 1);

        for _ in 0..MAX_ATTEMPTS {
            record_attempt(&conn, file).expect("attempt");
        }
        assert!(pending(&conn, 10).expect("pending").is_empty(), "should be capped out");
        assert_eq!(
            queue_depth(&conn).expect("queue_depth"),
            1,
            "a poisoned file must still count towards the depth, just not towards `pending`"
        );

        record_extraction(&conn, file, &sample()).expect("record");
        assert_eq!(queue_depth(&conn).expect("queue_depth"), 0);
    }
}

//! Bridges the SQLite store to the API's [`TimelineStore`] trait.
//!
//! Lives in the daemon rather than in either crate it joins: `dafs-api` should
//! not depend on a particular storage engine, and `dafs-store` should not know
//! about the HTTP layer's DTOs. The binary is the place that knows both.
//!
//! # Concurrency
//!
//! One connection behind a `Mutex`. See `dafs_api::timeline` for why this is not
//! a pool. Every caller reaches this through `spawn_blocking`, so the lock is
//! never held across an await point — the `Mutex` here is `std`'s, deliberately,
//! because an async mutex would invite exactly that mistake.

use std::sync::Mutex;

use dafs_api::{TimelineItem, TimelineStats, TimelineStore};
use dafs_store::enrichment::{self, FacetColumn as EnrichmentFacetColumn};
use dafs_store::events::{EventKind, TimelineQuery};
use dafs_store::metadata::{self, FacetColumn};
use rusqlite::Connection;

/// The shared metadata connection.
pub struct SqliteTimeline {
    conn: Mutex<Connection>,
}

impl SqliteTimeline {
    pub fn new(conn: Connection) -> Self {
        Self { conn: Mutex::new(conn) }
    }

    /// Run `f` against the connection.
    ///
    /// Recovers from lock poisoning rather than propagating it: a panic in some
    /// earlier query says nothing about whether *this* one can run, and the
    /// connection itself is not left in a broken state by an unwound rusqlite
    /// call. Refusing every subsequent request because one panicked would turn a
    /// single failed query into a dead daemon.
    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T, String>) -> Result<T, String> {
        match self.conn.lock() {
            Ok(guard) => f(&guard),
            Err(poisoned) => {
                tracing::warn!("metadata connection mutex was poisoned; recovering");
                f(&poisoned.into_inner())
            }
        }
    }
}

impl TimelineStore for SqliteTimeline {
    fn timeline(
        &self,
        limit: u32,
        before_id: Option<i64>,
        kind: Option<&str>,
        doc_type: Option<&str>,
        author: Option<&str>,
        language: Option<&str>,
        git_branch: Option<&str>,
        classification: Option<&str>,
    ) -> Result<Vec<TimelineItem>, String> {
        // An unparseable kind reaching here would mean the handler's validation
        // was bypassed; treat it as no filter rather than inventing one, since
        // silently filtering on a kind that does not exist returns an empty page
        // that reads as "nothing happened".
        let kind = kind.and_then(EventKind::parse);

        let query = TimelineQuery {
            limit: Some(limit),
            before_id,
            kind,
            doc_type: doc_type.map(String::from),
            author: author.map(String::from),
            language: language.map(String::from),
            git_branch: git_branch.map(String::from),
            classification: classification.map(String::from),
        };

        self.with_conn(|conn| {
            dafs_store::events::timeline(conn, &query)
                .map_err(|e| e.to_string())
                .map(|rows| rows.into_iter().map(to_dto).collect())
        })
    }

    fn stats(&self) -> Result<TimelineStats, String> {
        self.with_conn(|conn| {
            let events = dafs_store::events::count(conn).map_err(|e| e.to_string())?;
            let files = dafs_store::events::file_count(conn).map_err(|e| e.to_string())?;
            let pending_extractions = metadata::queue_depth(conn).map_err(|e| e.to_string())?;
            Ok(TimelineStats { events, files, pending_extractions })
        })
    }

    fn facets(&self, field: &str) -> Result<Vec<(String, i64)>, String> {
        // The handler validates `field` against these same five names before
        // calling here (see `dafs_api::lib`'s `/facets` route); an unrecognised
        // name reaching this far is a bypassed check, not a query this store
        // can answer, so it fails loudly rather than guessing a column.
        //
        // `classification` lives in `file_enrichment`, a different table from
        // the other four, so it goes through a separate enum and query
        // function (`enrichment::FacetColumn`/`distinct_facets`) rather than
        // `metadata::FacetColumn` gaining a variant it has no column for.
        if field == "classification" {
            return self.with_conn(|conn| {
                enrichment::distinct_facets(
                    conn,
                    EnrichmentFacetColumn::Classification,
                    MAX_FACET_VALUES,
                )
                .map_err(|e| e.to_string())
            });
        }

        let column = match field {
            "doc_type" => FacetColumn::DocType,
            "author" => FacetColumn::Author,
            "language" => FacetColumn::Language,
            "git_branch" => FacetColumn::GitBranch,
            other => return Err(format!("unknown facet field: {other}")),
        };

        self.with_conn(|conn| {
            metadata::distinct_facets(conn, column, MAX_FACET_VALUES).map_err(|e| e.to_string())
        })
    }
}

/// Cap on distinct facet values returned in one `/facets` response — bounds
/// the reply the same way `events::MAX_LIMIT` bounds a timeline page, so a
/// column with pathologically many distinct values cannot turn one request
/// into an unbounded response.
const MAX_FACET_VALUES: u32 = 50;

fn to_dto(entry: dafs_store::events::TimelineEntry) -> TimelineItem {
    TimelineItem {
        id: entry.id,
        path: entry.path,
        kind: entry.kind.as_str().to_string(),
        at_unix_ms: entry.at_unix_ms,
        size_bytes: entry.size_bytes,
        is_dir: entry.is_dir,
        previous_path: entry.previous_path,
        doc_type: entry.doc_type,
        title: entry.title,
        author: entry.author,
        language: entry.language,
        page_count: entry.page_count,
        word_count: entry.word_count,
        image_taken_at_unix: entry.image_taken_at_unix,
        image_camera_model: entry.image_camera_model,
        git_branch: entry.git_branch,
        git_head_commit: entry.git_head_commit,
        git_head_author: entry.git_head_author,
        git_head_at_unix: entry.git_head_at_unix,
        summary: entry.summary,
        keywords: entry.keywords,
        entities: entry.entities,
        classification: entry.classification,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use dafs_store::events::{EventKind as StoreKind, NewEvent, append};
    use dafs_store::paths::{Interner, ensure_dir_chain};

    use super::*;

    fn store_with_events() -> SqliteTimeline {
        let conn = dafs_store::open_in_memory().expect("open");
        let mut interner = Interner::new();
        let file =
            ensure_dir_chain(&conn, &mut interner, Path::new("/home/u/a.txt")).expect("file");

        append(&conn, &NewEvent::now(file, StoreKind::Created).at(1_000)).expect("append");
        append(&conn, &NewEvent::now(file, StoreKind::Modified).at(2_000)).expect("append");

        SqliteTimeline::new(conn)
    }

    #[test]
    fn timeline_maps_store_rows_to_dtos() {
        let store = store_with_events();
        let items = store.timeline(10, None, None, None, None, None, None, None).expect("timeline");

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, "modified", "most recent first");
        assert_eq!(items[0].path, "/home/u/a.txt");
    }

    #[test]
    fn filtering_by_kind_reaches_the_store() {
        let store = store_with_events();
        let items = store
            .timeline(10, None, Some("created"), None, None, None, None, None)
            .expect("timeline");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "created");
    }

    /// An unknown kind must not silently filter everything out.
    #[test]
    fn an_unparseable_kind_is_treated_as_no_filter() {
        let store = store_with_events();
        let items = store
            .timeline(10, None, Some("exploded"), None, None, None, None, None)
            .expect("timeline");
        assert_eq!(items.len(), 2, "an unknown kind silently emptied the timeline");
    }

    #[test]
    fn filtering_by_doc_type_reaches_the_store() {
        let store = store_with_events();
        let file = dafs_store::paths::ensure_dir_chain(
            &store.conn.lock().expect("lock"),
            &mut Interner::new(),
            Path::new("/home/u/a.txt"),
        )
        .expect("same file id");
        metadata::record_extraction(
            &store.conn.lock().expect("lock"),
            file,
            &metadata::FileMetadata {
                doc_type: Some("text".into()),
                extracted_at_unix: 1_000,
                extractor_version: 1,
                ..Default::default()
            },
        )
        .expect("record extraction");

        let items =
            store.timeline(10, None, None, Some("text"), None, None, None, None).expect("timeline");
        assert_eq!(items.len(), 2, "both events are for the same extracted file");
        assert!(items.iter().all(|i| i.doc_type.as_deref() == Some("text")));

        let none =
            store.timeline(10, None, None, Some("pdf"), None, None, None, None).expect("timeline");
        assert!(none.is_empty(), "a non-matching doc_type should exclude every row");
    }

    #[test]
    fn filtering_by_classification_reaches_the_store() {
        let store = store_with_events();
        let file = dafs_store::paths::ensure_dir_chain(
            &store.conn.lock().expect("lock"),
            &mut Interner::new(),
            Path::new("/home/u/a.txt"),
        )
        .expect("same file id");
        enrichment::record_enrichment(
            &store.conn.lock().expect("lock"),
            file,
            &enrichment::FileEnrichment {
                classification: Some("business".into()),
                enriched_at_unix: 1_000,
                enrichment_version: 1,
                ..Default::default()
            },
        )
        .expect("record enrichment");

        let items = store
            .timeline(10, None, None, None, None, None, None, Some("business"))
            .expect("timeline");
        assert_eq!(items.len(), 2, "both events are for the same enriched file");
        assert!(items.iter().all(|i| i.classification.as_deref() == Some("business")));

        let none = store
            .timeline(10, None, None, None, None, None, None, Some("personal"))
            .expect("timeline");
        assert!(none.is_empty(), "a non-matching classification should exclude every row");
    }

    #[test]
    fn facets_returns_distinct_values_from_file_metadata() {
        let store = store_with_events();
        let file = dafs_store::paths::ensure_dir_chain(
            &store.conn.lock().expect("lock"),
            &mut Interner::new(),
            Path::new("/home/u/a.txt"),
        )
        .expect("same file id");
        metadata::record_extraction(
            &store.conn.lock().expect("lock"),
            file,
            &metadata::FileMetadata {
                doc_type: Some("text".into()),
                extracted_at_unix: 1_000,
                extractor_version: 1,
                ..Default::default()
            },
        )
        .expect("record extraction");

        let values = store.facets("doc_type").expect("facets");
        assert_eq!(values, vec![("text".to_string(), 1)]);
    }

    #[test]
    fn facets_returns_distinct_values_from_file_enrichment() {
        let store = store_with_events();
        let file = dafs_store::paths::ensure_dir_chain(
            &store.conn.lock().expect("lock"),
            &mut Interner::new(),
            Path::new("/home/u/a.txt"),
        )
        .expect("same file id");
        enrichment::record_enrichment(
            &store.conn.lock().expect("lock"),
            file,
            &enrichment::FileEnrichment {
                classification: Some("business".into()),
                enriched_at_unix: 1_000,
                enrichment_version: 1,
                ..Default::default()
            },
        )
        .expect("record enrichment");

        let values = store.facets("classification").expect("facets");
        assert_eq!(values, vec![("business".to_string(), 1)]);
    }

    #[test]
    fn facets_rejects_an_unknown_field() {
        let store = store_with_events();
        assert!(store.facets("nonsense").is_err());
    }

    #[test]
    fn stats_counts_events_and_files() {
        let store = store_with_events();
        let stats = store.stats().expect("stats");
        assert_eq!(stats.events, 2);
    }

    #[test]
    fn stats_reports_the_extraction_queue_depth() {
        let store = store_with_events();
        assert_eq!(store.stats().expect("stats").pending_extractions, 0);

        let file = dafs_store::paths::ensure_dir_chain(
            &store.conn.lock().expect("lock"),
            &mut Interner::new(),
            Path::new("/home/u/a.txt"),
        )
        .expect("same file id");
        metadata::enqueue(&store.conn.lock().expect("lock"), file, 1_000).expect("enqueue");

        assert_eq!(store.stats().expect("stats").pending_extractions, 1);
    }

    /// A poisoned mutex must not permanently break the daemon.
    #[test]
    fn a_poisoned_lock_is_recovered() {
        let store = std::sync::Arc::new(store_with_events());

        let poisoner = std::sync::Arc::clone(&store);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.conn.lock().expect("lock");
            panic!("poisoning the mutex on purpose");
        })
        .join();

        // The next query must still work.
        let items = store
            .timeline(10, None, None, None, None, None, None, None)
            .expect("timeline after poisoning");
        assert_eq!(items.len(), 2);
    }
}

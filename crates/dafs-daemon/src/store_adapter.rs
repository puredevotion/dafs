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

use std::collections::HashMap;
use std::sync::Mutex;

use dafs_api::{SearchFilters, SearchHit, SearchStore, TimelineItem, TimelineStats, TimelineStore};
use dafs_store::enrichment::{self, FacetColumn as EnrichmentFacetColumn};
use dafs_store::events::{EventKind, TimelineEntry, TimelineQuery};
use dafs_store::metadata::{self, FacetColumn};
use dafs_store::paths::FileId;
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

/// Bridges the SQLite store and `dafs_enrich::embed` to the API's
/// [`SearchStore`] trait — see `dafs_api::search`'s own module docs for why
/// this is a separate trait/adapter from [`SqliteTimeline`] rather than a
/// method grafted onto it.
///
/// A connection of its own, not a share of `SqliteTimeline`'s: same WAL
/// reasoning as every other dedicated connection in this daemon (see
/// `main.rs`'s and `enrich_worker.rs`'s module docs) — a search embeds its
/// query text over the network before ever touching SQLite, and that network
/// call must not sit behind (or hold up) an unrelated timeline request's
/// lock for as long as it takes.
pub struct SqliteSearch {
    conn: Mutex<Connection>,
    config: dafs_enrich::Config,
}

impl SqliteSearch {
    /// `config` is the whole `dafs_enrich::Config`, not just its
    /// `embedding` field — `dafs_enrich::embed` needs `base_url`/`api_key`/
    /// `timeout` too, same as `embed_worker` — but callers should only
    /// construct this when `config.embedding.is_some()` (mirroring
    /// `embed_worker::spawn`'s own contract), since a search against an
    /// unconfigured embedding model can never do anything useful.
    pub fn new(conn: Connection, config: dafs_enrich::Config) -> Self {
        Self { conn: Mutex::new(conn), config }
    }

    /// Mirrors `SqliteTimeline::with_conn` exactly, including the
    /// poisoned-lock recovery.
    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T, String>) -> Result<T, String> {
        match self.conn.lock() {
            Ok(guard) => f(&guard),
            Err(poisoned) => {
                tracing::warn!("search connection mutex was poisoned; recovering");
                f(&poisoned.into_inner())
            }
        }
    }
}

impl SearchStore for SqliteSearch {
    fn search(
        &self,
        query: &str,
        limit: u32,
        filters: &SearchFilters,
    ) -> Result<Vec<SearchHit>, String> {
        // Deliberately outside `with_conn`: this is a blocking network call,
        // and holding the connection's mutex across it would stall every
        // other search (and, if this were sharing `SqliteTimeline`'s
        // connection, every timeline request too) for as long as the
        // embedding endpoint takes to answer.
        let vector = dafs_enrich::embed(query, &self.config).map_err(|e| e.to_string())?;

        // `dafs_store::embeddings::search` has no way to filter *before*
        // ranking — a vec0 query only ever knows about the embedding column,
        // never `file_metadata`/`file_enrichment` — so a filtered search
        // pulls a larger candidate pool and filters/truncates afterwards.
        // See `SearchStore::search`'s own docs on why this can return fewer
        // than `limit` hits rather than over-fetching indefinitely to
        // guarantee an exact count.
        let candidate_limit = if filters.is_empty() {
            limit
        } else {
            limit.saturating_mul(FACET_FILTER_OVERSAMPLE).min(MAX_FACET_CANDIDATE_LIMIT)
        };

        self.with_conn(|conn| {
            let hits = dafs_store::embeddings::search(conn, &vector, candidate_limit)
                .map_err(|e| e.to_string())?;
            let distance_by_file: HashMap<FileId, f64> = hits.iter().copied().collect();
            let file_ids: Vec<FileId> = hits.into_iter().map(|(id, _)| id).collect();

            let entries = dafs_store::events::latest_for_file_ids(conn, &file_ids)
                .map_err(|e| e.to_string())?;

            Ok(entries
                .into_iter()
                // `latest_for_file_ids` preserves `file_ids`' order — the
                // vector search's own ranking — so filtering here keeps
                // whatever order survives it, rather than needing a re-sort.
                .filter(|entry| entry_matches_filters(entry, filters))
                .take(limit as usize)
                .map(|entry| {
                    // Present for every entry `latest_for_file_ids` returned:
                    // it was only ever asked about `file_ids`, drawn from
                    // this same `distance_by_file`'s keys.
                    let distance = distance_by_file
                        .get(&entry.file_id)
                        .copied()
                        .expect("every returned entry's file_id came from hits above");
                    SearchHit { distance, item: to_dto(entry) }
                })
                .collect())
        })
    }
}

/// How many candidates a filtered search pulls from the vector search per
/// requested result, on top of (not instead of)
/// `dafs_store::embeddings::search`'s own internal Hamming/rescore
/// oversampling — a second, independent layer of oversampling because a
/// facet filter can exclude an arbitrary fraction of the corpus, which the
/// vector-distance oversample factor knows nothing about.
const FACET_FILTER_OVERSAMPLE: u32 = 5;

/// Hard ceiling on the candidate pool a filtered search ever requests,
/// regardless of `limit` — bounds the cost of a restrictive filter against a
/// large `limit` the same way `MAX_FACET_VALUES` bounds `/facets`, rather
/// than letting `limit * FACET_FILTER_OVERSAMPLE` grow without one.
const MAX_FACET_CANDIDATE_LIMIT: u32 = 500;

/// Whether `entry` passes every set filter — exact match, same "absent
/// means excluded" rule `dafs_store::events::TimelineQuery`'s own facet
/// filters use, applied here in Rust rather than SQL because
/// `dafs_store::embeddings::search` already returned the ranked candidates
/// by the time this runs.
fn entry_matches_filters(entry: &TimelineEntry, filters: &SearchFilters) -> bool {
    let matches = |filter: &Option<String>, field: &Option<String>| {
        filter.as_deref().is_none_or(|f| field.as_deref() == Some(f))
    };
    matches(&filters.doc_type, &entry.doc_type)
        && matches(&filters.author, &entry.author)
        && matches(&filters.language, &entry.language)
        && matches(&filters.git_branch, &entry.git_branch)
        && matches(&filters.classification, &entry.classification)
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

    /// A minimal HTTP/1.1 server answering one request with a canned
    /// embeddings-endpoint body — mirrors `embed_worker`'s own
    /// `spawn_mock_embedding_server`.
    fn spawn_mock_embedding_server(vector: Vec<f32>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let handle = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else { return };
            let mut buf = [0u8; 4096];
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(200)));
            let _ = stream.read(&mut buf);

            let numbers: Vec<String> = vector.iter().map(|f| f.to_string()).collect();
            let body = format!(r#"{{"data":[{{"embedding":[{}]}}]}}"#, numbers.join(","));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        });

        (format!("http://{addr}"), handle)
    }

    /// The end-to-end property `SqliteSearch` exists for: embed the query
    /// against a (fake) endpoint, find the nearest stored vector, and join it
    /// back to that file's own latest timeline row.
    #[test]
    fn search_embeds_the_query_and_returns_the_nearest_file() {
        let conn = dafs_store::open_in_memory().expect("open");
        let mut interner = Interner::new();
        let a = ensure_dir_chain(&conn, &mut interner, Path::new("/home/u/a.txt")).expect("a");
        let b = ensure_dir_chain(&conn, &mut interner, Path::new("/home/u/b.txt")).expect("b");
        append(&conn, &NewEvent::now(a, StoreKind::Created).at(1_000)).expect("append a");
        append(&conn, &NewEvent::now(b, StoreKind::Created).at(2_000)).expect("append b");

        dafs_store::embeddings::ensure_table(&conn, "test-model", 3).expect("ensure_table");
        dafs_store::embeddings::store(&conn, a, &[1.0, 0.0, 0.0]).expect("store a");
        dafs_store::embeddings::store(&conn, b, &[0.0, 1.0, 0.0]).expect("store b");

        let (base_url, server) = spawn_mock_embedding_server(vec![0.9, 0.1, 0.0]);
        let config = dafs_enrich::Config {
            base_url,
            api_key: None,
            model: "chat-model".to_string(),
            timeout: std::time::Duration::from_secs(5),
            embedding: Some(dafs_enrich::EmbeddingConfig {
                model: "test-model".to_string(),
                dimensions: 3,
            }),
        };

        let search = SqliteSearch::new(conn, config);
        let hits = search.search("looks like a", 10, &SearchFilters::default()).expect("search");
        server.join().expect("mock server thread");

        assert_eq!(hits.first().map(|h| h.item.path.as_str()), Some("/home/u/a.txt"));
        assert!(
            hits.first().map(|h| h.distance).unwrap_or(f64::MAX) < 1.0,
            "the nearest hit should have a small distance: {hits:?}"
        );
    }

    #[test]
    fn search_surfaces_embed_s_own_error_when_unconfigured() {
        let conn = dafs_store::open_in_memory().expect("open");
        let config = dafs_enrich::Config {
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: None,
            model: "chat-model".to_string(),
            timeout: std::time::Duration::from_millis(50),
            embedding: None,
        };

        let search = SqliteSearch::new(conn, config);
        assert!(search.search("anything", 10, &SearchFilters::default()).is_err());
    }

    /// The property `entry_matches_filters` exists for: a facet filter
    /// excludes a candidate whose own metadata doesn't match it, even though
    /// the vector search itself ranked it first.
    #[test]
    fn search_applies_facet_filters_to_vector_search_candidates() {
        let conn = dafs_store::open_in_memory().expect("open");
        let mut interner = Interner::new();
        let a = ensure_dir_chain(&conn, &mut interner, Path::new("/home/u/a.docx")).expect("a");
        let b = ensure_dir_chain(&conn, &mut interner, Path::new("/home/u/b.pdf")).expect("b");
        append(&conn, &NewEvent::now(a, StoreKind::Created).at(1_000)).expect("append a");
        append(&conn, &NewEvent::now(b, StoreKind::Created).at(2_000)).expect("append b");
        metadata::record_extraction(
            &conn,
            a,
            &metadata::FileMetadata {
                doc_type: Some("docx".into()),
                extracted_at_unix: 1_000,
                extractor_version: 1,
                ..Default::default()
            },
        )
        .expect("record a");
        metadata::record_extraction(
            &conn,
            b,
            &metadata::FileMetadata {
                doc_type: Some("pdf".into()),
                extracted_at_unix: 1_000,
                extractor_version: 1,
                ..Default::default()
            },
        )
        .expect("record b");

        dafs_store::embeddings::ensure_table(&conn, "test-model", 3).expect("ensure_table");
        // `a` is the nearer vector — an unfiltered search would rank it
        // first — but the filter below asks for `doc_type=pdf`, which only
        // `b` has.
        dafs_store::embeddings::store(&conn, a, &[1.0, 0.0, 0.0]).expect("store a");
        dafs_store::embeddings::store(&conn, b, &[0.0, 1.0, 0.0]).expect("store b");

        let (base_url, server) = spawn_mock_embedding_server(vec![0.9, 0.1, 0.0]);
        let config = dafs_enrich::Config {
            base_url,
            api_key: None,
            model: "chat-model".to_string(),
            timeout: std::time::Duration::from_secs(5),
            embedding: Some(dafs_enrich::EmbeddingConfig {
                model: "test-model".to_string(),
                dimensions: 3,
            }),
        };

        let search = SqliteSearch::new(conn, config);
        let filters = SearchFilters { doc_type: Some("pdf".to_string()), ..Default::default() };
        let hits = search.search("looks like a", 10, &filters).expect("search");
        server.join().expect("mock server thread");

        assert_eq!(hits.len(), 1, "the docx hit should have been filtered out: {hits:?}");
        assert_eq!(hits[0].item.path, "/home/u/b.pdf");
    }
}

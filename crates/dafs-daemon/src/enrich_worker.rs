//! The enrichment worker: drains `dafs_store::enrichment`'s queue, running
//! `dafs_enrich::enrich` against whatever it finds.
//!
//! Closely mirrors `extract_worker`'s shape — a single dedicated thread with
//! its own connection, an attempt-then-work ordering, a poll-when-empty
//! loop — but exists for a narrower reason: unlike extraction, this worker
//! is only ever spawned when the daemon was actually given an LLM endpoint
//! (`main.rs`'s `llm_config.is_some()`). No endpoint means no thread, no
//! connection, and no polling at all — the "0 cost when not enriching"
//! property `docs/roadmap-and-design-review.md`'s original local-model idea
//! wanted, now trivially true because there is nothing local to keep idle.
//!
//! # A fourth connection
//!
//! Same WAL reasoning as the observer's and `extract_worker`'s own
//! connections (see `main.rs`'s and `extract_worker.rs`'s module docs): an
//! enrichment call can block on a real network round trip for up to
//! `dafs_enrich::Config::timeout`, and putting that behind any other
//! connection's lock would stall an unrelated timeline request or a live
//! extraction for as long as one enrichment call takes.
//!
//! # No second timeout layer
//!
//! `extract_worker::extract_with_timeout` runs each extraction on a
//! throwaway thread specifically because a parsing library can loop forever
//! on hostile input without ever erroring. `dafs_enrich::enrich` has no
//! comparable failure mode to guard against here — it is a single blocking
//! HTTP call already bounded by its own `Config::timeout`, so that timeout
//! alone is the real bound; wrapping it in a second spawn-and-join-with-
//! timeout would only add complexity without buying anything the first
//! layer doesn't already guarantee.
//!
//! # Attempt-then-work ordering
//!
//! Same reasoning as `extract_worker`'s own comment on this:
//! `dafs_store::enrichment::record_attempt` runs before the network call is
//! even attempted, so a crash mid-request still counts as a used attempt on
//! restart rather than retrying a wedging file forever.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dafs_enrich::{Config, Enrichment};
use dafs_store::enrichment::FileEnrichment;
use dafs_store::paths::FileId;
use rusqlite::Connection;

/// Files pulled from the queue per poll. Same value as `extract_worker::BATCH`
/// for the same reason: one thread, so a bigger batch buys nothing but a
/// longer stretch between stop-flag checks.
const BATCH: u32 = 16;

/// How long to sleep when the queue is empty before polling again. Mirrors
/// `extract_worker::IDLE_POLL`.
const IDLE_POLL: Duration = Duration::from_millis(500);

/// Handle to the enrichment worker thread.
pub struct EnrichWorker {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl EnrichWorker {
    /// Mirrors `ExtractWorker::shutdown` exactly: flip the stop flag, then
    /// join — the worker checks the flag between every file and on every
    /// idle-sleep wakeup, so it exits within roughly one poll interval.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            match handle.join() {
                Ok(()) => tracing::debug!("enrichment worker stopped"),
                Err(_) => tracing::warn!("enrichment worker thread panicked"),
            }
        }
    }
}

/// Start the enrichment worker thread. Callers must only call this when
/// enrichment is actually configured — see the module docs on why an
/// unconfigured daemon never calls this at all rather than calling it with
/// some "disabled" sentinel.
pub fn spawn(db_path: &std::path::Path, config: Config) -> anyhow::Result<EnrichWorker> {
    use anyhow::Context as _;

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let db_path = db_path.to_path_buf();

    let handle = std::thread::Builder::new()
        .name("dafs-enrich".into())
        .spawn(move || run(&db_path, &thread_stop, &config))
        .context("spawning the enrichment worker thread")?;

    Ok(EnrichWorker { stop, handle: Some(handle) })
}

/// The worker's main loop, run on its own connection and its own thread.
fn run(db_path: &std::path::Path, stop: &AtomicBool, config: &Config) {
    let conn = match dafs_store::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("enrichment worker could not open the store: {e}");
            return;
        }
    };

    // Once at startup, same reasoning as `extract_worker`'s own
    // `requeue_stale` call: a prompt/schema upgrade (a bumped
    // `ENRICHMENT_VERSION`) should reprocess everything a prior version
    // already enriched. Never-enriched files are not this call's job — see
    // `dafs_store::enrichment::requeue_stale`'s own docs on why that
    // decision belongs to the daemon's extraction success path instead.
    if let Err(e) = dafs_store::enrichment::requeue_stale(
        &conn,
        dafs_enrich::ENRICHMENT_VERSION,
        crate::now_unix(),
    ) {
        tracing::error!("could not requeue stale enrichments: {e}");
    }

    while !stop.load(Ordering::Acquire) {
        let ids = match dafs_store::enrichment::pending(&conn, BATCH) {
            Ok(ids) => ids,
            Err(e) => {
                tracing::error!("could not poll the enrichment queue: {e}");
                std::thread::sleep(IDLE_POLL);
                continue;
            }
        };

        if ids.is_empty() {
            std::thread::sleep(IDLE_POLL);
            continue;
        }

        for file_id in ids {
            if stop.load(Ordering::Acquire) {
                return;
            }
            process_one(&conn, file_id, config);
        }
    }
}

/// Enrich one queued file, recording either its enrichment or nothing —
/// same non-propagating shape as `extract_worker::process_one`:
/// `dafs_store::enrichment::MAX_ATTEMPTS` is what bounds a failure's cost,
/// not this function's caller.
fn process_one(conn: &Connection, file_id: FileId, config: &Config) {
    // Recorded before the network call is even attempted — see the module
    // docs on why this ordering is what crash-consistency requires here.
    if let Err(e) = dafs_store::enrichment::record_attempt(conn, file_id) {
        tracing::warn!(file_id, "could not record an enrichment attempt: {e}");
        return;
    }

    let text = match dafs_store::metadata::get(conn, file_id) {
        Ok(Some(metadata)) => metadata.body_text,
        Ok(None) => {
            tracing::warn!(file_id, "no extracted metadata for a queued enrichment; skipping");
            return;
        }
        Err(e) => {
            tracing::warn!(file_id, "could not read metadata for enrichment: {e}");
            return;
        }
    };

    // Should not normally happen — nothing enqueues a file without body
    // text (see `extract_worker::maybe_enqueue_enrichment`) — but a
    // defensive check here costs nothing against a queue entry that somehow
    // outlived the file it pointed to having text at all.
    let Some(text) = text else {
        tracing::warn!(file_id, "queued for enrichment with no body text; skipping");
        return;
    };

    match dafs_enrich::enrich(&text, config) {
        Ok(enrichment) => {
            let fe = to_file_enrichment(enrichment);
            if let Err(e) = dafs_store::enrichment::record_enrichment(conn, file_id, &fe) {
                tracing::warn!(file_id, "could not record enrichment result: {e}");
            }
        }
        Err(e) => {
            tracing::warn!(file_id, "enrichment failed: {e}");
        }
    }
}

/// `Enrichment`'s `Vec<String>` fields become `FileEnrichment`'s JSON-encoded
/// `Option<String>` columns (see that struct's own doc comment on why they're
/// stored as text). An empty vector becomes `None` rather than
/// `Some("[]")` — the model producing zero keywords/entities and the field
/// never having been asked about are indistinguishable in `Enrichment`
/// itself, so there's no information lost by treating "empty" as "nothing to
/// store", and it keeps `file_enrichment` rows the same shape M02a's
/// `file_metadata` already uses for its own all-optional fields.
fn to_file_enrichment(e: Enrichment) -> FileEnrichment {
    FileEnrichment {
        summary: e.summary,
        keywords: encode_list(e.keywords),
        entities: encode_list(e.entities),
        classification: e.classification,
        enriched_at_unix: crate::now_unix(),
        enrichment_version: dafs_enrich::ENRICHMENT_VERSION,
    }
}

fn encode_list(items: Vec<String>) -> Option<String> {
    if items.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&items).expect("Vec<String> serializes infallibly"))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::Path;

    use dafs_store::metadata::FileMetadata;
    use dafs_store::paths::{Interner, ensure_dir_chain};

    use super::*;

    #[test]
    fn a_populated_enrichment_converts_field_for_field() {
        let e = Enrichment {
            summary: Some("A report.".into()),
            keywords: vec!["a".into(), "b".into()],
            entities: vec!["Acme".into()],
            classification: Some("business".into()),
        };
        let fe = to_file_enrichment(e);
        assert_eq!(fe.summary.as_deref(), Some("A report."));
        assert_eq!(fe.keywords.as_deref(), Some(r#"["a","b"]"#));
        assert_eq!(fe.entities.as_deref(), Some(r#"["Acme"]"#));
        assert_eq!(fe.classification.as_deref(), Some("business"));
        assert_eq!(fe.enrichment_version, dafs_enrich::ENRICHMENT_VERSION);
    }

    #[test]
    fn empty_keyword_and_entity_lists_become_none_not_an_empty_json_array() {
        let e = Enrichment {
            summary: Some("Ok.".into()),
            keywords: Vec::new(),
            entities: Vec::new(),
            classification: None,
        };
        let fe = to_file_enrichment(e);
        assert_eq!(fe.keywords, None, "an empty list must store as None, not Some(\"[]\")");
        assert_eq!(fe.entities, None);
    }

    /// A minimal HTTP/1.1 server that answers exactly one request with a
    /// canned chat-completions-shaped body, standing in for a real LLM
    /// endpoint. Good enough for one `enrich()` call: it does not need to
    /// parse the request at all, only to accept the connection and reply.
    fn spawn_mock_llm_server(reply_json: String) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let handle = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else { return };
            // Drain whatever the client has already written without waiting
            // for it to close the connection — enough for ureq's write to
            // land in the kernel buffer even though this never reads the
            // client's own end-of-body.
            let mut buf = [0u8; 4096];
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            let _ = stream.read(&mut buf);

            let body = format!(
                r#"{{"choices":[{{"message":{{"content":{}}}}}]}}"#,
                serde_json::to_string(&reply_json).expect("string serializes infallibly")
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        });

        (format!("http://{addr}"), handle)
    }

    fn db_with_extracted_file(body_text: &str) -> (Connection, FileId) {
        let conn = dafs_store::open_in_memory().expect("open");
        let mut i = Interner::new();
        let file_id = ensure_dir_chain(&conn, &mut i, Path::new("/a/report.docx")).expect("id");
        let metadata =
            FileMetadata { body_text: Some(body_text.to_string()), ..Default::default() };
        dafs_store::metadata::record_extraction(&conn, file_id, &metadata).expect("record");
        (conn, file_id)
    }

    /// The end-to-end property the task asks for: a file with enough body
    /// text, once enqueued and processed against a real (if fake) endpoint,
    /// ends up with a genuine `file_enrichment` row and an empty queue.
    #[test]
    fn processing_a_pending_file_against_a_mock_endpoint_records_real_enrichment() {
        let (conn, file_id) = db_with_extracted_file("word ".repeat(100).trim());
        dafs_store::enrichment::enqueue(&conn, file_id, 1).expect("enqueue");

        let reply = r#"{"summary": "A short report.", "keywords": ["report"], "entities": [], "classification": "note"}"#;
        let (base_url, server) = spawn_mock_llm_server(reply.to_string());

        let config = Config {
            base_url,
            api_key: None,
            model: "test-model".to_string(),
            timeout: Duration::from_secs(5),
        };

        process_one(&conn, file_id, &config);
        server.join().expect("mock server thread");

        let recorded = dafs_store::enrichment::get(&conn, file_id).expect("get").expect("recorded");
        assert_eq!(recorded.summary.as_deref(), Some("A short report."));
        assert_eq!(recorded.keywords.as_deref(), Some(r#"["report"]"#));
        assert_eq!(recorded.classification.as_deref(), Some("note"));
        assert!(
            dafs_store::enrichment::pending(&conn, 10).expect("pending").is_empty(),
            "a successfully enriched file must be cleared from the queue"
        );
    }

    /// A file whose metadata row exists but has no body text (should not
    /// normally happen — see `process_one`'s own comment) must be skipped
    /// without panicking, and left for `MAX_ATTEMPTS` to eventually bound.
    #[test]
    fn a_queued_file_with_no_body_text_is_skipped_without_panicking() {
        let (conn, file_id) = {
            let conn = dafs_store::open_in_memory().expect("open");
            let mut i = Interner::new();
            let file_id = ensure_dir_chain(&conn, &mut i, Path::new("/a/image.jpg")).expect("id");
            let metadata = FileMetadata { body_text: None, ..Default::default() };
            dafs_store::metadata::record_extraction(&conn, file_id, &metadata).expect("record");
            (conn, file_id)
        };
        dafs_store::enrichment::enqueue(&conn, file_id, 1).expect("enqueue");

        let config = Config {
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: None,
            model: "unused".to_string(),
            timeout: Duration::from_millis(50),
        };

        process_one(&conn, file_id, &config);

        assert!(
            dafs_store::enrichment::get(&conn, file_id).expect("get").is_none(),
            "no enrichment should be recorded for a file with no body text"
        );
        assert_eq!(
            dafs_store::enrichment::pending(&conn, 10).expect("pending"),
            vec![file_id],
            "the queue entry stays for MAX_ATTEMPTS to eventually bound, same as extract_worker"
        );
    }

    /// A network failure (nothing listening) must leave the file queued
    /// rather than recording a bogus enrichment — the attempt was already
    /// counted, and `MAX_ATTEMPTS` is what bounds the retries.
    #[test]
    fn a_connection_failure_leaves_the_file_queued_for_retry() {
        let (conn, file_id) = db_with_extracted_file(&"word ".repeat(100));
        dafs_store::enrichment::enqueue(&conn, file_id, 1).expect("enqueue");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);

        let config = Config {
            base_url: format!("http://127.0.0.1:{port}"),
            api_key: None,
            model: "unused".to_string(),
            timeout: Duration::from_secs(1),
        };

        process_one(&conn, file_id, &config);

        assert!(dafs_store::enrichment::get(&conn, file_id).expect("get").is_none());
        assert_eq!(dafs_store::enrichment::pending(&conn, 10).expect("pending"), vec![file_id]);
    }
}

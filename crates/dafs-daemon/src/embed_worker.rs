//! The embedding worker: drains `dafs_store::embeddings`'s queue, running
//! `dafs_enrich::embed` against whatever it finds and storing the result for
//! M03 semantic search.
//!
//! Mirrors `enrich_worker`'s shape closely — a single dedicated thread with
//! its own connection, attempt-then-work ordering, a poll-when-empty loop —
//! for the same reasons that worker documents. Two differences from it:
//!
//! # `ensure_table` runs once, at startup
//!
//! Unlike `file_metadata`/`file_enrichment` (static `MIGRATIONS` entries),
//! `file_embedding`/`file_embedding_bin` are created on demand, sized to
//! whatever dimensionality the configured embedding model produces — see
//! `dafs_store::embeddings`'s own module docs for why. This worker is the
//! one place that calls `ensure_table`, once before the poll loop starts, so
//! every `store` call after that point can assume the tables already exist
//! with the right width. A `DimensionMismatch` here (a deployment's admin
//! changed `--llm-embedding-model`/`--llm-embedding-dimensions` without
//! pointing at a fresh data directory) is unrecoverable for this worker
//! specifically, so it logs loudly and exits rather than spinning on an
//! error every poll would repeat identically forever.
//!
//! # No `requeue_stale` equivalent
//!
//! `enrich_worker` requeues everything a prior `ENRICHMENT_VERSION` already
//! processed, because a prompt/schema change can improve on old output
//! in-place. There is no comparable "re-embed with a newer version of the
//! same model" concept here: a model/width change is a `DimensionMismatch`
//! (see above), not a version bump the daemon can transparently migrate
//! through — `ensure_table`'s own docs say as much ("point at a different
//! data directory or re-embed from scratch").

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dafs_enrich::Config;
use dafs_store::paths::FileId;
use rusqlite::Connection;

/// Mirrors `enrich_worker::BATCH` exactly, same reasoning: one thread, so a
/// bigger batch only lengthens the stretch between stop-flag checks.
const BATCH: u32 = 16;

/// Mirrors `enrich_worker::IDLE_POLL`.
const IDLE_POLL: Duration = Duration::from_millis(500);

/// Handle to the embedding worker thread.
pub struct EmbedWorker {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl EmbedWorker {
    /// Mirrors `EnrichWorker::shutdown` exactly.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            match handle.join() {
                Ok(()) => tracing::debug!("embedding worker stopped"),
                Err(_) => tracing::warn!("embedding worker thread panicked"),
            }
        }
    }
}

/// Start the embedding worker thread. Callers must only call this when
/// `config.embedding` is actually `Some` — see the module docs on why an
/// unconfigured daemon never calls this at all, mirroring `enrich_worker`.
pub fn spawn(db_path: &std::path::Path, config: Config) -> anyhow::Result<EmbedWorker> {
    use anyhow::Context as _;

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let db_path = db_path.to_path_buf();

    let handle = std::thread::Builder::new()
        .name("dafs-embed".into())
        .spawn(move || run(&db_path, &thread_stop, &config))
        .context("spawning the embedding worker thread")?;

    Ok(EmbedWorker { stop, handle: Some(handle) })
}

/// The worker's main loop, run on its own connection and its own thread.
fn run(db_path: &std::path::Path, stop: &AtomicBool, config: &Config) {
    let conn = match dafs_store::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("embedding worker could not open the store: {e}");
            return;
        }
    };

    // `spawn`'s caller only calls this when `config.embedding.is_some()` —
    // see its own docs — so this `expect` is a consequence of that contract,
    // not a runtime guess.
    let embedding_config =
        config.embedding.as_ref().expect("embed_worker::spawn is only called when configured");

    // See the module docs' *`ensure_table` runs once, at startup* section on
    // why a failure here is fatal to this worker rather than retried.
    if let Err(e) = dafs_store::embeddings::ensure_table(
        &conn,
        &embedding_config.model,
        embedding_config.dimensions,
    ) {
        tracing::error!("could not prepare the embedding tables: {e}");
        return;
    }

    while !stop.load(Ordering::Acquire) {
        let ids = match dafs_store::embeddings::pending(&conn, BATCH) {
            Ok(ids) => ids,
            Err(e) => {
                tracing::error!("could not poll the embedding queue: {e}");
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

/// Embed one queued file, recording either its embedding or nothing — same
/// non-propagating shape as `enrich_worker::process_one`:
/// `dafs_store::embeddings::MAX_ATTEMPTS` is what bounds a failure's cost,
/// not this function's caller.
fn process_one(conn: &Connection, file_id: FileId, config: &Config) {
    // Recorded before the network call is even attempted — same
    // crash-consistency reasoning as `enrich_worker::process_one`.
    if let Err(e) = dafs_store::embeddings::record_attempt(conn, file_id) {
        tracing::warn!(file_id, "could not record an embedding attempt: {e}");
        return;
    }

    let text = match dafs_store::metadata::get(conn, file_id) {
        Ok(Some(metadata)) => metadata.body_text,
        Ok(None) => {
            tracing::warn!(file_id, "no extracted metadata for a queued embedding; skipping");
            return;
        }
        Err(e) => {
            tracing::warn!(file_id, "could not read metadata for embedding: {e}");
            return;
        }
    };

    // Should not normally happen — nothing enqueues a file without body text
    // (see `extract_worker::maybe_enqueue_embedding`) — but costs nothing to
    // guard against a queue entry that somehow outlived the file it pointed
    // to having text at all.
    let Some(text) = text else {
        tracing::warn!(file_id, "queued for embedding with no body text; skipping");
        return;
    };

    match dafs_enrich::embed(&text, config) {
        Ok(vector) => {
            if let Err(e) = dafs_store::embeddings::store(conn, file_id, &vector) {
                tracing::warn!(file_id, "could not record embedding result: {e}");
            }
        }
        Err(e) => {
            tracing::warn!(file_id, "embedding failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::Path;

    use dafs_enrich::EmbeddingConfig;
    use dafs_store::metadata::FileMetadata;
    use dafs_store::paths::{Interner, ensure_dir_chain};

    use super::*;

    /// A minimal HTTP/1.1 server that answers exactly one request with a
    /// canned embeddings-endpoint body, standing in for a real endpoint.
    /// Mirrors `enrich_worker`'s own `spawn_mock_llm_server`.
    fn spawn_mock_embedding_server(vector: Vec<f32>) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let handle = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else { return };
            let mut buf = [0u8; 4096];
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
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

    fn db_with_extracted_file(body_text: &str) -> (Connection, FileId) {
        let conn = dafs_store::open_in_memory().expect("open");
        let mut i = Interner::new();
        let file_id = ensure_dir_chain(&conn, &mut i, Path::new("/a/report.docx")).expect("id");
        let metadata =
            FileMetadata { body_text: Some(body_text.to_string()), ..Default::default() };
        dafs_store::metadata::record_extraction(&conn, file_id, &metadata).expect("record");
        (conn, file_id)
    }

    /// The end-to-end property: a file with enough body text, once enqueued
    /// and processed against a real (if fake) endpoint, ends up with a
    /// genuine `file_embedding` row and an empty queue.
    #[test]
    fn processing_a_pending_file_against_a_mock_endpoint_records_a_real_embedding() {
        let (conn, file_id) = db_with_extracted_file("word ".repeat(100).trim());
        dafs_store::embeddings::enqueue(&conn, file_id, 1).expect("enqueue");
        dafs_store::embeddings::ensure_table(&conn, "test-model", 3).expect("ensure_table");

        let (base_url, server) = spawn_mock_embedding_server(vec![0.1, 0.2, 0.3]);

        let config = Config {
            base_url,
            api_key: None,
            model: "chat-model".to_string(),
            timeout: Duration::from_secs(5),
            embedding: Some(EmbeddingConfig { model: "test-model".to_string(), dimensions: 3 }),
        };

        process_one(&conn, file_id, &config);
        server.join().expect("mock server thread");

        let hits = dafs_store::embeddings::search(&conn, &[0.1, 0.2, 0.3], 1).expect("search");
        assert_eq!(hits.first().map(|(id, _)| *id), Some(file_id));
        assert!(
            dafs_store::embeddings::pending(&conn, 10).expect("pending").is_empty(),
            "a successfully embedded file must be cleared from the queue"
        );
    }

    /// A file whose metadata row exists but has no body text must be skipped
    /// without panicking, mirroring `enrich_worker`'s own test of this.
    #[test]
    fn a_queued_file_with_no_body_text_is_skipped_without_panicking() {
        let conn = dafs_store::open_in_memory().expect("open");
        let mut i = Interner::new();
        let file_id = ensure_dir_chain(&conn, &mut i, Path::new("/a/image.jpg")).expect("id");
        let metadata = FileMetadata { body_text: None, ..Default::default() };
        dafs_store::metadata::record_extraction(&conn, file_id, &metadata).expect("record");
        dafs_store::embeddings::enqueue(&conn, file_id, 1).expect("enqueue");
        dafs_store::embeddings::ensure_table(&conn, "test-model", 3).expect("ensure_table");

        let config = Config {
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: None,
            model: "chat-model".to_string(),
            timeout: Duration::from_millis(50),
            embedding: Some(EmbeddingConfig { model: "test-model".to_string(), dimensions: 3 }),
        };

        process_one(&conn, file_id, &config);

        assert_eq!(
            dafs_store::embeddings::pending(&conn, 10).expect("pending"),
            vec![file_id],
            "the queue entry stays for MAX_ATTEMPTS to eventually bound, same as enrich_worker"
        );
    }

    /// A network failure must leave the file queued rather than recording a
    /// bogus embedding.
    #[test]
    fn a_connection_failure_leaves_the_file_queued_for_retry() {
        let (conn, file_id) = db_with_extracted_file(&"word ".repeat(100));
        dafs_store::embeddings::enqueue(&conn, file_id, 1).expect("enqueue");
        dafs_store::embeddings::ensure_table(&conn, "test-model", 3).expect("ensure_table");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);

        let config = Config {
            base_url: format!("http://127.0.0.1:{port}"),
            api_key: None,
            model: "chat-model".to_string(),
            timeout: Duration::from_secs(1),
            embedding: Some(EmbeddingConfig { model: "test-model".to_string(), dimensions: 3 }),
        };

        process_one(&conn, file_id, &config);

        assert_eq!(dafs_store::embeddings::pending(&conn, 10).expect("pending"), vec![file_id]);
    }

    /// A response whose vector width doesn't match the configured
    /// dimensionality must not be stored — `dafs_enrich::embed` already
    /// rejects it, and this proves that rejection actually stops the worker
    /// from calling `store` at all rather than passing a mismatched vector
    /// further down the pipe.
    #[test]
    fn a_wrong_width_response_is_not_stored() {
        let (conn, file_id) = db_with_extracted_file(&"word ".repeat(100));
        dafs_store::embeddings::enqueue(&conn, file_id, 1).expect("enqueue");
        dafs_store::embeddings::ensure_table(&conn, "test-model", 3).expect("ensure_table");

        let (base_url, server) = spawn_mock_embedding_server(vec![0.1, 0.2]);

        let config = Config {
            base_url,
            api_key: None,
            model: "chat-model".to_string(),
            timeout: Duration::from_secs(5),
            embedding: Some(EmbeddingConfig { model: "test-model".to_string(), dimensions: 3 }),
        };

        process_one(&conn, file_id, &config);
        server.join().expect("mock server thread");

        assert!(
            dafs_store::embeddings::search(&conn, &[0.1, 0.2, 0.0], 10).expect("search").is_empty(),
            "a wrong-width response must not be stored as an embedding"
        );
    }
}

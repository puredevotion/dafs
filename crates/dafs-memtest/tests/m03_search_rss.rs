//! Steady-state RSS with M03 search configured and serving.
//!
//! `docs/memory-budget.md` §8.4 sets this ceiling at 96 MiB — double the
//! plain-observer 32 MiB `rss_ceiling.rs`/`extraction_queue_rss.rs` assert —
//! specifically *because* embeddings are configured and `file_embedding`/
//! `file_embedding_bin` exist, not merely because a daemon is running. This
//! is the scenario `docs/m03-semantic-search.md`'s "Next" list called out as
//! missing.
//!
//! # Small corpus, same reasoning as `extraction_queue_rss.rs`
//!
//! `docs/memory-budget.md` §8.3's binary-quantization design is what makes
//! the ceiling reachable at a real (1M-document) corpus size — proving *that*
//! needs the golden corpus §6 item 3 describes, not a CI-sized fixture set.
//! What this test proves instead, at a scale that runs in seconds: the M03
//! code path (embedding worker thread, its own connection, `file_embedding`/
//! `file_embedding_bin`, a served `/search` query) does not cost meaningfully
//! more resident memory than the plain observer+extraction baseline already
//! does — the same "does this feature's shape regress the ceiling" question
//! `extraction_queue_rss.rs` asks of M02a's extraction queue, asked here of
//! M03's embedding queue and search route instead.
//!
//! Plain `.txt` files won't do here the way they do in
//! `extraction_queue_rss.rs`: `dafs_extract::extract` never populates
//! `body_text` for `DocType::Text` (see that crate's own doc-type match), so
//! nothing would ever reach `has_enough_text_to_enrich` and get queued for
//! embedding at all. Minimal `.docx` fixtures are the smallest real
//! `body_text`-populating doc type available without pdfium.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use dafs_memtest::{Daemon, binary, ceilings, metric_value};

/// Same reasoning as `rss_ceiling.rs`'s/`extraction_queue_rss.rs`'s
/// `SETTLE`: `dirty_decay_ms` is 1000, so wait past that before reading RSS.
const SETTLE: Duration = Duration::from_millis(2500);

/// Same scale as `extraction_queue_rss.rs`'s `FILE_COUNT` and the same
/// reasoning: this test is about the embedding queue draining and RSS
/// settling afterwards, not about throughput at any real corpus size.
const FILE_COUNT: usize = 20;

/// The dimensionality the mock embedding endpoint answers with — small and
/// arbitrary, since nothing here measures recall, only that the pipeline
/// runs end to end.
const EMBEDDING_DIMENSIONS: usize = 8;

fn skip(reason: &str) {
    eprintln!("SKIP: {reason}");
}

/// Builds an in-memory zip from `(entry name, contents)` pairs — copied from
/// `dafs-extract`'s own `office.rs` test helper of the same name and for the
/// same reason: a real docx-shaped archive without committing a binary
/// fixture for parts the reader code never looks at.
fn build_zip(parts: &[(&str, &str)]) -> Vec<u8> {
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    let mut cursor = std::io::Cursor::new(Vec::new());
    let mut zip = ZipWriter::new(&mut cursor);
    let options = SimpleFileOptions::default();
    for (name, contents) in parts {
        zip.start_file(*name, options).expect("start_file");
        zip.write_all(contents.as_bytes()).expect("write_all");
    }
    zip.finish().expect("finish");
    cursor.into_inner()
}

/// A minimal `.docx` whose body text clears
/// `extract_worker::MIN_CHARS_FOR_ENRICHMENT` (300 chars) — this test's whole
/// reason for using docx instead of `.txt` fixtures (see the module docs).
fn build_docx(n: usize) -> Vec<u8> {
    let paragraph = format!(
        "Quarterly budget note number {n}. This paragraph exists only to clear the \
         minimum-length floor extraction requires before a file is queued for \
         enrichment and embedding, repeated a few times so the count is comfortably \
         past it regardless of exactly how whitespace gets collapsed. Quarterly \
         budget note number {n}, repeated once more for good measure and margin."
    );
    let document_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body><w:p><w:r><w:t>{paragraph}</w:t></w:r></w:p></w:body>
</w:document>"#
    );
    build_zip(&[("word/document.xml", &document_xml)])
}

fn build_corpus(dir: &std::path::Path) {
    for n in 0..FILE_COUNT {
        std::fs::write(dir.join(format!("note-{n}.docx")), build_docx(n)).expect("write fixture");
    }
}

/// A persistent mock OpenAI-compatible endpoint answering both
/// `/chat/completions` and `/embeddings` for as many requests as the corpus
/// needs — unlike `enrich_worker`'s/`embed_worker`'s own mock servers, which
/// answer exactly one request each, this test drives `FILE_COUNT` files
/// through *two* LLM calls apiece (enrichment and embedding both run,
/// because both are configured), so the server has to keep accepting
/// connections for the test's duration rather than exit after the first.
///
/// Every embedding answers with the identical fixed vector: recall isn't
/// what this test measures (see the module docs), only that the pipeline —
/// worker threads, `file_embedding`/`file_embedding_bin`, `/search` — runs
/// and settles within budget. `Connection: close` on every response is what
/// lets a plain accept-loop serve many sequential requests without
/// implementing HTTP/1.1 keep-alive.
fn spawn_mock_llm_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 8192];
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let n = stream.read(&mut buf).unwrap_or(0);
            let request_line =
                String::from_utf8_lossy(&buf[..n]).lines().next().unwrap_or("").to_string();

            let body = if request_line.contains("/embeddings") {
                let vector = vec![0.1_f32; EMBEDDING_DIMENSIONS];
                format!(r#"{{"data":[{{"embedding":{vector:?}}}]}}"#)
            } else {
                let enrichment = serde_json::json!({
                    "summary": "A budget note.",
                    "keywords": ["budget"],
                    "entities": [],
                    "classification": "note",
                });
                format!(
                    r#"{{"choices":[{{"message":{{"content":{}}}}}]}}"#,
                    serde_json::to_string(&enrichment.to_string())
                        .expect("string serializes infallibly")
                )
            };

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    format!("http://{addr}/v1")
}

/// Poll `/search` until every corpus file has a stored embedding — the
/// closest thing to `extraction_queue_rss.rs`'s `/metrics`-based queue-drain
/// check this crate can do for the embedding queue, which (unlike the
/// extraction queue) has no `/metrics` gauge of its own. Every fixture
/// embeds to the identical mock vector, so a `limit` of `FILE_COUNT` returns
/// exactly `FILE_COUNT` hits once every file has landed, and strictly fewer
/// while some are still queued or being processed.
fn wait_for_embeddings_to_land(daemon: &Daemon, timeout: Duration) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    let mut last = String::from("never queried");
    while std::time::Instant::now() < deadline {
        match daemon.probe(&format!("/search?q=budget&limit={FILE_COUNT}")) {
            Ok(resp) if resp.matches("\"id\"").count() >= FILE_COUNT => return Ok(()),
            Ok(resp) => last = resp.lines().last().unwrap_or("empty").to_string(),
            Err(e) => last = e,
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("embeddings did not finish landing within {timeout:?}; last: {last}"))
}

#[test]
fn steady_state_rss_with_search_configured_is_within_the_m03_budget() {
    if !cfg!(target_os = "linux") {
        skip("RSS measurement is linux-only");
        return;
    }

    let bin = match binary() {
        Ok(b) => b,
        Err(e) => {
            skip(&e);
            return;
        }
    };

    let corpus = tempfile::tempdir().expect("corpus tempdir");
    build_corpus(corpus.path());

    let base_url = spawn_mock_llm_server();
    let dims = EMBEDDING_DIMENSIONS.to_string();
    let extra_args = [
        "--llm-base-url",
        base_url.as_str(),
        "--llm-model",
        "chat-model",
        "--llm-embedding-model",
        "test-embed",
        "--llm-embedding-dimensions",
        dims.as_str(),
    ];

    let daemon = Daemon::spawn_watching_with_args(&bin, corpus.path(), &extra_args)
        .expect("spawning the release daemon");
    daemon.wait_ready(Duration::from_secs(30)).expect("daemon should become ready");

    // Extraction first (embedding can only be enqueued after a successful
    // extraction — see `extract_worker::maybe_enqueue_embedding`).
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let resp = daemon.probe("/metrics").expect("probing /metrics");
        let files_known = metric_value(&resp, "dafs_files_known");
        let queue_depth = metric_value(&resp, "dafs_extraction_queue_depth");
        if files_known == Some(FILE_COUNT as u64) && queue_depth == Some(0) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "extraction queue did not drain within 60s; last: files_known={files_known:?} \
             queue_depth={queue_depth:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    wait_for_embeddings_to_land(&daemon, Duration::from_secs(60))
        .expect("every fixture should end up with a stored embedding");

    std::thread::sleep(SETTLE);

    let rss = daemon.resident_bytes().expect("reading RSS");
    let ceiling = ceilings::DAEMON_WITH_SEARCH;

    eprintln!(
        "M03 steady-state RSS: {:.2} MiB (ceiling {:.2} MiB, {:.0}% used)",
        rss as f64 / 1_048_576.0,
        ceiling as f64 / 1_048_576.0,
        100.0 * rss as f64 / ceiling as f64
    );

    let stderr = daemon.shutdown();
    if !stderr.trim().is_empty() {
        eprintln!("daemon stderr:\n{stderr}");
    }

    assert!(
        rss <= ceiling,
        "M03 steady-state RSS {rss} bytes exceeds the {ceiling}-byte ceiling in \
         docs/memory-budget.md §8.4. This is measured at a {FILE_COUNT}-file corpus, far below \
         the 1M-document scale that ceiling is actually about — a failure here at this size is \
         a real regression in the embedding worker's own footprint, not evidence about \
         quantization at scale. Do not raise the constant to make this pass."
    );
}

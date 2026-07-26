//! The extraction worker: drains `dafs_store::metadata`'s extraction queue,
//! running `dafs-extract` against whatever it finds.
//!
//! A single dedicated thread, not a pool. Extraction here is deterministic and
//! CPU-bound but bounded — nothing yet has measured a queue depth that a lone
//! thread cannot keep up with, and a pool is the kind of complexity that wants
//! a measurement behind it rather than being assumed up front.
//!
//! # A third connection
//!
//! Same reasoning as the observer's own connection (see `main`'s module
//! docs): extraction runs synchronously, sometimes over many seconds for a
//! large document, and putting that behind the scan's or the API's lock would
//! stall an unrelated timeline request or a live watch event for as long as
//! one extraction takes.
//!
//! # Attempt-then-work ordering
//!
//! [`dafs_store::metadata::record_attempt`] runs *before* extraction is even
//! attempted, not after it returns. A crash (or a `kill -9`) during the
//! extraction call itself must still count as a used attempt on restart, or a
//! file that reliably wedges the extractor would be retried forever instead of
//! eventually hitting `MAX_ATTEMPTS`.
//!
//! # The timeout
//!
//! `dafs_extract::extract` already caps the bytes it reads and catches panics,
//! but nothing stops a pathological file from looping inside a parsing
//! library instead of panicking. Each call runs on its own short-lived thread,
//! joined only via an `mpsc` channel with a timeout — if it never answers, the
//! poll moves on and the spawned thread is abandoned rather than joined. That
//! is safe here specifically because the attempt was already recorded: an
//! abandoned extraction is retried like any other failure, up to the cap.
//!
//! # PDF: a child process instead of an in-process call
//!
//! `dafs_extract::extract` deliberately stubs PDF (see that crate's module
//! docs): real PDF text extraction needs `pdfium-render`, a safe wrapper
//! around Pdfium, a C++ library parsing the same untrusted bytes every other
//! extractor here parses — except C++ is not memory-safe, so a malformed PDF
//! can crash the whole process it runs in, not just panic a Rust call frame.
//! `catch_unwind` (which is what protects every other extractor in this
//! worker) cannot catch a segfault.
//!
//! So a queued file that sniffs as [`dafs_extract::DocType::Pdf`] is routed to
//! `crates/dafs-pdf-worker`'s binary instead, spawned as a child process per
//! file (see that crate's module docs for why one request per process,
//! rather than a persistent worker). The request/response is the same
//! length-prefixed JSON framing that binary defines, and the same
//! [`EXTRACT_TIMEOUT`]/abandon-on-timeout contract applies — with one
//! addition the in-process path doesn't need: on timeout the child is still
//! running and holding real OS resources (an open file, allocated memory),
//! so it is killed explicitly (`Child::kill`) rather than merely left to be
//! abandoned like a Rust thread would be.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use dafs_extract::{DocType, ExtractError, Extraction};
use dafs_store::metadata::FileMetadata;
use dafs_store::paths::FileId;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Files pulled from the queue per poll. Small and bounded: this worker is one
/// thread, so a huge batch just means a long stretch between stop-flag checks
/// rather than any real throughput gain.
const BATCH: u32 = 16;

/// How long one file gets before its extraction is abandoned. Generous
/// against anything real — every extractor here is bounded by
/// `dafs_extract::MAX_EXTRACT_BYTES` — and short enough that a genuinely
/// wedged file does not stall the queue for long.
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to sleep when the queue is empty before polling again. Mirrors
/// the observer's 250ms watch poll: short enough that shutdown is responsive,
/// long enough that an idle daemon is not spinning on an empty table.
const IDLE_POLL: Duration = Duration::from_millis(500);

/// A file's body text needs real signal before an LLM summarization prompt
/// is worth a network round trip over it — well above
/// `dafs_extract::office::MIN_CHARS_FOR_LANG_DETECT`'s 40-char floor for a
/// mere language guess, since summarizing takes more than a few words of
/// context. A short file (a stub, a one-line note) is not worth enqueuing.
const MIN_CHARS_FOR_ENRICHMENT: usize = 300;

/// `crates/dafs-pdf-worker`'s binary name, resolved as a sibling of this
/// process's own executable — cargo places every workspace binary in the
/// same `target/<profile>/` directory, and the Nix package (`flake.nix`)
/// installs both into the same `$out/bin`, so "next to me" is the one
/// resolution rule that holds in every build this daemon ships from.
const PDF_WORKER_BIN_NAME: &str = "dafs-pdf-worker";

/// Handle to the extraction worker thread.
pub struct ExtractWorker {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ExtractWorker {
    /// Mirrors `Observer::shutdown` exactly: flip the stop flag, then join —
    /// the worker checks the flag between every file and on every idle-sleep
    /// wakeup, so it exits within roughly one poll interval.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            match handle.join() {
                Ok(()) => tracing::debug!("extraction worker stopped"),
                Err(_) => tracing::warn!("extraction worker thread panicked"),
            }
        }
    }
}

/// Start the extraction worker thread.
///
/// Safe to call with an empty queue — which is the common case at startup
/// before `requeue_stale` or a live watch event has put anything in it: the
/// worker just polls an empty table harmlessly until there is work.
///
/// `enrichment_enabled` is whether the daemon was given an LLM endpoint at
/// all (`main.rs`'s `llm_config.is_some()`) — threaded through as a bare
/// `bool` rather than importing `dafs_enrich::Config` here, since this
/// module otherwise has no reason to know anything about LLM configuration;
/// it only needs to know whether to enqueue, not how enrichment itself works.
pub fn spawn(db_path: &std::path::Path, enrichment_enabled: bool) -> anyhow::Result<ExtractWorker> {
    use anyhow::Context as _;

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let db_path = db_path.to_path_buf();
    let pdf_worker_bin = pdf_worker_path();

    let handle = std::thread::Builder::new()
        .name("dafs-extract".into())
        .spawn(move || run(&db_path, &thread_stop, &pdf_worker_bin, enrichment_enabled))
        .context("spawning the extraction worker thread")?;

    Ok(ExtractWorker { stop, handle: Some(handle) })
}

/// Where this process expects to find `dafs-pdf-worker`, given where it was
/// itself run from. Pure and separate from [`spawn`] for the same reason
/// `main.rs`'s `self_update_script_args` takes `current_exe` explicitly:
/// testable without depending on where the test binary itself happens to
/// live (which, for a `cargo test` binary, is *not* the same directory an
/// installed `dafs` runs from — see the module's tests).
fn sibling_binary_path(current_exe: &Path, name: &str) -> PathBuf {
    match current_exe.parent() {
        Some(dir) => dir.join(name),
        // No parent component at all is not realistic for a real running
        // process, but falling back to a bare name (resolved via `PATH`) is
        // a harmless degrade rather than a reason to fail startup.
        None => PathBuf::from(name),
    }
}

fn pdf_worker_path() -> PathBuf {
    let current_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dafs"));
    sibling_binary_path(&current_exe, PDF_WORKER_BIN_NAME)
}

/// The worker's main loop, run on its own connection and its own thread.
fn run(
    db_path: &std::path::Path,
    stop: &AtomicBool,
    pdf_worker_bin: &Path,
    enrichment_enabled: bool,
) {
    let conn = match dafs_store::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("extraction worker could not open the store: {e}");
            return;
        }
    };

    while !stop.load(Ordering::Acquire) {
        let ids = match dafs_store::metadata::pending(&conn, BATCH) {
            Ok(ids) => ids,
            Err(e) => {
                tracing::error!("could not poll the extraction queue: {e}");
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
            process_one(&conn, file_id, pdf_worker_bin, enrichment_enabled);
        }
    }
}

/// Extract one queued file, recording either its metadata or nothing —
/// extraction failures are never propagated past a log line, since
/// `MAX_ATTEMPTS` is what bounds their cost, not the caller.
fn process_one(
    conn: &Connection,
    file_id: FileId,
    pdf_worker_bin: &Path,
    enrichment_enabled: bool,
) {
    // Recorded before any work happens at all — see the module docs on why
    // this ordering, not "after extraction fails", is what crash-consistency
    // requires here.
    if let Err(e) = dafs_store::metadata::record_attempt(conn, file_id) {
        tracing::warn!(file_id, "could not record an extraction attempt: {e}");
        return;
    }

    let path = match dafs_store::paths::resolve_path(conn, file_id) {
        Ok(p) => PathBuf::from(p),
        Err(e) => {
            tracing::warn!(file_id, "could not resolve a path to extract: {e}");
            return;
        }
    };

    // A sniff failure here (vanished file, permission error) just means the
    // in-process path is taken below and fails there instead, the same way
    // it always has — `dafs_extract::extract` sniffs again internally, so
    // the error a caller sees is identical either way. Only a *successful*
    // sniff that comes back `Pdf` changes anything.
    let is_pdf = dafs_extract::sniff(&path).map(|t| t == DocType::Pdf).unwrap_or(false);

    let result = if is_pdf {
        extract_pdf_with_timeout(pdf_worker_bin, &path)
    } else {
        extract_with_timeout(&path)
    };

    match result {
        Some(Ok(extraction)) => {
            let metadata = to_file_metadata(extraction);
            match dafs_store::metadata::record_extraction(conn, file_id, &metadata) {
                // Also what makes a re-extracted file (modified, or an
                // extractor-version bump) get re-enriched: it comes back
                // through this same success path every time, not just on
                // first extraction.
                Ok(()) => maybe_enqueue_enrichment(conn, file_id, &metadata, enrichment_enabled),
                Err(e) => {
                    tracing::warn!(
                        file_id,
                        path = %path.display(),
                        "could not record extraction result: {e}"
                    );
                }
            }
        }
        Some(Err(e)) => {
            tracing::warn!(file_id, path = %path.display(), "extraction failed: {e}");
        }
        None => {
            tracing::warn!(
                file_id,
                path = %path.display(),
                timeout_secs = EXTRACT_TIMEOUT.as_secs(),
                "extraction timed out; moving on"
            );
        }
    }
}

/// Run `dafs_extract::extract` on a throwaway thread, bounded by
/// [`EXTRACT_TIMEOUT`]. `None` means the timeout elapsed first; the spawned
/// thread is deliberately not joined in that case (see the module docs).
fn extract_with_timeout(path: &Path) -> Option<Result<Extraction, ExtractError>> {
    let (tx, rx) = mpsc::channel();
    let owned = path.to_path_buf();

    std::thread::spawn(move || {
        let result = dafs_extract::extract(&owned);
        // The receiver may already have timed out and moved on; a failed send
        // just means nobody is listening any more, not a problem to report.
        let _ = tx.send(result);
    });

    rx.recv_timeout(EXTRACT_TIMEOUT).ok()
}

/// The `dafs-pdf-worker` wire request, mirroring that binary's own (private)
/// `Request` type — the two are kept as separate definitions on purpose (see
/// this crate's Cargo.toml comment): JSON over a pipe is the actual contract
/// between these two binaries, not a shared Rust type, so there is nothing
/// to keep in sync beyond the field names below matching that binary's.
#[derive(Serialize)]
struct PdfWorkerRequest {
    path: String,
    max_bytes: u64,
}

/// Mirrors `dafs-pdf-worker`'s `Response`, same reasoning as
/// [`PdfWorkerRequest`].
#[derive(Deserialize)]
struct PdfWorkerResponse {
    title: Option<String>,
    author: Option<String>,
    page_count: Option<i64>,
    word_count: Option<i64>,
    language: Option<String>,
    body_text: Option<String>,
    error: Option<String>,
}

/// Same [`EXTRACT_TIMEOUT`]/`Option`-as-timeout-signal contract as
/// [`extract_with_timeout`], for a PDF routed to the pdfium child process
/// instead. Every failure mode short of a genuine timeout — the binary
/// missing, a non-zero exit, an unreadable or malformed response, the
/// worker's own reported extraction error — collapses into
/// `Some(Err(ExtractError::Malformed))`, so the caller in [`process_one`]
/// needs no PDF-specific branch of its own; only a real timeout (`None`)
/// needs to reach in and kill the child, since that is the one case where
/// the child is still alive and holding resources when this function returns.
fn extract_pdf_with_timeout(
    pdf_worker_bin: &Path,
    path: &Path,
) -> Option<Result<Extraction, ExtractError>> {
    extract_pdf_with_deadline(pdf_worker_bin, path, EXTRACT_TIMEOUT)
}

/// [`extract_pdf_with_timeout`]'s actual body, with the timeout taken as a
/// parameter rather than hardcoded to [`EXTRACT_TIMEOUT`] — purely so tests
/// can exercise the never-responds/killed-not-abandoned path in well under a
/// second instead of the real 30.
fn extract_pdf_with_deadline(
    pdf_worker_bin: &Path,
    path: &Path,
    deadline: Duration,
) -> Option<Result<Extraction, ExtractError>> {
    let path_owned = path.to_path_buf();

    let mut child = match Command::new(pdf_worker_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return Some(Err(malformed(&path_owned, format!("spawning pdf worker: {e}"))));
        }
    };

    let mut stdin = child.stdin.take().expect("stdin was piped since it was just set above");
    let mut stdout = child.stdout.take().expect("stdout was piped since it was just set above");

    let request = PdfWorkerRequest {
        path: path_owned.display().to_string(),
        max_bytes: dafs_extract::MAX_EXTRACT_BYTES,
    };
    let request_bytes =
        serde_json::to_vec(&request).expect("PdfWorkerRequest serializes infallibly");

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = write_frame(&mut stdin, &request_bytes).and_then(|()| read_frame(&mut stdout));
        // Same non-listener reasoning as `extract_with_timeout`: if the
        // parent already timed out, nobody reads this and that's fine.
        let _ = tx.send(result);
    });

    let response_bytes = match rx.recv_timeout(deadline) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => {
            let _ = child.kill();
            let _ = child.wait();
            return Some(Err(malformed(&path_owned, format!("pdf worker I/O: {e}"))));
        }
        Err(_timeout_or_disconnected) => {
            // The child is still running (or the pipe is still open) —
            // explicitly killed rather than abandoned, unlike the in-process
            // path's spare thread, because this holds real OS resources.
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };

    match child.wait() {
        Ok(status) if !status.success() => {
            return Some(Err(malformed(&path_owned, format!("pdf worker exited with {status}"))));
        }
        Err(e) => {
            return Some(Err(malformed(&path_owned, format!("waiting for pdf worker: {e}"))));
        }
        Ok(_) => {}
    }

    let response: PdfWorkerResponse = match serde_json::from_slice(&response_bytes) {
        Ok(r) => r,
        Err(e) => {
            return Some(Err(malformed(&path_owned, format!("parsing pdf worker response: {e}"))));
        }
    };

    if let Some(reason) = response.error {
        return Some(Err(malformed(&path_owned, reason)));
    }

    let mut extraction = Extraction {
        doc_type: DocType::Pdf,
        title: response.title,
        author: response.author,
        language: response.language,
        page_count: response.page_count,
        word_count: response.word_count,
        body_text: response.body_text,
        ..Default::default()
    };
    // The daemon bypasses `dafs_extract::extract` entirely for PDFs, so this
    // is the one place that must call the same git-facts merge every other
    // document type gets through that function — see `merge_git_facts`'s own
    // docs for why it is a public, shared function rather than being
    // duplicated here.
    dafs_extract::merge_git_facts(&mut extraction, &path_owned);

    Some(Ok(extraction))
}

fn malformed(path: &Path, reason: String) -> ExtractError {
    ExtractError::Malformed { doc_type: DocType::Pdf, path: path.display().to_string(), reason }
}

/// Read one big-endian-`u32`-length-prefixed frame, matching
/// `dafs-pdf-worker`'s own framing exactly (see that binary's module docs).
fn read_frame(r: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)?;
    let len = u32::from_be_bytes(len_bytes) as usize;

    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Write one big-endian-`u32`-length-prefixed frame, matching [`read_frame`].
fn write_frame(w: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
    w.write_all(&(bytes.len() as u32).to_be_bytes())?;
    w.write_all(bytes)?;
    w.flush()
}

/// Whether a just-extracted file's body text clears
/// [`MIN_CHARS_FOR_ENRICHMENT`] — `None` (images, git-only facts) is not
/// enough, the same as any count below the floor.
fn has_enough_text_to_enrich(metadata: &FileMetadata) -> bool {
    metadata.body_text.as_deref().map(|t| t.chars().count()).unwrap_or(0)
        >= MIN_CHARS_FOR_ENRICHMENT
}

/// After a successful extraction, enqueue the file for LLM enrichment if
/// enrichment is configured at all and there is enough text to be worth a
/// network round trip. Split out from [`process_one`]'s match arm purely so
/// this decision is unit-testable without needing a real document type
/// (docx/xlsx/pptx, or a PDF via the pdfium worker) to run through
/// `dafs_extract::extract` — plain text files, this module's simplest test
/// fixture, never populate `body_text` at all (see `dafs_extract::extract`'s
/// own doc-type match), so exercising this decision through a real
/// extraction would need fixtures this module has no other reason to build.
fn maybe_enqueue_enrichment(
    conn: &Connection,
    file_id: FileId,
    metadata: &FileMetadata,
    enrichment_enabled: bool,
) {
    if !enrichment_enabled || !has_enough_text_to_enrich(metadata) {
        return;
    }
    if let Err(e) = dafs_store::enrichment::enqueue(conn, file_id, crate::now_unix()) {
        tracing::warn!(file_id, "could not enqueue a file for enrichment: {e}");
    }
}

/// Field-for-field except for `doc_type` (needs its stored string form) and
/// the two bookkeeping fields the store tracks that extraction itself has no
/// opinion on.
fn to_file_metadata(e: dafs_extract::Extraction) -> FileMetadata {
    FileMetadata {
        doc_type: Some(e.doc_type.as_str().to_string()),
        title: e.title,
        author: e.author,
        language: e.language,
        page_count: e.page_count,
        word_count: e.word_count,
        image_taken_at_unix: e.image_taken_at_unix,
        image_camera_model: e.image_camera_model,
        git_branch: e.git_branch,
        git_head_commit: e.git_head_commit,
        git_head_author: e.git_head_author,
        git_head_at_unix: e.git_head_at_unix,
        body_text: e.body_text,
        extracted_at_unix: crate::now_unix(),
        extractor_version: dafs_extract::EXTRACTOR_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use dafs_store::paths::{Interner, ensure_dir_chain};

    use super::*;

    /// The core loop logic, driven directly against a real temp sqlite file
    /// and real files — no process spawn, so this is the fast, iterate-on-it
    /// counterpart to `tests/extraction_crash_consistency.rs`'s full `kill -9`.
    #[test]
    fn one_pass_of_the_loop_extracts_every_pending_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("meta.sqlite");

        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "hello").expect("write a");
        std::fs::write(&b, "world").expect("write b");

        let (file_a, file_b) = {
            let conn = dafs_store::open(&db_path).expect("open");
            let mut i = Interner::new();
            let file_a = ensure_dir_chain(&conn, &mut i, &a).expect("intern a");
            let file_b = ensure_dir_chain(&conn, &mut i, &b).expect("intern b");
            dafs_store::metadata::enqueue(&conn, file_a, 100).expect("enqueue a");
            dafs_store::metadata::enqueue(&conn, file_b, 200).expect("enqueue b");
            (file_a, file_b)
        };

        // A fresh connection, as the real worker uses — proving this does not
        // depend on any in-process state left over from interning above.
        let conn = dafs_store::open(&db_path).expect("reopen");
        // Never dispatched: both files are plain text, so `is_pdf` is always
        // false and this path is never spawned.
        let no_pdf_worker = Path::new("/nonexistent/dafs-pdf-worker");
        for file_id in dafs_store::metadata::pending(&conn, BATCH).expect("pending") {
            process_one(&conn, file_id, no_pdf_worker, false);
        }

        assert!(dafs_store::metadata::pending(&conn, BATCH).expect("pending").is_empty());

        let meta_a = dafs_store::metadata::get(&conn, file_a).expect("get a").expect("extracted");
        assert_eq!(meta_a.doc_type.as_deref(), Some("text"));
        assert_eq!(meta_a.extractor_version, dafs_extract::EXTRACTOR_VERSION);

        let meta_b = dafs_store::metadata::get(&conn, file_b).expect("get b").expect("extracted");
        assert_eq!(meta_b.doc_type.as_deref(), Some("text"));
    }

    /// A file that vanishes between being queued and being processed must not
    /// wedge the worker — it is logged and left for `MAX_ATTEMPTS` to bound.
    #[test]
    fn a_file_that_no_longer_resolves_is_skipped_without_panicking() {
        let conn = dafs_store::open_in_memory().expect("open");
        let mut i = Interner::new();
        // A directory row with no children: resolve_path succeeds (it always
        // does, truncating rather than failing), and extract() itself will
        // fail to open a directory as a file — the ordinary error path.
        let file_id =
            ensure_dir_chain(&conn, &mut i, Path::new("/nonexistent/deeply/nested")).expect("id");
        dafs_store::metadata::enqueue(&conn, file_id, 1).expect("enqueue");

        // Must not panic, and must not leave the queue entry silently gone —
        // the attempt is recorded but the row stays for the next retry.
        process_one(&conn, file_id, Path::new("/nonexistent/dafs-pdf-worker"), false);

        assert!(
            dafs_store::metadata::get(&conn, file_id).expect("get").is_none(),
            "a failed extraction must not fabricate a metadata row"
        );
        let attempts: i64 = conn
            .query_row(
                "SELECT attempt_count FROM extraction_queue WHERE file_id = ?1",
                [file_id],
                |r| r.get(0),
            )
            .expect("attempt_count");
        assert_eq!(attempts, 1, "the attempt was not recorded");
    }

    #[test]
    fn to_file_metadata_carries_doc_type_as_its_stored_string() {
        let extraction = dafs_extract::Extraction {
            doc_type: dafs_extract::DocType::Docx,
            title: Some("Report".into()),
            ..Default::default()
        };
        let metadata = to_file_metadata(extraction);
        assert_eq!(metadata.doc_type.as_deref(), Some("docx"));
        assert_eq!(metadata.title.as_deref(), Some("Report"));
        assert_eq!(metadata.extractor_version, dafs_extract::EXTRACTOR_VERSION);
    }

    #[test]
    fn has_enough_text_to_enrich_respects_the_floor() {
        let none = FileMetadata { body_text: None, ..Default::default() };
        assert!(!has_enough_text_to_enrich(&none), "no body text is never enough");

        let short = FileMetadata { body_text: Some("short".into()), ..Default::default() };
        assert!(!has_enough_text_to_enrich(&short), "below the floor must not pass");

        let long = FileMetadata {
            body_text: Some("x".repeat(MIN_CHARS_FOR_ENRICHMENT)),
            ..Default::default()
        };
        assert!(has_enough_text_to_enrich(&long), "at the floor must pass");
    }

    #[test]
    fn maybe_enqueue_enrichment_only_enqueues_when_enabled_and_long_enough() {
        let conn = dafs_store::open_in_memory().expect("open");
        let mut i = Interner::new();
        let file_id = ensure_dir_chain(&conn, &mut i, Path::new("/a/report.docx")).expect("id");
        let long = FileMetadata { body_text: Some("x".repeat(500)), ..Default::default() };
        let short = FileMetadata { body_text: Some("short".into()), ..Default::default() };

        maybe_enqueue_enrichment(&conn, file_id, &short, true);
        assert!(
            dafs_store::enrichment::pending(&conn, 10).expect("pending").is_empty(),
            "short text must not be enqueued even when enrichment is enabled"
        );

        maybe_enqueue_enrichment(&conn, file_id, &long, false);
        assert!(
            dafs_store::enrichment::pending(&conn, 10).expect("pending").is_empty(),
            "long text must not be enqueued when enrichment is disabled"
        );

        maybe_enqueue_enrichment(&conn, file_id, &long, true);
        assert_eq!(
            dafs_store::enrichment::pending(&conn, 10).expect("pending"),
            vec![file_id],
            "long text with enrichment enabled must be enqueued"
        );
    }

    #[test]
    fn sibling_binary_path_joins_the_name_next_to_the_given_exe() {
        let path = sibling_binary_path(Path::new("/opt/dafs/bin/dafs"), "dafs-pdf-worker");
        assert_eq!(path, Path::new("/opt/dafs/bin/dafs-pdf-worker"));
    }

    #[test]
    fn sibling_binary_path_falls_back_to_a_bare_name_with_no_parent() {
        // `Path::new("dafs").parent()` is `Some("")`, not `None` — a relative
        // single-component path still has an (empty) parent, so this exercises
        // the fallback with a path that genuinely has none: the root itself.
        let path = sibling_binary_path(Path::new("/"), "dafs-pdf-worker");
        assert_eq!(path, Path::new("dafs-pdf-worker"));
    }

    /// A PDF whose worker process never writes a response must be killed —
    /// not merely abandoned like the in-process timeout path's spare thread
    /// — because it is a real OS process holding real resources. Uses a
    /// short-lived shell script standing in for a wedged `dafs-pdf-worker`,
    /// with its own PID written to a file the test can poll, since
    /// `extract_pdf_with_deadline` does not (and should not) leak the
    /// `Child` handle to its caller.
    #[test]
    fn a_pdf_worker_that_never_responds_is_killed_not_abandoned() {
        if !cfg!(unix) {
            eprintln!("SKIP: this test shells out to sh and kill -0, unix-only");
            return;
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join("pid");
        let script = dir.path().join("hang.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho $$ > {}\nexec sleep 100\n", pidfile.display()),
        )
        .expect("write script");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod +x");
        }

        let pdf_path = dir.path().join("whatever.pdf");
        std::fs::write(&pdf_path, b"irrelevant, the hung script never reads it").expect("write");

        let result = extract_pdf_with_deadline(&script, &pdf_path, Duration::from_millis(300));
        assert!(result.is_none(), "a never-responding worker must read as a timeout");

        let pid = std::fs::read_to_string(&pidfile)
            .expect("the script must have started and recorded its pid")
            .trim()
            .to_string();

        // The kill is asynchronous from this test's point of view; give it a
        // short, bounded grace period rather than asserting instantaneously.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !alive {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "pid {pid} was still alive {deadline:?} after the timeout — \
                 the child was abandoned, not killed"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A worker that exits immediately without writing a valid frame is a
    /// clean, retryable failure — not a hang and not a panic.
    #[test]
    fn a_worker_that_exits_without_a_response_is_a_clean_error() {
        if !cfg!(unix) {
            eprintln!("SKIP: this test relies on /bin/true, unix-only");
            return;
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let pdf_path = dir.path().join("whatever.pdf");
        std::fs::write(&pdf_path, b"irrelevant").expect("write");

        let result = extract_pdf_with_deadline(Path::new("/bin/true"), &pdf_path, EXTRACT_TIMEOUT);
        assert!(matches!(result, Some(Err(ExtractError::Malformed { .. }))));
    }

    /// A missing `dafs-pdf-worker` binary (a broken install, a stripped
    /// deployment) must fail the one file it was asked to extract, not wedge
    /// the whole worker thread.
    #[test]
    fn a_missing_pdf_worker_binary_is_a_clean_error_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf_path = dir.path().join("whatever.pdf");
        std::fs::write(&pdf_path, b"irrelevant").expect("write");

        let result =
            extract_pdf_with_timeout(Path::new("/no/such/binary/dafs-pdf-worker"), &pdf_path);
        assert!(matches!(result, Some(Err(ExtractError::Malformed { .. }))));
    }
}

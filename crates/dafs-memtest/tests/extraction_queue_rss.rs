//! Idle RSS after the extraction queue (M02a) has fully drained.
//!
//! `idle_rss_is_within_budget` (in `rss_ceiling.rs`) measures a daemon with
//! nothing to do at all. This is the scenario the memory budget actually
//! needs proven for M02a: extraction runs, its worker thread and connection
//! do real work, and the question is whether RSS comes back down to the same
//! 32 MiB ceiling once that work is done — not whether a brand-new ceiling is
//! needed for it. Deterministic extraction (no LLM) has no reason to cost
//! more resident memory than the observer already does; if a measurement
//! here disagreed, the fix would be in the extraction path, not in this
//! constant (see `docs/memory-budget.md`'s "do not raise the constant"
//! rule).
//!
//! Plain `.txt` files are enough. `dafs-extract`'s own tests already cover
//! per-format parsing correctness (docx/xlsx/pptx/EXIF/PDF); this test is
//! about the queue-then-settle memory story, which is identical regardless
//! of which extractor ran.

use std::time::Duration;

use dafs_memtest::{Daemon, binary, ceilings, metric_value};

/// Same reasoning as `rss_ceiling.rs`'s `SETTLE`: `dirty_decay_ms` is 1000,
/// so wait past that before reading RSS or the measurement is pre-purge.
const SETTLE: Duration = Duration::from_millis(2500);

/// Small and fast: this test is about the queue draining and RSS settling
/// afterwards, not about extraction throughput at scale — that is
/// `extraction_crash_consistency.rs`'s much larger corpus.
const FILE_COUNT: usize = 20;

fn skip(reason: &str) {
    eprintln!("SKIP: {reason}");
}

fn build_corpus(dir: &std::path::Path) {
    for n in 0..FILE_COUNT {
        std::fs::write(dir.join(format!("note-{n}.txt")), format!("note number {n}\n"))
            .expect("write corpus file");
    }
}

/// Poll `/metrics` until the scan has found every file *and* the extraction
/// queue is back to zero, or time out — the polling-with-timeout idiom this
/// project already uses for crash-consistency convergence, in place of a
/// fixed sleep that would be either flaky (too short) or slow on every run
/// (too long).
///
/// Both conditions come from the **same** `/metrics` response and both are
/// required, not `dafs_extraction_queue_depth == 0` alone: readiness does not
/// wait for the initial scan (M01), so immediately after `wait_ready` the
/// queue reads zero simply because nothing has been enqueued yet — a false
/// "drained" reading, not a true one. Requiring `dafs_files_known` to have
/// caught up first rules that window out.
fn wait_for_queue_to_drain(
    daemon: &Daemon,
    expected_files: u64,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    let mut last = String::from("never queried");
    while std::time::Instant::now() < deadline {
        match daemon.probe("/metrics") {
            Ok(resp) => {
                let files_known = metric_value(&resp, "dafs_files_known");
                let queue_depth = metric_value(&resp, "dafs_extraction_queue_depth");
                if files_known == Some(expected_files) && queue_depth == Some(0) {
                    return Ok(());
                }
                last = format!("files_known={files_known:?} queue_depth={queue_depth:?}");
            }
            Err(e) => last = e,
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!("extraction queue did not drain within {timeout:?}; last: {last}"))
}

#[test]
fn idle_rss_after_the_extraction_queue_drains_is_within_the_same_budget() {
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

    let daemon = Daemon::spawn_watching(&bin, corpus.path()).expect("spawning the release daemon");
    daemon.wait_ready(Duration::from_secs(30)).expect("daemon should become ready");

    wait_for_queue_to_drain(&daemon, FILE_COUNT as u64, Duration::from_secs(60))
        .expect("the extraction queue should fully drain on a 20-file corpus");

    std::thread::sleep(SETTLE);

    // Sanity check that extraction actually ran, rather than the gauges
    // reading their drained state because nothing was ever enqueued (a
    // wired-wrong `--watch`, say) — every file here sniffs as plain text.
    // Checked here, after `SETTLE`, not right off the back of the poll loop
    // above: `dafs_files_known` catching up to 20 and `requeue_stale`
    // actually enqueueing those 20 files are two sequential steps on the
    // observer thread, so a queue-depth-of-0 reading can (rarely) land in
    // the sliver between them — no files enqueued *yet*, not none left. Two
    // and a half more seconds is orders of magnitude more than that gap
    // needs to close, so by the time this reads, "0" and "done" are the same
    // thing.
    let facets = daemon.probe("/facets?field=doc_type").expect("probing /facets");
    assert!(
        facets.contains("\"text\""),
        "expected the corpus to be extracted as doc_type=text, got: {facets}"
    );

    let rss = daemon.resident_bytes().expect("reading RSS");
    let ceiling = ceilings::DAEMON_IDLE;

    eprintln!(
        "post-extraction idle RSS: {:.2} MiB (ceiling {:.2} MiB, {:.0}% used)",
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
        "post-extraction idle RSS {rss} bytes exceeds the {ceiling}-byte ceiling in \
         docs/memory-budget.md. Deterministic extraction is not expected to need a higher \
         ceiling than the plain observer — either the regression is real, or the budget needs \
         an explicit, documented revision. Do not raise the constant to make this pass."
    );
}

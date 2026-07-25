//! Measures one scan's memory cost, for one corpus size, in a clean process.
//!
//! Exists because two scans in a single process cannot be compared: the second
//! reads the first's allocator high-water mark rather than its own cost.
//! `tests/scan_memory.rs` spawns this once per corpus size and compares the
//! reported numbers.
//!
//! Usage: `scan_probe <file-count>`. Prints `anon=<bytes> total=<bytes> ...` to
//! stdout.
//!
//! Reported numbers:
//!
//! - `anon` — anonymous RSS attributable to the scan. The number that decides
//!   whether the process survives memory pressure, and the one the growth
//!   assertion uses.
//! - `total` — full RSS including mapped database pages. Grows with the database
//!   by design (`mmap_size`, see `docs/memory-budget.md` §8.3); those pages are
//!   file-backed and evictable.
//! - `settled` — anonymous RSS after closing the store and waiting out
//!   allocator decay. Distinguishes "still live" from "not yet purged".
//! - `cached` — interner cache entries. Must not scale with corpus size; see
//!   `dafs_store::paths`.

use std::path::Path;

use dafs_scan::{ScanOptions, scan};
use dafs_store::paths::Interner;

/// The same allocator and decay tuning the daemon uses. Without this the probe
/// measures glibc's arena behaviour, which is precisely what
/// `docs/memory-budget.md` rejects — and the numbers would not describe the
/// shipped binary at all.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: dafs_alloc::Allocator = dafs_alloc::ALLOCATOR;

fn main() {
    let count: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .expect("usage: scan_probe <file-count>");

    // Scan an existing tree when asked to, instead of building a fresh one.
    // The crash-consistency test uses this: it needs a scan it can SIGKILL
    // partway through, against a corpus and database that outlive the process.
    let existing = std::env::var_os("DAFS_PROBE_SCAN_DIR").map(std::path::PathBuf::from);

    // Held so the temporary directory outlives the scan; unused when scanning
    // an existing tree.
    let owned_dir = if existing.is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        build_corpus(dir.path(), count);
        Some(dir)
    } else {
        None
    };

    let root = existing
        .clone()
        .unwrap_or_else(|| owned_dir.as_ref().expect("owned dir").path().to_path_buf());

    // The store lives under `.dafs`, which the default skip list excludes, so
    // the scanner does not observe its own writes.
    let db_path = match std::env::var_os("DAFS_PROBE_DB") {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let db_dir = root.join(".dafs");
            std::fs::create_dir_all(&db_dir).expect("create db dir");
            db_dir.join("meta.sqlite")
        }
    };

    let conn = dafs_store::open(&db_path).expect("open store");
    let mut interner = Interner::new();

    let baseline = anonymous_rss().expect("anonymous rss baseline");
    let baseline_total = total_rss().expect("total rss baseline");

    scan(&conn, &mut interner, &root, &ScanOptions::default()).expect("scan");

    // Sampled at the end of the scan rather than continuously: the scan holds
    // no transient allocation larger than one batch, so this is the peak. A
    // sampling thread would report the same number with more machinery.
    let anon = anonymous_rss().expect("anonymous rss").saturating_sub(baseline);
    let total = total_rss().expect("total rss");
    let cached = interner.cached();

    // Attribution: how much of `anon` is SQLite's own arena versus the Rust
    // heap. Without this a growth failure sends the reader guessing, which is
    // how the WAL was blamed for growth it was not causing.
    let sqlite_used =
        conn.query_row("SELECT sqlite_memory_used()", [], |r| r.get::<_, i64>(0)).unwrap_or(-1);
    let sqlite_high = conn
        .query_row("SELECT sqlite_memory_highwater(0)", [], |r| r.get::<_, i64>(0))
        .unwrap_or(-1);
    let cache_pages = conn.query_row("PRAGMA cache_size", [], |r| r.get::<_, i64>(0)).unwrap_or(-1);
    let wal_pages =
        conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| r.get::<_, i64>(1)).unwrap_or(-1);

    // Closing releases SQLite's page and statement caches; the sleep waits out
    // jemalloc's decay (dirty_decay_ms is 1000). Whatever remains after both is
    // genuinely retained rather than merely unpurged.
    drop(conn);
    std::thread::sleep(std::time::Duration::from_millis(2500));
    let settled = anonymous_rss().expect("settled rss").saturating_sub(baseline);

    println!(
        "anon={anon} total={total} settled={settled} cached={cached} \
         sqlite_used={sqlite_used} sqlite_high={sqlite_high} cache_pages={cache_pages} \
         wal_pages={wal_pages} baseline_total={baseline_total}"
    );
}

fn build_corpus(root: &Path, count: usize) {
    // Spread across subdirectories rather than one flat directory: a realistic
    // tree shape, and it exercises the directory-component interning the whole
    // memory argument rests on.
    for n in 0..count {
        let dir = root.join(format!("dir-{:03}", n % 100));
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(dir.join(format!("file-{n}.txt")), b"some modest contents\n")
            .expect("write");
    }
}

/// Anonymous resident memory — private, non-file-backed pages.
///
/// `RssAnon` from `/proc/self/status` rather than a field of `statm`: statm's
/// resident count includes file-backed and shared pages, which for this process
/// means the mapped database. Separating them is the entire point here.
fn anonymous_rss() -> Option<u64> {
    read_status_kb("RssAnon:")
}

fn total_rss() -> Option<u64> {
    read_status_kb("VmRSS:")
}

fn read_status_kb(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix(field))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kb * 1024)
}

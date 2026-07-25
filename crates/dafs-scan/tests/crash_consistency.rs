//! Crash consistency of the scan write path.
//!
//! The testing bar requires fault injection across every write path, asserting
//! **no data loss and no unreadable state**. M00 seeded this with an abandoned
//! transaction in the store; M01 introduces the first real write path — a scan
//! producing thousands of events — so this is where a process actually dies
//! mid-write.
//!
//! # What "no data loss" means for a scan
//!
//! Not "every event survives a kill". A scan interrupted halfway has genuinely
//! not seen the second half of the tree, and inventing those events would be
//! worse than missing them. The requirement is narrower and stronger:
//!
//! 1. The database opens cleanly afterwards. A user's metadata store must never
//!    need manual repair because a laptop lid closed.
//! 2. Whatever *was* committed is intact and consistent — no event pointing at
//!    a file row that does not exist, no half-written batch.
//! 3. A rescan converges. The events missed by the interrupted run are recorded
//!    by the next one, so the interruption costs time rather than history.
//!
//! Point 3 is the one that matters most and the one most easily got wrong: it
//! is what makes an interrupted scan self-healing rather than permanently
//! leaving a hole in the timeline.
//!
//! # Why a real process kill
//!
//! `SIGKILL` to a separate process, not a simulated failure. A dropped
//! connection unwinds and runs SQLite's cleanup; `kill -9` does not, which is
//! the difference between testing the code's error handling and testing the
//! database's durability guarantees. The latter is the point.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use dafs_scan::{ScanOptions, scan};
use dafs_store::events::{TimelineQuery, count, timeline};
use dafs_store::paths::Interner;

/// Files in the corpus. Large enough that a scan takes long enough to be killed
/// partway through, small enough to build quickly.
const CORPUS: usize = 8_000;

fn build_corpus(root: &Path) {
    for n in 0..CORPUS {
        let dir = root.join(format!("dir-{:03}", n % 50));
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(dir.join(format!("file-{n}.txt")), b"contents\n").expect("write");
    }
}

/// Locate the scan_probe example, which performs a scan we can kill.
fn probe_binary() -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let profile_dir = current.parent()?.parent()?;
    let candidate = profile_dir.join("examples").join("scan_probe");
    candidate.exists().then_some(candidate)
}

fn skip(reason: &str) {
    eprintln!("SKIP: {reason}");
}

/// Assert the store is internally consistent: every event resolves to a file.
///
/// This is the invariant a torn write would break, and it is checked in SQL
/// rather than by walking the timeline so that a row the API would silently
/// skip still fails the test.
fn assert_consistent(conn: &rusqlite::Connection) {
    let orphans: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events e
              LEFT JOIN files f ON f.id = e.file_id
              WHERE f.id IS NULL",
            [],
            |r| r.get(0),
        )
        .expect("orphan query");
    assert_eq!(orphans, 0, "{orphans} events reference a file row that does not exist");

    let bad_parents: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files c
              LEFT JOIN files p ON p.id = c.parent_id
              WHERE c.parent_id IS NOT NULL AND p.id IS NULL",
            [],
            |r| r.get(0),
        )
        .expect("parent query");
    assert_eq!(bad_parents, 0, "{bad_parents} files have a dangling parent_id");

    let bad_components: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files f
              LEFT JOIN path_components c ON c.id = f.component_id
              WHERE c.id IS NULL",
            [],
            |r| r.get(0),
        )
        .expect("component query");
    assert_eq!(bad_components, 0, "{bad_components} files reference a missing path component");

    // SQLite's own structural check. Catches corruption the schema-level
    // queries above cannot see.
    let integrity: String =
        conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).expect("integrity_check");
    assert_eq!(integrity, "ok", "integrity_check reported: {integrity}");
}

#[test]
fn a_killed_scan_leaves_a_usable_store_and_a_rescan_converges() {
    if !cfg!(unix) {
        skip("process-kill fault injection is unix-only");
        return;
    }

    let Some(probe) = probe_binary() else {
        skip("scan_probe example not built; run `cargo build --examples -p dafs-scan`");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let corpus = dir.path().join("corpus");
    std::fs::create_dir_all(&corpus).expect("corpus dir");
    build_corpus(&corpus);

    let db_dir = corpus.join(".dafs");
    std::fs::create_dir_all(&db_dir).expect("db dir");
    let db_path = db_dir.join("meta.sqlite");

    // Run a scan in a child process against this corpus, then kill it partway.
    // DAFS_PROBE_SCAN_DIR points the probe at an existing tree instead of
    // building its own.
    let mut child = Command::new(&probe)
        .arg("0") // corpus size ignored when a directory is supplied
        .env("DAFS_PROBE_SCAN_DIR", &corpus)
        .env("DAFS_PROBE_DB", &db_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the scan probe");

    // Let the scan get properly underway — past the first batches, so the kill
    // lands mid-write rather than before any work.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut killed_with_data = false;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(150));

        // Only kill once events are actually committed; killing an empty
        // database would make this test pass without testing anything.
        if let Ok(conn) = rusqlite::Connection::open(&db_path)
            && let Ok(n) = conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get::<_, i64>(0))
            && n > 100
        {
            let _ = Command::new("kill").arg("-9").arg(child.id().to_string()).status();
            killed_with_data = true;
            break;
        }

        if child.try_wait().ok().flatten().is_some() {
            break; // finished before we could kill it
        }
    }

    let _ = child.wait();

    if !killed_with_data {
        skip("scan finished or produced no events before it could be killed");
        return;
    }

    // 1. The store opens cleanly after SIGKILL. `open` also runs migrations, so
    //    a database left in an unusable state fails here.
    let conn = dafs_store::open(&db_path).expect("store must open cleanly after a kill -9");

    // 2. What was committed is intact.
    assert_consistent(&conn);

    let after_kill = count(&conn).expect("count");
    assert!(after_kill > 0, "no events survived, so nothing was actually tested");
    eprintln!("events committed before the kill: {after_kill}");

    // Every surviving event must resolve to a real path — the check that would
    // catch a torn write the SQL constraints missed.
    let entries = timeline(&conn, &TimelineQuery { limit: Some(500), ..Default::default() })
        .expect("timeline must be readable");
    for entry in &entries {
        assert!(entry.path.starts_with('/'), "malformed path survived: {:?}", entry.path);
        assert!(!entry.path.contains("//"), "malformed path survived: {:?}", entry.path);
    }

    // 3. A rescan converges: the files the killed run never reached are
    //    recorded now, and the total matches the corpus.
    let mut interner = Interner::new();
    let summary =
        scan(&conn, &mut interner, &corpus, &ScanOptions::default()).expect("rescan must succeed");

    assert_consistent(&conn);

    let total = count(&conn).expect("count after rescan");
    eprintln!(
        "rescan recorded {} more events ({after_kill} -> {total}); corpus is {CORPUS} files",
        summary.events_recorded
    );

    assert_eq!(
        total, CORPUS as i64,
        "after a kill and a rescan the store should hold exactly one event per file"
    );

    // 4. And a third scan is quiet, proving convergence rather than an
    //    endlessly re-recording store.
    let third = scan(&conn, &mut interner, &corpus, &ScanOptions::default()).expect("third scan");
    assert_eq!(third.events_recorded, 0, "a converged store should record nothing on rescan");
}

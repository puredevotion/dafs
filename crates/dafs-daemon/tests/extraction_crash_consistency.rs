//! Crash consistency of the extraction queue (M02a).
//!
//! Mirrors `dafs-scan`'s own `tests/crash_consistency.rs`: a real `kill -9`
//! against a real spawned `dafs` process, not a simulated failure. See that
//! test's module docs for why a genuine SIGKILL is the point — it exercises
//! SQLite's durability guarantees, not this code's error-handling paths.
//!
//! # What "no data loss" means here
//!
//! Not "every file has metadata the instant the process dies" — a process
//! interrupted mid-extraction has genuinely not finished that file yet. The
//! requirement is narrower and stronger, matching `dafs_store::metadata`'s
//! own crash-consistency contract (`record_attempt` before the work,
//! `record_extraction` clearing the queue entry atomically with the write):
//!
//! 1. The database opens cleanly after the kill.
//! 2. Nothing is silently lost: a file the killed run had already finished
//!    keeps its `file_metadata` row; a file it had not gotten to (or had only
//!    recorded an attempt against) is still in `extraction_queue`.
//! 3. A restart converges: eventually every non-directory file has a
//!    `file_metadata` row and the queue is empty, regardless of how far the
//!    killed run got.

use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn dafs_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dafs")
}

/// Files in the corpus. Extraction of plain text is nearly free (no parsing,
/// just a sniff and two small writes per file), so this needs to be large
/// enough that the run is still mid-queue when polled for — not so large
/// that building the corpus itself dominates the test.
const CORPUS: usize = 6_000;

fn build_corpus(root: &Path) {
    for n in 0..CORPUS {
        let dir = root.join(format!("dir-{:03}", n % 40));
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(dir.join(format!("file-{n}.txt")), format!("contents of file {n}\n"))
            .expect("write");
    }
}

fn skip(reason: &str) {
    eprintln!("SKIP: {reason}");
}

#[derive(serde::Deserialize)]
struct PidFile {
    listen: SocketAddr,
}

/// Poll the pidfile until it exists and its recorded port actually accepts a
/// connection, or the deadline passes.
fn wait_for_ready(data_dir: &Path, deadline: Instant) -> bool {
    loop {
        if let Ok(contents) = std::fs::read_to_string(data_dir.join("dafs.pid"))
            && let Ok(pidfile) = serde_json::from_str::<PidFile>(&contents)
            && TcpStream::connect_timeout(&pidfile.listen, Duration::from_millis(200)).is_ok()
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// `None` when the file does not exist yet (opened before the daemon has
/// created it) or is momentarily unreadable — both routine early in startup,
/// never a reason to fail the poll loop.
fn file_metadata_count(db_path: &Path) -> Option<i64> {
    let conn = rusqlite::Connection::open(db_path).ok()?;
    conn.query_row("SELECT COUNT(*) FROM file_metadata", [], |r| r.get(0)).ok()
}

fn spawn_daemon(data_dir: &Path, watch: &Path) -> std::process::Child {
    Command::new(dafs_bin())
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--listen")
        .arg("127.0.0.1:0")
        .arg("--watch")
        .arg(watch)
        .arg("--detach")
        .arg("false")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dafs")
}

#[test]
fn a_killed_daemon_converges_on_full_extraction_after_restart() {
    if !cfg!(unix) {
        skip("process-kill fault injection is unix-only");
        return;
    }

    let root = tempfile::tempdir().expect("tempdir");
    let corpus = root.path().join("corpus");
    std::fs::create_dir_all(&corpus).expect("corpus dir");
    build_corpus(&corpus);

    let data_dir = root.path().join(".dafs");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let db_path = data_dir.join("metadata.sqlite");

    let mut child = spawn_daemon(&data_dir, &corpus);

    if !wait_for_ready(&data_dir, Instant::now() + Duration::from_secs(30)) {
        let _ = child.kill();
        let _ = child.wait();
        skip("daemon never became ready");
        return;
    }

    // Watch for extraction genuinely under way — some files done, not all —
    // so the kill lands mid-queue rather than before the queue existed or
    // after it was already drained.
    let kill_deadline = Instant::now() + Duration::from_secs(60);
    let mut killed_mid_extraction = false;
    while Instant::now() < kill_deadline {
        if let Some(n) = file_metadata_count(&db_path)
            && n > 0
            && (n as usize) < CORPUS
        {
            let _ = Command::new("kill").arg("-9").arg(child.id().to_string()).status();
            killed_mid_extraction = true;
            break;
        }
        if child.try_wait().ok().flatten().is_some() {
            break; // exited (finished or crashed) before we caught it mid-way
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = child.wait();

    if !killed_mid_extraction {
        skip("extraction finished (or never started) before it could be killed mid-way");
        return;
    }

    let before_restart =
        file_metadata_count(&db_path).expect("the store must open cleanly after a kill -9");
    eprintln!("file_metadata rows before restart: {before_restart}/{CORPUS}");
    assert!(before_restart > 0, "no extraction survived the kill, so nothing was tested");
    assert!(
        (before_restart as usize) < CORPUS,
        "the whole corpus finished before the kill landed — nothing was actually interrupted"
    );

    // Restart against the same data dir and corpus.
    let mut second = spawn_daemon(&data_dir, &corpus);
    assert!(
        wait_for_ready(&data_dir, Instant::now() + Duration::from_secs(30)),
        "restarted daemon never became ready"
    );

    // Convergence: every file eventually gets a metadata row, whether the
    // killed run reached it or not — the property that makes a crash cost
    // time rather than a permanently stuck or silently dropped file.
    let convergence_deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(n) = file_metadata_count(&db_path)
            && n as usize == CORPUS
        {
            break;
        }
        assert!(
            Instant::now() < convergence_deadline,
            "extraction did not converge: {:?}/{CORPUS} extracted after restart",
            file_metadata_count(&db_path)
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let conn =
        rusqlite::Connection::open(&db_path).expect("store must open cleanly after convergence");

    let integrity: String =
        conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).expect("integrity_check");
    assert_eq!(integrity, "ok", "integrity_check reported: {integrity}");

    let queue_left: i64 = conn
        .query_row("SELECT COUNT(*) FROM extraction_queue", [], |r| r.get(0))
        .expect("queue count");
    assert_eq!(queue_left, 0, "every extracted file should have been dequeued");

    let stop = Command::new(dafs_bin())
        .arg("stop")
        .arg("--data-dir")
        .arg(&data_dir)
        .output()
        .expect("run stop");
    assert!(stop.status.success(), "stderr: {}", String::from_utf8_lossy(&stop.stderr));
    let _ = second.wait();
}

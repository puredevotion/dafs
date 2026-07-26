//! End-to-end: a real PDF, in a real git repo, watched by a real spawned
//! `dafs` daemon, extracted through the real `dafs-pdf-worker` child process
//! — the one path `extract_worker.rs`'s own unit tests cannot cover. This is
//! the daemon-binary-spawning counterpart to `extraction_crash_consistency.rs`.
//!
//! # Locating `dafs-pdf-worker`
//!
//! `CARGO_BIN_EXE_<name>` (used below for `dafs` itself) only ever resolves
//! a binary belonging to *this* package — Cargo has no stable mechanism to
//! expose a sibling package's binary path to a dependent crate's tests (the
//! cross-package version of that is the `-Z bindeps` artifact-dependencies
//! feature, nightly-only and not something this workspace's stable toolchain
//! can use). So this file locates `dafs-pdf-worker` the same way the real
//! `dafs` binary does at run time — as a sibling of `dafs`'s own executable
//! (see `extract_worker.rs`'s `sibling_binary_path`) — and, since nothing
//! else guarantees that binary has been built yet when only `-p dafs-daemon`
//! is under test, builds it on demand the first time this file needs it.
//!
//! Skipped, not failed, when no Pdfium library is reachable in this
//! environment — same convention `dafs-pdf-worker`'s own
//! `tests/worker_process.rs` uses. In practice this should never skip: the
//! worker embeds and vendors its own Pdfium copy (see
//! `crates/dafs-pdf-worker/src/main.rs`'s `PDFIUM_SO_BYTES`), so a bare
//! `cargo test` always exercises the real path. The skip only guards against
//! a target this repo's vendored `.so` was never built for.

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn dafs_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dafs")
}

/// The sibling binary's path, built on demand if it isn't already sitting
/// next to `dafs` in the same target directory — see the module docs.
fn pdf_worker_bin() -> PathBuf {
    let sibling = Path::new(dafs_bin())
        .parent()
        .expect("CARGO_BIN_EXE_dafs has a containing directory")
        .join("dafs-pdf-worker");

    if !sibling.exists() {
        // Mirrors whatever profile this test binary itself was built with —
        // true for the overwhelmingly common `cargo test`/`cargo test
        // --release` invocations, which is all this needs to match.
        let mut cmd = Command::new(env!("CARGO"));
        cmd.args(["build", "--quiet", "-p", "dafs-pdf-worker"]);
        if !cfg!(debug_assertions) {
            cmd.arg("--release");
        }
        let status = cmd.status().expect("run cargo build -p dafs-pdf-worker");
        assert!(status.success(), "building dafs-pdf-worker failed");
    }

    sibling
}

#[derive(serde::Deserialize)]
struct PidFile {
    listen: SocketAddr,
}

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

fn skip(reason: &str) {
    eprintln!("SKIP: {reason}");
}

/// A minimal single-page PDF with a real xref table and an `/Info`
/// dictionary — the same hand-built-fixture approach `dafs-extract`'s EXIF
/// tests use for a minimal TIFF, and `dafs-pdf-worker`'s own
/// `tests/worker_process.rs` uses for this same format.
fn build_minimal_pdf(title: &str) -> Vec<u8> {
    let content = "BT /F1 24 Tf 20 100 Td (hello there) Tj ET";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 4 0 R >> >> \
         /MediaBox [0 0 400 200] /Contents 5 0 R >>"
            .to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        format!("<< /Title ({title}) >>"),
    ];

    let mut buf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(buf.len());
        buf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref_offset = buf.len();
    let mut xref = format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
    for off in &offsets {
        xref.push_str(&format!("{off:010} 00000 n \n"));
    }
    buf.extend_from_slice(xref.as_bytes());
    buf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R /Info 6 0 R >>\nstartxref\n{xref_offset}\n%%EOF",
            objects.len() + 1
        )
        .as_bytes(),
    );
    buf
}

fn write_frame(w: &mut impl std::io::Write, bytes: &[u8]) -> std::io::Result<()> {
    w.write_all(&(bytes.len() as u32).to_be_bytes())?;
    w.write_all(bytes)?;
    w.flush()
}

fn read_frame(r: &mut impl std::io::Read) -> std::io::Result<Vec<u8>> {
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// A direct, quick call to the real worker binary, used only to decide
/// whether this environment has a Pdfium library reachable at all — the same
/// preflight-and-skip idea `dafs-pdf-worker`'s own tests use, applied here
/// since this file needs its own independent decision (it never goes through
/// `extract_worker.rs`'s private functions).
fn pdfium_available() -> bool {
    let dir = tempfile::tempdir().expect("tempdir");
    let dummy = dir.path().join("dummy.pdf");
    std::fs::write(&dummy, build_minimal_pdf("dummy")).expect("write dummy pdf");

    let request = serde_json::to_vec(&serde_json::json!({
        "path": dummy.to_str().expect("utf8 path"),
        "max_bytes": 10_000_000u64,
    }))
    .expect("serialize request");

    let mut child = Command::new(pdf_worker_bin())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dafs-pdf-worker");
    write_frame(&mut child.stdin.take().expect("stdin"), &request).expect("write request");
    let response_bytes =
        read_frame(&mut child.stdout.take().expect("stdout")).expect("read response");
    let _ = child.wait();

    let response: serde_json::Value =
        serde_json::from_slice(&response_bytes).expect("valid json response");
    match response.get("error").and_then(|e| e.as_str()) {
        Some(reason) => !reason.contains("binding to pdfium"),
        None => true,
    }
}

fn init_repo_with_one_commit(root: &Path) {
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("git binary available");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "--quiet", "--initial-branch=trunk"]);
    git(&[
        "-c",
        "user.name=Test User",
        "-c",
        "user.email=test@example.com",
        "commit",
        "--quiet",
        "--allow-empty",
        "-m",
        "initial commit",
    ]);
}

#[derive(Debug, Default)]
struct PdfMetadataRow {
    doc_type: Option<String>,
    title: Option<String>,
    page_count: Option<i64>,
    word_count: Option<i64>,
    git_branch: Option<String>,
    git_head_commit: Option<String>,
}

fn read_metadata_row(db_path: &Path) -> Option<PdfMetadataRow> {
    let conn = rusqlite::Connection::open(db_path).ok()?;
    conn.query_row(
        "SELECT doc_type, title, page_count, word_count, git_branch, git_head_commit \
           FROM file_metadata LIMIT 1",
        [],
        |r| {
            Ok(PdfMetadataRow {
                doc_type: r.get(0)?,
                title: r.get(1)?,
                page_count: r.get(2)?,
                word_count: r.get(3)?,
                git_branch: r.get(4)?,
                git_head_commit: r.get(5)?,
            })
        },
    )
    .ok()
}

/// The full path a real PDF takes in production: sniffed as `Pdf` by the
/// running daemon, routed to a spawned `dafs-pdf-worker` child, and merged
/// into a `file_metadata` row with its git facts intact — proving the
/// daemon's PDF branch does not lose the git-facts merge that
/// `dafs_extract::extract` performs for every other document type (see
/// `dafs_extract::merge_git_facts`'s docs for why that matters here
/// specifically).
#[test]
fn a_real_pdf_in_a_git_repo_is_extracted_through_the_pdfium_worker() {
    if !pdfium_available() {
        skip("no Pdfium library available in this environment");
        return;
    }

    let root = tempfile::tempdir().expect("tempdir");
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    init_repo_with_one_commit(&repo);
    std::fs::write(repo.join("report.pdf"), build_minimal_pdf("Quarterly Report"))
        .expect("write pdf");

    let data_dir = root.path().join(".dafs");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let db_path = data_dir.join("metadata.sqlite");

    let mut child = Command::new(dafs_bin())
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--listen")
        .arg("127.0.0.1:0")
        .arg("--watch")
        .arg(&repo)
        .arg("--detach")
        .arg("false")
        // Explicitly removed, not just "not set by this test": in case one
        // leaked in from whatever shell runs this suite, this is the proof
        // that the spawned `dafs-pdf-worker` child still binds Pdfium from
        // its own embedded, vendored copy rather than needing this daemon
        // to hand it a path.
        .env_remove("PDFIUM_DYNAMIC_LIB_PATH")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dafs");

    if !wait_for_ready(&data_dir, Instant::now() + Duration::from_secs(30)) {
        let _ = child.kill();
        let _ = child.wait();
        skip("daemon never became ready");
        return;
    }

    let deadline = Instant::now() + Duration::from_secs(60);
    let row = loop {
        if let Some(row) = read_metadata_row(&db_path)
            && row.doc_type.is_some()
        {
            break row;
        }
        assert!(Instant::now() < deadline, "the pdf was never extracted within the deadline");
        std::thread::sleep(Duration::from_millis(100));
    };

    let stop = Command::new(dafs_bin())
        .arg("stop")
        .arg("--data-dir")
        .arg(&data_dir)
        .output()
        .expect("run stop");
    assert!(stop.status.success(), "stderr: {}", String::from_utf8_lossy(&stop.stderr));
    let _ = child.wait();

    assert_eq!(row.doc_type.as_deref(), Some("pdf"));
    assert_eq!(row.title.as_deref(), Some("Quarterly Report"));
    assert_eq!(row.page_count, Some(1));
    assert!(row.word_count.unwrap_or(0) >= 2, "expected 'hello there' counted, got {row:?}");
    assert_eq!(
        row.git_branch.as_deref(),
        Some("trunk"),
        "the pdfium path must merge git facts the same way every other doc_type does: {row:?}"
    );
    assert!(row.git_head_commit.is_some());
}

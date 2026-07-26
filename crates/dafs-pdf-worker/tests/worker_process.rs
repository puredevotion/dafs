//! Spawns the real `dafs-pdf-worker` binary and drives it exactly the way
//! `dafs-daemon`'s supervisor does (write a length-prefixed request to
//! stdin, read a length-prefixed response from stdout) — a unit test against
//! `main.rs`'s functions alone would not catch a framing mismatch between
//! what this binary writes and what a caller expects to read.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

#[derive(Debug, serde::Deserialize)]
struct Response {
    title: Option<String>,
    author: Option<String>,
    page_count: Option<i64>,
    word_count: Option<i64>,
    #[allow(dead_code)]
    language: Option<String>,
    error: Option<String>,
}

fn write_frame(w: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
    w.write_all(&(bytes.len() as u32).to_be_bytes())?;
    w.write_all(bytes)?;
    w.flush()
}

fn read_frame(r: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Hand-builds a minimal single-page PDF with a real xref table and an
/// `/Info` dictionary — the same "construct the smallest real instance of
/// the format by hand, with byte-exact offsets" approach `dafs-extract`'s
/// EXIF tests use for a minimal TIFF, applied to PDF's simpler-but-still-
/// exact offset table.
fn build_minimal_pdf(body_text: &str, title: &str, author: &str) -> Vec<u8> {
    let content = format!("BT /F1 24 Tf 20 100 Td ({body_text}) Tj ET");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        // Wide enough that 24pt Helvetica text at this test's length never
        // runs past the page's crop box — Pdfium's text extraction only
        // reports glyphs actually placed within the page, so a too-narrow
        // `MediaBox` silently truncates the extracted text rather than
        // erroring, which is not this test's concern.
        "<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 4 0 R >> >> \
         /MediaBox [0 0 400 200] /Contents 5 0 R >>"
            .to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        format!("<< /Title ({title}) /Author ({author}) >>"),
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

fn run_worker(request: &serde_json::Value) -> Response {
    run_worker_with(request, |cmd| cmd)
}

/// [`run_worker`], with the spawned `Command` passed through `configure`
/// first — the hook [`a_real_pdf_extracts_with_no_pdfium_dynamic_lib_path_set`]
/// uses to strip an env var from the child regardless of what this test
/// binary's own process happens to have inherited.
fn run_worker_with(
    request: &serde_json::Value,
    configure: impl FnOnce(&mut Command) -> &mut Command,
) -> Response {
    let request_bytes = serde_json::to_vec(request).expect("serialize request");

    let mut command = Command::new(env!("CARGO_BIN_EXE_dafs-pdf-worker"));
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    configure(&mut command);

    let mut child = command.spawn().expect("spawn dafs-pdf-worker");

    write_frame(&mut child.stdin.take().expect("stdin was piped"), &request_bytes)
        .expect("write request frame");

    let response_bytes = read_frame(&mut child.stdout.take().expect("stdout was piped"))
        .expect("read response frame");

    let status = child.wait().expect("wait for worker");
    assert!(status.success(), "worker exited non-zero on a well-formed request");

    serde_json::from_slice(&response_bytes).expect("response is valid json")
}

/// The end-to-end round trip the daemon's supervisor relies on: a real PDF
/// in, real extracted fields out, over the actual process boundary.
///
/// Skipped (not failed) when no Pdfium library is reachable in this
/// environment — the same "skip, don't fail, when an external capability
/// this test needs isn't present" convention `extraction_crash_consistency.rs`
/// uses for unix-only fault injection. In practice this should never skip: the
/// worker embeds and vendors its own Pdfium copy (see `src/main.rs`'s
/// `PDFIUM_SO_BYTES`), so the real path is exercised by a plain `cargo test`
/// with no setup. The skip only guards against a target this repo's vendored
/// `.so` was never built for.
#[test]
fn a_real_pdf_round_trips_through_the_worker_process() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pdf_path = dir.path().join("sample.pdf");
    std::fs::write(
        &pdf_path,
        build_minimal_pdf("Hello integration test world", "Sample PDF", "Ada Lovelace"),
    )
    .expect("write pdf");

    let response = run_worker(&serde_json::json!({
        "path": pdf_path.to_str().expect("utf8 path"),
        "max_bytes": 10_000_000u64,
    }));

    if let Some(err) = &response.error {
        if err.contains("binding to pdfium") {
            eprintln!("SKIP: no Pdfium library available in this environment: {err}");
            return;
        }
        panic!("unexpected extraction error: {err}");
    }

    assert_eq!(response.page_count, Some(1));
    assert!(
        response.word_count.unwrap_or(0) >= 4,
        "expected the body text's words to be counted, got {:?}",
        response.word_count
    );
    assert_eq!(response.title.as_deref(), Some("Sample PDF"));
    assert_eq!(response.author.as_deref(), Some("Ada Lovelace"));
}

/// A missing file must fail as an ordinary error response, not a nonzero
/// exit or a hang — the daemon's supervisor treats "process exited cleanly
/// with an error field set" as the normal retryable-failure path.
#[test]
fn a_missing_file_yields_a_clean_error_response() {
    let response = run_worker(&serde_json::json!({
        "path": "/no/such/file/anywhere.pdf",
        "max_bytes": 1_000_000u64,
    }));

    assert!(response.error.is_some());
    assert!(response.title.is_none());
}

/// Malformed input on stdin (not even valid JSON) must still get exactly one
/// well-formed response frame back, never a hang or a panic that kills the
/// process without writing anything.
#[test]
fn garbage_stdin_still_yields_one_response_frame() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dafs-pdf-worker"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dafs-pdf-worker");

    write_frame(&mut child.stdin.take().expect("stdin was piped"), b"not json at all")
        .expect("write garbage frame");

    let response_bytes = read_frame(&mut child.stdout.take().expect("stdout was piped"))
        .expect("read response frame");
    let status = child.wait().expect("wait for worker");
    assert!(status.success());

    let response: Response = serde_json::from_slice(&response_bytes).expect("valid json response");
    assert!(response.error.is_some());
}

/// The concrete proof that this binary is standalone: with
/// `PDFIUM_DYNAMIC_LIB_PATH` explicitly removed from the child's environment
/// (in case one leaked in from whatever shell runs this test suite), the
/// worker must still bind Pdfium — from its own embedded, vendored copy —
/// and extract a real PDF's text. No env var, no Nix, no system package.
#[test]
fn a_real_pdf_extracts_with_no_pdfium_dynamic_lib_path_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pdf_path = dir.path().join("sample.pdf");
    std::fs::write(
        &pdf_path,
        build_minimal_pdf("Hello standalone embedded pdfium", "Standalone PDF", "Ada Lovelace"),
    )
    .expect("write pdf");

    let response = run_worker_with(
        &serde_json::json!({
            "path": pdf_path.to_str().expect("utf8 path"),
            "max_bytes": 10_000_000u64,
        }),
        |cmd| cmd.env_remove("PDFIUM_DYNAMIC_LIB_PATH"),
    );

    assert!(response.error.is_none(), "expected a clean extraction, got {:?}", response.error);
    assert_eq!(response.page_count, Some(1));
    assert!(
        response.word_count.unwrap_or(0) >= 4,
        "expected the body text's words to be counted, got {:?}",
        response.word_count
    );
    assert_eq!(response.title.as_deref(), Some("Standalone PDF"));
}

//! Standalone PDF text-extraction worker (M02a).
//!
//! `dafs_extract::extract` deliberately stubs PDF: `pdfium-render` wraps
//! Pdfium, a C++ library parsing bytes a hostile user chose, and C++ is not
//! memory-safe the way the rest of this workspace's extractors are. Running
//! it inside the daemon would mean one malformed PDF can take the whole
//! daemon down with it — the same reasoning `docs/roadmap-and-design-review.md`
//! §8 applies to the LLM worker, applied here to a native parser instead.
//!
//! This binary is the isolation boundary: `dafs-daemon`'s extraction worker
//! (`crates/dafs-daemon/src/extract_worker.rs`) spawns one of these per PDF,
//! writes a request, reads a response, and treats the whole process — clean
//! exit, non-zero exit, or vanishing without a response — as the unit of
//! failure. **One request per invocation**, not a request/response loop: a
//! persistent worker would be more efficient, but it would also mean a
//! `catch_unwind` around one bad file could still leave Pdfium's global,
//! process-wide state (`pdfium-render` allows exactly one bound library
//! instance per process — see `Pdfium::new`) corrupted for the *next*
//! request in the same process. Exiting after one file makes every request
//! start from a fresh process and a fresh library binding, which is strictly
//! stronger isolation at the cost of a fork+exec per PDF — the right trade
//! for a workload that is not on any user-facing hot path.
//!
//! # Wire protocol
//!
//! One length-prefixed JSON request read from stdin, one length-prefixed
//! JSON response written to stdout, then the process exits. The prefix is a
//! big-endian `u32` byte count — simple enough to hand-roll without a
//! framing crate, and a fixed-size count (rather than a delimiter) makes a
//! truncated read unambiguous: short reads are structurally distinguishable
//! from "no more data".

use std::io::{self, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use pdfium_render::prelude::*;
use serde::{Deserialize, Serialize};

/// Mirrors `dafs_extract::office`'s threshold exactly: below this many
/// characters `whatlang` guesses wildly, and no language is a more honest
/// answer than a confident wrong one.
const MIN_CHARS_FOR_LANG_DETECT: usize = 40;

/// The env var `pdfium-render`'s own README documents for a dynamically
/// loaded library's path. Checked first, ahead of the embedded copy below —
/// a deployer pointing at their own system Pdfium (a different build, a
/// security patch, a non-x64 target) should always win over what this binary
/// carries by default.
const PDFIUM_LIB_PATH_VAR: &str = "PDFIUM_DYNAMIC_LIB_PATH";

/// The vendored Pdfium shared library, embedded directly in the binary so a
/// plain `cargo build` — no Nix, no network, no manual setup — produces a
/// working `dafs-pdf-worker`. See `vendor/NOTICE.md` for exactly which
/// release this is and its license.
static PDFIUM_SO_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/vendor/libpdfium_linux_x64.so"));

/// Identifies [`PDFIUM_SO_BYTES`] in its extracted-to-disk filename. Bumping
/// the vendored release (a different file at the same `vendor/` path) must
/// produce a different on-disk name too, or a binary built against the new
/// release could bind an old release's bytes left behind by a previous
/// install — this constant and the vendored file change together.
const PDFIUM_SO_VERSION: &str = "chromium-7961";

#[derive(Debug, Deserialize)]
struct Request {
    path: String,
    max_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Response {
    title: Option<String>,
    author: Option<String>,
    page_count: Option<i64>,
    word_count: Option<i64>,
    language: Option<String>,
    /// `Some` means every other field above is meaningless — the caller
    /// checks this first, not "are the other fields all `None`", since a
    /// PDF with genuinely no title/author is a successful extraction too.
    error: Option<String>,
}

impl Response {
    fn error(message: impl Into<String>) -> Self {
        Response {
            title: None,
            author: None,
            page_count: None,
            word_count: None,
            language: None,
            error: Some(message.into()),
        }
    }
}

fn main() {
    let response = match read_frame(&mut io::stdin()) {
        Ok(bytes) => respond_to(&bytes),
        // Nothing was readable at all: the parent likely isn't waiting for a
        // response either, but writing one costs nothing and keeps this
        // process's contract ("always write exactly one frame") absolute.
        Err(e) => Response::error(format!("reading request: {e}")),
    };

    let body = serde_json::to_vec(&response).expect("Response serializes infallibly");
    if let Err(e) = write_frame(&mut io::stdout(), &body) {
        eprintln!("dafs-pdf-worker: writing response: {e}");
        std::process::exit(1);
    }
}

/// Parse the request and run the extraction, collapsing every failure mode
/// (malformed JSON, a missing file, Pdfium erroring, Pdfium's C++ core
/// panicking through the safe Rust wrapper) into one [`Response`] shape —
/// the daemon's supervisor needs exactly one thing to check, not a menu of
/// distinct error paths.
fn respond_to(request_bytes: &[u8]) -> Response {
    let request: Request = match serde_json::from_slice(request_bytes) {
        Ok(r) => r,
        Err(e) => return Response::error(format!("malformed request: {e}")),
    };

    // `catch_unwind` here is the same defence `dafs_extract::extract` uses
    // around its own parsers, for the reason this binary exists at all: the
    // C++ underneath `pdfium-render` is not memory-safe, and a Rust-level
    // panic from the wrapper is an expected failure mode for hostile input,
    // not a bug. A segfault or abort is *not* caught by this — that is what
    // running as a separate process is for; the parent handles a fully dead
    // child, this only handles a merely panicked one.
    match catch_unwind(AssertUnwindSafe(|| extract(&request))) {
        Ok(Ok(response)) => response,
        Ok(Err(message)) => Response::error(message),
        Err(_panic) => Response::error("pdfium extraction panicked"),
    }
}

fn extract(request: &Request) -> Result<Response, String> {
    let bytes = read_capped(Path::new(&request.path), request.max_bytes)
        .map_err(|e| format!("reading {}: {e}", request.path))?;

    let pdfium = Pdfium::new(bind_pdfium()?);
    let document =
        pdfium.load_pdf_from_byte_slice(&bytes, None).map_err(|e| format!("loading pdf: {e}"))?;

    let title = metadata_tag(&document, PdfDocumentMetadataTagType::Title);
    let author = metadata_tag(&document, PdfDocumentMetadataTagType::Author);

    let mut text = String::new();
    for page in document.pages().iter() {
        let page_text = page.text().map_err(|e| format!("reading page text: {e}"))?;
        text.push_str(&page_text.all());
        // Same reasoning as office.rs's inter-sheet/inter-slide newline:
        // without it, the last word of one page fuses with the first word
        // of the next into a single token.
        text.push('\n');
    }

    Ok(Response {
        title,
        author,
        page_count: Some(document.pages().len() as i64),
        word_count: Some(word_count(&text)),
        language: detect_language(&text),
        error: None,
    })
}

/// Binds to Pdfium, in priority order:
///
/// 1. [`PDFIUM_LIB_PATH_VAR`], if a deployer set it — an explicit override
///    always wins.
/// 2. The embedded copy ([`PDFIUM_SO_BYTES`]), extracted to disk once and
///    bound from there. This is the path that makes the binary work with
///    zero setup, and is expected to succeed on every supported target
///    (linux-x64; see this crate's `Cargo.toml`).
/// 3. `Pdfium::bind_to_system_library`, the same fallback `pdfium-render`'s
///    own examples use, in case neither of the above bound — e.g. a CPU
///    architecture the vendored `.so` was never built for.
fn bind_pdfium() -> Result<Box<dyn PdfiumLibraryBindings>, String> {
    if let Ok(path) = std::env::var(PDFIUM_LIB_PATH_VAR)
        && let Ok(bindings) = Pdfium::bind_to_library(&path)
    {
        return Ok(bindings);
    }

    if let Ok(path) = ensure_pdfium_extracted()
        && let Ok(bindings) = Pdfium::bind_to_library(&path)
    {
        return Ok(bindings);
    }

    Pdfium::bind_to_system_library().map_err(|e| format!("binding to pdfium: {e}"))
}

/// Where [`PDFIUM_SO_BYTES`] lands once extracted. Under `temp_dir()` (not
/// alongside the binary itself, whose install location may not be
/// writable) and named with [`PDFIUM_SO_VERSION`] so a rebuild against a
/// different vendored release never collides with, or is silently satisfied
/// by, a stale extraction of an older one.
fn extracted_pdfium_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("dafs-pdfium-{PDFIUM_SO_VERSION}.so"))
}

/// Writes [`PDFIUM_SO_BYTES`] to [`extracted_pdfium_path`] if it is not
/// already there. This binary is spawned fresh per PDF (see the module
/// docs), so re-writing several megabytes on every single invocation would
/// be real, avoidable I/O for a file whose contents never change between
/// runs of the same build.
///
/// Written to a temporary name first and then renamed into place, rather
/// than written in place, so two invocations racing on the very first
/// extraction can never have a reader observe a torn file: `rename` onto an
/// existing path is atomic on the same filesystem, and both paths here are
/// under `temp_dir()`. Whichever process's rename lands last simply
/// overwrites the other's — harmless, since both wrote identical bytes.
fn ensure_pdfium_extracted() -> io::Result<std::path::PathBuf> {
    let dest = extracted_pdfium_path();
    if dest.is_file() {
        return Ok(dest);
    }

    let tmp = dest.with_extension(format!("so.tmp.{}", std::process::id()));
    std::fs::write(&tmp, PDFIUM_SO_BYTES)?;
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

fn metadata_tag(document: &PdfDocument<'_>, tag: PdfDocumentMetadataTagType) -> Option<String> {
    let value = document.metadata().get(tag)?.value().trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn word_count(text: &str) -> i64 {
    text.split_whitespace().count() as i64
}

fn detect_language(text: &str) -> Option<String> {
    if text.trim().chars().count() < MIN_CHARS_FOR_LANG_DETECT {
        return None;
    }
    whatlang::detect(text).map(|info| info.lang().code().to_string())
}

/// Same cap discipline as `dafs_extract::MAX_EXTRACT_BYTES`: bounds resident
/// memory for one extraction regardless of the on-disk file's real size.
/// `max_bytes` is caller-supplied (rather than a constant baked into this
/// binary) so the daemon's cap and this worker's cap can never drift apart.
fn read_capped(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut limited = file.take(max_bytes);
    let mut buf = Vec::new();
    limited.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Read one big-endian-`u32`-length-prefixed frame. `Ok` only once the exact
/// declared number of bytes has been read — a short read is an error, not a
/// partial frame handed to the caller.
fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)?;
    let len = u32::from_be_bytes(len_bytes) as usize;

    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Write one big-endian-`u32`-length-prefixed frame, matching [`read_frame`].
fn write_frame(w: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    w.write_all(&(bytes.len() as u32).to_be_bytes())?;
    w.write_all(bytes)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_frame_then_write_frame_round_trips() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello").expect("write");

        let mut cursor = io::Cursor::new(buf);
        let out = read_frame(&mut cursor).expect("read");
        assert_eq!(out, b"hello");
    }

    #[test]
    fn read_frame_on_a_truncated_stream_is_an_error_not_a_panic() {
        // Declares a 100-byte frame but supplies none of it.
        let mut cursor = io::Cursor::new(100u32.to_be_bytes().to_vec());
        assert!(read_frame(&mut cursor).is_err());
    }

    #[test]
    fn read_frame_on_empty_input_is_an_error() {
        let mut cursor = io::Cursor::new(Vec::new());
        assert!(read_frame(&mut cursor).is_err());
    }

    #[test]
    fn malformed_request_json_yields_an_error_response_not_a_panic() {
        let response = respond_to(b"not json at all");
        assert!(response.error.is_some());
        assert_eq!(response, Response::error(response.error.clone().unwrap()));
    }

    #[test]
    fn a_missing_file_yields_an_error_response() {
        let request = serde_json::to_vec(&serde_json::json!({
            "path": "/no/such/file/anywhere.pdf",
            "max_bytes": 1_000_000u64,
        }))
        .expect("serialize request");

        let response = respond_to(&request);
        assert!(response.error.is_some());
        assert!(response.title.is_none());
    }

    #[test]
    fn word_count_splits_on_whitespace() {
        assert_eq!(word_count("hello   world\nagain"), 3);
    }

    #[test]
    fn short_text_gets_no_language_guess() {
        assert_eq!(detect_language("hi"), None);
    }

    #[test]
    fn read_capped_truncates_at_max_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.bin");
        std::fs::write(&path, vec![7u8; 1000]).expect("write");

        let bytes = read_capped(&path, 10).expect("read_capped");
        assert_eq!(bytes.len(), 10);
    }

    #[test]
    fn ensure_pdfium_extracted_writes_bytes_matching_the_embedded_copy() {
        let path = ensure_pdfium_extracted().expect("extract embedded pdfium");
        let on_disk = std::fs::read(&path).expect("read extracted file");
        assert_eq!(on_disk, PDFIUM_SO_BYTES);
    }

    /// The whole point of the check-then-write shape: a second call must
    /// reuse the file already on disk, not pay to rewrite several megabytes
    /// it just wrote a moment ago.
    #[test]
    fn ensure_pdfium_extracted_does_not_rewrite_an_existing_file() {
        let first = ensure_pdfium_extracted().expect("first extract");
        let mtime_before = std::fs::metadata(&first).expect("metadata").modified().expect("mtime");

        let second = ensure_pdfium_extracted().expect("second extract");
        let mtime_after = std::fs::metadata(&second).expect("metadata").modified().expect("mtime");

        assert_eq!(first, second);
        assert_eq!(
            mtime_before, mtime_after,
            "a second call rewrote a file that was already present"
        );
    }

    /// Proves the standalone story: with no override set (not by this
    /// process, and not inherited from whatever shell launched the test
    /// runner), binding must still succeed from the embedded copy alone —
    /// no Nix, no system Pdfium, no manual setup.
    #[test]
    fn bind_pdfium_succeeds_with_no_override_env_var_set() {
        // SAFETY: test-only removal of a var this process's own code reads;
        // nothing else in this binary spawns threads that read it concurrently.
        unsafe {
            std::env::remove_var(PDFIUM_LIB_PATH_VAR);
        }

        if let Err(e) = bind_pdfium() {
            panic!("embedded pdfium must bind with no env var set: {e}");
        }
    }
}

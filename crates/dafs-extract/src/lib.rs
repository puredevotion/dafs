//! Deterministic metadata extraction (M02a). No LLM anywhere in this crate —
//! summary, keywords, entities, and classification are M02b's job and arrive
//! as a separate crate later. What lives here is mechanically derivable from
//! a file's bytes: document type, title, author, language, page/word counts,
//! EXIF fields, and git repo facts.
//!
//! # Untrusted input
//!
//! Every byte this crate parses came from a file the daemon does not control
//! the contents of. [`extract`] wraps every extractor call in `catch_unwind`
//! for exactly that reason: a panic deep in a parsing library is an expected
//! failure mode for hostile input, not a bug to let take the daemon down.
//!
//! PDF extraction is **not** in this crate. `pdfium-render` is native code
//! parsing the same untrusted bytes, and process isolation (a separate
//! supervised worker, per `docs/roadmap-and-design-review.md` §8's "the model
//! must not live inside the daemon" reasoning applied to a native parser) is
//! the daemon's job, not this crate's. See `crates/dafs-daemon`.
//!
//! # Git facts are orthogonal to content type
//!
//! A file's document type (PDF, docx, ...) and whether it happens to live
//! inside a git working tree are independent questions — a spreadsheet
//! checked into a repo is still a spreadsheet. [`extract`] therefore runs the
//! content-specific extractor for `doc_type` first, then separately merges in
//! [`git::lookup`]'s repo-level facts (branch, HEAD commit) if the file's
//! path resolves to one, rather than treating "is a git repo" as one more
//! mutually-exclusive `DocType` variant. That merge is public as
//! [`merge_git_facts`] so `dafs-daemon`'s pdfium worker path — which bypasses
//! [`extract`] entirely for PDFs — gets the identical git-facts behaviour
//! every other document type does, from one implementation.

#![forbid(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

mod exif;
mod filename;
mod git;
mod office;
mod sniff;

pub use sniff::{DocType, sniff};

/// Bumped whenever extraction logic changes meaningfully. Stored alongside
/// each file's metadata (`dafs_store::metadata::FileMetadata::extractor_version`)
/// so an upgrade can find and reprocess everything a prior version extracted.
pub const EXTRACTOR_VERSION: u32 = 1;

/// Deterministic metadata extracted from one file. Every field but `doc_type`
/// is optional: most extractors only ever populate a handful of these, and
/// "not applicable to this file type" is a normal outcome, not a failure.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Extraction {
    pub doc_type: DocType,
    pub title: Option<String>,
    pub author: Option<String>,
    pub language: Option<String>,
    pub page_count: Option<i64>,
    pub word_count: Option<i64>,
    pub image_taken_at_unix: Option<i64>,
    pub image_camera_model: Option<String>,
    pub git_branch: Option<String>,
    pub git_head_commit: Option<String>,
    pub git_head_author: Option<String>,
    pub git_head_at_unix: Option<i64>,
    /// The concatenated body text a text-bearing extractor already builds
    /// internally to compute `word_count`/`language` — exposed here (M02b)
    /// so an LLM enrichment pass can read it back rather than re-parsing the
    /// file a second time. `None` for anything with no text to expose
    /// (images, git-only facts) — never populated just because it's cheap
    /// to; the OOXML extractors already have this string, this field is
    /// exposing what they compute, not new extraction work.
    pub body_text: Option<String>,
}

/// Extraction failed. Always non-fatal to the caller — a failed extraction
/// leaves a file's queue entry in place for a retry (see
/// `dafs_store::metadata`), it never propagates as a daemon-level error.
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{doc_type:?} extractor panicked on {path}")]
    Panicked { doc_type: DocType, path: String },

    #[error("{doc_type:?} extractor failed on {path}: {reason}")]
    Malformed { doc_type: DocType, path: String, reason: String },
}

/// The byte cap every extractor reads under. Bounds memory for a single
/// extraction regardless of how large the on-disk file is — a multi-gigabyte
/// PDF or a zip bomb inside a docx should cost a bounded amount of resident
/// memory, not crash the worker that picked it up.
///
/// 64 MiB: generous for the text-bearing documents this crate targets (a
/// document with more actual content than that is not what "read a summary
/// without opening the file" is for), small enough to keep a handful of
/// concurrent extractions well under the daemon's memory budget.
pub const MAX_EXTRACT_BYTES: u64 = 64 * 1024 * 1024;

/// The character cap on [`Extraction::body_text`]. Independent of
/// [`MAX_EXTRACT_BYTES`]: that bounds what's read off disk, this bounds what
/// gets stored and later sent to an LLM — a few thousand characters is far
/// more than a summarization prompt needs, and storing the full text of
/// every large document would grow `file_metadata` roughly in proportion to
/// corpus size, which is exactly what M01's path-interning work exists to
/// avoid elsewhere in this store.
pub const MAX_BODY_TEXT_CHARS: usize = 8_000;

/// Truncate to [`MAX_BODY_TEXT_CHARS`] at a `char` boundary — byte-slicing
/// arbitrary extracted text can land inside a multi-byte UTF-8 sequence and
/// panic, which a `char_indices` walk cannot.
pub fn cap_body_text(text: &str) -> String {
    match text.char_indices().nth(MAX_BODY_TEXT_CHARS) {
        Some((byte_idx, _)) => text[..byte_idx].to_string(),
        None => text.to_string(),
    }
}

/// Sniff and extract in one call: the entry point `dafs-daemon`'s extraction
/// worker calls per queued file.
///
/// PDF is deliberately unhandled here — sniffing still reports [`DocType::Pdf`]
/// (so the caller's dispatch to the pdfium worker is driven by the same
/// sniffing logic as everything else), but calling `extract` on one returns
/// `Ok(Extraction { doc_type: Pdf, ..Default::default() })` rather than an
/// error, since "not this crate's job" is not a failure — the daemon routes
/// PDFs to the pdfium worker before ever calling this function on one.
pub fn extract(path: &Path) -> Result<Extraction, ExtractError> {
    let doc_type = sniff(path).map_err(|e| ExtractError::Io { path: display(path), source: e })?;

    let needs_bytes = !matches!(doc_type, DocType::Unknown | DocType::Text | DocType::Pdf);
    let bytes = if needs_bytes { read_capped(path)? } else { Vec::new() };

    let path_owned = display(path);
    let content = catch_unwind(AssertUnwindSafe(|| match doc_type {
        DocType::Pdf | DocType::Unknown | DocType::Text => Ok(Extraction::default()),
        DocType::Jpeg | DocType::Tiff => exif::extract(&bytes),
        DocType::Docx => office::extract_docx(&bytes),
        DocType::Xlsx => office::extract_xlsx(&bytes),
        DocType::Pptx => office::extract_pptx(&bytes),
    }));

    let mut extraction = match content {
        Ok(Ok(extraction)) => extraction,
        Ok(Err(reason)) => {
            return Err(ExtractError::Malformed {
                doc_type,
                path: path_owned,
                reason: reason.to_string(),
            });
        }
        Err(_panic) => return Err(ExtractError::Panicked { doc_type, path: path_owned }),
    };
    extraction.doc_type = doc_type;

    merge_git_facts(&mut extraction, path);
    Ok(extraction)
}

/// Merge a file's git repo facts (branch, HEAD commit, HEAD author/time)
/// into an already-built [`Extraction`] in place.
///
/// Exposed as its own function — rather than folded silently into
/// [`extract`]'s body — because `dafs-daemon`'s pdfium worker path bypasses
/// [`extract`] entirely for PDFs (see this crate's module docs): the daemon
/// still needs the exact same git-facts merge for a PDF that this function
/// gives every other document type, and duplicating `git::lookup`'s
/// `catch_unwind`/field-mapping logic in the daemon would let the two copies
/// drift. [`extract`] itself calls this too, so there is exactly one
/// implementation.
///
/// A lookup failure (not a repo, a corrupt `.git`, an I/O error, or the
/// lookup panicking) just means no facts to merge, never a hard error — this
/// is enrichment, not the primary extraction.
pub fn merge_git_facts(extraction: &mut Extraction, path: &Path) {
    if let Ok(Some(facts)) = catch_unwind(AssertUnwindSafe(|| git::lookup(path))) {
        extraction.git_branch = facts.branch;
        extraction.git_head_commit = facts.head_commit;
        extraction.git_head_author = facts.head_author;
        extraction.git_head_at_unix = facts.head_at_unix;
    }
}

fn read_capped(path: &Path) -> Result<Vec<u8>, ExtractError> {
    use std::io::Read;

    let file = std::fs::File::open(path)
        .map_err(|e| ExtractError::Io { path: display(path), source: e })?;
    let mut limited = file.take(MAX_EXTRACT_BYTES);
    let mut buf = Vec::new();
    limited
        .read_to_end(&mut buf)
        .map_err(|e| ExtractError::Io { path: display(path), source: e })?;
    Ok(buf)
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracting_a_missing_file_is_an_io_error() {
        let err = extract(Path::new("/no/such/file/at/all.pdf")).unwrap_err();
        assert!(matches!(err, ExtractError::Io { .. }));
    }

    #[test]
    fn an_unknown_file_extracts_cleanly_with_no_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mystery.bin");
        std::fs::write(&path, [0u8, 1, 2, 3]).expect("write");

        let extraction = extract(&path).expect("extract");
        assert_eq!(extraction.doc_type, DocType::Unknown);
        assert_eq!(extraction, Extraction { doc_type: DocType::Unknown, ..Default::default() });
    }

    /// `merge_git_facts` is what the daemon's pdfium worker path calls
    /// directly (it never goes through `extract`, see the crate's module
    /// docs) — this is its own contract test, independent of `extract`'s.
    #[test]
    fn merge_git_facts_leaves_a_non_repo_file_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("report.pdf");
        std::fs::write(&path, b"not a real pdf").expect("write");

        let mut extraction = Extraction { doc_type: DocType::Pdf, ..Default::default() };
        merge_git_facts(&mut extraction, &path);
        assert_eq!(extraction, Extraction { doc_type: DocType::Pdf, ..Default::default() });
    }
}

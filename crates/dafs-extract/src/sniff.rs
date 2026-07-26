//! Content-type detection: magic bytes first, filename extension as a
//! fallback for formats `infer` cannot distinguish by content alone (OOXML's
//! three formats all sniff as a plain zip).

use std::io::Read;
use std::path::Path;

use crate::filename;

/// What kind of document a file is, as far as extraction dispatch cares.
/// Deliberately not "every MIME type" — only the types this crate (or the
/// daemon's pdfium worker) has an extractor for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DocType {
    #[default]
    Unknown,
    Pdf,
    Docx,
    Xlsx,
    Pptx,
    Jpeg,
    Tiff,
    /// Plain text or source code, recognised by extension only — no content
    /// extraction happens for these, just the classification itself (the
    /// "filename/FS metadata" extractor the roadmap names, which needs no
    /// parsing library at all).
    Text,
}

impl DocType {
    /// The stored form, for `file_metadata.doc_type` and the `/facets`
    /// endpoint. Lowercase and stable — this is read back by the UI and
    /// potentially by a user's own tooling against the database, so it must
    /// not silently change shape between versions.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Xlsx => "xlsx",
            Self::Pptx => "pptx",
            Self::Jpeg => "jpeg",
            Self::Tiff => "tiff",
            Self::Text => "text",
        }
    }
}

/// Determine a file's [`DocType`] from its first bytes, falling back to its
/// extension when the content alone is ambiguous (a zip's magic bytes cannot
/// tell docx from xlsx from pptx — only the parts inside it can, and reading
/// those is the extractor's job, not sniffing's).
///
/// Reads at most a few KiB regardless of file size — sniffing is a fixed-cost
/// operation independent of corpus size, the same "stream, don't accumulate"
/// principle the scan path already follows.
pub fn sniff(path: &Path) -> std::io::Result<DocType> {
    const SNIFF_LEN: usize = 8192;

    let mut file = std::fs::File::open(path)?;
    let mut head = vec![0u8; SNIFF_LEN];
    let n = file.read(&mut head)?;
    head.truncate(n);

    if let Some(kind) = infer::get(&head) {
        match kind.mime_type() {
            "application/pdf" => return Ok(DocType::Pdf),
            "image/jpeg" => return Ok(DocType::Jpeg),
            "image/tiff" => return Ok(DocType::Tiff),
            // OOXML formats (docx/xlsx/pptx) and plain zip all sniff
            // identically here; the extension disambiguates.
            "application/zip" => {
                if let Some(office_type) = filename::office_type_by_extension(path) {
                    return Ok(office_type);
                }
            }
            _ => {}
        }
    }

    Ok(filename::guess(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("write");
        path
    }

    #[test]
    fn pdf_magic_bytes_are_recognised_regardless_of_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(&dir, "report.bin", b"%PDF-1.7\n...rest of a pdf...");
        assert_eq!(sniff(&path).expect("sniff"), DocType::Pdf);
    }

    #[test]
    fn a_docx_is_disambiguated_from_a_plain_zip_by_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A minimal real zip local-file-header magic, no real entries — sniff
        // only needs to recognise the container, not parse it.
        let path = write(&dir, "letter.docx", b"PK\x03\x04rest-is-irrelevant-to-sniffing");
        assert_eq!(sniff(&path).expect("sniff"), DocType::Docx);
    }

    #[test]
    fn an_unrecognised_zip_falls_back_to_unknown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(&dir, "archive.zip", b"PK\x03\x04rest-is-irrelevant-to-sniffing");
        assert_eq!(sniff(&path).expect("sniff"), DocType::Unknown);
    }

    #[test]
    fn a_source_file_is_classified_as_text_by_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(&dir, "main.rs", b"fn main() {}");
        assert_eq!(sniff(&path).expect("sniff"), DocType::Text);
    }

    #[test]
    fn an_empty_file_does_not_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(&dir, "empty.bin", b"");
        assert_eq!(sniff(&path).expect("sniff"), DocType::Unknown);
    }
}

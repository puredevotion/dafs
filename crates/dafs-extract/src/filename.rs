//! The "filename/FS metadata" extractor the roadmap names: classification by
//! extension alone, no bytes read and no dependency needed. Used by
//! [`crate::sniff`] both as the general fallback and to disambiguate OOXML's
//! three formats, which are indistinguishable from a plain zip by content.

use std::path::Path;

use crate::DocType;

/// Extensions treated as plain text/source — not an exhaustive list of every
/// language, just enough that a developer's working tree shows up as `text`
/// facets rather than `unknown`.
const TEXT_EXTENSIONS: &[&str] = &[
    "txt",
    "md",
    "markdown",
    "rst",
    "org",
    "rs",
    "py",
    "js",
    "ts",
    "jsx",
    "tsx",
    "go",
    "java",
    "c",
    "h",
    "cpp",
    "hpp",
    "cc",
    "cs",
    "rb",
    "php",
    "sh",
    "bash",
    "zsh",
    "toml",
    "yaml",
    "yml",
    "json",
    "xml",
    "html",
    "css",
    "sql",
    "lua",
    "swift",
    "kt",
    "scala",
    "el",
    "vim",
    "conf",
    "ini",
    "cfg",
    "gitignore",
    "env",
];

fn extension(path: &Path) -> Option<String> {
    path.extension().map(|e| e.to_string_lossy().to_lowercase())
}

/// Classify by extension when content sniffing found nothing more specific.
pub fn guess(path: &Path) -> DocType {
    match extension(path).as_deref() {
        Some(ext) if TEXT_EXTENSIONS.contains(&ext) => DocType::Text,
        Some("pdf") => DocType::Pdf,
        _ => office_type_by_extension(path).unwrap_or(DocType::Unknown),
    }
}

/// The one place `docx`/`xlsx`/`pptx` extensions map to their `DocType` —
/// used both as sniffing's zip-disambiguation step and as a plain extension
/// guess when a file couldn't be opened for content sniffing at all.
pub fn office_type_by_extension(path: &Path) -> Option<DocType> {
    match extension(path).as_deref() {
        Some("docx") => Some(DocType::Docx),
        Some("xlsx") => Some(DocType::Xlsx),
        Some("pptx") => Some(DocType::Pptx),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_source_extensions_are_text() {
        for ext in ["rs", "py", "md", "toml"] {
            assert_eq!(guess(Path::new(&format!("file.{ext}"))), DocType::Text, "ext: {ext}");
        }
    }

    #[test]
    fn office_extensions_map_to_their_type() {
        assert_eq!(guess(Path::new("a.docx")), DocType::Docx);
        assert_eq!(guess(Path::new("a.xlsx")), DocType::Xlsx);
        assert_eq!(guess(Path::new("a.pptx")), DocType::Pptx);
    }

    #[test]
    fn an_unrecognised_extension_is_unknown() {
        assert_eq!(guess(Path::new("a.xyzzy")), DocType::Unknown);
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        assert_eq!(guess(Path::new("REPORT.PDF")), DocType::Pdf);
        assert_eq!(guess(Path::new("Notes.MD")), DocType::Text);
    }

    #[test]
    fn a_file_with_no_extension_is_unknown() {
        assert_eq!(guess(Path::new("Makefile")), DocType::Unknown);
    }
}

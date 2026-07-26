//! OOXML extraction: docx (full body text), xlsx (cell text per sheet), pptx
//! (slide text), plus `docProps/core.xml` properties (title/author/created)
//! shared by all three. Hand-rolled over `zip` + `quick-xml` rather than a
//! per-format crate — all three share the same zip-of-XML-parts shape, and
//! one shared implementation over that shape is less code and less fuzz
//! surface than three dependencies with incompatible object models.

use std::error::Error;
use std::io::{Cursor, Read};

use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

use crate::Extraction;

/// `whatlang` guesses wildly on short input — a title or a single cell's
/// worth of text isn't enough signal. A few dozen characters of real body
/// text is the point past which its confidence becomes worth storing.
const MIN_CHARS_FOR_LANG_DETECT: usize = 40;

type Archive<'a> = ZipArchive<Cursor<&'a [u8]>>;

pub(crate) fn extract_docx(bytes: &[u8]) -> Result<Extraction, Box<dyn Error>> {
    let mut zip = open_zip(bytes)?;
    let (title, author) = read_core_properties(&mut zip);
    let document = read_entry(&mut zip, "word/document.xml")?;
    let text = extract_paragraph_text(&document);

    Ok(Extraction {
        title,
        author,
        word_count: Some(word_count(&text)),
        language: detect_language(&text),
        // A docx's laid-out page count depends on page size, margins, and
        // font metrics, none of which appear in document.xml — only an
        // actual layout engine can compute it. Leaving it unset is more
        // honest than a word-count-divided-by-a-guessed-constant number.
        page_count: None,
        body_text: Some(crate::cap_body_text(&text)),
        ..Extraction::default()
    })
}

pub(crate) fn extract_xlsx(bytes: &[u8]) -> Result<Extraction, Box<dyn Error>> {
    let mut zip = open_zip(bytes)?;
    let (title, author) = read_core_properties(&mut zip);
    let shared_strings = read_shared_strings(&mut zip);

    let sheet_names = entry_names(&zip, "xl/worksheets/sheet", ".xml");
    if sheet_names.is_empty() {
        return Err("no xl/worksheets/sheet*.xml entries".into());
    }

    // A newline between sheets, same reasoning as between paragraphs: without
    // it the last cell of one sheet and the first of the next fuse into one
    // token.
    let mut text = String::new();
    for name in &sheet_names {
        let sheet = read_entry(&mut zip, name)?;
        text.push_str(&extract_sheet_text(&sheet, &shared_strings));
        text.push('\n');
    }

    Ok(Extraction {
        title,
        author,
        word_count: Some(word_count(&text)),
        language: detect_language(&text),
        // xlsx has no notion of a page independent of print setup (which
        // isn't always present). Sheet count is the closest structural
        // analogue and is what this field means for spreadsheets here.
        page_count: Some(sheet_names.len() as i64),
        body_text: Some(crate::cap_body_text(&text)),
        ..Extraction::default()
    })
}

pub(crate) fn extract_pptx(bytes: &[u8]) -> Result<Extraction, Box<dyn Error>> {
    let mut zip = open_zip(bytes)?;
    let (title, author) = read_core_properties(&mut zip);

    let slide_names = entry_names(&zip, "ppt/slides/slide", ".xml");
    if slide_names.is_empty() {
        return Err("no ppt/slides/slide*.xml entries".into());
    }

    // A newline between slides for the same reason as between sheets above.
    let mut text = String::new();
    for name in &slide_names {
        let slide = read_entry(&mut zip, name)?;
        text.push_str(&extract_paragraph_text(&slide));
        text.push('\n');
    }

    Ok(Extraction {
        title,
        author,
        word_count: Some(word_count(&text)),
        language: detect_language(&text),
        // Unlike docx, a slide *is* the presentation's unit of pagination —
        // one slideN.xml part per slide, no layout engine required.
        page_count: Some(slide_names.len() as i64),
        body_text: Some(crate::cap_body_text(&text)),
        ..Extraction::default()
    })
}

fn open_zip(bytes: &[u8]) -> Result<Archive<'_>, Box<dyn Error>> {
    ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("opening zip: {e}").into())
}

/// Zip entry names matching `{prefix}*{suffix}`, sorted for deterministic
/// concatenation order. Matches e.g. `xl/worksheets/sheet1.xml` while
/// naturally excluding `xl/worksheets/_rels/sheet1.xml.rels` (wrong prefix)
/// without any dedicated rels-filtering logic.
fn entry_names(zip: &Archive<'_>, prefix: &str, suffix: &str) -> Vec<String> {
    let mut names: Vec<String> = zip
        .file_names()
        .filter(|n| n.starts_with(prefix) && n.ends_with(suffix))
        .map(String::from)
        .collect();
    names.sort();
    names
}

/// A missing or unreadable part is a real extraction failure — the caller
/// asked for a docx/xlsx/pptx and the one part that makes it that format
/// isn't there.
fn read_entry(zip: &mut Archive<'_>, name: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut file = zip.by_name(name).map_err(|e| format!("reading {name}: {e}"))?;
    let mut buf = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Same as [`read_entry`] but for parts that are enrichment, not the
/// document's substance (`docProps/core.xml`, `xl/sharedStrings.xml`) — a
/// real-world file missing one of these still deserves a successful
/// extraction, just without the properties it would have supplied.
fn try_read_entry(zip: &mut Archive<'_>, name: &str) -> Option<Vec<u8>> {
    let mut file = zip.by_name(name).ok()?;
    let mut buf = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// `docProps/core.xml`'s `<dc:title>`/`<dc:creator>`, common to all three
/// OOXML formats. Malformed XML here just yields no properties rather than
/// failing the whole extraction — this part is metadata about the document,
/// not the document.
fn read_core_properties(zip: &mut Archive<'_>) -> (Option<String>, Option<String>) {
    let Some(bytes) = try_read_entry(zip, "docProps/core.xml") else {
        return (None, None);
    };

    let mut reader = Reader::from_reader(bytes.as_slice());
    reader.config_mut().trim_text(true);

    let mut title = None;
    let mut author = None;
    let mut current: Option<bool> = None; // Some(true) = title, Some(false) = author

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                current = match e.local_name().as_ref() {
                    b"title" => Some(true),
                    b"creator" => Some(false),
                    _ => None,
                };
            }
            Ok(Event::Text(t)) => {
                if let (Some(want_title), Ok(text)) = (current, t.decode()) {
                    let text = text.into_owned();
                    if !text.is_empty() {
                        if want_title {
                            title.get_or_insert(text);
                        } else {
                            author.get_or_insert(text);
                        }
                    }
                }
            }
            Ok(Event::End(_)) => current = None,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    (title, author)
}

/// `xl/sharedStrings.xml`'s `<si>` table, indexed by position — that
/// position is exactly the `N` a `<c t="s"><v>N</v></c>` cell refers to, so
/// a plain `Vec` (not a `HashMap`) is the right shape for it.
fn read_shared_strings(zip: &mut Archive<'_>) -> Vec<String> {
    let Some(bytes) = try_read_entry(zip, "xl/sharedStrings.xml") else {
        return Vec::new();
    };

    let mut reader = Reader::from_reader(bytes.as_slice());
    reader.config_mut().trim_text(false);

    let mut strings = Vec::new();
    let mut current = String::new();
    let mut in_text = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"si" => current.clear(),
                b"t" => in_text = true,
                _ => {}
            },
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"si" => strings.push(std::mem::take(&mut current)),
                b"t" => in_text = false,
                _ => {}
            },
            Ok(Event::Text(t)) if in_text => {
                if let Ok(s) = t.decode() {
                    current.push_str(&s);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    strings
}

/// Concatenates every `<…:t>` text node, treating each `<…:p>` as a
/// paragraph boundary. docx (`w:t`/`w:p`) and pptx (`a:t`/`a:p`) share this
/// exact shape once namespace prefixes are stripped by `local_name()`, so
/// one reader loop covers both — a run's text is glued to its neighbours
/// (that's how Word/PowerPoint split a word across formatting runs) while
/// paragraphs get a real separator so words from adjacent paragraphs don't
/// fuse into one token.
///
/// A parse error partway through still leaves whatever text was collected
/// before the error — a truncated part's recoverable prefix is more useful
/// than discarding it, and the zip-level read already failed loudly for
/// genuinely missing parts.
fn extract_paragraph_text(xml: &[u8]) -> String {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);

    let mut out = String::new();
    let mut in_text = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"t" => in_text = true,
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"t" => in_text = false,
                b"p" => out.push('\n'),
                _ => {}
            },
            Ok(Event::Text(t)) if in_text => {
                if let Ok(s) = t.decode() {
                    out.push_str(&s);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    out
}

/// A worksheet's cell text: inline strings (`<c t="inlineStr"><is><t>`)
/// contribute their `<t>` directly; shared-string cells (`<c t="s">`) hold
/// only a numeric index in `<v>`, resolved against `shared`. Numeric and
/// other cell types are skipped deliberately — a spreadsheet's numbers
/// aren't "body text" in the sense `word_count`/`language` care about.
fn extract_sheet_text(xml: &[u8], shared: &[String]) -> String {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);

    let mut out = String::new();
    let mut cell_is_shared = false;
    let mut in_inline_text = false;
    let mut in_shared_value = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"c" => {
                    cell_is_shared = e
                        .try_get_attribute("t")
                        .ok()
                        .flatten()
                        .is_some_and(|a| a.value.as_ref() == b"s".as_slice());
                }
                b"t" => in_inline_text = true,
                b"v" => in_shared_value = cell_is_shared,
                _ => {}
            },
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"t" => {
                    in_inline_text = false;
                    out.push(' ');
                }
                b"v" => in_shared_value = false,
                b"row" => out.push('\n'),
                _ => {}
            },
            Ok(Event::Text(t)) => {
                if in_inline_text {
                    if let Ok(s) = t.decode() {
                        out.push_str(&s);
                    }
                } else if in_shared_value
                    && let Ok(s) = t.decode()
                    && let Ok(idx) = s.parse::<usize>()
                    && let Some(resolved) = shared.get(idx)
                {
                    out.push_str(resolved);
                    out.push(' ');
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    out
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

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use super::*;

    /// Builds an in-memory zip from `(entry name, contents)` pairs — the
    /// simplest way to get a real docx/xlsx/pptx-shaped archive for tests
    /// without committing binary fixtures for parts our reader code never
    /// looks at.
    fn build_zip(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(&mut cursor);
        let options = SimpleFileOptions::default();
        for (name, contents) in parts {
            zip.start_file(*name, options).expect("start_file");
            zip.write_all(contents.as_bytes()).expect("write_all");
        }
        zip.finish().expect("finish");
        cursor.into_inner()
    }

    const CORE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
                    xmlns:dc="http://purl.org/dc/elements/1.1/">
  <dc:title>Quarterly Report</dc:title>
  <dc:creator>Ada Lovelace</dc:creator>
</cp:coreProperties>"#;

    #[test]
    fn docx_extracts_title_author_and_word_count() {
        let document_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:r><w:t>Hello there</w:t></w:r></w:p>
    <w:p><w:r><w:t>General Kenobi</w:t></w:r></w:p>
  </w:body>
</w:document>"#;
        let bytes =
            build_zip(&[("docProps/core.xml", CORE_XML), ("word/document.xml", document_xml)]);

        let extraction = extract_docx(&bytes).expect("extract_docx");
        assert_eq!(extraction.title.as_deref(), Some("Quarterly Report"));
        assert_eq!(extraction.author.as_deref(), Some("Ada Lovelace"));
        assert_eq!(extraction.word_count, Some(4));
        assert_eq!(extraction.page_count, None);
    }

    #[test]
    fn docx_paragraphs_do_not_glue_words_together() {
        let document_xml = r#"<w:document xmlns:w="x">
  <w:body>
    <w:p><w:r><w:t>first</w:t></w:r></w:p>
    <w:p><w:r><w:t>second</w:t></w:r></w:p>
  </w:body>
</w:document>"#;
        let bytes = build_zip(&[("word/document.xml", document_xml)]);
        let extraction = extract_docx(&bytes).expect("extract_docx");
        // Two distinct tokens, not "firstsecond" glued into one.
        assert_eq!(extraction.word_count, Some(2));
    }

    #[test]
    fn xlsx_resolves_shared_string_references() {
        let shared_strings_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="2" uniqueCount="2">
  <si><t>Hello</t></si>
  <si><t>World</t></si>
</sst>"#;
        let sheet_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" t="s"><v>0</v></c>
      <c r="B1" t="s"><v>1</v></c>
      <c r="C1"><v>42</v></c>
    </row>
  </sheetData>
</worksheet>"#;
        let bytes = build_zip(&[
            ("docProps/core.xml", CORE_XML),
            ("xl/sharedStrings.xml", shared_strings_xml),
            ("xl/worksheets/sheet1.xml", sheet_xml),
        ]);

        let extraction = extract_xlsx(&bytes).expect("extract_xlsx");
        assert_eq!(extraction.word_count, Some(2));
        assert_eq!(extraction.page_count, Some(1));
    }

    #[test]
    fn xlsx_reads_inline_strings_without_shared_string_table() {
        let sheet_xml = r#"<worksheet xmlns="x">
  <sheetData>
    <row r="1">
      <c r="A1" t="inlineStr"><is><t>Inline text</t></is></c>
    </row>
  </sheetData>
</worksheet>"#;
        let bytes = build_zip(&[("xl/worksheets/sheet1.xml", sheet_xml)]);
        let extraction = extract_xlsx(&bytes).expect("extract_xlsx");
        assert_eq!(extraction.word_count, Some(2));
    }

    #[test]
    fn xlsx_counts_multiple_sheets_as_page_count() {
        let sheet_xml = r#"<worksheet xmlns="x"><sheetData></sheetData></worksheet>"#;
        let bytes = build_zip(&[
            ("xl/worksheets/sheet1.xml", sheet_xml),
            ("xl/worksheets/sheet2.xml", sheet_xml),
            ("xl/worksheets/_rels/sheet1.xml.rels", "<Relationships/>"),
        ]);
        let extraction = extract_xlsx(&bytes).expect("extract_xlsx");
        assert_eq!(extraction.page_count, Some(2));
    }

    #[test]
    fn pptx_slide_count_is_the_page_count() {
        let slide_xml = r#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <a:t>Title slide</a:t>
</p:sld>"#;
        let bytes = build_zip(&[
            ("docProps/core.xml", CORE_XML),
            ("ppt/slides/slide1.xml", slide_xml),
            ("ppt/slides/slide2.xml", slide_xml),
        ]);

        let extraction = extract_pptx(&bytes).expect("extract_pptx");
        assert_eq!(extraction.page_count, Some(2));
        assert_eq!(extraction.title.as_deref(), Some("Quarterly Report"));
        assert_eq!(extraction.word_count, Some(4)); // "Title slide" x2 slides
    }

    #[test]
    fn short_text_does_not_get_a_language_guess() {
        let document_xml = r#"<w:document xmlns:w="x"><w:body><w:p><w:r><w:t>hi</w:t></w:r></w:p></w:body></w:document>"#;
        let bytes = build_zip(&[("word/document.xml", document_xml)]);
        let extraction = extract_docx(&bytes).expect("extract_docx");
        assert_eq!(extraction.language, None);
    }

    #[test]
    fn long_english_text_is_detected_as_english() {
        let document_xml = r#"<w:document xmlns:w="x"><w:body><w:p><w:r><w:t>
            The quick brown fox jumps over the lazy dog near the riverbank
            every single morning before the sun rises above the hills.
        </w:t></w:r></w:p></w:body></w:document>"#;
        let bytes = build_zip(&[("word/document.xml", document_xml)]);
        let extraction = extract_docx(&bytes).expect("extract_docx");
        assert_eq!(extraction.language.as_deref(), Some("eng"));
    }

    #[test]
    fn missing_core_properties_still_extracts_body_text() {
        let document_xml = r#"<w:document xmlns:w="x"><w:body><w:p><w:r><w:t>no properties here</w:t></w:r></w:p></w:body></w:document>"#;
        let bytes = build_zip(&[("word/document.xml", document_xml)]);
        let extraction = extract_docx(&bytes).expect("extract_docx");
        assert_eq!(extraction.title, None);
        assert_eq!(extraction.author, None);
        assert_eq!(extraction.word_count, Some(3));
    }

    #[test]
    fn missing_content_part_is_an_error_not_a_default() {
        let bytes = build_zip(&[("docProps/core.xml", CORE_XML)]);
        assert!(extract_docx(&bytes).is_err());
        assert!(extract_xlsx(&bytes).is_err());
        assert!(extract_pptx(&bytes).is_err());
    }

    #[test]
    fn corrupt_zip_returns_err_not_panic() {
        let garbage = b"this is not a zip file at all".to_vec();
        assert!(extract_docx(&garbage).is_err());
        assert!(extract_xlsx(&garbage).is_err());
        assert!(extract_pptx(&garbage).is_err());
    }

    #[test]
    fn truncated_zip_returns_err_not_panic() {
        let mut bytes =
            build_zip(&[("docProps/core.xml", CORE_XML), ("word/document.xml", "<w:document/>")]);
        bytes.truncate(bytes.len() / 2);
        assert!(extract_docx(&bytes).is_err());
    }
}

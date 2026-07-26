//! EXIF extraction for JPEG/TIFF images: capture time and camera model.
//!
//! No date/time crate dependency: EXIF's `DateTimeOriginal`/`DateTime` tags
//! are plain "YYYY:MM:DD HH:MM:SS" calendar fields with no timezone, so
//! converting one to a unix timestamp only needs a (year, month, day) day
//! count, which `civil_to_unix` below computes directly rather than pulling
//! in `chrono` for one calculation (the rest of the workspace makes the same
//! call — see `dafs_store::events::now_unix_ms`'s neighbours).

use exif::{Field, In, Reader, Tag, Value};

use crate::Extraction;

pub(crate) fn extract(bytes: &[u8]) -> Result<Extraction, Box<dyn std::error::Error>> {
    let mut cursor = std::io::Cursor::new(bytes);
    let exif = match Reader::new().read_from_container(&mut cursor) {
        Ok(exif) => exif,
        // No APP1/EXIF segment at all is the ordinary case — a photo
        // stripped of metadata or exported from most web tools — not a
        // parse failure worth surfacing as one.
        Err(exif::Error::NotFound(_)) => return Ok(Extraction::default()),
        Err(e) => return Err(Box::new(e)),
    };

    let image_taken_at_unix = exif
        .get_field(Tag::DateTimeOriginal, In::PRIMARY)
        .or_else(|| exif.get_field(Tag::DateTime, In::PRIMARY))
        .and_then(datetime_field_to_unix);

    let make = ascii_field(&exif, Tag::Make);
    let model = ascii_field(&exif, Tag::Model);

    Ok(Extraction {
        image_taken_at_unix,
        image_camera_model: join_make_model(make, model),
        ..Default::default()
    })
}

fn ascii_field(exif: &exif::Exif, tag: Tag) -> Option<String> {
    match &exif.get_field(tag, In::PRIMARY)?.value {
        Value::Ascii(strings) => {
            let s = String::from_utf8_lossy(strings.first()?).trim().to_string();
            (!s.is_empty()).then_some(s)
        }
        _ => None,
    }
}

/// Some cameras (most Canon/Nikon bodies) set Model to already include the
/// Make, e.g. Make "Canon" / Model "Canon EOS R5" — joining verbatim would
/// read "Canon Canon EOS R5". Common enough in real files to be worth the
/// one-line check; anything less exact (different case, a shortened make)
/// falls through to a plain join rather than trying to be clever about it.
fn join_make_model(make: Option<String>, model: Option<String>) -> Option<String> {
    match (make, model) {
        (Some(make), Some(model)) => {
            if model.to_lowercase().starts_with(&make.to_lowercase()) {
                Some(model)
            } else {
                Some(format!("{make} {model}"))
            }
        }
        (Some(make), None) => Some(make),
        (None, Some(model)) => Some(model),
        (None, None) => None,
    }
}

fn datetime_field_to_unix(field: &Field) -> Option<i64> {
    let Value::Ascii(strings) = &field.value else { return None };
    let dt = exif::DateTime::from_ascii(strings.first()?).ok()?;
    civil_to_unix(dt.year.into(), dt.month.into(), dt.day.into(), dt.hour, dt.minute, dt.second)
}

/// Howard Hinnant's `days_from_civil` algorithm
/// (<http://howardhinnant.github.io/date_algorithms.html#days_from_civil>):
/// converts a proleptic Gregorian (year, month, day) into a day count
/// relative to the Unix epoch without a lookup table, correct for any
/// calendar year `from_ascii` can produce (it does not range-check month or
/// day, so this does).
///
/// EXIF datetimes carry no timezone. Treating the calendar fields as UTC is
/// a known accuracy caveat — a camera set to local time is off by its
/// offset, and there is no way to recover that from the field alone — but
/// it is the only reading that does not require guessing a timezone.
fn civil_to_unix(year: i64, month: i64, day: i64, hour: u8, minute: u8, second: u8) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (month + 9) % 12; // [0, 11], Mar = 0 .. Feb = 11
    let doy = (153 * mp + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    let days_since_epoch = era * 146_097 + doe - 719_468;

    Some(
        days_since_epoch * 86_400
            + i64::from(hour) * 3600
            + i64::from(minute) * 60
            + i64::from(second),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TYPE_ASCII: u16 = 2;
    const TYPE_LONG: u16 = 4;

    /// Builds a minimal little-endian TIFF/EXIF blob with one primary IFD
    /// holding the given (tag, type, count, raw value bytes) entries —
    /// enough for `kamadak-exif` to parse directly via `read_from_container`
    /// without needing a real JPEG wrapper.
    fn build_tiff(entries: &[(u16, u16, u32, Vec<u8>)]) -> Vec<u8> {
        let ifd_offset: u32 = 8;
        let ifd_size = 2 + entries.len() * 12 + 4;
        let mut data_offset = 8 + ifd_size as u32;

        let mut ifd = Vec::new();
        ifd.extend_from_slice(&(entries.len() as u16).to_le_bytes());

        let mut data_area = Vec::new();
        for (tag, typ, count, raw) in entries {
            ifd.extend_from_slice(&tag.to_le_bytes());
            ifd.extend_from_slice(&typ.to_le_bytes());
            ifd.extend_from_slice(&count.to_le_bytes());
            if raw.len() <= 4 {
                let mut inline = raw.clone();
                inline.resize(4, 0);
                ifd.extend_from_slice(&inline);
            } else {
                ifd.extend_from_slice(&data_offset.to_le_bytes());
                let mut padded = raw.clone();
                if padded.len() % 2 != 0 {
                    padded.push(0);
                }
                data_offset += padded.len() as u32;
                data_area.extend_from_slice(&padded);
            }
        }
        ifd.extend_from_slice(&0u32.to_le_bytes()); // no next IFD

        let mut out = Vec::new();
        out.extend_from_slice(b"II");
        out.extend_from_slice(&42u16.to_le_bytes());
        out.extend_from_slice(&ifd_offset.to_le_bytes());
        out.extend_from_slice(&ifd);
        out.extend_from_slice(&data_area);
        out
    }

    fn ascii_entry(tag: u16, s: &str) -> (u16, u16, u32, Vec<u8>) {
        let mut raw = s.as_bytes().to_vec();
        raw.push(0);
        (tag, TYPE_ASCII, raw.len() as u32, raw)
    }

    const TAG_MAKE: u16 = 0x10f;
    const TAG_MODEL: u16 = 0x110;
    const TAG_DATETIME: u16 = 0x132;
    const TAG_EXIF_IFD_POINTER: u16 = 0x8769;
    const TAG_DATETIME_ORIGINAL: u16 = 0x9003;

    #[test]
    fn valid_exif_populates_taken_at_and_camera_model() {
        let bytes = build_tiff(&[
            ascii_entry(TAG_MAKE, "Nikon"),
            ascii_entry(TAG_MODEL, "D850"),
            ascii_entry(TAG_DATETIME, "2023:06:15 10:30:00"),
        ]);

        let extraction = extract(&bytes).expect("extract");
        assert_eq!(extraction.image_camera_model.as_deref(), Some("Nikon D850"));
        // 2023-06-15T10:30:00Z, computed independently via `date -u -d`.
        assert_eq!(extraction.image_taken_at_unix, Some(1_686_825_000));
    }

    #[test]
    fn date_time_original_is_preferred_over_date_time() {
        let mut entries = vec![
            ascii_entry(TAG_DATETIME, "2000:01:01 00:00:00"),
            (TAG_EXIF_IFD_POINTER, TYPE_LONG, 1, Vec::new()),
        ];
        // The sub-IFD pointer's value is fixed up below, once we know where
        // the sub-IFD will land in the buffer.
        let placeholder_index = entries.len() - 1;

        let primary = build_tiff(&entries);
        let sub_ifd_offset = primary.len() as u32;

        let sub_entries = [ascii_entry(TAG_DATETIME_ORIGINAL, "2023:06:15 10:30:00")];
        let mut sub_ifd = Vec::new();
        sub_ifd.extend_from_slice(&(sub_entries.len() as u16).to_le_bytes());
        let mut sub_data = Vec::new();
        let mut sub_data_offset = sub_ifd_offset + 2 + sub_entries.len() as u32 * 12 + 4;
        for (tag, typ, count, raw) in &sub_entries {
            sub_ifd.extend_from_slice(&tag.to_le_bytes());
            sub_ifd.extend_from_slice(&typ.to_le_bytes());
            sub_ifd.extend_from_slice(&count.to_le_bytes());
            let mut padded = raw.clone();
            if padded.len() % 2 != 0 {
                padded.push(0);
            }
            sub_ifd.extend_from_slice(&sub_data_offset.to_le_bytes());
            sub_data_offset += padded.len() as u32;
            sub_data.extend_from_slice(&padded);
        }
        sub_ifd.extend_from_slice(&0u32.to_le_bytes());

        entries[placeholder_index].3 = sub_ifd_offset.to_le_bytes().to_vec();
        let mut bytes = build_tiff(&entries);
        bytes.extend_from_slice(&sub_ifd);
        bytes.extend_from_slice(&sub_data);

        let extraction = extract(&bytes).expect("extract");
        assert_eq!(extraction.image_taken_at_unix, Some(1_686_825_000));
    }

    #[test]
    fn model_already_prefixed_with_make_is_not_duplicated() {
        let bytes =
            build_tiff(&[ascii_entry(TAG_MAKE, "Canon"), ascii_entry(TAG_MODEL, "Canon EOS R5")]);

        let extraction = extract(&bytes).expect("extract");
        assert_eq!(extraction.image_camera_model.as_deref(), Some("Canon EOS R5"));
    }

    #[test]
    fn no_exif_segment_extracts_cleanly_with_no_fields() {
        // Bare JPEG SOI+EOI, no APP1 segment at all — the ordinary case for
        // a photo with metadata stripped.
        let bytes = b"\xff\xd8\xff\xd9";

        let extraction = extract(bytes).expect("extract");
        assert_eq!(extraction, Extraction::default());
    }

    #[test]
    fn truncated_garbage_does_not_panic() {
        for bytes in [&b""[..], &b"II"[..], &b"II*\0"[..], &b"not an image at all"[..]] {
            // Either outcome is acceptable for unparseable input — the only
            // thing under test is that neither panics.
            let _ = extract(bytes);
        }
    }
}

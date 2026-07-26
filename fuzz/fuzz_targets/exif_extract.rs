//! Fuzz EXIF extraction.
//!
//! # Why this target exists
//!
//! EXIF is a binary tag/offset structure read straight out of whatever camera
//! or tool produced the file — attacker-controlled offsets pointing into
//! attacker-controlled data, the textbook shape for an out-of-bounds read.
//! The testing bar (`docs/roadmap-and-design-review.md` §5.2 item 3) names
//! EXIF explicitly as one of the M02 extractors that needs a fuzz target for
//! this reason.
//!
//! # The property asserted
//!
//! No panic, no hang. `exif::extract` returns `Result`, and any `Err` is a
//! fine outcome for garbage input — the crate's own unit tests already
//! assert this for a handful of truncated/empty cases
//! (`truncated_garbage_does_not_panic`); this target is the same property
//! under continuous random mutation instead of a fixed list. Only a panic or
//! an infinite loop is a bug.
//!
//! # Why a JPEG magic prefix
//!
//! `dafs_extract::extract`'s sniffing only recognises Jpeg/Tiff by content
//! (`infer`'s magic-byte check), unlike docx/xlsx/pptx which sniff falls back
//! to recognising by extension alone. Fuzzed bytes essentially never contain
//! `FF D8 FF` by chance, so without a fixed prefix this target would almost
//! always take the `DocType::Unknown` short-circuit in `extract` and never
//! reach `exif::extract` at all. Prepending the three-byte JPEG SOI marker
//! and letting the rest of the fuzzed bytes stand in for everything after it
//! (segments, the APP1/Exif block, the TIFF structure inside) mirrors a real
//! hostile file: a valid outer container with a malformed inside, same as the
//! docx/xlsx/pptx targets relying on a real `.docx`-shaped extension while
//! fuzzing what's inside the zip.
//!
//! # Why through `extract`, not the parser directly
//!
//! `exif::extract` is `pub(crate)`, an internal seam rather than a public
//! API. The bytes go to a real tempfile named `fuzz.jpg` and through the
//! crate's actual public entry point, `dafs_extract::extract` — exercising
//! sniffing and the `catch_unwind` wrapper together with the parser, the same
//! path a hostile photo walks in production.
#![no_main]

use libfuzzer_sys::fuzz_target;

/// JPEG SOI + start of the next marker (`FF D8 FF`) — enough for `infer` to
/// classify the file as `image/jpeg` so `extract` dispatches to
/// `exif::extract` instead of stopping at `DocType::Unknown`. Everything
/// after these three bytes, including whatever would normally be the rest of
/// the SOS/APP1 markers, is entirely fuzzed.
const JPEG_MAGIC: [u8; 3] = [0xFF, 0xD8, 0xFF];

fuzz_target!(|data: &[u8]| {
    let Ok(dir) = tempfile::Builder::new().prefix("dafs-fuzz").tempdir() else {
        return;
    };
    let path = dir.path().join("fuzz.jpg");

    let mut bytes = Vec::with_capacity(JPEG_MAGIC.len() + data.len());
    bytes.extend_from_slice(&JPEG_MAGIC);
    bytes.extend_from_slice(data);

    if std::fs::write(&path, &bytes).is_err() {
        return;
    }

    // The contract: extraction succeeds or returns Err. A panic or hang is a
    // bug. The result is deliberately discarded — absence of a crash is the
    // assertion.
    let _ = dafs_extract::extract(&path);
});

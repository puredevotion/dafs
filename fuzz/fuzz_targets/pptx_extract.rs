//! Fuzz pptx extraction.
//!
//! # Why this target exists
//!
//! A `.pptx` is a zip of attacker-controlled XML parts, same as docx/xlsx —
//! the testing bar (`docs/roadmap-and-design-review.md` §5.2 item 3) requires
//! a fuzz target for exactly this shape of input. `office::extract_pptx`
//! unzips `ppt/slides/slide*.xml` and walks its text runs with nothing
//! sanitising the archive contents first.
//!
//! # The property asserted
//!
//! No panic, no hang. `extract_pptx` returns `Result`, and any `Err` is a
//! fine outcome for garbage input — a zip that isn't one, or one with no
//! `slide*.xml` entries, is the ordinary case for fuzzed input, not a bug.
//! Only a panic or an infinite loop is one.
//!
//! # Why through `extract`, not the parser directly
//!
//! `office::extract_pptx` is `pub(crate)`, an internal seam rather than a
//! public API. The fuzzed bytes go to a real tempfile named `fuzz.pptx` and
//! through the crate's actual public entry point, `dafs_extract::extract`,
//! the same function `dafs-daemon` calls on every queued file — exercising
//! sniffing's extension-fallback dispatch and the `catch_unwind` wrapper
//! together with the parser, the same path a hostile file walks in
//! production.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(dir) = tempfile::Builder::new().prefix("dafs-fuzz").tempdir() else {
        return;
    };
    let path = dir.path().join("fuzz.pptx");
    if std::fs::write(&path, data).is_err() {
        return;
    }

    // The contract: extraction succeeds or returns Err. A panic or hang is a
    // bug. The result is deliberately discarded — absence of a crash is the
    // assertion.
    let _ = dafs_extract::extract(&path);
});

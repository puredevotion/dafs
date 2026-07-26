//! Fuzz docx extraction.
//!
//! # Why this target exists
//!
//! A `.docx` is a zip of attacker-controlled XML parts — precisely the shape
//! the testing bar (`docs/roadmap-and-design-review.md` §5.2 item 3) names
//! directly: "a fuzz target for every parser that touches bytes the user did
//! not type... PDF/Office/EXIF extractors (M02 — attacker-supplied
//! documents)." `office::extract_docx` unzips and walks that XML with
//! nothing sanitising it upstream, so it is exactly the code a hostile file
//! reaches.
//!
//! # The property asserted
//!
//! No panic, no hang. `extract_docx` returns `Result`, and any `Err` is a
//! fine outcome for garbage input — a docx-shaped pile of bytes that fails to
//! unzip, or unzips but has no `word/document.xml`, is the ordinary case for
//! fuzzed input, not a bug. Only a panic or an infinite loop is one.
//!
//! # Why through `extract`, not the parser directly
//!
//! `office::extract_docx` is `pub(crate)` — an internal seam, not something
//! meant to be called from outside this crate. The fuzzed bytes go to a real
//! tempfile named `fuzz.docx` and through the crate's actual public entry
//! point, `dafs_extract::extract`, the same function `dafs-daemon` calls on
//! every queued file. That exercises sniffing's extension-fallback dispatch
//! and the `catch_unwind` wrapper together with the parser — the same path a
//! hostile file walks in production, not the parser in isolation.
//!
//! A tempfile per iteration costs a syscall or two; fine at this milestone's
//! throughput, not worth an in-memory seam to avoid.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(dir) = tempfile::Builder::new().prefix("dafs-fuzz").tempdir() else {
        return;
    };
    let path = dir.path().join("fuzz.docx");
    if std::fs::write(&path, data).is_err() {
        return;
    }

    // The contract: extraction succeeds or returns Err. A panic or hang is a
    // bug. The result is deliberately discarded — absence of a crash is the
    // assertion.
    let _ = dafs_extract::extract(&path);
});

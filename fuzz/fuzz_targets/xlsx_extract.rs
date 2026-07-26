//! Fuzz xlsx extraction.
//!
//! # Why this target exists
//!
//! An `.xlsx` is a zip of attacker-controlled XML parts — the same
//! attacker-shaped input the testing bar (`docs/roadmap-and-design-review.md`
//! §5.2 item 3) requires a fuzz target for. `office::extract_xlsx` unzips
//! `xl/worksheets/sheet*.xml` and resolves cell references against
//! `xl/sharedStrings.xml`, both of which come straight from the untrusted
//! archive with nothing sanitising them first.
//!
//! # The property asserted
//!
//! No panic, no hang. `extract_xlsx` returns `Result`, and any `Err` is a
//! fine outcome for garbage input — a zip that isn't really one, or one with
//! no `sheet*.xml` entries at all, is the ordinary case for fuzzed input, not
//! a bug. Only a panic or an infinite loop is one. The shared-string index
//! lookup (`shared.get(idx)` on an attacker-chosen `idx`) is a natural place
//! for an off-by-one to hide, which is exactly the kind of thing this target
//! is for.
//!
//! # Why through `extract`, not the parser directly
//!
//! `office::extract_xlsx` is `pub(crate)`, an internal seam rather than a
//! public API. The fuzzed bytes go to a real tempfile named `fuzz.xlsx` and
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
    let path = dir.path().join("fuzz.xlsx");
    if std::fs::write(&path, data).is_err() {
        return;
    }

    // The contract: extraction succeeds or returns Err. A panic or hang is a
    // bug. The result is deliberately discarded — absence of a crash is the
    // assertion.
    let _ = dafs_extract::extract(&path);
});

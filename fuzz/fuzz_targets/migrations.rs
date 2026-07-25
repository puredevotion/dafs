//! Fuzz the metadata store against arbitrary SQL-adjacent input.
//!
//! # What this actually tests
//!
//! Not "can SQLite be broken" — that is upstream's problem and it is far better
//! fuzzed than anything here would manage. This target covers the store's own
//! behaviour when a database file is not what it expects: garbage content, a
//! truncated header, a valid SQLite file with a hostile `schema_migrations`
//! table. Those are reachable in practice — a user's data directory can be
//! corrupted by a full disk, a bad USB drive, or a partially-restored backup —
//! and the requirement is that `open()` returns an error rather than panicking
//! or looping.
//!
//! This is the M00 seed of the testing bar's fuzz requirement. M02's document
//! extractors and M06's network metadata parsers are the higher-severity
//! targets; they get their own as those milestones land.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(dir) = tempfile::Builder::new().prefix("dafs-fuzz").tempdir() else {
        return;
    };
    let path = dir.path().join("metadata.sqlite");
    if std::fs::write(&path, data).is_err() {
        return;
    }

    // The contract: any input either opens successfully (having migrated) or
    // returns Err. A panic, abort, or hang is a bug. The result is deliberately
    // discarded — the assertion is the absence of a crash.
    let _ = dafs_store::open(&path);
});

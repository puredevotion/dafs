//! The scan's memory invariant.
//!
//! `docs/memory-budget.md` sets a 128 MiB peak during an initial 1M-file scan.
//! Generating a million files on a CI runner costs minutes of wall clock and
//! gigabytes of inodes, for a number that would be stale the moment the corpus
//! shape changed. So this asserts something both cheaper and stronger:
//!
//! **anonymous memory does not grow with corpus size.**
//!
//! That is the actual design property. The scan streams and batches, so its
//! footprint is one batch regardless of how many files exist. A scan that
//! accumulated per-file state would pass a single-size ceiling check on a small
//! corpus and fail on a real one; it cannot pass this.
//!
//! # Why anonymous rather than total RSS
//!
//! The store maps up to 256 MiB of the database (`mmap_size`, see
//! `dafs_store::tune`), and those mapped pages appear in total RSS and grow with
//! the database. They are *file-backed and evictable*: the kernel reclaims them
//! under pressure and they cannot cause an OOM. Counting them against a memory
//! ceiling measures the page cache rather than the daemon, and would make the
//! test fail for a design decision `docs/memory-budget.md` §8.3 deliberately
//! made — it trades anonymous pages for mapped ones on purpose.
//!
//! Anonymous RSS is the number that decides whether the process survives memory
//! pressure, so it is the number asserted here. The total is reported alongside
//! it, because a reader looking at a failure wants both.
//!
//! # Why the corpora are large
//!
//! SQLite's page cache is anonymous memory and fills gradually as the database
//! grows, so below ~50k files anonymous RSS rises with corpus size for a reason
//! that is not accumulation — a bounded cache approaching its bound. Both
//! measured sizes therefore sit above that plateau. See [`SMALL`].
//!
//! # Why a subprocess per measurement
//!
//! Two scans in one process cannot be compared: the second reads the first's
//! allocator high-water mark rather than its own cost, so the small corpus
//! appears to use nothing and any ratio is meaningless. An earlier version of
//! this test did exactly that and reported a 40x growth that was pure
//! measurement artifact — while a real linear growth of ~430 bytes per file was
//! sitting underneath it. Each size therefore runs in a fresh
//! `examples/scan_probe` process.
//!
//! The full 1M-file measurement stays a deliberate, occasional exercise rather
//! than a per-PR gate. Nothing here silently substitutes for it: the growth
//! assertion is what generalises, and this comment is the record that the larger
//! run is a separate activity.

use std::path::PathBuf;
use std::process::Command;

/// Corpus sizes, chosen to sit **above** the point where SQLite's page cache
/// has filled.
///
/// This matters and is not a tuning knob. The store sets `cache_size` to 8 MiB
/// (`dafs_store::tune`), and that cache fills gradually as the database grows.
/// Below roughly 50k files it is still filling, so anonymous memory rises
/// near-linearly with corpus size for a reason that has nothing to do with the
/// scan retaining anything — it is a fixed-size cache approaching its bound.
///
/// Measured on this design: 32k files → 7.0 MiB, 64k → 9.8 MiB, 128k → 9.8 MiB.
/// Flat from 64k onward. Comparing 2k against 16k, as an earlier version of this
/// test did, measures two points on that ramp and reports ~6x growth for a
/// perfectly well-behaved scan.
///
/// So both sizes are past the plateau, where any remaining growth really is
/// per-file accumulation.
const SMALL: usize = 64_000;
const LARGE: usize = 128_000;

/// Allowed growth in anonymous memory between the two corpora.
///
/// Both sizes are past the page-cache plateau (see above), so the honest
/// expectation is ~1.0x. The margin absorbs allocator slack and runner noise
/// without being loose enough to hide a real regression: per-file accumulation
/// of even 100 bytes would add ~6 MiB between these two sizes, well outside it.
const MAX_GROWTH_RATIO: f64 = 1.4;

/// One probe run's numbers, all in bytes.
struct Probe {
    /// Anonymous RSS attributable to the scan.
    anon: u64,
    /// Total RSS at the end of the scan, including mapped database pages.
    total: u64,
}

/// Run `examples/scan_probe` for one corpus size in a fresh process.
fn probe(count: usize) -> Option<Probe> {
    let exe = probe_binary()?;

    let output = Command::new(&exe)
        .arg(count.to_string())
        .output()
        .unwrap_or_else(|e| panic!("running {}: {e}", exe.display()));

    assert!(
        output.status.success(),
        "scan_probe {count} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<&str> = stdout.split_whitespace().collect();

    let get = |key: &str| -> u64 {
        fields
            .iter()
            .find_map(|f| f.strip_prefix(key)?.parse().ok())
            .unwrap_or_else(|| panic!("probe output missing {key}: {stdout:?}"))
    };

    Some(Probe { anon: get("anon="), total: get("total=") })
}

/// Locate the probe binary next to the test binary.
///
/// Built by the same `cargo test` invocation via the `required-features`-free
/// example target, but cargo does not hand tests a path to it, so it is found
/// relative to the current executable.
fn probe_binary() -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    // target/<profile>/deps/scan_memory-<hash> -> target/<profile>/examples/
    let profile_dir = current.parent()?.parent()?;
    let candidate = profile_dir.join("examples").join("scan_probe");
    candidate.exists().then_some(candidate)
}

fn skip(reason: &str) {
    eprintln!("SKIP: {reason}");
}

#[test]
fn scan_memory_does_not_grow_with_corpus_size() {
    if !cfg!(target_os = "linux") {
        skip("RSS measurement is linux-only");
        return;
    }

    let (Some(large), Some(small)) = (probe(LARGE), probe(SMALL)) else {
        skip("scan_probe example not built; run `cargo build --examples -p dafs-scan`");
        return;
    };

    let ratio = large.anon as f64 / small.anon.max(1) as f64;
    let corpus_ratio = LARGE as f64 / SMALL as f64;

    eprintln!(
        "anonymous memory: {SMALL} files -> {:.2} MiB, {LARGE} files -> {:.2} MiB \
         (growth {ratio:.2}x for a {corpus_ratio:.0}x corpus)",
        small.anon as f64 / 1_048_576.0,
        large.anon as f64 / 1_048_576.0,
    );
    eprintln!(
        "total RSS incl. mapped pages: {:.2} MiB / {:.2} MiB",
        small.total as f64 / 1_048_576.0,
        large.total as f64 / 1_048_576.0,
    );

    assert!(
        ratio <= MAX_GROWTH_RATIO,
        "anonymous memory grew {ratio:.2}x for a {corpus_ratio:.0}x larger corpus, above the \
         {MAX_GROWTH_RATIO}x bound. The scan is accumulating per-file state — check that nothing \
         collects paths or events instead of streaming them in batches, and that no cache is \
         keyed by something as unique as a filename. See docs/memory-budget.md §M01."
    );
}

#[test]
fn scan_peak_is_within_the_documented_ceiling() {
    if !cfg!(target_os = "linux") {
        skip("RSS measurement is linux-only");
        return;
    }

    // The ceiling from docs/memory-budget.md. Duplicated from
    // dafs-memtest::ceilings rather than depended on, to keep this crate's test
    // graph free of the memtest harness; the doc is the shared source of truth.
    const SCAN_PEAK: u64 = 128 * 1024 * 1024;

    let Some(measured) = probe(LARGE) else {
        skip("scan_probe example not built; run `cargo build --examples -p dafs-scan`");
        return;
    };

    eprintln!(
        "scan over {LARGE} files: {:.2} MiB total, {:.2} MiB anonymous (ceiling {:.0} MiB)",
        measured.total as f64 / 1_048_576.0,
        measured.anon as f64 / 1_048_576.0,
        SCAN_PEAK as f64 / 1_048_576.0
    );

    assert!(
        measured.total <= SCAN_PEAK,
        "scan peak {} bytes exceeds the {SCAN_PEAK}-byte ceiling in docs/memory-budget.md. \
         Either the regression is real, or the budget needs an explicit, documented revision — \
         do not raise the constant to make this pass.",
        measured.total
    );
}

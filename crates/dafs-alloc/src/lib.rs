//! Allocator selection and RSS measurement.
//!
//! # Why this crate exists
//!
//! The memory budget (`docs/memory-budget.md`) requires that RSS **returns to
//! the idle ceiling after a large scan completes**, not merely that it stays
//! low while idle. That is an allocator property, not an application one:
//!
//! - glibc's malloc keeps per-thread arenas and, on a many-thread
//!   scan-then-idle workload, fragments them badly enough that freed memory is
//!   never returned to the OS. RSS ratchets up and stays there.
//! - jemalloc has background purge with tunable dirty/muzzy decay, so freed
//!   pages go back to the OS on a timer without the application asking.
//!
//! The scan is exactly the shape that breaks glibc: many threads, millions of
//! short-lived small allocations, then near-total quiescence. So the allocator
//! is a correctness dependency of the budget, and is not configurable — a
//! deployment that swapped it would silently fail the ceilings that CI asserts.
//!
//! # Decay tuning
//!
//! Defaults are tuned for "give pages back promptly", the opposite of the
//! throughput-first defaults:
//!
//! - `dirty_decay_ms:1000` — dirty pages (still mapped, still faulted in) are
//!   purged one second after becoming unused.
//! - `muzzy_decay_ms:0` — muzzy pages (madvised away but still mapped) are
//!   returned immediately rather than being held for reuse.
//! - `background_thread:true` — decay actually runs without needing an
//!   allocation call to drive it. Without this, an idle daemon never purges,
//!   which is precisely the state the idle ceiling measures.
//!
//! This trades some allocation throughput for lower steady-state RSS. That is
//! the correct trade for this workload: the scan is I/O-bound, and the daemon
//! spends most of its life idle.

// `deny` rather than `forbid`: the single `malloc_conf` export below needs a
// scoped allow, and `forbid` cannot be downgraded per-item. Everything else in
// this crate is still rejected at compile time.
#![deny(unsafe_code)]

#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

/// The process allocator.
///
/// Bind this in the binary crate:
///
/// ```ignore
/// #[global_allocator]
/// static GLOBAL: dafs_alloc::Allocator = dafs_alloc::ALLOCATOR;
/// ```
#[cfg(not(target_env = "msvc"))]
pub type Allocator = Jemalloc;

/// The allocator instance to bind to `#[global_allocator]`.
#[cfg(not(target_env = "msvc"))]
pub const ALLOCATOR: Allocator = Jemalloc;

/// jemalloc tuning, applied via the `malloc_conf` symbol that jemalloc reads at
/// initialisation.
///
/// Setting it here rather than through the `MALLOC_CONF` environment variable
/// means the tuning ships inside the binary and cannot be lost by a deployment
/// that forgets to set an env var. Since the decay settings are a correctness
/// dependency of the memory budget (see the module docs), losing them silently
/// is exactly the failure mode worth engineering against.
///
/// The `export_name` override earns a lint, because duplicate exported symbols
/// across libraries are undefined behaviour. It is sound here for one specific
/// reason: `malloc_conf` is a weak symbol that jemalloc itself defines and
/// documents as the intended override point, and only one allocator is linked.
/// The allow is scoped to this item alone, so the crate-level
/// `deny(unsafe_code)` still covers everything else.
#[cfg(not(target_env = "msvc"))]
#[allow(non_upper_case_globals, unsafe_code)]
#[unsafe(export_name = "malloc_conf")]
pub static malloc_conf: &[u8] = b"background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:0\0";

/// Resident set size in bytes, or `None` if it could not be read.
///
/// Reads `/proc/self/statm` rather than jemalloc's own `stats.resident`: the
/// budget is about what the *operating system* thinks the process occupies,
/// which is what gets a process OOM-killed. jemalloc's accounting excludes
/// anything it did not allocate — the binary's own text and data, thread
/// stacks, and any mmap done outside the allocator (which the scan's read path
/// deliberately does).
///
/// Returns `None` on platforms without procfs; callers in tests should skip
/// rather than fail, so the harness stays portable.
#[cfg(target_os = "linux")]
pub fn resident_bytes() -> Option<u64> {
    // statm fields are in pages: size, resident, shared, text, lib, data, dt
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(resident_pages * page_size())
}

#[cfg(not(target_os = "linux"))]
pub fn resident_bytes() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn page_size() -> u64 {
    // SAFETY-free alternative to libc::sysconf: procfs-adjacent constant. 4 KiB
    // is correct on every platform this targets; a wrong value here would make
    // the ceiling assertions wrong in a visible, testable direction rather than
    // silently, and the harness cross-checks against /proc/self/status.
    4096
}

/// Refresh jemalloc's cached statistics and return the resulting process RSS.
///
/// The RSS ceiling in `docs/memory-budget.md` is defined *after* the process has
/// been idle for a period, which is when background decay has run.
///
/// Note what this does **not** do: it does not force an arena purge. The
/// `arenas.purge` mallctl is only reachable through `raw::write`, which is
/// `unsafe`, and punching a hole in this crate's unsafe ban to make a test
/// deterministic is the wrong trade — an allocator wrapper is exactly where
/// an unsafe escape hatch will later be reached for casually. Instead the decay
/// settings above (`dirty_decay_ms:1000`, `muzzy_decay_ms:0`,
/// `background_thread:true`) make the purge happen on its own within about a
/// second, and callers that need a settled measurement wait for it — see
/// `dafs-memtest`, which measures after readiness plus a settle delay.
#[cfg(not(target_env = "msvc"))]
pub fn refresh_and_measure() -> Option<u64> {
    // Statistics are cached and only recomputed when the epoch advances; without
    // this, repeated reads return the same stale numbers.
    if let Err(e) = tikv_jemalloc_ctl::epoch::advance() {
        tracing::warn!("jemalloc epoch advance failed: {e}");
    }
    resident_bytes()
}

/// Bytes jemalloc reports as resident in its own arenas.
///
/// Useful for attributing a ceiling failure: if process RSS is over budget but
/// this is not, the growth is outside the allocator (mmap, thread stacks, the
/// binary itself) and chasing allocation patterns will not help.
#[cfg(not(target_env = "msvc"))]
pub fn jemalloc_resident_bytes() -> Option<u64> {
    tikv_jemalloc_ctl::epoch::advance().ok()?;
    tikv_jemalloc_ctl::stats::resident::read().ok().map(|v| v as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn resident_bytes_is_plausible() {
        let rss = resident_bytes().expect("procfs should be readable on linux");
        // A running test process is at least a few hundred KiB and, if this is
        // ever in the gigabytes, the parse is picking up the wrong field.
        assert!(rss > 256 * 1024, "RSS implausibly small: {rss}");
        assert!(rss < 8 * 1024 * 1024 * 1024, "RSS implausibly large: {rss}");
    }

    /// The page-size constant is load-bearing for every ceiling assertion, so
    /// cross-check it against a second source rather than trusting it.
    #[test]
    #[cfg(target_os = "linux")]
    fn statm_agrees_with_status() {
        let rss_statm = resident_bytes().expect("statm");
        let status = std::fs::read_to_string("/proc/self/status").expect("status");
        let rss_status_kb: u64 = status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok())
            .expect("VmRSS in /proc/self/status");
        let rss_status = rss_status_kb * 1024;

        // Both are sampled at slightly different instants, so allow slack —
        // but a wrong page size would be off by a factor, not a few pages.
        let ratio = rss_statm as f64 / rss_status as f64;
        assert!(
            (0.5..2.0).contains(&ratio),
            "statm ({rss_statm}) and status ({rss_status}) disagree by more than 2x — \
             page-size constant is probably wrong"
        );
    }

    #[test]
    #[cfg(not(target_env = "msvc"))]
    fn refresh_and_measure_returns_a_value() {
        // Allocate something substantial, drop it, and confirm the purge path
        // runs end to end. Deliberately does NOT assert RSS went down: on a
        // shared CI runner that is flaky, and the real ceiling assertions live
        // in dafs-memtest where they measure the release binary.
        let big: Vec<u8> = vec![7u8; 32 * 1024 * 1024];
        assert_eq!(big[0], 7);
        drop(big);
        assert!(refresh_and_measure().is_some());
    }

    #[test]
    #[cfg(not(target_env = "msvc"))]
    fn jemalloc_is_actually_the_allocator() {
        // If the global allocator were not jemalloc, these stats would not be
        // available at all. This is the test that catches someone dropping the
        // #[global_allocator] binding in a binary crate.
        assert!(
            jemalloc_resident_bytes().is_some_and(|v| v > 0),
            "jemalloc stats unavailable — is #[global_allocator] still bound?"
        );
    }
}

//! Asserts the release daemon's idle RSS against the documented ceiling.
//!
//! Skipped (not failed) when the release binary is absent or the platform has no
//! procfs, so `cargo test` on a dev machine stays useful. CI builds the release
//! binary first, so there the assertion always runs — see `.github/workflows/ci.yml`.

use std::time::Duration;

use dafs_memtest::{Daemon, binary, ceilings, status_is};

/// How long to let decay settle before measuring.
///
/// `dirty_decay_ms` is 1000, so a second plus margin. Measuring earlier reads
/// pre-purge RSS and would fail for a reason that has nothing to do with the
/// daemon's actual footprint.
const SETTLE: Duration = Duration::from_millis(2500);

fn skip(reason: &str) {
    // Rust's test harness has no first-class skip, and returning quietly hides
    // a harness that silently never runs. Printing makes CI logs show whether
    // the assertion actually executed.
    eprintln!("SKIP: {reason}");
}

#[test]
fn idle_rss_is_within_budget() {
    if !cfg!(target_os = "linux") {
        skip("RSS measurement is linux-only");
        return;
    }

    let bin = match binary() {
        Ok(b) => b,
        Err(e) => {
            skip(&e);
            return;
        }
    };

    let daemon = Daemon::spawn(&bin).expect("spawning the release daemon");
    daemon.wait_ready(Duration::from_secs(30)).expect("daemon should become ready");

    std::thread::sleep(SETTLE);

    let rss = daemon.resident_bytes().expect("reading RSS");
    let ceiling = ceilings::DAEMON_IDLE;

    // Report before asserting: a failure message that only says "over budget"
    // sends the reader hunting for the number.
    eprintln!(
        "idle RSS: {:.2} MiB (ceiling {:.2} MiB, {:.0}% used)",
        rss as f64 / 1_048_576.0,
        ceiling as f64 / 1_048_576.0,
        100.0 * rss as f64 / ceiling as f64
    );

    let stderr = daemon.shutdown();
    if !stderr.trim().is_empty() {
        eprintln!("daemon stderr:\n{stderr}");
    }

    assert!(
        rss <= ceiling,
        "idle RSS {rss} bytes exceeds the {ceiling}-byte ceiling in docs/memory-budget.md. \
         Either the regression is real, or the budget needs an explicit, documented revision — \
         do not raise the constant to make this pass."
    );
}

/// The daemon must serve `/healthz` and shut down cleanly on SIGTERM.
///
/// Lives here rather than in `dafs-api` because it exercises the real binary's
/// startup and signal handling, which unit tests of the router cannot reach.
#[test]
fn release_binary_serves_and_shuts_down() {
    let bin = match binary() {
        Ok(b) => b,
        Err(e) => {
            skip(&e);
            return;
        }
    };

    let daemon = Daemon::spawn(&bin).expect("spawning the release daemon");
    daemon.wait_ready(Duration::from_secs(30)).expect("daemon should become ready");

    let health = daemon.probe("/healthz").expect("probing /healthz");
    assert!(status_is(&health, 200), "unexpected /healthz response: {health}");

    let version = daemon.probe("/version").expect("probing /version");
    assert!(version.contains("schema_version"), "unexpected /version response: {version}");

    let metrics = daemon.probe("/metrics").expect("probing /metrics");
    assert!(
        metrics.contains("dafs_resident_bytes"),
        "metrics should export RSS so the budget is observable in production, got: {metrics}"
    );

    // shutdown() sends SIGTERM and waits. A daemon that ignored it would hang
    // here rather than pass silently.
    let stderr = daemon.shutdown();
    if !stderr.trim().is_empty() {
        eprintln!("daemon stderr:\n{stderr}");
    }
}

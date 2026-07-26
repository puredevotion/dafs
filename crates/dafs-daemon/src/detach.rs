//! `--detach`: fork into the background, redirect logs to a file instead of
//! the terminal, and let the caller's shell move on immediately.
//!
//! Must run before any tokio runtime is built. Forking after starting an
//! async runtime is not something to rely on: only the calling thread
//! survives `fork()`, and tokio's other worker threads simply vanish in the
//! child, so this has to happen while the process is still single-threaded.
//!
//! The fork/setsid/redirect dance itself lives in `dafs-detach`, not here:
//! it needs an unsafe `fork()` call, which this crate's own
//! `forbid(unsafe_code)` can't have. See that crate for why `daemonize` (the
//! obvious off-the-shelf choice) isn't used instead — it's RUSTSEC-2025-0069,
//! unmaintained with no safe upgrade, which cargo-deny in CI catches.

use std::path::Path;

use anyhow::Context as _;

/// Fork, detach from the controlling terminal, and redirect stdout/stderr to
/// `<data_dir>/dafs.log`.
///
/// On success, the *parent* process exits inside this call and never
/// returns — only the child (now the daemon) continues past it.
pub fn start(data_dir: &Path) -> anyhow::Result<()> {
    let log_path = data_dir.join("dafs.log");
    dafs_detach::start(data_dir, &log_path)
        .with_context(|| format!("daemonizing (log file {})", log_path.display()))
}

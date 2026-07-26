//! `--detach`: fork into the background, redirect logs to a file instead of
//! the terminal, and let the caller's shell move on immediately.
//!
//! Must run before any tokio runtime is built. Forking after starting an
//! async runtime is not something to rely on: only the calling thread
//! survives `fork()`, and tokio's other worker threads simply vanish in the
//! child, so this has to happen while the process is still single-threaded.

use std::path::Path;

use anyhow::Context as _;

/// Fork, detach from the controlling terminal, and redirect stdout/stderr to
/// `<data_dir>/dafs.log`.
///
/// On success, the *parent* process exits inside this call and never
/// returns — only the child (now the daemon) continues past it. That is the
/// `daemonize` crate's own contract, not something this function adds.
pub fn start(data_dir: &Path) -> anyhow::Result<()> {
    let log_path = data_dir.join("dafs.log");
    let stdout = std::fs::File::create(&log_path)
        .with_context(|| format!("creating log file {}", log_path.display()))?;
    let stderr =
        stdout.try_clone().context("cloning the log file handle for stderr redirection")?;

    daemonize::Daemonize::new()
        .working_directory(data_dir)
        .stdout(stdout)
        .stderr(stderr)
        .start()
        .map_err(|e| anyhow::anyhow!("daemonizing: {e}"))
}

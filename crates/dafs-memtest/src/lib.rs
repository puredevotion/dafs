//! RSS ceiling assertions for the release daemon.
//!
//! # Why this is a separate crate
//!
//! The ceilings in `docs/memory-budget.md` are properties of the **release**
//! binary, measured from **outside** the process. A `#[test]` inside the daemon
//! cannot do that: it measures a debug build, with the test harness's own
//! allocations mixed in, and debug allocation behaviour is not a proxy for
//! release. So this crate spawns the real binary and reads its RSS from procfs.
//!
//! # Why the ceiling is asserted in CI from M00
//!
//! Retrofitting a memory budget is how budgets get missed. Asserting 32 MB now,
//! when the daemon does almost nothing, means every later milestone's PR shows
//! its own memory cost — instead of the ceiling being discovered as unreachable
//! at M03 with three milestones of design already committed to it.
//!
//! # `--detach false` is not optional here (found at M02a)
//!
//! `--detach` defaults on since M01a: the spawned process forks, redirects its
//! own stdio, and the *parent* — the one whose pid `Command::spawn` hands
//! back — exits immediately, leaving the daemon proper running as a different,
//! unrelated pid this harness never learns. Every `Daemon::spawn*` constructor
//! passes `--detach false` for exactly this reason. Without it,
//! `resident_bytes()` still succeeds — it reads a real, if already-exited,
//! process's `/proc/<pid>/statm` — so nothing here fails loudly; the ceiling
//! assertion just silently measures near-zero and always "passes". This was
//! caught by the M02a extraction-queue scenario reporting an implausible
//! 0.00 MiB, not by the pre-existing idle-RSS test, which had been measuring
//! nothing meaningful since `--detach` defaulted on and gave no sign of it.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Ceilings from `docs/memory-budget.md`, in bytes.
pub mod ceilings {
    /// Daemon idle: watcher + event store + API, no index resident.
    pub const DAEMON_IDLE: u64 = 32 * 1024 * 1024;
    /// Daemon serving search, including quantised vectors. Not yet reachable —
    /// there is no index before M03; recorded here so the number lives in one
    /// place.
    pub const DAEMON_WITH_SEARCH: u64 = 96 * 1024 * 1024;
    /// Peak during an initial 1M-file scan. Bounded by streaming, not corpus
    /// size. Asserted from M01, when a scan exists.
    pub const SCAN_PEAK: u64 = 128 * 1024 * 1024;
}

/// A spawned daemon under measurement, killed on drop.
pub struct Daemon {
    child: std::process::Child,
    port: u16,
    #[allow(dead_code)]
    data_dir: tempfile::TempDir,
}

impl Daemon {
    /// Spawn the release daemon on an ephemeral port with a fresh data dir,
    /// watching nothing.
    ///
    /// Requires the binary to have been built already; the harness does not
    /// invoke cargo, so a stale binary would be measured silently. `binary()`
    /// checks the mtime against the source tree to catch that.
    pub fn spawn(binary: &Path) -> std::io::Result<Self> {
        Self::spawn_inner(binary, None)
    }

    /// Spawn the release daemon watching `watch`, for scenarios that need it
    /// to have something to scan and extract — `spawn`'s idle scenario is
    /// deliberately watch-free, which is a different (and less realistic)
    /// startup path than every real deployment takes.
    pub fn spawn_watching(binary: &Path, watch: &Path) -> std::io::Result<Self> {
        Self::spawn_inner(binary, Some(watch))
    }

    fn spawn_inner(binary: &Path, watch: Option<&Path>) -> std::io::Result<Self> {
        let data_dir = tempfile::tempdir()?;
        // Port 0 lets the OS choose, avoiding collisions when tests run in
        // parallel; the daemon logs the bound address, but reading it back from
        // stderr is racy, so instead bind a probe socket, take its port, drop
        // it, and hand that to the daemon. A tiny race remains if something
        // else grabs the port in between — retried by the caller.
        let port = free_port()?;

        let mut cmd = std::process::Command::new(binary);
        cmd.arg("--listen")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--data-dir")
            .arg(data_dir.path())
            .arg("--log")
            .arg("warn") // quieter logs mean less allocation noise in the measurement
            // `--detach` defaults on (M01a): the spawned process forks, the
            // parent exits immediately, and the daemon proper continues as a
            // *different* pid this harness never learns. Left at its
            // default, `resident_bytes()` reads the already-exited parent —
            // near-zero RSS, read successfully, ceiling trivially "met". See
            // this crate's docs on the bug this was.
            .arg("--detach")
            .arg("false");
        if let Some(watch) = watch {
            cmd.arg("--watch").arg(watch);
        }
        let child =
            cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn()?;

        Ok(Self { child, port, data_dir })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Block until `/readyz` returns 200, or time out.
    ///
    /// Polls with a plain TCP connect plus a minimal HTTP/1.0 request rather
    /// than pulling in an HTTP client: this crate is test-only and a client
    /// dependency would be a dependency the daemon's own supply chain has to
    /// answer for.
    pub fn wait_ready(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut last = String::from("never attempted");
        while Instant::now() < deadline {
            match self.probe("/readyz") {
                Ok(resp) if status_is(&resp, 200) => return Ok(()),
                Ok(resp) => last = resp.lines().next().unwrap_or("empty").to_string(),
                Err(e) => last = e,
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Err(format!("daemon not ready within {timeout:?}; last response: {last}"))
    }

    /// Issue a bare HTTP/1.0 GET and return the raw response.
    pub fn probe(&self, path: &str) -> Result<String, String> {
        use std::io::{Read as _, Write as _};

        let mut stream =
            std::net::TcpStream::connect(("127.0.0.1", self.port)).map_err(|e| e.to_string())?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
        write!(stream, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .map_err(|e| e.to_string())?;

        let mut buf = String::new();
        stream.read_to_string(&mut buf).map_err(|e| e.to_string())?;
        Ok(buf)
    }

    /// Resident set size of the spawned process, in bytes.
    #[cfg(target_os = "linux")]
    pub fn resident_bytes(&self) -> Result<u64, String> {
        let path = format!("/proc/{}/statm", self.pid());
        let statm = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
        let pages: u64 = statm
            .split_whitespace()
            .nth(1)
            .ok_or("statm missing resident field")?
            .parse()
            .map_err(|e| format!("parsing statm: {e}"))?;
        Ok(pages * 4096)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn resident_bytes(&self) -> Result<u64, String> {
        Err("RSS measurement is linux-only".into())
    }

    /// Ask the daemon to stop, then wait for it. Returns its stderr.
    pub fn shutdown(mut self) -> String {
        // SIGTERM rather than kill: this also exercises the daemon's graceful
        // shutdown path, so a hang there fails a test instead of going unnoticed.
        #[cfg(unix)]
        unsafe_kill(self.child.id());

        let _ = self.child.wait();
        let mut err = String::new();
        if let Some(mut stderr) = self.child.stderr.take() {
            use std::io::Read as _;
            let _ = stderr.read_to_string(&mut err);
        }
        err
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Best effort: a panicking test must not leave a daemon running.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Send SIGTERM without a libc dependency, via /bin/kill.
///
/// Shelling out is slower than `libc::kill`, but this is test-only code run a
/// handful of times, and it keeps the dependency tree free of libc for a signal.
#[cfg(unix)]
fn unsafe_kill(pid: u32) {
    let _ = std::process::Command::new("kill").arg("-TERM").arg(pid.to_string()).status();
}

/// Whether a raw HTTP response carries the given status code.
///
/// Matches on the code alone, not the whole status line: the server echoes the
/// request's HTTP version, so probing with HTTP/1.0 yields an "HTTP/1.0 200 OK"
/// status line. An earlier version of this harness compared against a hardcoded
/// "HTTP/1.1 200" prefix and timed out against a daemon that was answering
/// correctly the whole time.
pub fn status_is(response: &str, code: u16) -> bool {
    response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .is_some_and(|c| c == code)
}

/// Pull one Prometheus metric's numeric value out of a raw `/metrics`
/// response.
///
/// A hand parser rather than a metrics-parsing crate: this is test-only code
/// reading output produced by `dafs-api`'s own handler, a dependency this
/// crate would otherwise have no reason to take on. Matches on the metric
/// name starting a line, which the exposition format guarantees only the
/// value line itself does — `# HELP`/`# TYPE` lines start with `#`.
pub fn metric_value(response: &str, name: &str) -> Option<u64> {
    response.lines().find_map(|line| line.strip_prefix(name)?.trim_start().parse().ok())
}

fn free_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Locate the release daemon binary.
///
/// Honours `CARGO_BIN_EXE_dafs` when present (set by cargo for integration
/// tests of the same package), else walks up from the manifest dir to
/// `target/release/dafs`.
pub fn binary() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("DAFS_BINARY") {
        let p = PathBuf::from(p);
        return if p.exists() { Ok(p) } else { Err(format!("DAFS_BINARY={p:?} does not exist")) };
    }

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest
        .parent()
        .and_then(|p| p.parent())
        .ok_or("cannot locate workspace root from CARGO_MANIFEST_DIR")?;
    let candidate = workspace.join("target/release/dafs");

    if candidate.exists() {
        Ok(candidate)
    } else {
        Err(format!(
            "release binary not found at {}; run `cargo build --release -p dafs-daemon` first",
            candidate.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_value_reads_a_gauge_line_and_ignores_help_and_type_comments() {
        let body = "# HELP dafs_extraction_queue_depth Files waiting.\n\
                     # TYPE dafs_extraction_queue_depth gauge\n\
                     dafs_extraction_queue_depth 7\n";
        assert_eq!(metric_value(body, "dafs_extraction_queue_depth"), Some(7));
    }

    #[test]
    fn metric_value_is_none_for_a_metric_absent_from_the_response() {
        let body = "dafs_ready 1\n";
        assert_eq!(metric_value(body, "dafs_extraction_queue_depth"), None);
    }
}

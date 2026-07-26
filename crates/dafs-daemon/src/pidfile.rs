//! The pidfile: how a second `dafs` invocation against the same data-dir
//! finds — and talks to — an already-running one, instead of just failing to
//! bind the port with no path forward.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PidFile {
    pub pid: u32,
    pub listen: SocketAddr,
}

fn path_for(data_dir: &Path) -> PathBuf {
    data_dir.join("dafs.pid")
}

/// Check for a live daemon already running against `data_dir`.
///
/// A pidfile whose process no longer exists is stale — left behind by a
/// crash or a `kill -9` rather than a clean shutdown — and is removed rather
/// than trusted: the whole point of this check is telling a live daemon
/// apart from a dead one, and a stale file left in place would make every
/// future start attempt think one is running when none is.
pub fn find_live(data_dir: &Path) -> Option<PidFile> {
    let path = path_for(data_dir);
    let contents = std::fs::read_to_string(&path).ok()?;
    let parsed: PidFile = serde_json::from_str(&contents).ok()?;

    if is_process_alive(parsed.pid) {
        Some(parsed)
    } else {
        let _ = std::fs::remove_file(&path);
        None
    }
}

/// Linux-only (`/proc`), matching the project's primary target (README's
/// Platforms table) and avoiding a libc dependency in a crate that forbids
/// unsafe code. Elsewhere this conservatively assumes "alive": a false
/// positive here just means a start attempt hits the ordinary "address
/// already in use" error, which is a real failure with its own real fix,
/// rather than silently binding over a daemon that is genuinely still there.
#[cfg(target_os = "linux")]
fn is_process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn is_process_alive(_pid: u32) -> bool {
    true
}

pub fn write(data_dir: &Path, listen: SocketAddr) -> anyhow::Result<()> {
    let pidfile = PidFile { pid: std::process::id(), listen };
    let json = serde_json::to_string(&pidfile).context("serializing the pidfile")?;
    let path = path_for(data_dir);
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))
}

/// Best-effort: called on the clean-shutdown path, where a failure to remove
/// the file is worth logging but never worth failing the shutdown over.
pub fn remove(data_dir: &Path) {
    if let Err(e) = std::fs::remove_file(path_for(data_dir))
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("could not remove pidfile: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_pidfile_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let addr: SocketAddr = "127.0.0.1:7878".parse().unwrap();

        write(dir.path(), addr).expect("write");
        let found = find_live(dir.path()).expect("should find the pidfile we just wrote");

        assert_eq!(found.pid, std::process::id(), "our own pid is always alive");
        assert_eq!(found.listen, addr);
    }

    #[test]
    fn a_missing_pidfile_is_quietly_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(find_live(dir.path()).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_stale_pidfile_is_removed_and_reported_as_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = path_for(dir.path());
        // A pid essentially guaranteed not to be a live process on any real
        // system (max pid on Linux is far below this).
        let stale = PidFile { pid: 4_000_000_000, listen: "127.0.0.1:7878".parse().unwrap() };
        std::fs::write(&path, serde_json::to_string(&stale).unwrap()).unwrap();

        assert!(find_live(dir.path()).is_none(), "a dead pid's file must not count as live");
        assert!(!path.exists(), "the stale file should have been cleaned up");
    }

    #[test]
    fn garbage_in_the_pidfile_is_treated_as_absent_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(path_for(dir.path()), "not json").unwrap();
        assert!(find_live(dir.path()).is_none());
    }
}

//! End-to-end test of the whole "second `dafs` invocation against a running
//! one" story: pidfile discovery, `--on-running add`/`replace`, `GET /watch`
//! reflecting the change, and `dafs stop` actually stopping it.
//!
//! Runs the real built binary as a subprocess (`CARGO_BIN_EXE_dafs`) against
//! real temp directories and a real loopback socket — this is exactly the
//! failure mode found in practice (a stale daemon silently answering while a
//! new one fails to bind), so it's tested with real processes, not mocks.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn dafs_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dafs")
}

#[derive(serde::Deserialize)]
struct PidFile {
    listen: SocketAddr,
}

/// Poll the pidfile until it exists and its recorded port actually accepts a
/// connection — the file can be written a moment before the listener is
/// fully ready to accept.
fn wait_for_ready(data_dir: &Path) -> SocketAddr {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(contents) = std::fs::read_to_string(data_dir.join("dafs.pid"))
            && let Ok(pidfile) = serde_json::from_str::<PidFile>(&contents)
            && TcpStream::connect_timeout(&pidfile.listen, Duration::from_millis(200)).is_ok()
        {
            return pidfile.listen;
        }
        assert!(Instant::now() < deadline, "daemon never became ready");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn http(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).expect("write");
    // No write-side half-close here: hyper treats an incoming FIN as the
    // client hanging up and never writes a response. `Connection: close`
    // alone is enough for read_to_string to be correct once the server closes.
    let mut resp = String::new();
    stream.read_to_string(&mut resp).expect("read");
    let (head, body) = resp.split_once("\r\n\r\n").unwrap_or((resp.as_str(), ""));
    let status = head.lines().next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, body.to_string())
}

struct RunningDaemon {
    child: std::process::Child,
    data_dir: tempfile::TempDir,
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        // Best-effort: most tests stop it deliberately as part of the
        // assertion, this is only a safety net for the ones that don't (or
        // that fail before reaching that point).
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_daemon(watch: &Path) -> RunningDaemon {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let child = Command::new(dafs_bin())
        .arg("--data-dir")
        .arg(data_dir.path())
        .arg("--listen")
        .arg("127.0.0.1:0")
        .arg("--watch")
        .arg(watch)
        // --detach defaults on now; this test's Child handle (and its
        // Drop-based cleanup) needs to track the actual daemon process, not
        // a parent that forks and exits immediately.
        .arg("--detach")
        .arg("false")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dafs");
    RunningDaemon { child, data_dir }
}

#[test]
fn a_second_invocation_with_on_running_add_extends_the_watch_list() {
    let watch_a = tempfile::tempdir().expect("tempdir");
    let watch_b = tempfile::tempdir().expect("tempdir");
    let daemon = spawn_daemon(watch_a.path());
    let addr = wait_for_ready(daemon.data_dir.path());

    let output = Command::new(dafs_bin())
        .arg("--data-dir")
        .arg(daemon.data_dir.path())
        .arg("--watch")
        .arg(watch_b.path())
        .arg("--on-running")
        .arg("add")
        .output()
        .expect("run second invocation");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&watch_a.path().display().to_string()),
        "lost the original root: {stdout}"
    );
    assert!(
        stdout.contains(&watch_b.path().display().to_string()),
        "did not add the new root: {stdout}"
    );

    let (status, body) = http(addr, "GET", "/watch", "");
    assert_eq!(status, 200);
    assert!(
        body.contains(&watch_b.path().display().to_string()),
        "daemon's own /watch disagrees: {body}"
    );
}

#[test]
fn a_second_invocation_with_on_running_replace_drops_the_old_root() {
    let watch_a = tempfile::tempdir().expect("tempdir");
    let watch_b = tempfile::tempdir().expect("tempdir");
    let daemon = spawn_daemon(watch_a.path());
    let addr = wait_for_ready(daemon.data_dir.path());

    let output = Command::new(dafs_bin())
        .arg("--data-dir")
        .arg(daemon.data_dir.path())
        .arg("--watch")
        .arg(watch_b.path())
        .arg("--on-running")
        .arg("replace")
        .output()
        .expect("run second invocation");
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));

    let (status, body) = http(addr, "GET", "/watch", "");
    assert_eq!(status, 200);
    assert!(
        !body.contains(&watch_a.path().display().to_string()),
        "replace kept the old root: {body}"
    );
    assert!(body.contains(&watch_b.path().display().to_string()));
}

#[test]
fn a_second_invocation_with_on_running_cancel_leaves_it_alone() {
    let watch_a = tempfile::tempdir().expect("tempdir");
    let watch_b = tempfile::tempdir().expect("tempdir");
    let daemon = spawn_daemon(watch_a.path());
    let addr = wait_for_ready(daemon.data_dir.path());

    let output = Command::new(dafs_bin())
        .arg("--data-dir")
        .arg(daemon.data_dir.path())
        .arg("--watch")
        .arg(watch_b.path())
        .arg("--on-running")
        .arg("cancel")
        .output()
        .expect("run second invocation");
    assert!(output.status.success());

    let (_, body) = http(addr, "GET", "/watch", "");
    assert!(body.contains(&watch_a.path().display().to_string()));
    assert!(
        !body.contains(&watch_b.path().display().to_string()),
        "cancel should not have added anything"
    );
}

#[test]
fn a_second_invocation_with_no_flag_and_no_tty_refuses_to_guess() {
    let watch_a = tempfile::tempdir().expect("tempdir");
    let watch_b = tempfile::tempdir().expect("tempdir");
    let daemon = spawn_daemon(watch_a.path());
    wait_for_ready(daemon.data_dir.path());

    let output = Command::new(dafs_bin())
        .arg("--data-dir")
        .arg(daemon.data_dir.path())
        .arg("--watch")
        .arg(watch_b.path())
        .stdin(Stdio::null())
        .output()
        .expect("run second invocation");

    assert!(!output.status.success(), "must not silently pick add or replace");
    assert!(String::from_utf8_lossy(&output.stderr).contains("--on-running"));
}

#[test]
fn a_second_invocation_with_no_watch_reports_status_without_erroring() {
    let watch_a = tempfile::tempdir().expect("tempdir");
    let daemon = spawn_daemon(watch_a.path());
    wait_for_ready(daemon.data_dir.path());

    let output = Command::new(dafs_bin())
        .arg("--data-dir")
        .arg(daemon.data_dir.path())
        .output()
        .expect("run second invocation");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(&watch_a.path().display().to_string()));
}

#[test]
fn stop_actually_stops_it() {
    let watch_a = tempfile::tempdir().expect("tempdir");
    let mut daemon = spawn_daemon(watch_a.path());
    wait_for_ready(daemon.data_dir.path());

    let output = Command::new(dafs_bin())
        .arg("stop")
        .arg("--data-dir")
        .arg(daemon.data_dir.path())
        .output()
        .expect("run stop");
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));

    // The child should have exited on its own (graceful shutdown from the
    // SIGTERM `stop` sent) well within the wait() call's patience.
    let status = daemon.child.wait().expect("wait");
    assert!(status.success() || status.code().is_none(), "daemon exited abnormally: {status:?}");
    assert!(
        !daemon.data_dir.path().join("dafs.pid").exists(),
        "pidfile should be gone after clean shutdown"
    );
}

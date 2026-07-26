//! Bridges the observer thread to [`dafs_api::WatchControl`].
//!
//! The HTTP handlers never touch the observer directly — they send a
//! [`WatchCommand`] with a reply channel and wait for the observer thread's
//! answer. A fire-and-forget version of this (send the command, immediately
//! read the shared root list) was tried first and is wrong: the observer
//! thread might not have reached its next poll iteration yet, so the "post
//! change" response would sometimes still show the pre-change state — caught
//! by `tests/live_reconfiguration.rs` failing intermittently against a real
//! daemon. Blocking for the reply is what makes `PUT /watch`'s response
//! trustworthy.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dafs_api::WatchControl;

/// Generous: a newly added root is scanned before the reply is sent, and a
/// very large directory could take a while. Long enough that a legitimate
/// scan is never mistaken for a hung observer thread; short enough that a
/// genuinely hung one doesn't leave an HTTP request blocked forever.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// A live reconfiguration request, applied by the observer thread on its next
/// poll iteration. Each variant carries a reply channel so the caller knows
/// once it has actually happened, not just been enqueued.
pub enum WatchCommand {
    AddRoots { roots: Vec<PathBuf>, reply: mpsc::Sender<Result<(), String>> },
    ReplaceRoots { roots: Vec<PathBuf>, reply: mpsc::Sender<Result<(), String>> },
}

pub struct DaemonWatchControl {
    roots: Arc<Mutex<Vec<PathBuf>>>,
    commands: mpsc::Sender<WatchCommand>,
}

impl DaemonWatchControl {
    pub fn new(roots: Arc<Mutex<Vec<PathBuf>>>, commands: mpsc::Sender<WatchCommand>) -> Self {
        Self { roots, commands }
    }

    fn send_and_wait(
        &self,
        build: impl FnOnce(Vec<PathBuf>, mpsc::Sender<Result<(), String>>) -> WatchCommand,
        roots: Vec<String>,
    ) -> Result<(), String> {
        let paths = validate_dirs(&roots)?;
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(build(paths, reply_tx))
            .map_err(|_| "the observer thread is gone".to_string())?;
        reply_rx
            .recv_timeout(REPLY_TIMEOUT)
            .map_err(|_| "the observer thread did not respond in time".to_string())?
    }
}

/// Rejects anything that isn't an existing directory before it ever reaches
/// the observer thread — a bad path should be a 400 to the caller, not a
/// silently-ignored command or a scan error buried in the daemon's own log.
fn validate_dirs(roots: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::with_capacity(roots.len());
    for root in roots {
        let path = PathBuf::from(root);
        if !path.is_dir() {
            return Err(format!("{} is not a directory", path.display()));
        }
        paths.push(path);
    }
    Ok(paths)
}

impl WatchControl for DaemonWatchControl {
    fn roots(&self) -> Vec<String> {
        self.roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|p| p.display().to_string())
            .collect()
    }

    fn add_roots(&self, roots: Vec<String>) -> Result<(), String> {
        self.send_and_wait(|roots, reply| WatchCommand::AddRoots { roots, reply }, roots)
    }

    fn replace_roots(&self, roots: Vec<String>) -> Result<(), String> {
        self.send_and_wait(|roots, reply| WatchCommand::ReplaceRoots { roots, reply }, roots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nonexistent_path_is_rejected_before_it_reaches_the_channel() {
        let (tx, rx) = mpsc::channel();
        let control = DaemonWatchControl::new(Arc::new(Mutex::new(vec![])), tx);

        let result = control.add_roots(vec!["/definitely/not/a/real/path/xyz".into()]);

        assert!(result.is_err());
        assert!(rx.try_recv().is_err(), "an invalid path must never reach the observer thread");
    }

    #[test]
    fn a_real_directory_is_accepted_queued_and_waits_for_the_reply() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = mpsc::channel();
        let control = DaemonWatchControl::new(Arc::new(Mutex::new(vec![])), tx);

        // Stand in for the observer thread: receive the command, reply
        // success, exactly what add_roots is waiting to see.
        let responder = std::thread::spawn(move || match rx.recv().expect("command") {
            WatchCommand::AddRoots { roots, reply } => {
                assert_eq!(roots.len(), 1);
                reply.send(Ok(())).unwrap();
            }
            WatchCommand::ReplaceRoots { .. } => panic!("expected AddRoots"),
        });

        control.add_roots(vec![dir.path().display().to_string()]).expect("valid dir, replied ok");
        responder.join().unwrap();
    }

    #[test]
    fn the_observer_s_own_error_surfaces_to_the_caller() {
        let (tx, rx) = mpsc::channel();
        let control = DaemonWatchControl::new(Arc::new(Mutex::new(vec![])), tx);
        let dir = tempfile::tempdir().expect("tempdir");

        let responder = std::thread::spawn(move || match rx.recv().expect("command") {
            WatchCommand::AddRoots { reply, .. } => {
                reply.send(Err("permission denied".to_string())).unwrap();
            }
            WatchCommand::ReplaceRoots { .. } => panic!("expected AddRoots"),
        });

        let result = control.add_roots(vec![dir.path().display().to_string()]);
        assert_eq!(result, Err("permission denied".to_string()));
        responder.join().unwrap();
    }

    #[test]
    fn no_observer_thread_at_all_is_reported_not_hung_forever() {
        let (tx, rx) = mpsc::channel();
        drop(rx); // nothing will ever receive the command
        let dir = tempfile::tempdir().expect("tempdir");
        let control = DaemonWatchControl::new(Arc::new(Mutex::new(vec![])), tx);

        let result = control.add_roots(vec![dir.path().display().to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn roots_reflects_whatever_the_shared_state_currently_holds() {
        let shared = Arc::new(Mutex::new(vec![PathBuf::from("/a"), PathBuf::from("/b")]));
        let (tx, _rx) = mpsc::channel();
        let control = DaemonWatchControl::new(shared, tx);

        assert_eq!(control.roots(), vec!["/a".to_string(), "/b".to_string()]);
    }
}

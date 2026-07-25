//! Live filesystem watching.
//!
//! The initial scan establishes what exists; this keeps it current. Both write
//! the same events through the same path, so the timeline cannot tell whether a
//! row came from a scan or a watch — which is the point, since a user does not
//! care and a divergence between the two would be a bug that only shows up as
//! inconsistent history.
//!
//! # Debouncing
//!
//! An editor saving one file produces several inotify events: a create of a
//! temporary, a write, a rename over the original, sometimes a chmod. Recording
//! all of them would make the timeline a log of syscalls rather than of work.
//!
//! Events are therefore coalesced per path over a short window, and the window
//! restarts on each new event for that path — so a file being written
//! continuously produces one event when the writing stops, not one per burst.
//! [`DEBOUNCE`] is the window; it is deliberately short enough that the timeline
//! still feels live.
//!
//! # Overflow
//!
//! The kernel's event queue is finite. Under a large burst — extracting an
//! archive, checking out a branch — it overflows and events are lost. `notify`
//! surfaces that, and the only correct response is a rescan of the affected
//! root, because there is no way to know what was missed. Treating an overflow
//! as "no events" would silently desynchronise the timeline from the filesystem
//! and stay wrong until the next restart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::{EventKind as NotifyKind, RecursiveMode, Watcher};

/// How long a path stays quiet before its pending change is recorded.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// What the watcher decided happened, after coalescing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A path was created or modified. Which of the two is decided against the
    /// store at record time, not here — the watcher cannot know whether a path
    /// it has just seen was already known.
    Touched(PathBuf),
    Removed(PathBuf),
    /// The kernel queue overflowed and events were lost. The only safe response
    /// is a rescan; see the module docs.
    Overflowed,
}

/// A pending change, with the instant its debounce window last restarted.
struct Pending {
    change: Change,
    last_seen: Instant,
}

/// Watches one or more roots and emits coalesced changes.
///
/// Owns a background thread from `notify` plus the debounce state. Dropping it
/// stops the watch.
pub struct Watch {
    _watcher: notify::RecommendedWatcher,
    rx: mpsc::Receiver<notify::Result<notify::Event>>,
    pending: HashMap<PathBuf, Pending>,
    overflowed: bool,
}

impl Watch {
    /// Start watching `roots` recursively.
    pub fn new(roots: &[PathBuf]) -> notify::Result<Self> {
        let (tx, rx) = mpsc::channel();

        let mut watcher = notify::recommended_watcher(move |res| {
            // A send failure means the receiver is gone, i.e. the Watch was
            // dropped. Nothing to do but stop caring.
            let _ = tx.send(res);
        })?;

        for root in roots {
            watcher.watch(root, RecursiveMode::Recursive)?;
        }

        Ok(Self { _watcher: watcher, rx, pending: HashMap::new(), overflowed: false })
    }

    /// Collect changes whose debounce window has expired.
    ///
    /// Non-blocking beyond `timeout`: drains whatever the kernel has produced,
    /// then returns the pending changes that have gone quiet. A caller loops on
    /// this.
    pub fn poll(&mut self, timeout: Duration) -> Vec<Change> {
        let deadline = Instant::now() + timeout;

        // Drain everything currently queued rather than handling one event per
        // call: under a burst, one-at-a-time would fall behind the producer and
        // the debounce window would never expire.
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(remaining) {
                Ok(Ok(event)) => self.absorb(event),
                Ok(Err(e)) => {
                    // notify reports queue overflow as an error on some
                    // backends and as an event kind on others.
                    tracing::warn!("watch error: {e}");
                    self.overflowed = true;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if Instant::now() >= deadline {
                break;
            }
        }

        self.take_expired()
    }

    /// Fold one raw notify event into the pending set.
    fn absorb(&mut self, event: notify::Event) {
        if matches!(event.kind, NotifyKind::Other) && event.need_rescan() {
            self.overflowed = true;
            return;
        }

        let now = Instant::now();

        for path in event.paths {
            let change = match event.kind {
                NotifyKind::Remove(_) => Change::Removed(path.clone()),
                NotifyKind::Create(_) | NotifyKind::Modify(_) => Change::Touched(path.clone()),
                // Access events (a file being read) are not changes and would
                // flood the timeline with noise about merely opening things.
                NotifyKind::Access(_) => continue,
                NotifyKind::Any | NotifyKind::Other => Change::Touched(path.clone()),
            };

            // Restarting the window on each event is what collapses a
            // continuous write into one entry rather than one per burst.
            self.pending.insert(path, Pending { change, last_seen: now });
        }
    }

    /// Remove and return changes that have been quiet for longer than the
    /// debounce window.
    fn take_expired(&mut self) -> Vec<Change> {
        let now = Instant::now();
        let mut out = Vec::new();

        if std::mem::take(&mut self.overflowed) {
            // First, so a caller that stops at the first overflow still rescans
            // before acting on individual paths that may now be stale.
            out.push(Change::Overflowed);
        }

        self.pending.retain(|_, pending| {
            if now.duration_since(pending.last_seen) >= DEBOUNCE {
                out.push(pending.change.clone());
                false
            } else {
                true
            }
        });

        out
    }

    /// Pending, not-yet-emitted changes. For tests and diagnostics.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

/// Whether a path is inside a directory the scanner would skip.
///
/// The watcher sees everything under a root, including the `.git` churn the
/// scan filters out, so the same exclusions have to apply here — otherwise a
/// single `git status` fills the timeline.
pub fn is_excluded(path: &Path, skip_dirs: &[String]) -> bool {
    path.components().any(|c| match c {
        std::path::Component::Normal(name) => {
            let name = name.to_string_lossy();
            skip_dirs.iter().any(|d| d == name.as_ref())
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusions_match_any_ancestor_not_just_the_leaf() {
        let skip: Vec<String> = [".git", "node_modules"].iter().map(|s| s.to_string()).collect();

        assert!(is_excluded(Path::new("/home/u/p/.git/objects/ab/cdef"), &skip));
        assert!(is_excluded(Path::new("/home/u/p/node_modules/left-pad/index.js"), &skip));
        assert!(!is_excluded(Path::new("/home/u/p/src/main.rs"), &skip));
    }

    /// A path merely *containing* an excluded name as a substring is not
    /// excluded — `.gitignore` is a real file a user edits.
    #[test]
    fn exclusions_match_whole_components_not_substrings() {
        let skip: Vec<String> = [".git"].iter().map(|s| s.to_string()).collect();
        assert!(!is_excluded(Path::new("/home/u/p/.gitignore"), &skip));
        assert!(!is_excluded(Path::new("/home/u/p/git/notes.md"), &skip));
    }

    #[test]
    fn watching_a_real_directory_reports_a_created_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut watch = Watch::new(&[dir.path().to_path_buf()]).expect("watch");

        std::fs::write(dir.path().join("a.txt"), "hello").expect("write");

        // Poll until the debounce window has expired. Bounded so a platform
        // where the watch silently does nothing fails rather than hangs.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut changes = Vec::new();
        while Instant::now() < deadline && changes.is_empty() {
            changes = watch.poll(Duration::from_millis(200));
        }

        assert!(
            changes.iter().any(|c| matches!(c, Change::Touched(p) if p.ends_with("a.txt"))),
            "expected a Touched event for a.txt, got {changes:?}"
        );
    }

    /// The debounce contract: a burst of writes to one path yields one change,
    /// not one per write.
    #[test]
    fn repeated_writes_to_one_path_coalesce() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("busy.txt");
        let mut watch = Watch::new(&[dir.path().to_path_buf()]).expect("watch");

        for n in 0..10 {
            std::fs::write(&path, format!("write {n}")).expect("write");
            // Well inside the debounce window, so every write restarts it.
            std::thread::sleep(Duration::from_millis(20));
            let _ = watch.poll(Duration::from_millis(1));
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut changes = Vec::new();
        while Instant::now() < deadline && changes.is_empty() {
            changes = watch.poll(Duration::from_millis(200));
        }

        let touches = changes
            .iter()
            .filter(|c| matches!(c, Change::Touched(p) if p.ends_with("busy.txt")))
            .count();
        assert_eq!(touches, 1, "ten writes should coalesce to one change, got {changes:?}");
    }

    #[test]
    fn an_overflow_is_reported_so_the_caller_can_rescan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut watch = Watch::new(&[dir.path().to_path_buf()]).expect("watch");

        // Simulating a real kernel overflow is not portable, so drive the flag
        // directly: what matters is that the signal reaches the caller, which
        // is the part the daemon's correctness depends on.
        watch.overflowed = true;
        let changes = watch.poll(Duration::from_millis(10));

        assert!(
            changes.contains(&Change::Overflowed),
            "an overflow must be surfaced, got {changes:?}"
        );
        assert!(
            !watch.poll(Duration::from_millis(10)).contains(&Change::Overflowed),
            "an overflow should be reported once, not latch forever"
        );
    }
}

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
//! # Rename correlation
//!
//! A rename arrives as two events — the path it left and the path it arrived at
//! — and pairing them wrongly is worse than not pairing them at all, because it
//! merges two files' histories into one. So this does not guess.
//!
//! inotify assigns both halves a **cookie**, surfaced by `notify` as an event
//! tracker id, and that pairing comes from the kernel rather than from
//! heuristics about timing or size. `RenameMode::Both` carries both paths in one
//! event; `From`/`To` arrive separately and are matched on the tracker id.
//!
//! An unmatched half is not a rename. A file moved *out* of a watched tree has a
//! `From` with no `To`, and a file moved *in* has the reverse — those are a
//! delete and a create, which is exactly what they are from the tree's point of
//! view. [`PENDING_RENAME_TTL`] bounds how long an unmatched half waits before
//! being treated that way.
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

use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind as NotifyKind, RecursiveMode, Watcher};

/// How long a path stays quiet before its pending change is recorded.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// How long an unmatched half of a rename waits for its partner.
///
/// The two halves are emitted back to back by the kernel, so this only has to
/// cover scheduling jitter. Longer would delay reporting a genuine
/// move-out-of-tree as the deletion it is; shorter risks splitting a real
/// rename into a delete and a create under load.
pub const PENDING_RENAME_TTL: Duration = Duration::from_millis(1_000);

/// What the watcher decided happened, after coalescing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A path was created or modified. Which of the two is decided against the
    /// store at record time, not here — the watcher cannot know whether a path
    /// it has just seen was already known.
    Touched(PathBuf),
    Removed(PathBuf),
    /// A path moved. Both endpoints are known, so this is a real rename rather
    /// than an inferred one — see the module docs on rename correlation.
    Renamed {
        from: PathBuf,
        to: PathBuf,
    },
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
    /// Halves of a rename waiting for their partner, keyed by the kernel's
    /// tracker id. See the module docs on rename correlation.
    pending_renames: HashMap<usize, PendingRename>,
    overflowed: bool,
}

/// One half of a rename, waiting to be paired.
struct PendingRename {
    path: PathBuf,
    /// Which half this is. A `From` with no partner is a move out of the tree
    /// (a deletion); a `To` with no partner is a move in (a creation).
    is_source: bool,
    seen: Instant,
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

        Ok(Self {
            _watcher: watcher,
            rx,
            pending: HashMap::new(),
            pending_renames: HashMap::new(),
            overflowed: false,
        })
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

        // Renames are handled separately: they carry two paths that must stay
        // associated, which the per-path pending map cannot express.
        if let NotifyKind::Modify(ModifyKind::Name(mode)) = event.kind
            && self.absorb_rename(mode, &event, now)
        {
            return;
        }

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

    /// Fold a rename event into the pending set.
    ///
    /// Returns `false` when the event could not be interpreted as a rename, so
    /// the caller falls back to treating it as an ordinary change — better a
    /// delete-plus-create than a dropped event.
    fn absorb_rename(&mut self, mode: RenameMode, event: &notify::Event, now: Instant) -> bool {
        match mode {
            // Both endpoints in one event: no correlation needed.
            RenameMode::Both if event.paths.len() >= 2 => {
                let from = event.paths[0].clone();
                let to = event.paths[1].clone();
                self.emit_rename(from, to, now);
                true
            }

            RenameMode::From | RenameMode::To => {
                let Some(path) = event.paths.first().cloned() else {
                    return false;
                };
                // Without a tracker id there is nothing to pair on, and pairing
                // by guesswork could merge two unrelated files' histories.
                // Falling back to delete-plus-create is the honest answer.
                let Some(tracker) = event.tracker() else {
                    return false;
                };
                let is_source = mode == RenameMode::From;

                match self.pending_renames.remove(&tracker) {
                    // The partner arrived: pair them, oldest half first.
                    Some(partner) if partner.is_source != is_source => {
                        let (from, to) =
                            if is_source { (path, partner.path) } else { (partner.path, path) };
                        self.emit_rename(from, to, now);
                    }
                    // Two halves of the same direction under one tracker id
                    // should not happen; keep the newer and let the older
                    // expire rather than pairing something nonsensical.
                    Some(_) | None => {
                        self.pending_renames
                            .insert(tracker, PendingRename { path, is_source, seen: now });
                    }
                }
                true
            }

            // `Any`/`Other`, or a `Both` without two paths: not enough to
            // correlate on.
            _ => false,
        }
    }

    fn emit_rename(&mut self, from: PathBuf, to: PathBuf, now: Instant) {
        // Any pending change against either endpoint is superseded: the file's
        // new identity is what the timeline should show.
        self.pending.remove(&from);
        self.pending
            .insert(to.clone(), Pending { change: Change::Renamed { from, to }, last_seen: now });
    }

    /// Turn rename halves that never found a partner into plain changes.
    ///
    /// A `From` with no `To` is a file moved out of the watched tree, which is a
    /// deletion as far as this tree is concerned; a `To` with no `From` is a
    /// file moved in, which is a creation.
    fn expire_pending_renames(&mut self, now: Instant) {
        let expired: Vec<usize> = self
            .pending_renames
            .iter()
            .filter(|(_, p)| now.duration_since(p.seen) >= PENDING_RENAME_TTL)
            .map(|(id, _)| *id)
            .collect();

        for id in expired {
            let Some(half) = self.pending_renames.remove(&id) else { continue };
            let change = if half.is_source {
                Change::Removed(half.path.clone())
            } else {
                Change::Touched(half.path.clone())
            };
            tracing::trace!(
                path = %half.path.display(),
                moved_out = half.is_source,
                "rename half found no partner; treating it as a plain change"
            );
            self.pending.insert(half.path, Pending { change, last_seen: now });
        }
    }

    /// Remove and return changes that have been quiet for longer than the
    /// debounce window.
    fn take_expired(&mut self) -> Vec<Change> {
        let now = Instant::now();
        let mut out = Vec::new();

        // Before draining: a rename half whose partner never arrived becomes an
        // ordinary change, and must go through the same debounce as any other.
        self.expire_pending_renames(now);

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

    /// A real rename inside a watched tree must arrive as one `Renamed` with
    /// both endpoints — not as a delete plus a create, which would split the
    /// file's history in two.
    #[test]
    fn a_rename_within_the_tree_is_reported_as_one_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let before = dir.path().join("before.txt");
        std::fs::write(&before, "contents").expect("write");

        let mut watch = Watch::new(&[dir.path().to_path_buf()]).expect("watch");

        // Drain the creation above so it does not confuse the assertion.
        let settle = Instant::now() + Duration::from_secs(3);
        while Instant::now() < settle {
            watch.poll(Duration::from_millis(200));
        }

        let after = dir.path().join("after.txt");
        std::fs::rename(&before, &after).expect("rename");

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut changes = Vec::new();
        while Instant::now() < deadline && changes.is_empty() {
            changes = watch.poll(Duration::from_millis(200));
        }

        let renames: Vec<_> = changes
            .iter()
            .filter_map(|c| match c {
                Change::Renamed { from, to } => Some((from, to)),
                _ => None,
            })
            .collect();

        assert_eq!(renames.len(), 1, "expected exactly one rename, got {changes:?}");
        assert!(renames[0].0.ends_with("before.txt"), "wrong source: {:?}", renames[0].0);
        assert!(renames[0].1.ends_with("after.txt"), "wrong destination: {:?}", renames[0].1);

        // And no stray delete or create for either endpoint, which is the
        // failure mode this whole mechanism exists to avoid.
        assert!(
            !changes.iter().any(|c| matches!(c, Change::Removed(_))),
            "a rename also produced a deletion: {changes:?}"
        );
    }

    /// A half with no partner must not wait forever. Moving a file *out* of the
    /// tree produces a `From` that never pairs, and it has to surface as the
    /// deletion it effectively is.
    #[test]
    fn an_unpaired_rename_half_expires_into_a_plain_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut watch = Watch::new(&[dir.path().to_path_buf()]).expect("watch");

        // Inject a half directly: producing a genuinely unpaired one depends on
        // moving across a filesystem boundary, which is not portable in a test.
        // What matters is that an unmatched half converts rather than leaking.
        watch.pending_renames.insert(
            42,
            PendingRename {
                path: dir.path().join("gone.txt"),
                is_source: true,
                seen: Instant::now() - PENDING_RENAME_TTL - Duration::from_millis(50),
            },
        );

        // First poll expires it into `pending`; the debounce then has to lapse.
        watch.poll(Duration::from_millis(10));
        std::thread::sleep(DEBOUNCE + Duration::from_millis(100));
        let changes = watch.poll(Duration::from_millis(10));

        assert!(
            changes.iter().any(|c| matches!(c, Change::Removed(p) if p.ends_with("gone.txt"))),
            "an unpaired rename source should become a deletion, got {changes:?}"
        );
        assert_eq!(watch.pending_renames.len(), 0, "the expired half was not cleaned up");
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

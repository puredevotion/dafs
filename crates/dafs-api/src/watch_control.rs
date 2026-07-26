//! Live watch-root control.
//!
//! Mirrors `timeline.rs`'s trait-object shape: the HTTP layer needs to read
//! and change the daemon's watch roots without depending on `dafs-scan`'s
//! `Watch` or knowing how the observer thread is wired.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// What the watch-control handlers need from the daemon's observer.
///
/// Errors are `String` for the same reason as `TimelineStore`'s: the HTTP
/// layer turns them into a 4xx and a log line either way, and a richer type
/// here would leak the daemon's own error taxonomy into this crate for no
/// gain.
pub trait WatchControl: Send + Sync + 'static {
    /// Roots currently being watched, in no particular order.
    fn roots(&self) -> Vec<String>;

    /// Start watching additional roots, alongside whatever is already
    /// watched. Fails if any entry is not a directory that exists.
    fn add_roots(&self, roots: Vec<String>) -> Result<(), String>;

    /// Stop watching every current root and start watching only `roots`
    /// instead. Fails if any entry is not a directory that exists — silently
    /// producing zero watched roots would be indistinguishable from
    /// "watching nothing on purpose".
    fn replace_roots(&self, roots: Vec<String>) -> Result<(), String>;
}

/// A shared, type-erased [`WatchControl`].
pub type WatchControlHandle = Arc<dyn WatchControl>;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WatchRootsResponse {
    pub roots: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct WatchChangeRequest {
    pub mode: WatchMode,
    pub roots: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WatchMode {
    Add,
    Replace,
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Mutex;

    use super::*;

    /// An in-memory watch control for testing the HTTP layer without a real
    /// observer thread.
    pub struct FakeWatchControl {
        pub roots: Mutex<Vec<String>>,
        pub fail: bool,
    }

    impl FakeWatchControl {
        pub fn new(roots: Vec<String>) -> Self {
            Self { roots: Mutex::new(roots), fail: false }
        }
    }

    impl WatchControl for FakeWatchControl {
        fn roots(&self) -> Vec<String> {
            self.roots.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        fn add_roots(&self, roots: Vec<String>) -> Result<(), String> {
            if self.fail {
                return Err("watch control is broken".into());
            }
            self.roots.lock().unwrap_or_else(|e| e.into_inner()).extend(roots);
            Ok(())
        }

        fn replace_roots(&self, roots: Vec<String>) -> Result<(), String> {
            if self.fail {
                return Err("watch control is broken".into());
            }
            *self.roots.lock().unwrap_or_else(|e| e.into_inner()) = roots;
            Ok(())
        }
    }
}

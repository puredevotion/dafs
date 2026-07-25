//! Serving the timeline.
//!
//! # The connection question M00 left open
//!
//! M00 deliberately kept the SQLite connection out of `AppState`, because
//! rusqlite's `Connection` is not `Sync` and the right shape depends on the
//! query mix. M01 introduces that mix, so here is the answer:
//!
//! **one connection behind a mutex, on a blocking thread.**
//!
//! Not a pool. A pool exists to let queries run concurrently, and these cannot
//! usefully do so: they are all short reads against a database that is local,
//! memory-mapped, and warm, served to a single-user daemon. A pool would add N
//! page caches — the thing `docs/memory-budget.md` §8.3 spends effort keeping to
//! one — plus connection-tuning that must be reapplied per connection and is
//! silent when forgotten. If the assistant milestone brings long-running
//! queries, that is the point to revisit it, with a measurement rather than an
//! assumption.
//!
//! The mutex is held across a blocking call, so every handler wraps its access
//! in `spawn_blocking`. Holding it inside an async task would stall the whole
//! single-threaded runtime — including `/healthz` — for the duration of a query.
//!
//! The reader is defined as a trait so the HTTP layer can be tested without a
//! database, and so the daemon can hand in the real store without this crate
//! depending on `dafs-store`.

use std::sync::Arc;

use serde::Serialize;

/// One timeline row, as served.
///
/// A DTO rather than the store's own type: the wire format is a compatibility
/// surface with the UI and should not change just because a column was renamed.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TimelineItem {
    pub id: i64,
    pub path: String,
    pub kind: String,
    pub at_unix_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<i64>,
    pub is_dir: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_path: Option<String>,
}

/// What the timeline handler needs from a store.
///
/// Errors are `String` rather than a concrete type: the HTTP layer turns them
/// into a 500 and a log line either way, and a richer type here would leak the
/// store's error taxonomy into the API crate for no gain.
pub trait TimelineStore: Send + Sync + 'static {
    fn timeline(
        &self,
        limit: u32,
        before_id: Option<i64>,
        kind: Option<&str>,
    ) -> Result<Vec<TimelineItem>, String>;

    /// Event and file counts, for `/metrics` and the UI header.
    fn stats(&self) -> Result<TimelineStats, String>;
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct TimelineStats {
    pub events: i64,
    pub files: i64,
}

/// A shared, type-erased [`TimelineStore`].
pub type TimelineReader = Arc<dyn TimelineStore>;

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// An in-memory store for testing the HTTP layer without a database.
    pub struct FakeStore {
        pub items: Vec<TimelineItem>,
        pub fail: bool,
    }

    impl TimelineStore for FakeStore {
        fn timeline(
            &self,
            limit: u32,
            before_id: Option<i64>,
            kind: Option<&str>,
        ) -> Result<Vec<TimelineItem>, String> {
            if self.fail {
                return Err("store is broken".into());
            }
            Ok(self
                .items
                .iter()
                .filter(|i| before_id.is_none_or(|b| i.id < b))
                .filter(|i| kind.is_none_or(|k| i.kind == k))
                .take(limit as usize)
                .cloned()
                .collect())
        }

        fn stats(&self) -> Result<TimelineStats, String> {
            if self.fail {
                return Err("store is broken".into());
            }
            Ok(TimelineStats { events: self.items.len() as i64, files: self.items.len() as i64 })
        }
    }
}

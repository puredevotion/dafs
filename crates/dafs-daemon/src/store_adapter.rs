//! Bridges the SQLite store to the API's [`TimelineStore`] trait.
//!
//! Lives in the daemon rather than in either crate it joins: `dafs-api` should
//! not depend on a particular storage engine, and `dafs-store` should not know
//! about the HTTP layer's DTOs. The binary is the place that knows both.
//!
//! # Concurrency
//!
//! One connection behind a `Mutex`. See `dafs_api::timeline` for why this is not
//! a pool. Every caller reaches this through `spawn_blocking`, so the lock is
//! never held across an await point — the `Mutex` here is `std`'s, deliberately,
//! because an async mutex would invite exactly that mistake.

use std::sync::Mutex;

use dafs_api::{TimelineItem, TimelineStats, TimelineStore};
use dafs_store::events::{EventKind, TimelineQuery};
use rusqlite::Connection;

/// The shared metadata connection.
pub struct SqliteTimeline {
    conn: Mutex<Connection>,
}

impl SqliteTimeline {
    pub fn new(conn: Connection) -> Self {
        Self { conn: Mutex::new(conn) }
    }

    /// Run `f` against the connection.
    ///
    /// Recovers from lock poisoning rather than propagating it: a panic in some
    /// earlier query says nothing about whether *this* one can run, and the
    /// connection itself is not left in a broken state by an unwound rusqlite
    /// call. Refusing every subsequent request because one panicked would turn a
    /// single failed query into a dead daemon.
    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T, String>) -> Result<T, String> {
        match self.conn.lock() {
            Ok(guard) => f(&guard),
            Err(poisoned) => {
                tracing::warn!("metadata connection mutex was poisoned; recovering");
                f(&poisoned.into_inner())
            }
        }
    }
}

impl TimelineStore for SqliteTimeline {
    fn timeline(
        &self,
        limit: u32,
        before_id: Option<i64>,
        kind: Option<&str>,
    ) -> Result<Vec<TimelineItem>, String> {
        // An unparseable kind reaching here would mean the handler's validation
        // was bypassed; treat it as no filter rather than inventing one, since
        // silently filtering on a kind that does not exist returns an empty page
        // that reads as "nothing happened".
        let kind = kind.and_then(EventKind::parse);

        let query = TimelineQuery { limit: Some(limit), before_id, kind };

        self.with_conn(|conn| {
            dafs_store::events::timeline(conn, &query)
                .map_err(|e| e.to_string())
                .map(|rows| rows.into_iter().map(to_dto).collect())
        })
    }

    fn stats(&self) -> Result<TimelineStats, String> {
        self.with_conn(|conn| {
            let events = dafs_store::events::count(conn).map_err(|e| e.to_string())?;
            let files = dafs_store::events::file_count(conn).map_err(|e| e.to_string())?;
            Ok(TimelineStats { events, files })
        })
    }
}

fn to_dto(entry: dafs_store::events::TimelineEntry) -> TimelineItem {
    TimelineItem {
        id: entry.id,
        path: entry.path,
        kind: entry.kind.as_str().to_string(),
        at_unix_ms: entry.at_unix_ms,
        size_bytes: entry.size_bytes,
        is_dir: entry.is_dir,
        previous_path: entry.previous_path,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use dafs_store::events::{EventKind as StoreKind, NewEvent, append};
    use dafs_store::paths::{Interner, ensure_dir_chain};

    use super::*;

    fn store_with_events() -> SqliteTimeline {
        let conn = dafs_store::open_in_memory().expect("open");
        let mut interner = Interner::new();
        let file =
            ensure_dir_chain(&conn, &mut interner, Path::new("/home/u/a.txt")).expect("file");

        append(&conn, &NewEvent::now(file, StoreKind::Created).at(1_000)).expect("append");
        append(&conn, &NewEvent::now(file, StoreKind::Modified).at(2_000)).expect("append");

        SqliteTimeline::new(conn)
    }

    #[test]
    fn timeline_maps_store_rows_to_dtos() {
        let store = store_with_events();
        let items = store.timeline(10, None, None).expect("timeline");

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, "modified", "most recent first");
        assert_eq!(items[0].path, "/home/u/a.txt");
    }

    #[test]
    fn filtering_by_kind_reaches_the_store() {
        let store = store_with_events();
        let items = store.timeline(10, None, Some("created")).expect("timeline");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "created");
    }

    /// An unknown kind must not silently filter everything out.
    #[test]
    fn an_unparseable_kind_is_treated_as_no_filter() {
        let store = store_with_events();
        let items = store.timeline(10, None, Some("exploded")).expect("timeline");
        assert_eq!(items.len(), 2, "an unknown kind silently emptied the timeline");
    }

    #[test]
    fn stats_counts_events_and_files() {
        let store = store_with_events();
        let stats = store.stats().expect("stats");
        assert_eq!(stats.events, 2);
    }

    /// A poisoned mutex must not permanently break the daemon.
    #[test]
    fn a_poisoned_lock_is_recovered() {
        let store = std::sync::Arc::new(store_with_events());

        let poisoner = std::sync::Arc::clone(&store);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.conn.lock().expect("lock");
            panic!("poisoning the mutex on purpose");
        })
        .join();

        // The next query must still work.
        let items = store.timeline(10, None, None).expect("timeline after poisoning");
        assert_eq!(items.len(), 2);
    }
}

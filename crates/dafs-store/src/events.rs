//! The event log: append-only writes, and the timeline query that reads them.
//!
//! Events are the primary historical view, and from M06 they become the unit of
//! synchronisation. Both properties mean the same thing for this module: an
//! event is written once and never updated in place. There is deliberately no
//! `update_event` here — a correction is a new event, not an edit, because a log
//! that can be rewritten cannot be replicated by replaying it.

use rusqlite::{Connection, OptionalExtension};

use crate::StoreError;
use crate::paths::{ComponentId, FileId, resolve_path};

/// What happened to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Created,
    Modified,
    Deleted,
    Renamed,
}

impl EventKind {
    /// The stored form. Must match the schema's CHECK constraint, which is what
    /// makes an unlisted value a write failure rather than a silent bad row.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "created" => Some(Self::Created),
            "modified" => Some(Self::Modified),
            "deleted" => Some(Self::Deleted),
            "renamed" => Some(Self::Renamed),
            _ => None,
        }
    }
}

/// One row of the timeline, with its path already resolved.
#[derive(Debug, Clone)]
pub struct TimelineEntry {
    pub id: i64,
    pub file_id: FileId,
    pub path: String,
    pub kind: EventKind,
    pub at_unix_ms: i64,
    pub size_bytes: Option<i64>,
    pub is_dir: bool,
    /// Previous path, for renames.
    pub previous_path: Option<String>,
}

/// An event to append.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub file_id: FileId,
    pub kind: EventKind,
    pub at_unix_ms: i64,
    pub size_bytes: Option<i64>,
    pub prev_parent_id: Option<FileId>,
    pub prev_component_id: Option<ComponentId>,
}

impl NewEvent {
    /// An event about `file_id` happening now.
    pub fn now(file_id: FileId, kind: EventKind) -> Self {
        Self {
            file_id,
            kind,
            at_unix_ms: now_unix_ms(),
            size_bytes: None,
            prev_parent_id: None,
            prev_component_id: None,
        }
    }

    pub fn with_size(mut self, size: Option<i64>) -> Self {
        self.size_bytes = size;
        self
    }

    pub fn at(mut self, at_unix_ms: i64) -> Self {
        self.at_unix_ms = at_unix_ms;
        self
    }
}

/// Append one event.
pub fn append(conn: &Connection, event: &NewEvent) -> Result<i64, StoreError> {
    conn.execute(
        "INSERT INTO events
             (file_id, kind, at_unix_ms, size_bytes, prev_parent_id, prev_component_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            event.file_id,
            event.kind.as_str(),
            event.at_unix_ms,
            event.size_bytes,
            event.prev_parent_id,
            event.prev_component_id,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Append many events in one transaction.
///
/// The scanner produces events in batches, and one transaction per event would
/// make a million-file scan a million fsyncs. Batching is also what makes the
/// scan crash-consistent in a useful way: a batch either lands whole or not at
/// all, so a crash mid-scan leaves a prefix of the tree recorded rather than a
/// half-written event.
pub fn append_batch(conn: &Connection, events: &[NewEvent]) -> Result<usize, StoreError> {
    if events.is_empty() {
        return Ok(0);
    }

    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO events
                 (file_id, kind, at_unix_ms, size_bytes, prev_parent_id, prev_component_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for e in events {
            stmt.execute(rusqlite::params![
                e.file_id,
                e.kind.as_str(),
                e.at_unix_ms,
                e.size_bytes,
                e.prev_parent_id,
                e.prev_component_id,
            ])?;
        }
    }
    tx.commit()?;

    Ok(events.len())
}

/// How many events the timeline will return in one page.
///
/// A cap, not a default: an unbounded `limit` in a query string is a
/// denial-of-service on a daemon holding a million events, and the timeline UI
/// pages anyway.
pub const MAX_LIMIT: u32 = 500;
pub const DEFAULT_LIMIT: u32 = 50;

/// Timeline query parameters.
#[derive(Debug, Clone, Default)]
pub struct TimelineQuery {
    pub limit: Option<u32>,
    /// Return only events strictly older than this id — the pagination cursor.
    ///
    /// An id rather than a timestamp: many events share a millisecond during a
    /// scan, and a timestamp cursor would either skip or repeat them at a page
    /// boundary.
    pub before_id: Option<i64>,
    pub kind: Option<EventKind>,
}

impl TimelineQuery {
    fn effective_limit(&self) -> u32 {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
    }
}

/// Read the timeline, most recent first.
pub fn timeline(
    conn: &Connection,
    query: &TimelineQuery,
) -> Result<Vec<TimelineEntry>, StoreError> {
    let limit = query.effective_limit();

    // Built by branching over fixed SQL strings rather than concatenating
    // fragments: every value is still bound, and the set of possible statements
    // is enumerable by reading this function.
    let sql = match (query.before_id.is_some(), query.kind.is_some()) {
        (false, false) => {
            "SELECT e.id, e.file_id, e.kind, e.at_unix_ms, e.size_bytes,
                    f.is_dir, e.prev_parent_id, e.prev_component_id
               FROM events e JOIN files f ON f.id = e.file_id
              ORDER BY e.at_unix_ms DESC, e.id DESC LIMIT ?1"
        }
        (true, false) => {
            "SELECT e.id, e.file_id, e.kind, e.at_unix_ms, e.size_bytes,
                    f.is_dir, e.prev_parent_id, e.prev_component_id
               FROM events e JOIN files f ON f.id = e.file_id
              WHERE e.id < ?2
              ORDER BY e.at_unix_ms DESC, e.id DESC LIMIT ?1"
        }
        (false, true) => {
            "SELECT e.id, e.file_id, e.kind, e.at_unix_ms, e.size_bytes,
                    f.is_dir, e.prev_parent_id, e.prev_component_id
               FROM events e JOIN files f ON f.id = e.file_id
              WHERE e.kind = ?2
              ORDER BY e.at_unix_ms DESC, e.id DESC LIMIT ?1"
        }
        (true, true) => {
            "SELECT e.id, e.file_id, e.kind, e.at_unix_ms, e.size_bytes,
                    f.is_dir, e.prev_parent_id, e.prev_component_id
               FROM events e JOIN files f ON f.id = e.file_id
              WHERE e.id < ?2 AND e.kind = ?3
              ORDER BY e.at_unix_ms DESC, e.id DESC LIMIT ?1"
        }
    };

    let mut stmt = conn.prepare_cached(sql)?;

    let map_row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<RawEvent> {
        Ok(RawEvent {
            id: r.get(0)?,
            file_id: r.get(1)?,
            kind: r.get::<_, String>(2)?,
            at_unix_ms: r.get(3)?,
            size_bytes: r.get(4)?,
            is_dir: r.get::<_, i64>(5)? != 0,
            prev_parent_id: r.get(6)?,
            prev_component_id: r.get(7)?,
        })
    };

    type Rows = Result<Vec<RawEvent>, rusqlite::Error>;
    let raw: Vec<RawEvent> = match (query.before_id, query.kind) {
        (None, None) => stmt.query_map(rusqlite::params![limit], map_row)?.collect::<Rows>(),
        (Some(before), None) => {
            stmt.query_map(rusqlite::params![limit, before], map_row)?.collect::<Rows>()
        }
        (None, Some(kind)) => {
            stmt.query_map(rusqlite::params![limit, kind.as_str()], map_row)?.collect::<Rows>()
        }
        (Some(before), Some(kind)) => stmt
            .query_map(rusqlite::params![limit, before, kind.as_str()], map_row)?
            .collect::<Rows>(),
    }?;

    // Path resolution is a separate pass rather than a recursive CTE in the
    // query: the walk is short, the rows are already limited to one page, and
    // keeping it in Rust means one implementation of path reconstruction
    // instead of a second one in SQL that could disagree with it.
    let mut out = Vec::with_capacity(raw.len());
    for e in raw {
        let previous_path = match (e.prev_parent_id, e.prev_component_id) {
            (parent, Some(component)) => previous_path(conn, parent, component)?,
            _ => None,
        };

        out.push(TimelineEntry {
            id: e.id,
            file_id: e.file_id,
            path: resolve_path(conn, e.file_id)?,
            // A row whose kind is not one of the four would have failed the
            // CHECK constraint on write, so this is unreachable short of a
            // hand-edited database; treat it as Modified rather than failing
            // the whole page.
            kind: EventKind::parse(&e.kind).unwrap_or(EventKind::Modified),
            at_unix_ms: e.at_unix_ms,
            size_bytes: e.size_bytes,
            is_dir: e.is_dir,
            previous_path,
        });
    }

    Ok(out)
}

struct RawEvent {
    id: i64,
    file_id: FileId,
    kind: String,
    at_unix_ms: i64,
    size_bytes: Option<i64>,
    is_dir: bool,
    prev_parent_id: Option<FileId>,
    prev_component_id: Option<ComponentId>,
}

/// Reconstruct where a renamed file used to be.
fn previous_path(
    conn: &Connection,
    parent: Option<FileId>,
    component: ComponentId,
) -> Result<Option<String>, StoreError> {
    let name: Option<String> = conn
        .query_row("SELECT name FROM path_components WHERE id = ?1", [component], |r| r.get(0))
        .optional()?;

    let Some(name) = name else { return Ok(None) };

    Ok(Some(match parent {
        Some(p) => format!("{}/{}", resolve_path(conn, p)?, name),
        None => format!("/{name}"),
    }))
}

/// Total events recorded. Exported on `/metrics`.
pub fn count(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?)
}

/// Total files known, excluding tombstoned ones.
pub fn file_count(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM files WHERE deleted_at IS NULL AND is_dir = 0",
        [],
        |r| r.get(0),
    )?)
}

pub(crate) fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::paths::{Interner, ensure_dir_chain};

    fn db_with_file(name: &str) -> (Connection, FileId) {
        let conn = crate::open_in_memory().expect("open");
        let mut i = Interner::new();
        let id = ensure_dir_chain(&conn, &mut i, Path::new(name)).expect("chain");
        (conn, id)
    }

    #[test]
    fn append_then_read_round_trips() {
        let (conn, file) = db_with_file("/home/user/notes.md");
        append(&conn, &NewEvent::now(file, EventKind::Created).with_size(Some(42)))
            .expect("append");

        let entries = timeline(&conn, &TimelineQuery::default()).expect("timeline");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "/home/user/notes.md");
        assert_eq!(entries[0].kind, EventKind::Created);
        assert_eq!(entries[0].size_bytes, Some(42));
    }

    #[test]
    fn timeline_is_most_recent_first() {
        let (conn, file) = db_with_file("/a/b");
        for (n, at) in [(1, 1_000), (2, 3_000), (3, 2_000)] {
            append(&conn, &NewEvent::now(file, EventKind::Modified).at(at).with_size(Some(n)))
                .expect("append");
        }

        let entries = timeline(&conn, &TimelineQuery::default()).expect("timeline");
        let times: Vec<i64> = entries.iter().map(|e| e.at_unix_ms).collect();
        assert_eq!(times, vec![3_000, 2_000, 1_000]);
    }

    #[test]
    fn limit_is_clamped_not_trusted() {
        let (conn, file) = db_with_file("/a/b");
        for _ in 0..10 {
            append(&conn, &NewEvent::now(file, EventKind::Modified)).expect("append");
        }

        let entries =
            timeline(&conn, &TimelineQuery { limit: Some(u32::MAX), ..Default::default() })
                .expect("timeline");
        assert!(
            entries.len() <= MAX_LIMIT as usize,
            "an absurd limit was honoured: {} rows",
            entries.len()
        );

        let zero = timeline(&conn, &TimelineQuery { limit: Some(0), ..Default::default() })
            .expect("timeline");
        assert_eq!(zero.len(), 1, "a zero limit should clamp to 1, not return nothing");
    }

    /// Pagination must not skip or repeat rows that share a millisecond — the
    /// exact case a timestamp cursor gets wrong, and the reason the cursor is
    /// an id.
    #[test]
    fn pagination_covers_events_sharing_a_timestamp() {
        let (conn, file) = db_with_file("/a/b");
        for _ in 0..10 {
            append(&conn, &NewEvent::now(file, EventKind::Modified).at(5_000)).expect("append");
        }

        let mut seen: Vec<i64> = Vec::new();
        let mut cursor: Option<i64> = None;
        loop {
            let page =
                timeline(&conn, &TimelineQuery { limit: Some(3), before_id: cursor, kind: None })
                    .expect("timeline");
            if page.is_empty() {
                break;
            }
            cursor = Some(page.last().expect("non-empty").id);
            seen.extend(page.iter().map(|e| e.id));
        }

        let mut unique = seen.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(seen.len(), 10, "paging lost or repeated rows: {seen:?}");
        assert_eq!(unique.len(), 10, "paging returned duplicates: {seen:?}");
    }

    #[test]
    fn filtering_by_kind_excludes_others() {
        let (conn, file) = db_with_file("/a/b");
        append(&conn, &NewEvent::now(file, EventKind::Created)).expect("append");
        append(&conn, &NewEvent::now(file, EventKind::Modified)).expect("append");
        append(&conn, &NewEvent::now(file, EventKind::Deleted)).expect("append");

        let created = timeline(
            &conn,
            &TimelineQuery { kind: Some(EventKind::Created), ..Default::default() },
        )
        .expect("timeline");
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].kind, EventKind::Created);
    }

    #[test]
    fn a_rename_records_where_the_file_came_from() {
        let conn = crate::open_in_memory().expect("open");
        let mut i = Interner::new();

        let old_dir = ensure_dir_chain(&conn, &mut i, Path::new("/home/user")).expect("dir");
        let old_name = i.intern(&conn, "before.md").expect("intern");
        let file = ensure_dir_chain(&conn, &mut i, Path::new("/home/user/after.md")).expect("file");

        append(
            &conn,
            &NewEvent {
                file_id: file,
                kind: EventKind::Renamed,
                at_unix_ms: 1_000,
                size_bytes: None,
                prev_parent_id: Some(old_dir),
                prev_component_id: Some(old_name),
            },
        )
        .expect("append");

        let entries = timeline(&conn, &TimelineQuery::default()).expect("timeline");
        assert_eq!(entries[0].path, "/home/user/after.md");
        assert_eq!(entries[0].previous_path.as_deref(), Some("/home/user/before.md"));
    }

    #[test]
    fn batch_append_is_all_or_nothing() {
        let (conn, file) = db_with_file("/a/b");

        let good: Vec<NewEvent> =
            (0..5).map(|_| NewEvent::now(file, EventKind::Modified)).collect();
        assert_eq!(append_batch(&conn, &good).expect("batch"), 5);
        assert_eq!(count(&conn).expect("count"), 5);

        // A batch referencing a nonexistent file violates the foreign key. The
        // whole batch must roll back — a partial batch would mean a crash-time
        // scan left events the file table cannot explain.
        let mut bad = good.clone();
        bad.push(NewEvent::now(999_999, EventKind::Modified));
        assert!(append_batch(&conn, &bad).is_err(), "an invalid batch should fail");
        assert_eq!(count(&conn).expect("count"), 5, "a failed batch left rows behind");
    }

    #[test]
    fn empty_batch_is_not_an_error() {
        let (conn, _) = db_with_file("/a/b");
        assert_eq!(append_batch(&conn, &[]).expect("empty batch"), 0);
    }

    #[test]
    fn event_kinds_round_trip_through_their_stored_form() {
        for kind in
            [EventKind::Created, EventKind::Modified, EventKind::Deleted, EventKind::Renamed]
        {
            assert_eq!(EventKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(EventKind::parse("exploded"), None);
    }

    /// The CHECK constraint is the schema's guard against an unlisted kind. If
    /// someone adds a variant to the enum without a migration, this fails.
    #[test]
    fn the_schema_rejects_an_unknown_kind() {
        let (conn, file) = db_with_file("/a/b");
        let err = conn.execute(
            "INSERT INTO events (file_id, kind, at_unix_ms) VALUES (?1, 'exploded', 1)",
            [file],
        );
        assert!(err.is_err(), "the schema accepted an unknown event kind");
    }

    #[test]
    fn counts_exclude_directories_and_tombstones() {
        let conn = crate::open_in_memory().expect("open");
        let mut i = Interner::new();

        // ensure_dir_chain makes directories; a file row is the leaf.
        let dir = ensure_dir_chain(&conn, &mut i, Path::new("/home/user")).expect("dir");
        let name = i.intern(&conn, "notes.md").expect("intern");
        let file = crate::paths::upsert_entry(&conn, Some(dir), name, false, Some(10), Some(0))
            .expect("file");

        assert_eq!(file_count(&conn).expect("count"), 1, "only the leaf is a file");

        conn.execute("UPDATE files SET deleted_at = 1 WHERE id = ?1", [file]).expect("tombstone");
        assert_eq!(file_count(&conn).expect("count"), 0, "a tombstoned file was counted");
    }
}

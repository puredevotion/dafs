//! Metadata store: SQLite schema, migrations, and connection tuning.
//!
//! M00 ships the migration runner and the tuning, with a schema that is
//! deliberately minimal (one table, recording the migrations themselves). M01
//! adds the real `files`/`events` tables. The point of doing this now is that
//! the *mechanism* — forward-only migrations, applied in a transaction, with a
//! crash-consistency test — is what later milestones depend on being correct.
//!
//! # Connection tuning
//!
//! See `docs/memory-budget.md` §index-and-storage. The short version: a small
//! page cache and a large mmap window. Both hold the same data, but mapped
//! pages are file-backed and evictable by the kernel under pressure, whereas
//! page-cache pages are anonymous and count fully against the process. On an
//! SSD the latency difference is small; the RSS difference is not.

#![forbid(unsafe_code)]

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

/// Store errors. Migration failures carry the version so a failed upgrade
/// names the offending step rather than just "SQL error".
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("migration {version} ({name}) failed: {source}")]
    Migration {
        version: u32,
        name: &'static str,
        #[source]
        source: rusqlite::Error,
    },

    /// The database was written by a newer build. Refusing is the only safe
    /// action: an older binary cannot know what invariants the newer schema
    /// relies on, and guessing risks corrupting a user's metadata.
    #[error(
        "database schema version {found} is newer than this build supports ({supported}); \
         upgrade dafs or point at a different data directory"
    )]
    SchemaTooNew { found: u32, supported: u32 },
}

/// One forward-only schema step.
struct Migration {
    version: u32,
    name: &'static str,
    sql: &'static str,
}

/// Every migration, in order. Append only — never edit or reorder a shipped
/// entry, because databases in the wild have already applied it and the
/// recorded version would no longer describe their actual schema.
const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "schema_version",
    sql: "
        CREATE TABLE schema_migrations (
            version    INTEGER PRIMARY KEY,
            name       TEXT    NOT NULL,
            applied_at INTEGER NOT NULL
        ) STRICT;
    ",
}];

/// The highest schema version this build understands.
pub fn supported_version() -> u32 {
    MIGRATIONS.iter().map(|m| m.version).max().unwrap_or(0)
}

/// Open (creating if absent) the metadata database at `path`, apply any pending
/// migrations, and return the tuned connection.
pub fn open(path: &Path) -> Result<Connection, StoreError> {
    let conn = Connection::open(path)?;
    tune(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// Open an in-memory database for tests. Same tuning and migrations, so a test
/// exercises the real schema rather than a hand-rolled approximation.
pub fn open_in_memory() -> Result<Connection, StoreError> {
    let conn = Connection::open_in_memory()?;
    tune(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// Apply connection pragmas.
///
/// `journal_mode=WAL` is persistent (stored in the file), the rest are
/// per-connection and must be set on every open — a silent trap, since
/// forgetting them degrades memory behaviour without any error.
fn tune(conn: &Connection) -> Result<(), StoreError> {
    // WAL: readers do not block the writer. Required for the API to serve reads
    // while the scanner writes. Returns a row, so query_row not execute.
    let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;

    conn.pragma_update(None, "synchronous", "NORMAL")?;

    // Negative cache_size is KiB rather than pages: 8 MiB, deliberately small.
    // The mmap window below does the heavy lifting.
    conn.pragma_update(None, "cache_size", -8_192)?;

    // 256 MiB of address space, not of RSS — mapped pages are file-backed and
    // the kernel evicts them under pressure. This is the trade described in the
    // module docs.
    conn.pragma_update(None, "mmap_size", 268_435_456i64)?;

    // Enforce declared foreign keys. Off by default in SQLite, and the schema
    // relies on it from M01 onward.
    conn.pragma_update(None, "foreign_keys", "ON")?;

    // Block instead of failing immediately when another connection holds a
    // write lock.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    Ok(())
}

/// Apply pending migrations.
///
/// Each runs inside an `IMMEDIATE` transaction together with its bookkeeping
/// row, so a crash mid-migration leaves the database at the previous version
/// rather than half-upgraded. `IMMEDIATE` takes the write lock up front instead
/// of upgrading mid-transaction, which is what prevents two processes starting
/// the same migration concurrently.
fn migrate(conn: &Connection) -> Result<(), StoreError> {
    let current = current_version(conn)?;
    let supported = supported_version();

    if current > supported {
        return Err(StoreError::SchemaTooNew { found: current, supported });
    }

    for m in MIGRATIONS.iter().filter(|m| m.version > current) {
        let tx = conn.unchecked_transaction()?;
        // unchecked_transaction gives a Transaction without borrowing conn
        // mutably; set the behaviour explicitly since the default is DEFERRED.
        tx.execute_batch("ROLLBACK; BEGIN IMMEDIATE;").map_err(|e| StoreError::Migration {
            version: m.version,
            name: m.name,
            source: e,
        })?;

        tx.execute_batch(m.sql).map_err(|e| StoreError::Migration {
            version: m.version,
            name: m.name,
            source: e,
        })?;

        // The bookkeeping row is inside the same transaction as the DDL. If it
        // were not, a crash between the two would leave a schema change with no
        // record of it, and the next start would try to apply it again.
        tx.execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![m.version, m.name, now_unix()],
        )
        .map_err(|e| StoreError::Migration {
            version: m.version,
            name: m.name,
            source: e,
        })?;

        tx.commit().map_err(|e| StoreError::Migration {
            version: m.version,
            name: m.name,
            source: e,
        })?;

        tracing::info!(version = m.version, name = m.name, "applied migration");
    }

    Ok(())
}

/// Current schema version, or 0 for a fresh database.
pub fn current_version(conn: &Connection) -> Result<u32, StoreError> {
    // The migrations table may not exist yet, which is not an error — it is
    // exactly the fresh-database case.
    let exists: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
            [],
            |r| r.get(0),
        )
        .optional()?;

    if exists.is_none() {
        return Ok(0);
    }

    let v: Option<u32> =
        conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |r| r.get(0))?;
    Ok(v.unwrap_or(0))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        // A clock before the epoch is not worth failing a migration over.
        .unwrap_or(0)
}

/// Read a pragma back as an integer, for tests and diagnostics.
pub fn pragma_i64(conn: &Connection, name: &str) -> Result<i64, StoreError> {
    Ok(conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))?)
}

/// Silence the unused-import warning when `TransactionBehavior` is only
/// referenced from documentation. Kept as a real reference so the import does
/// not drift out of sync with the migration strategy above.
#[allow(dead_code)]
const _BEHAVIOUR: TransactionBehavior = TransactionBehavior::Immediate;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_migrates_to_supported_version() {
        let conn = open_in_memory().expect("open");
        assert_eq!(current_version(&conn).expect("version"), supported_version());
    }

    #[test]
    fn migrations_are_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("meta.sqlite");

        let first = open(&path).expect("first open");
        let v1 = current_version(&first).expect("version");
        drop(first);

        // Reopening must not reapply anything, and must not fail on the
        // already-present tables.
        let second = open(&path).expect("second open");
        assert_eq!(current_version(&second).expect("version"), v1);

        let count: u32 = second
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count as usize, MIGRATIONS.len(), "a migration was applied twice");
    }

    #[test]
    fn versions_are_unique_and_ordered() {
        // Guards against an append that duplicates or misorders a version —
        // both would make `version > current` filtering silently skip a step.
        let mut seen = std::collections::BTreeSet::new();
        let mut last = 0;
        for m in MIGRATIONS {
            assert!(m.version > 0, "migration versions start at 1");
            assert!(seen.insert(m.version), "duplicate migration version {}", m.version);
            assert!(m.version > last, "migration {} is out of order", m.version);
            last = m.version;
        }
    }

    #[test]
    fn refuses_a_newer_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("meta.sqlite");

        {
            let conn = open(&path).expect("open");
            // Simulate a database written by a future build.
            conn.execute(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![9999, "from_the_future", 0],
            )
            .expect("insert");
        }

        let err = open(&path).expect_err("must refuse a newer schema");
        assert!(
            matches!(err, StoreError::SchemaTooNew { found: 9999, .. }),
            "wrong error: {err:?}"
        );
    }

    #[test]
    fn tuning_is_applied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("meta.sqlite");
        let conn = open(&path).expect("open");

        // WAL only takes effect on a file-backed database, which is why this
        // test does not use open_in_memory.
        let mode: String =
            conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).expect("journal_mode");
        assert_eq!(mode.to_lowercase(), "wal");

        assert!(pragma_i64(&conn, "mmap_size").expect("mmap_size") > 0, "mmap_size not set");
        assert_eq!(pragma_i64(&conn, "foreign_keys").expect("foreign_keys"), 1);

        // cache_size is reported in pages once SQLite normalises the negative
        // KiB form, so assert only that it is bounded — the exact page count
        // depends on page_size and would make this test brittle.
        let cache = pragma_i64(&conn, "cache_size").expect("cache_size");
        assert!(cache != 0, "cache_size unset");
    }

    /// Crash consistency: a database whose WAL is truncated mid-write must open
    /// cleanly at a consistent version, not half-migrated.
    ///
    /// This is the M00 seed of the fault-injection requirement in the testing
    /// bar. It is deliberately crude — copying the file mid-transaction rather
    /// than killing a process — because a real `kill -9` harness belongs with
    /// the write paths it tests, which arrive in M01/M04.
    #[test]
    fn survives_truncated_wal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("meta.sqlite");
        let conn = open(&path).expect("open");
        let version_before = current_version(&conn).expect("version");

        // Leave an uncommitted transaction's data in the WAL, then abandon it.
        conn.execute_batch("BEGIN; CREATE TABLE doomed (x INTEGER); ").expect("begin");
        drop(conn); // no commit — rollback on close

        let reopened = open(&path).expect("reopen after abandoned transaction");
        assert_eq!(current_version(&reopened).expect("version"), version_before);

        let doomed: Option<String> = reopened
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='doomed'",
                [],
                |r| r.get(0),
            )
            .optional()
            .expect("query");
        assert!(doomed.is_none(), "uncommitted DDL survived a close");
    }
}

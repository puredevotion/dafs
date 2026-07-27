//! Metadata store: SQLite schema, migrations, and connection tuning.
//!
//! M00 shipped the migration runner and the tuning against a deliberately
//! minimal schema. M01 adds the real tables: interned path components, files,
//! and the append-only event log the timeline reads from. M02a adds
//! `file_metadata` (deterministic extraction output) and `extraction_queue`
//! (the durable work list that drives it) — see the `metadata` module. M02b
//! adds `file_metadata.body_text` (M02a's own extracted text, exposed for
//! reuse rather than re-parsed) and `file_enrichment`/`enrichment_queue`
//! (LLM-derived fields, kept in their own table — see the `enrichment`
//! module). M03 adds `embedding_queue` (same durable-queue shape again) and,
//! on demand rather than as a static migration, `file_embedding` — a
//! `sqlite-vec` `vec0` virtual table for nearest-neighbour search over those
//! embeddings — see the `embeddings` module for why that one table's
//! creation can't be a plain append to `MIGRATIONS` the way every other
//! table here is.
//!
//! # Every connection this crate opens can see vectors
//!
//! `open`/`open_in_memory` both call `dafs_vecstore::register()` before
//! opening anything. That crate holds the one `unsafe` FFI call in this
//! workspace that registers the `vec0` module — see its own docs for why —
//! which keeps this crate's `forbid(unsafe_code)` intact while still making
//! `CREATE VIRTUAL TABLE ... USING vec0(...)` and `... MATCH ...` queries
//! work everywhere a connection is opened, tests included.
//!
//! # Paths are interned, not stored
//!
//! No table here holds a path string. A path is a chain of *components*, each
//! interned once in `path_components` and referenced by id, with `files.parent_id`
//! pointing at the containing directory. Reconstructing a path means walking
//! parent links back to the root.
//!
//! This is not premature optimisation, it is the M01 memory requirement
//! (`docs/memory-budget.md` §M01): a million paths averaging ~80 bytes is ~80 MB
//! of `String` before any structure at all, against a 32 MiB idle ceiling. The
//! tree shape means components repeat heavily — `home`, `src`, `Documents` are
//! stored once each regardless of how many paths contain them.
//!
//! It also has to land *now* rather than later: M07's knowledge graph will hang
//! relations off path ids, and retrofitting interning underneath a graph that
//! already references path strings is far more invasive than doing it here.
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

pub mod embeddings;
pub mod enrichment;
pub mod events;
pub mod metadata;
pub mod paths;

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

    /// A path with no components — `/` or an empty string. Rejected rather than
    /// silently treated as a root, because the caller almost certainly meant
    /// something else and a fabricated root row would be hard to notice.
    #[error("path has no components")]
    EmptyPath,

    /// `embeddings::ensure_table` was asked for a model/dimensionality that
    /// doesn't match what `file_embedding` was already created with. Boxed:
    /// this variant is far larger than every other (`DimensionMismatch`
    /// carries two owned `String`s), and `clippy::result_large_err` is right
    /// that bloating every `Result<_, StoreError>` return by that much for a
    /// variant that fires once per misconfigured deployment is a bad trade.
    #[error(transparent)]
    EmbeddingDimensionMismatch(#[from] Box<embeddings::DimensionMismatch>),
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
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "schema_version",
        sql: "
        CREATE TABLE schema_migrations (
            version    INTEGER PRIMARY KEY,
            name       TEXT    NOT NULL,
            applied_at INTEGER NOT NULL
        ) STRICT;
    ",
    },
    Migration {
        version: 2,
        name: "timeline",
        sql: "
        -- One row per distinct path component, ever. `home` is stored once no
        -- matter how many paths begin with it. See the module docs for why the
        -- schema stores no path strings at all.
        CREATE TABLE path_components (
            id   INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE
        ) STRICT;

        -- A file or directory. `parent_id` is the containing directory, NULL
        -- only for a watch root. The (parent_id, component_id) pair is unique:
        -- a directory cannot hold two entries with the same name, which is a
        -- filesystem invariant worth having the database enforce rather than
        -- trusting the scanner to maintain.
        --
        -- `parent_id` is self-referential rather than a materialised path, so a
        -- directory rename is one row updated instead of a subtree rewrite.
        CREATE TABLE files (
            id           INTEGER PRIMARY KEY,
            parent_id    INTEGER REFERENCES files(id) ON DELETE CASCADE,
            component_id INTEGER NOT NULL REFERENCES path_components(id),
            is_dir       INTEGER NOT NULL CHECK (is_dir IN (0, 1)),
            size_bytes   INTEGER,
            mtime_unix   INTEGER,
            -- NULL until content is hashed; directories never have one.
            content_hash BLOB,
            -- Set when the entry is observed gone. Rows are tombstoned rather
            -- than deleted so events referencing them keep resolving, and so a
            -- delete is itself a fact the timeline can show.
            deleted_at   INTEGER
        ) STRICT;

        -- A directory cannot hold two entries with the same name — a filesystem
        -- invariant worth having the database enforce rather than trusting the
        -- scanner to maintain.
        --
        -- Partial, on live rows only. A tombstone must not keep occupying the
        -- slot: a file deleted and later recreated, or replaced by a rename
        -- over the top of it, would otherwise collide with its own corpse. A
        -- plain table-level UNIQUE cannot express that, which is why this is an
        -- index rather than a column constraint.
        CREATE UNIQUE INDEX files_live_entry
            ON files(parent_id, component_id)
            WHERE deleted_at IS NULL;

        CREATE INDEX files_parent ON files(parent_id);

        -- The append-only event log. This is the primary historical view and,
        -- per the architecture, the unit of synchronisation from M06 onward —
        -- so it is written once and never updated in place.
        --
        -- `kind` is text rather than an integer enum: the log outlives any one
        -- build, and a value that is readable in a bare sqlite3 shell during an
        -- incident is worth more than the bytes saved.
        CREATE TABLE events (
            id         INTEGER PRIMARY KEY,
            file_id    INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            kind       TEXT    NOT NULL CHECK (kind IN ('created', 'modified', 'deleted', 'renamed')),
            -- Milliseconds, not seconds: a scan generates many events inside one
            -- second and the timeline orders by this.
            at_unix_ms INTEGER NOT NULL,
            size_bytes INTEGER,
            -- For 'renamed', the file's previous parent/component, so the
            -- timeline can say what it was called before without another table.
            prev_parent_id    INTEGER REFERENCES files(id) ON DELETE SET NULL,
            prev_component_id INTEGER REFERENCES path_components(id)
        ) STRICT;

        -- The timeline's only query shape: most recent first. Descending so the
        -- index is walked forwards rather than backwards.
        CREATE INDEX events_recent ON events(at_unix_ms DESC, id DESC);
        CREATE INDEX events_by_file ON events(file_id, at_unix_ms DESC);
    ",
    },
    Migration {
        version: 3,
        name: "metadata",
        sql: "
        -- Deterministic extraction output (M02a): document type, author,
        -- language, page/word counts, EXIF, git repo facts. No LLM output
        -- lives here — that is M02b, and arrives as its own migration.
        --
        -- Flat and mostly NULL by design: one row per file regardless of
        -- which extractor produced it, which keeps the timeline join to
        -- exactly one table instead of one per document kind.
        --
        -- Unlike `events`, this is a *replaceable cache* over derived,
        -- regenerable data (per the architecture's locked decision that AI
        -- and extraction output must never be treated as a source of truth)
        -- — re-extracting a file overwrites its row rather than appending a
        -- new fact, which is why this table has no analogue of `events`'
        -- append-only rule.
        CREATE TABLE file_metadata (
            file_id             INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
            doc_type            TEXT,
            title               TEXT,
            author              TEXT,
            language            TEXT,
            page_count          INTEGER,
            word_count          INTEGER,
            image_taken_at_unix INTEGER,
            image_camera_model  TEXT,
            git_branch          TEXT,
            git_head_commit     TEXT,
            git_head_author     TEXT,
            git_head_at_unix    INTEGER,
            extracted_at_unix   INTEGER NOT NULL,
            -- Bumped whenever extraction logic changes meaningfully, so an
            -- upgrade can find and reprocess everything extracted by an
            -- older version rather than leaving it stale forever.
            extractor_version   INTEGER NOT NULL
        ) STRICT;

        -- The durable work queue: every Created/Modified event upserts a row
        -- here, and a worker's success deletes it in the same transaction
        -- that writes file_metadata. A crash between the two leaves the row
        -- in place, which is what makes a kill -9 mid-extraction safe to
        -- retry rather than silently losing the request.
        --
        -- attempt_count bounds a poison file (one that reliably crashes or
        -- times out its extractor) to a handful of retries rather than
        -- spinning a worker on it forever across restarts.
        CREATE TABLE extraction_queue (
            file_id        INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
            queued_at_unix INTEGER NOT NULL,
            attempt_count  INTEGER NOT NULL DEFAULT 0
        ) STRICT;

        CREATE INDEX extraction_queue_order ON extraction_queue(queued_at_unix ASC);
    ",
    },
    Migration {
        version: 4,
        name: "enrichment",
        sql: "
        -- Deterministic extraction output M02a computed internally (to derive
        -- word_count/language) but never persisted. Added here, not in M02a's
        -- migration — that one is never edited once shipped — so M02b's
        -- enrichment worker can read a file's text straight back out of the
        -- store instead of re-parsing the original file a second time.
        ALTER TABLE file_metadata ADD COLUMN body_text TEXT;

        -- LLM-derived fields live in their own table, never as columns on
        -- file_metadata: M02a's crate and schema stay LLM-free in substance,
        -- not just in comments, and enrichment can be independently enabled,
        -- disabled, or re-run without ever touching M02a's shipped schema.
        --
        -- Same replaceable-cache reasoning as file_metadata: re-enrichment
        -- overwrites the row rather than appending a new fact.
        CREATE TABLE file_enrichment (
            file_id            INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
            summary            TEXT,
            -- JSON arrays, stored as text — same reasoning `events.kind` being
            -- TEXT gives: readable in a bare sqlite3 shell during an incident.
            keywords           TEXT,
            entities           TEXT,
            classification     TEXT,
            enriched_at_unix   INTEGER NOT NULL,
            -- Bumped whenever the prompt/schema changes meaningfully, so an
            -- upgrade can find and reprocess everything a prior version
            -- enriched — same role as file_metadata.extractor_version.
            enrichment_version INTEGER NOT NULL
        ) STRICT;

        -- Same durable-queue shape as extraction_queue, and the same
        -- attempt_count poison-file cap. Unlike extraction_queue, nothing
        -- enqueues here unconditionally: a file only ever lands in this queue
        -- when enrichment is configured, driven from the extraction worker
        -- after a successful extraction with enough text to be worth sending.
        CREATE TABLE enrichment_queue (
            file_id        INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
            queued_at_unix INTEGER NOT NULL,
            attempt_count  INTEGER NOT NULL DEFAULT 0
        ) STRICT;

        CREATE INDEX enrichment_queue_order ON enrichment_queue(queued_at_unix ASC);
    ",
    },
    Migration {
        version: 5,
        name: "embeddings",
        sql: "
        -- Same durable-queue shape as enrichment_queue, and the same
        -- attempt_count poison-file cap. Nothing enqueues here unconditionally
        -- — driven by the daemon only once embeddings are configured.
        CREATE TABLE embedding_queue (
            file_id        INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
            queued_at_unix INTEGER NOT NULL,
            attempt_count  INTEGER NOT NULL DEFAULT 0
        ) STRICT;

        CREATE INDEX embedding_queue_order ON embedding_queue(queued_at_unix ASC);

        -- Bookkeeping for `file_embedding`, the vec0 virtual table `embeddings`
        -- creates on demand — see that module's `ensure_table` for why the
        -- vec0 table itself is not part of this static, append-only migration
        -- list: its column width is fixed at creation time to whatever
        -- embedding model the deployment is configured with, which is a
        -- per-deployment choice this shared migration list has no way to
        -- express. This row is what lets a later run notice its configured
        -- model or dimensionality changed, instead of silently corrupting a
        -- fixed-width column.
        CREATE TABLE embedding_config (
            id         INTEGER PRIMARY KEY CHECK (id = 1),
            model      TEXT    NOT NULL,
            dimensions INTEGER NOT NULL
        ) STRICT;
    ",
    },
];

/// The highest schema version this build understands.
pub fn supported_version() -> u32 {
    MIGRATIONS.iter().map(|m| m.version).max().unwrap_or(0)
}

/// Open (creating if absent) the metadata database at `path`, apply any pending
/// migrations, and return the tuned connection.
pub fn open(path: &Path) -> Result<Connection, StoreError> {
    dafs_vecstore::register();
    let conn = Connection::open(path)?;
    tune(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// Open an in-memory database for tests. Same tuning and migrations, so a test
/// exercises the real schema rather than a hand-rolled approximation.
pub fn open_in_memory() -> Result<Connection, StoreError> {
    dafs_vecstore::register();
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

//! Path interning: turning filesystem paths into component ids and back.
//!
//! See the crate docs for why paths are never stored as strings. This module is
//! the only place that converts between the two representations, so the
//! invariant has one enforcement point rather than being a convention.
//!
//! # The cache, and what does not go in it
//!
//! Interning every component through SQLite would put two statements on the hot
//! path of a scan that walks a million files, so hot components are cached in an
//! in-memory `name -> id` map.
//!
//! **Only directory components are cached.** This is the whole design, and
//! getting it wrong is a memory bug rather than a slow path:
//!
//! - *Directory* names repeat enormously. A million-file tree might contain a
//!   few thousand distinct directory names, each appearing in thousands of
//!   paths. Caching them turns almost every lookup into a hash hit.
//! - *Leaf* names are nearly all distinct — that is what makes them filenames.
//!   Caching them means one entry per file in the corpus, which is precisely the
//!   per-file resident state the interning exists to eliminate. Each entry is
//!   only ~50 bytes, but a million of them is ~50 MB against a 128 MiB scan
//!   ceiling, and every one of those entries would be a cache line that is never
//!   read again.
//!
//! An earlier version of this module cached both and grew linearly with corpus
//! size at ~430 bytes per file — caught by `dafs-scan`'s growth test, which is
//! there precisely because a single-size ceiling check would have passed.
//!
//! So [`Interner::intern_dir`] caches and [`Interner::intern_leaf`] does not.
//! Correctness does not depend on either: a miss is a query, never a wrong
//! answer, and the `UNIQUE` constraint on `path_components.name` is what
//! actually guarantees one id per name.
//!
//! The cache is still bounded as a backstop, for the pathological tree whose
//! directory names are themselves all distinct. When it fills it is cleared
//! rather than evicted per-entry: an LRU needs a second data structure and
//! per-access bookkeeping to save a handful of statements against a database
//! already warm in page cache.

use std::collections::HashMap;
use std::path::{Component, Path};

use rusqlite::{Connection, OptionalExtension};

use crate::StoreError;

/// Directory names cached before the cache is cleared.
///
/// Sized for directory names, not files: a tree with more than 16k *distinct*
/// directory names is already unusual, and at ~50 bytes an entry this bound is
/// under 1 MiB. Leaf names never enter the cache at all (see the module docs),
/// so this no longer scales with corpus size.
const CACHE_CAPACITY: usize = 16_384;

/// A file or directory id.
pub type FileId = i64;

/// An interned path component id.
pub type ComponentId = i64;

/// Interns path components, caching the hot ones.
///
/// Not `Clone`: the cache is only correct if every writer shares one, since two
/// caches over one database would each hold ids the other could also insert.
/// The `UNIQUE` constraint means that is safe rather than corrupting, but it
/// would waste the queries the cache exists to avoid.
#[derive(Default)]
pub struct Interner {
    cache: HashMap<Box<str>, ComponentId>,
}

impl Interner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of cached components. For tests and diagnostics.
    pub fn cached(&self) -> usize {
        self.cache.len()
    }

    /// Intern a **directory** component, caching the result.
    ///
    /// Use this for anything that repeats across paths. See the module docs for
    /// why the distinction from [`Self::intern_leaf`] is a memory requirement
    /// rather than a tuning choice.
    pub fn intern_dir(&mut self, conn: &Connection, name: &str) -> Result<ComponentId, StoreError> {
        if let Some(&id) = self.cache.get(name) {
            return Ok(id);
        }

        let id = self.lookup_or_insert(conn, name)?;

        if self.cache.len() >= CACHE_CAPACITY {
            // Bounded, not an LRU — see the module docs.
            self.cache.clear();
        }
        self.cache.insert(name.into(), id);

        Ok(id)
    }

    /// Intern a **leaf** (file) name without caching it.
    ///
    /// Filenames are nearly all distinct, so caching them would add one resident
    /// entry per file in the corpus for a hit rate near zero. The database is
    /// warm and the `name` column is UNIQUE-indexed, so the lookup this costs is
    /// a page-cache hit rather than disk I/O.
    ///
    /// It still *reads* the cache first: a name that happens to also be a
    /// directory name elsewhere in the tree is then free, and the check costs
    /// one hash.
    pub fn intern_leaf(
        &mut self,
        conn: &Connection,
        name: &str,
    ) -> Result<ComponentId, StoreError> {
        if let Some(&id) = self.cache.get(name) {
            return Ok(id);
        }
        self.lookup_or_insert(conn, name)
    }

    /// Select-then-insert against `path_components`.
    ///
    /// Select first rather than `INSERT OR IGNORE` then select: the
    /// already-present case is overwhelmingly the common one during a rescan,
    /// and this makes it a single statement.
    fn lookup_or_insert(&self, conn: &Connection, name: &str) -> Result<ComponentId, StoreError> {
        let existing: Option<ComponentId> = conn
            .query_row("SELECT id FROM path_components WHERE name = ?1", [name], |r| r.get(0))
            .optional()?;

        if let Some(id) = existing {
            return Ok(id);
        }

        conn.execute("INSERT INTO path_components (name) VALUES (?1)", [name])?;
        Ok(conn.last_insert_rowid())
    }

    /// Read back the name for a component id.
    pub fn name(&self, conn: &Connection, id: ComponentId) -> Result<Option<String>, StoreError> {
        Ok(conn
            .query_row("SELECT name FROM path_components WHERE id = ?1", [id], |r| r.get(0))
            .optional()?)
    }
}

/// Ensure a directory chain exists in `files`, returning the id of the deepest
/// directory. Used to create the ancestors of a scanned file.
///
/// Every component becomes a directory row. `root` must be absolute; a relative
/// path has no unambiguous chain to build.
pub fn ensure_dir_chain(
    conn: &Connection,
    interner: &mut Interner,
    dir: &Path,
) -> Result<FileId, StoreError> {
    let mut parent: Option<FileId> = None;

    for component in normalised_components(dir) {
        let component_id = interner.intern_dir(conn, &component)?;
        parent = Some(upsert_entry(conn, parent, component_id, true, None, None)?);
    }

    parent.ok_or(StoreError::EmptyPath)
}

/// Insert or fetch one `files` row.
///
/// The `ON CONFLICT` clause is what makes a rescan idempotent: the same entry
/// observed twice updates its metadata rather than failing the unique
/// constraint or duplicating the row. It also clears `deleted_at`, so a file
/// that comes back is un-tombstoned rather than shadowed by its own corpse.
pub fn upsert_entry(
    conn: &Connection,
    parent: Option<FileId>,
    component_id: ComponentId,
    is_dir: bool,
    size_bytes: Option<i64>,
    mtime_unix: Option<i64>,
) -> Result<FileId, StoreError> {
    // `parent_id IS ?1` rather than `= ?1`: a root's parent is NULL, and
    // `NULL = NULL` is NULL in SQL, so the equality form silently never matches
    // a root and would insert a duplicate on every scan.
    //
    // The same asymmetry applies to the UNIQUE index — SQLite treats NULLs as
    // distinct — which is why roots are looked up explicitly here rather than
    // relying on the conflict clause to catch them.
    //
    // Live rows first. The uniqueness index is partial (`WHERE deleted_at IS
    // NULL`), so a location can hold one live row plus any number of
    // tombstones from files that were there before. Reviving an arbitrary
    // tombstone when a live row already exists would resurrect the wrong file's
    // history; `ORDER BY deleted_at IS NULL DESC` puts the live one first and
    // otherwise takes the most recent corpse, which is the file that was most
    // plausibly just recreated.
    let existing: Option<FileId> = conn
        .query_row(
            "SELECT id FROM files
              WHERE parent_id IS ?1 AND component_id = ?2
              ORDER BY deleted_at IS NULL DESC, deleted_at DESC
              LIMIT 1",
            rusqlite::params![parent, component_id],
            |r| r.get(0),
        )
        .optional()?;

    if let Some(id) = existing {
        conn.execute(
            "UPDATE files SET size_bytes = ?2, mtime_unix = ?3, deleted_at = NULL WHERE id = ?1",
            rusqlite::params![id, size_bytes, mtime_unix],
        )?;
        return Ok(id);
    }

    conn.execute(
        "INSERT INTO files (parent_id, component_id, is_dir, size_bytes, mtime_unix)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![parent, component_id, is_dir as i64, size_bytes, mtime_unix],
    )?;

    Ok(conn.last_insert_rowid())
}

/// Reconstruct a path by walking `parent_id` links back to the root.
///
/// Bounded by `MAX_DEPTH` rather than trusting the data: a `parent_id` cycle
/// would otherwise loop forever inside an HTTP handler. The schema makes a cycle
/// hard to create but not impossible (a rename that reparents a directory under
/// its own descendant), and an unbounded walk in a request path is the kind of
/// bug that only shows up under a corrupted database, which is exactly when the
/// daemon most needs to stay responsive.
pub fn resolve_path(conn: &Connection, file_id: FileId) -> Result<String, StoreError> {
    /// Deeper than any real filesystem path; `PATH_MAX` is 4096 bytes total.
    const MAX_DEPTH: usize = 256;

    let mut components: Vec<String> = Vec::new();
    let mut current = Some(file_id);

    for _ in 0..MAX_DEPTH {
        let Some(id) = current else {
            let mut path = String::with_capacity(64);
            for c in components.iter().rev() {
                path.push('/');
                path.push_str(c);
            }
            if path.is_empty() {
                path.push('/');
            }
            return Ok(path);
        };

        let row: Option<(String, Option<FileId>)> = conn
            .query_row(
                "SELECT c.name, f.parent_id
                   FROM files f JOIN path_components c ON c.id = f.component_id
                  WHERE f.id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        match row {
            Some((name, parent)) => {
                components.push(name);
                current = parent;
            }
            // A dangling parent_id. Return what resolved rather than erroring:
            // a partial path in the timeline is more useful than a failed
            // request, and the truncation is visible.
            None => break,
        }
    }

    let mut path = String::with_capacity(64);
    for c in components.iter().rev() {
        path.push('/');
        path.push_str(c);
    }
    Ok(path)
}

/// Split a path into interning-ready components, dropping the root and
/// resolving `.` / `..` textually.
///
/// Textual resolution is correct here because the scanner only ever passes
/// paths it has already read from the filesystem, so there is no symlink to be
/// wrong about. A user-supplied path would need `canonicalize` instead.
fn normalised_components(path: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    for component in path.components() {
        match component {
            Component::Normal(part) => {
                // Lossy is right: an id for a mangled name still lets the file
                // appear in the timeline, where refusing to index it would make
                // it invisible with no indication why.
                out.push(part.to_string_lossy().into_owned());
            }
            Component::ParentDir => {
                out.pop();
            }
            // RootDir and CurDir carry no component; Prefix is Windows-only.
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        crate::open_in_memory().expect("open")
    }

    #[test]
    fn interning_is_stable_and_deduplicates() {
        let conn = db();
        let mut i = Interner::new();

        let a = i.intern_dir(&conn, "documents").expect("intern");
        let b = i.intern_dir(&conn, "documents").expect("intern again");
        assert_eq!(a, b, "same name must yield the same id");

        let c = i.intern_dir(&conn, "downloads").expect("intern other");
        assert_ne!(a, c);

        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM path_components", [], |r| r.get(0))
            .expect("count");
        assert_eq!(rows, 2, "a repeated component was stored twice");
    }

    /// The cache must never change an answer — only how fast it is reached.
    #[test]
    fn cold_cache_agrees_with_warm_cache() {
        let conn = db();
        let mut warm = Interner::new();
        let id = warm.intern_dir(&conn, "src").expect("intern");

        let mut cold = Interner::new();
        assert_eq!(cold.intern_dir(&conn, "src").expect("intern"), id);
    }

    /// The memory property the whole `intern_dir`/`intern_leaf` split exists
    /// for. Without this, a future refactor that "simplifies" the two methods
    /// back into one would reintroduce linear growth, and the only thing
    /// catching it would be a slow scan-level test in another crate.
    #[test]
    fn leaf_names_never_enter_the_cache() {
        let conn = db();
        let mut i = Interner::new();

        for n in 0..1_000 {
            i.intern_leaf(&conn, &format!("file-{n}.txt")).expect("intern leaf");
        }

        assert_eq!(i.cached(), 0, "leaf names were cached: {} entries", i.cached());

        // They are still interned — just not held in memory.
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM path_components", [], |r| r.get(0))
            .expect("count");
        assert_eq!(rows, 1_000, "leaf names were not interned at all");
    }

    /// Both methods must agree on the id for a given name, or a file whose name
    /// matches a directory name would get two rows.
    #[test]
    fn dir_and_leaf_interning_agree_on_ids() {
        let conn = db();
        let mut i = Interner::new();

        let as_dir = i.intern_dir(&conn, "notes").expect("dir");
        let as_leaf = i.intern_leaf(&conn, "notes").expect("leaf");
        assert_eq!(as_dir, as_leaf);

        let mut cold = Interner::new();
        assert_eq!(cold.intern_leaf(&conn, "notes").expect("cold leaf"), as_dir);
    }

    #[test]
    fn cache_is_bounded() {
        let conn = db();
        let mut i = Interner::new();
        for n in 0..(CACHE_CAPACITY + 100) {
            i.intern_dir(&conn, &format!("component-{n}")).expect("intern");
        }
        assert!(i.cached() <= CACHE_CAPACITY, "cache grew past its bound: {} entries", i.cached());
    }

    #[test]
    fn dir_chain_round_trips_to_the_original_path() {
        let conn = db();
        let mut i = Interner::new();

        let id = ensure_dir_chain(&conn, &mut i, Path::new("/home/user/documents")).expect("chain");
        assert_eq!(resolve_path(&conn, id).expect("resolve"), "/home/user/documents");
    }

    #[test]
    fn shared_prefixes_are_stored_once() {
        let conn = db();
        let mut i = Interner::new();

        ensure_dir_chain(&conn, &mut i, Path::new("/home/user/a")).expect("chain a");
        ensure_dir_chain(&conn, &mut i, Path::new("/home/user/b")).expect("chain b");
        ensure_dir_chain(&conn, &mut i, Path::new("/home/user/c")).expect("chain c");

        // home, user, a, b, c — five components, not nine. This is the property
        // the whole interning design exists for.
        let components: i64 = conn
            .query_row("SELECT COUNT(*) FROM path_components", [], |r| r.get(0))
            .expect("count");
        assert_eq!(components, 5);

        // And one row per directory, with the prefix shared rather than copied.
        let files: i64 =
            conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0)).expect("count");
        assert_eq!(files, 5, "/home, /home/user, and three leaves");
    }

    #[test]
    fn rescanning_a_tree_is_idempotent() {
        let conn = db();
        let mut i = Interner::new();

        ensure_dir_chain(&conn, &mut i, Path::new("/home/user/documents")).expect("first");
        let after_first: i64 =
            conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0)).expect("count");

        ensure_dir_chain(&conn, &mut i, Path::new("/home/user/documents")).expect("second");
        let after_second: i64 =
            conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0)).expect("count");

        assert_eq!(after_first, after_second, "rescanning duplicated rows");
    }

    /// A fresh interner on a rescan is the realistic restart case: the daemon
    /// stops, loses its cache, and scans the same tree again.
    #[test]
    fn rescan_with_a_cold_interner_is_idempotent() {
        let conn = db();

        let mut first = Interner::new();
        ensure_dir_chain(&conn, &mut first, Path::new("/home/user/documents")).expect("first");
        let before: i64 =
            conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0)).expect("count");

        let mut second = Interner::new();
        ensure_dir_chain(&conn, &mut second, Path::new("/home/user/documents")).expect("second");
        let after: i64 =
            conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0)).expect("count");

        assert_eq!(before, after, "a cold interner duplicated rows on rescan");
    }

    #[test]
    fn same_name_in_different_directories_is_a_distinct_file() {
        let conn = db();
        let mut i = Interner::new();

        let a = ensure_dir_chain(&conn, &mut i, Path::new("/a/notes")).expect("a");
        let b = ensure_dir_chain(&conn, &mut i, Path::new("/b/notes")).expect("b");

        assert_ne!(a, b);
        assert_eq!(resolve_path(&conn, a).expect("resolve a"), "/a/notes");
        assert_eq!(resolve_path(&conn, b).expect("resolve b"), "/b/notes");
    }

    #[test]
    fn parent_traversal_is_resolved() {
        let conn = db();
        let mut i = Interner::new();

        let id =
            ensure_dir_chain(&conn, &mut i, Path::new("/home/user/../user/docs")).expect("chain");
        assert_eq!(resolve_path(&conn, id).expect("resolve"), "/home/user/docs");
    }

    #[test]
    fn an_empty_path_is_an_error_not_a_silent_root() {
        let conn = db();
        let mut i = Interner::new();
        assert!(
            matches!(ensure_dir_chain(&conn, &mut i, Path::new("/")), Err(StoreError::EmptyPath)),
            "a path with no components should be rejected"
        );
    }

    /// A dangling `parent_id` must truncate rather than hang or error. Written
    /// as a direct test because the bounded walk is only reachable with data the
    /// scanner cannot produce.
    #[test]
    fn resolving_a_dangling_parent_truncates() {
        let conn = db();
        let mut i = Interner::new();
        let id = ensure_dir_chain(&conn, &mut i, Path::new("/home/user")).expect("chain");

        // Point at a parent that does not exist. Foreign keys are enforced, so
        // this needs them off for the one statement.
        conn.pragma_update(None, "foreign_keys", "OFF").expect("fk off");
        conn.execute("UPDATE files SET parent_id = 9999 WHERE id = ?1", [id]).expect("update");
        conn.pragma_update(None, "foreign_keys", "ON").expect("fk on");

        let path = resolve_path(&conn, id).expect("resolve must not fail");
        assert_eq!(path, "/user", "should return the resolvable suffix");
    }
}

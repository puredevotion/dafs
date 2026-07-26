//! The filesystem observer: an initial scan, then a live watch.
//!
//! M01 is read-only by design. Nothing here opens a file for writing, moves
//! anything, or changes a single byte of the user's data — the observer reads
//! metadata and records what it saw. That is what makes this milestone safe to
//! run against a real home directory on day one, and it is a property worth
//! keeping deliberately rather than by accident, so it has its own test.
//!
//! # Memory
//!
//! The scan is the large-input, small-state problem from `docs/memory-budget.md`
//! §M01, and the 128 MiB scan-peak ceiling holds only if nothing here
//! accumulates per-file state. Two rules follow, and both are load-bearing:
//!
//! - **Batch, never collect.** The walker yields entries lazily and this module
//!   flushes every [`BATCH_SIZE`] events. Peak memory is one batch, *independent
//!   of corpus size* — which is what makes the ceiling hold at 10M files as well
//!   as at 1M.
//! - **No path strings retained.** Components are interned as they are seen (see
//!   `dafs_store::paths`) and the owned `PathBuf` from the walker is dropped at
//!   the end of each iteration.
//!
//! The invariant that matters is not "peak is under 128 MiB on the test corpus"
//! but "peak does not grow with corpus size". `tests/scan_memory.rs` asserts the
//! second, which is the one that actually generalises.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use dafs_store::events::{EventKind, NewEvent, now_unix_ms};
use dafs_store::paths::{FileId, Interner, ensure_dir_chain, upsert_entry};
use rusqlite::Connection;

pub mod watch;

/// Events accumulated before a flush.
///
/// 1024 events is on the order of 100 KiB — small against the ceiling, large
/// enough that per-transaction overhead disappears. Bigger batches buy little
/// and cost linearly in peak memory; smaller ones make a large scan a sequence
/// of tiny transactions.
pub const BATCH_SIZE: usize = 1024;

/// Batches written before the WAL is checkpointed back into the database.
///
/// In WAL mode a write appends to the log and the pages stay there — and
/// resident — until a checkpoint folds them into the main database. SQLite's
/// automatic checkpoint is driven by WAL size and is generous enough that a
/// long scan accumulates tens of megabytes of dirty pages before it fires.
///
/// That growth is transient and released when the connection closes, so it
/// never leaks; but "transient" over the several minutes of a million-file scan
/// is exactly when the daemon is nearest its ceiling. Checkpointing every
/// [`BATCH_SIZE`] × this many events bounds it instead.
///
/// 16 batches is ~16k events between checkpoints: frequent enough to keep the
/// WAL small, rare enough that the fsync cost stays negligible against the
/// filesystem walk that dominates a scan.
const BATCHES_PER_CHECKPOINT: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("store: {0}")]
    Store(#[from] dafs_store::StoreError),

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("walking {path}: {source}")]
    Walk {
        path: PathBuf,
        #[source]
        source: walkdir::Error,
    },

    #[error("watch root {0} does not exist")]
    MissingRoot(PathBuf),
}

/// What a scan did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScanSummary {
    pub files_seen: u64,
    pub dirs_seen: u64,
    pub events_recorded: u64,
    /// Entries skipped because their metadata could not be read — a permission
    /// denial, or a file deleted between being listed and being stat'd.
    ///
    /// Counted rather than ignored: a scan that silently skips half a home
    /// directory looks identical to one that worked, and the difference matters
    /// to a user wondering why their files are missing from the timeline.
    pub skipped: u64,
}

/// Options for a scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Directory names to skip entirely, matched on the component name.
    ///
    /// These are not user preferences so much as a correctness measure: a
    /// `.git` directory or a `node_modules` tree generates enormous volumes of
    /// churn that says nothing about what a person worked on, and indexing the
    /// daemon's own data directory would make it observe its own writes.
    pub skip_dirs: Vec<String>,
    /// Follow symlinks. Off by default: a symlink loop turns a bounded walk into
    /// an unbounded one, and a link pointing outside the watch root would pull
    /// files the user never asked to index into the timeline.
    pub follow_symlinks: bool,
    /// Stop after this many entries, if set. For bounding a first-run scan.
    pub max_entries: Option<u64>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            skip_dirs: [".git", ".dafs", "node_modules", "target", ".cache", "__pycache__"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            follow_symlinks: false,
            max_entries: None,
        }
    }
}

/// Walk `root`, recording every entry and emitting an event for anything new or
/// changed.
///
/// Idempotent: scanning an unchanged tree twice records events the first time
/// and none the second, because an entry whose size and mtime match what is
/// already stored is not a change. Without that, every daemon restart would
/// republish the user's entire filesystem into the timeline.
pub fn scan(
    conn: &Connection,
    interner: &mut Interner,
    root: &Path,
    options: &ScanOptions,
) -> Result<ScanSummary, ScanError> {
    if !root.exists() {
        return Err(ScanError::MissingRoot(root.to_path_buf()));
    }

    let started = std::time::Instant::now();
    let mut summary = ScanSummary::default();
    let mut batch: Vec<NewEvent> = Vec::with_capacity(BATCH_SIZE);
    let mut batches_since_checkpoint = 0usize;

    tracing::debug!(
        root = %root.display(),
        skip_dirs = ?options.skip_dirs,
        follow_symlinks = options.follow_symlinks,
        max_entries = ?options.max_entries,
        "scan starting"
    );

    // The root's own chain is created once, up front, so each entry below only
    // needs its own row rather than re-walking its ancestors.
    let root_id = ensure_dir_chain(conn, interner, root)?;

    // Each batch runs inside one transaction covering **both** the `files`
    // upserts and the events they produce.
    //
    // This is a crash-consistency requirement, not a performance tweak. With
    // the upserts committing immediately and the events only at flush, a
    // `kill -9` between the two leaves file rows with no event — and because a
    // rescan sees those files as unchanged (size and mtime already match), it
    // emits nothing for them. The file is in the store but absent from history,
    // permanently. Measured at 305 such rows on an 8k-file corpus before this
    // was fixed; `tests/crash_consistency.rs` is the regression test.
    // The walk runs in a helper so that an error partway through cannot return
    // past an open transaction. Rust has no `defer`, and rusqlite's `Transaction`
    // guard cannot be held here because the batch loop needs `&Connection` for
    // the interner as well — so the rollback is explicit, and this is the shape
    // that guarantees it always runs.
    let mut tx_open = false;
    let result = scan_inner(
        conn,
        interner,
        root,
        root_id,
        options,
        &mut summary,
        &mut batch,
        &mut batches_since_checkpoint,
        &mut tx_open,
    );

    if let Err(e) = result {
        if tx_open && let Err(rollback) = conn.execute_batch("ROLLBACK") {
            // Log rather than mask the original error: the rollback failing is
            // worth knowing about, but the scan error is what the caller needs.
            tracing::error!("rolling back after a scan error also failed: {rollback}");
        }
        return Err(e);
    }

    // A final checkpoint so the scan does not leave its whole WAL behind for
    // the next reader to fault in.
    checkpoint(conn);

    tracing::info!(
        root = %root.display(),
        files = summary.files_seen,
        dirs = summary.dirs_seen,
        events = summary.events_recorded,
        skipped = summary.skipped,
        elapsed_ms = started.elapsed().as_millis(),
        anon_bytes = anonymous_rss().unwrap_or(0),
        interner_cached = interner.cached(),
        "scan complete"
    );

    Ok(summary)
}

/// The walk itself. See [`scan`] for why this is a separate function.
#[allow(clippy::too_many_arguments)]
fn scan_inner(
    conn: &Connection,
    interner: &mut Interner,
    root: &Path,
    root_id: FileId,
    options: &ScanOptions,
    summary: &mut ScanSummary,
    batch: &mut Vec<NewEvent>,
    batches_since_checkpoint: &mut usize,
    tx_open: &mut bool,
) -> Result<(), ScanError> {
    begin(conn, tx_open)?;

    let walker = walkdir::WalkDir::new(root)
        .follow_links(options.follow_symlinks)
        .into_iter()
        .filter_entry(|e| !is_skipped(e, options));

    for entry in walker {
        if options.max_entries.is_some_and(|max| summary.files_seen + summary.dirs_seen >= max) {
            tracing::info!(max = options.max_entries, "scan stopped at the entry limit");
            break;
        }

        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                // A walk error is per-entry — an unreadable subdirectory should
                // not abandon the rest of the scan.
                tracing::debug!("skipping unreadable entry: {e}");
                summary.skipped += 1;
                continue;
            }
        };

        // The root itself is already recorded by ensure_dir_chain.
        if entry.depth() == 0 {
            continue;
        }

        match record_entry(conn, interner, root, root_id, &entry) {
            Ok(Some(event)) => {
                // Per-entry, so `--log dafs_scan=trace` answers "did it see my
                // file, and what did it decide about it" — the first question
                // asked whenever something is missing from the timeline.
                tracing::trace!(
                    path = %entry.path().display(),
                    kind = event.kind.as_str(),
                    size = ?event.size_bytes,
                    "recorded event"
                );
                batch.push(event);
                summary.events_recorded += 1;
            }
            Ok(None) => {
                tracing::trace!(path = %entry.path().display(), "unchanged, no event");
            }
            Err(e) => {
                // Debug rather than warn: on a live home directory, files
                // vanishing mid-scan and unreadable system paths are routine,
                // and warning on each would train the reader to ignore warnings.
                // The count in the summary is the signal that something is
                // systematically unreadable.
                tracing::debug!(path = %entry.path().display(), "skipping entry: {e}");
                summary.skipped += 1;
                continue;
            }
        }

        if entry.file_type().is_dir() {
            summary.dirs_seen += 1;
        } else {
            summary.files_seen += 1;
        }

        // Flush on a fixed batch size, not at the end: this is the bound that
        // keeps peak memory independent of corpus size.
        if batch.len() >= BATCH_SIZE {
            // Events first, then commit: the upserts for these entries are
            // already in this transaction, so the two land together.
            append_events(conn, batch)?;
            commit(conn, tx_open)?;
            batch.clear();

            *batches_since_checkpoint += 1;
            if *batches_since_checkpoint >= BATCHES_PER_CHECKPOINT {
                // Between transactions, never inside one: a checkpoint cannot
                // run with a write transaction open.
                checkpoint(conn);
                *batches_since_checkpoint = 0;

                // Progress with the memory numbers attached, at debug. A long
                // scan is otherwise silent for minutes, and when the ceiling is
                // the thing under suspicion the useful log line is the one that
                // says what memory was doing *while* it ran — reproducing a
                // ceiling failure after the fact is far more work than leaving
                // the trail here.
                tracing::debug!(
                    files = summary.files_seen,
                    dirs = summary.dirs_seen,
                    events = summary.events_recorded,
                    skipped = summary.skipped,
                    anon_bytes = anonymous_rss().unwrap_or(0),
                    interner_cached = interner.cached(),
                    "scan progress"
                );
            }

            begin(conn, tx_open)?;
        }
    }

    // The final partial batch, in the transaction its upserts already live in.
    append_events(conn, batch)?;
    commit(conn, tx_open)?;

    Ok(())
}

/// Open the per-batch transaction.
///
/// `IMMEDIATE` takes the write lock up front rather than upgrading partway
/// through. The scan writes from its first statement, so deferring only creates
/// a window in which another writer can take the lock and force this one to
/// fail mid-batch.
fn begin(conn: &Connection, open: &mut bool) -> Result<(), ScanError> {
    if *open {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE")?;
    *open = true;
    Ok(())
}

fn commit(conn: &Connection, open: &mut bool) -> Result<(), ScanError> {
    if !*open {
        return Ok(());
    }
    conn.execute_batch("COMMIT")?;
    *open = false;
    Ok(())
}

/// Append events on the current connection, without opening a transaction.
///
/// Distinct from `dafs_store::events::append_batch`, which wraps its own: here
/// the caller already holds one spanning the `files` upserts these events refer
/// to, and a nested transaction would commit the events separately — which is
/// exactly the bug this design exists to prevent.
fn append_events(conn: &Connection, events: &[NewEvent]) -> Result<(), ScanError> {
    for event in events {
        dafs_store::events::append(conn, event)?;
    }
    Ok(())
}

/// Record a single path observed by the watcher.
///
/// The watch path and the scan path deliberately converge on the same
/// [`record_entry`] logic, so an event looks identical whether it came from a
/// scan or a live change. A separate implementation here would be a second
/// source of truth for "what counts as a modification", and the two would drift.
///
/// Returns the file's id when a non-directory file was actually created or
/// modified — the signal the daemon's extraction queue is driven off of.
/// Every other outcome (outside every watch root, vanished before it could be
/// stat'd, a directory, an unchanged watch-fire) returns `None`: none of those
/// are a file whose *content* extraction could ever care about.
pub fn record_path(
    conn: &Connection,
    interner: &mut Interner,
    roots: &[PathBuf],
    path: &Path,
) -> Result<Option<FileId>, ScanError> {
    let Some(root) = owning_root(roots, path) else {
        // A change outside every watch root. Not an error — notify can report
        // paths from a root that was just removed — but nothing to record.
        tracing::trace!(path = %path.display(), "change outside all watch roots, ignoring");
        return Ok(None);
    };

    // The file may already be gone again by the time this runs, which is normal
    // for an editor's temporary file. Treat it as a removal rather than an
    // error.
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            record_removal(conn, interner, roots, path)?;
            return Ok(None);
        }
        Err(e) => {
            tracing::debug!(path = %path.display(), "cannot stat changed path: {e}");
            return Ok(None);
        }
    };

    let root_id = ensure_dir_chain(conn, interner, root)?;
    let is_dir = metadata.is_dir();
    let size = if is_dir { None } else { Some(metadata.len() as i64) };
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);

    let relative = path.strip_prefix(root).unwrap_or(path);
    let parent_id = match relative.parent() {
        Some(p) if !p.as_os_str().is_empty() => ensure_relative_dirs(conn, interner, root_id, p)?,
        _ => root_id,
    };

    let Some(name) = path.file_name() else {
        return Ok(None);
    };
    let name = name.to_string_lossy();
    let component_id = if is_dir {
        interner.intern_dir(conn, &name)?
    } else {
        interner.intern_leaf(conn, &name)?
    };

    let previous = existing_entry(conn, parent_id, component_id)?;
    let file_id = upsert_entry(conn, Some(parent_id), component_id, is_dir, size, mtime)?;

    if is_dir {
        return Ok(None);
    }

    let kind = match previous {
        None => EventKind::Created,
        Some(prev) if prev.size_bytes != size || prev.mtime_unix != mtime => EventKind::Modified,
        Some(_) => {
            tracing::trace!(path = %path.display(), "watch fired but nothing changed");
            return Ok(None);
        }
    };

    dafs_store::events::append(conn, &NewEvent::now(file_id, kind).with_size(size))?;
    tracing::debug!(path = %path.display(), kind = kind.as_str(), "recorded watch event");

    Ok(Some(file_id))
}

/// Record that a path moved.
///
/// The file keeps its row and its id, so its history survives the move — which
/// is the whole point of detecting renames rather than recording a delete and a
/// create. The event carries where it came from, so the timeline can show the
/// old name.
///
/// Falls back to recording the destination as a plain change when the source
/// was never known (a move in from outside the watched tree, or from a
/// directory the scan skipped).
///
/// Returns the file's id whenever the destination is a file inside a watched
/// root — same signal as [`record_path`], since a rename is as much a reason
/// to (re-)extract as a create or a modify: the fallback and moved-out cases
/// below delegate to functions that already return the right thing.
pub fn record_rename(
    conn: &Connection,
    interner: &mut Interner,
    roots: &[PathBuf],
    from: &Path,
    to: &Path,
) -> Result<Option<FileId>, ScanError> {
    let Some(to_root) = owning_root(roots, to) else {
        // Moved out of every watched tree: from this tree's point of view the
        // file is gone.
        record_removal(conn, interner, roots, from)?;
        return Ok(None);
    };

    // Resolve the source row without creating anything — a rename of something
    // never recorded is just an arrival.
    let source = lookup_file(conn, interner, roots, from)?;

    let Some((file_id, old_parent_id, old_component_id)) = source else {
        return record_path(conn, interner, roots, to);
    };

    // Re-parent the existing row rather than inserting a new one. This is the
    // step that preserves history: every event already pointing at `file_id`
    // stays attached to the file under its new name.
    let to_root_id = ensure_dir_chain(conn, interner, to_root)?;
    let relative = to.strip_prefix(to_root).unwrap_or(to);
    let new_parent_id = match relative.parent() {
        Some(p) if !p.as_os_str().is_empty() => {
            ensure_relative_dirs(conn, interner, to_root_id, p)?
        }
        _ => to_root_id,
    };

    let Some(name) = to.file_name() else {
        return Ok(None);
    };
    let name = name.to_string_lossy();

    let is_dir = std::fs::symlink_metadata(to).map(|m| m.is_dir()).unwrap_or(false);
    let new_component_id = if is_dir {
        interner.intern_dir(conn, &name)?
    } else {
        interner.intern_leaf(conn, &name)?
    };

    // A row may already exist at the destination if something was overwritten.
    // Tombstone it first: two live rows cannot share (parent_id, component_id),
    // and the file being replaced is genuinely gone.
    if let Some((displaced, _, _)) = lookup_file(conn, interner, roots, to)?
        && displaced != file_id
    {
        conn.execute(
            "UPDATE files SET deleted_at = ?2 WHERE id = ?1",
            rusqlite::params![displaced, now_unix_ms()],
        )?;
        dafs_store::events::append(conn, &NewEvent::now(displaced, EventKind::Deleted))?;
    }

    conn.execute(
        "UPDATE files SET parent_id = ?2, component_id = ?3, deleted_at = NULL WHERE id = ?1",
        rusqlite::params![file_id, new_parent_id, new_component_id],
    )?;

    dafs_store::events::append(
        conn,
        &NewEvent {
            file_id,
            kind: EventKind::Renamed,
            at_unix_ms: now_unix_ms(),
            size_bytes: std::fs::symlink_metadata(to).ok().map(|m| m.len() as i64),
            prev_parent_id: Some(old_parent_id),
            prev_component_id: Some(old_component_id),
        },
    )?;

    tracing::debug!(from = %from.display(), to = %to.display(), "recorded rename");

    // A renamed directory still needs the row update above (its children's
    // paths resolve through it), but directories never need extraction — same
    // rule `record_entry` applies to a plain create or modify.
    Ok(if is_dir { None } else { Some(file_id) })
}

/// Find an existing `files` row for a path, without creating anything.
///
/// Returns the row id along with its current parent and component, which a
/// rename needs in order to record where the file came from.
#[allow(clippy::type_complexity)]
fn lookup_file(
    conn: &Connection,
    interner: &mut Interner,
    roots: &[PathBuf],
    path: &Path,
) -> Result<Option<(FileId, FileId, i64)>, ScanError> {
    use rusqlite::OptionalExtension as _;

    let Some(root) = owning_root(roots, path) else {
        return Ok(None);
    };
    let Some(name) = path.file_name() else {
        return Ok(None);
    };
    let Some(component_id) = lookup_component(conn, &name.to_string_lossy())? else {
        return Ok(None);
    };

    let relative = path.strip_prefix(root).unwrap_or(path);
    let Some(parent_id) = lookup_parent(conn, interner, root, relative)? else {
        return Ok(None);
    };

    Ok(conn
        .query_row(
            // Live row first: the uniqueness index is partial, so a location
            // can hold one live row plus tombstones of files previously there.
            "SELECT id FROM files
              WHERE parent_id IS ?1 AND component_id = ?2
              ORDER BY deleted_at IS NULL DESC, deleted_at DESC
              LIMIT 1",
            rusqlite::params![parent_id, component_id],
            |r| r.get::<_, FileId>(0),
        )
        .optional()?
        .map(|id| (id, parent_id, component_id)))
}

/// Record that a path is gone.
///
/// Tombstones rather than deletes: events referencing the row must keep
/// resolving, and the deletion is itself a fact the timeline should show. A row
/// removed outright would take its history with it, which for a tool whose
/// purpose is remembering what happened is the wrong direction.
pub fn record_removal(
    conn: &Connection,
    interner: &mut Interner,
    roots: &[PathBuf],
    path: &Path,
) -> Result<(), ScanError> {
    let Some(root) = owning_root(roots, path) else {
        return Ok(());
    };

    let Some(name) = path.file_name() else {
        return Ok(());
    };
    let name = name.to_string_lossy();

    // Look the entry up rather than creating it: a removal for something never
    // recorded is nothing to do, and interning here would create rows for paths
    // that no longer exist.
    let Some(component_id) = lookup_component(conn, &name)? else {
        return Ok(());
    };

    let relative = path.strip_prefix(root).unwrap_or(path);
    let Some(parent_id) = lookup_parent(conn, interner, root, relative)? else {
        return Ok(());
    };

    let existing: Option<FileId> = {
        use rusqlite::OptionalExtension as _;
        conn.query_row(
            "SELECT id FROM files
              WHERE parent_id IS ?1 AND component_id = ?2 AND deleted_at IS NULL",
            rusqlite::params![parent_id, component_id],
            |r| r.get(0),
        )
        .optional()?
    };

    let Some(file_id) = existing else {
        return Ok(());
    };

    conn.execute(
        "UPDATE files SET deleted_at = ?2 WHERE id = ?1",
        rusqlite::params![file_id, now_unix_ms()],
    )?;

    dafs_store::events::append(conn, &NewEvent::now(file_id, EventKind::Deleted))?;
    tracing::debug!(path = %path.display(), "recorded deletion");

    Ok(())
}

/// The watch root that contains `path`, if any.
///
/// Longest match wins, so a nested root records against the most specific one
/// rather than whichever happens to be first in the list.
fn owning_root<'a>(roots: &'a [PathBuf], path: &Path) -> Option<&'a Path> {
    roots
        .iter()
        .filter(|r| path.starts_with(r))
        .max_by_key(|r| r.as_os_str().len())
        .map(PathBuf::as_path)
}

fn lookup_component(conn: &Connection, name: &str) -> Result<Option<i64>, ScanError> {
    use rusqlite::OptionalExtension as _;
    Ok(conn
        .query_row("SELECT id FROM path_components WHERE name = ?1", [name], |r| r.get(0))
        .optional()?)
}

/// Resolve the parent directory row for a path already known to the store,
/// without creating anything.
fn lookup_parent(
    conn: &Connection,
    interner: &mut Interner,
    root: &Path,
    relative: &Path,
) -> Result<Option<FileId>, ScanError> {
    use rusqlite::OptionalExtension as _;

    let mut current = ensure_dir_chain(conn, interner, root)?;

    let Some(parent) = relative.parent() else {
        return Ok(Some(current));
    };

    for component in parent.components() {
        let std::path::Component::Normal(part) = component else { continue };
        let Some(component_id) = lookup_component(conn, &part.to_string_lossy())? else {
            return Ok(None);
        };

        let next: Option<FileId> = conn
            .query_row(
                // Live row first: the uniqueness index is partial, so a location
                // can hold one live row plus tombstones of files previously there.
                "SELECT id FROM files
              WHERE parent_id IS ?1 AND component_id = ?2
              ORDER BY deleted_at IS NULL DESC, deleted_at DESC
              LIMIT 1",
                rusqlite::params![current, component_id],
                |r| r.get(0),
            )
            .optional()?;

        match next {
            Some(id) => current = id,
            None => return Ok(None),
        }
    }

    Ok(Some(current))
}

/// Anonymous resident memory, for progress logging.
///
/// `RssAnon` rather than `VmRSS`: the store maps the database, and those pages
/// grow with it while being file-backed and evictable. A progress line reporting
/// total RSS would look like a leak during a large scan and send whoever reads
/// it chasing the page cache. See `docs/memory-budget.md` §8.3.
fn anonymous_rss() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("RssAnon:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kb * 1024)
}

/// Fold the WAL back into the database, bounding its resident size.
///
/// `PASSIVE` rather than `TRUNCATE` or `FULL`: a passive checkpoint does what it
/// can without waiting for readers and returns immediately if one is active.
/// Blocking the scan on an API request that happens to be reading the timeline
/// would be a poor trade for a slightly smaller WAL.
///
/// A failure here is logged rather than propagated. A checkpoint is an
/// optimisation — the data is already durably committed to the WAL — so failing
/// the whole scan because the log could not be folded in yet would turn a
/// non-problem into lost work.
fn checkpoint(conn: &Connection) {
    if let Err(e) = conn.pragma_update(None, "wal_checkpoint", "PASSIVE") {
        tracing::debug!("wal checkpoint failed, continuing: {e}");
    }
}

/// Record one entry, returning an event if it represents a change.
fn record_entry(
    conn: &Connection,
    interner: &mut Interner,
    root: &Path,
    root_id: FileId,
    entry: &walkdir::DirEntry,
) -> Result<Option<NewEvent>, ScanError> {
    // Metadata can fail on a file that vanished between listing and stat, which
    // is normal on a live filesystem rather than an error worth failing on.
    let metadata = entry
        .metadata()
        .map_err(|e| ScanError::Walk { path: entry.path().to_path_buf(), source: e })?;

    let is_dir = metadata.is_dir();
    let size = if is_dir { None } else { Some(metadata.len() as i64) };
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);

    // Resolve the parent by walking the path relative to the root, so the
    // ancestor rows above the root are never re-created per entry.
    let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
    let parent_id = match relative.parent() {
        Some(p) if p.as_os_str().is_empty() => root_id,
        Some(p) => ensure_relative_dirs(conn, interner, root_id, p)?,
        None => root_id,
    };

    let name = entry.file_name().to_string_lossy();
    // A directory name repeats across the tree and is worth caching; a filename
    // is nearly always distinct and caching it would add one resident entry per
    // file scanned. See `dafs_store::paths` for why that distinction is a memory
    // requirement.
    let component_id = if is_dir {
        interner.intern_dir(conn, &name)?
    } else {
        interner.intern_leaf(conn, &name)?
    };

    let previous = existing_entry(conn, parent_id, component_id)?;
    let file_id = upsert_entry(conn, Some(parent_id), component_id, is_dir, size, mtime)?;

    // Directories generate no events. A directory's mtime changes whenever a
    // child is added or removed, so emitting on it would double every create
    // and delete with a meaningless "the folder changed" row.
    if is_dir {
        return Ok(None);
    }

    let kind = match previous {
        None => EventKind::Created,
        Some(prev) if prev.size_bytes != size || prev.mtime_unix != mtime => EventKind::Modified,
        // Unchanged. This is the branch that makes a rescan quiet.
        Some(_) => return Ok(None),
    };

    Ok(Some(NewEvent::now(file_id, kind).with_size(size)))
}

/// Create the directory rows for a path relative to the scan root.
fn ensure_relative_dirs(
    conn: &Connection,
    interner: &mut Interner,
    root_id: FileId,
    relative: &Path,
) -> Result<FileId, ScanError> {
    let mut parent = root_id;
    for component in relative.components() {
        if let std::path::Component::Normal(part) = component {
            let component_id = interner.intern_dir(conn, &part.to_string_lossy())?;
            parent = upsert_entry(conn, Some(parent), component_id, true, None, None)?;
        }
    }
    Ok(parent)
}

/// What is already stored for an entry, if anything.
struct StoredEntry {
    size_bytes: Option<i64>,
    mtime_unix: Option<i64>,
}

fn existing_entry(
    conn: &Connection,
    parent_id: FileId,
    component_id: i64,
) -> Result<Option<StoredEntry>, ScanError> {
    use rusqlite::OptionalExtension as _;

    Ok(conn
        .query_row(
            "SELECT size_bytes, mtime_unix FROM files
              WHERE parent_id IS ?1 AND component_id = ?2 AND deleted_at IS NULL",
            rusqlite::params![parent_id, component_id],
            |r| Ok(StoredEntry { size_bytes: r.get(0)?, mtime_unix: r.get(1)? }),
        )
        .optional()?)
}

fn is_skipped(entry: &walkdir::DirEntry, options: &ScanOptions) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    // Depth 0 is the root itself: skipping it because its name matches would
    // silently scan nothing at all, which is a confusing way to configure an
    // exclusion.
    if entry.depth() == 0 {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    options.skip_dirs.iter().any(|d| d == name.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dafs_store::events::{TimelineQuery, count, timeline};

    fn setup() -> (Connection, Interner, tempfile::TempDir) {
        let conn = dafs_store::open_in_memory().expect("open");
        (conn, Interner::new(), tempfile::tempdir().expect("tempdir"))
    }

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&path, contents).expect("write");
        path
    }

    #[test]
    fn a_scan_records_every_file_once() {
        let (conn, mut i, dir) = setup();
        write(dir.path(), "a.txt", "one");
        write(dir.path(), "b.txt", "two");
        write(dir.path(), "nested/c.txt", "three");

        let summary = scan(&conn, &mut i, dir.path(), &ScanOptions::default()).expect("scan");

        assert_eq!(summary.files_seen, 3);
        assert_eq!(summary.events_recorded, 3);
        assert_eq!(count(&conn).expect("count"), 3);
    }

    /// A rescan of an unchanged tree must be silent. Without this, every daemon
    /// restart would republish the user's whole filesystem into the timeline.
    #[test]
    fn rescanning_an_unchanged_tree_records_nothing() {
        let (conn, mut i, dir) = setup();
        write(dir.path(), "a.txt", "one");

        scan(&conn, &mut i, dir.path(), &ScanOptions::default()).expect("first");
        let after_first = count(&conn).expect("count");

        let second = scan(&conn, &mut i, dir.path(), &ScanOptions::default()).expect("second");
        assert_eq!(second.events_recorded, 0, "an unchanged rescan emitted events");
        assert_eq!(count(&conn).expect("count"), after_first);
    }

    /// The realistic restart: the process died, so the interner cache is gone.
    #[test]
    fn rescanning_with_a_cold_interner_records_nothing() {
        let (conn, mut i, dir) = setup();
        write(dir.path(), "a.txt", "one");
        scan(&conn, &mut i, dir.path(), &ScanOptions::default()).expect("first");
        let after_first = count(&conn).expect("count");

        let mut cold = Interner::new();
        let second = scan(&conn, &mut cold, dir.path(), &ScanOptions::default()).expect("second");
        assert_eq!(second.events_recorded, 0, "a cold interner republished the tree");
        assert_eq!(count(&conn).expect("count"), after_first);
    }

    #[test]
    fn a_changed_file_is_recorded_as_modified() {
        let (conn, mut i, dir) = setup();
        let path = write(dir.path(), "a.txt", "one");
        scan(&conn, &mut i, dir.path(), &ScanOptions::default()).expect("first");

        // A longer body changes the size, so the comparison catches it even if
        // the filesystem's mtime granularity is coarse.
        std::fs::write(&path, "substantially longer contents").expect("rewrite");

        let second = scan(&conn, &mut i, dir.path(), &ScanOptions::default()).expect("second");
        assert_eq!(second.events_recorded, 1);

        let entries = timeline(&conn, &TimelineQuery::default()).expect("timeline");
        assert_eq!(entries[0].kind, EventKind::Modified);
        assert!(entries[0].path.ends_with("/a.txt"), "unexpected path {}", entries[0].path);
    }

    #[test]
    fn skipped_directories_are_not_walked() {
        let (conn, mut i, dir) = setup();
        write(dir.path(), "keep.txt", "yes");
        write(dir.path(), ".git/objects/deadbeef", "no");
        write(dir.path(), "node_modules/left-pad/index.js", "no");

        let summary = scan(&conn, &mut i, dir.path(), &ScanOptions::default()).expect("scan");
        assert_eq!(summary.files_seen, 1, "a skipped directory was walked");

        let entries = timeline(&conn, &TimelineQuery::default()).expect("timeline");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].path.ends_with("/keep.txt"));
    }

    /// A root whose own name is on the skip list must still be scanned —
    /// otherwise configuring a watch on `~/src/target` silently indexes nothing.
    #[test]
    fn a_root_matching_the_skip_list_is_still_scanned() {
        let (conn, mut i, dir) = setup();
        let root = dir.path().join("target");
        std::fs::create_dir_all(&root).expect("mkdir");
        write(&root, "a.txt", "one");

        let summary = scan(&conn, &mut i, &root, &ScanOptions::default()).expect("scan");
        assert_eq!(summary.files_seen, 1, "the root was skipped by its own name");
    }

    #[test]
    fn directories_do_not_generate_events() {
        let (conn, mut i, dir) = setup();
        write(dir.path(), "nested/deep/a.txt", "one");

        let summary = scan(&conn, &mut i, dir.path(), &ScanOptions::default()).expect("scan");
        assert_eq!(summary.dirs_seen, 2, "nested and deep");
        assert_eq!(summary.events_recorded, 1, "only the file should emit an event");
    }

    #[test]
    fn a_missing_root_is_an_error_not_an_empty_scan() {
        let (conn, mut i, dir) = setup();
        let missing = dir.path().join("does-not-exist");
        assert!(matches!(
            scan(&conn, &mut i, &missing, &ScanOptions::default()),
            Err(ScanError::MissingRoot(_))
        ));
    }

    #[test]
    fn the_entry_limit_bounds_a_scan() {
        let (conn, mut i, dir) = setup();
        for n in 0..20 {
            write(dir.path(), &format!("file-{n}.txt"), "x");
        }

        let options = ScanOptions { max_entries: Some(5), ..Default::default() };
        let summary = scan(&conn, &mut i, dir.path(), &options).expect("scan");
        assert!(summary.files_seen <= 5, "the limit was exceeded: {}", summary.files_seen);
    }

    /// Symlinks are not followed, so a loop cannot turn a bounded walk into an
    /// unbounded one. Without this the scan hangs rather than failing.
    #[test]
    #[cfg(unix)]
    fn a_symlink_loop_terminates() {
        let (conn, mut i, dir) = setup();
        write(dir.path(), "real.txt", "one");
        std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).expect("symlink");

        let summary = scan(&conn, &mut i, dir.path(), &ScanOptions::default()).expect("scan");
        assert!(summary.files_seen >= 1, "the real file should still be seen");
    }

    /// M01's central safety property: the observer never writes to the tree it
    /// observes. Asserted by comparing the whole tree's contents and mtimes
    /// before and after, rather than by reading the code and trusting it.
    #[test]
    fn scanning_never_modifies_the_observed_tree() {
        let (conn, mut i, dir) = setup();
        write(dir.path(), "a.txt", "one");
        write(dir.path(), "nested/b.txt", "two");

        fn snapshot(root: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
            let mut out: Vec<_> = walkdir::WalkDir::new(root)
                .into_iter()
                .filter_map(Result::ok)
                .filter_map(|e| {
                    let m = e.metadata().ok()?;
                    Some((e.path().to_path_buf(), m.len(), m.modified().ok()?))
                })
                .collect();
            out.sort();
            out
        }

        let before = snapshot(dir.path());
        scan(&conn, &mut i, dir.path(), &ScanOptions::default()).expect("scan");
        let after = snapshot(dir.path());

        assert_eq!(before, after, "the scan modified the tree it was observing");
    }

    /// The point of detecting a rename rather than recording delete+create:
    /// the file keeps its id, so everything already known about it survives.
    #[test]
    fn a_rename_preserves_the_file_and_its_history() {
        let (conn, mut i, dir) = setup();
        let roots = vec![dir.path().to_path_buf()];

        let before = write(dir.path(), "before.txt", "contents");
        scan(&conn, &mut i, dir.path(), &ScanOptions::default()).expect("scan");

        let original_id: i64 = conn
            .query_row("SELECT file_id FROM events LIMIT 1", [], |r| r.get(0))
            .expect("original file id");

        let after = dir.path().join("after.txt");
        std::fs::rename(&before, &after).expect("rename");
        record_rename(&conn, &mut i, &roots, &before, &after).expect("record rename");

        // Same row, so the created event still belongs to this file.
        let events: Vec<(i64, String)> = conn
            .prepare("SELECT file_id, kind FROM events ORDER BY id")
            .expect("prepare")
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");

        assert_eq!(events.len(), 2, "expected a create and a rename, got {events:?}");
        assert_eq!(events[1].1, "renamed");
        assert!(
            events.iter().all(|(id, _)| *id == original_id),
            "the rename created a new file row, losing history: {events:?}"
        );

        // And the timeline shows both names.
        let entries = timeline(&conn, &TimelineQuery::default()).expect("timeline");
        assert!(entries[0].path.ends_with("/after.txt"), "unexpected path {}", entries[0].path);
        assert!(
            entries[0].previous_path.as_deref().is_some_and(|p| p.ends_with("/before.txt")),
            "rename did not record where the file came from: {:?}",
            entries[0].previous_path
        );
    }

    /// A rename onto an existing file destroys that file. It must be tombstoned
    /// rather than left as a second live row sharing the same location.
    #[test]
    fn a_rename_over_an_existing_file_tombstones_it() {
        let (conn, mut i, dir) = setup();
        let roots = vec![dir.path().to_path_buf()];

        let source = write(dir.path(), "source.txt", "new contents");
        let victim = write(dir.path(), "victim.txt", "about to be replaced");
        scan(&conn, &mut i, dir.path(), &ScanOptions::default()).expect("scan");

        let victim_id = {
            let component: i64 = conn
                .query_row("SELECT id FROM path_components WHERE name = 'victim.txt'", [], |r| {
                    r.get(0)
                })
                .expect("victim component");
            conn.query_row("SELECT id FROM files WHERE component_id = ?1", [component], |r| {
                r.get::<_, i64>(0)
            })
            .expect("victim row")
        };

        std::fs::rename(&source, &victim).expect("rename over");
        record_rename(&conn, &mut i, &roots, &source, &victim).expect("record rename");

        let deleted: Option<i64> = conn
            .query_row("SELECT deleted_at FROM files WHERE id = ?1", [victim_id], |r| r.get(0))
            .expect("victim row still present");
        assert!(deleted.is_some(), "the overwritten file was not tombstoned");

        // Exactly one live row at that location, or the unique constraint would
        // have been violated.
        let live: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files f
                   JOIN path_components c ON c.id = f.component_id
                  WHERE c.name = 'victim.txt' AND f.deleted_at IS NULL",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(live, 1, "expected exactly one live row at the destination");
    }

    /// A file moved in from outside the watched tree was never known, so it is
    /// an arrival rather than a move.
    #[test]
    fn a_rename_from_an_unknown_source_is_recorded_as_a_creation() {
        let (conn, mut i, dir) = setup();
        let roots = vec![dir.path().to_path_buf()];

        // Never scanned, so the store has no row for it.
        let outside = dir.path().join("never-seen.txt");
        let inside = dir.path().join("arrived.txt");
        std::fs::write(&inside, "hello").expect("write");

        record_rename(&conn, &mut i, &roots, &outside, &inside).expect("record rename");

        let entries = timeline(&conn, &TimelineQuery::default()).expect("timeline");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].kind,
            EventKind::Created,
            "expected a creation, got {:?}",
            entries[0]
        );
        assert!(entries[0].path.ends_with("/arrived.txt"));
    }

    /// Moving a file out of every watched root is a deletion from the tree's
    /// point of view.
    #[test]
    fn a_rename_out_of_every_root_is_recorded_as_a_deletion() {
        let (conn, mut i, dir) = setup();
        let watched = dir.path().join("watched");
        std::fs::create_dir_all(&watched).expect("mkdir");
        let roots = vec![watched.clone()];

        let inside = write(&watched, "leaving.txt", "bye");
        scan(&conn, &mut i, &watched, &ScanOptions::default()).expect("scan");

        let outside = dir.path().join("elsewhere.txt");
        std::fs::rename(&inside, &outside).expect("rename out");
        record_rename(&conn, &mut i, &roots, &inside, &outside).expect("record rename");

        let entries = timeline(&conn, &TimelineQuery::default()).expect("timeline");
        assert_eq!(
            entries[0].kind,
            EventKind::Deleted,
            "expected a deletion, got {:?}",
            entries[0]
        );
    }

    /// A file deleted and then recreated must work. The uniqueness index is
    /// partial for exactly this: a tombstone left occupying the slot would make
    /// the recreation a constraint violation.
    #[test]
    fn a_file_can_be_deleted_and_recreated() {
        let (conn, mut i, dir) = setup();
        let roots = vec![dir.path().to_path_buf()];

        let path = write(dir.path(), "flaky.txt", "first life");
        scan(&conn, &mut i, dir.path(), &ScanOptions::default()).expect("scan");

        std::fs::remove_file(&path).expect("remove");
        record_removal(&conn, &mut i, &roots, &path).expect("record removal");

        std::fs::write(&path, "second life, rather longer").expect("recreate");
        record_path(&conn, &mut i, &roots, &path).expect("record recreation");

        let kinds: Vec<String> = conn
            .prepare("SELECT kind FROM events ORDER BY id")
            .expect("prepare")
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");

        assert_eq!(
            kinds,
            vec!["created", "deleted", "created"],
            "a delete-then-recreate should read as three events"
        );

        // Exactly one live row, so a later scan does not see two.
        let live: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files f
                   JOIN path_components c ON c.id = f.component_id
                  WHERE c.name = 'flaky.txt' AND f.deleted_at IS NULL",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(live, 1, "expected one live row after recreation");
    }

    /// Batching is the memory bound, so a corpus larger than one batch must
    /// still record every event — an off-by-one in the flush would lose the
    /// remainder silently.
    #[test]
    fn a_corpus_larger_than_one_batch_records_every_event() {
        let (conn, mut i, dir) = setup();
        let n = BATCH_SIZE + 37;
        for f in 0..n {
            write(dir.path(), &format!("file-{f}.txt"), "x");
        }

        let summary = scan(&conn, &mut i, dir.path(), &ScanOptions::default()).expect("scan");
        assert_eq!(summary.files_seen as usize, n);
        assert_eq!(count(&conn).expect("count") as usize, n, "the final partial batch was lost");
    }

    /// The signal `dafs-daemon` drives its extraction queue off: a genuinely
    /// new file must hand back an id to enqueue.
    #[test]
    fn record_path_returns_the_file_id_for_a_new_file() {
        let (conn, mut i, dir) = setup();
        let roots = vec![dir.path().to_path_buf()];
        let path = write(dir.path(), "a.txt", "one");

        let file_id =
            record_path(&conn, &mut i, &roots, &path).expect("record path").expect("a new file");
        assert!(
            dafs_store::paths::resolve_path(&conn, file_id).expect("resolve").ends_with("/a.txt"),
            "the id handed back was not the file just recorded"
        );
    }

    /// A watch firing with nothing actually changed must not enqueue a
    /// pointless re-extraction of an untouched file.
    #[test]
    fn record_path_returns_none_when_nothing_changed() {
        let (conn, mut i, dir) = setup();
        let roots = vec![dir.path().to_path_buf()];
        let path = write(dir.path(), "a.txt", "one");

        record_path(&conn, &mut i, &roots, &path).expect("first").expect("created");
        let second = record_path(&conn, &mut i, &roots, &path).expect("second");
        assert_eq!(second, None, "an unchanged file should not be reported as changed");
    }

    /// Directories never need extraction — the same rule a scan already
    /// applies to events applies here to the id handed back for enqueuing.
    #[test]
    fn record_path_returns_none_for_a_directory() {
        let (conn, mut i, dir) = setup();
        let roots = vec![dir.path().to_path_buf()];
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).expect("mkdir");

        let result = record_path(&conn, &mut i, &roots, &nested).expect("record path");
        assert_eq!(result, None, "a directory should never be queued for extraction");
    }

    /// A change outside every watch root is a no-op, not a file to extract.
    #[test]
    fn record_path_returns_none_outside_every_root() {
        let (conn, mut i, dir) = setup();
        let watched = dir.path().join("watched");
        std::fs::create_dir_all(&watched).expect("mkdir");
        let roots = vec![watched];
        let outside = write(dir.path(), "elsewhere.txt", "one");

        let result = record_path(&conn, &mut i, &roots, &outside).expect("record path");
        assert_eq!(result, None);
    }

    /// The rename counterpart of the above: a moved file is exactly as much a
    /// reason to (re-)extract as a create or modify.
    #[test]
    fn record_rename_returns_the_file_id_for_a_renamed_file() {
        let (conn, mut i, dir) = setup();
        let roots = vec![dir.path().to_path_buf()];

        let before = write(dir.path(), "before.txt", "contents");
        scan(&conn, &mut i, dir.path(), &ScanOptions::default()).expect("scan");

        let after = dir.path().join("after.txt");
        std::fs::rename(&before, &after).expect("rename");
        let file_id = record_rename(&conn, &mut i, &roots, &before, &after)
            .expect("record rename")
            .expect("a renamed file");

        assert!(
            dafs_store::paths::resolve_path(&conn, file_id)
                .expect("resolve")
                .ends_with("/after.txt"),
            "the id handed back was not the renamed file"
        );
    }

    /// A renamed directory must still update its row (its children resolve
    /// through it) but must not be queued for extraction.
    #[test]
    fn record_rename_returns_none_for_a_renamed_directory() {
        let (conn, mut i, dir) = setup();
        let roots = vec![dir.path().to_path_buf()];

        let before = dir.path().join("before-dir");
        std::fs::create_dir(&before).expect("mkdir");
        scan(&conn, &mut i, dir.path(), &ScanOptions::default()).expect("scan");

        let after = dir.path().join("after-dir");
        std::fs::rename(&before, &after).expect("rename");
        let result = record_rename(&conn, &mut i, &roots, &before, &after).expect("record rename");
        assert_eq!(result, None, "a renamed directory should never be queued for extraction");
    }
}

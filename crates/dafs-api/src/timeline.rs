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
///
/// The extraction fields are all omitted from the JSON when absent (`Option`
/// plus `skip_serializing_if`, matching `size_bytes`/`previous_path` above)
/// rather than serialized as `null` — most files have no metadata yet, and a
/// client should be able to tell "no metadata" from "empty string" without
/// special-casing null.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub word_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_taken_at_unix: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_camera_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_head_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_head_author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_head_at_unix: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keywords: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entities: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classification: Option<String>,
}

/// What the timeline handler needs from a store.
///
/// Errors are `String` rather than a concrete type: the HTTP layer turns them
/// into a 500 and a log line either way, and a richer type here would leak the
/// store's error taxonomy into the API crate for no gain.
pub trait TimelineStore: Send + Sync + 'static {
    #[allow(clippy::too_many_arguments)]
    fn timeline(
        &self,
        limit: u32,
        before_id: Option<i64>,
        kind: Option<&str>,
        doc_type: Option<&str>,
        author: Option<&str>,
        language: Option<&str>,
        git_branch: Option<&str>,
        classification: Option<&str>,
    ) -> Result<Vec<TimelineItem>, String>;

    /// Event and file counts, plus the extraction queue depth, for `/metrics`
    /// and the UI header.
    fn stats(&self) -> Result<TimelineStats, String>;

    /// Distinct values (and counts) of one facet column, most common first —
    /// what populates a `/facets`-backed filter dropdown. `field` is one of
    /// `doc_type`/`author`/`language`/`git_branch`/`classification`;
    /// validating that is the caller's job (the HTTP handler), same division
    /// of labour `events` already uses for `kind` — this trait method is not
    /// the place to 400.
    fn facets(&self, field: &str) -> Result<Vec<(String, i64)>, String>;
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct TimelineStats {
    pub events: i64,
    pub files: i64,
    /// Files still in `extraction_queue` (M02a) — a raw row count, not the
    /// dispatcher's retry-eligible view. Zero for a store with no metadata
    /// module wired in at all.
    pub pending_extractions: i64,
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
            doc_type: Option<&str>,
            author: Option<&str>,
            language: Option<&str>,
            git_branch: Option<&str>,
            classification: Option<&str>,
        ) -> Result<Vec<TimelineItem>, String> {
            if self.fail {
                return Err("store is broken".into());
            }
            Ok(self
                .items
                .iter()
                .filter(|i| before_id.is_none_or(|b| i.id < b))
                .filter(|i| kind.is_none_or(|k| i.kind == k))
                .filter(|i| doc_type.is_none_or(|d| i.doc_type.as_deref() == Some(d)))
                .filter(|i| author.is_none_or(|a| i.author.as_deref() == Some(a)))
                .filter(|i| language.is_none_or(|l| i.language.as_deref() == Some(l)))
                .filter(|i| git_branch.is_none_or(|g| i.git_branch.as_deref() == Some(g)))
                .filter(|i| classification.is_none_or(|c| i.classification.as_deref() == Some(c)))
                .take(limit as usize)
                .cloned()
                .collect())
        }

        fn stats(&self) -> Result<TimelineStats, String> {
            if self.fail {
                return Err("store is broken".into());
            }
            Ok(TimelineStats {
                events: self.items.len() as i64,
                files: self.items.len() as i64,
                // FakeStore has no extraction queue to model; every real test
                // of the gauge itself goes through `SqliteTimeline`.
                pending_extractions: 0,
            })
        }

        fn facets(&self, field: &str) -> Result<Vec<(String, i64)>, String> {
            if self.fail {
                return Err("store is broken".into());
            }
            let value = |i: &TimelineItem| -> Option<String> {
                match field {
                    "doc_type" => i.doc_type.clone(),
                    "author" => i.author.clone(),
                    "language" => i.language.clone(),
                    "git_branch" => i.git_branch.clone(),
                    "classification" => i.classification.clone(),
                    _ => None,
                }
            };
            let mut counts: Vec<(String, i64)> = Vec::new();
            for v in self.items.iter().filter_map(value) {
                match counts.iter_mut().find(|(existing, _)| *existing == v) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((v, 1)),
                }
            }
            counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            Ok(counts)
        }
    }
}

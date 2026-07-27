//! Serving M03 semantic search.
//!
//! Mirrors `timeline`'s own shape and reasoning almost exactly — a trait so
//! the HTTP layer is testable without a store, `Arc<dyn ...>` so the daemon
//! can hand in a real implementation without this crate depending on a
//! storage engine.
//!
//! # Why a second trait, not a method on `TimelineStore`
//!
//! `docs/m03-semantic-search.md`'s own "Next" section calls this out
//! explicitly: search should be "a new trait rather than a direct
//! `dafs-api` → `dafs-enrich`/`dafs-store` dependency". Answering a search
//! query means embedding the query text (`dafs_enrich::embed`, a network
//! call to a user-configured LLM endpoint) before it can even reach
//! `dafs_store::embeddings::search` — a different shape of work from every
//! `TimelineStore` method, none of which touch the network. Folding it into
//! `TimelineStore` would mean every implementor (including
//! `timeline::testing::FakeStore`) grows a method about embeddings whether
//! or not it has any, and would make "search is configured" indistinguishable
//! from "the timeline store is attached" — they are independently optional
//! today (a daemon can watch and extract with no LLM endpoint at all) and
//! should stay that way at the trait boundary, not just in `AppState`.

use std::sync::Arc;

use serde::Serialize;

use crate::timeline::TimelineItem;

/// One search result: a timeline row plus how far its embedding sat from the
/// query's, closest (smallest) first.
///
/// `#[serde(flatten)]` on `item` so a client sees exactly the same shape as
/// an `/events` row with one extra `distance` field, rather than a nested
/// `{"item": {...}}` envelope it would need a second, search-specific
/// deserializer to unwrap.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchHit {
    pub distance: f64,
    #[serde(flatten)]
    pub item: TimelineItem,
}

/// Facet filters for a search, exact-match against the same five columns
/// `/events`' own facet filters narrow — a search is still over files that
/// have (or lack) the same extracted/enriched metadata, so it gets the same
/// filter vocabulary rather than a search-specific one.
///
/// All-`None` (via `Default`) means no filter at all, same as an absent
/// query-string field on `/events`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchFilters {
    pub doc_type: Option<String>,
    pub author: Option<String>,
    pub language: Option<String>,
    pub git_branch: Option<String>,
    pub classification: Option<String>,
}

impl SearchFilters {
    /// Whether every field is unset — the common case, and the one where an
    /// implementation can skip filtering work entirely rather than running a
    /// no-op check per candidate.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// What the search handler needs from a store.
///
/// Errors are `String`, same reasoning as `TimelineStore`'s own: the HTTP
/// layer turns them into a response and a log line either way, and a richer
/// type here would leak `dafs-store`'s and `dafs-enrich`'s error taxonomies
/// into this crate for no gain.
pub trait SearchStore: Send + Sync + 'static {
    /// Embed `query`, apply `filters`, and return the nearest-neighbour
    /// files among the survivors, closest first, capped at `limit`.
    ///
    /// A filtered search may return fewer than `limit` hits even when more
    /// files exist overall: nearest-neighbour search has no way to filter
    /// *before* ranking (see `SqliteSearch`'s own docs on how it oversamples
    /// to compensate), so a filter that excludes most of the corpus can
    /// exhaust the candidate pool before `limit` survivors are found. That
    /// is a best-effort trade-off, not a bug — the alternative is scanning
    /// arbitrarily deep into the ranked list to guarantee exactly `limit`.
    fn search(
        &self,
        query: &str,
        limit: u32,
        filters: &SearchFilters,
    ) -> Result<Vec<SearchHit>, String>;
}

/// A shared, type-erased [`SearchStore`].
pub type SearchReader = Arc<dyn SearchStore>;

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// An in-memory store for testing the HTTP layer without a database or a
    /// network-reaching embedding client.
    pub struct FakeSearchStore {
        pub hits: Vec<SearchHit>,
        pub fail: bool,
    }

    impl SearchStore for FakeSearchStore {
        fn search(
            &self,
            _query: &str,
            limit: u32,
            filters: &SearchFilters,
        ) -> Result<Vec<SearchHit>, String> {
            if self.fail {
                return Err("search store is broken".into());
            }
            Ok(self
                .hits
                .iter()
                .filter(|h| matches(&filters.doc_type, &h.item.doc_type))
                .filter(|h| matches(&filters.author, &h.item.author))
                .filter(|h| matches(&filters.language, &h.item.language))
                .filter(|h| matches(&filters.git_branch, &h.item.git_branch))
                .filter(|h| matches(&filters.classification, &h.item.classification))
                .take(limit as usize)
                .cloned()
                .collect())
        }
    }

    /// `None` (no filter) always matches; `Some(v)` matches only an item
    /// whose own field is `Some` and equal — mirrors
    /// `timeline::testing::FakeStore`'s identical filter shape for
    /// `/events`.
    fn matches(filter: &Option<String>, field: &Option<String>) -> bool {
        filter.as_deref().is_none_or(|f| field.as_deref() == Some(f))
    }
}

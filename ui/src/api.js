//! Thin wrapper over the daemon's HTTP API.
//
// Separate from the rendering code so the two can be reasoned about apart: this
// module knows the wire format, and nothing else does. When M02a adds metadata
// fields and M03 adds search, this is the only file that has to learn about
// them.

/**
 * A row of the timeline as the daemon serves it.
 *
 * The M02a fields below are all optional and independently nullable:
 * extraction runs per-file and per-extractor, so a PDF carries
 * doc_type/title/page_count but never git_*, a source file under a repo
 * carries git_* but never page_count, and a file that has not been extracted
 * yet (or predates M02a entirely) carries none of them. Absence is the normal
 * case, not a partial-failure signal — rendering code must treat a missing
 * field exactly like an extractor that found nothing, never as an error.
 *
 * @typedef {Object} TimelineItem
 * @property {number} id
 * @property {string} kind
 * @property {string} path
 * @property {string} [previous_path]
 * @property {number} at_unix_ms
 * @property {number} [size_bytes]
 * @property {string} [doc_type]
 * @property {string} [title]
 * @property {string} [author]
 * @property {string} [language]
 * @property {number} [page_count]
 * @property {number} [word_count]
 * @property {string} [git_branch]
 * @property {string} [git_head_commit]
 * @property {string} [git_head_author]
 * @property {number} [git_head_at_unix] - Seconds, unlike `at_unix_ms` above:
 *   this is the commit's own timestamp, not when the daemon observed the file.
 */

/**
 * Fetch a page of the timeline.
 *
 * @param {{ kind?: string, beforeId?: number, limit?: number, facets?: Record<string, string> }} options
 *   `facets` maps a facet field name (doc_type, author, language, git_branch)
 *   to the single value to filter on; empty/absent values are omitted rather
 *   than sent as empty query params, matching how `kind` already behaves.
 * @returns {Promise<{events: TimelineItem[], next_before_id?: number}>}
 */
export async function fetchEvents({ kind = "", beforeId, limit = 50, facets = {} } = {}) {
  const params = new URLSearchParams();
  params.set("limit", String(limit));
  if (kind) params.set("kind", kind);
  if (beforeId != null) params.set("before_id", String(beforeId));
  for (const [field, value] of Object.entries(facets)) {
    if (value) params.set(field, value);
  }

  const response = await fetch(`/events?${params}`);
  if (!response.ok) {
    // Surface the status rather than a generic failure: a 503 means the store
    // is not attached yet, which is a different problem from a 500, and the
    // person reading the message is usually the one running the daemon.
    throw new Error(`/events returned HTTP ${response.status}`);
  }
  return response.json();
}

/**
 * Distinct values for one extracted-metadata facet, with counts, for building
 * filter controls.
 *
 * M02a-only endpoint: it does not exist before that lands, so a 404 here is
 * the expected response from today's daemon, not a real error. Callers must
 * treat any failure — 404 or otherwise — as "no facets available" and hide
 * the corresponding filter rather than surfacing it as a page error.
 *
 * @param {string} field - One of doc_type, author, language, git_branch.
 * @returns {Promise<Array<{value: string, count: number}>>}
 */
export async function fetchFacets(field) {
  const params = new URLSearchParams({ field });
  const response = await fetch(`/facets?${params}`);
  if (!response.ok) throw new Error(`/facets returned HTTP ${response.status}`);
  return response.json();
}

/**
 * One M03 semantic search result: a timeline row plus how far its embedding
 * sat from the query's, closest (smallest) first — mirrors
 * `dafs_api::search::SearchHit`'s `#[serde(flatten)]` shape, so every
 * `TimelineItem` field above is present here too, alongside `distance`.
 *
 * @typedef {TimelineItem & { distance: number }} SearchHit
 */

/**
 * Semantic search: embeds `q` against the daemon's configured embedding
 * model and returns the nearest files, closest first.
 *
 * 503 is the expected response when M03 embeddings aren't configured on this
 * daemon — the error's `status` field lets a caller distinguish "not
 * configured" from any other failure and word the message accordingly,
 * matching how `fetchEvents` already surfaces status rather than collapsing
 * every failure into one generic message.
 *
 * @param {string} q
 * @param {{ limit?: number, facets?: Record<string, string> }} [options]
 *   `facets` is the same shape `fetchEvents` takes and means the same thing:
 *   a facet field name mapped to the single value to filter on, with
 *   empty/absent values omitted rather than sent as empty query params.
 * @returns {Promise<{hits: SearchHit[]}>}
 */
export async function fetchSearch(q, { limit = 20, facets = {} } = {}) {
  const params = new URLSearchParams({ q, limit: String(limit) });
  for (const [field, value] of Object.entries(facets)) {
    if (value) params.set(field, value);
  }
  const response = await fetch(`/search?${params}`);
  if (!response.ok) {
    const error = new Error(`/search returned HTTP ${response.status}`);
    error.status = response.status;
    throw error;
  }
  return response.json();
}

/**
 * Daemon version and schema.
 */
export async function fetchVersion() {
  const response = await fetch("/version");
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return response.json();
}

/**
 * Counters from the Prometheus endpoint.
 *
 * Parsed from the scrape format rather than served as JSON somewhere else:
 * these exist for scrapers first, and a second representation would be a second
 * thing to keep correct. The parser is deliberately forgiving — a missing
 * metric yields `undefined` rather than throwing, because the status strip is
 * decoration and must never be the reason the timeline fails to render.
 */
export async function fetchMetrics() {
  const response = await fetch("/metrics");
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  const text = await response.text();

  const value = (name) => {
    const line = text.split("\n").find((l) => l.startsWith(`${name} `));
    if (!line) return undefined;
    const parsed = Number(line.slice(name.length + 1));
    return Number.isFinite(parsed) ? parsed : undefined;
  };

  return {
    residentBytes: value("dafs_resident_bytes"),
    events: value("dafs_events_total"),
    files: value("dafs_files_known"),
  };
}

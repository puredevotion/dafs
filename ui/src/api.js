//! Thin wrapper over the daemon's HTTP API.
//
// Separate from the rendering code so the two can be reasoned about apart: this
// module knows the wire format, and nothing else does. When M02a adds metadata
// fields and M03 adds search, this is the only file that has to learn about
// them.

/**
 * Fetch a page of the timeline.
 *
 * @param {{ kind?: string, beforeId?: number, limit?: number }} options
 * @returns {Promise<{events: Array, next_before_id?: number}>}
 */
export async function fetchEvents({ kind = "", beforeId, limit = 50 } = {}) {
  const params = new URLSearchParams();
  params.set("limit", String(limit));
  if (kind) params.set("kind", kind);
  if (beforeId != null) params.set("before_id", String(beforeId));

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

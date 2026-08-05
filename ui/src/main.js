// The timeline view.
//
// No framework. The page is one list that grows at the bottom and refreshes at
// the top; a component library would be more machinery than the problem has,
// and the bundle is embedded in a daemon with a 32 MiB ceiling. M02a's faceted
// filtering is the point to revisit that, against requirements rather than
// taste.

import "./style.css";
import { fetchEvents, fetchFacets, fetchMetrics, fetchSearch, fetchVersion } from "./api.js";
import { formatBytes, formatExact, formatWhen, groupByDay, splitPath } from "./format.js";

/** How often the status strip and newest events refresh. */
const POLL_MS = 5_000;
const PAGE_SIZE = 50;

/** Facet fields the daemon may expose, in the order the filter row shows them. */
const FACET_FIELDS = [
  { field: "doc_type", label: "Type" },
  { field: "author", label: "Author" },
  { field: "language", label: "Language" },
  { field: "git_branch", label: "Branch" },
];

/**
 * Extracted-metadata fields shown in a row's expansion, in display order.
 *
 * `format` converts the raw value to display text; its absence means
 * `String(value)` is already right. Only fields present on a given entry are
 * rendered, so a row with no M02a data (today's daemon, or a file no
 * extractor matched) expands to nothing rather than a wall of blanks.
 */
const DETAIL_FIELDS = [
  { key: "doc_type", label: "Type" },
  { key: "title", label: "Title" },
  { key: "author", label: "Author" },
  { key: "language", label: "Language" },
  { key: "page_count", label: "Pages" },
  { key: "word_count", label: "Words", format: (v) => v.toLocaleString() },
  { key: "git_branch", label: "Branch" },
  { key: "git_head_commit", label: "Commit" },
  { key: "git_head_author", label: "Commit author" },
  // Seconds, not the milliseconds formatExact otherwise expects — this is the
  // commit's own timestamp, not an observation time.
  { key: "git_head_at_unix", label: "Commit time", format: (v) => formatExact(v * 1000) },
];

const el = {
  status: document.getElementById("status"),
  files: document.getElementById("files"),
  eventCount: document.getElementById("event-count"),
  rss: document.getElementById("rss"),
  timeline: document.getElementById("timeline"),
  message: document.getElementById("message"),
  more: document.getElementById("more"),
  filters: document.querySelector(".filters"),
  searchForm: document.getElementById("search-form"),
  searchInput: document.getElementById("search-input"),
  searchClear: document.getElementById("search-clear"),
  // Populated by loadFacets, and only inserted into the page at all once at
  // least one facet field comes back — against today's daemon /facets 404s
  // for every field, so this stays null and the row never appears.
  facets: null,
};

const state = {
  kind: "",
  /** Selected value per facet field; empty string means "no filter". */
  facets: {},
  /** Every entry currently displayed, newest first. */
  entries: [],
  /** Cursor for the next page, or null when the end has been reached. */
  nextBeforeId: null,
  /** True once a page has come back empty, so "Load more" can be hidden. */
  exhausted: false,
  loading: false,
  /**
   * `null` outside search mode. While searching, the timeline's own kind/
   * facet filters and pagination don't apply — a search result set is a
   * ranked list, not a filtered page of the same append-only log — so
   * search replaces the timeline's rendering rather than filtering it.
   */
  searchHits: null,
  /** The query text the current `searchHits` were found for, for the
   * "no matches" message. Meaningless while `searchHits` is `null`. */
  searchQuery: "",
};

/** Replace the list with `entries`, grouped by day — or, in search mode,
 * with the current search hits, ranked rather than grouped. */
function render() {
  if (state.searchHits !== null) {
    renderSearchHits();
    return;
  }

  if (state.entries.length === 0) {
    el.timeline.replaceChildren();
    showMessage(
      state.kind
        ? `No ${state.kind} events recorded.`
        : "Nothing recorded yet. Start the daemon with --watch <directory> to observe a folder.",
    );
    el.more.hidden = true;
    return;
  }

  hideMessage();

  const fragment = document.createDocumentFragment();
  for (const group of groupByDay(state.entries)) {
    fragment.append(dayHeading(group.label));
    for (const entry of group.items) {
      fragment.append(eventRow(entry));
    }
  }

  // One replaceChildren rather than incremental patching: the list is at most a
  // few hundred rows, and rebuilding it is both simpler and fast enough that
  // the difference is not perceptible.
  el.timeline.replaceChildren(fragment);
  el.more.hidden = state.exhausted;
}

/** Render the current search hits, closest match first. No day grouping and
 * no "Load more" — a search result is a fixed ranked list, not a page of an
 * append-only log. */
function renderSearchHits() {
  el.more.hidden = true;

  if (state.searchHits.length === 0) {
    el.timeline.replaceChildren();
    showMessage(`No matches for “${state.searchQuery}”.`);
    return;
  }

  hideMessage();
  const fragment = document.createDocumentFragment();
  for (const hit of state.searchHits) {
    fragment.append(searchHitRow(hit));
  }
  el.timeline.replaceChildren(fragment);
}

function dayHeading(label) {
  const li = document.createElement("li");
  li.className = "day";
  li.setAttribute("role", "presentation");
  li.textContent = label;
  return li;
}

function eventRow(entry) {
  const li = document.createElement("li");
  li.className = "event";

  const kind = document.createElement("span");
  kind.className = `kind kind-${entry.kind}`;
  kind.textContent = entry.kind;
  li.append(kind);

  const path = document.createElement("span");
  path.className = "path";
  const { directory, name } = splitPath(entry.path);

  // textContent throughout, never innerHTML: a path is arbitrary user data and
  // a filename containing markup must render as that filename, not as markup.
  const dir = document.createElement("span");
  dir.className = "dir";
  dir.textContent = directory;
  const base = document.createElement("span");
  base.className = "name";
  base.textContent = name;
  path.append(dir, base);
  path.title = entry.path;
  li.append(path);

  if (entry.previous_path) {
    const from = document.createElement("span");
    from.className = "from";
    from.textContent = `from ${entry.previous_path}`;
    li.append(from);
  }

  const meta = document.createElement("span");
  meta.className = "meta";
  if (entry.size_bytes != null) {
    const size = document.createElement("span");
    size.className = "size";
    size.textContent = formatBytes(entry.size_bytes);
    meta.append(size);
  }
  const when = document.createElement("time");
  when.dateTime = new Date(entry.at_unix_ms).toISOString();
  when.textContent = formatWhen(entry.at_unix_ms);
  when.title = formatExact(entry.at_unix_ms);
  meta.append(when);
  li.append(meta);

  // Only fields present on this entry, so a row with nothing extracted (every
  // row, against today's pre-M02a daemon) gets no panel, no click handler, no
  // extra attributes — it renders exactly as it did before this feature.
  const present = DETAIL_FIELDS.filter(({ key }) => entry[key] != null);
  if (present.length > 0) {
    li.append(detailsPanel(entry, present));
    makeExpandable(li);
  }

  return li;
}

/**
 * A search hit: the same path/metadata rendering `eventRow` uses, with a
 * match-strength badge in place of the created/modified/deleted/renamed
 * kind badge — a search result has no "kind", but it does have a ranking,
 * and showing that (rather than silently ordering the list by it) is what
 * tells a reader why the top row is the top row.
 */
function searchHitRow(hit) {
  const li = document.createElement("li");
  li.className = "event search-hit";

  const badge = document.createElement("span");
  badge.className = "kind distance";
  badge.textContent = formatDistance(hit.distance);
  badge.title = `Euclidean distance: ${hit.distance}`;
  li.append(badge);

  const path = document.createElement("span");
  path.className = "path";
  const { directory, name } = splitPath(hit.path);

  const dir = document.createElement("span");
  dir.className = "dir";
  dir.textContent = directory;
  const base = document.createElement("span");
  base.className = "name";
  base.textContent = name;
  path.append(dir, base);
  path.title = hit.path;
  li.append(path);

  if (hit.summary) {
    const summary = document.createElement("span");
    summary.className = "summary";
    summary.textContent = hit.summary;
    li.append(summary);
  }

  const present = DETAIL_FIELDS.filter(({ key }) => hit[key] != null);
  if (present.length > 0) {
    li.append(detailsPanel(hit, present));
    makeExpandable(li);
  }

  return li;
}

/** A short, unitless closeness reading — smaller is a better match. Rounded
 * to 2 decimal places: the raw Euclidean distance is a real number with no
 * inherent scale a reader can eyeball, so the exact value goes in the title
 * tooltip instead (see `searchHitRow`) and this is only ever a relative cue
 * for comparing rows against each other. */
function formatDistance(distance) {
  return distance.toFixed(2);
}

/** A hidden-by-default definition list of the extracted fields present on `entry`. */
function detailsPanel(entry, fields) {
  const dl = document.createElement("dl");
  dl.className = "details";
  dl.hidden = true;

  for (const { key, label, format } of fields) {
    const dt = document.createElement("dt");
    dt.textContent = label;
    const dd = document.createElement("dd");
    // textContent, same reasoning as the path above: extracted text (a
    // document title, a git author) is still arbitrary user data.
    dd.textContent = format ? format(entry[key]) : String(entry[key]);
    dl.append(dt, dd);
  }

  return dl;
}

/** Wire a row that has a details panel appended to it to toggle that panel on click or Enter/Space. */
function makeExpandable(li) {
  const panel = li.querySelector(".details");
  li.classList.add("expandable");
  li.tabIndex = 0;
  li.setAttribute("role", "button");
  li.setAttribute("aria-expanded", "false");

  const toggle = () => {
    panel.hidden = !panel.hidden;
    li.setAttribute("aria-expanded", String(!panel.hidden));
  };

  li.addEventListener("click", toggle);
  li.addEventListener("keydown", (event) => {
    if (event.key !== "Enter" && event.key !== " ") return;
    // Space must not also scroll the page while the row is focused.
    event.preventDefault();
    toggle();
  });
}

function showMessage(text, isError = false) {
  el.message.textContent = text;
  el.message.classList.toggle("error", isError);
  el.message.hidden = false;
}

function hideMessage() {
  el.message.hidden = true;
}

/** Load the first page for the current filter, replacing what is shown. */
async function loadFirstPage() {
  // Loading the timeline's first page always means leaving search mode —
  // the two views are mutually exclusive (see `state.searchHits`'s own
  // comment), and every caller of this function (kind filters, facet
  // filters, the initial page load) means "show me the timeline", not
  // "refine the current search".
  state.searchHits = null;

  state.loading = true;
  try {
    const page = await fetchEvents({ kind: state.kind, limit: PAGE_SIZE, facets: state.facets });
    state.entries = page.events;
    state.nextBeforeId = page.next_before_id ?? null;
    state.exhausted = page.events.length < PAGE_SIZE;
    render();
  } catch (error) {
    showMessage(`Could not load the timeline: ${error.message}`, true);
  } finally {
    state.loading = false;
  }
}

/** Append the next page. */
async function loadMore() {
  if (state.loading || state.exhausted || state.nextBeforeId == null) return;

  state.loading = true;
  el.more.disabled = true;
  try {
    const page = await fetchEvents({
      kind: state.kind,
      beforeId: state.nextBeforeId,
      limit: PAGE_SIZE,
      facets: state.facets,
    });

    if (page.events.length === 0) {
      state.exhausted = true;
    } else {
      state.entries.push(...page.events);
      state.nextBeforeId = page.next_before_id ?? null;
      // A short page means the end, so the button disappears rather than
      // offering one more click that returns nothing.
      state.exhausted = page.events.length < PAGE_SIZE || state.nextBeforeId == null;
    }
    render();
  } catch (error) {
    showMessage(`Could not load more: ${error.message}`, true);
  } finally {
    state.loading = false;
    el.more.disabled = false;
  }
}

/**
 * Poll for events newer than the newest one shown, and prepend them.
 *
 * Refetching the first page rather than diffing the whole list: the timeline is
 * append-mostly, so the newest page is where changes appear, and re-rendering
 * from it keeps this simple. Anything the user has paged into stays put.
 */
async function pollNewest() {
  if (state.loading) return;

  try {
    const page = await fetchEvents({ kind: state.kind, limit: PAGE_SIZE, facets: state.facets });
    if (page.events.length === 0) return;

    const known = new Set(state.entries.map((e) => e.id));
    const fresh = page.events.filter((e) => !known.has(e.id));
    if (fresh.length === 0) return;

    state.entries.unshift(...fresh);
    render();
  } catch {
    // A failed poll is not worth an error message — the next one is five
    // seconds away, and the status strip already shows the daemon is down.
  }
}

/** Refresh the status strip. */
async function pollStatus() {
  try {
    await fetchVersion();
    el.status.textContent = "up";
    el.status.classList.remove("bad");
  } catch (error) {
    el.status.textContent = `unreachable (${error.message})`;
    el.status.classList.add("bad");
    return;
  }

  try {
    const metrics = await fetchMetrics();
    if (metrics.residentBytes != null) {
      el.rss.textContent = formatBytes(metrics.residentBytes);
    }
    if (metrics.files != null) el.files.textContent = metrics.files.toLocaleString();
    if (metrics.events != null) el.eventCount.textContent = metrics.events.toLocaleString();
  } catch {
    // Counters are decoration; the timeline below is the real content.
  }
}

/**
 * Build the facet-filter row from whichever fields the daemon has data for.
 *
 * Each field is fetched independently and a failure — 404 against today's
 * pre-M02a daemon, or any other error — just drops that one field, matching
 * fetchFacets' contract. If every field fails, no container is ever inserted
 * into the page: there is no empty filter row to explain away.
 */
async function loadFacets() {
  const loaded = [];
  for (const { field, label } of FACET_FIELDS) {
    try {
      const options = await fetchFacets(field);
      if (Array.isArray(options) && options.length > 0) loaded.push({ field, label, options });
    } catch {
      // Expected against today's daemon (no /facets endpoint yet); nothing to
      // report, the field's filter simply does not appear.
    }
  }
  if (loaded.length === 0) return;

  const container = document.createElement("nav");
  container.className = "facets";
  container.setAttribute("aria-label", "Filter by extracted metadata");

  for (const { field, label, options } of loaded) {
    state.facets[field] = "";

    const wrapper = document.createElement("label");
    wrapper.className = "facet";
    wrapper.append(`${label} `);

    const select = document.createElement("select");
    select.dataset.facet = field;

    const all = document.createElement("option");
    all.value = "";
    all.textContent = "All";
    select.append(all);

    for (const { value, count } of options) {
      const option = document.createElement("option");
      option.value = value;
      option.textContent = `${value} (${count})`;
      select.append(option);
    }

    wrapper.append(select);
    container.append(wrapper);
  }

  container.addEventListener("change", (event) => {
    const select = event.target.closest("select[data-facet]");
    if (!select) return;
    state.facets[select.dataset.facet] = select.value;
    // A facet change while a search is active narrows *that* search rather
    // than leaving it — the facets apply just as much to a ranked result set
    // as to the timeline, and silently dropping back to the timeline on a
    // filter change would be a surprising way to lose the query.
    if (state.searchHits !== null) {
      runSearch(state.searchQuery);
    } else {
      loadFirstPage();
    }
  });

  el.filters.insertAdjacentElement("afterend", container);
  el.facets = container;
}

/** Enter (or replace) search mode with `hits` for `query`. */
function enterSearchMode(query, hits) {
  state.searchQuery = query;
  state.searchHits = hits;
  el.searchClear.hidden = false;
  render();
}

/** Leave search mode and go back to the timeline. A no-op if not searching. */
function exitSearchMode() {
  if (state.searchHits === null) return;
  el.searchInput.value = "";
  el.searchClear.hidden = true;
  loadFirstPage();
}

/**
 * Run a search for `query` against `/search`, entering search mode with the
 * result. An empty (post-trim) query exits search mode instead of asking the
 * daemon to embed nothing.
 *
 * A 503 gets a specific message rather than the generic failure text — it is
 * the expected response from a daemon with no `--llm-embedding-model`
 * configured (see `fetchSearch`'s own docs), not a real error, and reads very
 * differently to someone who just tried to use the feature.
 */
async function runSearch(query) {
  const trimmed = query.trim();
  if (!trimmed) {
    exitSearchMode();
    return;
  }

  state.loading = true;
  try {
    const { hits } = await fetchSearch(trimmed, { facets: state.facets });
    enterSearchMode(trimmed, hits);
  } catch (error) {
    el.timeline.replaceChildren();
    el.more.hidden = true;
    showMessage(
      error.status === 503
        ? "Semantic search is not configured on this daemon."
        : `Search failed: ${error.message}`,
      true,
    );
  } finally {
    state.loading = false;
  }
}

el.searchForm.addEventListener("submit", (event) => {
  event.preventDefault();
  runSearch(el.searchInput.value);
});

el.searchClear.addEventListener("click", exitSearchMode);

el.filters.addEventListener("click", (event) => {
  const button = event.target.closest("button[data-kind]");
  if (!button) return;

  state.kind = button.dataset.kind;
  for (const other of el.filters.querySelectorAll("button")) {
    const active = other === button;
    other.classList.toggle("active", active);
    other.setAttribute("aria-pressed", String(active));
  }
  loadFirstPage();
});

el.more.addEventListener("click", loadMore);

loadFirstPage();
loadFacets();
pollStatus();
setInterval(pollStatus, POLL_MS);
setInterval(pollNewest, POLL_MS);

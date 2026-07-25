// The timeline view.
//
// No framework. The page is one list that grows at the bottom and refreshes at
// the top; a component library would be more machinery than the problem has,
// and the bundle is embedded in a daemon with a 32 MiB ceiling. M02a's faceted
// filtering is the point to revisit that, against requirements rather than
// taste.

import "./style.css";
import { fetchEvents, fetchMetrics, fetchVersion } from "./api.js";
import { formatBytes, formatExact, formatWhen, groupByDay, splitPath } from "./format.js";

/** How often the status strip and newest events refresh. */
const POLL_MS = 5_000;
const PAGE_SIZE = 50;

const el = {
  status: document.getElementById("status"),
  files: document.getElementById("files"),
  eventCount: document.getElementById("event-count"),
  rss: document.getElementById("rss"),
  timeline: document.getElementById("timeline"),
  message: document.getElementById("message"),
  more: document.getElementById("more"),
  filters: document.querySelector(".filters"),
};

const state = {
  kind: "",
  /** Every entry currently displayed, newest first. */
  entries: [],
  /** Cursor for the next page, or null when the end has been reached. */
  nextBeforeId: null,
  /** True once a page has come back empty, so "Load more" can be hidden. */
  exhausted: false,
  loading: false,
};

/** Replace the list with `entries`, grouped by day. */
function render() {
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

  return li;
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
  state.loading = true;
  try {
    const page = await fetchEvents({ kind: state.kind, limit: PAGE_SIZE });
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
    const page = await fetchEvents({ kind: state.kind, limit: PAGE_SIZE });
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
pollStatus();
setInterval(pollStatus, POLL_MS);
setInterval(pollNewest, POLL_MS);

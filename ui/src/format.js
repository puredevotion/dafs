// Formatting helpers. Pure functions, no DOM — so they are the part of the UI
// that can be reasoned about (and, later, tested) without a browser.

/**
 * A path split into the part that gives context and the part that identifies
 * the file, so the timeline can de-emphasise the former.
 *
 * @param {string} path
 */
export function splitPath(path) {
  const index = path.lastIndexOf("/");
  if (index < 0) return { directory: "", name: path };
  return { directory: path.slice(0, index + 1), name: path.slice(index + 1) };
}

/**
 * Bytes as a short human string.
 *
 * Binary units, matching how the memory budget and every file manager on the
 * target platform report sizes.
 */
export function formatBytes(bytes) {
  if (bytes == null) return "";
  if (bytes < 1024) return `${bytes} B`;

  const units = ["KiB", "MiB", "GiB", "TiB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  // One decimal below 10, none above: "1.4 MiB" is useful, "847.3 KiB" is
  // false precision for a file listing.
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

/**
 * A timestamp as a relative phrase, falling back to a date once "days ago"
 * stops being the useful framing.
 *
 * The timeline answers "what did I work on today?", so recent times are what
 * matter and are worth spelling out precisely.
 */
export function formatWhen(unixMs, now = Date.now()) {
  const seconds = Math.round((now - unixMs) / 1000);

  if (seconds < 0) return "just now"; // clock skew; not worth showing a negative
  if (seconds < 60) return "just now";
  if (seconds < 3600) {
    const minutes = Math.floor(seconds / 60);
    return `${minutes} min ago`;
  }
  if (seconds < 86_400) {
    const hours = Math.floor(seconds / 3600);
    return `${hours} hour${hours === 1 ? "" : "s"} ago`;
  }
  if (seconds < 7 * 86_400) {
    const days = Math.floor(seconds / 86_400);
    return `${days} day${days === 1 ? "" : "s"} ago`;
  }

  return new Date(unixMs).toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}

/**
 * The exact time, for a tooltip. The relative phrase above is easier to read;
 * this is what someone reaches for when they need the real answer.
 */
export function formatExact(unixMs) {
  return new Date(unixMs).toLocaleString();
}

/**
 * Group entries under day headings.
 *
 * Returns an array of `{ label, items }` preserving the input order, which is
 * already newest-first from the API.
 */
export function groupByDay(entries, now = Date.now()) {
  const dayOf = (ms) => new Date(ms).toDateString();
  const today = dayOf(now);
  const yesterday = dayOf(now - 86_400_000);

  const groups = [];
  for (const entry of entries) {
    const day = dayOf(entry.at_unix_ms);
    const label =
      day === today
        ? "Today"
        : day === yesterday
          ? "Yesterday"
          : new Date(entry.at_unix_ms).toLocaleDateString(undefined, {
              weekday: "long",
              month: "short",
              day: "numeric",
            });

    const last = groups[groups.length - 1];
    if (last && last.label === label) {
      last.items.push(entry);
    } else {
      groups.push({ label, items: [entry] });
    }
  }
  return groups;
}

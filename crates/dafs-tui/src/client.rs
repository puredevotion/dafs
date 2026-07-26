//! A thin, read-only client for the daemon's existing HTTP surface
//! (`crates/dafs-api`). No new daemon endpoints — everything here is already
//! served today.

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct VersionResp {
    version: String,
    schema_version: u32,
    uptime_seconds: u64,
}

#[derive(Debug, Deserialize)]
struct ReadyzResp {
    ready: bool,
}

#[derive(Debug, Deserialize)]
pub struct TimelineItem {
    pub id: i64,
    pub path: String,
    pub kind: String,
    pub at_unix_ms: i64,
    #[serde(default)]
    pub size_bytes: Option<i64>,
    #[serde(default)]
    pub is_dir: bool,
}

#[derive(Debug, Deserialize)]
struct EventsResp {
    events: Vec<TimelineItem>,
}

#[derive(Debug, Deserialize)]
struct WatchResp {
    roots: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct LogsResp {
    lines: Vec<String>,
}

/// How many log lines to ask the daemon for each poll. Generous relative to
/// what the scrollable panel shows at once — the point is to have enough
/// history to actually scroll back through, not just the latest screenful.
pub const LOG_FETCH_LIMIT: u32 = 500;

/// A snapshot of everything the TUI shows, refreshed on every tick.
///
/// Fetched as four independent requests rather than one combined call: there
/// is no such endpoint, and adding one to the daemon just for this client
/// would be new API surface for a read-only monitor to invent. `readyz` alone
/// answering is enough to call the daemon "connected" — the others degrade
/// individually rather than failing the whole snapshot, the same principle
/// `/metrics` itself uses when the store is unhappy.
#[derive(Debug, Default)]
pub struct Status {
    pub connected: bool,
    pub ready: bool,
    pub version: Option<String>,
    pub schema_version: Option<u32>,
    pub uptime_seconds: Option<u64>,
    pub resident_bytes: Option<u64>,
    pub events_total: Option<u64>,
    pub files_known: Option<u64>,
    pub events: Vec<TimelineItem>,
    /// Directories the daemon is currently watching. Always shown, not just
    /// when empty — the whole point is that a user should never have to
    /// wonder what a running daemon is actually pointed at, which is exactly
    /// what caused the confusion this field exists to prevent (a stale
    /// daemon and a new one watching different directories, indistinguishable
    /// from the outside without asking it directly).
    pub watch_roots: Vec<String>,
    /// Recent formatted log lines, oldest first — the daemon's own ring
    /// buffer is the source of truth; this is replaced wholesale each poll
    /// rather than accumulated locally, so it's always what the daemon
    /// actually has, not a client-side guess at what's new.
    pub log_lines: Vec<String>,
    pub error: Option<String>,
}

pub struct Client {
    base_url: String,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(base_url: String) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(2)).build();
        Self { base_url, agent }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    pub fn poll(&self, event_limit: u32) -> Status {
        let readyz = self.agent.get(&self.url("/readyz")).call();
        let connected = readyz.is_ok() || matches!(&readyz, Err(ureq::Error::Status(_, _)));

        if !connected {
            let error = readyz.err().map(|e| e.to_string());
            return Status { connected: false, error, ..Status::default() };
        }

        let ready = readyz
            .ok()
            .and_then(|r| r.into_json::<ReadyzResp>().ok())
            .map(|r| r.ready)
            .unwrap_or(false);

        let version = self
            .agent
            .get(&self.url("/version"))
            .call()
            .ok()
            .and_then(|r| r.into_json::<VersionResp>().ok());

        let metrics = self
            .agent
            .get(&self.url("/metrics"))
            .call()
            .ok()
            .and_then(|r| r.into_string().ok())
            .map(|body| parse_prometheus(&body))
            .unwrap_or_default();

        let events = self
            .agent
            .get(&self.url("/events"))
            .query("limit", &event_limit.to_string())
            .call()
            .ok()
            .and_then(|r| r.into_json::<EventsResp>().ok())
            .map(|r| r.events)
            .unwrap_or_default();

        let watch_roots = self
            .agent
            .get(&self.url("/watch"))
            .call()
            .ok()
            .and_then(|r| r.into_json::<WatchResp>().ok())
            .map(|r| r.roots)
            .unwrap_or_default();

        let log_lines = self
            .agent
            .get(&self.url("/logs"))
            .query("limit", &LOG_FETCH_LIMIT.to_string())
            .call()
            .ok()
            .and_then(|r| r.into_json::<LogsResp>().ok())
            .map(|r| r.lines)
            .unwrap_or_default();

        Status {
            connected: true,
            ready,
            version: version.as_ref().map(|v| v.version.clone()),
            schema_version: version.as_ref().map(|v| v.schema_version),
            uptime_seconds: version.as_ref().map(|v| v.uptime_seconds),
            resident_bytes: metrics.get("dafs_resident_bytes").map(|v| *v as u64),
            events_total: metrics.get("dafs_events_total").map(|v| *v as u64),
            files_known: metrics.get("dafs_files_known").map(|v| *v as u64),
            events,
            watch_roots,
            log_lines,
            error: None,
        }
    }
}

/// Parse the subset of the Prometheus text exposition format `/metrics` uses:
/// comment lines starting with `#`, and otherwise `name value` pairs.
fn parse_prometheus(body: &str) -> HashMap<String, f64> {
    body.lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|line| {
            let (name, value) = line.split_once(' ')?;
            Some((name.to_string(), value.parse::<f64>().ok()?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_metrics_output() {
        let body = "# HELP dafs_ready ...\n# TYPE dafs_ready gauge\ndafs_ready 1\n\
                     dafs_resident_bytes 9805824\ndafs_events_total 42\n";
        let parsed = parse_prometheus(body);
        assert_eq!(parsed.get("dafs_ready"), Some(&1.0));
        assert_eq!(parsed.get("dafs_resident_bytes"), Some(&9_805_824.0));
        assert_eq!(parsed.get("dafs_events_total"), Some(&42.0));
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let parsed = parse_prometheus("# just a comment\n\n");
        assert!(parsed.is_empty());
    }

    #[test]
    fn skips_a_malformed_line_rather_than_panicking() {
        // No space, so no value to split on — must be dropped, not crash the
        // poll that every other field's display depends on.
        let parsed = parse_prometheus("dafs_ready\ndafs_events_total 3\n");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed.get("dafs_events_total"), Some(&3.0));
    }
}

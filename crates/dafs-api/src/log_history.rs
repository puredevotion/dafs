//! An in-memory ring buffer of recent formatted log lines.
//!
//! Exists so a monitor (`dafs-tui`) can show what the daemon is actually
//! doing without shelling in to tail a log file — the daemon detaches by
//! default now (`--detach`, on unless told otherwise), which is exactly what
//! makes its own log output invisible unless something surfaces it back.
//!
//! No dependency on `tracing-subscriber` here deliberately: this crate stays
//! subscriber-agnostic, like the rest of it. The daemon wires this in as a
//! second `fmt::layer()` writer (see its `init_tracing`) via
//! `tracing-subscriber`'s blanket `MakeWriter` impl for `Fn() -> W where W:
//! Write` — a plain closure returning [`LogHistoryWriter`] satisfies that
//! with no trait impl needed on either side.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Bounded so a daemon that runs for weeks doesn't grow this with its own
/// uptime — old lines are simply gone once the ring is full, the same
/// tradeoff every bounded log buffer makes. Generous relative to what a
/// monitor actually displays at once (dafs-tui shows a scrollable window
/// well under this).
const CAPACITY: usize = 2000;

#[derive(Clone)]
pub struct LogHistory {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl LogHistory {
    pub fn new() -> Self {
        Self { lines: Arc::new(Mutex::new(VecDeque::with_capacity(CAPACITY))) }
    }

    /// A `Write` implementation appending to this buffer, one stored entry
    /// per physical line — a multi-line chunk from a single write() call is
    /// split rather than stored as one blob, so `recent` returns lines a
    /// caller can actually scroll through.
    pub fn writer(&self) -> LogHistoryWriter {
        LogHistoryWriter { history: self.clone(), buffer: String::new() }
    }

    fn push_line(&self, line: &str) {
        if line.is_empty() {
            return;
        }
        let mut guard = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        if guard.len() == CAPACITY {
            guard.pop_front();
        }
        guard.push_back(line.to_string());
    }

    /// The most recent `limit` lines, oldest first — the order a scrollable
    /// view wants to render top-to-bottom.
    pub fn recent(&self, limit: usize) -> Vec<String> {
        let guard = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        let skip = guard.len().saturating_sub(limit);
        guard.iter().skip(skip).cloned().collect()
    }
}

impl Default for LogHistory {
    fn default() -> Self {
        Self::new()
    }
}

/// A single logical log event can reach `write()` as more than one call —
/// `tracing-subscriber`'s formatter writes a line's parts incrementally to
/// whatever writer it's handed, not necessarily as one complete chunk. This
/// buffers across calls and only commits a line to the shared ring once a
/// `\n` actually closes it, so one event never fragments into several
/// spurious ring entries.
pub struct LogHistoryWriter {
    history: LogHistory,
    buffer: String,
}

impl std::io::Write for LogHistoryWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(text) = std::str::from_utf8(buf) {
            self.buffer.push_str(text);
            while let Some(pos) = self.buffer.find('\n') {
                let line = self.buffer[..pos].to_string();
                self.history.push_line(&line);
                self.buffer.drain(..=pos);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for LogHistoryWriter {
    /// Whatever never saw a trailing `\n` is still worth keeping — dropping
    /// it silently would be a quieter, harder-to-notice version of the exact
    /// bug this buffering exists to avoid.
    fn drop(&mut self) {
        if !self.buffer.is_empty() {
            self.history.push_line(&std::mem::take(&mut self.buffer));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn a_write_call_splits_into_separate_lines() {
        let history = LogHistory::new();
        history.writer().write_all(b"line one\nline two\n").unwrap();
        assert_eq!(history.recent(10), vec!["line one", "line two"]);
    }

    #[test]
    fn empty_lines_are_dropped() {
        let history = LogHistory::new();
        history.writer().write_all(b"a\n\nb\n").unwrap();
        assert_eq!(history.recent(10), vec!["a", "b"]);
    }

    #[test]
    fn recent_returns_only_the_last_n_oldest_first() {
        let history = LogHistory::new();
        for n in 0..5 {
            history.writer().write_all(format!("line {n}\n").as_bytes()).unwrap();
        }
        assert_eq!(history.recent(2), vec!["line 3", "line 4"]);
    }

    #[test]
    fn the_ring_drops_the_oldest_once_full() {
        let history = LogHistory::new();
        for n in 0..(CAPACITY + 10) {
            history.writer().write_all(format!("line {n}\n").as_bytes()).unwrap();
        }
        let all = history.recent(CAPACITY + 10);
        assert_eq!(all.len(), CAPACITY, "the ring should never grow past its capacity");
        assert_eq!(all[0], format!("line {}", 10), "the oldest lines should have been dropped");
    }
}

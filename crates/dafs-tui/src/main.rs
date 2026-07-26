//! `dafs-tui`: a read-only terminal status monitor for a running `dafs`
//! daemon.
//!
//! Talks only to the HTTP surface `dafs-api` already serves (`/readyz`,
//! `/version`, `/metrics`, `/events`) — no daemon changes, no control-plane
//! actions. M01's daemon has no control surface yet, so this doesn't invent
//! one ahead of it.
//!
//! A separate binary rather than a mode of `dafs` itself: the daemon's 32 MB
//! idle-RSS ceiling has no business paying for ratatui/crossterm, and this is
//! a short-lived interactive tool a user runs, not something that stays
//! resident.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use dafs_tui::capabilities::Capabilities;
use dafs_tui::client::{self, Client};
use dafs_tui::ui::{self, LogScroll};

/// Samples of `dafs_resident_bytes` kept for the trend sparkline. At the
/// default 1s refresh that's a 2-minute window — enough to see a scan spike
/// and its decay without keeping unbounded history.
const RSS_HISTORY_LEN: usize = 120;

#[derive(Parser, Debug)]
#[command(name = "dafs-tui", version, about = "Read-only status monitor for a dafs daemon")]
struct Args {
    /// Base URL of the daemon's HTTP API.
    #[arg(long, default_value = "http://127.0.0.1:7878")]
    url: String,

    /// How often to poll the daemon, in milliseconds.
    #[arg(long, default_value_t = 1000)]
    refresh_ms: u64,

    /// How many recent events to show.
    #[arg(long, default_value_t = 20)]
    limit: u32,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let client = Client::new(args.url.clone());
    let refresh = Duration::from_millis(args.refresh_ms.max(100));
    let caps = Capabilities::detect();

    // Checked *before* entering the alternate screen: a daemon that never
    // answers must fail as a plain, readable message on the normal terminal,
    // not as a raw-mode dashboard sitting empty (or, worse, a startup error
    // from the daemon racing the terminal for the same tty and corrupting
    // both — this is exactly the failure a user hit in practice).
    let mut status = client.poll(args.limit);
    if !status.connected {
        eprintln!("{}", unreachable_message(&args.url, status.error.as_deref()));
        std::process::exit(1);
    }

    let mut terminal = ratatui::init();
    let mut rss_history: VecDeque<u64> = VecDeque::with_capacity(RSS_HISTORY_LEN);
    push_rss_sample(&mut rss_history, &status);
    let mut last_poll = Instant::now();
    let mut log_scroll = LogScroll::default();

    let result = loop {
        if let Err(e) = terminal
            .draw(|frame| ui::draw(frame, &args.url, &status, &rss_history, log_scroll, caps))
        {
            break Err(e.into());
        }

        let timeout = refresh.saturating_sub(last_poll.elapsed());
        match event::poll(timeout)
            .and_then(|has_event| if has_event { event::read().map(Some) } else { Ok(None) })
        {
            Ok(Some(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                let quit = matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL));
                if quit {
                    break Ok(());
                }

                let total = status.log_lines.len();
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => log_scroll.up(total, 1),
                    KeyCode::Down | KeyCode::Char('j') => log_scroll.down(total, 1),
                    KeyCode::PageUp => log_scroll.up(total, 10),
                    KeyCode::PageDown => log_scroll.down(total, 10),
                    KeyCode::Home | KeyCode::Char('g') => log_scroll.to_top(),
                    KeyCode::End | KeyCode::Char('G') => log_scroll.to_bottom(),
                    _ => {}
                }
            }
            Ok(_) => {}
            Err(e) => break Err(e.into()),
        }

        if last_poll.elapsed() >= refresh {
            status = client.poll(args.limit);
            push_rss_sample(&mut rss_history, &status);
            last_poll = Instant::now();
        }
    };

    ratatui::restore();
    result
}

/// The message printed when the initial poll can't reach a daemon at all.
/// Pure and separate from `main` so its wording is unit-testable without
/// exiting a process.
fn unreachable_message(url: &str, reason: Option<&str>) -> String {
    let reason = reason.unwrap_or("connection failed");
    format!(
        "dafs-tui: can't reach a dafs daemon at {url}\n  ({reason})\n\n\
         Is one running? Start it, then try again:\n  \
         dafs --watch <a directory> &\n\n\
         Already running somewhere else? Point at it:\n  \
         dafs-tui --url http://host:port\n\n\
         Already running on this port but not answering? Something else may \
         hold it — check with `lsof -i :7878` (or the port in --url) and \
         free it before retrying."
    )
}

/// Append the latest RSS sample, dropping the oldest once the window is full.
/// A missed sample (daemon unreachable this tick) is skipped rather than
/// recorded as zero, so a disconnect blank rather than a fake trough.
fn push_rss_sample(history: &mut VecDeque<u64>, status: &client::Status) {
    let Some(rss) = status.resident_bytes else { return };
    if history.len() == RSS_HISTORY_LEN {
        history.pop_front();
    }
    history.push_back(rss);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreachable_message_names_the_url_and_a_fix() {
        let msg = unreachable_message("http://127.0.0.1:7878", Some("connection refused"));
        assert!(msg.contains("http://127.0.0.1:7878"));
        assert!(msg.contains("connection refused"));
        assert!(msg.contains("dafs --watch"), "should suggest starting the daemon");
        assert!(msg.contains("--url"), "should mention pointing at a different daemon");
    }

    #[test]
    fn unreachable_message_has_a_fallback_reason() {
        let msg = unreachable_message("http://127.0.0.1:7878", None);
        assert!(msg.contains("connection failed"));
    }
}

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
use dafs_tui::ui;

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

    let mut terminal = ratatui::init();
    let mut status = client.poll(args.limit);
    let mut rss_history: VecDeque<u64> = VecDeque::with_capacity(RSS_HISTORY_LEN);
    push_rss_sample(&mut rss_history, &status);
    let mut last_poll = Instant::now();

    let result = loop {
        if let Err(e) =
            terminal.draw(|frame| ui::draw(frame, &args.url, &status, &rss_history, caps))
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
